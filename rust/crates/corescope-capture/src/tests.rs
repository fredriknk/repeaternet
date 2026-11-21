use std::{
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};

use repeaternet_providers::corescope::capture::Endpoint;
use serde_json::{Value, json};

use super::*;
use crate::model::packet_page;

const SAMPLE: &str = include_str!("../fixtures/packets.json");
const ON8AR: &str = "https://analyzer.on8ar.eu";
const SENDER: &str = "70d8fb2dae304b220850aa24672f066785105752e80a87cd98314a9fe8bc83bf";

fn sample() -> Value {
    serde_json::from_str(SAMPLE).unwrap()
}

#[test]
fn packets_allowlist_advert_fields_and_deduplicate_receptions() {
    let mut payload = sample();
    let rows = payload["packets"].as_array_mut().unwrap();
    let advert = rows[0].clone();
    let mut duplicate_reception = advert["observations"][0].clone();
    duplicate_reception["id"] = json!(999);
    duplicate_reception["rssi"] = json!(-110);
    rows[0]["observations"]
        .as_array_mut()
        .unwrap()
        .push(duplicate_reception);
    rows[0]["raw_hex"] = json!("TEST_RAW_BYTES");
    rows[0]["observations"][0]["decoded_json"] = json!({"text":"TEST_NESTED_MESSAGE"});
    let mut decoded: Value =
        serde_json::from_str(advert["decoded_json"].as_str().unwrap()).unwrap();
    decoded["text"] = json!("TEST_ADVERT_MESSAGE");
    decoded["flags"]["text"] = json!("TEST_FLAGS_MESSAGE");
    rows[0]["decoded_json"] = json!(decoded.to_string());
    rows[1]["payload_type"] = json!(5);
    rows[1]["decoded_json"] = decoded;
    rows[1]["decoded_json"]["text"] = json!("TEST_CHANNEL_MESSAGE");
    rows[1].as_object_mut().unwrap().remove("observations");

    let page = packet_page(payload, 0).unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db_path = db_dir.path().join("capture.sqlite3");
    let mut db = db::open(&db_path, "sample").unwrap();
    assert_eq!(
        db::save_page(
            &mut db,
            "sample",
            db::Cursor::default(),
            db::Cursor::default(),
            &page.packets
        )
        .unwrap(),
        4
    );
    assert_eq!(
        db::save_page(
            &mut db,
            "sample",
            db::Cursor::default(),
            db::Cursor::default(),
            &page.packets
        )
        .unwrap(),
        0
    );

    let advert = db
        .query_row(
            "SELECT sender_pubkey, advert_lat, advert_lon, advert_flags, advert_name,
                rssi, snr, path_prefixes, observer_id
         FROM observations WHERE observation_id = 15686416",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, u8>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, f64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(advert.0, SENDER);
    assert_eq!((advert.1, advert.2, advert.3), (59.382817, 10.65313, 146));
    assert_eq!(advert.4, "NO-1570 Moss / Fuglevik");
    assert_eq!((advert.5, advert.6, advert.7.as_str()), (-38.0, 11.5, "[]"));
    assert_eq!(
        advert.8,
        "0933f690ce37579031fdc56da8de12d5a660600daa3087d513f61b109b0bfc1c"
    );
    let bare_packet = db
        .query_row(
            "SELECT sender_pubkey, observation_id FROM observations WHERE payload_type = 5",
            [],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(bare_packet, (None, None));
    drop(db);
    let bytes = fs::read(db_path).unwrap();
    let data = String::from_utf8_lossy(&bytes);
    assert!(!data.contains("TEST_"));
    assert!(!data.contains("raw_hex"));
    assert!(!data.contains("decoded_json"));
}

#[test]
fn resumes_capped_scan_and_honors_backoff() {
    let raw = sample();
    let rows = raw["packets"].as_array().unwrap();
    let page1 = json!({"packets":[rows[0], rows[1]], "total":3, "limit":2, "offset":0});
    let page2 = json!({"packets":[rows[1], rows[2]], "total":3, "limit":2, "offset":1});
    let empty = json!({"packets":[], "total":0, "limit":2, "offset":0});
    let (base, server) = mock(vec![
        (200, "", page1),
        (429, "Retry-After: 2\r\n", Value::Null),
        (503, "", Value::Null),
        (200, "", page2),
        (200, "", empty),
    ]);
    let db_dir = tempfile::tempdir().unwrap();
    let path = db_dir.path().join("resume.sqlite3");
    let mut db = db::open(&path, &base).unwrap();
    let mut client = CaptureClient::new(&base, Duration::from_millis(10)).unwrap();
    let mut run = db::Run::start(&db, &base, "packets").unwrap();
    capture::packets(&mut db, &mut client, 1, &mut run, &|| false).unwrap();
    let cursor = db::cursor(&db, &base).unwrap();
    assert!(run.capped);
    assert_eq!(run.observations, 2);
    assert_eq!(cursor.next_offset, 2);
    assert_eq!(cursor.last_packet_id, Some(1316284));
    drop(db);

    let mut db = db::open(&path, &base).unwrap();
    let mut run = db::Run::start(&db, &base, "packets").unwrap();
    capture::packets(&mut db, &mut client, 1, &mut run, &|| false).unwrap();
    assert!(!run.capped);
    assert_eq!(run.observations, 1);
    assert_eq!(
        db::cursor(&db, &base).unwrap().completed_until,
        cursor.window_until
    );
    capture::packets(&mut db, &mut client, 1, &mut run, &|| false).unwrap();
    let calls = server.join().unwrap();
    assert!(calls[0].0.contains("GET /api/packets?"));
    assert!(calls[0].0.contains("expand=observations"));
    assert!(calls[0].0.contains("offset=0"));
    assert!(
        calls[0]
            .0
            .to_ascii_lowercase()
            .contains("user-agent: repeaternet/")
    );
    assert!(
        calls[0]
            .0
            .to_ascii_lowercase()
            .contains("accept-encoding: gzip")
    );
    assert!(calls[1].0.contains("offset=1"));
    let until = |call: &str| {
        call.split("until=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_owned()
    };
    assert_eq!(until(&calls[0].0), until(&calls[1].0));
    assert_eq!(calls[1].0, calls[2].0);
    assert_eq!(calls[2].0, calls[3].0);
    assert!(calls[2].1.duration_since(calls[1].1) >= Duration::from_secs(2));
    assert!(calls[3].1.duration_since(calls[2].1) >= Duration::from_secs(2));
    assert!(calls[4].0.contains("since="));
    assert!(calls[4].0.contains("offset=0"));
    assert_eq!(
        db::requests_today(&db, &base, chrono::Utc::now().timestamp_millis()).unwrap(),
        5
    );
}

#[test]
fn watch_defers_retries_and_uses_server_validators() {
    let body = json!({"observers": []});
    let (base, server) = mock(vec![
        (200, "ETag: \"observers-v1\"\r\n", body.clone()),
        (304, "", Value::Null),
        (429, "Retry-After: 270\r\n", Value::Null),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let db = db::open(&dir.path().join("watch.sqlite3"), &base).unwrap();
    let mut client = CaptureClient::new(&base, Duration::from_millis(10)).unwrap();
    client.defer_retries();
    for _ in 0..2 {
        assert_eq!(
            capture::get(&db, &mut client, Endpoint::Observers, &[], &|| false).unwrap(),
            body
        );
    }
    assert!(capture::get(&db, &mut client, Endpoint::Observers, &[], &|| false).is_err());
    assert_eq!(client.retry_after(), Duration::from_secs(270));
    let calls = server.join().unwrap();
    assert!(
        calls[1]
            .0
            .to_ascii_lowercase()
            .contains("if-none-match: \"observers-v1\"")
    );
    assert_eq!(client.requests(), 3);
    assert_eq!(
        db::requests_today(&db, &base, chrono::Utc::now().timestamp_millis()).unwrap(),
        3
    );
}

#[test]
fn neighbor_batches_rotate_across_restarts_and_respect_the_rolling_day() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("neighbors.sqlite3");
    let mut db = db::open(&path, "sample").unwrap();
    let run = db::Run::start(&db, "sample", "watch-topology").unwrap();
    let inside = json!({"lat": 56, "lon": 11});
    let outside = json!({"lat": 52, "lon": 11});
    db::save_snapshot(
        &mut db,
        db::Snapshot::Nodes,
        run.id,
        [
            ("a", &inside),
            ("b", &inside),
            ("c", &inside),
            ("d", &inside),
            ("e", &outside),
        ],
    )
    .unwrap();
    run.finish(&db, 0, false).unwrap();
    let bbox = "54,71.5,4,31.7".parse().unwrap();
    let day = 86_400_000;
    let start = 10 * day;
    assert_eq!(
        db::neighbor_batch(&db, "sample", bbox, start, 2, day).unwrap(),
        ["a", "b"]
    );
    for (key, sent) in [
        ("a", start + 10),
        ("b", start + 20),
        ("c", start - day),
        ("d", start - 2 * day),
    ] {
        db.execute(
            "INSERT INTO request_log VALUES ('sample', ?, ?)",
            rusqlite::params![sent, key],
        )
        .unwrap();
    }
    drop(db);
    let db = db::open(&path, "sample").unwrap();
    assert_eq!(
        db::neighbor_batch(&db, "sample", bbox, start + 20, 20, day).unwrap(),
        ["d", "c"]
    );
    assert_eq!(
        db::neighbor_batch(&db, "sample", bbox, start + day + 9, 20, day).unwrap(),
        ["d", "c"]
    );
    assert_eq!(
        db::neighbor_batch(&db, "sample", bbox, start + day + 10, 20, day).unwrap(),
        ["d", "c", "a"]
    );
    assert!(
        db::neighbor_batch(&db, "sample", bbox, start + day + 10, 20, 7 * day)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db::neighbor_batch(&db, "sample", bbox, start + 5 * day, 20, 7 * day).unwrap(),
        ["d"]
    );
    db.execute(
        "WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM numbers WHERE n < 299)
         INSERT INTO request_log SELECT 'sample', ?, 'other-' || n FROM numbers",
        [start + day],
    )
    .unwrap();
    assert!(
        db::neighbor_batch(&db, "sample", bbox, start + day + 10, 20, day)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn on8ar_snapshot_and_expanded_packets() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/on8ar.json")).unwrap();
    let nodes = fixture["nodes"]["nodes"].as_array().unwrap();
    // v3.18 can hide rows after paging, so a short page with has_more
    // advances by the requested limit.
    let (base, server) = mock(vec![
        (200, "", fixture["stats"].clone()),
        (200, "", fixture["observers"].clone()),
        (200, "", fixture["graph"].clone()),
        (
            200,
            "",
            json!({"nodes": [&nodes[0]], "total": 1, "has_more": true}),
        ),
        (
            200,
            "",
            json!({"nodes": [&nodes[1]], "total": 1, "has_more": false}),
        ),
        (200, "", fixture["neighbors"].clone()),
        (200, "", fixture["packets"].clone()),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let mut db = db::open(&dir.path().join("on8ar.sqlite3"), &base).unwrap();
    let mut client = CaptureClient::new(&base, Duration::from_millis(10)).unwrap();
    let mut topology = db::Run::start(&db, &base, "snapshot").unwrap();
    capture::topology(&mut db, &mut client, 2, &mut topology, &|| false).unwrap();
    assert_eq!(topology.version.as_deref(), Some("v3.18.0-on8ar.3"));
    assert_eq!(
        (topology.nodes, topology.observers, topology.neighbors),
        (2, 3, 1)
    );
    topology.finish(&db, 5, false).unwrap();

    let bbox = "49.4,53.6,2.5,7.3".parse().unwrap();
    let mut neighbors = db::Run::start(&db, &base, "neighbors").unwrap();
    capture::neighbors(
        &mut db,
        &mut client,
        bbox,
        1,
        168 * 3_600_000,
        &mut neighbors,
        &|| false,
    )
    .unwrap();
    assert_eq!(neighbors.neighbors, 1);

    let mut packets = db::Run::start(&db, &base, "packets").unwrap();
    capture::packets(&mut db, &mut client, 1, &mut packets, &|| false).unwrap();
    assert_eq!(packets.observations, 4);
    assert!(packets.capped);
    let (rssi, snr, prefix, unresolved, last_hop, sender): (
        f64,
        f64,
        String,
        String,
        String,
        String,
    ) = db
        .query_row(
            "SELECT rssi, snr, json_extract(path_prefixes, '$[0]'),
                    json_type(resolved_path, '$[3]'), last_hop, sender_pubkey
             FROM observations WHERE observation_id = 53013927",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        (rssi, snr, prefix.as_str(), unresolved.as_str()),
        (-127.0, -11.5, "d4be", "null")
    );
    assert_eq!(last_hop.len(), 64);
    assert_eq!(sender.len(), 64);
    let calls = server.join().unwrap();
    assert!(calls[4].0.contains("offset=500"));
    let node = fixture["neighbors"]["node"].as_str().unwrap();
    assert!(
        calls[5]
            .0
            .starts_with(&format!("GET /api/nodes/{node}/neighbors "))
    );
}

#[test]
fn default_database_names_follow_the_instance_host() {
    let name = |url| default_database_filename(&CaptureClient::new(url, Duration::ZERO).unwrap());
    assert_eq!(
        name("https://CORESCOPE.eth0.no/"),
        "corescope-capture.sqlite3"
    );
    assert_eq!(name(ON8AR), "corescope-capture-analyzer.on8ar.eu.sqlite3");
    assert_eq!(
        name("http://[::1]:8080/test"),
        "corescope-capture----1-.sqlite3"
    );
}

#[test]
fn new_database_rejects_another_instance_before_any_capture() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("capture.sqlite3");
    drop(db::open(&path, ON8AR).unwrap());
    let error = db::open(&path, "https://analyzer.on8ar.eu/other")
        .unwrap_err()
        .to_string();
    assert!(error.contains(ON8AR) && error.contains("--db"));
    db::open(&path, ON8AR).unwrap();
}

#[test]
fn adopts_existing_version_two_database_and_preserves_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corescope-capture.sqlite3");
    let legacy = rusqlite::Connection::open(&path).unwrap();
    legacy.execute_batch(include_str!("schema.sql")).unwrap();
    legacy
        .execute_batch(include_str!("watch_schema.sql"))
        .unwrap();
    let run = db::Run::start(&legacy, DEFAULT_BASE_URL, "watch-topology").unwrap();
    run.finish(&legacy, 1, false).unwrap();
    db::log_request(&legacy, DEFAULT_BASE_URL, None).unwrap();
    legacy
        .execute(
            "INSERT INTO packet_cursor VALUES (?, 100, 200, 13, 42)",
            [DEFAULT_BASE_URL],
        )
        .unwrap();
    let error = db::open(&path, ON8AR).unwrap_err().to_string();
    assert!(error.contains(DEFAULT_BASE_URL) && error.contains("--db"));

    db::log_request(&legacy, ON8AR, None).unwrap();
    let error = db::open(&path, DEFAULT_BASE_URL).unwrap_err().to_string();
    assert!(error.contains("multiple CoreScope instances"));
    legacy
        .execute("DELETE FROM request_log WHERE endpoint = ?", [ON8AR])
        .unwrap();
    drop(legacy);

    let db = db::open(&path, DEFAULT_BASE_URL).unwrap();
    assert_eq!(
        db::cursor(&db, DEFAULT_BASE_URL).unwrap(),
        db::Cursor {
            completed_until: Some(100),
            window_until: Some(200),
            next_offset: 13,
            last_packet_id: Some(42),
        }
    );
    assert!(
        db::latest_topology(&db, DEFAULT_BASE_URL)
            .unwrap()
            .is_some()
    );
    let legacy = rusqlite::Connection::open(&path).unwrap();
    let version: u32 = legacy
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
    assert!(db::log_request(&legacy, ON8AR, None).is_err());
    assert!(db::Run::start(&legacy, ON8AR, "snapshot").is_err());
    db::log_request(&legacy, DEFAULT_BASE_URL, None).unwrap();
}

fn mock(
    replies: Vec<(u16, &'static str, Value)>,
) -> (String, thread::JoinHandle<Vec<(String, Instant)>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        replies.into_iter().map(|(status, headers, body)| {
            let (stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let received = Instant::now();
            let body = body.to_string();
            write!(reader.get_mut(),
                "HTTP/1.1 {status} Capture mock\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len()
            ).unwrap();
            (request, received)
        }).collect()
    });
    (base, server)
}
