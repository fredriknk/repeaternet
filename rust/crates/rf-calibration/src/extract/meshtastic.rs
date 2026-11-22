use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, functions::FunctionFlags};

use super::{
    model::{Metadata, Position, Sample, Source, TimedPosition, radio_basis},
    privacy::node_id,
    quality::{distance, point},
};
use crate::dataset::{Evidence, Radio};

pub(super) fn read(db: &Connection, salt: &str) -> Result<Source> {
    let host: String =
        db.query_row("SELECT host FROM meshtastic_instance", [], |row| row.get(0))?;
    ensure!(
        !host.is_empty()
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')),
        "invalid Meshtastic capture host"
    );
    let mut source = Source::new(format!("meshtastic:{}", host.to_ascii_lowercase()), true);
    let mut identities = std::collections::HashMap::new();
    let mut id = |raw: u32| {
        identities
            .entry(raw)
            .or_insert_with(|| node_id(salt, "meshtastic", &raw.to_string()))
            .clone()
    };

    read_positions(db, &mut source, &mut id)?;
    read_radios(db, &mut source, &mut id)?;
    source.prepare_nodes();

    // Reuse the capture view, including its firmware-time fallback and
    // latest-position check for MQTT-suspect neighbor reports.
    db.create_scalar_function(
        "capture_over_300_km",
        4,
        FunctionFlags::SQLITE_DETERMINISTIC | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let coordinate = |index| -> rusqlite::Result<_> {
                let lat: Option<f64> = ctx.get(index)?;
                let lon: Option<f64> = ctx.get(index + 1)?;
                Ok(lat
                    .zip(lon)
                    .map(|(latitude, longitude)| repeaternet_terrain::LonLat {
                        latitude,
                        longitude,
                    }))
            };
            Ok(coordinate(0)?
                .zip(coordinate(2)?)
                .is_some_and(|(a, b)| distance(a, b) > 300_000.0))
        },
    )?;
    let mut query = db.prepare(
        "SELECT seen_ms, tx, rx, evidence, rssi_dbm, snr_db, hop_start, hop_limit,
                relay_state, via_mqtt, region, mqtt_suspect,
                evidence = 'neighbor' AND capture_over_300_km(tx_lat, tx_lon, rx_lat, rx_lon)
         FROM measurement_flags ORDER BY seen_ms, source, observation_id",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        source.count("measurement_rows");
        let evidence = match row.get::<_, String>(3)?.as_str() {
            "gateway_rx" => Evidence::MeshtasticGatewayRx,
            "traceroute_towards" | "traceroute_back" => Evidence::MeshtasticTraceroute,
            "neighbor" => Evidence::MeshtasticNeighbor,
            _ => bail!("unsupported Meshtastic measurement evidence"),
        };
        let rx = id(row.get(2)?);
        source.nodes.entry(rx.clone()).or_default();
        let Some(tx) = row.get::<_, Option<u32>>(1)?.map(&mut id) else {
            source.count("measurements_missing_transmitter");
            continue;
        };
        source.nodes.entry(tx.clone()).or_default();
        if tx == rx {
            source.count("receptions_self_link");
            continue;
        }
        let sample = Sample {
            ms: row.get(0)?,
            rssi: row.get::<_, Option<f64>>(4)?.filter(|v| v.is_finite()),
            snr: row.get::<_, Option<f64>>(5)?.filter(|v| v.is_finite()),
            region: row.get::<_, Option<String>>(10)?.and_then(region),
        };
        if sample.rssi.is_none() && sample.snr.is_none() {
            source.count("measurements_invalid_signal");
            continue;
        }
        let via_mqtt = row.get::<_, Option<bool>>(9)? == Some(true);
        let suspect: bool = row.get(11)?;
        let distant_neighbor: bool = row.get(12)?;
        let relayed = if evidence == Evidence::MeshtasticGatewayRx {
            let hops = row
                .get::<_, Option<u8>>(6)?
                .zip(row.get::<_, Option<u8>>(7)?);
            let state: Option<String> = row.get(8)?;
            let direct = match state.as_deref() {
                Some("direct" | "own_packet") => true,
                None => hops.is_some_and(|(start, limit)| start != 0 && start == limit),
                _ => false,
            };
            !direct || hops.is_some_and(|(start, limit)| start != limit)
        } else {
            false
        };
        let flags: Vec<_> = [
            ("mqtt_suspect", suspect),
            ("over_300km", distant_neighbor),
            ("via_mqtt", via_mqtt),
            ("relayed", relayed),
        ]
        .into_iter()
        .filter_map(|(flag, set)| set.then_some(flag))
        .collect();
        source.add_sample(&tx, &rx, evidence, sample, &flags);
    }
    Ok(source)
}

fn read_positions(
    db: &Connection,
    source: &mut Source,
    id: &mut impl FnMut(u32) -> String,
) -> Result<()> {
    let mut query = db.prepare(
        "SELECT node, observed_ms, lat, lon, altitude_m, precision_bits, precision_radius_m
         FROM positions ORDER BY node, observed_ms, location_source",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let node = source.nodes.entry(id(row.get(0)?)).or_default();
        let lat: f64 = row.get(2)?;
        let lon: f64 = row.get(3)?;
        node.saw_default |= lat == 0.0 && lon == 0.0;
        let Some(point) = point(lat, lon) else {
            continue;
        };
        let bits = row
            .get::<_, Option<u8>>(5)?
            .filter(|bits| (1..=32).contains(bits));
        let precision = row
            .get::<_, Option<f64>>(6)?
            .filter(|r| r.is_finite() && *r >= 0.0)
            .or_else(|| {
                bits.map(|bits| {
                    // Same half diagonal of the quantized cell as meshtastic-capture.
                    let half_width = 2_f64.powi(31 - i32::from(bits)) * 0.011132;
                    half_width * 1_f64.hypot(lat.to_radians().cos())
                })
            });
        let position = Position {
            point,
            source: "meshtastic_position",
            precision,
            bits,
            altitude: row.get::<_, Option<f64>>(4)?.filter(|m| m.is_finite()),
        };
        node.positions.push(TimedPosition {
            ms: row.get(1)?,
            position,
        });
    }
    Ok(())
}

fn read_radios(
    db: &Connection,
    source: &mut Source,
    id: &mut impl FnMut(u32) -> String,
) -> Result<()> {
    let mut query = db.prepare(
        "SELECT node, observed_ms, region, modem_preset, frequency_mhz, sf,
                bandwidth_khz, coding_rate, tx_power_dbm, role
         FROM radios ORDER BY node, observed_ms",
    )?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        let node = source.nodes.entry(id(row.get(0)?)).or_default();
        let modem_preset: Option<String> = row.get(3)?;
        let preset = modem_preset.as_deref().and_then(preset);
        let coding_rate: Option<String> = row.get(7)?;
        let mut radio = Radio {
            lora_region: row.get::<_, Option<String>>(2)?.and_then(region),
            modem_preset: preset.map(|(name, _, _)| name.to_owned()),
            freq_mhz: row
                .get::<_, Option<f64>>(4)?
                .filter(|f| f.is_finite() && *f > 0.0),
            sf: row
                .get::<_, Option<u8>>(5)?
                .filter(|sf| (5..=12).contains(sf)),
            bw_khz: row
                .get::<_, Option<f64>>(6)?
                .filter(|v| v.is_finite() && *v > 0.0),
            cr: coding_rate
                .as_deref()
                .and_then(|cr| cr.strip_prefix("4/").unwrap_or(cr).parse().ok())
                .filter(|cr| (5..=8).contains(cr)),
            tx_power_dbm: row.get::<_, Option<f64>>(8)?.filter(|p| p.is_finite()),
            ..Radio::default()
        };
        for (field, present) in [
            ("lora_region", radio.lora_region.is_some()),
            ("modem_preset", radio.modem_preset.is_some()),
            ("freq_mhz", radio.freq_mhz.is_some()),
            ("sf", radio.sf.is_some()),
            ("bw_khz", radio.bw_khz.is_some()),
            ("cr", radio.cr.is_some()),
            ("tx_power_dbm", radio.tx_power_dbm.is_some()),
        ] {
            if present {
                radio_basis(&mut radio, field, "node_radio");
            }
        }
        if let Some((_, sf, bw)) = preset {
            if radio.sf.is_none() {
                radio.sf = Some(sf);
                radio_basis(&mut radio, "sf", "preset_default");
            }
            if radio.bw_khz.is_none() {
                let wide = radio.lora_region.as_deref() == Some("LORA_24");
                radio.bw_khz = Some(if wide {
                    match bw {
                        125.0 => 406.25,
                        250.0 => 812.5,
                        500.0 => 1625.0,
                        _ => bw,
                    }
                } else {
                    bw
                });
                radio_basis(&mut radio, "bw_khz", "preset_default");
            }
        }
        let role = row.get::<_, Option<String>>(9)?.as_deref().and_then(role);
        node.metadata.push((
            row.get(1)?,
            Metadata {
                role,
                radio,
                noise_floor: None,
            },
        ));
    }
    Ok(())
}

fn role(value: &str) -> Option<String> {
    // Meshtastic DeviceConfig.Role is stored as either its enum name or number.
    const ROLES: &[&str] = &[
        "client",
        "client_mute",
        "router",
        "router_client",
        "repeater",
        "tracker",
        "sensor",
        "tak",
        "client_hidden",
        "lost_and_found",
        "tak_tracker",
        "router_late",
        "client_base",
    ];
    let name = if let Ok(index) = value.parse::<usize>() {
        ROLES.get(index).copied()
    } else {
        ROLES
            .iter()
            .copied()
            .find(|name| name.eq_ignore_ascii_case(value))
    };
    name.map(str::to_owned)
}

fn region(value: String) -> Option<String> {
    matches!(
        value.as_str(),
        "US" | "EU_433"
            | "EU_868"
            | "CN"
            | "JP"
            | "ANZ"
            | "KR"
            | "TW"
            | "RU"
            | "IN"
            | "NZ_865"
            | "TH"
            | "LORA_24"
            | "UA_433"
            | "UA_868"
            | "MY_433"
            | "MY_919"
            | "SG_923"
            | "PH_433"
            | "PH_868"
            | "PH_915"
            | "ANZ_433"
            | "KZ_433"
            | "KZ_863"
            | "NP_865"
            | "BR_902"
            | "ITU1_2M"
            | "ITU2_2M"
            | "EU_866"
            | "EU_874"
            | "EU_917"
            | "EU_N_868"
            | "ITU3_2M"
            | "ITU1_70CM"
    )
    .then_some(value)
}

fn preset(value: &str) -> Option<(&'static str, u8, f64)> {
    // Canonical capture names and nominal sub-GHz parameters from Meshtastic
    // modemPresetToParams, including the historical VeryLongSlow preset.
    Some(match value {
        "LongFast" => ("LongFast", 11, 250.0),
        "LongSlow" => ("LongSlow", 12, 125.0),
        "VeryLongSlow" => ("VeryLongSlow", 12, 62.5),
        "MediumSlow" => ("MediumSlow", 10, 250.0),
        "MediumFast" => ("MediumFast", 9, 250.0),
        "ShortSlow" => ("ShortSlow", 8, 250.0),
        "ShortFast" => ("ShortFast", 7, 250.0),
        "LongModerate" => ("LongModerate", 11, 125.0),
        "ShortTurbo" => ("ShortTurbo", 7, 500.0),
        "LongTurbo" => ("LongTurbo", 11, 500.0),
        "LiteFast" => ("LiteFast", 9, 125.0),
        "LiteSlow" => ("LiteSlow", 10, 125.0),
        "NarrowFast" => ("NarrowFast", 7, 62.5),
        "NarrowSlow" => ("NarrowSlow", 8, 62.5),
        "TinyFast" => ("TinyFast", 7, 15.6),
        "TinySlow" => ("TinySlow", 8, 15.6),
        "MediumTurbo" => ("MediumTurbo", 9, 500.0),
        _ => return None,
    })
}
