-- Distinct measured pairs, stepping through observations_pairs one key at a time
-- rather than reading every reception. Keep the conditions equal to the index's.
WITH RECURSIVE hops(last_hop) AS (
    SELECT min(observations.last_hop) FROM observations INDEXED BY observations_pairs
    WHERE endpoint = ?1 AND direction = 'rx' AND last_hop IS NOT NULL
    AND observer_id IS NOT NULL AND last_hop <> observer_id
    UNION ALL
    SELECT (SELECT min(observations.last_hop) FROM observations INDEXED BY observations_pairs
            WHERE endpoint = ?1 AND direction = 'rx' AND last_hop IS NOT NULL
    AND observer_id IS NOT NULL AND last_hop <> observer_id
              AND observations.last_hop > hops.last_hop)
    FROM hops WHERE last_hop IS NOT NULL
), pairs(last_hop, observer_id) AS (
    SELECT last_hop, (SELECT min(o.observer_id) FROM observations o INDEXED BY observations_pairs
                      WHERE o.endpoint = ?1 AND o.direction = 'rx' AND o.last_hop = hops.last_hop
                        AND o.observer_id IS NOT NULL AND o.last_hop <> o.observer_id)
    FROM hops WHERE last_hop IS NOT NULL
    UNION ALL
    SELECT last_hop, (SELECT min(o.observer_id) FROM observations o INDEXED BY observations_pairs
                      WHERE o.endpoint = ?1 AND o.direction = 'rx' AND o.last_hop = pairs.last_hop
                        AND o.observer_id IS NOT NULL AND o.last_hop <> o.observer_id
                        AND o.observer_id > pairs.observer_id)
    FROM pairs WHERE observer_id IS NOT NULL
)
SELECT last_hop, observer_id FROM pairs WHERE observer_id IS NOT NULL;
