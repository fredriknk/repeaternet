CREATE TABLE request_log (
    endpoint TEXT NOT NULL,
    sent_ms INTEGER NOT NULL,
    neighbor TEXT
);
CREATE INDEX request_time ON request_log(endpoint, sent_ms);
CREATE INDEX request_neighbor ON request_log(endpoint, neighbor, sent_ms)
WHERE neighbor IS NOT NULL;
CREATE INDEX snapshots_by_endpoint ON capture_runs(endpoint, mode, id);
CREATE INDEX neighbors_source ON neighbors_snapshot(source_pubkey, captured_at);

CREATE TABLE watch_state (
    endpoint TEXT PRIMARY KEY,
    last_poll INTEGER,
    next_poll INTEGER,
    backoff_until INTEGER,
    backoff_reason TEXT,
    failures INTEGER NOT NULL DEFAULT 0
);

ALTER TABLE observations ADD COLUMN last_hop TEXT GENERATED ALWAYS AS (
    CASE WHEN json_array_length(path_prefixes) = 0 THEN sender_pubkey
         ELSE json_extract(resolved_path, '$[#-1]') END
) VIRTUAL;
ALTER TABLE observations ADD COLUMN zero_hop INTEGER GENERATED ALWAYS AS (
    json_array_length(path_prefixes) IS 0
) VIRTUAL;
ALTER TABLE observations ADD COLUMN seen_unix INTEGER GENERATED ALWAYS AS (
    unixepoch(timestamp)
) VIRTUAL;

CREATE INDEX observations_pairs ON observations(endpoint, last_hop, observer_id)
WHERE direction = 'rx' AND last_hop IS NOT NULL AND observer_id IS NOT NULL
    AND last_hop <> observer_id;
CREATE INDEX observations_pair_time ON observations(endpoint, seen_unix)
WHERE direction = 'rx' AND last_hop IS NOT NULL AND observer_id IS NOT NULL
    AND last_hop <> observer_id;
CREATE INDEX observations_adverts ON observations(endpoint, sender_pubkey, seen_unix DESC)
WHERE sender_pubkey IS NOT NULL AND advert_lat BETWEEN -90 AND 90
  AND advert_lon BETWEEN -180 AND 180 AND (advert_lat <> 0 OR advert_lon <> 0);

CREATE VIEW measured_hops AS
SELECT endpoint, last_hop, observer_id, zero_hop, seen_unix, snr,
       CASE WHEN rssi < 0 THEN rssi END AS rssi
FROM observations
WHERE direction = 'rx' AND last_hop IS NOT NULL AND observer_id IS NOT NULL
    AND last_hop <> observer_id;

PRAGMA user_version = 2;
