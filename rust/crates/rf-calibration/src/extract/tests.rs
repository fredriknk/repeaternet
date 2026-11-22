use std::{fs, path::Path};

use rusqlite::{Connection, params};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{privacy::node_id, run};
use crate::dataset::{Evidence, LinkRecord, NodeRecord, Region};

fn lines<T: DeserializeOwned>(path: &Path) -> Vec<T> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn core_snapshot(path: &Path, host: &str) -> Connection {
    let db = Connection::open(path).unwrap();
    for schema in [
        include_str!("../../../corescope-capture/src/schema.sql"),
        include_str!("../../../corescope-capture/src/watch_schema.sql"),
        include_str!("../../../corescope-capture/src/instance_schema.sql"),
    ] {
        db.execute_batch(schema).unwrap();
    }
    db.execute("INSERT INTO capture_instance VALUES (1, ?)", [host])
        .unwrap();
    db.execute(
        "INSERT INTO capture_runs(id,endpoint,mode,started_at)
        VALUES (1, ?, 'snapshot', '1970-01-01T00:00:01Z')",
        [host],
    )
    .unwrap();
    db.execute_batch(
        r#"INSERT INTO nodes_snapshot VALUES
           (1,'1970-01-01T00:00:01Z','AA',60.00,10.00,'{"role":"repeater","name":"private-node-name"}'),
           (1,'1970-01-01T00:00:01Z','FF',0,0,'{}'),
           (1,'1970-01-01T00:00:01Z','AB',60.03,10.00,'{}');
           INSERT INTO observers_snapshot VALUES
           (1,'1970-01-01T00:00:01Z','BB',60.01,10.00,'{"nodeRole":"room","radio":"869.617981,62.5,8,8","noise_floor":-113}'),
           (1,'1970-01-01T00:00:01Z','CC',60.02,10.00,'{}'),
           (1,'1970-01-01T00:00:01Z','DD',62.00,10.00,'{}'),
           (1,'1970-01-01T00:00:01Z','EE',60.04,10.00,'{}');"#,
    ).unwrap();
    db
}

#[test]
fn corescope_positions_and_detection_use_packet_hash_activity_and_attribution() {
    let work = tempfile::tempdir().unwrap();
    let snapshots = work.path().join("snapshots");
    let dataset = work.path().join("dataset");
    fs::create_dir(&snapshots).unwrap();
    fs::write(work.path().join("salt.txt"), " test-salt \r\n").unwrap();
    let db_path = snapshots.join("corescope-capture-test.sqlite3");
    let host = "https://one.example.test:8080/base";
    let db = core_snapshot(&db_path, host);
    let insert = |hash: &str, observer: &str, first: i64, seen: i64, resolved: &str| {
        db.execute(
            "INSERT INTO observations(endpoint, packet_id, observation_key, packet_hash, observer_id,
                    first_seen, timestamp, rssi, snr, path_prefixes, resolved_path, direction)
             VALUES (?1, (SELECT count(*) + 1 FROM observations), 'rx', ?2, ?3,
                   strftime('%Y-%m-%dT%H:%M:%fZ', ?4/1000.0, 'unixepoch'),
                   strftime('%Y-%m-%dT%H:%M:%fZ', ?5/1000.0, 'unixepoch'),
                   -100, -5, '[\"aa\"]', ?6, 'rx')",
             params![host, hash, observer, first, seen, resolved],
        ).unwrap();
    };
    for i in 0..22 {
        let hash = format!("packet-{i}");
        let ms = 150_000 + i * 1000;
        insert(&hash, "BB", ms, ms, r#"["AA"]"#);
        insert(&hash, "bb", ms + 500, ms + 500, r#"["aa"]"#);
    }
    insert("packet-0", "BB", 150_000, 150_500, r#"["AB"]"#);
    // The activity edge is exactly +120 s from the first sighting. This
    // receiver's unresolved copy for packet-0 makes that opportunity ambiguous.
    insert("packet-0", "CC", 150_000, 270_000, "[null]");
    insert("far-active", "DD", 151_000, 151_000, "[null]");
    insert("near-inactive", "EE", 10_000, 10_000, "[null]");
    insert("incomplete", "BB", 152_000, 152_000, r#"[null,"AA"]"#);
    db.execute(
        "UPDATE observations SET path_prefixes = '[\"ab\",\"aa\"]'
                WHERE packet_hash = 'incomplete'",
        [],
    )
    .unwrap();
    // Zero-hop copy of the same hash does not double-count the opportunity.
    // This advert arrives after all but one of the measured receptions.
    insert("packet-21", "BB", 171_000, 172_000, "[]");
    db.execute(
        "UPDATE observations SET path_prefixes = '[]', sender_pubkey = 'aa',
                    advert_lat = 60.04, advert_lon = 10 WHERE timestamp LIKE '%02:52.000Z'",
        [],
    )
    .unwrap();
    insert("self", "AA", 150_000, 150_000, r#"["aa"]"#);
    drop(db);
    drop(core_snapshot(
        &snapshots.join("corescope-capture-other.sqlite3"),
        "https://two.example.test",
    ));

    let before = fs::read(db_path).unwrap();
    run(&snapshots, &dataset).unwrap();
    let links: Vec<LinkRecord> = lines(&dataset.join("links.jsonl"));
    let measured = links
        .iter()
        .find(|l| {
            l.evidence == Evidence::CorescopeLastHop
                && l.tx.node == node_id("test-salt", "corescope", "AA")
        })
        .unwrap();
    let silence = links
        .iter()
        .find(|l| l.evidence == Evidence::CorescopeNonDetection)
        .unwrap();
    let zero_hop = links
        .iter()
        .find(|l| l.evidence == Evidence::CorescopeZeroHop)
        .unwrap();
    assert_eq!(links.len(), 4);
    let second_relay = links
        .iter()
        .find(|l| l.tx.node == node_id("test-salt", "corescope", "AB"))
        .unwrap();
    assert_eq!(second_relay.n, 1);
    assert_eq!(second_relay.detection.as_ref().unwrap().heard, 1);
    assert_eq!(second_relay.detection.as_ref().unwrap().opportunities, 1);
    assert_eq!(measured.source, "corescope:one.example.test");
    assert_eq!(measured.region, Region::Norway);
    assert_eq!(measured.n, 45);
    assert_eq!(measured.first_ms, Some(150_000));
    assert_eq!(measured.last_ms, Some(171_500));
    assert_eq!(measured.signal_dbm.as_ref().unwrap().n, 45);
    assert_eq!(measured.tx.lat, 60.0);
    assert_eq!(measured.tx.pos_source, "node_snapshot");
    assert!(measured.tx.flags.contains(&"moved".to_owned()));
    assert_eq!(measured.rx.role.as_deref(), Some("room"));
    assert_eq!(measured.radio.bw_khz, Some(62.5));
    assert_eq!(measured.radio.tx_power_dbm, None);
    assert_eq!(measured.detection.as_ref().unwrap().opportunities, 22);
    assert_eq!(measured.detection.as_ref().unwrap().heard, 22);
    assert_eq!(zero_hop.tx.pos_source, "advert");
    assert!(zero_hop.detection.is_none());
    assert_eq!(silence.n, 0);
    assert_eq!(silence.rx.node, node_id("test-salt", "corescope", "CC"));
    assert_eq!(silence.detection.as_ref().unwrap().heard, 0);
    assert_eq!(silence.detection.as_ref().unwrap().opportunities, 21);
    assert_eq!(silence.last_ms, None);
    assert!(silence.signal_dbm.is_none());

    let nodes: Vec<NodeRecord> = lines(&dataset.join("nodes.jsonl"));
    let same_node: Vec<_> = nodes
        .iter()
        .filter(|n| n.node == measured.tx.node)
        .collect();
    assert_eq!(same_node.len(), 2);
    assert_eq!(same_node[0].lat, Some(60.04));
    assert_eq!(same_node[0].position_count, 2);
    assert_eq!(
        nodes
            .iter()
            .find(|n| n.node == measured.rx.node)
            .unwrap()
            .noise_floor_dbm,
        Some(-113.)
    );
    let default = nodes
        .iter()
        .find(|n| n.node == node_id("test-salt", "corescope", "FF"))
        .unwrap();
    assert_eq!(default.lat, None);
    assert!(default.flags.contains(&"default_position".to_owned()));

    let outputs = ["links.jsonl", "nodes.jsonl", "summary.json"].map(|name| dataset.join(name));
    let first_bytes = outputs.each_ref().map(|path| fs::read(path).unwrap());
    assert!(!String::from_utf8_lossy(&first_bytes[1]).contains("private-node-name"));
    run(&snapshots, &dataset).unwrap();
    assert_eq!(first_bytes, outputs.map(|path| fs::read(path).unwrap()));
    assert_eq!(
        before,
        fs::read(snapshots.join("corescope-capture-test.sqlite3")).unwrap()
    );
    assert_eq!(fs::read_dir(&dataset).unwrap().count(), 3);
}

#[test]
fn meshtastic_quality_and_radios_use_capture_evidence() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(include_str!("../../../meshtastic-capture/src/schema.sql"))
        .unwrap();
    db.execute_batch(
        "INSERT INTO meshtastic_instance VALUES (1, 'mesh.example.test');
         INSERT INTO positions VALUES
             (1, 0,      60,    10, 100, 14, null, 'test'),
             (1, 200000, 60.01, 10, 200, 16, null, 'test'),
             (2, 100000, 60.04, 10, null, 32, 0.5, 'test'),
             (3, 0,      60.06, 10, null, null, null, 'test'),
             (4, 0,      60.06, 10, null, null, null, 'test'),
             (5, 0,      60.06, 10, null, null, null, 'test'),
             (6, 0,      60.06001, 10, null, null, null, 'test'),
             (7, 0,      50,    10, null, null, null, 'test');
         INSERT INTO radios(node,observed_ms,region,modem_preset,sf,firmware,role,tx_power_dbm,old_neighbor_firmware) VALUES
             (1, 0, 'EU_868', 'NarrowFast', null, null, '2', 20, 0),
             (2, 100000, 'EU_868', 'LongFast', 10, '2.2.0', 'CLIENT', 27, 1),
             (2, 200000, 'EU_868', 'LongFast', 10, '2.4.0', 'CLIENT', 27, 0);
         INSERT INTO measurements (source,observation_id,upstream_id,seen_ms,tx,rx,evidence,
                                   snr_db,rssi_dbm,hop_start,hop_limit,relay_state,via_mqtt,region,provenance)
         VALUES
             ('mesh.example.test','a','a',120000,1,2,'gateway_rx',-6,-112,3,2,null,1,'EU_868','fixture'),
             ('mesh.example.test','b','b',125000,1,2,'neighbor',-8,null,null,null,null,null,null,'fixture'),
             ('mesh.example.test','c','c',125000,1,2,'traceroute_towards',4,null,null,null,null,null,null,'fixture'),
             ('mesh.example.test','d','d',130000,1,2,'traceroute_back',8,null,null,null,null,null,null,'fixture'),
             ('mesh.example.test','e','e',125000,3,4,'neighbor',0,null,null,null,null,null,null,'fixture'),
             ('mesh.example.test','f','f',125000,1,7,'neighbor',-7,null,null,null,null,null,null,'fixture'),
             ('mesh.example.test','g','g',125000,3,6,'gateway_rx',-7,-100,null,null,null,null,null,'fixture'),
             ('mesh.example.test','h','h',125000,null,4,'gateway_rx',-7,-100,1,0,'relayed',null,null,'fixture'),
             ('mesh.example.test','i','i',225000,2,1,'gateway_rx',-7,-100,3,3,null,null,null,'fixture');",
    ).unwrap();
    let source = super::meshtastic::read(&db, "salt").unwrap();
    assert_eq!(source.counts["measurements_missing_transmitter"], 1);
    let quality = super::quality::PositionQuality::from_sources(std::slice::from_ref(&source));
    let (links, nodes) =
        source.records(&repeaternet_providers::sources::Coverage::Global, &quality);
    let id = |raw| node_id("salt", "meshtastic", raw);
    let gateway = links
        .iter()
        .find(|l| l.evidence == Evidence::MeshtasticGatewayRx && l.tx.node == id("1"))
        .unwrap();
    assert_eq!(gateway.tx.lat, 60.01);
    assert_eq!(gateway.tx.altitude_m, Some(200.));
    assert_eq!(gateway.rx.pos_precision_m, Some(0.5));
    assert_eq!(gateway.tx.role.as_deref(), Some("router"));
    assert_eq!(gateway.rx.role.as_deref(), Some("client"));
    assert_eq!(gateway.radio.sf, Some(7));
    assert_eq!(gateway.radio.bw_khz, Some(62.5));
    assert_eq!(gateway.radio.tx_power_dbm, Some(20.));
    assert_eq!(gateway.radio.field_basis["bw_khz"], "preset_default");
    assert_eq!(gateway.radio.field_basis["lora_region"], "measurement");
    assert_eq!(
        gateway.flags,
        ["few_samples", "radio_mismatch", "relayed", "via_mqtt"]
    );
    assert_eq!(gateway.tx.flags, ["coarse_position", "moved"]);
    let neighbors: Vec<_> = links
        .iter()
        .filter(|l| l.evidence == Evidence::MeshtasticNeighbor)
        .collect();
    assert_eq!(neighbors.len(), 3);
    assert!(
        neighbors
            .iter()
            .all(|l| l.flags.contains(&"mqtt_suspect".into()))
    );
    assert_eq!(
        neighbors
            .iter()
            .filter(|l| l.flags.contains(&"over_300km".into()))
            .count(),
        1
    );
    let traceroute = links
        .iter()
        .find(|l| l.evidence == Evidence::MeshtasticTraceroute)
        .unwrap();
    assert_eq!(traceroute.n, 2);
    assert_eq!(traceroute.snr_db.as_ref().unwrap().median, 6.);
    assert!(traceroute.rssi_dbm.is_none());
    assert!(!traceroute.flags.contains(&"mqtt_suspect".into()));
    for (raw, expected) in [
        (
            "3",
            vec![
                "default_position",
                "duplicate_position",
                "unknown_precision",
            ],
        ),
        ("6", vec!["duplicate_position", "unknown_precision"]),
    ] {
        assert_eq!(
            nodes.iter().find(|n| n.node == id(raw)).unwrap().flags,
            expected
        );
    }
    let direct = links
        .iter()
        .find(|l| l.evidence == Evidence::MeshtasticGatewayRx && l.tx.node == id("2"))
        .unwrap();
    assert!(!direct.flags.contains(&"relayed".into()));
    let unconfirmed = links
        .iter()
        .find(|l| l.evidence == Evidence::MeshtasticGatewayRx && l.tx.node == id("3"))
        .unwrap();
    assert!(unconfirmed.flags.contains(&"relayed".into()));
}

#[test]
fn extraction_keeps_existing_dataset_if_a_snapshot_cannot_be_read() {
    let work = tempfile::tempdir().unwrap();
    let snapshots = work.path().join("snapshots");
    let dataset = work.path().join("dataset");
    fs::create_dir(&snapshots).unwrap();
    fs::create_dir(&dataset).unwrap();
    fs::write(dataset.join("summary.json"), "{}").unwrap();
    fs::write(
        snapshots.join("meshtastic-capture-broken.sqlite3"),
        "broken",
    )
    .unwrap();
    assert!(run(&snapshots, &dataset).is_err());
    assert_eq!(
        lines::<Value>(&dataset.join("summary.json")),
        [serde_json::json!({})]
    );
}
