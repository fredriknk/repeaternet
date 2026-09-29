"""Transactional, workspace-scoped persistence for web planner projects."""

from __future__ import annotations

import json
import secrets
import shutil
import sqlite3
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any


class ProjectStore:
    """Small SQLite project catalog; project files remain in workspace folders."""

    def __init__(self, database: Path, data_root: Path) -> None:
        self.database = database
        self.data_root = data_root
        self.data_root.mkdir(parents=True, exist_ok=True)
        with self._connection() as connection:
            connection.execute("PRAGMA journal_mode=WAL")
            connection.executescript(
                """
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
                CREATE TABLE IF NOT EXISTS workspace_state (
                    workspace_key TEXT PRIMARY KEY,
                    active_project_id TEXT NOT NULL
                );
                PRAGMA user_version=1;
                """
            )

    def _connect(self) -> sqlite3.Connection:
        connection = sqlite3.connect(self.database, timeout=10.0)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA foreign_keys=ON")
        return connection

    @contextmanager
    def _connection(self) -> Iterator[sqlite3.Connection]:
        connection = self._connect()
        try:
            with connection:
                yield connection
        finally:
            connection.close()

    def _workspace_path(self, key: str) -> Path:
        return self.data_root / key

    def project_path(self, key: str, project: dict[str, Any]) -> Path:
        base = self._workspace_path(key).resolve()
        path = (base / str(project["directory"])).resolve()
        if path != base and base not in path.parents:
            raise ValueError("Project storage path escaped its workspace")
        path.mkdir(parents=True, exist_ok=True)
        return path

    @staticmethod
    def _project(row: sqlite3.Row | None) -> dict[str, Any] | None:
        if row is None:
            return None
        project = dict(row)
        project["plan"] = json.loads(project.pop("plan_json"))
        previous = project.pop("previous_plan_json")
        project["previous_plan"] = json.loads(previous) if previous else None
        summary = project.pop("result_summary_json")
        project["result_summary"] = json.loads(summary) if summary else None
        project["archived"] = bool(project["archived"])
        return project

    def _get(self, connection: sqlite3.Connection, key: str, project_id: str) -> dict[str, Any] | None:
        row = connection.execute(
            "SELECT * FROM projects WHERE workspace_key=? AND id=?", (key, project_id)
        ).fetchone()
        return self._project(row)

    def _insert(
        self,
        connection: sqlite3.Connection,
        key: str,
        name: str,
        plan: dict[str, Any],
        directory: str,
    ) -> str:
        project_id = secrets.token_hex(10)
        now = time.time()
        connection.execute(
            """INSERT INTO projects
               (id, workspace_key, name, directory, plan_json, created_at, updated_at)
               VALUES (?, ?, ?, ?, ?, ?, ?)""",
            (project_id, key, name, directory, json.dumps(plan), now, now),
        )
        return project_id

    def active(self, key: str) -> dict[str, Any]:
        with self._connection() as connection:
            row = connection.execute(
                "SELECT active_project_id FROM workspace_state WHERE workspace_key=?", (key,)
            ).fetchone()
            project = self._get(connection, key, row[0]) if row else None
            if project and not project["archived"]:
                if project["run_state"] in {"queued", "running", "preparing"}:
                    connection.execute(
                        "UPDATE projects SET run_state='interrupted', updated_at=? WHERE id=?",
                        (time.time(), project["id"]),
                    )
                    project["run_state"] = "interrupted"
                return project

            row = connection.execute(
                "SELECT * FROM projects WHERE workspace_key=? AND archived=0 "
                "ORDER BY updated_at DESC LIMIT 1",
                (key,),
            ).fetchone()
            project = self._project(row)
            if project is None:
                legacy = self._workspace_path(key)
                has_legacy_data = any(
                    (legacy / name).exists()
                    for name in (
                        "plan.json",
                        "dtm",
                        "dom",
                        "terrain-generations",
                        "kartverket-cache",
                    )
                )
                directory = "." if has_legacy_data else "projects/new"
                if directory != ".":
                    directory = f"projects/{secrets.token_hex(10)}"
                legacy_plan = legacy / "plan.json"
                try:
                    plan = json.loads(legacy_plan.read_text(encoding="utf-8")) if legacy_plan.exists() else {}
                except (OSError, json.JSONDecodeError):
                    plan = {}
                project_id = self._insert(connection, key, "Untitled plan", plan, directory)
                project = self._get(connection, key, project_id)
                assert project is not None
            connection.execute(
                "INSERT INTO workspace_state(workspace_key, active_project_id) VALUES (?, ?) "
                "ON CONFLICT(workspace_key) DO UPDATE SET active_project_id=excluded.active_project_id",
                (key, project["id"]),
            )
            return project

    def list(self, key: str) -> list[dict[str, Any]]:
        with self._connection() as connection:
            rows = connection.execute(
                "SELECT * FROM projects WHERE workspace_key=? AND archived=0 "
                "ORDER BY updated_at DESC",
                (key,),
            ).fetchall()
            return [self._project(row) or {} for row in rows]

    def list_archived(self, key: str) -> list[dict[str, Any]]:
        with self._connection() as connection:
            rows = connection.execute(
                "SELECT * FROM projects WHERE workspace_key=? AND archived=1 "
                "ORDER BY updated_at DESC",
                (key,),
            ).fetchall()
            return [self._project(row) or {} for row in rows]

    def get(self, key: str, project_id: str) -> dict[str, Any] | None:
        with self._connection() as connection:
            return self._get(connection, key, project_id)

    def activate(self, key: str, project_id: str) -> dict[str, Any] | None:
        with self._connection() as connection:
            project = self._get(connection, key, project_id)
            if project is None or project["archived"]:
                return None
            connection.execute(
                "INSERT INTO workspace_state(workspace_key, active_project_id) VALUES (?, ?) "
                "ON CONFLICT(workspace_key) DO UPDATE SET active_project_id=excluded.active_project_id",
                (key, project_id),
            )
            return project

    def create(self, key: str, name: str, plan: dict[str, Any]) -> dict[str, Any]:
        with self._connection() as connection:
            project_id = self._insert(connection, key, name, plan, f"projects/{secrets.token_hex(10)}")
            connection.execute(
                "INSERT INTO workspace_state(workspace_key, active_project_id) VALUES (?, ?) "
                "ON CONFLICT(workspace_key) DO UPDATE SET active_project_id=excluded.active_project_id",
                (key, project_id),
            )
            return self._get(connection, key, project_id) or {}

    def autosave(self, key: str, project_id: str, plan: dict[str, Any]) -> bool:
        encoded = json.dumps(plan, separators=(",", ":"))
        with self._connection() as connection:
            row = connection.execute(
                "SELECT plan_json FROM projects WHERE workspace_key=? AND id=? AND archived=0",
                (key, project_id),
            ).fetchone()
            if row is None:
                return False
            if row[0] != encoded:
                connection.execute(
                    "UPDATE projects SET previous_plan_json=plan_json, plan_json=?, updated_at=? "
                    "WHERE workspace_key=? AND id=?",
                    (encoded, time.time(), key, project_id),
                )
        return True

    def rename(self, key: str, project_id: str, name: str) -> bool:
        with self._connection() as connection:
            cursor = connection.execute(
                "UPDATE projects SET name=?, updated_at=? WHERE workspace_key=? AND id=? AND archived=0",
                (name, time.time(), key, project_id),
            )
            return cursor.rowcount == 1

    def duplicate(self, key: str, project_id: str, name: str) -> dict[str, Any] | None:
        with self._connection() as connection:
            source = self._get(connection, key, project_id)
            if source is None or source["archived"]:
                return None
            target_id = self._insert(
                connection, key, name, source["plan"], f"projects/{secrets.token_hex(10)}"
            )
            target = self._get(connection, key, target_id)
            assert target is not None
            source_dir = self.project_path(key, source)
            target_dir = self.project_path(key, target)
            for name in ("dtm", "dom", "terrain-generations"):
                source_path = source_dir / name
                target_path = target_dir / name
                if source_path.exists():
                    shutil.copytree(source_path, target_path, copy_function=shutil.copy2)
            connection.execute(
                "INSERT INTO workspace_state(workspace_key, active_project_id) VALUES (?, ?) "
                "ON CONFLICT(workspace_key) DO UPDATE SET active_project_id=excluded.active_project_id",
                (key, target_id),
            )
            return target

    def archive(self, key: str, project_id: str) -> bool:
        with self._connection() as connection:
            active = connection.execute(
                "SELECT active_project_id FROM workspace_state WHERE workspace_key=?", (key,)
            ).fetchone()
            if active and active[0] == project_id:
                raise ValueError("Switch to another project before archiving this one")
            cursor = connection.execute(
                "UPDATE projects SET archived=1, updated_at=? WHERE workspace_key=? AND id=?",
                (time.time(), key, project_id),
            )
            return cursor.rowcount == 1

    def restore(self, key: str, project_id: str, name: str) -> dict[str, Any] | None:
        with self._connection() as connection:
            cursor = connection.execute(
                "UPDATE projects SET name=?, archived=0, updated_at=? "
                "WHERE workspace_key=? AND id=? AND archived=1",
                (name, time.time(), key, project_id),
            )
            if cursor.rowcount != 1:
                return None
            connection.execute(
                "INSERT INTO workspace_state(workspace_key, active_project_id) VALUES (?, ?) "
                "ON CONFLICT(workspace_key) DO UPDATE SET active_project_id=excluded.active_project_id",
                (key, project_id),
            )
            return self._get(connection, key, project_id)

    def delete(self, key: str, project_id: str) -> dict[str, Any] | None:
        with self._connection() as connection:
            active = connection.execute(
                "SELECT active_project_id FROM workspace_state WHERE workspace_key=?", (key,)
            ).fetchone()
            if active and active[0] == project_id:
                raise ValueError("Switch to another project before deleting this one")
            project = self._get(connection, key, project_id)
            if project is None:
                return None
            connection.execute(
                "DELETE FROM projects WHERE workspace_key=? AND id=?", (key, project_id)
            )
            return project

    def set_run_state(self, key: str, project_id: str, state: str) -> None:
        with self._connection() as connection:
            connection.execute(
                "UPDATE projects SET run_state=?, updated_at=? WHERE workspace_key=? AND id=?",
                (state, time.time(), key, project_id),
            )

    def save_result_summary(
        self, key: str, project_id: str, summary: dict[str, Any] | None
    ) -> None:
        with self._connection() as connection:
            connection.execute(
                "UPDATE projects SET result_summary_json=?, updated_at=? "
                "WHERE workspace_key=? AND id=?",
                (json.dumps(summary) if summary is not None else None, time.time(), key, project_id),
            )

    def storage_usage(self, key: str) -> int:
        base = self._workspace_path(key)
        return sum(path.stat().st_size for path in base.rglob("*") if path.is_file()) if base.exists() else 0
