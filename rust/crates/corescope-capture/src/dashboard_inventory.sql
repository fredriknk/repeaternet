WITH latest AS (
    SELECT id FROM capture_runs WHERE endpoint = ?1
      AND mode IN ('watch-topology', 'snapshot', 'run')
      AND status = 'complete' ORDER BY id DESC LIMIT 1
), nodes AS (
    SELECT pubkey, lat, lon, NULLIF(json_extract(raw_json, '$.name'), '') AS name
    FROM nodes_snapshot WHERE run_id = (SELECT id FROM latest)
), valid_adverts AS (
    SELECT sender_pubkey AS pubkey, advert_lat AS lat, advert_lon AS lon,
           NULLIF(advert_name, '') AS name,
           row_number() OVER (PARTITION BY sender_pubkey ORDER BY seen_unix DESC, packet_id DESC) AS priority
    FROM observations
    WHERE endpoint = ?1 AND sender_pubkey IS NOT NULL
      AND advert_lat BETWEEN -90 AND 90 AND advert_lon BETWEEN -180 AND 180
      AND (advert_lat <> 0 OR advert_lon <> 0)
), adverts AS (
    SELECT pubkey, lat, lon, name FROM valid_adverts WHERE priority = 1
), observers AS (
    SELECT observer_id, lat, lon, NULLIF(json_extract(raw_json, '$.name'), '') AS name,
           lower(COALESCE(json_extract(raw_json, '$.public_key'), json_extract(raw_json, '$.pubkey'))) AS pubkey
    FROM observers_snapshot WHERE run_id = (SELECT id FROM latest)
), observed AS (
    SELECT DISTINCT observer_id FROM observations WHERE endpoint = ?1 AND observer_id IS NOT NULL
)
SELECT 0 AS is_observer, n.pubkey AS id, COALESCE(n.name, a.name), n.lat, n.lon, NULL
FROM nodes n LEFT JOIN adverts a ON a.pubkey = n.pubkey
WHERE n.lat BETWEEN -90 AND 90 AND n.lon BETWEEN -180 AND 180 AND (n.lat <> 0 OR n.lon <> 0)
UNION ALL
SELECT 0, n.pubkey, COALESCE(n.name, a.name), a.lat, a.lon, NULL
FROM nodes n LEFT JOIN adverts a ON a.pubkey = n.pubkey
WHERE NOT COALESCE(n.lat BETWEEN -90 AND 90 AND n.lon BETWEEN -180 AND 180
                   AND (n.lat <> 0 OR n.lon <> 0), 0)
UNION ALL
SELECT 0, a.pubkey, a.name, a.lat, a.lon, NULL
FROM adverts a WHERE NOT EXISTS (SELECT 1 FROM nodes n WHERE n.pubkey = a.pubkey)
UNION ALL
SELECT 1, observer_id, name, lat, lon, pubkey FROM observers
UNION ALL
SELECT 1, o.observer_id,
       (SELECT observer_name FROM observations
        WHERE endpoint = ?1 AND observer_id = o.observer_id AND NULLIF(observer_name, '') IS NOT NULL LIMIT 1),
       NULL, NULL, NULL
FROM observed o WHERE NOT EXISTS (SELECT 1 FROM observers s WHERE s.observer_id = o.observer_id)
ORDER BY is_observer, id;
