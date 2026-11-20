CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    workspace_key TEXT NOT NULL,
    name TEXT NOT NULL,
    directory TEXT NOT NULL,
    plan_json TEXT NOT NULL,
    previous_plan_json TEXT,
    result_summary_json TEXT,
    run_state TEXT NOT NULL DEFAULT 'idle',
    archived INTEGER NOT NULL DEFAULT 0,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL,
    UNIQUE(workspace_key, name)
);
CREATE INDEX IF NOT EXISTS projects_workspace_updated
    ON projects(workspace_key, archived, updated_at DESC);
CREATE TABLE IF NOT EXISTS route_results (
    project_id TEXT PRIMARY KEY REFERENCES projects(id) ON DELETE CASCADE,
    snapshot_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS workspace_state (
    workspace_key TEXT PRIMARY KEY,
    active_project_id TEXT NOT NULL
);
PRAGMA user_version=1;
