WITH flights AS (
    SELECT source_id, first_row, last_row,
           (first_ms >= :since_ms AND last_ms <= :until_ms
             AND south >= :south AND north <= :north
             AND ((west >= :west AND east <= :east) OR (:wrap AND east <= :wrapped_east))) AS covered
      FROM flight_extent
     WHERE last_ms >= :since_ms AND first_ms <= :until_ms
       AND north >= :south AND south <= :north
       AND ((east >= :west AND west <= :east) OR (:wrap AND west <= :wrapped_east))
), groups AS (
    SELECT kind,
           CASE WHEN :hide_suspect THEN clean_sum ELSE all_sum END AS signal_sum,
           CASE WHEN :hide_suspect THEN clean_count ELSE all_count END AS samples,
           CASE WHEN :hide_suspect THEN clean_row ELSE all_row END AS last_row
      FROM flights f
      CROSS JOIN flight_signal s ON s.source_id = f.source_id
     WHERE f.covered AND (NOT :hide_suspect OR clean_count > 0)
       AND (:evidence IS NULL OR kind = :evidence)
       AND (:agl_bands IS NULL OR agl_band < :agl_bands)
    UNION ALL
    SELECT kind, sum(samples * mean), sum(samples), max(id)
      FROM flights f
      CROSS JOIN radiosonde_signal s ON s.id BETWEEN f.first_row AND f.last_row
     WHERE NOT f.covered
       AND observed_at_ms BETWEEN :since_ms AND :until_ms
       AND tx_lat BETWEEN :south AND :north
       AND (tx_lon BETWEEN :west AND :east OR (:wrap AND tx_lon <= :wrapped_east))
       AND (:evidence IS NULL OR kind = :evidence)
       AND (:agl_bands IS NULL OR agl_band < :agl_bands)
       AND (NOT :hide_suspect OR (quality_flags & :suspect_flags) = 0)
     GROUP BY f.source_id, receiver_id, kind, agl_band
), limited AS (
    SELECT kind, signal_sum / samples AS mean, samples, last_row, count(*) OVER () AS total
      FROM groups
     ORDER BY samples DESC, last_row DESC, kind
     LIMIT :limit
)
SELECT serial, receiver_key, tx_lat, tx_lon, rx_lat, rx_lon, tx_agl_m, quality_flags,
       kind, mean, samples, observed_at_ms, total
  FROM limited
  CROSS JOIN measured_link m ON m.id = limited.last_row
  CROSS JOIN receiver ON receiver.id = m.receiver_id
 ORDER BY samples DESC, limited.last_row DESC, kind
