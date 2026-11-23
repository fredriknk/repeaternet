//! Paced, read-only transport for CoreScope RF capture.

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use reqwest::{
    blocking::Client,
    header::{self, HeaderValue},
    redirect::Policy,
    Url,
};
use serde_json::Value;

use super::PROVIDER;
use crate::{invalid, transport, ProviderError, Result};

const BODY_LIMIT: u64 = 64 * 1024 * 1024;

pub enum Endpoint<'a> {
    Stats,
    Packets,
    Nodes,
    Observers,
    Observer(&'a str),
    Graph,
    Neighbors(&'a str),
}

pub struct CaptureClient {
    base: Url,
    client: Client,
    interval: Duration,
    previous_request: Option<Instant>,
    requests: usize,
    retries: u32,
    retry_after: Duration,
    validators: HashMap<Url, CachedResponse>,
}

struct CachedResponse {
    header: header::HeaderName,
    validator: HeaderValue,
    value: Value,
}

impl CaptureClient {
    pub fn new(base: &str, interval: Duration) -> Result<Self> {
        let base = Url::parse(base.trim_end_matches('/'))
            .map_err(|_| invalid("CoreScope base URL is invalid"))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(invalid(
                "CoreScope base URL must be an HTTP(S) URL without credentials, query or fragment",
            ));
        }
        let client = transport::client_builder(10, 30)
            .http1_only()
            .pool_max_idle_per_host(1)
            .retry(reqwest::retry::never())
            .redirect(Policy::none())
            .user_agent(transport::user_agent(
                "temporary read-only collection for an RF link calibration dataset",
            ))
            .build()
            .map_err(|source| ProviderError::Transport {
                provider: PROVIDER,
                source,
            })?;
        Ok(Self {
            base,
            client,
            interval,
            previous_request: None,
            requests: 0,
            retries: 3,
            retry_after: Duration::ZERO,
            validators: HashMap::new(),
        })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    pub fn host(&self) -> &str {
        self.base.host_str().expect("validated CoreScope URL")
    }

    pub fn requests(&self) -> usize {
        self.requests
    }

    pub fn defer_retries(&mut self) {
        self.retries = 0;
    }

    pub fn retry_after(&self) -> Duration {
        self.retry_after
    }

    pub fn get(
        &mut self,
        endpoint: Endpoint<'_>,
        query: &[(&str, &str)],
        cancelled: &dyn Fn() -> bool,
        mut before_request: impl FnMut(Option<&str>) -> Result<()>,
    ) -> Result<Value> {
        self.retry_after = Duration::ZERO;
        let neighbor = match endpoint {
            Endpoint::Neighbors(key) => Some(key),
            _ => None,
        };
        let conditional = matches!(
            endpoint,
            Endpoint::Stats | Endpoint::Observers | Endpoint::Graph | Endpoint::Nodes
        );
        let mut url = self.base.clone();
        {
            let mut segments = url.path_segments_mut().expect("validated HTTP URL");
            segments.pop_if_empty().push("api");
            match endpoint {
                Endpoint::Stats => segments.push("stats"),
                Endpoint::Packets => segments.push("packets"),
                Endpoint::Nodes => segments.push("nodes"),
                Endpoint::Observers => segments.push("observers"),
                Endpoint::Observer(id) => segments.extend(["observers", id]),
                Endpoint::Graph => segments.extend(["analytics", "neighbor-graph"]),
                Endpoint::Neighbors(key) => {
                    if key.len() != 64 || !key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        return Err(invalid("CoreScope neighbor request requires a public key"));
                    }
                    segments.extend(["nodes", key, "neighbors"])
                }
            };
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        let retries = if matches!(endpoint, Endpoint::Neighbors(_) | Endpoint::Observer(_)) {
            0
        } else {
            self.retries
        };
        for attempt in 0..=retries {
            if let Some(previous) = self.previous_request {
                transport::wait(self.interval.saturating_sub(previous.elapsed()), cancelled)?;
            }
            transport::check(cancelled)?;
            before_request(neighbor)?;
            self.previous_request = Some(Instant::now());
            self.requests += 1;
            let cached = self.validators.get(&url);
            let mut request = self.client.get(url.clone());
            if let Some(cached) = cached {
                request = request.header(&cached.header, &cached.validator);
            }
            let response = request.send().map_err(|source| ProviderError::Transport {
                provider: PROVIDER,
                source: source.without_url(),
            })?;
            transport::check(cancelled)?;
            let status = response.status();
            if let (reqwest::StatusCode::NOT_MODIFIED, Some(cached)) = (status, cached) {
                return Ok(cached.value.clone());
            }
            if status.is_success() {
                let headers = response.headers();
                let validator = headers
                    .get(header::ETAG)
                    .cloned()
                    .map(|value| (header::IF_NONE_MATCH, value))
                    .or_else(|| {
                        headers
                            .get(header::LAST_MODIFIED)
                            .cloned()
                            .map(|value| (header::IF_MODIFIED_SINCE, value))
                    });
                let value = transport::json(PROVIDER, response, BODY_LIMIT, cancelled)?;
                if let (true, Some((header, validator))) = (conditional, validator) {
                    self.validators.insert(
                        url,
                        CachedResponse {
                            header,
                            validator,
                            value: value.clone(),
                        },
                    );
                } else {
                    self.validators.remove(&url);
                }
                return Ok(value);
            }
            let transient = status.as_u16() == 429 || status.is_server_error();
            self.retry_after = transport::retry_seconds(&response, true)
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                .unwrap_or_default();
            if attempt == retries || !transient {
                return Err(ProviderError::Http {
                    provider: PROVIDER,
                    status: status.as_u16(),
                });
            }
            let backoff = Duration::from_secs(1 << attempt);
            drop(response);
            transport::wait(backoff.max(self.retry_after), cancelled)?;
        }
        unreachable!("the final attempt returns")
    }
}
