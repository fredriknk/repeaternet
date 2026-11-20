//! Export renderers for the saved coverage and active route wire DTOs.

pub mod coverage;
pub mod route;

use serde_json::Value;
use std::{
    fmt,
    io::{self, Write},
};

pub const MAX_COVERAGE_EXPORT_BYTES: u64 = 100 * 1024 * 1024;

/// XY order in both directions, with WGS84 coordinates in degrees.
pub type Transform<'a> = &'a dyn Fn([f64; 2]) -> Result<[f64; 2], ExportError>;

#[derive(Debug)]
pub enum ExportError {
    LimitExceeded,
    Invalid(String),
    Io(io::Error),
    Json(serde_json::Error),
    Csv(csv::Error),
    Png(png::EncodingError),
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded => {
                f.write_str("Coverage export exceeds the 100 MiB download limit")
            }
            Self::Invalid(message) => f.write_str(message),
            Self::Io(error) => error.fmt(f),
            Self::Json(error) => error.fmt(f),
            Self::Csv(error) => error.fmt(f),
            Self::Png(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::Csv(error) => Some(error),
            Self::Png(error) => Some(error),
            _ => None,
        }
    }
}

macro_rules! from_error {
    ($source:path, $variant:ident) => {
        impl From<$source> for ExportError {
            fn from(error: $source) -> Self {
                Self::$variant(error)
            }
        }
    };
}
from_error!(io::Error, Io);
from_error!(serde_json::Error, Json);
from_error!(csv::Error, Csv);
from_error!(png::EncodingError, Png);

fn invalid(message: impl Into<String>) -> ExportError {
    ExportError::Invalid(message.into())
}

fn rows(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

fn numeric<const N: usize>(value: &Value) -> Option<[f64; N]> {
    let fields = value.as_array()?;
    if fields.len() != N {
        return None;
    }
    let mut result = [0.0; N];
    for (out, field) in result.iter_mut().zip(fields) {
        *out = field.as_f64()?;
    }
    Some(result)
}

fn site_coordinate(site: &Value) -> Option<[f64; 2]> {
    Some([site["longitude"].as_f64()?, site["latitude"].as_f64()?])
}

fn project(transform: Transform<'_>, point: [f64; 2]) -> Result<[f64; 2], ExportError> {
    let coordinate = transform(point)?;
    if !coordinate.iter().all(|v| v.is_finite()) {
        return Err(invalid("Projection produced non-finite coordinates"));
    }
    Ok(coordinate)
}

fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#x27;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "—".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        _ => value.to_string(),
    }
}

fn html(value: &Value) -> String {
    escape(&display(value))
}

/// Build the entire export before exposing it to HTTP. The unnamed temporary
/// file is removed when its last handle is dropped, including on error.
pub fn spool(
    mut render: impl FnMut(&mut std::fs::File) -> Result<(), ExportError>,
) -> Result<std::fs::File, ExportError> {
    use std::io::Seek;
    let mut file = tempfile::tempfile()?;
    render(&mut file)?;
    file.rewind()?;
    Ok(file)
}

fn limited<W: Write>(
    output: W,
    render: impl FnOnce(&mut dyn Write) -> Result<(), ExportError>,
) -> Result<(), ExportError> {
    struct Capped<W> {
        output: W,
        remaining: u64,
        exceeded: bool,
    }
    impl<W: Write> Write for Capped<W> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() as u64 > self.remaining {
                self.exceeded = true;
                return Err(io::Error::other(
                    "Coverage export exceeds the 100 MiB download limit",
                ));
            }
            let written = self.output.write(bytes)?;
            self.remaining -= written as u64;
            Ok(written)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.output.flush()
        }
    }
    let mut output = Capped {
        output,
        remaining: MAX_COVERAGE_EXPORT_BYTES,
        exceeded: false,
    };
    let result = render(&mut output);
    if output.exceeded {
        Err(ExportError::LimitExceeded)
    } else {
        result
    }
}
