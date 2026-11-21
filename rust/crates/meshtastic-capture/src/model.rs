use chrono::DateTime;
use serde_json::Value;

pub type NodeId = u32;

#[derive(Default)]
pub struct Batch {
    pub positions: Vec<Position>,
    pub radios: Vec<(NodeId, i64, Radio)>,
    pub measurements: Vec<Measurement>,
}

pub struct Position {
    pub node: NodeId,
    pub time: i64,
    pub lat: f64,
    pub lon: f64,
    pub altitude: Option<f64>,
    pub bits: Option<u32>,
    pub radius: Option<f64>,
    pub source: &'static str,
}

impl Position {
    pub fn validate(mut self) -> Option<Self> {
        if !(-90.0..=90.0).contains(&self.lat)
            || !(-180.0..=180.0).contains(&self.lon)
            || (self.lat == 0.0 && self.lon == 0.0)
        {
            return None;
        }
        if self.radius.is_none() {
            // Half the diagonal of the coordinate cell, in metres.
            self.radius = self.bits.map(|bits| {
                let half_width = 2_f64.powi(31 - bits as i32) * 0.011132;
                half_width * 1_f64.hypot(self.lat.to_radians().cos())
            });
        }
        Some(self)
    }
}

#[derive(Default)]
pub struct Radio {
    pub region: Option<String>,
    pub preset: Option<String>,
    pub frequency_mhz: Option<f64>,
    pub slot: Option<u32>,
    pub sf: Option<u32>,
    pub bw_khz: Option<f64>,
    pub cr: Option<String>,
    pub tx_power_dbm: Option<f64>,
    pub hardware: Option<String>,
    pub firmware: Option<String>,
    pub role: Option<String>,
}

impl Radio {
    pub fn read(v: &Value) -> Self {
        Self {
            region: region(first(v, &["region_name", "region", "lora_region"])),
            preset: preset(first(v, &["modem_preset_name", "modem_preset"])),
            frequency_mhz: number(first(v, &["frequency_mhz", "frequency"])).and_then(|f| {
                let mhz = if f > 10_000.0 { f / 1_000_000.0 } else { f };
                (100.0..=3000.0).contains(&mhz).then_some(mhz)
            }),
            slot: unsigned(first(v, &["frequency_slot", "channel_num", "slot"])),
            sf: unsigned(first(v, &["spread_factor", "sf"])),
            bw_khz: number(first(v, &["bandwidth_khz", "bandwidth", "bw"])),
            cr: token(first(v, &["coding_rate", "cr"])),
            tx_power_dbm: number(first(v, &["tx_power_dbm", "tx_power"])),
            hardware: token(first(
                v,
                &[
                    "hardware_model_name",
                    "hw_model",
                    "hardware_model",
                    "hardware",
                ],
            )),
            firmware: token(first(v, &["firmware_version", "firmware"])),
            role: token(first(v, &["role_name", "role"])),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    Gateway,
    Towards,
    Back,
    Neighbor,
}

impl Evidence {
    pub fn label(self) -> &'static str {
        match self {
            Self::Gateway => "gateway_rx",
            Self::Towards => "traceroute_towards",
            Self::Back => "traceroute_back",
            Self::Neighbor => "neighbor",
        }
    }
}

#[derive(Clone)]
pub struct Packet {
    pub id: String,
    pub logical_id: Option<String>,
    pub time: i64,
    pub gateway: Option<NodeId>,
    pub hop_start: Option<u32>,
    pub hop_limit: Option<u32>,
    pub relay_hint: Option<u32>,
    pub relay_state: Option<&'static str>,
    pub via_mqtt: Option<bool>,
    pub region: Option<String>,
    pub provenance: &'static str,
}

pub struct Measurement {
    pub packet: Packet,
    pub tx: Option<NodeId>,
    pub rx: NodeId,
    pub evidence: Evidence,
    pub hop: Option<usize>,
    pub snr: Option<f64>,
    pub rssi: Option<f64>,
}

impl Measurement {
    pub fn key(&self) -> String {
        match self.evidence {
            Evidence::Gateway => format!("{}/{}", self.packet.id, self.evidence.label()),
            Evidence::Neighbor => format!(
                "{}/neighbor/{}",
                self.packet.id,
                self.tx.unwrap_or_default()
            ),
            _ => format!(
                "{}/{}/{}",
                self.packet.id,
                self.evidence.label(),
                self.hop.unwrap_or_default()
            ),
        }
    }
}

pub fn first<'a>(v: &'a Value, keys: &[&str]) -> &'a Value {
    keys.iter()
        .find_map(|key| v.get(key).filter(|field| !field.is_null()))
        .unwrap_or(&Value::Null)
}

pub fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str()?.parse().ok())
        .filter(|n| n.is_finite())
}

pub fn unsigned(v: &Value) -> Option<u32> {
    v.as_u64()
        .and_then(|n| n.try_into().ok())
        .or_else(|| v.as_str()?.parse().ok())
}

pub fn node(v: &Value) -> Option<NodeId> {
    let id = unsigned(v).or_else(|| hex_node(v))?;
    valid_id(id)
}

pub fn hex_node(v: &Value) -> Option<NodeId> {
    let value = v.as_str()?.trim_start_matches('!').trim_start_matches("0x");
    valid_id(u32::from_str_radix(value, 16).ok()?)
}

fn valid_id(id: NodeId) -> Option<NodeId> {
    (id != u32::MAX && id != 0).then_some(id)
}

pub fn token(v: &Value) -> Option<String> {
    if let Some(n) = v.as_u64() {
        return Some(n.to_string());
    }
    let value = v.as_str()?;
    (!value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-/.:+".contains(&b)))
    .then(|| value.to_owned())
}

pub fn timestamp(v: &Value) -> Option<i64> {
    let ms = if let Some(value) = v
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        value.timestamp_millis()
    } else {
        let n = number(v)? as i64;
        match n {
            1..=9_999_999_999 => n * 1000,
            10_000_000_000..=9_999_999_999_999 => n,
            _ => n / 1000,
        }
    };
    (ms > 0).then_some(ms)
}

pub fn bits(v: &Value) -> Option<u32> {
    unsigned(v).filter(|n| (1..=32).contains(n))
}

pub fn signal(v: &Value, scale: f64) -> Option<f64> {
    number(v)
        .filter(|snr| *snr != -128.0)
        .map(|snr| snr / scale)
}

pub fn old_firmware(firmware: Option<&str>) -> bool {
    let mut parts = firmware
        .unwrap_or_default()
        .trim_start_matches('v')
        .split(['.', '-', '+']);
    let version = (
        parts.next().and_then(|n| n.parse::<u32>().ok()),
        parts.next().and_then(|n| n.parse::<u32>().ok()),
        parts.next().and_then(|n| n.parse::<u32>().ok()),
    );
    matches!(version, (Some(major), Some(minor), Some(patch)) if (major, minor, patch) < (2, 3, 2))
}

pub fn region(v: &Value) -> Option<String> {
    const REGIONS: &[&str] = &[
        "UNSET",
        "US",
        "EU_433",
        "EU_868",
        "CN",
        "JP",
        "ANZ",
        "KR",
        "TW",
        "RU",
        "IN",
        "NZ_865",
        "TH",
        "LORA_24",
        "UA_433",
        "UA_868",
        "MY_433",
        "MY_919",
        "SG_923",
        "PH_433",
        "PH_868",
        "PH_915",
        "ANZ_433",
        "KZ_433",
        "KZ_863",
        "NP_865",
        "BR_902",
        "ITU1_2M",
        "ITU2_2M",
        "EU_866",
        "EU_874",
        "EU_917",
        "EU_N_868",
        "ITU3_2M",
        "ITU1_70CM",
    ];
    let code = if let Some(n) = unsigned(v) {
        REGIONS.get(n as usize)?.to_string()
    } else {
        token(v)?.to_ascii_uppercase()
    };
    (code != "UNSET" && REGIONS.contains(&code.as_str())).then_some(code)
}

fn preset(v: &Value) -> Option<String> {
    const PRESETS: &[&str] = &[
        "LongFast",
        "LongSlow",
        "VeryLongSlow",
        "MediumSlow",
        "MediumFast",
        "ShortSlow",
        "ShortFast",
        "LongModerate",
        "ShortTurbo",
        "LongTurbo",
        "LiteFast",
        "LiteSlow",
        "NarrowFast",
        "NarrowSlow",
        "TinyFast",
        "TinySlow",
        "MediumTurbo",
    ];
    if let Some(n) = unsigned(v) {
        return PRESETS.get(n as usize).map(|p| p.to_string());
    }
    let code = token(v)?.replace(['_', '-'], "");
    PRESETS
        .iter()
        .find(|p| p.eq_ignore_ascii_case(&code))
        .map(|p| p.to_string())
}
