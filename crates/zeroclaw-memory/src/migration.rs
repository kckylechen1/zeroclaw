use anyhow::{Context as MigContext, Result as MigResult};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

// -----------------------------------------------------------------------------
// SQLite agent_id backfill and schema versioning.
// -----------------------------------------------------------------------------

/// On-disk schema version stamped after a successful SQLite memory
/// migration. Future migrations consult this rather than re-running
/// PRAGMA detection.
pub const SQLITE_MEMORY_SCHEMA_VERSION: i64 = 1;

pub fn migrate_sqlite_memory_to_v3(db_path: &Path, conn: &Connection) -> MigResult<()> {
    if sqlite_memories_agent_id_is_not_null(conn)? && sqlite_memories_has_unique_agent_key(conn)? {
        return Ok(());
    }

    if sqlite_memories_row_count(conn)? > 0 && db_path.exists() {
        backup_sqlite_for_multi_agent_migration(db_path)?;
    }

    conn.execute_batch("BEGIN IMMEDIATE; PRAGMA defer_foreign_keys = ON;")?;
    let result = (|| -> MigResult<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agents (
                id          TEXT PRIMARY KEY,
                alias       TEXT NOT NULL UNIQUE,
                created_at  TEXT NOT NULL
             );",
        )?;
        let default_uuid = sqlite_ensure_default_agent_uuid(conn)?;

        // The legacy table may contain rows before init_schema creates its
        // FTS index. Backfilling agent_id must not fire update triggers that
        // try to delete those still-unindexed rows. Rebuild FTS below, within
        // this same transaction, after copying the migrated memories.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS memories_ai;
             DROP TRIGGER IF EXISTS memories_ad;
             DROP TRIGGER IF EXISTS memories_au;
             DROP TABLE IF EXISTS memories_fts;",
        )?;

        if !sqlite_memories_has_agent_id_column(conn)? {
            conn.execute_batch("ALTER TABLE memories ADD COLUMN agent_id TEXT;")?;
        }
        conn.execute(
            "UPDATE memories SET agent_id = ?1 WHERE agent_id IS NULL",
            params![default_uuid],
        )?;

        conn.execute_batch(
            "CREATE TABLE memories_new (
                id            TEXT PRIMARY KEY,
                key           TEXT NOT NULL,
                content       TEXT NOT NULL,
                category      TEXT NOT NULL DEFAULT 'core',
                embedding     BLOB,
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL,
                session_id    TEXT,
                namespace     TEXT DEFAULT 'default',
                importance    REAL DEFAULT 0.5,
                superseded_by TEXT,
                agent_id      TEXT NOT NULL REFERENCES agents(id),
                UNIQUE (agent_id, key)
             );

             INSERT INTO memories_new (
                id, key, content, category, embedding, created_at, updated_at,
                session_id, namespace, importance, superseded_by, agent_id
             )
             SELECT
                id, key, content, category, embedding, created_at, updated_at,
                session_id, namespace, importance, superseded_by, agent_id
             FROM memories;

             DROP TABLE memories;
             ALTER TABLE memories_new RENAME TO memories;

             CREATE INDEX IF NOT EXISTS idx_memories_category  ON memories(category);
             CREATE INDEX IF NOT EXISTS idx_memories_key       ON memories(key);
             CREATE INDEX IF NOT EXISTS idx_memories_session   ON memories(session_id);
             CREATE INDEX IF NOT EXISTS idx_memories_namespace ON memories(namespace);
             CREATE INDEX IF NOT EXISTS idx_memories_agent_id  ON memories(agent_id);

             CREATE VIRTUAL TABLE memories_fts USING fts5(
                key, content, content=memories, content_rowid=rowid
             );
             INSERT INTO memories_fts(memories_fts) VALUES('rebuild');

             CREATE TRIGGER memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
             END;
             CREATE TRIGGER memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
             END;
             CREATE TRIGGER memories_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
             END;",
        )?;

        sqlite_ensure_schema_version_table(conn)?;
        conn.execute(
            "INSERT OR REPLACE INTO schema_version (component, version, applied_at) \
             VALUES ('memories', ?1, ?2)",
            params![
                SQLITE_MEMORY_SCHEMA_VERSION,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(e)
        }
    }
}

fn sqlite_ensure_schema_version_table(conn: &Connection) -> MigResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version (
            component  TEXT PRIMARY KEY,
            version    INTEGER NOT NULL,
            applied_at TEXT NOT NULL
         );",
    )?;
    Ok(())
}

fn sqlite_memories_agent_id_is_not_null(conn: &Connection) -> MigResult<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
    let agent_id_notnull: Option<bool> = stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let notnull: i64 = row.get(3)?;
            Ok((name, notnull != 0))
        })?
        .filter_map(Result::ok)
        .find(|(name, _)| name == "agent_id")
        .map(|(_, notnull)| notnull);

    let Some(true) = agent_id_notnull else {
        return Ok(false);
    };

    let mut fk_stmt = conn.prepare("PRAGMA foreign_key_list(memories)")?;
    let has_fk = fk_stmt
        .query_map([], |row| {
            let target_table: String = row.get(2)?;
            let from_col: String = row.get(3)?;
            Ok((target_table, from_col))
        })?
        .filter_map(Result::ok)
        .any(|(target, from)| target == "agents" && from == "agent_id");
    Ok(has_fk)
}

fn sqlite_memories_has_agent_id_column(conn: &Connection) -> MigResult<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(memories)")?;
    Ok(stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == "agent_id"))
}

fn sqlite_memories_has_unique_agent_key(conn: &Connection) -> MigResult<bool> {
    // `PRAGMA index_list` returns one row per index; `PRAGMA index_info`
    // returns one row per column in that index.  We want an index that is
    // UNIQUE and whose column set is exactly {"agent_id", "key"}.
    let mut idx_stmt = conn.prepare("PRAGMA index_list(memories)")?;
    let index_names: Vec<(String, bool)> = idx_stmt
        .query_map([], |row| {
            let name: String = row.get(1)?;
            let unique: i64 = row.get(2)?;
            Ok((name, unique != 0))
        })?
        .filter_map(Result::ok)
        .collect();

    for (idx_name, is_unique) in index_names {
        if !is_unique {
            continue;
        }
        // PRAGMA index_info does not support parameter binding; format inline.
        // Index names come from sqlite_master and are controlled by SQLite
        // itself or our own migrations, so this is safe.
        let pragma = format!("PRAGMA index_info(\"{}\")", idx_name.replace('"', "\"\""));
        let mut info_stmt = conn.prepare(&pragma)?;
        let cols: Vec<String> = info_stmt
            .query_map([], |row| row.get::<_, String>(2))?
            .filter_map(Result::ok)
            .collect();
        if cols.len() == 2
            && cols.contains(&"agent_id".to_string())
            && cols.contains(&"key".to_string())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sqlite_memories_row_count(conn: &Connection) -> MigResult<i64> {
    let table_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='memories' LIMIT 1",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !table_exists {
        return Ok(0);
    }
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
    Ok(count)
}

/// Mint or query the `default` agent's row. Idempotent on concurrent
/// first-init: the returned UUID is the row that actually persisted,
/// not the candidate we attempted to insert.
pub fn sqlite_ensure_default_agent_uuid(conn: &Connection) -> MigResult<String> {
    sqlite_ensure_agent_uuid(conn, "default")
}

/// Mint-or-query a single agent row keyed by alias. Used by the
/// SQLite migration's default-agent backfill and by the `ensure_agent_uuid`
/// trait impl on the memory backend (alias resolution at agent-loop entry).
pub fn sqlite_ensure_agent_uuid(conn: &Connection, alias: &str) -> MigResult<String> {
    let new_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO agents (id, alias, created_at) VALUES (?1, ?2, ?3)",
        params![new_id, alias, now],
    )?;
    let final_id: String = conn.query_row(
        "SELECT id FROM agents WHERE alias = ?1 LIMIT 1",
        params![alias],
        |row| row.get(0),
    )?;
    Ok(final_id)
}

fn backup_sqlite_for_multi_agent_migration(db_path: &Path) -> MigResult<()> {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let backup_path = db_path.with_file_name(format!(
        "{}.backup-{timestamp}",
        db_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "brain.db".to_string()),
    ));
    std::fs::copy(db_path, &backup_path).with_context(|| {
        format!(
            "failed to copy {} to {} before multi-agent migration",
            db_path.display(),
            backup_path.display(),
        )
    })?;
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
            ::serde_json::json!({
                "backup": backup_path.display().to_string(),
            })
        ),
        "multi-agent migration: backed up SQLite memory DB before adding agents table"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocated_v2_database_is_upgraded_on_memory_backend_open() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_db = dir.path().join("workspace/memory/brain.db");
        std::fs::create_dir_all(legacy_db.parent().unwrap()).unwrap();
        {
            let conn = Connection::open(&legacy_db).unwrap();
            conn.execute_batch(
                "CREATE TABLE memories (
                    id TEXT PRIMARY KEY,
                    key TEXT NOT NULL UNIQUE,
                    content TEXT NOT NULL,
                    category TEXT NOT NULL DEFAULT 'core',
                    embedding BLOB,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    session_id TEXT,
                    namespace TEXT DEFAULT 'default',
                    importance REAL DEFAULT 0.5,
                    superseded_by TEXT
                 );
                 INSERT INTO memories (id, key, content, created_at, updated_at)
                 VALUES ('m1', 'hello', 'world', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z'),
                        ('m2', 'foo', 'bar', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');",
            )
            .unwrap();
        }

        zeroclaw_config::schema::v2::migrate_v2_to_v3_install_filesystem(dir.path()).unwrap();
        let data_dir = dir.path().join("data");
        let db_path = data_dir.join("memory/brain.db");
        assert!(db_path.is_file());
        assert!(!legacy_db.exists());

        // Exercise the production constructor, rather than calling the moved
        // migration directly: it must still upgrade the relocated database.
        let memory = crate::sqlite::SqliteMemory::new("default", &data_dir).unwrap();
        drop(memory);
        let conn = Connection::open(&db_path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 2, "both legacy memory rows must survive");
        let null_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE agent_id IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(null_count, 0, "all legacy rows must get an agent_id");
        let default_agents: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agents WHERE alias = 'default'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(default_agents, 1);
        let version: i64 = conn
            .query_row(
                "SELECT version FROM schema_version WHERE component = 'memories'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SQLITE_MEMORY_SCHEMA_VERSION);
        assert!(sqlite_memories_has_unique_agent_key(&conn).unwrap());
        let foreign_key_errors: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(foreign_key_errors, 0);
        drop(conn);

        let reopened = crate::sqlite::SqliteMemory::new("default", &data_dir).unwrap();
        drop(reopened);
        let backups = std::fs::read_dir(data_dir.join("memory"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with("brain.db.backup-"))
            .count();
        assert_eq!(backups, 1, "reopening must not repeat the migration backup");
    }

    #[test]
    fn migrate_sqlite_memory_to_v3_adds_unique_constraint_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("brain.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        // Manually build the "partially-migrated" shape: agent_id NOT NULL +
        // FK, but NO UNIQUE (agent_id, key) constraint.
        conn.execute_batch(
            "CREATE TABLE agents (
                id         TEXT PRIMARY KEY,
                alias      TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL
             );
             INSERT INTO agents VALUES ('uuid-1','default','2025-01-01T00:00:00Z');

             CREATE TABLE memories (
                id         TEXT PRIMARY KEY,
                key        TEXT NOT NULL,
                content    TEXT NOT NULL,
                category   TEXT NOT NULL DEFAULT 'core',
                embedding  BLOB,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                session_id TEXT,
                namespace  TEXT DEFAULT 'default',
                importance REAL DEFAULT 0.5,
                superseded_by TEXT,
                agent_id   TEXT NOT NULL REFERENCES agents(id)
                -- intentionally NO UNIQUE (agent_id, key)
             );
             INSERT INTO memories VALUES (
                'mid-1','test-key','test-content','core',NULL,
                '2025-01-01T00:00:00Z','2025-01-01T00:00:00Z',
                NULL,'default',0.5,NULL,'uuid-1'
             );",
        )
        .unwrap();

        // Migration must detect the missing unique constraint and re-run.
        migrate_sqlite_memory_to_v3(&db_path, &conn)
            .expect("migration must succeed on partially-migrated DB");

        // Idempotent second call must also succeed.
        migrate_sqlite_memory_to_v3(&db_path, &conn).expect("second migration run must be a no-op");

        // The unique index must now exist.
        let has_unique = sqlite_memories_has_unique_agent_key(&conn).unwrap();
        assert!(
            has_unique,
            "UNIQUE (agent_id, key) must be present after migration"
        );

        // Existing row must have survived.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE key='test-key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "existing memory row must survive the migration");
    }
}
