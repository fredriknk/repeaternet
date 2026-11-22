use std::{
    num::NonZeroUsize,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};

use lru::LruCache;
use repeaternet_coverage::live::ElevationLookup;
use repeaternet_terrain::{Point, RasterTerrain, TerrainLayer, TerrainSource};

use crate::{
    api::{ApiError, ApiResult},
    terrain_http::{TerrainContext, terrain_invalid},
};

const SIDE: usize = 256;

#[derive(PartialEq, Eq, Hash)]
struct TerrainKey {
    directory: PathBuf,
    fingerprint: String,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct TileKey {
    terrain: Arc<TerrainKey>,
    level: u32,
    x: i32,
    y: i32,
}

struct Tile {
    ground: Vec<f32>,
    surface: Option<Vec<f32>>,
}

impl Tile {
    fn bytes(&self) -> usize {
        (self.ground.len() + self.surface.as_ref().map_or(0, Vec::len)) * size_of::<f32>()
    }

    fn build(
        raster: &RasterTerrain,
        key: &TileKey,
        step: f64,
        points: &mut Vec<Point>,
    ) -> ApiResult<Self> {
        points.clear();
        let start_x = f64::from(key.x) * SIDE as f64;
        let start_y = f64::from(key.y) * SIDE as f64;
        for row in 0..SIDE {
            let y = (start_y + row as f64 + 0.5) * step;
            points.extend((0..SIDE).map(|column| Point {
                x: (start_x + column as f64 + 0.5) * step,
                y,
            }));
        }
        let sample = |layer| {
            raster
                .sample(points, layer)
                .map_err(terrain_invalid)
                .map(|samples| samples.into_iter().map(|value| value as f32).collect())
        };
        Ok(Self {
            ground: sample(TerrainLayer::GroundDtm)?,
            surface: raster
                .has_surface()
                .then(|| sample(TerrainLayer::SurfaceDom))
                .transpose()?,
        })
    }
}

struct Tiles {
    lru: LruCache<TileKey, Arc<Tile>>,
    bytes: usize,
}

pub(crate) struct LiveTerrainCache {
    rasters: Mutex<LruCache<Arc<TerrainKey>, Arc<RasterTerrain>>>,
    tiles: Mutex<Tiles>,
    budget: usize,
}

impl LiveTerrainCache {
    pub fn from_env() -> Self {
        let budget = std::env::var("RF_PLANNER_LIVE_COVERAGE_CACHE_MB")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(512)
            .saturating_mul(1024 * 1024);
        Self {
            rasters: Mutex::new(LruCache::new(NonZeroUsize::new(16).unwrap())),
            tiles: Mutex::new(Tiles {
                lru: LruCache::unbounded(),
                bytes: 0,
            }),
            budget,
        }
    }

    pub fn open(&self, context: &TerrainContext) -> ApiResult<LiveRaster<'_>> {
        let key = Arc::new(TerrainKey {
            directory: context.directory.clone(),
            fingerprint: context.fingerprint.to_string(),
        });
        let mut rasters = self.rasters.lock().unwrap();
        let raster = if let Some(raster) = rasters.get(&key) {
            raster.clone()
        } else {
            let raster = Arc::new(context.open()?);
            rasters.put(key.clone(), raster.clone());
            raster
        };
        Ok(LiveRaster {
            cache: self,
            key,
            raster,
        })
    }

    fn retain(&self, key: TileKey, tile: Arc<Tile>) {
        let bytes = tile.bytes();
        if bytes > self.budget {
            return;
        }
        let mut tiles = self.tiles.lock().unwrap();
        if tiles.lru.get(&key).is_some() {
            return;
        }
        while tiles.bytes > self.budget - bytes {
            if let Some((_, tile)) = tiles.lru.pop_lru() {
                tiles.bytes -= tile.bytes();
            }
        }
        tiles.bytes += bytes;
        tiles.lru.put(key, tile);
    }
}

pub(crate) struct LiveRaster<'a> {
    cache: &'a LiveTerrainCache,
    key: Arc<TerrainKey>,
    pub raster: Arc<RasterTerrain>,
}

impl LiveRaster<'_> {
    pub fn tiles(&self, center: Point, radius: f64, resolution: f64) -> ApiResult<TileLookup> {
        let mut step = self.raster.resolution_m().max(5.0);
        let mut level = 0;
        while step * 2.0 <= resolution {
            step *= 2.0;
            level += 1;
        }

        // Bilinear interpolation can reach a half-cell beyond the sample area.
        let tile_span = step * SIDE as f64;
        let tile_index = |v: f64| (v / tile_span).floor() as i32;
        let extent = radius + step;
        let left = tile_index(center.x - extent);
        let right = tile_index(center.x + extent);
        let bottom = tile_index(center.y - extent);
        let top = tile_index(center.y + extent);
        let columns = (right - left + 1) as usize;
        let mut tiles = vec![None; columns * (top - bottom + 1) as usize];
        let mut missing = Vec::new();
        let footprints: Vec<_> = self
            .raster
            .ground_tiles()
            .iter()
            .map(|tile| tile.metadata().bounds)
            .collect();
        {
            let mut cached = self.cache.tiles.lock().unwrap();
            for (i, tile) in tiles.iter_mut().enumerate() {
                let x = left + (i % columns) as i32;
                let y = bottom + (i / columns) as i32;
                let west = f64::from(x) * tile_span;
                let south = f64::from(y) * tile_span;
                let east = west + tile_span;
                let north = south + tile_span;
                let dx = center.x - center.x.clamp(west, east);
                let dy = center.y - center.y.clamp(south, north);
                if dx * dx + dy * dy > extent * extent
                    || !footprints.iter().any(|b| {
                        b.min_x <= east && b.max_x >= west && b.min_y <= north && b.max_y >= south
                    })
                {
                    continue;
                }
                let key = TileKey {
                    terrain: self.key.clone(),
                    level,
                    x,
                    y,
                };
                if let Some(found) = cached.lru.get(&key) {
                    *tile = Some(found.clone());
                } else {
                    missing.push((i, key));
                }
            }
        }
        if !missing.is_empty() {
            let workers = thread::available_parallelism().map_or(1, |cpus| (cpus.get() - 1).max(1));
            let batch = missing.len().div_ceil(workers);
            thread::scope(|scope| -> ApiResult<()> {
                let jobs: Vec<_> = missing
                    .chunks(batch)
                    .map(|keys| {
                        scope.spawn(move || {
                            let mut points = Vec::with_capacity(SIDE * SIDE);
                            keys.iter()
                                .map(|(i, key)| {
                                    let tile = Arc::new(Tile::build(
                                        &self.raster,
                                        key,
                                        step,
                                        &mut points,
                                    )?);
                                    self.cache.retain(key.clone(), tile.clone());
                                    Ok((*i, tile))
                                })
                                .collect::<ApiResult<Vec<_>>>()
                        })
                    })
                    .collect();
                for job in jobs {
                    for (i, tile) in job
                        .join()
                        .map_err(|_| ApiError::internal("Live terrain tile worker panicked"))??
                    {
                        tiles[i] = Some(tile);
                    }
                }
                Ok(())
            })?;
        }
        Ok(TileLookup {
            tiles,
            left,
            bottom,
            columns,
            inverse_step: step.recip(),
            has_surface: self.raster.has_surface(),
        })
    }
}

pub(crate) struct TileLookup {
    tiles: Vec<Option<Arc<Tile>>>,
    left: i32,
    bottom: i32,
    columns: usize,
    inverse_step: f64,
    has_surface: bool,
}

impl TileLookup {
    fn tile(&self, x: i32, y: i32) -> Option<&Tile> {
        let column = (x >> 8) - self.left;
        let row = (y >> 8) - self.bottom;
        if column < 0 || column as usize >= self.columns || row < 0 {
            return None;
        }
        self.tiles
            .get(row as usize * self.columns + column as usize)?
            .as_deref()
    }

    fn sample<const SURFACE: bool>(&self, point: Point) -> (f64, f64) {
        let cx = point.x * self.inverse_step - 0.5;
        let cy = point.y * self.inverse_step - 0.5;
        let x = cx.floor() as i32;
        let y = cy.floor() as i32;
        let fx = cx - f64::from(x);
        let fy = cy - f64::from(y);
        let Some(tile) = self.tile(x, y) else {
            return (f64::NAN, f64::NAN);
        };
        let value = |tile: &Tile, x: i32, y: i32| {
            let i = (y as usize & (SIDE - 1)) * SIDE + (x as usize & (SIDE - 1));
            (
                f64::from(tile.ground[i]),
                if SURFACE {
                    f64::from(tile.surface.as_ref().unwrap()[i])
                } else {
                    f64::NAN
                },
            )
        };
        let corner = |sx: i32, sy: i32| {
            if ((x ^ sx) | (y ^ sy)) & !(SIDE as i32 - 1) == 0 {
                value(tile, sx, sy)
            } else {
                self.tile(sx, sy)
                    .map_or((f64::NAN, f64::NAN), |t| value(t, sx, sy))
            }
        };
        let mix =
            |a: (f64, f64), b: (f64, f64), w: f64| (a.0 + (b.0 - a.0) * w, a.1 + (b.1 - a.1) * w);
        let east = x + i32::from(fx != 0.0);
        let lower = mix(value(tile, x, y), corner(east, y), fx);
        if fy == 0.0 {
            lower
        } else {
            mix(lower, mix(corner(x, y + 1), corner(east, y + 1), fx), fy)
        }
    }
}

impl ElevationLookup for TileLookup {
    fn ground(&self, point: Point) -> f64 {
        self.sample::<false>(point).0
    }

    fn surface(&self, point: Point) -> Option<f64> {
        self.has_surface.then(|| self.sample::<true>(point).1)
    }

    fn has_surface(&self) -> bool {
        self.has_surface
    }

    fn elevations(&self, point: Point) -> (f64, f64) {
        if self.has_surface {
            let (ground, surface) = self.sample::<true>(point);
            (ground, if surface.is_finite() { surface } else { ground })
        } else {
            let ground = self.ground(point);
            (ground, ground)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bilinear_lookup_across_tile_boundary_and_missing_corners() {
        let mut west = Tile {
            ground: vec![10.0; SIDE * SIDE],
            surface: Some(vec![12.0; SIDE * SIDE]),
        };
        let mut east = Tile {
            ground: vec![30.0; SIDE * SIDE],
            surface: Some(vec![34.0; SIDE * SIDE]),
        };
        west.ground[2 * SIDE - 1] = 20.0;
        east.ground[SIDE] = 40.0;
        east.surface.as_mut().unwrap()[SIDE] = f32::NAN;
        east.ground[2 * SIDE] = f32::NAN;
        let lookup = TileLookup {
            tiles: vec![Some(Arc::new(west)), Some(Arc::new(east))],
            left: -1,
            bottom: 0,
            columns: 2,
            inverse_step: 1.0,
            has_surface: true,
        };
        assert_eq!(lookup.elevations(Point { x: 0.0, y: 0.5 }), (20.0, 23.0));
        assert_eq!(lookup.elevations(Point { x: 0.0, y: 1.0 }), (25.0, 25.0));
        assert_eq!(lookup.elevations(Point { x: 0.5, y: 1.5 }), (40.0, 40.0));
        assert!(lookup.ground(Point { x: 0.5, y: 2.0 }).is_nan());
    }
}
