use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    time::Duration,
};

use crate::{DATABASE, SUSPECT_FLAGS, database_bytes};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, named_params, params};

use crate::{http, records::Flight};

const LICENCE: &str = "CC BY-SA 2.0";

pub struct Store<'a> {
    db: Connection,
    dir: &'a Path,
    ceiling: u64,
}

pub struct Source<'a> {
    pub locator: &'a str,
    pub day: chrono::NaiveDate,
    pub status: &'static str,
    pub etag: Option<String>,
    pub sha256: Option<String>,
    pub bytes: usize,
    pub retrieved_at: i64,
}

impl<'a> Store<'a> {
    pub fn open(dir: &'a Path, ceiling: u64, low: u32, high: u32) -> Result<Option<Self>> {
        if database_bytes(dir)?.saturating_add(65_536) > ceiling {
            return Ok(None);
        }
        let mut db = Connection::open(dir.join(DATABASE))?;
        db.busy_timeout(Duration::from_secs(2))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.pragma_update(None, "foreign_keys", true)?;
        // Keep the write set in memory until the budget calculation.
        db.pragma_update(None, "cache_spill", false)?;
        db.pragma_update(None, "cache_size", -2048)?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(include_str!("schema.sql"))?;
        tx.execute(
            "INSERT OR IGNORE INTO import_settings VALUES(1, ?1, ?2)",
            params![low, high],
        )?;
        let stored = tx.query_row(
            "SELECT low_bin_seconds, high_bin_seconds FROM import_settings WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            stored == (low, high),
            "This database uses {} s and {} s bins. Use those intervals to resume, or choose another database directory.",
            stored.0,
            stored.1
        );
        if !fits(&tx, dir, ceiling)? {
            return Ok(None);
        }
        tx.commit()?;
        Ok(Some(Self { db, dir, ceiling }))
    }

    pub fn already_imported(&self, locator: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT 1 FROM source_object WHERE locator=?1 AND status IN ('complete', 'invalid')",
            [locator], |_| Ok(()),
        ).optional()?.is_some())
    }

    pub fn save(
        &mut self,
        source: Source<'_>,
        flight: &Flight,
        stop: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let last = flight
            .bins
            .iter()
            .map(|bin| bin.first.observed_ms.div_euclid(1000))
            .max();
        let pairs = flight
            .bins
            .iter()
            .map(|bin| &bin.first.receiver)
            .collect::<HashSet<_>>()
            .len();
        let source_id: i64 = tx.query_row(
            "INSERT INTO source_object (locator, archive_day, etag, sha256, retrieved_at, licence,
                 status, download_bytes, input_rows, kept_rows, reject_count, rejects, last_observed, located_links)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(locator) DO UPDATE SET archive_day=excluded.archive_day, etag=excluded.etag,
                 sha256=excluded.sha256, retrieved_at=excluded.retrieved_at, status=excluded.status,
                 download_bytes=excluded.download_bytes, input_rows=excluded.input_rows, kept_rows=excluded.kept_rows,
                 reject_count=excluded.reject_count, rejects=excluded.rejects, last_observed=excluded.last_observed,
                 located_links=excluded.located_links
             WHERE source_object.status='missing' RETURNING id",
            params![source.locator, source.day.to_string(), source.etag, source.sha256, source.retrieved_at,
                LICENCE, source.status, source.bytes as i64, flight.input as i64, flight.bins.len() as i64, flight.rejects.values().sum::<usize>() as i64,
                serde_json::to_string(&flight.rejects)?, last, pairs as i64],
            |r| r.get(0))?;
        if !flight.bins.is_empty() {
            let mut receiver_ids = HashMap::new();
            let mut receiver =
                tx.prepare("INSERT OR IGNORE INTO receiver(receiver_key) VALUES(?1)")?;
            let mut find_receiver = tx.prepare("SELECT id FROM receiver WHERE receiver_key=?1")?;
            let mut insert = tx.prepare(
                "INSERT INTO measured_link (source_id, receiver_id, serial, frame, observed_at_ms, received_at_ms,
                    tx_lat, tx_lon, tx_alt_m, tx_alt_datum, tx_agl_m, rx_lat, rx_lon, rx_alt_m, rx_alt_datum,
                    frequency_hz, frequency_basis, modulation, sonde_type, tx_power_mw, tx_power_basis, decoder,
                    decoder_version, rx_antenna, lna, bin_start, bin_seconds, bin_count,
                    rssi_dbm, rssi_kind, rssi_count, rssi_min, rssi_max, rssi_mean,
                    snr_db, snr_kind, snr_count, snr_min, snr_max, snr_mean, quality_flags)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, 'unknown',
                    ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27,
                    ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40)"
            )?;
            let mut first = None;
            let mut last = 0;
            for bin in &flight.bins {
                http::check(stop)?;
                let row = &bin.first;
                let receiver_id = match receiver_ids.entry(&row.receiver) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        receiver.execute([&row.receiver])?;
                        *entry.insert(
                            find_receiver.query_row([&row.receiver], |r| r.get::<_, i64>(0))?,
                        )
                    }
                };
                let id = insert.insert(params![
                    source_id,
                    receiver_id,
                    row.serial,
                    row.frame,
                    row.observed_ms,
                    row.received_ms,
                    row.tx[0],
                    row.tx[1],
                    row.tx_alt,
                    row.tx_datum,
                    bin.agl,
                    row.rx[0],
                    row.rx[1],
                    row.rx_alt,
                    row.hz,
                    row.frequency_basis,
                    row.modulation,
                    row.sonde_type,
                    row.power,
                    row.power.map(|_| "nominal_model_spec"),
                    row.decoder,
                    row.version,
                    row.antenna,
                    row.lna,
                    bin.start,
                    bin.seconds,
                    bin.count,
                    row.rssi,
                    (bin.rssi.count != 0).then_some(&row.decoder),
                    bin.rssi.count,
                    bin.rssi.min,
                    bin.rssi.max,
                    bin.rssi.mean(),
                    row.snr,
                    (bin.snr.count != 0).then_some(&row.decoder),
                    bin.snr.count,
                    bin.snr.min,
                    bin.snr.max,
                    bin.snr.mean(),
                    bin.flags
                ])?;
                first.get_or_insert(id);
                last = id;
            }
            let first = first.context("Non-empty flight must contain a first row")?;
            tx.execute(
                "INSERT INTO flight_extent
                 SELECT ?1, ?2, ?3, min(tx_lat), max(tx_lat), min(tx_lon), max(tx_lon),
                        min(observed_at_ms), max(observed_at_ms)
                   FROM measured_link WHERE id BETWEEN ?2 AND ?3",
                params![source_id, first, last],
            )?;
            tx.execute(
                include_str!("flight_rollup.sql"),
                named_params! {
                    ":source": source_id, ":first": first, ":last": last, ":suspect": SUSPECT_FLAGS,
                },
            )?;
        }
        http::check(stop)?;
        if !fits(&tx, self.dir, self.ceiling)? {
            tx.rollback()?;
            return Ok(false);
        }
        tx.commit()
            .with_context(|| format!("Commit {}", source.locator))?;
        Ok(true)
    }

    pub fn checkpoint(&self) -> Result<()> {
        let busy: i64 = self
            .db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))?;
        if busy != 0 {
            eprintln!(
                "A viewer is reading the database. WAL space may remain in use until readers finish."
            );
        }
        Ok(())
    }
}

/// With spilling disabled, every page that can go into the WAL is still in
/// this connection's cache. Count the whole cache (including read pages), plus
/// possible main-file growth from checkpointing and room for WAL headers/index.
fn fits(db: &Connection, dir: &Path, ceiling: u64) -> Result<bool> {
    let bytes = database_bytes(dir)?;
    let page_count = db.pragma_query_value(None, "page_count", |r| r.get::<_, i64>(0))? as u64;
    let page_size = db.pragma_query_value(None, "page_size", |r| r.get::<_, i64>(0))? as u64;
    let main_size = fs::metadata(dir.join(DATABASE))?.len();
    let mut cache = 0;
    let mut high_water = 0;
    // SAFETY: rusqlite owns this live connection. No other thread or statement is using its handle.
    let code = unsafe {
        rusqlite::ffi::sqlite3_db_status64(
            db.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED,
            &mut cache,
            &mut high_water,
            0,
        )
    };
    ensure!(
        code == rusqlite::ffi::SQLITE_OK && cache >= 0,
        "Could not measure the SQLite page cache"
    );
    let frames = (cache as u64).div_ceil(page_size);
    let shared_memory_growth = (frames * 16).div_ceil(32_768) * 32_768;
    let projected = bytes
        .saturating_add((page_count * page_size).saturating_sub(main_size))
        .saturating_add((frames + 2) * (page_size + 24) + 32 + shared_memory_growth + 32_768);
    Ok(projected <= ceiling)
}

impl Source<'_> {
    pub fn missing(locator: &str, day: chrono::NaiveDate) -> Source<'_> {
        Source {
            locator,
            day,
            status: "missing",
            etag: None,
            sha256: None,
            bytes: 0,
            retrieved_at: Utc::now().timestamp(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records;

    #[test]
    fn over_budget_and_interrupted_batches_leave_no_manifest_or_rows() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let ceiling = 1_000_000;
        let mut store = Store::open(dir.path(), ceiling, 30, 300)?.context("Test schema fits")?;
        store.checkpoint()?;
        let flight = records::bin("FIXTURE", records::fixture()?, 30, 300, |row| {
            Ok(Some(row.tx_alt))
        })?;
        let locator = "https://history.sondehub.org/serial/FIXTURE.json.gz";
        let source = || Source {
            status: "complete",
            ..Source::missing(locator, Utc::now().date_naive())
        };
        let live = dir.path().join("mesh.sqlite3-wal");
        let remaining = ceiling - database_bytes(dir.path())?;
        fs::File::create(&live)?.set_len(remaining - 4096)?;
        assert!(!store.save(source(), &flight, &|| false)?);
        assert!(!store.already_imported(locator)?);

        fs::remove_file(live)?;
        assert!(
            store
                .save(source(), &flight, &|| true)
                .is_err_and(|err| err.is::<http::Interrupted>())
        );
        assert!(!store.already_imported(locator)?);

        assert!(store.save(source(), &flight, &|| false)?);
        assert!(store.already_imported(locator)?);
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM measured_link", [], |r| r
                    .get::<_, u32>(0))?,
            3
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT sum(clean_count) FROM flight_signal", [], |r| r
                    .get::<_, u32>(0))?,
            3
        );
        assert!(database_bytes(dir.path())? < ceiling);
        Ok(())
    }
}
