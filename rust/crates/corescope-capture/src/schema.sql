CREATE TABLE capture_runs (
    id INTEGER PRIMARY KEY,
    endpoint TEXT NOT NULL,
    mode TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    status TEXT NOT NULL DEFAULT 'running',
    server_version TEXT,
    requests INTEGER NOT NULL DEFAULT 0,
    packet_pages INTEGER NOT NULL DEFAULT 0,
    observations_added INTEGER NOT NULL DEFAULT 0,
    nodes INTEGER NOT NULL DEFAULT 0,
    observers INTEGER NOT NULL DEFAULT 0,
    neighbor_responses INTEGER NOT NULL DEFAULT 0,
    capped INTEGER NOT NULL DEFAULT 0,
    errors_json TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE packet_cursor (
    endpoint TEXT PRIMARY KEY,
    completed_until INTEGER,
    window_until INTEGER,
    next_offset INTEGER NOT NULL CHECK(next_offset >= 0),
    last_packet_id INTEGER
);

CREATE TABLE observations (
    endpoint TEXT NOT NULL,
    packet_id INTEGER NOT NULL,
    observation_key TEXT NOT NULL,
    observation_id INTEGER,
    packet_hash TEXT NOT NULL,
    observer_id TEXT,
    observer_name TEXT,
    observer_iata TEXT,
    timestamp TEXT NOT NULL,
    first_seen TEXT NOT NULL,
    rssi REAL,
    snr REAL,
    path_prefixes TEXT,
    resolved_path TEXT,
    route_type INTEGER,
    payload_type INTEGER,
    direction TEXT,
    observation_count INTEGER,
    scope_name TEXT,
    sender_pubkey TEXT,
    advert_lat REAL,
    advert_lon REAL,
    advert_flags INTEGER,
    advert_name TEXT,
    PRIMARY KEY (endpoint, packet_id, observation_key)
) WITHOUT ROWID;
CREATE INDEX observations_time ON observations(endpoint, timestamp);
CREATE INDEX observations_observer ON observations(endpoint, observer_id);

CREATE TABLE nodes_snapshot (
    run_id INTEGER NOT NULL REFERENCES capture_runs(id),
    captured_at TEXT NOT NULL,
    pubkey TEXT NOT NULL,
    lat REAL,
    lon REAL,
    raw_json TEXT NOT NULL,
    PRIMARY KEY (run_id, pubkey)
) WITHOUT ROWID;

CREATE TABLE observers_snapshot (
    run_id INTEGER NOT NULL REFERENCES capture_runs(id),
    captured_at TEXT NOT NULL,
    observer_id TEXT NOT NULL,
    lat REAL,
    lon REAL,
    raw_json TEXT NOT NULL,
    PRIMARY KEY (run_id, observer_id)
) WITHOUT ROWID;

CREATE TABLE neighbors_snapshot (
    run_id INTEGER NOT NULL REFERENCES capture_runs(id),
    captured_at TEXT NOT NULL,
    source_pubkey TEXT NOT NULL,
    raw_json TEXT NOT NULL,
    PRIMARY KEY (run_id, source_pubkey)
) WITHOUT ROWID;

PRAGMA user_version = 1;
