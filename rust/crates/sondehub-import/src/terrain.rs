use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, ensure};
use flate2::read::ZlibDecoder;
use reqwest::Url;

use crate::http::Http;

const ZOOM: u32 = 9;
const PIXELS: u32 = 256;
const MEMORY_TILES: usize = 32;
const WIDTH: usize = 360;
const HEIGHT: usize = 181;

pub struct Geoid(Vec<i16>);

impl Geoid {
    pub fn load() -> Result<Self> {
        let mut data = Vec::new();
        ZlibDecoder::new(include_bytes!("egm96-1deg.i16.zlib").as_slice())
            .read_to_end(&mut data)?;
        ensure!(data.len() == WIDTH * HEIGHT * 2, "Invalid EGM96 grid");
        Ok(Self(
            data.as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i16::from_le_bytes(*pair))
                .collect(),
        ))
    }

    pub fn undulation(&self, lat: f64, lon: f64) -> f64 {
        let row = (90.0 - lat).clamp(0.0, 180.0);
        let col = (lon + 180.0).rem_euclid(360.0);
        let (y, x) = (row as usize, col as usize);
        let (next_y, next_x) = ((y + 1).min(HEIGHT - 1), (x + 1) % WIDTH);
        let fraction_x = col.fract();
        let interpolate = |r: usize| {
            (1.0 - fraction_x) * f64::from(self.0[r * WIDTH + x])
                + fraction_x * f64::from(self.0[r * WIDTH + next_x])
        };
        ((1.0 - row.fract()) * interpolate(y) + row.fract() * interpolate(next_y)) / 100.0
    }
}

type Key = (u32, u32);

struct Tile {
    data: Vec<u8>,
    channels: usize,
}

pub struct Terrain {
    dir: PathBuf,
    capacity: u64,
    bytes: u64,
    disk: HashMap<Key, (SystemTime, u64)>,
    memory: VecDeque<(Key, Option<Tile>)>,
    last_disk_touch: Option<Key>,
}

impl Terrain {
    pub fn open(dir: PathBuf, capacity: u64) -> Result<Self> {
        fs::create_dir_all(&dir)?;
        let mut terrain = Self {
            dir,
            capacity,
            bytes: 0,
            disk: HashMap::new(),
            memory: VecDeque::new(),
            last_disk_touch: None,
        };
        for entry in fs::read_dir(&terrain.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some((x, y)) = name
                .to_str()
                .and_then(|s| s.strip_prefix("9-"))
                .and_then(|s| s.strip_suffix(".png"))
                .and_then(|s| s.split_once('-'))
            else {
                continue;
            };
            if let (Ok(x), Ok(y)) = (x.parse(), y.parse())
                && name == file_name((x, y)).as_str()
                && entry.file_type()?.is_file()
            {
                let meta = entry.metadata()?;
                terrain.disk.insert((x, y), (meta.modified()?, meta.len()));
                terrain.bytes += meta.len();
            }
        }
        terrain.prune(0)?;
        Ok(terrain)
    }

    fn remove(&mut self, key: Key) -> Result<()> {
        if let Err(err) = fs::remove_file(self.dir.join(file_name(key)))
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(err.into());
        }
        if let Some((_, bytes)) = self.disk.remove(&key) {
            self.bytes -= bytes;
        }
        Ok(())
    }

    fn prune(&mut self, incoming: u64) -> Result<()> {
        if self.bytes + incoming > self.capacity {
            let mut oldest: Vec<_> = self
                .disk
                .iter()
                .map(|(&key, &(time, _))| (time, key))
                .collect();
            oldest.sort_unstable();
            for (_, key) in oldest {
                self.remove(key)?;
                if self.bytes + incoming <= self.capacity {
                    break;
                }
            }
        }
        Ok(())
    }

    pub fn at(&mut self, lat: f64, lon: f64, http: &mut Http<'_>) -> Result<Option<f64>> {
        if lat.abs() > 85.051_128_78 {
            return Ok(None);
        }
        let world = (1 << ZOOM) * PIXELS;
        let extent = f64::from(world);
        let x = ((lon + 180.0).rem_euclid(360.0) / 360.0 * extent) as u32;
        let y = ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * extent)
            .clamp(0.0, extent - 1.0) as u32;
        let key = (x / PIXELS, y / PIXELS);
        let item = match self.memory.iter().position(|(cached, _)| *cached == key) {
            Some(index) => self.memory.remove(index).expect("cached index exists"),
            None => {
                let tile = self.load(key, http)?;
                if self.memory.len() == MEMORY_TILES {
                    self.memory.pop_back();
                }
                (key, tile)
            }
        };
        let elevation = item.1.as_ref().map(|tile| {
            let index = ((y % PIXELS * PIXELS + x % PIXELS) as usize) * tile.channels;
            (f64::from(tile.data[index]) * 256.0
                + f64::from(tile.data[index + 1])
                + f64::from(tile.data[index + 2]) / 256.0
                - 32_768.0)
                .max(0.0)
        });
        self.memory.push_front(item);

        if self.last_disk_touch != Some(key) {
            self.last_disk_touch = Some(key);
            if let Some((modified, _)) = self.disk.get_mut(&key) {
                let now = SystemTime::now();
                File::options()
                    .write(true)
                    .open(self.dir.join(file_name(key)))?
                    .set_modified(now)?;
                *modified = now;
            }
        }
        Ok(elevation)
    }

    fn load(&mut self, key: Key, http: &mut Http<'_>) -> Result<Option<Tile>> {
        let path = self.dir.join(file_name(key));
        if self.disk.contains_key(&key) {
            match fs::read(&path).ok().and_then(|bytes| decode(&bytes).ok()) {
                Some(tile) => return Ok(Some(tile)),
                None => self.remove(key)?,
            }
        }
        let (x, y) = key;
        let url = Url::parse(&format!(
            "https://s3.amazonaws.com/elevation-tiles-prod/terrarium/{ZOOM}/{x}/{y}.png"
        ))?;
        let Some(object) = http.get(&url, 2_000_000)? else {
            return Ok(None);
        };
        let tile = decode(&object.bytes).with_context(|| format!("Decode {url}"))?;
        let size = object.bytes.len() as u64;
        if size <= self.capacity {
            self.prune(size)?;
            write_tile(&path, &object.bytes)?;
            self.bytes += size;
            self.disk.insert(key, (SystemTime::now(), size));
        }
        Ok(Some(tile))
    }
}

fn file_name((x, y): Key) -> String {
    format!("{ZOOM}-{x}-{y}.png")
}

fn write_tile(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("png.partial");
    fs::write(&temp, bytes)?;
    fs::rename(temp, path)?;
    Ok(())
}

fn decode(bytes: &[u8]) -> Result<Tile> {
    let mut reader = png::Decoder::new(Cursor::new(bytes)).read_info()?;
    let info = reader.info();
    ensure!(
        info.width == PIXELS && info.height == PIXELS && info.bit_depth == png::BitDepth::Eight,
        "Unexpected Terrarium tile dimensions or bit depth"
    );
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        _ => anyhow::bail!("Unexpected Terrarium tile colour type"),
    };
    let mut data = vec![0; (PIXELS * PIXELS) as usize * channels];
    reader.next_frame(&mut data)?;
    Ok(Tile { data, channels })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egm96_has_expected_sign_and_wraps_at_the_antimeridian() -> Result<()> {
        let grid = Geoid::load()?;
        assert!((grid.undulation(0.0, 0.0) - 17.16).abs() < 0.1);
        assert!(grid.undulation(25.0, 89.0) < -30.0);
        assert_eq!(grid.undulation(20.0, -180.0), grid.undulation(20.0, 180.0));
        Ok(())
    }
}
