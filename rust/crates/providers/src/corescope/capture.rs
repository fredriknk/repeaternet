//! Paced, read-only transport for CoreScope RF capture.

use std::time::{Duration, Instant};

use reqwest::{blocking::Client, redirect::Policy, Url};
use serde_json::Value;

use super::PROVIDER;
use crate::{invalid, transport, ProviderError, Result};

const BODY_LIMIT: u64 = 64 * 1024 * 1024;

pub enum Endpoint<'a> {
    Stats,
    Packets,
    Nodes,
    Observers,
    Graph,
    Neighbors(&'a str),
}

pub struct CaptureClient {
    base: Url,
    client: Client,
    interval: Duration,
    previous_request: Option<Instant>,
    requests: usize,
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
            .redirect(Policy::none())
            .user_agent(concat!(
                "RepeaterNet/",
                env!("CARGO_PKG_VERSION"),
                " (read-only CoreScope RF capture)"
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
        })
    }

    pub fn base_url(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    pub fn requests(&self) -> usize {
        self.requests
    }

    pub fn get(
        &mut self,
        endpoint: Endpoint<'_>,
        query: &[(&str, &str)],
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Value> {
        let mut url = self.base.clone();
        {
            let mut segments = url.path_segments_mut().expect("validated HTTP URL");
            segments.pop_if_empty().push("api");
            match endpoint {
                Endpoint::Stats => segments.push("stats"),
                Endpoint::Packets => segments.push("packets"),
                Endpoint::Nodes => segments.push("nodes"),
                Endpoint::Observers => segments.push("observers"),
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
        for attempt in 0..=3 {
            if let Some(previous) = self.previous_request {
                transport::wait(self.interval.saturating_sub(previous.elapsed()), cancelled)?;
            }
            transport::check(cancelled)?;
            self.previous_request = Some(Instant::now());
            self.requests += 1;
            let response =
                self.client
                    .get(url.clone())
                    .send()
                    .map_err(|source| ProviderError::Transport {
                        provider: PROVIDER,
                        source: source.without_url(),
                    })?;
            transport::check(cancelled)?;
            let status = response.status();
            if status.is_success() {
                return transport::json(PROVIDER, response, BODY_LIMIT, cancelled);
            }
            if attempt == 3 || (status.as_u16() != 429 && !status.is_server_error()) {
                return Err(ProviderError::Http {
                    provider: PROVIDER,
                    status: status.as_u16(),
                });
            }
            let backoff = Duration::from_secs(1 << attempt);
            let retry_after = transport::retry_seconds(&response, true)
                .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok())
                .unwrap_or_default();
            drop(response);
            transport::wait(backoff.max(retry_after), cancelled)?;
        }
        unreachable!("the final attempt returns")
    }
}
