use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use chrono::DateTime;
use serde_json::Value;

pub const LONG_PATH: u32 = 1;
pub const MOBILE_RECEIVER: u32 = 2;
pub const BELOW_GROUND: u32 = 4;
pub const HIGH_SNR: u32 = 8;
pub const UNKNOWN_TX_DATUM: u32 = 16;
pub const NO_TERRAIN: u32 = 32;
pub const UNKNOWN_RX_DATUM: u32 = 64;
pub const RSSI_FLOOR: u32 = 128;

pub struct Observation {
    pub serial: String,
    pub frame: i64,
    pub observed_ms: i64,
    observed_us: i64,
    pub received_ms: Option<i64>,
    pub tx: [f64; 2],
    pub tx_alt: f64,
    pub tx_datum: &'static str,
    pub rx: [f64; 2],
    pub rx_alt: Option<f64>,
    pub receiver: String,
    pub hz: i64,
    pub frequency_basis: &'static str,
    pub sonde_type: String,
    pub modulation: Option<&'static str>,
    pub power: Option<f64>,
    pub decoder: String,
    pub version: Option<String>,
    pub antenna: Option<String>,
    pub lna: bool,
    pub snr: Option<f64>,
    pub rssi: Option<f64>,
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|f| f.is_finite())
}

fn coordinates(lat: f64, lon: f64) -> Option<[f64; 2]> {
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some([lat, lon])
}

fn receiver_position(value: &Value) -> Option<[f64; 2]> {
    if let Some(text) = value.as_str() {
        let (lat, lon) = text.split_once(',')?;
        return coordinates(lat.trim().parse().ok()?, lon.trim().parse().ok()?)
            .filter(|position| *position != [0.0, 0.0]);
    }
    if let Some(pair) = value.as_array()
        && let [lat, lon] = pair.as_slice()
    {
        return coordinates(number(lat)?, number(lon)?).filter(|position| *position != [0.0, 0.0]);
    }
    None
}

fn timestamp(value: &Value) -> Option<DateTime<chrono::FixedOffset>> {
    let text = value.as_str()?;
    DateTime::parse_from_rfc3339(text).ok()
}

fn text(value: &Value, limit: usize) -> Option<String> {
    let value = value.as_str()?.trim();
    (!value.is_empty()).then(|| value.chars().take(limit).collect())
}

impl Observation {
    fn parse(row: &Value, serial: &str) -> std::result::Result<Self, &'static str> {
        if row["serial"].as_str() != Some(serial) {
            return Err("invalid_serial");
        }
        let receiver = text(&row["uploader_callsign"], 128).ok_or("missing_receiver_id")?;
        if receiver.contains('@') || receiver.contains("://") {
            return Err("invalid_receiver_id");
        }
        let rx = receiver_position(&row["uploader_position"]).ok_or("invalid_receiver_position")?;
        let [lat, lon] = [number(&row["lat"]), number(&row["lon"])];
        let tx = lat
            .zip(lon)
            .and_then(|(lat, lon)| coordinates(lat, lon))
            .ok_or("invalid_tx_position")?;
        let tx_alt = number(&row["alt"]).ok_or("invalid_tx_altitude")?;
        let tx_datum = match row["ref_position"].as_str() {
            Some("GPS") => "WGS84_ellipsoid",
            Some("MSL") => "MSL",
            _ => "unknown",
        };
        let observed = timestamp(&row["datetime"]).ok_or("invalid_sonde_time")?;
        let frame = row["frame"]
            .as_i64()
            .or_else(|| row["frame"].as_str()?.parse().ok())
            .ok_or("invalid_frame")?;
        let snr = number(&row["snr"]);
        let rssi = number(&row["rssi"]);
        if snr.is_none() && rssi.is_none() {
            return Err("missing_signal");
        }
        let mhz = |name| number(&row[name]).filter(|mhz| (400.0..=406.0).contains(mhz));
        let (frequency, frequency_basis) = if let Some(freq) = mhz("tx_frequency") {
            (freq, "reported_tx")
        } else {
            (
                mhz("frequency").ok_or("missing_or_out_of_band_frequency")?,
                "receiver_estimate",
            )
        };
        let sonde_type = text(&row["subtype"], 64)
            .or_else(|| text(&row["type"], 64))
            .unwrap_or_else(|| "unknown".into());
        let power = match sonde_type.as_str() {
            "RS41-SG" | "RS41-SGP" | "iMet-4-AB" => Some(60.0),
            "DFM17" | "DFM-17" | "WxR-301D" | "iMS-100" => Some(100.0),
            "M20" => Some(150.0),
            "PS-B3" => Some(50.0),
            _ => None,
        };
        let modulation = match (row["type"].as_str(), sonde_type.as_str()) {
            (Some("RS41"), _) | (_, "RS41-SG" | "RS41-SGP" | "DFM17" | "DFM-17" | "iMet-4-AB") => {
                Some("GFSK")
            }
            _ => None,
        };
        let decoder = text(&row["software_name"], 64).map_or_else(
            || "unknown".into(),
            |name| match name.to_ascii_lowercase().as_str() {
                "radiosonde_auto_rx" => "radiosonde_auto_rx".into(),
                "rdzttgosonde" => "rdzTTGOsonde".into(),
                _ => name,
            },
        );
        let antenna = text(&row["uploader_antenna"], 512);
        let lna = antenna.as_ref().is_some_and(|a| {
            let words = a.split(|c: char| !c.is_ascii_alphanumeric());
            words.into_iter().any(|word| {
                let word = word.to_ascii_lowercase();
                word == "lna" || word.starts_with("preamp")
            })
        });
        Ok(Self {
            serial: serial.into(),
            frame,
            observed_ms: observed.timestamp_millis(),
            observed_us: observed.timestamp_micros(),
            received_ms: timestamp(&row["time_received"]).map(|time| time.timestamp_millis()),
            tx,
            tx_alt,
            tx_datum,
            rx,
            rx_alt: number(&row["uploader_alt"]),
            receiver,
            hz: (frequency * 1_000_000.0).round() as i64,
            frequency_basis,
            sonde_type,
            modulation,
            power,
            decoder,
            version: text(&row["software_version"], 64),
            antenna: antenna.and_then(redact_contact),
            lna,
            snr,
            rssi,
        })
    }
}

fn redact_contact(text: String) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if lower.contains('@')
        || lower.contains("://")
        || lower.contains("www.")
        || lower.contains("(at)")
        || lower.contains("[at]")
        || lower
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| {
                matches!(
                    word,
                    "contact" | "email" | "phone" | "whatsapp" | "telegram"
                )
            })
        || text
            .split(|c: char| c.is_alphabetic())
            .any(|token| token.chars().filter(char::is_ascii_digit).count() >= 7)
    {
        return None;
    }
    Some(text)
}

#[derive(Default)]
pub struct Signal {
    pub count: u32,
    pub min: Option<f64>,
    pub max: Option<f64>,
    sum: f64,
}

impl Signal {
    fn add(&mut self, sample: Option<f64>) {
        if let Some(sample) = sample {
            self.count += 1;
            self.min = Some(self.min.map_or(sample, |previous| previous.min(sample)));
            self.max = Some(self.max.map_or(sample, |previous| previous.max(sample)));
            self.sum += sample;
        }
    }

    pub fn mean(&self) -> Option<f64> {
        (self.count != 0).then(|| self.sum / f64::from(self.count))
    }
}

pub struct Bin {
    pub first: Observation,
    pub agl: Option<f64>,
    pub start: i64,
    pub seconds: u32,
    pub count: u32,
    pub snr: Signal,
    pub rssi: Signal,
    pub flags: u32,
}

#[derive(Default)]
pub struct Flight {
    pub input: usize,
    pub bins: Vec<Bin>,
    pub rejects: BTreeMap<&'static str, usize>,
}

fn km(a: [f64; 2], b: [f64; 2]) -> f64 {
    let [a_lat, a_lon] = a.map(f64::to_radians);
    let [b_lat, b_lon] = b.map(f64::to_radians);
    let h = ((a_lat - b_lat) / 2.0).sin().powi(2)
        + a_lat.cos() * b_lat.cos() * ((a_lon - b_lon) / 2.0).sin().powi(2);
    12_742.0 * h.clamp(0.0, 1.0).sqrt().asin()
}

pub fn bin(
    serial: &str,
    rows: Vec<Value>,
    low: u32,
    high: u32,
    mut height: impl FnMut(&Observation) -> Result<Option<f64>>,
) -> Result<Flight> {
    let mut flight = Flight {
        input: rows.len(),
        ..Flight::default()
    };
    let mut parsed = Vec::new();
    for row in rows {
        match Observation::parse(&row, serial) {
            Ok(row) => parsed.push(row),
            Err(reason) => *flight.rejects.entry(reason).or_default() += 1,
        }
    }
    parsed.sort_by_key(|row| {
        (
            row.observed_us,
            row.frame,
            row.received_ms.unwrap_or(i64::MAX),
        )
    });
    let mut seen = HashSet::new();
    parsed.retain(|row| {
        let unique = seen.insert((row.receiver.clone(), row.frame, row.observed_us));
        if !unique {
            *flight.rejects.entry("duplicate").or_default() += 1;
        }
        unique
    });
    drop(seen);

    let mut positions = HashMap::new();
    let mut mobile = HashSet::new();
    for row in &parsed {
        let (first, previous) = positions
            .entry(row.receiver.as_str())
            .or_insert((row.rx, row.rx));
        if km(*first, row.rx) > 1.0 || km(*previous, row.rx) > 1.0 {
            mobile.insert(row.receiver.clone());
        }
        *previous = row.rx;
    }

    let mut height_cache = HashMap::new();
    let mut bins = HashMap::new();
    for row in parsed {
        let key = (
            row.tx[0].to_bits(),
            row.tx[1].to_bits(),
            row.tx_alt.to_bits(),
            row.tx_datum,
        );
        let agl = match height_cache.entry(key) {
            std::collections::hash_map::Entry::Occupied(value) => *value.get(),
            std::collections::hash_map::Entry::Vacant(entry) => *entry.insert(height(&row)?),
        };
        let seconds = if agl.is_some_and(|agl| agl < 3_000.0) {
            low
        } else {
            high
        };
        let start = row.observed_ms.div_euclid(i64::from(seconds) * 1000) * i64::from(seconds);
        let flags = (if km(row.tx, row.rx) > 800.0 {
            LONG_PATH
        } else {
            0
        }) | (if mobile.contains(&row.receiver) {
            MOBILE_RECEIVER
        } else {
            0
        }) | (if agl.is_some_and(|agl| agl < -100.0) {
            BELOW_GROUND
        } else {
            0
        }) | (if row.snr.is_some_and(|snr| snr >= 60.0) {
            HIGH_SNR
        } else {
            0
        }) | (if row.tx_datum == "unknown" {
            UNKNOWN_TX_DATUM
        } else if agl.is_none() {
            NO_TERRAIN
        } else {
            0
        }) | UNKNOWN_RX_DATUM
            | (if row.decoder == "rdzTTGOsonde" && row.rssi.is_some_and(|rssi| rssi <= -127.5) {
                RSSI_FLOOR
            } else {
                0
            });
        let key = (row.receiver.clone(), row.decoder.clone(), seconds, start);
        let snr = row.snr;
        let rssi = row.rssi;
        let index = *bins.entry(key).or_insert_with(|| {
            let index = flight.bins.len();
            flight.bins.push(Bin {
                first: row,
                agl,
                start,
                seconds,
                count: 0,
                snr: Signal::default(),
                rssi: Signal::default(),
                flags: 0,
            });
            index
        });
        let bin = &mut flight.bins[index];
        bin.count += 1;
        bin.flags |= flags;
        bin.snr.add(snr);
        bin.rssi.add(rssi);
    }
    Ok(flight)
}

#[cfg(test)]
pub(crate) fn fixture() -> Result<Vec<Value>> {
    Ok(serde_json::from_str(include_str!(
        "../fixtures-flight.json"
    ))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_position_and_frequency_and_rejects_zero_position() -> Result<()> {
        let rows = fixture()?;
        let record = Observation::parse(&rows[0], "FIXTURE").map_err(anyhow::Error::msg)?;
        assert_eq!(record.rx, [60.0, 10.0]);
        assert_eq!(record.rx_alt, Some(120.0));
        assert_eq!(record.hz, 403_200_000);
        assert_eq!(record.frequency_basis, "reported_tx");
        assert_eq!(record.tx_datum, "MSL");
        assert_eq!(record.modulation, Some("GFSK"));
        assert_eq!(record.power, Some(60.0));
        assert!(record.lna);
        assert!(matches!(
            Observation::parse(&rows[6], "FIXTURE"),
            Err("invalid_receiver_position")
        ));
        assert!(receiver_position(&Value::Null).is_none());
        Ok(())
    }

    #[test]
    fn dedupes_then_bins_at_the_agl_threshold_and_keeps_distinct_receivers() -> Result<()> {
        let flight = bin("FIXTURE", fixture()?, 30, 300, |row| Ok(Some(row.tx_alt)))?;
        assert_eq!(flight.input, 7);
        assert_eq!(
            flight.rejects,
            BTreeMap::from([("duplicate", 1), ("invalid_receiver_position", 1)])
        );
        assert_eq!(flight.bins.len(), 3);
        let a = &flight.bins[0];
        assert_eq!(
            (
                a.seconds,
                a.count,
                a.first.snr,
                a.snr.min,
                a.snr.max,
                a.snr.mean()
            ),
            (30, 2, Some(13.0), Some(13.0), Some(21.0), Some(17.0))
        );
        let b = &flight.bins[1];
        assert_eq!(
            (b.first.receiver.as_str(), b.rssi.mean()),
            ("RECEIVER-B", Some(-127.5))
        );
        assert_ne!(b.flags & RSSI_FLOOR, 0);
        let high = &flight.bins[2];
        assert_eq!((high.seconds, high.count, high.agl), (300, 2, Some(3000.0)));
        assert_eq!(high.flags, HIGH_SNR | UNKNOWN_RX_DATUM);
        Ok(())
    }

    #[test]
    fn flags_mobile_receiver_long_paths_below_ground_and_unknown_datums() -> Result<()> {
        let rows = fixture()?;
        let mut first = rows[0].clone();
        let mut second = rows[3].clone();
        let mut unknown = rows[2].clone();
        first["uploader_position"] = serde_json::json!("40,10");
        second["alt"] = serde_json::json!(-101);
        unknown["ref_position"] = Value::Null;
        unknown["uploader_antenna"] = serde_json::json!("LNA contact receiver@example.invalid");
        let flight = bin("FIXTURE", vec![first, second, unknown], 30, 300, |row| {
            Ok((row.tx_datum != "unknown").then_some(row.tx_alt))
        })?;
        assert_eq!(
            flight.bins[0].flags,
            LONG_PATH | MOBILE_RECEIVER | BELOW_GROUND | UNKNOWN_RX_DATUM
        );
        assert_eq!(
            flight.bins[1].flags,
            UNKNOWN_TX_DATUM | UNKNOWN_RX_DATUM | RSSI_FLOOR
        );
        assert!(flight.bins[1].first.antenna.is_none());
        assert!(flight.bins[1].first.lna);
        let unavailable = bin("FIXTURE", vec![rows[0].clone()], 30, 300, |_| Ok(None))?;
        assert_eq!(unavailable.bins[0].flags, NO_TERRAIN | UNKNOWN_RX_DATUM);
        Ok(())
    }
}
