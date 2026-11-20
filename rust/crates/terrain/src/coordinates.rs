//! Offline transforms between WGS84 longitude/latitude and terrain coordinates.

use std::fmt;

use proj4rs::{adaptors::transform_xy, proj::Proj};

use crate::{Point, TerrainError};

/// The projected metre coordinate systems supported by the terrain backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerrainCrs {
    code: u16,
    custom: Option<CustomProjection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CustomProjection {
    method: CustomMethod,
    grs80: bool,
    parameters: [u64; 6],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CustomMethod {
    TransverseMercator,
    MercatorScale,
    MercatorParallel,
    LambertConformalTwoParallels,
    LambertConformalOneParallel,
    LambertAzimuthalEqualArea,
    AlbersEqualArea,
    PolarStereographicScale,
    PolarStereographicParallel,
    ObliqueStereographic,
    Equirectangular,
    CylindricalEqualArea,
}

const USER_DEFINED: u16 = 32767;

fn utm(code: u16) -> Option<(u16, &'static str, &'static str)> {
    match code {
        32601..=32660 => Some((code - 32600, "WGS84", "")),
        32701..=32760 => Some((code - 32700, "WGS84", " +south")),
        25828..=25837 => Some((code - 25800, "GRS80", "")),
        _ => None,
    }
}

fn epsg_definition(code: u16) -> Option<(u16, &'static str)> {
    include_str!("epsg.txt").lines().find_map(|line| {
        let (id, rest) = line.split_once(' ')?;
        if id.parse::<u16>().ok()? != code {
            return None;
        }
        let (base, definition) = rest.split_once(' ')?;
        Some((base.parse().ok()?, definition))
    })
}

#[allow(non_upper_case_globals)]
impl TerrainCrs {
    pub const Etrs89Utm32: Self = Self::epsg_unchecked(25832);
    pub const Etrs89Utm33: Self = Self::epsg_unchecked(25833);
    pub const Etrs89Utm35: Self = Self::epsg_unchecked(25835);

    const fn epsg_unchecked(code: u16) -> Self {
        Self { code, custom: None }
    }

    pub fn from_epsg(code: u16) -> Result<Self, TerrainError> {
        if utm(code).is_some() || epsg_definition(code).is_some() {
            Ok(Self::epsg_unchecked(code))
        } else {
            Err(TerrainError::UnsupportedCrs(code))
        }
    }

    /// The EPSG code, if registered. Custom CRSs have no authority code.
    pub fn epsg_code(self) -> Option<u16> {
        self.custom.is_none().then_some(self.code)
    }

    /// Returns the GeoTIFF user-defined marker (32767) for a custom CRS.
    pub fn epsg(self) -> u16 {
        self.code
    }

    pub(crate) fn geographic_epsg(self) -> Option<u16> {
        match self.code {
            25828..=25837 => Some(4258),
            32601..=32660 | 32701..=32760 => Some(4326),
            code => epsg_definition(code).map(|(base, _)| base),
        }
    }

    pub(crate) fn custom(
        method: CustomMethod,
        grs80: bool,
        parameters: [f64; 6],
    ) -> Result<Self, TerrainError> {
        let crs = Self {
            code: USER_DEFINED,
            custom: Some(CustomProjection {
                method,
                grs80,
                parameters: parameters.map(|v| if v == 0.0 { 0 } else { v.to_bits() }),
            }),
        };
        crs.projected()?;
        Ok(crs)
    }

    fn projected(self) -> Result<Proj, TerrainError> {
        let definition = if let Some(custom) = self.custom {
            let [lon, lat, x, y, p, q] = custom.parameters.map(f64::from_bits);
            let ellps = if custom.grs80 { "GRS80" } else { "WGS84" };
            use CustomMethod::*;
            let (method, extra) = match custom.method {
                TransverseMercator => ("etmerc", format!("+k={p}")),
                MercatorScale => ("merc", format!("+k={p}")),
                MercatorParallel => ("merc", format!("+lat_ts={p}")),
                LambertConformalTwoParallels => ("lcc", format!("+lat_1={p} +lat_2={q}")),
                LambertConformalOneParallel => ("lcc", format!("+lat_1={lat} +k={p}")),
                LambertAzimuthalEqualArea => ("laea", String::new()),
                AlbersEqualArea => ("aea", format!("+lat_1={p} +lat_2={q}")),
                PolarStereographicScale => ("stere", format!("+k={p}")),
                PolarStereographicParallel => ("stere", format!("+lat_ts={p}")),
                ObliqueStereographic => ("sterea", format!("+k={p}")),
                Equirectangular => {
                    // Extended TM on its central meridian gives the ellipsoidal
                    // meridional arc and its inverse used by EPSG method 1028.
                    return Proj::from_proj_string(&format!(
                        "+proj=etmerc +ellps={ellps} +lat_0={lat} +y_0={y}"
                    ))
                    .map_err(TerrainError::Projection);
                }
                CylindricalEqualArea => ("cea", format!("+lat_ts={p}")),
            };
            format!(
                "+proj={method} +ellps={ellps} +lon_0={lon} +lat_0={lat} +x_0={x} +y_0={y} {extra}"
            )
        } else if let Some((zone, ellps, south)) = utm(self.code) {
            format!("+proj=utm +ellps={ellps} +zone={zone}{south}")
        } else {
            let (_, definition) =
                epsg_definition(self.code).ok_or(TerrainError::UnsupportedCrs(self.code))?;
            if self.code == 4087 {
                return Proj::from_proj_string("+proj=etmerc +ellps=WGS84")
                    .map_err(TerrainError::Projection);
            }
            return Proj::from_proj_string(definition).map_err(TerrainError::Projection);
        };
        Proj::from_proj_string(&definition).map_err(TerrainError::Projection)
    }

    fn equidistant_cylindrical(self) -> Option<EquidistantCylindrical> {
        let (lon, x, parallel, reciprocal_flattening) = if self.code == 4087 {
            (0.0, 0.0, 0.0, 298.257_223_563)
        } else {
            let custom = self.custom?;
            if custom.method != CustomMethod::Equirectangular {
                return None;
            }
            let [lon, _, x, _, parallel, _] = custom.parameters.map(f64::from_bits);
            let reciprocal_flattening = if custom.grs80 {
                298.257_222_101
            } else {
                298.257_223_563
            };
            (lon, x, parallel, reciprocal_flattening)
        };
        let flattening = 1.0 / reciprocal_flattening;
        let eccentricity_squared = flattening * (2.0 - flattening);
        let (sin, cos) = parallel.to_radians().sin_cos();
        Some(EquidistantCylindrical {
            central_meridian: lon.to_radians(),
            false_easting: x,
            metres_per_radian: 6_378_137.0 * cos / (1.0 - eccentricity_squared * sin * sin).sqrt(),
        })
    }

    /// Use the planner's longitude bands, including its assignment of nominal
    /// zone 34 to terrain CRS EPSG:25833.
    pub fn for_norway_longitude(longitude: f64) -> Result<Self, TerrainError> {
        if !(-180.0..=180.0).contains(&longitude) {
            return Err(TerrainError::InvalidInput(
                "Longitude must be between -180 and 180 degrees",
            ));
        }
        let zone = ((longitude + 180.0) / 6.0).floor() + 1.0;
        Ok(if zone <= 32.0 {
            Self::Etrs89Utm32
        } else if zone <= 34.0 {
            Self::Etrs89Utm33
        } else {
            Self::Etrs89Utm35
        })
    }
}

impl fmt::Display for TerrainCrs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.epsg_code() {
            Some(code) => write!(f, "EPSG:{code}"),
            None => f.write_str("custom projected metre CRS"),
        }
    }
}

pub fn norway_utm_epsg(longitude: f64) -> Result<u16, TerrainError> {
    TerrainCrs::for_norway_longitude(longitude).map(TerrainCrs::epsg)
}

/// Geographic XY coordinate. Both fields are in decimal degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LonLat {
    pub longitude: f64,
    pub latitude: f64,
}

/// Reusable offline transform pair for horizontal longitude/latitude and easting/northing.
pub struct Transformer {
    crs: TerrainCrs,
    geographic: Proj,
    projected: Proj,
    equidistant_cylindrical: Option<EquidistantCylindrical>,
}

struct EquidistantCylindrical {
    central_meridian: f64,
    false_easting: f64,
    metres_per_radian: f64,
}

fn wrap_longitude(longitude: f64) -> f64 {
    use std::f64::consts::{PI, TAU};
    if (-PI..=PI).contains(&longitude) {
        longitude
    } else {
        (longitude + PI).rem_euclid(TAU) - PI
    }
}

impl Transformer {
    pub fn new(crs: TerrainCrs) -> Result<Self, TerrainError> {
        let geographic = Proj::from_proj_string("+proj=longlat +ellps=WGS84")
            .map_err(TerrainError::Projection)?;
        let projected = crs.projected()?;
        Ok(Self {
            crs,
            geographic,
            projected,
            equidistant_cylindrical: crs.equidistant_cylindrical(),
        })
    }

    pub fn for_norway_longitude(longitude: f64) -> Result<Self, TerrainError> {
        Self::new(TerrainCrs::for_norway_longitude(longitude)?)
    }

    pub fn crs(&self) -> TerrainCrs {
        self.crs
    }

    pub fn forward(&self, point: LonLat) -> Result<Point, TerrainError> {
        if !(-180.0..=180.0).contains(&point.longitude) || !(-90.0..=90.0).contains(&point.latitude)
        {
            return Err(TerrainError::InvalidInput(
                "Expected valid WGS84 longitude and latitude",
            ));
        }
        // proj4rs uses longitude/latitude in radians for geographic coordinates.
        let lat = point.latitude.to_radians();
        let lon = point.longitude.to_radians();
        let (mut x, y) = transform_xy(
            &self.geographic,
            &self.projected,
            if self.equidistant_cylindrical.is_some() {
                0.0
            } else {
                lon
            },
            lat,
        )
        .map_err(TerrainError::Projection)?;
        if let Some(eqc) = &self.equidistant_cylindrical {
            let delta = wrap_longitude(lon - eqc.central_meridian);
            x = eqc.false_easting + eqc.metres_per_radian * delta;
        }
        if !x.is_finite() || !y.is_finite() {
            return Err(TerrainError::InvalidInput(
                "Projection produced non-finite coordinates",
            ));
        }
        Ok(Point { x, y })
    }

    pub fn reverse(&self, point: Point) -> Result<LonLat, TerrainError> {
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(TerrainError::InvalidInput(
                "Expected finite projected coordinates",
            ));
        }
        let x = if self.equidistant_cylindrical.is_some() {
            0.0
        } else {
            point.x
        };
        let (mut longitude, latitude) = transform_xy(&self.projected, &self.geographic, x, point.y)
            .map_err(TerrainError::Projection)?;
        if let Some(eqc) = &self.equidistant_cylindrical {
            let delta = (point.x - eqc.false_easting) / eqc.metres_per_radian;
            if delta.abs() > std::f64::consts::PI + 1e-12 {
                return Err(TerrainError::InvalidInput(
                    "Easting lies outside the projection domain",
                ));
            }
            longitude = wrap_longitude(eqc.central_meridian + delta);
        }
        if !longitude.is_finite()
            || !(-std::f64::consts::FRAC_PI_2..=std::f64::consts::FRAC_PI_2).contains(&latitude)
        {
            return Err(TerrainError::InvalidInput(
                "Projection produced invalid geographic coordinates",
            ));
        }
        Ok(LonLat {
            longitude: longitude.to_degrees(),
            latitude: latitude.to_degrees(),
        })
    }
}
