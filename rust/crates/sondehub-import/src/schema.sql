CREATE TABLE IF NOT EXISTS import_settings (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    low_bin_seconds INTEGER NOT NULL,
    high_bin_seconds INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS source_object (
    id INTEGER PRIMARY KEY,
    locator TEXT NOT NULL UNIQUE,
    archive_day TEXT NOT NULL,
    etag TEXT,
    sha256 TEXT,
    retrieved_at INTEGER NOT NULL,
    licence TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('complete', 'missing', 'invalid')),
    download_bytes INTEGER NOT NULL,
    input_rows INTEGER NOT NULL,
    kept_rows INTEGER NOT NULL,
    located_links INTEGER NOT NULL,
    reject_count INTEGER NOT NULL,
    rejects TEXT NOT NULL,
    last_observed INTEGER
);

CREATE TABLE IF NOT EXISTS receiver (
    id INTEGER PRIMARY KEY,
    receiver_key TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS measured_link (
    id INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES source_object(id),
    receiver_id INTEGER NOT NULL REFERENCES receiver(id),
    serial TEXT NOT NULL,
    frame INTEGER NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    received_at_ms INTEGER,
    tx_lat REAL NOT NULL,
    tx_lon REAL NOT NULL,
    tx_alt_m REAL NOT NULL,
    tx_alt_datum TEXT NOT NULL,
    tx_agl_m REAL,
    rx_lat REAL NOT NULL,
    rx_lon REAL NOT NULL,
    rx_alt_m REAL,
    rx_alt_datum TEXT NOT NULL,
    frequency_hz INTEGER NOT NULL,
    frequency_basis TEXT NOT NULL,
    modulation TEXT,
    sonde_type TEXT NOT NULL,
    tx_power_mw REAL,
    tx_power_basis TEXT,
    decoder TEXT NOT NULL,
    decoder_version TEXT,
    rx_antenna TEXT,
    lna INTEGER NOT NULL,
    bin_start INTEGER NOT NULL,
    bin_seconds INTEGER NOT NULL,
    bin_count INTEGER NOT NULL,
    rssi_dbm REAL,
    rssi_kind TEXT,
    rssi_count INTEGER NOT NULL,
    rssi_min REAL,
    rssi_max REAL,
    rssi_mean REAL,
    snr_db REAL,
    snr_kind TEXT,
    snr_count INTEGER NOT NULL,
    snr_min REAL,
    snr_max REAL,
    snr_mean REAL,
    quality_flags INTEGER NOT NULL
);

-- Rows for a flight are contiguous and written in observation order.
CREATE TABLE IF NOT EXISTS flight_extent (
    source_id INTEGER PRIMARY KEY REFERENCES source_object(id),
    first_row INTEGER NOT NULL,
    last_row INTEGER NOT NULL,
    south REAL NOT NULL,
    north REAL NOT NULL,
    west REAL NOT NULL,
    east REAL NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS flight_extent_last_ms ON flight_extent(last_ms);

CREATE TABLE IF NOT EXISTS flight_signal (
    source_id INTEGER NOT NULL REFERENCES flight_extent(source_id),
    receiver_id INTEGER NOT NULL REFERENCES receiver(id),
    kind TEXT NOT NULL,
    agl_band INTEGER NOT NULL,
    all_count INTEGER NOT NULL,
    all_sum REAL NOT NULL,
    all_row INTEGER NOT NULL,
    clean_count INTEGER NOT NULL,
    clean_sum REAL,
    clean_row INTEGER,
    PRIMARY KEY (source_id, receiver_id, kind, agl_band)
) WITHOUT ROWID;

CREATE VIEW IF NOT EXISTS radiosonde_signal AS
SELECT m.*, metrics.kind,
       CASE WHEN tx_agl_m IS NULL THEN 3 WHEN tx_agl_m < 1000 THEN 0
            WHEN tx_agl_m < 3000 THEN 1 ELSE 2 END AS agl_band,
       CASE metrics.kind WHEN 'sonde_snr_auto_rx' THEN snr_count ELSE rssi_count END AS samples,
       CASE metrics.kind WHEN 'sonde_snr_auto_rx' THEN snr_mean ELSE rssi_mean END AS mean
  FROM measured_link m
 CROSS JOIN (SELECT 'sonde_snr_auto_rx' AS kind UNION ALL SELECT 'sonde_rssi_rdz') metrics
 WHERE (metrics.kind = 'sonde_snr_auto_rx' AND snr_kind = 'radiosonde_auto_rx')
    OR (metrics.kind = 'sonde_rssi_rdz' AND rssi_kind = 'rdzTTGOsonde');
