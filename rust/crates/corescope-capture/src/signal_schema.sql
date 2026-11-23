CREATE TABLE IF NOT EXISTS trace_hops (
    endpoint TEXT NOT NULL,
    packet_id INTEGER NOT NULL,
    packet_hash TEXT NOT NULL,
    observer_id TEXT,
    timestamp TEXT NOT NULL,
    hash_size INTEGER NOT NULL,
    path_hashes TEXT NOT NULL,
    snr_values TEXT NOT NULL,
    PRIMARY KEY (endpoint, packet_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS observer_neighbor_reports (
    endpoint TEXT NOT NULL,
    observer_id TEXT NOT NULL,
    received_at TEXT NOT NULL,
    reported_at TEXT,
    neighbor_pubkey TEXT NOT NULL,
    snr REAL,
    heard_secs_ago INTEGER,
    status TEXT,
    PRIMARY KEY (endpoint, observer_id, received_at, neighbor_pubkey)
) WITHOUT ROWID;

-- Empty reports also need a checkpoint so they don't consume every refresh budget.
CREATE TABLE IF NOT EXISTS observer_neighbor_checkpoints (
    endpoint TEXT NOT NULL,
    observer_id TEXT NOT NULL,
    received_at TEXT NOT NULL,
    PRIMARY KEY (endpoint, observer_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS reception_signal (
    endpoint TEXT NOT NULL,
    packet_id INTEGER NOT NULL,
    observation_key TEXT NOT NULL,
    radio TEXT,
    score INTEGER,
    PRIMARY KEY (endpoint, packet_id, observation_key),
    FOREIGN KEY (endpoint, packet_id, observation_key)
        REFERENCES observations(endpoint, packet_id, observation_key) ON DELETE CASCADE
) WITHOUT ROWID;
