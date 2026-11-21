use serde_json::{Value, json};

use crate::{
    db,
    model::{Evidence, old_firmware},
    sources::{self, Api},
};

const HOST: &str = "capture.example";

#[test]
fn norway_uses_observation_keys_and_normalized_snr() {
    let body: Value = serde_json::from_str(include_str!("../fixtures/868.json")).unwrap();
    let nodes = sources::nodes(Api::Norway, &body, 0).unwrap();
    assert_eq!(nodes.positions[0].bits, Some(14));
    assert_eq!(nodes.positions[0].source, "position");
    let batch = sources::norway(&body).unwrap();
    assert_eq!(batch.positions[0].altitude, Some(51.0));
    let forward = &batch.measurements[0];
    assert_eq!(forward.packet.id, "99fccb6179c656b3");
    assert_eq!(
        forward.packet.logical_id.as_deref(),
        Some("bb1578ef4a9d6fc0")
    );
    assert_eq!(
        (forward.tx, forward.rx, forward.snr),
        (Some(1144615257), 2798376056, Some(-1.0))
    );
    let reverse = &batch.measurements[2];
    assert!(reverse.evidence == Evidence::Back);
    assert_eq!(
        (reverse.tx, reverse.rx, reverse.snr),
        (Some(1299288895), 2798376056, Some(5.25))
    );
    let gateway = &batch.measurements[4];
    assert!(gateway.evidence == Evidence::Gateway);
    assert_eq!(
        (gateway.tx, gateway.rx, gateway.rssi),
        (None, 501821116, Some(-117.0))
    );
}

#[test]
fn liam_reports_who_the_node_heard_and_keeps_radio_settings() {
    let body = serde_json::from_str(include_str!("../fixtures/liam.json")).unwrap();
    let batch = sources::nodes(Api::Liam, &body, 0).unwrap();
    assert_eq!(batch.positions[0].lat, 59.1921152);
    assert_eq!(batch.radios[0].2.region.as_deref(), Some("EU_868"));
    assert_eq!(batch.radios[1].2.preset.as_deref(), Some("MediumFast"));
    let heard = &batch.measurements[0];
    assert!(heard.evidence == Evidence::Neighbor);
    assert_eq!(
        (heard.tx, heard.rx, heard.snr),
        (Some(1770367220), 1978245180, Some(-1.5))
    );
}

#[test]
fn meshview_reads_only_rf_ports_and_aligns_quarter_db_values() {
    let body = serde_json::from_str(include_str!("../fixtures/meshview.json")).unwrap();
    let nodes = sources::nodes(Api::Meshview, &body, 0).unwrap();
    assert_eq!(nodes.positions[0].lon, 8.5917696);
    let (batch, packet) = sources::meshview_packets(&body, 70, None).unwrap();
    assert_eq!(packet.len(), 1);
    assert_eq!(batch.measurements.len(), 2);
    let last = &batch.measurements[1];
    assert_eq!(
        (last.tx, last.rx, last.snr),
        (Some(3860612400), 3364630309, Some(5.75))
    );
    let (batch, _) = sources::meshview_packets(&body, 71, None).unwrap();
    let heard = &batch.measurements[0];
    assert_eq!(
        (heard.tx, heard.rx, heard.snr),
        (Some(2131803354), 3364630309, Some(6.25))
    );
    let mut body: Value = body;
    let batch = sources::meshview_seen(&body, &packet[0], None).unwrap();
    assert_eq!(batch.measurements[0].tx, None);
    body["seen"][0]["hop_limit"] = json!(3);
    let batch = sources::meshview_seen(&body, &packet[0], None).unwrap();
    assert_eq!(batch.measurements[0].tx, Some(360630829));
}

#[test]
fn meshinfo_uses_reply_direction_and_drops_unknown_legs_without_bridging() {
    let mut body: Value = serde_json::from_str(include_str!("../fixtures/meshinfo.json")).unwrap();
    assert_eq!(
        sources::nodes(Api::MeshInfo, &body, 0).unwrap().positions[0].lon,
        -122.45
    );
    let batch = sources::meshinfo(&body).unwrap();
    let forward = &batch.measurements[0];
    assert_eq!(
        (forward.tx, forward.rx, forward.snr),
        (Some(0x69856e88), 0x7a31efe8, Some(-7.25))
    );
    let back = &batch.measurements[3];
    assert_eq!(
        (back.tx, back.rx, back.snr),
        (Some(0xd1cc9b3b), 0xbb661928, Some(5.5))
    );
    assert_eq!(
        forward.packet.logical_id,
        batch.measurements[4].packet.logical_id
    );
    body["traceroutes"][0]["route_ids"][0] = json!("ffffffff");
    body["traceroutes"][0]["payload"]["snr_back"][0] = json!(-128);
    let batch = sources::meshinfo(&body).unwrap();
    assert_eq!(batch.measurements.len(), 3);
    assert_eq!(batch.measurements[0].hop, Some(2));
}

#[test]
fn dedup_and_quality_flags_keep_each_report() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("test.sqlite3");
    let (mut db, lock) = db::open(&file, HOST).unwrap();
    assert!(db::open(&file, HOST).is_err());
    db::register_geometry(&db).unwrap();
    let body = json!({"nodes": [
        {"node_id": 101, "latitude": 600000000, "longitude": 10000000, "position_precision": 23},
        {"node_id": 102, "latitude": 700000000, "longitude": 10000000, "position_precision": 12},
        {"node_id": 103, "latitude": 600100000, "longitude": 10000000, "position_precision": 23,
         "neighbours_updated_at": "2026-11-21T08:07:34.263Z",
         "neighbours": [{"node_id": 101, "snr": 0}, {"node_id": 102, "snr": 1}]},
        {"node_id": 104, "latitude": 600200000, "longitude": 10000000, "position_precision": 23, "firmware_version": "2.3.1.abc",
         "neighbours_updated_at": "2026-11-21T08:07:34.263Z",
         "neighbours": [{"node_id": 101, "snr": 1}]},
        {"node_id": 105, "latitude": 600300000, "longitude": 10000000, "position_precision": 23, "firmware_version": "2.3.2",
         "neighbours_updated_at": "2026-11-21T08:07:34.263Z",
         "neighbours": [{"node_id": 101, "snr": 1}]}
    ]});
    for _ in 0..2 {
        db::store(&mut db, HOST, sources::nodes(Api::Liam, &body, 1).unwrap()).unwrap();
    }
    assert_eq!(db::stats(&db).unwrap(), (4, 4));
    let mut query = db
        .prepare("SELECT coarse, mqtt_suspect FROM measurement_flags ORDER BY rx,tx")
        .unwrap();
    let flags: Vec<(bool, bool)> = query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        flags,
        [(false, true), (true, true), (false, true), (false, false)]
    );
    assert!(old_firmware(Some("v2.2.99-dev")));
    assert!(!old_firmware(Some("unknown")));
    drop(query);
    drop(db);
    drop(lock);
    assert!(db::open(&file, "wrong.example").is_err());
}

#[test]
fn daily_budget_counts_requests_in_a_rolling_window() {
    let dir = tempfile::tempdir().unwrap();
    let (db, _lock) = db::open(&dir.path().join("test.sqlite3"), HOST).unwrap();
    assert_eq!(db::record_request(&db, 1000, 2).unwrap(), None);
    assert_eq!(db::record_request(&db, 4000, 2).unwrap(), None);
    assert_eq!(db::record_request(&db, 8000, 2).unwrap(), Some(86_401_000));
    assert_eq!(db::record_request(&db, 86_401_000, 2).unwrap(), None);
}
