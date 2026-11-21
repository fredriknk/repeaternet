CREATE TABLE meshtastic_instance (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    host TEXT NOT NULL UNIQUE
);
CREATE TABLE requests (sent_ms INTEGER NOT NULL);
CREATE INDEX requests_time ON requests(sent_ms);
CREATE TABLE poll_state (name TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;

CREATE TABLE positions (
    node INTEGER NOT NULL,
    observed_ms INTEGER NOT NULL,
    lat REAL NOT NULL,
    lon REAL NOT NULL,
    altitude_m REAL,
    precision_bits INTEGER,
    precision_radius_m REAL,
    location_source TEXT NOT NULL,
    PRIMARY KEY (node, observed_ms, location_source)
) WITHOUT ROWID;
CREATE TABLE radios (
    node INTEGER NOT NULL,
    observed_ms INTEGER NOT NULL,
    region TEXT,
    modem_preset TEXT,
    frequency_mhz REAL,
    frequency_slot INTEGER,
    sf INTEGER,
    bandwidth_khz REAL,
    coding_rate TEXT,
    tx_power_dbm REAL,
    hardware TEXT,
    firmware TEXT,
    role TEXT,
    old_neighbor_firmware INTEGER NOT NULL,
    PRIMARY KEY (node, observed_ms)
) WITHOUT ROWID;
CREATE TABLE measurements (
    source TEXT NOT NULL REFERENCES meshtastic_instance(host),
    observation_id TEXT NOT NULL,
    upstream_id TEXT NOT NULL,
    logical_packet_id TEXT,
    seen_ms INTEGER NOT NULL,
    tx INTEGER,
    rx INTEGER NOT NULL,
    gateway INTEGER,
    evidence TEXT NOT NULL CHECK(evidence IN ('gateway_rx','traceroute_towards','traceroute_back','neighbor')),
    hop_index INTEGER,
    snr_db REAL,
    rssi_dbm REAL,
    hop_start INTEGER,
    hop_limit INTEGER,
    relay_hint INTEGER,
    relay_state TEXT,
    via_mqtt INTEGER,
    region TEXT,
    provenance TEXT NOT NULL,
    CHECK (snr_db IS NOT NULL OR rssi_dbm IS NOT NULL),
    PRIMARY KEY(source, observation_id)
) WITHOUT ROWID;
CREATE INDEX measurements_time ON measurements(seen_ms);
CREATE INDEX measurements_pair ON measurements(tx, rx);

-- Position and radio histories remain available for time-aware calibration.
-- The live map uses the latest available reported position for each endpoint.
CREATE VIEW latest_positions AS
SELECT p.* FROM positions p WHERE (p.observed_ms, p.location_source) = (
    SELECT observed_ms, location_source FROM positions WHERE node = p.node
    ORDER BY observed_ms DESC, location_source DESC LIMIT 1
);
CREATE VIEW measurement_flags AS
SELECT m.*, a.lat AS tx_lat, a.lon AS tx_lon, b.lat AS rx_lat, b.lon AS rx_lon,
       COALESCE(a.precision_radius_m > 100, 0) OR COALESCE(b.precision_radius_m > 100, 0) AS coarse,
       a.precision_radius_m IS NULL OR b.precision_radius_m IS NULL AS unknown_precision,
       COALESCE(m.region, (SELECT region FROM radios WHERE node = m.tx AND region IS NOT NULL ORDER BY observed_ms DESC LIMIT 1),
                         (SELECT region FROM radios WHERE node = m.rx AND region IS NOT NULL ORDER BY observed_ms DESC LIMIT 1)) AS link_region,
       COALESCE((SELECT modem_preset FROM radios WHERE node = m.tx AND modem_preset IS NOT NULL ORDER BY observed_ms DESC LIMIT 1),
                (SELECT modem_preset FROM radios WHERE node = m.rx AND modem_preset IS NOT NULL ORDER BY observed_ms DESC LIMIT 1)) AS modem_preset,
       m.evidence = 'neighbor' AND (
           COALESCE(m.snr_db = 0, 0)
           OR COALESCE((SELECT old_neighbor_firmware FROM radios WHERE node = m.rx AND firmware IS NOT NULL AND observed_ms <= m.seen_ms ORDER BY observed_ms DESC LIMIT 1),
                       (SELECT old_neighbor_firmware FROM radios WHERE node = m.rx AND firmware IS NOT NULL ORDER BY observed_ms LIMIT 1), 0)
           OR capture_over_300_km(a.lat, a.lon, b.lat, b.lon)
       ) AS mqtt_suspect
FROM measurements m LEFT JOIN latest_positions a ON a.node=m.tx LEFT JOIN latest_positions b ON b.node=m.rx;
CREATE VIEW located_measurements AS
SELECT * FROM measurement_flags WHERE tx_lat IS NOT NULL AND rx_lat IS NOT NULL;

PRAGMA user_version = 1;
