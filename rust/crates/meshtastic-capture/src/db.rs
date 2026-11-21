use std::{
    fs::{File, OpenOptions},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use rusqlite::{
    Connection, OptionalExtension, TransactionBehavior, functions::FunctionFlags, params,
};

use crate::model::{Batch, old_firmware};

pub fn open(path: &Path, host: &str) -> Result<(Connection, File)> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("sqlite3.lock"))?;
    lock.try_lock()
        .with_context(|| format!("Another capture holds {}", path.display()))?;
    let mut db = Connection::open(path)?;
    db.busy_timeout(Duration::from_secs(2))?;
    db.pragma_update(None, "foreign_keys", true)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let objects: u32 = tx.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if objects == 0 {
        tx.execute_batch(include_str!("schema.sql"))?;
        tx.execute("INSERT INTO meshtastic_instance VALUES (1, ?1)", [host])?;
    } else {
        let stored: String = tx
            .query_row(
                "SELECT host FROM meshtastic_instance WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .context("Select a Meshtastic capture database")?;
        ensure!(
            stored == host,
            "Database belongs to {stored}, requested {host}"
        );
        let version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version == 1,
            "Unsupported Meshtastic capture schema version {version}"
        );
    }
    tx.commit()?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    Ok((db, lock))
}

pub fn store(db: &mut Connection, host: &str, batch: Batch) -> Result<()> {
    let tx = db.transaction()?;
    {
        let mut position =
            tx.prepare_cached("INSERT OR IGNORE INTO positions VALUES (?1,?2,?3,?4,?5,?6,?7,?8)")?;
        for p in batch.positions {
            position.execute(params![
                p.node, p.time, p.lat, p.lon, p.altitude, p.bits, p.radius, p.source
            ])?;
        }
        let mut radio = tx.prepare_cached(
            "INSERT INTO radios VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
             ON CONFLICT(node, observed_ms) DO UPDATE SET
               region=coalesce(excluded.region,region), modem_preset=coalesce(excluded.modem_preset,modem_preset),
               frequency_mhz=coalesce(excluded.frequency_mhz,frequency_mhz),
               frequency_slot=coalesce(excluded.frequency_slot,frequency_slot), sf=coalesce(excluded.sf,sf),
               bandwidth_khz=coalesce(excluded.bandwidth_khz,bandwidth_khz), coding_rate=coalesce(excluded.coding_rate,coding_rate),
               tx_power_dbm=coalesce(excluded.tx_power_dbm,tx_power_dbm), hardware=coalesce(excluded.hardware,hardware),
               role=coalesce(excluded.role,role), firmware=coalesce(excluded.firmware,firmware),
               old_neighbor_firmware=CASE WHEN excluded.firmware IS NULL THEN old_neighbor_firmware ELSE excluded.old_neighbor_firmware END"
        )?;
        for (id, time, r) in batch.radios {
            radio.execute(params![
                id,
                time,
                r.region,
                r.preset,
                r.frequency_mhz,
                r.slot,
                r.sf,
                r.bw_khz,
                r.cr,
                r.tx_power_dbm,
                r.hardware,
                r.firmware,
                r.role,
                old_firmware(r.firmware.as_deref())
            ])?;
        }
        let mut measurement = tx.prepare_cached(
            "INSERT OR IGNORE INTO measurements VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)"
        )?;
        for m in batch.measurements {
            measurement.execute(params![
                host,
                m.key(),
                m.packet.id,
                m.packet.logical_id,
                m.packet.time,
                m.tx,
                m.rx,
                m.packet.gateway,
                m.evidence.label(),
                m.hop.map(|hop| hop as i64),
                m.snr,
                m.rssi,
                m.packet.hop_start,
                m.packet.hop_limit,
                m.packet.relay_hint,
                m.packet.relay_state,
                m.packet.via_mqtt,
                m.packet.region,
                m.packet.provenance
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn state(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("SELECT value FROM poll_state WHERE name=?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

pub fn set_state(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute(
        "INSERT INTO poll_state VALUES (?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value",
        [key, value],
    )?;
    Ok(())
}

pub fn record_request(db: &Connection, now: i64, budget: u32) -> Result<Option<i64>> {
    let tx = db.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM requests WHERE sent_ms <= ?1",
        [now - 86_400_000],
    )?;
    let (count, first): (u32, Option<i64>) =
        tx.query_row("SELECT count(*), min(sent_ms) FROM requests", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
    if count >= budget {
        return Ok(first.map(|t| t + 86_400_000));
    }
    tx.execute("INSERT INTO requests VALUES (?1)", [now])?;
    tx.commit()?;
    Ok(None)
}

pub fn stats(db: &Connection) -> Result<(i64, i64)> {
    let measurements = db.query_row("SELECT count(*) FROM measurements", [], |row| row.get(0))?;
    let located = db.query_row(
        "SELECT count(*) FROM (SELECT DISTINCT m.tx, m.rx FROM measurements m
            WHERE EXISTS(SELECT 1 FROM positions WHERE node=m.tx)
              AND EXISTS(SELECT 1 FROM positions WHERE node=m.rx))",
        [],
        |row| row.get(0),
    )?;
    Ok((measurements, located))
}

pub fn register_geometry(db: &Connection) -> Result<()> {
    db.create_scalar_function(
        "capture_over_300_km",
        4,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let (Some(lat_a), Some(lon_a), Some(lat_b), Some(lon_b)) = (
                ctx.get::<Option<f64>>(0)?,
                ctx.get::<Option<f64>>(1)?,
                ctx.get::<Option<f64>>(2)?,
                ctx.get::<Option<f64>>(3)?,
            ) else {
                return Ok(false);
            };
            let (lat_a, lat_b) = (lat_a.to_radians(), lat_b.to_radians());
            let haversine = ((lat_b - lat_a) / 2.0).sin().powi(2)
                + lat_a.cos() * lat_b.cos() * ((lon_b - lon_a).to_radians() / 2.0).sin().powi(2);
            Ok(haversine > (300.0_f64 / (2.0 * 6371.0)).sin().powi(2))
        },
    )?;
    Ok(())
}
