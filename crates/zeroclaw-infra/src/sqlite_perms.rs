//! Owner-only permissions for SQLite main files and WAL/SHM sidecars.
//!
//! `PRAGMA journal_mode = WAL` creates `-wal`/`-shm` under the process umask.
//! Hardening only the main file leaves the same pages group/world-readable
//! on a multi-user host with umask 022.

use std::path::{Path, PathBuf};

/// Best-effort `0600` on `db_path` and any existing `-wal`/`-shm` sidecars.
///
/// Missing sidecars are skipped: SQLite may not have created them yet.
/// Call again after the first write so a freshly created sidecar is covered.
pub fn harden_sqlite_owner_only(db_path: &Path) {
    harden_if_exists(db_path);
    harden_if_exists(&sqlite_sidecar_path(db_path, "-wal"));
    harden_if_exists(&sqlite_sidecar_path(db_path, "-shm"));
}

fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut path = db_path.as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

fn harden_if_exists(path: &Path) {
    if !path.exists() {
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn harden_resets_wal_and_shm_sidecars_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("secret.db");
        // Keep the connection open: closing the last handle checkpoints WAL
        // and can delete the sidecars, which hides the umask leak.
        let conn = Connection::open(&db_path).unwrap();
        let journal: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_ascii_lowercase(), "wal");
        conn.execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();

        std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let mut saw_sidecar = false;
        for suffix in ["-wal", "-shm"] {
            let sidecar = sqlite_sidecar_path(&db_path, suffix);
            if sidecar.exists() {
                std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o666)).unwrap();
                saw_sidecar = true;
            }
        }
        assert!(
            saw_sidecar,
            "live WAL connection must create a sidecar so this test can fail on the old chmod-main-only path"
        );

        harden_sqlite_owner_only(&db_path);

        assert_eq!(mode(&db_path), 0o600, "main db must be 0o600");
        for suffix in ["-wal", "-shm"] {
            let sidecar = sqlite_sidecar_path(&db_path, suffix);
            if sidecar.exists() {
                assert_eq!(
                    mode(&sidecar),
                    0o600,
                    "{} must be 0o600 after harden",
                    sidecar.display()
                );
            }
        }
    }
}
