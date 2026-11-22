//! Copy capture databases into `<work>/snapshots/` with the SQLite backup
//! API. Sources are opened read-only; capture processes may keep writing.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, backup::Backup};

/// Snapshot every `*capture*.sqlite3` and `sondehub-import.sqlite3` in
/// `source_dir` (skipping files named `backup`). Returns the copied names.
pub fn snapshot_all(source_dir: &Path, dest_dir: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(dest_dir)?;
    let mut copied = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(source_dir)?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let wanted = name.ends_with(".sqlite3")
            && !name.contains("backup")
            && (name.contains("capture") || name == "sondehub-import.sqlite3");
        if !wanted {
            continue;
        }
        let partial = dest_dir.join(format!("{name}.partial"));
        let _ = std::fs::remove_file(&partial);
        {
            let src = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .with_context(|| format!("open {} read-only", path.display()))?;
            let mut dst = Connection::open(&partial)?;
            let backup = Backup::new(&src, &mut dst)?;
            backup.run_to_completion(1024, Duration::from_millis(5), None)?;
        }
        std::fs::rename(&partial, dest_dir.join(name))?;
        copied.push(name.to_string());
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_capture_databases_only() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        for name in [
            "corescope-capture.sqlite3",
            "corescope-capture.backup-1.sqlite3",
            "projects.sqlite3",
        ] {
            let c = Connection::open(src.path().join(name)).unwrap();
            c.execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE t(x); INSERT INTO t VALUES (1);",
            )
            .unwrap();
        }
        let copied = snapshot_all(src.path(), dst.path()).unwrap();
        assert_eq!(copied, vec!["corescope-capture.sqlite3".to_string()]);
        let c = Connection::open(dst.path().join("corescope-capture.sqlite3")).unwrap();
        let n: i64 = c
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
