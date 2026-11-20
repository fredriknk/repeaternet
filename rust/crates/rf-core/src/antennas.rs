//! Constant-gain and measured elevation antenna patterns.

use std::{io::Read, path::Path};

use crate::{require_positive, RfError};

/// Gain in dBi, either constant or a function of elevation.
#[derive(Clone, Debug, PartialEq)]
pub enum AntennaPattern {
    ConstantGain(f64),
    Elevation(ElevationPattern),
}

impl AntennaPattern {
    /// Evaluate gain at the given elevation angle in degrees.
    pub fn gain_at(&self, elevation_deg: f64) -> f64 {
        match self {
            Self::ConstantGain(gain_dbi) => *gain_dbi,
            Self::Elevation(pattern) => pattern.gain_at(elevation_deg),
        }
    }

    /// Greatest gain across all elevations, for optimistic route screening.
    pub fn peak_gain_dbi(&self) -> f64 {
        match self {
            Self::ConstantGain(gain_dbi) => *gain_dbi,
            Self::Elevation(pattern) => pattern
                .gain_dbi
                .iter()
                .copied()
                .fold(f64::NEG_INFINITY, f64::max),
        }
    }
}

/// A piecewise linear gain curve sampled at increasing elevation angles.
#[derive(Clone, Debug, PartialEq)]
pub struct ElevationPattern {
    elevation_deg: Vec<f64>,
    gain_dbi: Vec<f64>,
}

impl ElevationPattern {
    /// Construct a pattern from at least two finite angle/gain samples.
    pub fn new(elevation_deg: Vec<f64>, gain_dbi: Vec<f64>) -> Result<Self, RfError> {
        if elevation_deg.len() < 2 || elevation_deg.len() != gain_dbi.len() {
            return Err(RfError::InvalidInput(
                "An elevation pattern needs at least two matching samples",
            ));
        }
        if elevation_deg
            .iter()
            .chain(&gain_dbi)
            .any(|x| !x.is_finite())
        {
            return Err(RfError::InvalidInput("Antenna samples must be finite"));
        }
        if elevation_deg.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(RfError::InvalidInput(
                "Pattern elevation angles must be strictly increasing",
            ));
        }
        Ok(Self {
            elevation_deg,
            gain_dbi,
        })
    }

    /// Load the `elevation_deg` and `gain_dbi` columns of a UTF-8 CSV file.
    pub fn from_csv(path: impl AsRef<Path>) -> Result<Self, RfError> {
        Self::read_csv(csv::ReaderBuilder::new().flexible(true).from_path(path)?)
    }

    /// Read CSV samples from a stream, sorting them by elevation.
    pub fn from_csv_reader(reader: impl Read) -> Result<Self, RfError> {
        Self::read_csv(csv::ReaderBuilder::new().flexible(true).from_reader(reader))
    }

    fn read_csv<R: Read>(mut csv: csv::Reader<R>) -> Result<Self, RfError> {
        let headers = csv.headers()?;
        let mut angle_column = None;
        let mut gain_column = None;
        for (index, name) in headers.iter().enumerate() {
            match name {
                "elevation_deg" => angle_column = Some(index),
                "gain_dbi" => gain_column = Some(index),
                _ => {}
            }
        }
        let (Some(angle_column), Some(gain_column)) = (angle_column, gain_column) else {
            return Err(RfError::InvalidInput(
                "CSV must contain elevation_deg and gain_dbi columns",
            ));
        };
        let mut samples = Vec::new();
        for row in csv.records() {
            let row = row?;
            let value = |index, column| {
                row.get(index)
                    .and_then(|value| value.trim().parse::<f64>().ok())
                    .ok_or_else(|| RfError::InvalidAntennaCsvValue {
                        line: row.position().map_or(0, csv::Position::line),
                        column,
                    })
            };
            samples.push((
                value(angle_column, "elevation_deg")?,
                value(gain_column, "gain_dbi")?,
            ));
        }
        samples.sort_by(|left, right| left.0.total_cmp(&right.0));
        let (angles, gains) = samples.into_iter().unzip();
        Self::new(angles, gains)
    }

    /// Interpolate gain, using the nearest endpoint gain outside the sampled range.
    pub fn gain_at(&self, elevation_deg: f64) -> f64 {
        if elevation_deg.is_nan() {
            return f64::NAN;
        }
        let upper = self
            .elevation_deg
            .partition_point(|&angle| angle < elevation_deg);
        if upper == 0 {
            return self.gain_dbi[0];
        }
        if upper == self.elevation_deg.len() || self.elevation_deg[upper] == elevation_deg {
            return self.gain_dbi[upper.min(self.elevation_deg.len() - 1)];
        }
        let lower = upper - 1;
        let slope = (self.gain_dbi[upper] - self.gain_dbi[lower])
            / (self.elevation_deg[upper] - self.elevation_deg[lower]);
        self.gain_dbi[lower] + slope * (elevation_deg - self.elevation_deg[lower])
    }
}

/// Signed elevation angle from the transmitter to the receiver in degrees.
pub fn elevation_angle_deg(
    horizontal_distance_m: f64,
    tx_elevation_m: f64,
    rx_elevation_m: f64,
) -> Result<f64, RfError> {
    require_positive(
        horizontal_distance_m,
        "Horizontal distance must be positive",
    )?;
    Ok((rx_elevation_m - tx_elevation_m)
        .atan2(horizontal_distance_m)
        .to_degrees())
}
