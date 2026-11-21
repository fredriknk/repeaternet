use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::db::{Run, Snapshot};

#[tokio::test]
async fn json_map_aggregates_directed_last_hops_and_applies_filters() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.sqlite3");
    let mut db = db::open(&path, "fixture").unwrap();
    let run = Run::start(&db, "fixture", "watch-topology").unwrap();
    db::save_snapshot(
        &mut db,
        Snapshot::Nodes,
        run.id,
        [
            ("tx", &json!({"lat": 60, "lon": 10, "name": "Repeater"})),
            (
                "rx",
                &json!({"lat": 61, "lon": 10, "name": "Receiver node"}),
            ),
            (
                "outside",
                &json!({"lat": 52, "lon": 10, "name": "Outside bounds"}),
            ),
        ],
    )
    .unwrap();
    db::save_snapshot(
        &mut db,
        Snapshot::Observers,
        run.id,
        [
            ("rx", &json!({"lat": 0, "lon": 0, "name": "Base"})),
            (
                "unknown",
                &json!({"lat": 0, "lon": 0, "name": "Unknown position"}),
            ),
        ],
    )
    .unwrap();
    run.finish(&db, 0, false).unwrap();
    let now = Utc::now().timestamp();
    for (id, key, observer, prefixes, path, sender, snr, rssi, age) in [
        (
            1,
            "one",
            "rx",
            r#"["26","be"]"#,
            r#"["previous","tx"]"#,
            None,
            -2.0,
            -110.0,
            10,
        ),
        (
            1,
            "two",
            "rx",
            r#"["be"]"#,
            r#"["tx"]"#,
            None,
            8.0,
            -90.0,
            15,
        ),
        (2, "one", "rx", "[]", "[]", Some("tx"), 2.0, -95.0, 20),
        (
            3,
            "one",
            "rx",
            r#"["be"]"#,
            r#"["tx"]"#,
            None,
            20.0,
            0.0,
            2 * 86_400,
        ),
        (
            4,
            "one",
            "rx",
            r#"["be"]"#,
            r#"["outside"]"#,
            None,
            -8.0,
            -120.0,
            30,
        ),
        (
            5,
            "one",
            "unknown",
            r#"["be"]"#,
            r#"["tx"]"#,
            None,
            4.0,
            -100.0,
            30,
        ),
        (
            6,
            "one",
            "rx",
            r#"["26","be"]"#,
            r#"["tx",null]"#,
            None,
            5.0,
            -100.0,
            30,
        ),
        (
            7,
            "one",
            "rx",
            r#"["be"]"#,
            r#"["rx"]"#,
            None,
            5.0,
            -100.0,
            30,
        ),
    ] {
        db.execute(
            "INSERT INTO observations(endpoint, packet_id, observation_key, packet_hash,
             observer_id, timestamp, first_seen, direction, path_prefixes, resolved_path,
             sender_pubkey, snr, rssi)
             VALUES ('fixture', ?1, ?2, 'hash', ?3, datetime(?4, 'unixepoch'),
                 datetime(?4, 'unixepoch'), 'rx', ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                key,
                observer,
                now - age,
                prefixes,
                path,
                sender,
                snr,
                rssi
            ],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO watch_state VALUES ('fixture', ?1, ?2, NULL, NULL, 0)",
        params![now, now + 900],
    )
    .unwrap();
    db::log_request(&db, "fixture", None).unwrap();
    let app = router(
        path,
        "fixture",
        "analyzer.on8ar.eu",
        "54,71.5,4,31.7".parse().unwrap(),
    )
    .unwrap();
    let page = app
        .clone()
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = to_bytes(page.into_body(), usize::MAX).await.unwrap();
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("<h1>CoreScope capture · analyzer.on8ar.eu</h1>")
    );
    let result = fetch(&app, "/api/map?window=day&hops=both&min_count=2").await;
    let stats = &result["stats"];
    for (key, value) in [
        ("packets", 7),
        ("receptions", 8),
        ("pairs", 3),
        ("located_pairs", 2),
        ("bbox_pairs", 1),
        ("observers", 2),
        ("located_observers", 1),
        ("nodes", 3),
        ("requests_24h", 1),
        ("last_poll", now),
        ("next_poll", now + 900),
    ] {
        assert_eq!(stats[key], value, "{key}");
    }
    assert!(stats["db_bytes"].as_u64().unwrap() > 0);
    let displayed = result["map"]["pairs"].as_array().unwrap();
    assert_eq!(displayed.len(), 1);
    let pair = &displayed[0];
    let points = &result["map"]["points"];
    assert_eq!(points[pair[0].as_u64().unwrap() as usize][0], "Repeater");
    assert_eq!(
        points[pair[1].as_u64().unwrap() as usize],
        json!(["Base", 61.0, 10.0, true])
    );
    assert_eq!(pair[2], 3);
    assert_eq!(pair[3], 2.0);
    assert_eq!(pair[4], -95.0);
    assert_eq!(pair[5], 111.2);
    assert_eq!(pair[6], now - 10);

    let relayed = fetch(&app, "/api/map?window=week&hops=relayed&min_count=2").await;
    let row = &relayed["map"]["pairs"][0];
    assert_eq!(row[2], 3);
    assert_eq!(row[3], 8.0);
    assert_eq!(row[4], -100.0);
    let zero = fetch(&app, "/api/map?window=all&hops=zero").await;
    assert_eq!(zero["map"]["pairs"].as_array().unwrap().len(), 1);
    assert_eq!(zero["map"]["pairs"][0][2], 1);
    let all = fetch(&app, "/api/map?window=all&hops=both&min_count=2").await;
    assert_eq!(all["map"]["pairs"][0][2], 4);
    assert_eq!(all["map"]["pairs"][0][3], 5.0);

    // The writer can commit while the server has a WAL reader open.
    db.execute("DELETE FROM observations WHERE packet_id = 2", [])
        .unwrap();
    let updated = fetch(&app, "/api/map?window=all&hops=both&min_count=2").await;
    assert_eq!(updated["stats"]["receptions"], 7);
    assert_eq!(updated["map"]["pairs"][0][2], 3);
    assert_eq!(
        db.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
            .unwrap(),
        "wal"
    );
}

async fn fetch(app: &Router, uri: &str) -> Value {
    let response = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}
