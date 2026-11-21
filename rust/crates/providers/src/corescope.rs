use std::{collections::HashSet, time::Duration};

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, SecondsFormat, Timelike, Utc};
use repeaternet_terrain::{
    corridor::{distance_to_segment_m, within_corridor_latlon},
    LonLat,
};
use reqwest::blocking::{Client, Response};
use serde::Serialize;
use serde_json::Value;
use unicode_casefold::UnicodeCaseFold;

use crate::{invalid, response_error, transport, ProviderError, Result};

pub mod capture;

const PROVIDER: &str = "CoreScope";
pub const DEFAULT_BASE_URL: &str = "https://corescope.eth0.no";
pub const DEFAULT_MAXIMUM_PAGES: usize = 20;
pub const DEFAULT_PAGE_SIZE: usize = 500;
const BODY_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeardTime {
    WithOffset(DateTime<FixedOffset>),
    Naive(NaiveDateTime),
}

impl HeardTime {
    pub fn isoformat(self) -> String {
        match self {
            Self::WithOffset(time) => time.to_rfc3339_opts(
                if time.nanosecond() == 0 {
                    SecondsFormat::Secs
                } else {
                    SecondsFormat::Micros
                },
                false,
            ),
            Self::Naive(time) => {
                if time.nanosecond() == 0 {
                    time.format("%Y-%m-%dT%H:%M:%S").to_string()
                } else {
                    time.format("%Y-%m-%dT%H:%M:%S%.6f").to_string()
                }
            }
        }
    }

    fn utc(self) -> DateTime<Utc> {
        match self {
            Self::WithOffset(time) => time.to_utc(),
            Self::Naive(time) => time.and_utc(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CoreScopeRepeater {
    pub public_key: String,
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub last_heard: Option<HeardTime>,
    pub relay_active: Option<bool>,
    pub relay_count_24h: i64,
}

impl CoreScopeRepeater {
    pub fn id(&self) -> String {
        format!("K-{}", self.public_key.chars().take(10).collect::<String>())
    }

    pub fn freshness_at(&self, now: DateTime<Utc>) -> String {
        let Some(heard) = self.last_heard else {
            return "last heard unknown".into();
        };
        let days = now
            .signed_duration_since(heard.utc())
            .num_milliseconds()
            .div_euclid(86_400_000);
        if days != 0 {
            format!("heard {days} day(s) ago")
        } else {
            format!(
                "heard {} hour(s) ago",
                now.signed_duration_since(heard.utc()).num_hours().max(0)
            )
        }
    }
}

pub struct CoreScopeClient {
    base_url: String,
    client: Client,
    page_size: usize,
    timeout: Duration,
}

impl CoreScopeClient {
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        Self::with_client(
            base_url,
            transport::client(PROVIDER, 20, 20)?,
            DEFAULT_PAGE_SIZE,
            Duration::from_secs(20),
        )
    }

    pub fn with_client(
        base_url: impl Into<String>,
        client: Client,
        page_size: usize,
        timeout: Duration,
    ) -> Result<Self> {
        if page_size == 0 || timeout.is_zero() {
            return Err(invalid("CoreScope page size and timeout must be positive"));
        }
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').into(),
            client,
            page_size,
            timeout,
        })
    }

    pub fn fetch_repeaters(
        &self,
        maximum_pages: usize,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<CoreScopeRepeater>> {
        let mut keys = HashSet::new();
        let mut repeaters = Vec::new();
        for page in 0..maximum_pages {
            let offset = page
                .checked_mul(self.page_size)
                .ok_or_else(|| invalid("CoreScope pagination offset exceeds the limit"))?;
            let response = self.get(offset, cancelled)?;
            let payload = transport::json(PROVIDER, response, BODY_LIMIT, cancelled)?;
            let Some(nodes) = payload.get("nodes") else {
                break;
            };
            let records = nodes
                .as_array()
                .ok_or_else(|| invalid("CoreScope returned an invalid node list"))?;
            let previous_count = repeaters.len();
            for record in records {
                transport::check(cancelled)?;
                if let Some(repeater) = parse_repeater(record)? {
                    if keys.insert(repeater.public_key.clone()) {
                        repeaters.push(repeater);
                    }
                }
            }
            let total_reached = payload
                .get("total")
                .and_then(Value::as_i64)
                .is_some_and(|total| total <= repeaters.len() as i64);
            if records.len() < self.page_size || repeaters.len() == previous_count || total_reached
            {
                break;
            }
        }
        repeaters.sort_by_cached_key(|item| item.name.as_str().case_fold().collect::<String>());
        Ok(repeaters)
    }

    fn get(&self, offset: usize, cancelled: &dyn Fn() -> bool) -> Result<Response> {
        for attempt in 0..3 {
            transport::check(cancelled)?;
            let response = self
                .client
                .get(format!("{}/api/nodes", self.base_url))
                .header(
                    "User-Agent",
                    "RF-Router-Planner/0.1 (interactive read-only import)",
                )
                .timeout(self.timeout)
                .query(&[
                    ("role", "repeater".into()),
                    ("limit", self.page_size.to_string()),
                    ("offset", offset.to_string()),
                ])
                .send()
                .map_err(|source| ProviderError::Transport {
                    provider: PROVIDER,
                    source,
                })?;
            transport::check(cancelled)?;
            if response.status().is_success() {
                return Ok(response);
            }
            let status = response.status().as_u16();
            if !matches!(status, 429 | 503) || attempt == 2 {
                return Err(ProviderError::Http {
                    provider: PROVIDER,
                    status,
                });
            }
            let seconds = transport::retry_seconds(&response, false)
                .unwrap_or(1.0)
                .clamp(0.0, 5.0);
            transport::wait(Duration::from_secs_f64(seconds), cancelled)?;
        }
        unreachable!("the final attempt returns")
    }
}

fn parse_repeater(value: &Value) -> Result<Option<CoreScopeRepeater>> {
    if value.get("role").and_then(Value::as_str) != Some("repeater") {
        return Ok(None);
    }
    let key = truthy_field(value, "public_key")
        .map(py_string)
        .unwrap_or_default();
    let public_key = key.trim().to_owned();
    let (Some(latitude), Some(longitude)) = (
        value.get("lat").and_then(number),
        value.get("lon").and_then(number),
    ) else {
        return Ok(None);
    };
    if public_key.is_empty()
        || !(54.0..=72.0).contains(&latitude)
        || !(4.0..=32.0).contains(&longitude)
    {
        return Ok(None);
    }
    let name = truthy_field(value, "name")
        .map_or_else(|| public_key.chars().take(10).collect(), py_string);
    let last_heard = truthy_field(value, "last_heard")
        .or_else(|| truthy_field(value, "last_seen"))
        .and_then(Value::as_str)
        .and_then(parse_time);
    let relay_active = value.get("relay_active").and_then(Value::as_bool);
    let relay_count_24h = match truthy_field(value, "relay_count_24h") {
        None => 0,
        Some(raw) => {
            integer(raw).ok_or_else(|| invalid("CoreScope returned an invalid relay count"))?
        }
    };
    Ok(Some(CoreScopeRepeater {
        public_key,
        name,
        latitude,
        longitude,
        last_heard,
        relay_active,
        relay_count_24h,
    }))
}

fn parse_time(value: &str) -> Option<HeardTime> {
    let value = value.replace('Z', "+00:00");
    if let Ok(time) =
        DateTime::parse_from_rfc3339(&value).or_else(|_| value.parse::<DateTime<FixedOffset>>())
    {
        return time
            .with_nanosecond(time.nanosecond() / 1_000 * 1_000)
            .map(HeardTime::WithOffset);
    }
    for separator in ['T', ' '] {
        for format in ["%H:%M:%S%.f", "%H:%M"] {
            let format = format!("%Y-%m-%d{separator}{format}");
            if let Ok(time) = NaiveDateTime::parse_from_str(&value, &format) {
                return time
                    .with_nanosecond(time.nanosecond() / 1_000 * 1_000)
                    .map(HeardTime::Naive);
            }
            if let Ok(time) = DateTime::parse_from_str(&value, &format!("{}%:z", format)) {
                return time
                    .with_nanosecond(time.nanosecond() / 1_000 * 1_000)
                    .map(HeardTime::WithOffset);
            }
        }
    }
    NaiveDate::parse_from_str(&value, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(HeardTime::Naive)
}

fn truthy_field<'a>(value: &'a Value, field: &str) -> Option<&'a Value> {
    value.get(field).filter(|value| match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    })
}

fn py_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        _ => value.to_string(),
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .or_else(|| value.as_bool().map(|b| u8::from(b).into()))
}

fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::String(string) => string.trim().parse().ok(),
        Value::Bool(boolean) => Some(i64::from(*boolean)),
        Value::Number(number) => number.as_i64().or_else(|| {
            let float = number.as_f64()?;
            (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0)
                .contains(&float)
                .then_some(float as i64)
        }),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CorridorRepeater {
    pub id: String,
    pub name: String,
    pub latitude: f64,
    pub longitude: f64,
    pub corridor_distance_m: i64,
    pub freshness: String,
    pub last_heard: Option<String>,
    pub relay_active: Option<bool>,
    pub relay_count_24h: i64,
    pub provenance: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CorridorRepeaters {
    pub routers: Vec<CorridorRepeater>,
    pub count: usize,
}

/// Filter with the square-ended corridor, then sort on the rounded distance to the segment.
pub fn filter_corridor(
    repeaters: &[CoreScopeRepeater],
    a: LonLat,
    b: LonLat,
    width_m: f64,
    now: DateTime<Utc>,
) -> Result<CorridorRepeaters> {
    if !(1.0..=500_000.0).contains(&width_m) {
        return Err(invalid("Corridor width must be between 1 and 500,000 m"));
    }
    for point in [a, b] {
        if !(-90.0..=90.0).contains(&point.latitude) || !(-180.0..=180.0).contains(&point.longitude)
        {
            return Err(invalid("Route coordinates are outside WGS84 bounds"));
        }
    }
    let mut routers = Vec::new();
    for repeater in repeaters {
        let point = LonLat {
            longitude: repeater.longitude,
            latitude: repeater.latitude,
        };
        if within_corridor_latlon(point, a, b, width_m) {
            let distance = distance_to_segment_m(point, a, b);
            if !distance.is_finite() {
                return Err(response_error(PROVIDER, "invalid corridor distance"));
            }
            routers.push(CorridorRepeater {
                id: repeater.id(),
                name: repeater.name.clone(),
                latitude: repeater.latitude,
                longitude: repeater.longitude,
                corridor_distance_m: distance.round_ties_even() as i64,
                freshness: repeater.freshness_at(now),
                last_heard: repeater.last_heard.map(HeardTime::isoformat),
                relay_active: repeater.relay_active,
                relay_count_24h: repeater.relay_count_24h,
                provenance: PROVIDER,
            });
        }
    }
    routers.sort_by_cached_key(|item| {
        (
            item.corridor_distance_m,
            item.name.as_str().case_fold().collect::<String>(),
        )
    });
    Ok(CorridorRepeaters {
        count: routers.len(),
        routers,
    })
}
