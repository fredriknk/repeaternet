use std::{path::Path, sync::Arc};

use quick_xml::{events::Event, Reader};
use repeaternet_terrain::{
    Bounds, FileSource, LonLat, Point, SourceCrs, SourceRaster, Transformer,
};
use reqwest::{
    blocking::Client,
    header::{HeaderMap, CONTENT_TYPE},
};
use tempfile::NamedTempFile;

use crate::{invalid, response_error, transport, ProviderError, Result};

use super::{
    downloader::{FetchContext, PROVIDER},
    hash,
    schema::parse_crs,
    DownloadTile, TerrainProvider, Wcs, WcsFormat, WcsVersion,
};

const BODY_LIMIT: u64 = 512 * 1024 * 1024;

#[derive(Debug)]
struct Request {
    endpoint: String,
    query: Vec<(String, String)>,
    crs: SourceCrs,
}

fn request(
    wcs: &Wcs,
    provider: &TerrainProvider,
    target: &Transformer,
    tile: DownloadTile,
    latitude: f64,
) -> Result<Request> {
    let target_code = target.crs().to_string();
    let name = wcs
        .crs_codes()
        .find(|&code| code == target_code)
        .or_else(|| wcs.crs_codes().next())
        .ok_or_else(|| invalid("WCS has no native CRS"))?;
    let crs = parse_crs(name).ok_or_else(|| invalid("Unsupported WCS CRS"))?;
    let (endpoint, coverage) = if let Some(service) = wcs.per_crs.get(name) {
        (&service.endpoint, &service.coverage)
    } else {
        (
            wcs.endpoint
                .as_ref()
                .ok_or_else(|| invalid("Missing WCS endpoint"))?,
            wcs.coverage
                .as_ref()
                .ok_or_else(|| invalid("Missing WCS coverage name"))?,
        )
    };
    let (bounds, width, height) = if crs == SourceCrs::Projected(target.crs()) {
        (tile.bounds, tile.width_pixels, tile.height_pixels)
    } else {
        source_grid(crs, target, tile, provider.resolution_m, latitude)?
    };
    let format = match wcs.format {
        WcsFormat::GeoTiff => "GeoTIFF",
        WcsFormat::ImageTiff => "image/tiff",
    };
    let mut query = wcs.params.clone().into_iter().collect::<Vec<_>>();
    let version = match wcs.version {
        WcsVersion::V100 => "1.0.0",
        WcsVersion::V201 => "2.0.1",
    };
    let mut put = |key: &str, value: String| query.push((key.into(), value));
    put("service", "WCS".into());
    put("version", version.into());
    put("request", "GetCoverage".into());
    put("format", format.into());
    let [west, south, east, north] = bounds.as_array().map(transport::py_float);
    match wcs.version {
        WcsVersion::V100 => {
            put("coverage", coverage.clone());
            put("crs", name.into());
            put("bbox", [west, south, east, north].join(","));
            put("width", width.to_string());
            put("height", height.to_string());
        }
        WcsVersion::V201 => {
            let axes = wcs
                .axis_labels
                .as_ref()
                .map(|labels| labels.each_ref().map(String::as_str))
                .unwrap_or_else(|| match crs {
                    SourceCrs::Geographic(_) => ["Long", "Lat"],
                    SourceCrs::Projected(_) => ["E", "N"],
                });
            let [x, y] = axes;
            let code = name.strip_prefix("EPSG:").expect("parsed EPSG identifier");
            let uri = format!("http://www.opengis.net/def/crs/EPSG/0/{code}");
            put("coverageId", coverage.clone());
            put("subset", format!("{x}({west},{east})"));
            put("subset", format!("{y}({south},{north})"));
            put("subsettingCrs", uri.clone());
            put("outputCrs", uri);
            put("scaleSize", format!("{x}({width}),{y}({height})"));
        }
    }
    Ok(Request {
        endpoint: endpoint.clone(),
        query,
        crs,
    })
}

fn source_grid(
    crs: SourceCrs,
    target: &Transformer,
    tile: DownloadTile,
    native_resolution: f64,
    latitude: f64,
) -> Result<(Bounds, u64, u64)> {
    let source = crs.transformer()?;
    let [west, south, east, north] = tile.bounds.as_array();
    let width = east - west;
    let height = north - south;
    let desired = (width / tile.width_pixels as f64)
        .min(height / tile.height_pixels as f64)
        .max(native_resolution);
    let mut out = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    let center = target.reverse(Point {
        x: west + width / 2.0,
        y: south + height / 2.0,
    })?;
    for i in 0..=16 {
        let t = f64::from(i) / 16.0;
        for (x, y) in [
            (west, south + height * t),
            (east, south + height * t),
            (west + width * t, south),
            (west + width * t, north),
        ] {
            let ll = target.reverse(Point { x, y })?;
            let p = match &source {
                Some(source) => source.forward(ll)?,
                None => {
                    let lon = center.longitude
                        + (ll.longitude - center.longitude + 180.0).rem_euclid(360.0)
                        - 180.0;
                    Point {
                        x: lon,
                        y: ll.latitude,
                    }
                }
            };
            out = [
                out[0].min(p.x),
                out[1].min(p.y),
                out[2].max(p.x),
                out[3].max(p.y),
            ];
        }
    }
    let (mx, my) = if source.is_some() {
        (1.0, 1.0)
    } else {
        (
            111_320.0 * latitude.to_radians().cos().abs().max(0.001),
            111_132.0,
        )
    };
    let [mut min_x, mut min_y, mut max_x, mut max_y] = [
        out[0] - 2.0 * desired / mx,
        out[1] - 2.0 * desired / my,
        out[2] + 2.0 * desired / mx,
        out[3] + 2.0 * desired / my,
    ];
    if source.is_none() {
        min_x = min_x.max(-180.0);
        max_x = max_x.min(180.0);
        min_y = min_y.max(-90.0);
        max_y = max_y.min(90.0);
    }
    let width = ((max_x - min_x) * mx / desired).ceil().max(1.0) as u64;
    let height = ((max_y - min_y) * my / desired).ceil().max(1.0) as u64;
    if width.saturating_mul(height) > 16_000_000 {
        return Err(invalid(
            "Transformed WCS request exceeds the source pixel limit",
        ));
    }
    Ok((
        Bounds {
            min_x,
            min_y,
            max_x,
            max_y,
        },
        width,
        height,
    ))
}

pub(super) struct Fetch<'a> {
    pub client: &'a Client,
    pub root: &'a Path,
    pub headers: &'a HeaderMap,
    pub context: &'a FetchContext,
}

impl Fetch<'_> {
    pub fn raster(
        &self,
        wcs: &Wcs,
        provider: &TerrainProvider,
        target: &Transformer,
        tile: DownloadTile,
        at: LonLat,
        resolution: f64,
    ) -> Result<SourceRaster> {
        let request = request(wcs, provider, target, tile, at.latitude)?;
        let key = hash(
            &serde_json::to_vec(&(&request.endpoint, &request.query))
                .expect("WCS request serializes"),
        );
        let path = self.root.join("wcs").join(format!("{}.tif", &key[..20]));
        let load = |path: &Path| -> Result<SourceRaster> {
            let source =
                FileSource::open(path).map_err(|e| response_error(PROVIDER, e.to_string()))?;
            let raster = SourceRaster::open(Arc::new(source), resolution, at.latitude)?;
            if raster.crs() != request.crs {
                return Err(response_error(
                    PROVIDER,
                    format!("WCS returned {}, expected {}", raster.crs(), request.crs),
                ));
            }
            // Decode the first block before publishing this WCS response into the cache.
            raster
                .read_window(0, 0, 1, 1)
                .map_err(|error| response_error(PROVIDER, error.to_string()))?;
            Ok(raster)
        };
        if path.is_file() {
            if let Ok(raster) = load(&path) {
                return Ok(raster);
            }
        }
        let response = self.context.send(
            self.client
                .get(&request.endpoint)
                .headers(self.headers.clone())
                .query(&request.query),
        )?;
        let status = response.status();
        let xml = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("xml"));
        if xml || !status.is_success() {
            let mut bytes = Vec::new();
            let _ = self.context.stream(response, &mut bytes, 128 * 1024);
            if let Some(message) = exception_text(&bytes) {
                return Err(response_error(PROVIDER, message));
            }
            if !status.is_success() {
                return Err(ProviderError::Http {
                    provider: PROVIDER,
                    status: status.as_u16(),
                });
            }
            return Err(response_error(
                PROVIDER,
                "WCS returned XML instead of GeoTIFF",
            ));
        }
        super::downloader::check_tiff_type(&response)?;
        let dir = path.parent().expect("WCS cache parent");
        std::fs::create_dir_all(dir)?;
        let mut staged = NamedTempFile::new_in(dir)?;
        self.context.stream(response, &mut staged, BODY_LIMIT)?;
        let raster = load(staged.path()).map_err(|error| {
            if let Ok(file) = std::fs::File::open(staged.path()) {
                use std::io::Read;
                let mut bytes = Vec::new();
                if file.take(128 * 1024).read_to_end(&mut bytes).is_ok() {
                    if let Some(message) = exception_text(&bytes) {
                        return response_error(PROVIDER, message);
                    }
                }
            }
            error
        })?;
        drop(raster);
        self.context.check()?;
        staged.persist(&path).map_err(|error| error.error)?;
        load(&path)
    }
}

fn exception_text(xml: &[u8]) -> Option<String> {
    if !xml
        .iter()
        .copied()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|byte| byte == b'<')
    {
        return None;
    }
    let mut reader = Reader::from_reader(xml);
    let mut inside = false;
    let mut messages = Vec::new();
    let mut message = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag))
                if matches!(
                    tag.local_name().as_ref(),
                    b"ExceptionText" | b"ServiceException"
                ) =>
            {
                inside = true
            }
            Ok(Event::End(tag))
                if matches!(
                    tag.local_name().as_ref(),
                    b"ExceptionText" | b"ServiceException"
                ) =>
            {
                inside = false;
                if !message.trim().is_empty() {
                    messages.push(message.trim().chars().take(1000).collect::<String>());
                }
                message.clear();
            }
            Ok(Event::Text(text)) if inside => message.push_str(&text.decode().ok()?),
            Ok(Event::CData(text)) if inside => message.push_str(&text.decode().ok()?),
            Ok(Event::GeneralRef(reference)) if inside => {
                if let Some(value) = reference.resolve_char_ref().ok()? {
                    message.push(value);
                } else if let Some(value) =
                    quick_xml::escape::resolve_xml_entity(&reference.decode().ok()?)
                {
                    message.push_str(value);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    (!messages.is_empty()).then(|| messages.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::{Product, ProductDefinition, ProviderRegistry};

    #[test]
    fn request_shapes_for_wcs_versions_and_source_crs() {
        let registry = ProviderRegistry::builtin();
        let provider = registry.provider("kartverket-nhm").unwrap();
        let Some(ProductDefinition::Wcs(base)) = provider.products.get(Product::Dtm) else {
            panic!()
        };
        let target = Transformer::new(repeaternet_terrain::TerrainCrs::Etrs89Utm32).unwrap();
        let tile = DownloadTile {
            bounds: Bounds {
                min_x: 590_000.0,
                min_y: 6_640_000.0,
                max_x: 590_050.0,
                max_y: 6_640_080.0,
            },
            width_pixels: 5,
            height_pixels: 8,
        };
        let req = request(base, provider, &target, tile, 59.9).unwrap();
        let get = |key: &str| {
            req.query
                .iter()
                .find(|(name, _)| name == key)
                .unwrap()
                .1
                .as_str()
        };
        assert_eq!(get("version"), "1.0.0");
        assert_eq!(get("coverage"), "nhm_dtm_topo_25832");
        assert_eq!(get("bbox"), "590000.0,6640000.0,590050.0,6640080.0");
        assert_eq!(get("width"), "5");
        assert_eq!(get("height"), "8");
        assert_eq!(get("crs"), "EPSG:25832");
        let mut modern = base.clone();
        modern.version = WcsVersion::V201;
        let req = request(&modern, provider, &target, tile, 59.9).unwrap();
        assert!(req
            .query
            .contains(&("coverageId".into(), "nhm_dtm_topo_25832".into())));
        assert!(req
            .query
            .contains(&("scaleSize".into(), "E(5),N(8)".into())));
        assert!(req
            .query
            .contains(&("subset".into(), "E(590000.0,590050.0)".into())));
        assert!(req
            .query
            .contains(&("subset".into(), "N(6640000.0,6640080.0)".into())));
        assert!(req.query.contains(&(
            "outputCrs".into(),
            "http://www.opengis.net/def/crs/EPSG/0/25832".into()
        )));
        modern.per_crs.clear();
        modern.native_crs = vec!["EPSG:4326".into()];
        modern.endpoint = Some("https://example.com/wcs".into());
        modern.coverage = Some("elevation".into());
        modern.axis_labels = Some(["Long".into(), "Lat".into()]);
        let req = request(&modern, provider, &target, tile, 59.9).unwrap();
        let subsets: Vec<_> = req
            .query
            .iter()
            .filter(|(name, _)| name == "subset")
            .map(|(_, val)| val)
            .collect();
        assert!(subsets[0].starts_with("Long(10."));
        assert!(subsets[1].starts_with("Lat(59."));
        assert!(req.query.contains(&(
            "subsettingCrs".into(),
            "http://www.opengis.net/def/crs/EPSG/0/4326".into()
        )));
        assert_eq!(req.crs, SourceCrs::Geographic(4326));
    }
}
