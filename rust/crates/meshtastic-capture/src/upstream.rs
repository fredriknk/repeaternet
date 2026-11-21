use std::{
    collections::HashMap,
    fmt,
    io::Read,
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use reqwest::{StatusCode, Url, blocking::Client, header, redirect::Policy};
use robotstxt::DefaultMatcher;
use rusqlite::Connection;
use serde_json::Value;

use crate::db;

const PURPOSE: &str = "temporary read-only collection for an RF link calibration dataset";
const MAX_JSON: u64 = 64 * 1024 * 1024;
const MAX_ROBOTS: u64 = 500 * 1024;

#[derive(Debug)]
pub struct HttpStatus {
    pub code: StatusCode,
    endpoint: &'static str,
}

impl fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HTTP {} on {}", self.code, self.endpoint)
    }
}

impl std::error::Error for HttpStatus {}

pub fn jitter(base: Duration, minimum: Duration) -> Duration {
    let factor = getrandom::u32().map_or(1.0, |x| 0.9 + 0.2 * f64::from(x) / f64::from(u32::MAX));
    base.mul_f64(factor).max(minimum)
}

pub fn wait(duration: Duration, stop: &dyn Fn() -> bool) {
    let start = Instant::now();
    while !stop() {
        let remaining = duration.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

#[derive(Default)]
struct Validators {
    etag: Option<header::HeaderValue>,
    modified: Option<header::HeaderValue>,
}

pub struct Http<'a> {
    client: Client,
    host: &'static str,
    stop: &'a dyn Fn() -> bool,
    min: Duration,
    budget: u32,
    last: Option<Instant>,
    robots: String,
    blocked_paths: std::collections::HashSet<String>,
    validators: HashMap<&'static str, Validators>,
    failures: u32,
    severe: u32,
    pub resume_ms: i64,
    pub requests: u32,
    pub disabled: bool,
}

impl<'a> Http<'a> {
    pub fn new(
        host: &'static str,
        min: Duration,
        budget: u32,
        stop: &'a dyn Fn() -> bool,
    ) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .user_agent(repeaternet_providers::user_agent(PURPOSE))
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .gzip(true)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(40))
            .build()?;
        Ok(Self {
            client,
            host,
            stop,
            min,
            budget,
            last: None,
            robots: String::new(),
            blocked_paths: Default::default(),
            validators: HashMap::new(),
            failures: 0,
            severe: 0,
            resume_ms: 0,
            requests: 0,
            disabled: false,
        })
    }

    pub fn robots(&mut self, db: &Connection) -> Result<()> {
        let url = self.url("/robots.txt", &[])?;
        let response = self.request(db, url, None)?;
        if matches!(response.status(), StatusCode::NOT_FOUND | StatusCode::GONE) {
            return Ok(());
        }
        ensure!(
            response.status().is_success(),
            "robots.txt returned HTTP {}",
            response.status()
        );
        self.robots = String::from_utf8_lossy(&body(response, MAX_ROBOTS)?).into_owned();
        Ok(())
    }

    pub fn json(
        &mut self,
        db: &mut Connection,
        path: &'static str,
        query: &[(&str, String)],
        conditional: bool,
        ingest: impl FnOnce(&mut Connection, &Value) -> Result<()>,
    ) -> Result<bool> {
        let url = self.url(path, query)?;
        self.fetch_json(db, url, path, conditional, ingest)
    }

    pub fn packet_seen(
        &mut self,
        db: &mut Connection,
        id: u64,
        ingest: impl FnOnce(&mut Connection, &Value) -> Result<()>,
    ) -> Result<bool> {
        let url = self.url(&format!("/api/packets_seen/{id}"), &[])?;
        self.fetch_json(db, url, "/api/packets_seen/{id}", false, ingest)
    }

    fn fetch_json(
        &mut self,
        db: &mut Connection,
        url: Url,
        path_label: &'static str,
        conditional: bool,
        ingest: impl FnOnce(&mut Connection, &Value) -> Result<()>,
    ) -> Result<bool> {
        if !DefaultMatcher::default().one_agent_allowed_by_robots(
            &self.robots,
            "RepeaterNet",
            url.as_str(),
        ) {
            if self.blocked_paths.insert(path_label.into()) {
                println!("{} · robots.txt disallows {path_label}", self.host);
            }
            return Ok(false);
        }
        let response = self.request(db, url, conditional.then_some(path_label))?;
        let status = response.status();
        if status == StatusCode::NOT_MODIFIED && conditional {
            return Ok(true);
        }
        if !status.is_success() {
            if status == StatusCode::FORBIDDEN
                || status == StatusCode::NOT_FOUND
                || status.is_server_error()
            {
                self.severe += 1;
                if self.severe >= 3 {
                    self.disabled = true;
                }
            }
            let retry = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(retry_after)
                .unwrap_or_default();
            self.backoff(retry);
            return Err(HttpStatus {
                code: status,
                endpoint: path_label,
            }
            .into());
        }
        let validators = Validators {
            etag: response.headers().get(header::ETAG).cloned(),
            modified: response.headers().get(header::LAST_MODIFIED).cloned(),
        };
        let bytes = body(response, MAX_JSON)?;
        let value = serde_json::from_slice(&bytes)
            .with_context(|| format!("Invalid JSON on {path_label}"))?;
        ingest(db, &value).with_context(|| format!("Process {path_label}"))?;
        if conditional {
            self.validators.insert(path_label, validators);
        }
        Ok(true)
    }

    fn url(&self, path: &str, query: &[(&str, String)]) -> Result<Url> {
        let mut url = Url::parse(&format!("https://{}{path}", self.host))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    fn request(
        &mut self,
        db: &Connection,
        url: Url,
        validator_key: Option<&str>,
    ) -> Result<reqwest::blocking::Response> {
        if let Some(last) = self.last {
            wait(
                jitter(self.min, self.min).saturating_sub(last.elapsed()),
                self.stop,
            );
        }
        ensure!(!(self.stop)(), "Capture stopping");
        let now = Utc::now().timestamp_millis();
        if let Some(resume) = db::record_request(db, now, self.budget)? {
            self.resume_ms = resume;
            bail!("Rolling daily request budget reached");
        }
        let mut request = self.client.get(url);
        if let Some(validators) = validator_key.and_then(|key| self.validators.get(key)) {
            if let Some(value) = &validators.etag {
                request = request.header(header::IF_NONE_MATCH, value);
            }
            if let Some(value) = &validators.modified {
                request = request.header(header::IF_MODIFIED_SINCE, value);
            }
        }
        self.requests += 1;
        self.last = Some(Instant::now());
        request
            .send()
            .map_err(|err| err.without_url())
            .context("GET failed")
    }

    pub fn backoff(&mut self, retry: Duration) {
        self.failures = self.failures.saturating_add(1);
        let seconds = (60_u64 << (self.failures - 1).min(6)).min(3600);
        let ms = jitter(Duration::from_secs(seconds), Duration::from_secs(60))
            .min(Duration::from_secs(3600))
            .max(retry)
            .as_millis();
        self.resume_ms = Utc::now()
            .timestamp_millis()
            .saturating_add(i64::try_from(ms).unwrap_or(i64::MAX));
    }

    pub fn success(&mut self) {
        self.failures = 0;
        self.resume_ms = 0;
    }
}

fn body(response: reqwest::blocking::Response, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    response.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "Response exceeds size limit");
    Ok(bytes)
}

fn retry_after(value: &header::HeaderValue) -> Option<Duration> {
    let value = value.to_str().ok()?.trim();
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            Some(
                httpdate::parse_http_date(value)
                    .ok()?
                    .duration_since(SystemTime::now())
                    .unwrap_or_default(),
            )
        })
}
