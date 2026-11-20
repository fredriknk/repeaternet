//! Read-side compatibility with coverage runs written by the Python server.
//!
//! The encoder below reproduces `web._plan_fingerprint`, which hashes
//! `json.dumps(value, sort_keys=True, separators=(",", ":"))` as UTF-8. It is
//! verified against `tests/fixtures/python-goldens.json`. Python runs are never
//! reusable by native workers, and they can never be current (see
//! [`reason::LEGACY_ROUTE_NOT_CURRENT`]).

use crate::identity::reason;
use crate::REUSE_FIELDS;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::io;
use std::path::{Component, Path};
use std::time::{SystemTime, UNIX_EPOCH};

/// `COVERAGE_MODEL_VERSION` of the Python reference. Native runs must not use it.
pub const PYTHON_MODEL_VERSION: &str = "coverage-v3";

/// The exact text Python hashes: compact, key-sorted, ASCII-escaped JSON with
/// `repr` floats. Returns `None` for numbers Python would emit as `Infinity`.
pub fn python_canonical_json(value: &Value) -> Option<String> {
    let mut output = String::new();
    write_json(&mut output, value)?;
    Some(output)
}

/// SHA-256 hex of [`python_canonical_json`], equal to `web._plan_fingerprint`.
pub fn python_fingerprint(value: &Value) -> Option<String> {
    let output = python_canonical_json(value)?;
    Some(format!("{:x}", Sha256::digest(output.as_bytes())))
}

fn write_json(output: &mut String, value: &Value) -> Option<()> {
    match value {
        Value::Object(fields) => {
            // Rust string order is UTF-8 byte order, which equals the code
            // point order Python uses for `sort_keys`.
            let mut fields: Vec<_> = fields.iter().collect();
            fields.sort_unstable_by_key(|(key, _)| *key);
            output.push('{');
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_string(output, key);
                output.push(':');
                write_json(output, value)?;
            }
            output.push('}');
        }
        Value::Array(items) => {
            output.push('[');
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write_json(output, item)?;
            }
            output.push(']');
        }
        Value::String(value) => write_string(output, value),
        Value::Number(number) => {
            // `arbitrary_precision` keeps the source token, which tells
            // Python ints (arbitrary size) from floats.
            let raw = number.as_str();
            if raw.contains(['.', 'e', 'E']) {
                write_float(output, raw.parse().ok()?)?;
            } else if raw == "-0" {
                output.push('0');
            } else {
                output.push_str(raw);
            }
        }
        Value::Bool(true) => output.push_str("true"),
        Value::Bool(false) => output.push_str("false"),
        Value::Null => output.push_str("null"),
    }
    Some(())
}

/// `ensure_ascii`: printable ASCII except `"` and `\` is literal. Everything else
/// is escaped. Short escapes are used for `\b\f\n\r\t`, and non-BMP characters
/// become UTF-16 surrogate pairs in lowercase hex.
fn write_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            ' '..='~' => output.push(character),
            _ => {
                let mut units = [0; 2];
                for unit in character.encode_utf16(&mut units) {
                    write!(output, "\\u{unit:04x}").expect("string writes");
                }
            }
        }
    }
    output.push('"');
}

/// CPython `float.__repr__`: shortest round-trip digits, scientific notation
/// when the decimal exponent is below -4 or at least 16, `e±XX` exponents.
fn write_float(output: &mut String, value: f64) -> Option<()> {
    if !value.is_finite() {
        return None;
    }
    // Rust `{:e}` prints the shortest round-trip digits. Re-rounding to that
    // precision with exact decimal conversion picks the nearest candidate,
    // which is what CPython's dtoa mode 0 returns.
    let shortest = format!("{value:e}");
    let (mantissa, _) = shortest.split_once('e')?;
    let digits = mantissa.bytes().filter(u8::is_ascii_digit).count();
    let precision = digits - 1;
    let rounded = format!("{value:.precision$e}");
    let (mantissa, exponent) = rounded.split_once('e')?;
    let exponent: i32 = exponent.parse().ok()?;
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    if negative {
        output.push('-');
    }
    if !(-4..16).contains(&exponent) {
        output.push_str(&digits[..1]);
        if digits.len() > 1 {
            output.push('.');
            output.push_str(&digits[1..]);
        }
        write!(
            output,
            "e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
        .expect("string writes");
    } else if exponent < 0 {
        output.push_str("0.");
        for _ in 0..(-exponent - 1) {
            output.push('0');
        }
        output.push_str(digits);
    } else {
        let point = exponent as usize + 1;
        if digits.len() > point {
            output.push_str(&digits[..point]);
            output.push('.');
            output.push_str(&digits[point..]);
        } else {
            output.push_str(digits);
            for _ in digits.len()..point {
                output.push('0');
            }
            output.push_str(".0");
        }
    }
    Some(())
}

/// `web._route_plan_fingerprint`: the plan without coverage-only fields.
pub fn python_route_fingerprint(plan: &Value) -> Option<String> {
    let mut route = plan.as_object()?.clone();
    for field in ["coverage", "coverage_targets", "resolved_search"] {
        route.remove(field);
    }
    python_fingerprint(&Value::Object(route))
}

/// `web._coverage_reuse_fingerprint`, for diagnostics only. It never authorizes
/// native reuse, because Python RF output is a different model.
pub fn python_reuse_fingerprint(job: &Value) -> Option<String> {
    let fields = REUSE_FIELDS
        .iter()
        .map(|&key| Some((key.to_owned(), job.get(key)?.clone())))
        .collect::<Option<_>>()?;
    python_fingerprint(&Value::Object(fields))
}

/// `web._terrain_fingerprint` from `(project-relative POSIX path, size, mtime_ns)`
/// entries. Python sorts the tuples, so the input order does not matter.
pub fn python_terrain_fingerprint(mut entries: Vec<(String, u64, i128)>) -> Value {
    entries.sort_unstable();
    json!(entries)
}

/// `os.stat().st_mtime_ns` for a modification time, including pre-1970 times.
pub fn python_mtime_ns(time: SystemTime) -> i128 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    }
}

/// One Python terrain tuple for `path` under `project`, following symbolic links
/// like `Path.stat()`. Paths must be UTF-8, relative to the project, and free of
/// `..` components. The caller still chooses the files (see the README).
pub fn python_terrain_entry(project: &Path, path: &Path) -> io::Result<(String, u64, i128)> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "terrain path outside project");
    let relative = path.strip_prefix(project).map_err(|_| invalid())?;
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str().ok_or_else(invalid)?),
            _ => return Err(invalid()),
        }
    }
    if parts.is_empty() {
        return Err(invalid());
    }
    let metadata = std::fs::metadata(path)?;
    Ok((
        parts.join("/"),
        metadata.len(),
        python_mtime_ns(metadata.modified()?),
    ))
}

/// Python writes manifests and reports with `allow_nan`, so fields that skip
/// `encode()` can hold `NaN`, `Infinity` or `-Infinity`. Replace those bare
/// tokens outside strings with `null`, as `encode()` does. Returns `None` when
/// there is nothing to replace.
pub(crate) fn python_nonfinite_to_null(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut replaced = false;
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            output.push(byte);
            if byte == b'\\' {
                if let Some(&next) = bytes.get(index + 1) {
                    output.push(next);
                    index += 1;
                }
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        let rest = &bytes[index..];
        if let Some(token) = [&b"-Infinity"[..], b"Infinity", b"NaN"]
            .into_iter()
            .find(|token| rest.starts_with(token))
        {
            output.extend_from_slice(b"null");
            index += token.len();
            replaced = true;
            continue;
        }
        in_string = byte == b'"';
        output.push(byte);
        index += 1;
    }
    replaced.then_some(output)
}

pub(crate) fn is_legacy(job: &Value) -> bool {
    job.get("coverage_store_format").is_none() && job["model_version"] == PYTHON_MODEL_VERSION
}

pub(crate) fn embedded_settings_match(job: &Value) -> bool {
    [
        ("settings", "settings_fingerprint"),
        ("radio_settings", "radio_fingerprint"),
    ]
    .iter()
    .all(|(value, digest)| {
        job[value].is_object()
            && python_fingerprint(&job[value]).is_some_and(|hash| job[digest] == hash)
    })
}

/// The live project and Python-format terrain fingerprint, used to explain why a
/// Python run is stale and to authorize Python-to-Python comparisons.
#[derive(Clone, Debug)]
pub struct LegacyIdentity {
    project_id: String,
    terrain: Value,
}

impl LegacyIdentity {
    /// Supply tuples for the live coverage mosaic (`coverage=True` file set).
    pub fn new(project_id: impl Into<String>, entries: Vec<(String, u64, i128)>) -> Self {
        Self::from_fingerprint(project_id, python_terrain_fingerprint(entries))
    }

    /// Use an already computed Python-format terrain fingerprint.
    pub fn from_fingerprint(project_id: impl Into<String>, terrain: Value) -> Self {
        Self {
            project_id: project_id.into(),
            terrain,
        }
    }

    pub fn terrain(&self) -> &Value {
        &self.terrain
    }

    pub fn terrain_matches(&self, job: &Value) -> bool {
        is_legacy(job)
            && !self.project_id.is_empty()
            && job["project_id"] == self.project_id
            && self.terrain.is_array()
            && job["terrain_fingerprint"] == self.terrain
    }

    /// Always a reason: a Python run is never current in a native server. The
    /// reason names the first identity that fails.
    pub fn stale_reason(&self, job: &Value) -> &'static str {
        if !is_legacy(job) {
            reason::UNSUPPORTED_FORMAT
        } else if job["stale"] != false || job["state"] == "stale" {
            reason::PERSISTED_STALE
        } else if self.project_id.is_empty() || job["project_id"] != self.project_id {
            reason::LEGACY_PROJECT
        } else if !self.terrain_matches(job) {
            reason::LEGACY_TERRAIN
        } else if !embedded_settings_match(job) {
            reason::LEGACY_EMBEDDED
        } else {
            reason::LEGACY_ROUTE_NOT_CURRENT
        }
    }

    pub(crate) fn project_id(&self) -> &str {
        &self.project_id
    }
}
