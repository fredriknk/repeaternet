use std::collections::BTreeMap;

use tiff_reader::Ifd;

use crate::{coordinates::CustomMethod, TerrainCrs};

use super::geotiff::{doubles, invalid, invalid_error, RasterResult};

const GEO_KEY_DIRECTORY: u16 = 34735;
const GEO_DOUBLE_PARAMS: u16 = 34736;
const GEO_ASCII_PARAMS: u16 = 34737;
const USER_DEFINED: u16 = 32767;

type GeoKeys = BTreeMap<u16, [u16; 3]>;

fn geo_keys(ifd: &Ifd) -> RasterResult<GeoKeys> {
    let keys = ifd
        .tag(GEO_KEY_DIRECTORY)
        .and_then(|tag| tag.value.as_u16_slice())
        .ok_or_else(|| invalid_error("Missing GeoTIFF key directory"))?;
    let [1, 1, 0 | 1, count, rest @ ..] = keys else {
        return invalid("Invalid or unsupported GeoTIFF key directory");
    };
    let entries = rest
        .get(..usize::from(*count) * 4)
        .ok_or_else(|| invalid_error("Truncated GeoTIFF key directory"))?;
    let mut values = BTreeMap::new();
    for &[id, location, count, value] in entries.as_chunks::<4>().0 {
        if values.insert(id, [location, count, value]).is_some() {
            return invalid(format!("Duplicate GeoTIFF key {id}"));
        }
    }
    Ok(values)
}

fn short_key(keys: &GeoKeys, id: u16) -> RasterResult<Option<u16>> {
    match keys.get(&id) {
        None => Ok(None),
        Some(&[0, 1, value]) => Ok(Some(value)),
        _ => invalid(format!("Invalid GeoTIFF SHORT key {id}")),
    }
}

pub(super) fn projected_crs(ifd: &Ifd) -> RasterResult<(TerrainCrs, bool)> {
    let keys = geo_keys(ifd)?;
    if short_key(&keys, 1024)? != Some(1) {
        return invalid("GeoTIFF must declare a projected CRS");
    }
    let values = doubles(ifd, GEO_DOUBLE_PARAMS)?.unwrap_or_default();
    let number = |id| -> RasterResult<Option<f64>> {
        match keys.get(&id) {
            None => Ok(None),
            Some(&[GEO_DOUBLE_PARAMS, 1, offset]) => values
                .get(usize::from(offset))
                .copied()
                .map(Some)
                .ok_or_else(|| invalid_error(format!("Missing GeoTIFF DOUBLE at key {id}")).into()),
            _ => invalid(format!("Invalid GeoTIFF DOUBLE key {id}")),
        }
    };
    let projected = short_key(&keys, 3072)?;
    let registered = projected.filter(|code| *code != USER_DEFINED);
    match (short_key(&keys, 3076)?, number(3077)?) {
        (Some(9001), None | Some(1.0)) | (Some(USER_DEFINED), Some(1.0)) => {}
        (None, None) if registered.is_some() => {}
        _ => return invalid("Projected GeoTIFF coordinates must use metres"),
    }
    let pixel_is_point = match short_key(&keys, 1025)? {
        None | Some(1) => false,
        Some(2) => true,
        _ => return invalid("Unsupported GeoTIFF raster type"),
    };
    for id in [1026, 2049, 3073, 4097] {
        if let Some(&[location, count, offset]) = keys.get(&id) {
            let start = usize::from(offset);
            let citation = ifd
                .tag(GEO_ASCII_PARAMS)
                .and_then(|tag| tag.value.as_str())
                .filter(|_| location == GEO_ASCII_PARAMS && count != 0)
                .and_then(|ascii| ascii.get(start..start + usize::from(count)))
                .ok_or_else(|| invalid_error(format!("Invalid GeoTIFF citation key {id}")))?;
            // GDAL can embed a complete coordinate operation in an ESRI citation.
            // A registered projected CRS uses its numeric definition. A custom
            // CRS with this citation would require a WKT reader to resolve it.
            if registered.is_none() && citation.contains("ESRI PE String = ") {
                return invalid("Unsupported embedded ESRI CRS definition");
            }
        }
    }
    let crs = match registered {
        Some(code) => {
            let crs = TerrainCrs::from_epsg(code)?;
            validate_key_set(&keys, &[], true)?;
            validate_geodetic(&keys, &number, crs.geographic_epsg())?;
            crs
        }
        None => custom_crs(&keys, &number)?,
    };
    Ok((crs, pixel_is_point))
}

fn validate_key_set(keys: &GeoKeys, parameters: &[u16], registered: bool) -> RasterResult<()> {
    for &key in keys.keys() {
        match key {
            1024..=1026 | 2048..=2061 | 3072 | 3073 | 3076 | 3077 | 4096..=4099 => {}
            3074 if !registered || short_key(keys, key)? == Some(USER_DEFINED) => {}
            3075 if !registered => {}
            _ if parameters.contains(&key) => {}
            _ => return invalid(format!("Unsupported or conflicting GeoTIFF key {key}")),
        }
    }
    Ok(())
}

fn validate_geodetic(
    keys: &GeoKeys,
    number: &impl Fn(u16) -> RasterResult<Option<f64>>,
    registered_base: Option<u16>,
) -> RasterResult<bool> {
    let base = short_key(keys, 2048)?.filter(|code| *code != USER_DEFINED);
    let datum = short_key(keys, 2050)?.filter(|code| *code != USER_DEFINED);
    let geographic = registered_base.or(base).or(match datum {
        Some(6326) => Some(4326),
        Some(6258) => Some(4258),
        _ => None,
    });
    let (expected_datum, ellipsoid, reciprocal_flattening) = match geographic {
        Some(4326) => (6326, 7030, 298.257_223_563),
        Some(4258) => (6258, 7019, 298.257_222_101),
        Some(4171) if registered_base.is_some() => (6171, 7019, 298.257_222_101),
        Some(4167) if registered_base.is_some() => (6167, 7019, 298.257_222_101),
        _ => return invalid("Unsupported GeoTIFF geographic datum"),
    };
    if base.is_some() && base != geographic || datum.is_some_and(|code| code != expected_datum) {
        return invalid("Conflicting GeoTIFF geographic CRS or datum");
    }
    let major = 6_378_137.0;
    let minor = major * (1.0 - 1.0 / reciprocal_flattening);
    if !matches!(short_key(keys, 2054)?, None | Some(9102))
        || !matches!(short_key(keys, 2060)?, None | Some(9102))
        || !matches!(short_key(keys, 2051)?, None | Some(8901))
        || !matches!(short_key(keys, 2052)?, None | Some(9001))
        || number(2053)?.is_some_and(|v| v != 1.0)
        || number(2055)?.is_some_and(|v| (v - 1_f64.to_radians()).abs() > 1e-15)
        || short_key(keys, 2056)?.is_some_and(|v| v != ellipsoid && v != USER_DEFINED)
        || number(2061)?.is_some_and(|v| v != 0.0)
        || number(2057)?.is_some_and(|v| (v - major).abs() > 1e-6)
        || number(2058)?.is_some_and(|v| (v - minor).abs() > 1e-6)
        || number(2059)?.is_some_and(|v| (v - reciprocal_flattening).abs() > 1e-9)
    {
        return invalid(
            "Unsupported or conflicting GeoTIFF angular units, prime meridian or ellipsoid",
        );
    }
    Ok(ellipsoid == 7019)
}

fn custom_crs(
    keys: &GeoKeys,
    number: &impl Fn(u16) -> RasterResult<Option<f64>>,
) -> RasterResult<TerrainCrs> {
    if !matches!(short_key(keys, 3074)?, None | Some(USER_DEFINED)) {
        return invalid("Custom GeoTIFF CRS requires explicit projection parameters");
    }
    use CustomMethod::*;
    let (method, parameters): (_, &[u16]) = match short_key(keys, 3075)? {
        Some(1) => (TransverseMercator, &[3080, 3081, 3082, 3083, 3092]),
        Some(7) if keys.contains_key(&3078) => (MercatorParallel, &[3080, 3081, 3082, 3083, 3078]),
        Some(7) => (MercatorScale, &[3080, 3081, 3082, 3083, 3092]),
        Some(8) => (
            LambertConformalTwoParallels,
            &[3084, 3085, 3086, 3087, 3078, 3079],
        ),
        Some(9) => (LambertConformalOneParallel, &[3080, 3081, 3082, 3083, 3092]),
        Some(10) => (LambertAzimuthalEqualArea, &[3088, 3089, 3082, 3083]),
        Some(11) => (AlbersEqualArea, &[3080, 3081, 3082, 3083, 3078, 3079]),
        Some(15) => (PolarStereographicScale, &[3095, 3081, 3082, 3083, 3092]),
        Some(16) => (ObliqueStereographic, &[3080, 3081, 3082, 3083, 3092]),
        Some(17) => (Equirectangular, &[3088, 3089, 3082, 3083, 3078]),
        Some(28) => (CylindricalEqualArea, &[3080, 0, 3082, 3083, 3078]),
        other => {
            return invalid(format!(
                "Unsupported custom GeoTIFF projection method {other:?}"
            ))
        }
    };
    validate_key_set(keys, parameters, false)?;
    let grs80 = validate_geodetic(keys, number, None)?;
    let mut values = [0.0; 6];
    for (&id, value) in parameters.iter().zip(&mut values) {
        if id != 0 {
            *value = number(id)?
                .ok_or_else(|| invalid_error(format!("Missing custom GeoTIFF parameter {id}")))?;
        }
    }
    let [lon, lat, x, y, p, q] = values;
    let method = if method == PolarStereographicScale && lat.abs() < 90.0 {
        PolarStereographicParallel
    } else {
        method
    };
    let valid_parallel = |angle: f64| angle.abs() < 90.0;
    let valid = match method {
        TransverseMercator | ObliqueStereographic => p > 0.0,
        MercatorScale => lat == 0.0 && p > 0.0,
        MercatorParallel => lat == 0.0 && valid_parallel(p),
        LambertConformalOneParallel => lat != 0.0 && valid_parallel(lat) && p > 0.0,
        LambertConformalTwoParallels | AlbersEqualArea => {
            valid_parallel(p) && valid_parallel(q) && p + q != 0.0
        }
        LambertAzimuthalEqualArea => lat == -90.0 || lat > -89.999_999_99,
        PolarStereographicScale => lat.abs() == 90.0 && p > 0.0,
        PolarStereographicParallel => lat != 0.0 && p == 1.0,
        Equirectangular | CylindricalEqualArea => valid_parallel(p),
    };
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) || !valid {
        return invalid("Invalid custom GeoTIFF projection parameters");
    }
    if method == PolarStereographicParallel {
        values = [lon, 90.0_f64.copysign(lat), x, y, lat, 0.0];
    }
    Ok(TerrainCrs::custom(method, grs80, values)?)
}
