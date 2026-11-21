CREATE TABLE IF NOT EXISTS capture_instance (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    base_url TEXT NOT NULL
);

-- Older version 2 builds can still write to the file for the same instance.
CREATE TRIGGER IF NOT EXISTS capture_runs_instance
BEFORE INSERT ON capture_runs
WHEN NEW.endpoint <> (SELECT base_url FROM capture_instance)
BEGIN
    SELECT RAISE(ABORT, 'Capture database belongs to a different CoreScope instance');
END;

CREATE TRIGGER IF NOT EXISTS observations_instance
BEFORE INSERT ON observations
WHEN NEW.endpoint <> (SELECT base_url FROM capture_instance)
BEGIN
    SELECT RAISE(ABORT, 'Capture database belongs to a different CoreScope instance');
END;

CREATE TRIGGER IF NOT EXISTS packet_cursor_instance
BEFORE INSERT ON packet_cursor
WHEN NEW.endpoint <> (SELECT base_url FROM capture_instance)
BEGIN
    SELECT RAISE(ABORT, 'Capture database belongs to a different CoreScope instance');
END;

CREATE TRIGGER IF NOT EXISTS request_log_instance
BEFORE INSERT ON request_log
WHEN NEW.endpoint <> (SELECT base_url FROM capture_instance)
BEGIN
    SELECT RAISE(ABORT, 'Capture database belongs to a different CoreScope instance');
END;

CREATE TRIGGER IF NOT EXISTS watch_state_instance
BEFORE INSERT ON watch_state
WHEN NEW.endpoint <> (SELECT base_url FROM capture_instance)
BEGIN
    SELECT RAISE(ABORT, 'Capture database belongs to a different CoreScope instance');
END;
