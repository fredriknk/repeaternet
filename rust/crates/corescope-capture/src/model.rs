use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct PacketPage {
    pub packets: Vec<Packet>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Deserialize)]
pub struct Packet {
    pub id: i64,
    pub hash: String,
    pub first_seen: Option<String>,
    pub route_type: Option<i64>,
    pub payload_type: Option<i64>,
    pub observation_count: Option<i64>,
    pub scope_name: Option<String>,
    decoded_json: Option<Value>,
    raw_hex: Option<Value>,
    observations: Option<Vec<Reception>>,
    #[serde(flatten)]
    representative: Reception,
}

impl Packet {
    pub fn first_seen(&self) -> Result<DateTime<Utc>> {
        self.first_seen
            .as_deref()
            .or(self.representative.timestamp.as_deref())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| anyhow!("CoreScope packet has no valid first-seen time"))
    }

    pub fn advert(&self) -> Option<Advert> {
        if self.payload_type != Some(4) {
            return None;
        }
        let decoded = self.decoded()?;
        let key = decoded
            .get("pubKey")
            .and_then(Value::as_str)
            .and_then(public_key)?;
        let flags = decoded.get("flags");
        Some(Advert {
            key,
            lat: number(&decoded, "lat"),
            lon: number(&decoded, "lon"),
            flags: flags
                .and_then(|value| value.get("raw").unwrap_or(value).as_u64())
                .and_then(|raw| u8::try_from(raw).ok()),
            name: string(&decoded, "name").map(str::to_owned),
        })
    }

    pub fn receptions(&self) -> &[Reception] {
        self.observations
            .as_deref()
            .filter(|observations| !observations.is_empty())
            .unwrap_or_else(|| std::slice::from_ref(&self.representative))
    }

    fn decoded(&self) -> Option<std::borrow::Cow<'_, Value>> {
        let value = self.decoded_json.as_ref()?;
        match value {
            Value::String(raw) => serde_json::from_str(raw).ok().map(std::borrow::Cow::Owned),
            _ => Some(std::borrow::Cow::Borrowed(value)),
        }
    }

    pub fn trace_hops(&self) -> Option<TraceHops> {
        if self.payload_type != Some(9) {
            return None;
        }
        if let Some(decoded) = self.decoded()
            && let Some(values) = decoded.get("snrValues").filter(|value| !value.is_null())
        {
            let snr = values
                .as_array()?
                .iter()
                .map(Value::as_f64)
                .collect::<Option<Vec<_>>>()?;
            let flags = u8::try_from(decoded.get("traceFlags")?.as_u64()?).ok()?;
            return TraceHops::new(flags, decoded.get("pathData")?.as_str()?, snr);
        }
        TraceHops::from_wire(self.raw_hex.as_ref()?.as_str()?)
    }

    pub fn trace_observer(&self) -> Option<String> {
        self.representative.observer_id.as_deref().map(observer_key)
    }
}

pub struct TraceHops {
    pub hash_size: usize,
    pub path_hashes: Vec<String>,
    pub snr_values: Vec<f64>,
}

impl TraceHops {
    /// MeshCore Mesh.cpp (onRecvPacket) matches the route hash at offset
    /// `path_len * (1 << (flags & 3))` before appending that repeater's receive
    /// SNR. Each row therefore measures the completed prefix of the payload
    /// route, including the first hop from the originator. Packet.h and Mesh.cpp
    /// use signed quarter-dB bytes. CoreScope decoder.go already divides by four
    /// in decoded JSON, so only the wire fallback applies that conversion.
    fn new(flags: u8, route: &str, snr_values: Vec<f64>) -> Option<Self> {
        let hash_size = 1 << (flags & 3);
        let width = 2 * hash_size;
        if snr_values.is_empty()
            || snr_values.len() > 63
            || !route.len().is_multiple_of(width)
            || snr_values.len() > route.len() / width
            || !route.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let path_hashes = (0..snr_values.len())
            .map(|i| route[i * width..(i + 1) * width].to_ascii_lowercase())
            .collect();
        Some(Self {
            hash_size,
            path_hashes,
            snr_values,
        })
    }

    fn from_wire(hex: &str) -> Option<Self> {
        // Packet.cpp writeTo: header, optional transport codes, path count,
        // signed SNR bytes, then the TRACE payload (tag, auth, flags, route).
        if !hex.len().is_multiple_of(2) || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let byte = |i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok();
        let count_at = match byte(0)? & 0x3f {
            0x26 => 1,
            0x27 => 5,
            _ => return None,
        };
        let count = usize::from(byte(count_at)?);
        if count > 63 {
            return None;
        }
        let snr_start = count_at + 1;
        let flags_at = snr_start + count + 8;
        let flags = byte(flags_at)?;
        let snr = (snr_start..snr_start + count)
            .map(|i| Some(f64::from(byte(i)? as i8) / 4.0))
            .collect::<Option<Vec<_>>>()?;
        Self::new(flags, hex.get((flags_at + 1) * 2..)?, snr)
    }
}

#[derive(Deserialize)]
pub struct Reception {
    // The transmission ID belongs to Packet; serde flatten leaves this None
    // for the representative row.
    pub id: Option<i64>,
    pub timestamp: Option<String>,
    pub observer_id: Option<String>,
    pub observer_name: Option<String>,
    pub observer_iata: Option<String>,
    pub rssi: Option<f64>,
    pub snr: Option<f64>,
    pub radio: Option<String>,
    pub score: Option<i64>,
    path_json: Option<Value>,
    #[serde(rename = "_parsedPath")]
    parsed_path: Option<Value>,
    pub resolved_path: Option<Value>,
    pub direction: Option<String>,
}

impl Reception {
    pub fn path_prefixes(&self) -> Option<String> {
        path(self.parsed_path.as_ref(), false).or_else(|| path(self.path_json.as_ref(), false))
    }

    pub fn resolved_path(&self) -> Option<String> {
        path(self.resolved_path.as_ref(), true)
    }
}

pub struct Advert {
    pub key: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub flags: Option<u8>,
    pub name: Option<String>,
}

fn path(value: Option<&Value>, resolved: bool) -> Option<String> {
    let value = value?;
    let decoded;
    let entries = if let Some(json) = value.as_str() {
        decoded = serde_json::from_str::<Value>(json).ok()?;
        decoded.as_array()?
    } else {
        value.as_array()?
    };
    let entries: Vec<_> = entries
        .iter()
        .map(|entry| {
            let hex = entry.as_str()?;
            if resolved {
                return public_key(hex);
            }
            (matches!(hex.len(), 2 | 4 | 6) && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
                .then(|| hex.to_ascii_lowercase())
        })
        .collect();
    Some(serde_json::to_string(&entries).expect("path entries are JSON strings or null"))
}

pub fn public_key(value: &str) -> Option<String> {
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

pub fn observer_key(value: &str) -> String {
    public_key(value).unwrap_or_else(|| value.to_owned())
}

pub fn string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

pub fn number(value: &Value, field: &str) -> Option<f64> {
    let value = value.get(field)?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite())
}

pub fn records<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| anyhow!("CoreScope response has no {field} list"))
}

// Preserve the snapshot objects as returned, excluding packet-content fields
// defensively in case a future server embeds packet examples in a snapshot.
pub fn snapshot_json(value: &Value) -> String {
    fn scrub(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.retain(|key, _| {
                    !matches!(
                        key.to_ascii_lowercase().as_str(),
                        "raw_hex"
                            | "rawhex"
                            | "decoded_json"
                            | "decodedjson"
                            | "message"
                            | "messages"
                            | "channel_messages"
                            | "neighbor_report"
                    )
                });
                fields.values_mut().for_each(scrub);
            }
            Value::Array(items) => items.iter_mut().for_each(scrub),
            _ => {}
        }
    }
    let mut value = value.clone();
    scrub(&mut value);
    value.to_string()
}

pub fn packet_page(value: Value, offset: usize) -> Result<PacketPage> {
    let page: PacketPage = serde_json::from_value(value)
        .map_err(|_| anyhow!("CoreScope returned an invalid packet page"))?;
    ensure!(
        page.offset == offset && page.limit > 1 && page.packets.len() <= page.limit,
        "CoreScope returned invalid packet pagination"
    );
    ensure!(
        page.packets.iter().all(|packet| packet.id > 0)
            && page.packets.windows(2).all(|pair| pair[0].id > pair[1].id),
        "CoreScope packets are out of descending ID order"
    );
    Ok(page)
}
