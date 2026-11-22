//! Projected site coordinates and antenna elevations.

use serde::{Deserialize, Serialize};

use crate::TerminalClutter;

/// A projected coordinate in metres.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    /// Planar separation in metres.
    pub fn distance_to(self, other: Self) -> f64 {
        (other.x - self.x).hypot(other.y - self.y)
    }
}

/// Site role used to select a default antenna configuration.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum SiteKind {
    EndpointA,
    EndpointB,
    Client,
    Router,
    #[default]
    Candidate,
}

/// Elevation datum for the antenna mounting height.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub enum HeightReference {
    #[default]
    GroundDtm,
    SurfaceDom,
}

/// RF-relevant site geometry and identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Site {
    pub id: String,
    pub position: Point,
    pub kind: SiteKind,
    pub ground_elevation_m: f64,
    pub surface_elevation_m: Option<f64>,
    /// Height above the selected terrain reference.
    pub antenna_height_m: f64,
    pub height_reference: HeightReference,
    #[serde(default)]
    pub terminal_clutter: TerminalClutter,
    /// Representative height of surrounding clutter above ground DTM.
    #[serde(default)]
    pub clutter_height_m: Option<f64>,
}

impl Site {
    /// Candidate site with the default 3 m antenna height and zero ground elevation.
    pub fn new(id: impl Into<String>, x: f64, y: f64) -> Self {
        Self {
            id: id.into(),
            position: Point { x, y },
            kind: SiteKind::Candidate,
            ground_elevation_m: 0.0,
            surface_elevation_m: None,
            antenna_height_m: 3.0,
            height_reference: HeightReference::GroundDtm,
            terminal_clutter: TerminalClutter::default(),
            clutter_height_m: None,
        }
    }

    /// Nonnegative height of the surface above the ground when surface data exists.
    pub fn obstruction_height_m(&self) -> Option<f64> {
        self.surface_elevation_m
            .map(|surface| (surface - self.ground_elevation_m).max(0.0))
    }

    /// Antenna elevation on the terrain datum. Missing surface elevations fall back to DTM.
    pub fn antenna_absolute_elevation_m(&self) -> f64 {
        let base = match self.height_reference {
            HeightReference::GroundDtm => self.ground_elevation_m,
            HeightReference::SurfaceDom => {
                self.surface_elevation_m.unwrap_or(self.ground_elevation_m)
            }
        };
        base + self.antenna_height_m
    }

    /// Planar site separation in metres.
    pub fn distance_to(&self, other: &Self) -> f64 {
        self.position.distance_to(other.position)
    }
}
