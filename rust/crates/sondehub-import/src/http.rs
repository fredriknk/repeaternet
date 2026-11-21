use std::{
    io::Read,
    thread,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Result, bail, ensure};
use repeaternet_providers::user_agent;
use reqwest::{StatusCode, Url, blocking::Client, header};

pub struct Http<'a> {
    client: Client,
    next: Instant,
    interval: Duration,
    stop: &'a dyn Fn() -> bool,
    pub bytes: u64,
    pub requests: u64,
    pub terrain_requests: u64,
}

pub struct Object {
    pub bytes: Vec<u8>,
    pub etag: Option<String>,
}

#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Interrupted")
    }
}

impl std::error::Error for Interrupted {}

pub fn check(stop: &dyn Fn() -> bool) -> Result<()> {
    if stop() {
        return Err(Interrupted.into());
    }
    Ok(())
}

impl<'a> Http<'a> {
    pub fn new(interval_ms: u64, stop: &'a dyn Fn() -> bool) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .no_gzip()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(user_agent(
                "temporary read-only collection for an RF link calibration dataset",
            ))
            .build()?;
        Ok(Self {
            client,
            next: Instant::now(),
            interval: Duration::from_millis(interval_ms),
            stop,
            bytes: 0,
            requests: 0,
            terrain_requests: 0,
        })
    }

    fn wait(&self, delay: Duration) -> Result<()> {
        let start = Instant::now();
        loop {
            check(self.stop)?;
            let remaining = delay.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Ok(());
            }
            thread::sleep(remaining.min(Duration::from_millis(100)));
        }
    }

    pub fn get(&mut self, url: &Url, limit: usize) -> Result<Option<Object>> {
        for attempt in 0..3 {
            self.wait(self.next.saturating_duration_since(Instant::now()))?;
            self.next = Instant::now() + self.interval;
            self.requests += 1;
            self.terrain_requests += u64::from(url.host_str() == Some("s3.amazonaws.com"));
            let mut retry = Duration::from_secs(2 << attempt);
            let response = self
                .client
                .get(url.clone())
                .header(header::ACCEPT_ENCODING, "gzip")
                .send();
            let error = match response {
                Ok(mut response) => {
                    let status = response.status();
                    if status == StatusCode::NOT_FOUND {
                        return Ok(None);
                    }
                    if status.is_success() {
                        let etag = response
                            .headers()
                            .get(header::ETAG)
                            .and_then(|s| s.to_str().ok())
                            .map(str::to_owned);
                        ensure!(
                            response.content_length().unwrap_or(0) <= limit as u64,
                            "{url} exceeds the download limit"
                        );
                        let mut bytes = Vec::new();
                        let mut buffer = [0; 64 * 1024];
                        let read = loop {
                            check(self.stop)?;
                            match response.read(&mut buffer) {
                                Ok(0) => break Ok(()),
                                Ok(n) => {
                                    self.bytes += n as u64;
                                    ensure!(
                                        bytes.len() + n <= limit,
                                        "{url} exceeds the download limit"
                                    );
                                    bytes.extend_from_slice(&buffer[..n]);
                                }
                                Err(err) => break Err(err),
                            }
                        };
                        match read {
                            Ok(()) => return Ok(Some(Object { bytes, etag })),
                            Err(err) => err.to_string(),
                        }
                    } else {
                        ensure!(
                            status == StatusCode::FORBIDDEN
                                || status == StatusCode::TOO_MANY_REQUESTS
                                || status.is_server_error(),
                            "{url} returned HTTP {status}"
                        );
                        if let Some(value) = response
                            .headers()
                            .get(header::RETRY_AFTER)
                            .and_then(|s| s.to_str().ok())
                        {
                            let delay = value
                                .trim()
                                .parse::<u64>()
                                .ok()
                                .map(Duration::from_secs)
                                .or_else(|| {
                                    httpdate::parse_http_date(value).ok().map(|date| {
                                        date.duration_since(SystemTime::now()).unwrap_or_default()
                                    })
                                });
                            retry = retry.max(delay.unwrap_or_default());
                        }
                        format!("HTTP {status}")
                    }
                }
                Err(err) => err.to_string(),
            };
            if attempt == 2 {
                bail!("Stopped after three attempts at {url} · {error}");
            }
            eprintln!("{url} · {error} · retrying in {} s", retry.as_secs());
            self.wait(retry)?;
        }
        unreachable!()
    }
}
