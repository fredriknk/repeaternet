import pytest

from rf_router_planner.project_store import ProjectStore


def test_project_store_keeps_previous_revision_and_workspace_isolation(tmp_path):
    store = ProjectStore(tmp_path / "projects.sqlite3", tmp_path)
    key_a, key_b = "a" * 48, "b" * 48
    original = store.active(key_a)
    first = {"a": [1, 2], "manual_routers": []}
    second = {"a": [3, 4], "manual_routers": [{"id": "M-one"}]}

    assert store.autosave(key_a, original["id"], first)
    assert store.autosave(key_a, original["id"], second)
    recovered = store.get(key_a, original["id"])
    assert recovered is not None
    assert recovered["plan"] == second
    assert recovered["previous_plan"] == first
    assert [item["id"] for item in store.list(key_b)] != [original["id"]]


def test_duplicate_copies_terrain_and_archive_preserves_files(tmp_path):
    store = ProjectStore(tmp_path / "projects.sqlite3", tmp_path)
    key = "c" * 48
    source = store.active(key)
    source_dir = store.project_path(key, source)
    (source_dir / "dtm").mkdir()
    (source_dir / "dtm" / "tile.tif").write_bytes(b"test terrain")
    copy = store.duplicate(key, source["id"], "Copy")
    assert copy is not None
    copy_file = store.project_path(key, copy) / "dtm" / "tile.tif"
    assert copy_file.read_bytes() == b"test terrain"
    assert store.archive(key, source["id"])
    assert (source_dir / "dtm" / "tile.tif").exists()
    assert [item["id"] for item in store.list(key)] == [copy["id"]]


def test_active_project_cannot_be_archived_or_deleted(tmp_path):
    store = ProjectStore(tmp_path / "projects.sqlite3", tmp_path)
    key = "d" * 48
    project = store.active(key)
    with pytest.raises(ValueError, match="Switch to another project"):
        store.archive(key, project["id"])
    with pytest.raises(ValueError, match="Switch to another project"):
        store.delete(key, project["id"])
