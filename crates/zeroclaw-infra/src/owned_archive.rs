//! Verified, private archives for the existing agent-owned stores.
//!
//! Store owners keep their write transaction open while publishing the archive,
//! then delete exactly that snapshot. This module owns no data or transaction.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, types::ValueRef};
use serde_json::{Map, Value};

/// Preserve every selected SQLite column, including raw timestamps, JSON and
/// binary embeddings. BLOBs use an explicit tagged byte array for restoration.
/// The SQL is supplied by the store owner, never by a request or model.
pub fn sqlite_rows(conn: &Connection, sql: &str, agent_alias: &str) -> Result<Vec<Value>> {
    let mut statement = conn.prepare(sql)?;
    let names: Vec<String> = statement.column_names().map(String::from).collect();
    let mut rows = statement.query([agent_alias])?;
    let mut exported = Vec::new();
    while let Some(row) = rows.next()? {
        let mut record = Map::new();
        for (index, name) in names.iter().enumerate() {
            let value = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(value) => Value::from(value),
                ValueRef::Real(value) => Value::Number(
                    serde_json::Number::from_f64(value)
                        .context("cannot archive non-finite SQLite number")?,
                ),
                ValueRef::Text(value) => Value::from(std::str::from_utf8(value)?),
                ValueRef::Blob(value) => serde_json::json!({"sqlite_blob": value}),
            };
            record.insert(name.clone(), value);
        }
        exported.push(Value::Object(record));
    }
    Ok(exported)
}

/// Create the private archive directory without following a final symlink.
pub fn create_private_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .context("create owned-state archive directory")?;
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(metadata.is_dir(), "archive destination is not a directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn verify_bytes(path: &Path, expected: &[u8]) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.is_file(),
        "archive destination is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "archive is not owner-only"
        );
    }
    let mut actual = Vec::new();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    // An identical pre-existing file can be a retry's authoritative archive.
    // Sync that actual file too, not just the discarded new temporary.
    file.sync_all()
        .context("sync published owned-state archive")?;
    file.take(u64::try_from(expected.len())?.saturating_add(1))
        .read_to_end(&mut actual)?;
    anyhow::ensure!(actual == expected, "archive integrity verification failed");
    Ok(())
}

/// Atomically publish a complete archive, sync it and verify its final bytes.
/// Existing archives are never overwritten: an identical snapshot is a safe
/// retry; a different, truncated, symlinked or unreadable file blocks deletion.
pub fn write_verified_json(path: &Path, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).context("serialize owned-state archive")?;
    let parent = path.parent().context("archive path has no parent")?;
    create_private_dir(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary
        .write_all(&bytes)
        .context("write complete owned-state archive")?;
    temporary
        .as_file()
        .sync_all()
        .context("sync owned-state archive")?;
    // persist_noclobber prevents a retry or a same-second deletion from
    // destroying the only surviving copy of an earlier snapshot.
    if let Err(error) = temporary.persist_noclobber(path)
        && error.error.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(error.error).context("publish owned-state archive");
    }
    #[cfg(unix)]
    {
        // The caller may have just created several archive ancestors. Syncing
        // only `cascade/` does not make its own entry in the parent durable.
        // Walk bottom-up through the existing path before the store may commit
        // its DELETE, including directories created before this helper ran.
        let absolute_parent = std::fs::canonicalize(parent)?;
        for directory in absolute_parent.ancestors() {
            std::fs::File::open(directory)?
                .sync_all()
                .context("sync archive directory ancestry")?;
        }
    }
    verify_bytes(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_retries_verify_without_overwriting() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cascade/state.json");
        let snapshot = serde_json::json!({"body": "preserved 中文", "null": null});
        write_verified_json(&path, &snapshot).unwrap();
        write_verified_json(&path, &snapshot).unwrap();
        assert!(write_verified_json(&path, &serde_json::json!({"new": true})).is_err());
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            snapshot
        );
        std::fs::write(&path, b"{truncated").unwrap();
        assert!(write_verified_json(&path, &snapshot).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{truncated");
    }

    #[test]
    fn archive_write_failure_is_returned() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("not-a-directory");
        std::fs::write(&blocker, b"preserve").unwrap();
        assert!(write_verified_json(&blocker.join("memory.json"), &Value::Null).is_err());
        assert_eq!(std::fs::read(blocker).unwrap(), b"preserve");
    }

    #[cfg(unix)]
    #[test]
    fn archives_are_private_and_refuse_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("archive/state.json");
        write_verified_json(&path, &Value::Null).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let link = directory.path().join("archive/link.json");
        symlink(&path, &link).unwrap();
        assert!(write_verified_json(&link, &Value::Null).is_err());
    }

    #[test]
    fn sqlite_archive_preserves_raw_columns_and_blobs() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE sample (agent TEXT, body TEXT, data BLOB, nullable TEXT, version INTEGER);
            INSERT INTO sample VALUES ('alpha', 'raw body', X'0001FF', NULL, 7);").unwrap();
        let rows = sqlite_rows(
            &connection,
            "SELECT * FROM sample WHERE agent = ?1",
            "alpha",
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![
                serde_json::json!({"agent": "alpha", "body": "raw body", "data": {"sqlite_blob": [0,1,255]}, "nullable": null, "version": 7})
            ]
        );
    }
}
