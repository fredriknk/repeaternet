//! File contracts shared by every calibration stage.
//!
//! These records use JSON Lines under the calibration work directory
//! (see [`crate::paths`]). Each stage reads the previous stage's files and
//! never edits them in place. Field names are part of the contract with the
//! Python tools in `tools/rf-calibration/`; change them together with
//! `docs/RF_CALIBRATION.md`.
//!
//! Identities are pseudonymous: `node` is the first 16 hex digits of
//! `SHA-256(salt || source_namespace || ":" || lowercase(raw_id))`. The salt lives
//! in `<work>/salt.txt`. Exact coordinates stay in the work directory only.

use serde::{Deserialize, Serialize};

/// Schema version written into every record. Bump on incompatible changes.
pub const SCHEMA_VERSION: u32 = 1;

/// Where a measurement came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// CoreScope reception whose final resolved relay is the transmitter.
    CorescopeLastHop,
    /// CoreScope zero-hop public advert: the advert sender is the transmitter.
    CorescopeZeroHop,
    /// CoreScope pair that had opportunities to be heard but never was.
    CorescopeNonDetection,
    /// Meshtastic gateway reception with a confirmed transmitter.
    MeshtasticGatewayRx,
    /// Meshtastic traceroute leg (towards or back).
    MeshtasticTraceroute,
    /// Meshtastic neighbour-info report.
    MeshtasticNeighbor,
}

/// Which terrain family will be used for the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Region {
    /// Both endpoints inside the Kartverket NHM coverage polygon.
    Norway,
    /// Anything else (global DEM such as Copernicus GLO-30).
    World,
}

/// Robust summary of one per-packet quantity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stat {
    pub n: u32,
    pub median: f64,
    pub mean: f64,
    pub p10: f64,
    pub p90: f64,
    /// Median absolute deviation, scaled by 1.4826 (sigma-equivalent).
    pub mad: f64,
    pub min: f64,
    pub max: f64,
}

/// Radio configuration best known for the link. `field_basis` identifies the
/// source of each populated field, and `basis` lists the distinct sources.
/// Unknown values are `null`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Radio {
    /// Reported regulatory region (for example Meshtastic `EU_868`).
    #[serde(default)]
    pub lora_region: Option<String>,
    pub freq_mhz: Option<f64>,
    pub bw_khz: Option<f64>,
    pub sf: Option<u8>,
    pub cr: Option<u8>,
    pub tx_power_dbm: Option<f64>,
    pub modem_preset: Option<String>,
    pub basis: Vec<String>,
    #[serde(default)]
    pub field_basis: std::collections::BTreeMap<String, String>,
}

/// One endpoint of a link.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    /// Pseudonymous node id (see module docs).
    pub node: String,
    pub lat: f64,
    pub lon: f64,
    /// Where the position came from (`advert`, `node_snapshot`,
    /// `observer_snapshot`, `meshtastic_position`, ...).
    pub pos_source: String,
    /// Reported horizontal uncertainty in metres, if any.
    pub pos_precision_m: Option<f64>,
    /// Reported altitude above mean sea level, if any (GPS, often noisy).
    pub altitude_m: Option<f64>,
    /// Role as reported (`repeater`, `room`, `companion`, `observer`,
    /// `router`, `client`, ...).
    pub role: Option<String>,
    /// Position quality flags, see `docs/RF_CALIBRATION.md`.
    pub flags: Vec<String>,
}

/// One directed link `tx -> rx` aggregated over the capture window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LinkRecord {
    pub schema: u32,
    /// Stable id: `<source>:<tx node>><rx node>:<evidence>`.
    pub link_id: String,
    /// `corescope:<host>` or `meshtastic:<host>`.
    pub source: String,
    pub evidence: Evidence,
    pub region: Region,
    pub tx: Endpoint,
    pub rx: Endpoint,
    pub distance_m: f64,
    pub radio: Radio,
    /// Number of receptions aggregated (0 for non-detections).
    pub n: u32,
    pub first_ms: Option<i64>,
    pub last_ms: Option<i64>,
    /// Reported packet RSSI (total in-band power), dBm.
    pub rssi_dbm: Option<Stat>,
    /// Reported LoRa SNR, dB.
    pub snr_db: Option<Stat>,
    /// Per-packet signal power estimate, dBm: `rssi + snr - 10*log10(1 + 10^(snr/10))`
    /// when both are known (removes the noise contribution near the floor).
    pub signal_dbm: Option<Stat>,
    /// Per-packet noise estimate, dBm: `rssi - 10*log10(1 + 10^(snr/10))`.
    pub noise_dbm: Option<Stat>,
    /// Detection evidence: receptions versus opportunities, when inferable.
    pub detection: Option<Detection>,
    /// Link-level quality flags (`mqtt_suspect`, `over_300km`, `moved`,
    /// `duplicate_position`, `few_samples`, ...).
    pub flags: Vec<String>,
}

/// How often a link was heard when it could have been.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Detection {
    pub heard: u32,
    pub opportunities: u32,
    /// How opportunities were counted (for example
    /// `relayed_packets_while_rx_active`).
    pub basis: String,
}

/// Latest known state for one node in one capture source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub schema: u32,
    pub node: String,
    pub source: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub role: Option<String>,
    pub radio: Radio,
    /// Reported or estimated noise floor, dBm.
    pub noise_floor_dbm: Option<f64>,
    /// Number of distinct positions seen and their spread in metres.
    pub position_count: u32,
    pub position_spread_m: Option<f64>,
    pub flags: Vec<String>,
}

/// Sampled terrain for one undirected endpoint pair, written by
/// `rf-calibration profiles` to `profiles.jsonl`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileRecord {
    pub schema: u32,
    /// `<node a>~<node b>` with `a < b` lexically.
    pub path_id: String,
    pub link_ids: Vec<String>,
    /// `kartverket` (DTM and DOM) or `glo30` / another global surface model.
    pub terrain: String,
    /// EPSG code of the projected CRS the profile was sampled in.
    pub crs_epsg: u32,
    /// Native terrain resolution in metres.
    pub resolution_m: f64,
    /// Uniform sample spacing along the great-circle-free projected chord.
    pub step_m: f64,
    pub distance_m: f64,
    pub a: ProfileEnd,
    pub b: ProfileEnd,
    /// Bare-ground elevations from `a` to `b`, metres, rounded to 0.01.
    pub dtm: Vec<f64>,
    /// Surface elevations (trees and buildings) or `null` when unavailable.
    pub dom: Option<Vec<f64>>,
    /// Fraction of samples that had to be filled (NaN in the source).
    pub missing_fraction: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileEnd {
    pub node: String,
    pub lat: f64,
    pub lon: f64,
    pub x: f64,
    pub y: f64,
    /// Ground at the endpoint (DTM), metres.
    pub ground_m: f64,
    /// Surface at the endpoint (DOM), metres, if available.
    pub surface_m: Option<f64>,
}

/// One engine prediction for one directed link, written to
/// `predictions/<engine>.jsonl` by Rust and Python engines alike.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PredictionRecord {
    pub schema: u32,
    pub link_id: String,
    pub path_id: String,
    /// `rfcore_bullington`, `p526_delta_bullington`, `deygout3`,
    /// `giovaneli`, `epstein_peterson`, `itm_p2p`, `p1812`, ...
    pub engine: String,
    /// `dtm` or `dom` (obstacle surface used for the path interior).
    pub surface: String,
    /// `none`, `rfcore_clutter`, `dom_p2108`, `p2108`, `p833`, ...
    pub clutter: String,
    pub tx_height_m: f64,
    pub rx_height_m: f64,
    pub k_factor: f64,
    pub step_m: f64,
    pub freq_mhz: f64,
    /// Free-space loss over the 3-D antenna separation, dB.
    pub fspl_db: f64,
    /// Loss above free space from terrain (diffraction, scatter, ...), dB.
    pub excess_db: f64,
    /// Additional terminal clutter loss, dB (0 for `clutter = none`).
    pub clutter_db: f64,
    /// `fspl_db + excess_db + clutter_db`.
    pub total_db: f64,
    /// Largest Fresnel-Kirchhoff v of the dominant edge, if defined.
    pub v_max: Option<f64>,
    pub line_of_sight: Option<bool>,
    /// Engine-specific diagnostics (mode, warnings, edge positions).
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub diag: serde_json::Value,
}
