use std::{
    io::{Read, Write},
    time::{Duration, Instant, SystemTime},
};

use crate::{response_error, ProviderError, Result};
use reqwest::blocking::{Client, ClientBuilder, Response};

pub(crate) fn client(provider: &'static str, connect: u64, timeout: u64) -> Result<Client> {
    client_builder(connect, timeout)
        .build()
        .map_err(|source| ProviderError::Transport { provider, source })
}

pub(crate) fn client_builder(connect: u64, timeout: u64) -> ClientBuilder {
    // Keep ring for the static release builds. Install it once per process.
    let _ = rustls::crypto::ring::default_provider().install_default();
    Client::builder()
        .connect_timeout(Duration::from_secs(connect))
        .timeout(Duration::from_secs(timeout))
}

/// Descriptive User-Agent for every upstream request, with a project link so
/// service operators can see what is calling them.
pub(crate) fn user_agent(purpose: &str) -> String {
    format!(
        "RepeaterNet/{} ({purpose}; +https://github.com/fredriknk/repeaternet)",
        env!("CARGO_PKG_VERSION")
    )
}

pub(crate) fn check(cancelled: &dyn Fn() -> bool) -> Result<()> {
    if cancelled() {
        Err(ProviderError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn wait(delay: Duration, cancelled: &dyn Fn() -> bool) -> Result<()> {
    let start = Instant::now();
    loop {
        check(cancelled)?;
        let remaining = delay.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Ok(());
        }
        std::thread::sleep(remaining.min(Duration::from_millis(50)));
    }
}

pub(crate) fn retry_seconds(response: &Response, http_date: bool) -> Option<f64> {
    let value = response.headers().get("retry-after")?.to_str().ok()?.trim();
    if http_date {
        if let Ok(seconds) = value.parse::<i64>() {
            return Some(seconds as f64);
        }
        return httpdate::parse_http_date(value).ok().map(|date| {
            date.duration_since(SystemTime::now())
                .unwrap_or_default()
                .as_secs_f64()
        });
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|seconds| !seconds.is_nan())
}

pub(crate) fn stream(
    provider: &'static str,
    mut response: Response,
    output: &mut impl Write,
    limit: u64,
    cancelled: &dyn Fn() -> bool,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let total = response.content_length().unwrap_or(0);
    if total > limit {
        return Err(response_error(provider, "body exceeds the size limit"));
    }
    let mut downloaded = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check(cancelled)?;
        let count = response
            .read(&mut buffer)
            .map_err(|error| response_error(provider, format!("could not read body: {error}")))?;
        check(cancelled)?;
        if count == 0 {
            return Ok(());
        }
        downloaded += count as u64;
        if downloaded > limit {
            return Err(response_error(provider, "body exceeds the size limit"));
        }
        output.write_all(&buffer[..count])?;
        progress(downloaded, total);
    }
}

pub(crate) fn json(
    provider: &'static str,
    response: Response,
    limit: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<serde_json::Value> {
    let mut bytes = Vec::new();
    stream(provider, response, &mut bytes, limit, cancelled, |_, _| {})?;
    serde_json::from_slice(&bytes).map_err(|error| response_error(provider, error.to_string()))
}

pub(crate) fn py_float(value: f64) -> String {
    let formatted = format!("{value:?}");
    if let Some((mantissa, exponent)) = formatted.split_once('e') {
        let exponent: i32 = exponent.parse().expect("Rust float exponent");
        format!("{mantissa}e{exponent:+03}")
    } else {
        formatted
    }
}

pub(crate) fn py_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut repr = String::from(quote);
    for c in value.chars() {
        match c {
            '\\' => repr.push_str("\\\\"),
            '\n' => repr.push_str("\\n"),
            '\r' => repr.push_str("\\r"),
            '\t' => repr.push_str("\\t"),
            c if c == quote => {
                repr.push('\\');
                repr.push(c);
            }
            c if c.is_control() && (c as u32) <= 0xff => {
                repr.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => repr.push(c),
        }
    }
    repr.push(quote);
    repr
}
