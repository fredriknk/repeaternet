INSERT INTO flight_signal
SELECT :source, receiver_id, kind, agl_band,
       sum(samples), sum(samples * mean), max(id),
       coalesce(sum(samples) FILTER (WHERE (quality_flags & :suspect) = 0), 0),
       sum(samples * mean) FILTER (WHERE (quality_flags & :suspect) = 0),
       max(id) FILTER (WHERE (quality_flags & :suspect) = 0)
  FROM radiosonde_signal
 WHERE id BETWEEN :first AND :last
 GROUP BY receiver_id, kind, agl_band;
