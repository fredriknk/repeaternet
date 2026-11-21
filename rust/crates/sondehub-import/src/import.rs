use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use crate::{Budget, DATABASE, GB, database_bytes, default_dir, parse_gb};
use anyhow::{Context, Result, ensure};
use chrono::{NaiveDate, Utc};
use clap::Parser;
use flate2::read::MultiGzDecoder;
use reqwest::Url;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{
    database::{Source, Store},
    http::{Http, Interrupted},
    records::Flight,
    terrain::{Geoid, Terrain},
};

const HOST: &str = "https://history.sondehub.org/";
const MAX_OBJECT: usize = 96_000_000;
const MAX_DECODED: u64 = 768_000_000;

#[derive(Parser)]
#[command(version, about = "Import worldwide SondeHub measured links")]
struct Args {
    /// Shared capture database directory.
    #[arg(long)]
    db_dir: Option<PathBuf>,
    /// Oldest UTC archive day, inclusive.
    #[arg(long, default_value = "2021-01-01")]
    since: NaiveDate,
    /// Newest UTC archive day, inclusive. Defaults to two UTC days ago.
    #[arg(long)]
    until: Option<NaiveDate>,
    #[command(flatten)]
    budget: Budget,
    /// Minimum interval between HTTP request starts.
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(1..=3_600_000))]
    min_interval_ms: u64,
    /// Time bin below 3,000 m AGL.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=86_400))]
    low_bin_seconds: u32,
    /// Time bin at or above 3,000 m AGL, or when AGL is unavailable.
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..=86_400))]
    high_bin_seconds: u32,
    /// Directory for the Terrarium tile cache. Defaults to RepeaterNet/cache/terrarium.
    #[arg(long)]
    tile_cache_dir: Option<PathBuf>,
    /// Maximum Terrarium disk cache size (decimal GB).
    #[arg(long, default_value_t = 2.0, value_parser = parse_gb)]
    tile_cache_gb: f64,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    let newest = args
        .until
        .unwrap_or_else(|| Utc::now().date_naive() - chrono::Days::new(2));
    ensure!(
        args.since <= newest && newest <= Utc::now().date_naive(),
        "Choose an inclusive UTC date range ending today or earlier"
    );
    let ceiling = args.budget.ceiling()?;
    let dir = args.db_dir.clone().map_or_else(default_dir, Ok)?;
    if database_bytes(&dir)? >= ceiling {
        budget_stop(&dir, ceiling)?;
        return Ok(());
    }
    fs::create_dir_all(&dir)?;
    let _import_lock = lock(&dir, "sondehub-import.sqlite3.lock")?;
    let Some(mut store) = Store::open(&dir, ceiling, args.low_bin_seconds, args.high_bin_seconds)?
    else {
        budget_stop(&dir, ceiling)?;
        return Ok(());
    };
    let cache = args.tile_cache_dir.clone().map_or_else(
        || Ok::<_, anyhow::Error>(default_dir()?.join("cache").join("terrarium")),
        Ok,
    )?;
    fs::create_dir_all(&cache)?;
    let _cache_lock = lock(&cache, "sondehub-tiles.lock")?;
    let mut terrain = Terrain::open(cache, (args.tile_cache_gb * GB) as u64)?;
    let geoid = Geoid::load()?;
    static STOP: AtomicBool = AtomicBool::new(false);
    ctrlc::set_handler(|| STOP.store(true, Ordering::Relaxed))?;
    let stop = || STOP.load(Ordering::Relaxed);
    let mut run = Import {
        store: &mut store,
        http: Http::new(args.min_interval_ms, &stop)?,
        terrain: &mut terrain,
        geoid: &geoid,
        stop: &stop,
        stats: Stats::default(),
    };
    println!(
        "SondeHub · {newest} through {} UTC · {}",
        args.since,
        dir.display()
    );
    let result = run.days(&args, newest, &dir, ceiling);
    run.report();
    store.checkpoint()?;
    drop(store);
    println!(
        "SondeHub database bytes: {} · all capture database bytes: {}",
        fs::metadata(dir.join(DATABASE))?.len(),
        database_bytes(&dir)?
    );
    match result {
        Err(err) if err.is::<Interrupted>() => {
            println!("Interrupted · run the same command to resume");
            Ok(())
        }
        result => result,
    }
}

fn lock(dir: &Path, file: &str) -> Result<fs::File> {
    let path = dir.join(file);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    file.try_lock()
        .with_context(|| format!("Another importer holds {}", path.display()))?;
    Ok(file)
}

fn budget_stop(dir: &Path, ceiling: u64) -> Result<()> {
    println!(
        "Database budget reached · next batch would exceed {ceiling} bytes with reserve · {} bytes currently in {}",
        database_bytes(dir)?,
        dir.display()
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Listing {
    #[serde(default, rename = "Contents")]
    objects: Vec<Listed>,
    next_continuation_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Listed {
    key: String,
}

fn serials(day: NaiveDate, http: &mut Http<'_>) -> Result<Vec<String>> {
    let prefix = format!("date/{}/", day.format("%Y/%m/%d"));
    let mut serials = Vec::new();
    let mut token = None;
    loop {
        let mut url = Url::parse(HOST)?;
        {
            let mut params = url.query_pairs_mut();
            params
                .append_pair("list-type", "2")
                .append_pair("prefix", &prefix);
            if let Some(previous) = token.as_deref() {
                params.append_pair("continuation-token", previous);
            }
        }
        let response = http
            .get(&url, 8_000_000)?
            .context("Archive listing returned HTTP 404")?;
        let data = inflate(&response.bytes, 8_000_000)?;
        let page: Listing =
            quick_xml::de::from_reader(data.as_slice()).context("Decode archive list")?;
        for item in page.objects {
            if let Some(serial) = item
                .key
                .strip_prefix(&prefix)
                .and_then(|item| item.strip_suffix(".json"))
                && !serial.is_empty()
                && !serial.contains('/')
            {
                serials.push(serial.to_owned());
            }
        }
        ensure!(
            page.next_continuation_token.is_none() || page.next_continuation_token != token,
            "Archive returned the same continuation token twice"
        );
        token = page.next_continuation_token;
        if token.is_none() {
            break;
        }
    }
    serials.sort_unstable();
    serials.dedup();
    Ok(serials)
}

fn inflate(bytes: &[u8], limit: u64) -> Result<Vec<u8>> {
    if !bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(bytes.to_vec());
    }
    let mut data = Vec::new();
    MultiGzDecoder::new(bytes)
        .take(limit + 1)
        .read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= limit,
        "Inflated body exceeds the limit"
    );
    Ok(data)
}

#[derive(Default)]
struct Stats {
    completed: usize,
    skipped: usize,
    missing: usize,
    invalid: usize,
    input: usize,
    kept: usize,
    rejects: BTreeMap<&'static str, usize>,
}

struct Import<'a, 'dir> {
    store: &'a mut Store<'dir>,
    http: Http<'a>,
    terrain: &'a mut Terrain,
    geoid: &'a Geoid,
    stop: &'a dyn Fn() -> bool,
    stats: Stats,
}

impl Import<'_, '_> {
    fn days(&mut self, args: &Args, newest: NaiveDate, dir: &Path, ceiling: u64) -> Result<()> {
        let mut day = newest;
        let mut seen = HashSet::new();
        loop {
            http::check(self.stop)?;
            if database_bytes(dir)? >= ceiling {
                return budget_stop(dir, ceiling);
            }
            let serials = serials(day, &mut self.http)?;
            println!("{day} · {} indexed serials", serials.len());
            for serial in serials {
                http::check(self.stop)?;
                if database_bytes(dir)? >= ceiling {
                    return budget_stop(dir, ceiling);
                }
                if seen.insert(serial.clone()) && !self.flight(args, day, &serial)? {
                    return budget_stop(dir, ceiling);
                }
            }
            self.report();
            if day == args.since {
                return Ok(());
            }
            day = day.pred_opt().context("Archive date underflow")?;
        }
    }

    fn flight(&mut self, args: &Args, day: NaiveDate, serial: &str) -> Result<bool> {
        let mut url = Url::parse(HOST)?;
        url.path_segments_mut()
            .expect("archive URL has paths")
            .extend(["serial", &format!("{serial}.json.gz")]);
        let locator = url.as_str();
        if self.store.already_imported(locator)? {
            self.stats.skipped += 1;
            return Ok(true);
        }
        let (source, flight) = match self.http.get(&url, MAX_OBJECT)? {
            Some(object) => {
                let mut source = Source {
                    locator,
                    day,
                    status: "complete",
                    etag: object.etag,
                    sha256: Some(
                        Sha256::digest(&object.bytes)
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect(),
                    ),
                    bytes: object.bytes.len(),
                    retrieved_at: Utc::now().timestamp(),
                };
                let rows = inflate(&object.bytes, MAX_DECODED).and_then(|data| {
                    serde_json::from_slice(&data).context("Decode serial telemetry")
                });
                let flight = match rows {
                    Ok(rows) => records::bin(
                        serial,
                        rows,
                        args.low_bin_seconds,
                        args.high_bin_seconds,
                        |row| {
                            http::check(self.stop)?;
                            let [lat, lon] = row.tx;
                            let alt_msl = match row.tx_datum {
                                "WGS84_ellipsoid" => row.tx_alt - self.geoid.undulation(lat, lon),
                                "MSL" => row.tx_alt,
                                _ => return Ok(None),
                            };
                            Ok(self
                                .terrain
                                .at(lat, lon, &mut self.http)?
                                .map(|ground| alt_msl - ground))
                        },
                    )?,
                    Err(err) => {
                        eprintln!("{locator} · invalid archive object · {err:#}");
                        source.status = "invalid";
                        Flight {
                            rejects: BTreeMap::from([("invalid_object", 1)]),
                            ..Flight::default()
                        }
                    }
                };
                (source, flight)
            }
            None => (Source::missing(locator, day), Flight::default()),
        };
        let status = source.status;
        if !self.store.save(source, &flight, self.stop)? {
            return Ok(false);
        }
        match status {
            "complete" => self.stats.completed += 1,
            "missing" => self.stats.missing += 1,
            _ => self.stats.invalid += 1,
        }
        self.stats.input += flight.input;
        self.stats.kept += flight.bins.len();
        for (reason, count) in flight.rejects {
            *self.stats.rejects.entry(reason).or_default() += count;
        }
        if (self.stats.completed + self.stats.missing + self.stats.invalid).is_multiple_of(50) {
            self.report();
        }
        Ok(true)
    }

    fn report(&self) {
        let s = &self.stats;
        println!(
            "Objects: {} complete, {} missing, {} invalid, {} skipped · {} bytes downloaded in {} requests ({} terrain) · {} input rows · {} kept · rejects {:?}",
            s.completed,
            s.missing,
            s.invalid,
            s.skipped,
            self.http.bytes,
            self.http.requests,
            self.http.terrain_requests,
            s.input,
            s.kept,
            s.rejects
        );
    }
}
use crate::{http, records};
