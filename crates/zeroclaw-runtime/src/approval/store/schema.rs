use anyhow::{Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

const GRANTS: &str = "CREATE TABLE approval_grants (
    approval_id TEXT PRIMARY KEY,
    grant_kind TEXT NOT NULL,
    boot_id TEXT, run_id TEXT, tool_name TEXT,
    args_hash TEXT NOT NULL, granted_at TEXT NOT NULL, expires_at TEXT NOT NULL,
    consumed_at TEXT, approver TEXT, channel TEXT,
    device_id TEXT, identity_epoch INTEGER, capability TEXT, nonce TEXT,
    revoked_at TEXT, claim_connection_id TEXT, claim_cap_revision TEXT, claim_call_id TEXT,
    CHECK (
      (grant_kind = 'local_tool' AND boot_id IS NOT NULL AND run_id IS NOT NULL
       AND tool_name IS NOT NULL AND approver IS NOT NULL AND channel IS NOT NULL
       AND device_id IS NULL AND identity_epoch IS NULL AND capability IS NULL
       AND nonce IS NULL AND revoked_at IS NULL AND claim_connection_id IS NULL
       AND claim_cap_revision IS NULL AND claim_call_id IS NULL)
      OR
      (grant_kind IN ('node_capability', 'tachi_projected')
       AND approval_id IS NOT NULL AND length(approval_id) > 0
       AND boot_id IS NULL AND run_id IS NULL AND tool_name IS NULL
       AND approver IS NULL AND channel IS NULL
       AND device_id IS NOT NULL AND length(device_id) > 0
       AND identity_epoch IS NOT NULL AND typeof(identity_epoch) = 'integer' AND identity_epoch >= 0
       AND capability IS NOT NULL AND length(capability) > 0
       AND length(args_hash) = 64 AND args_hash NOT GLOB '*[^0-9a-f]*'
       AND nonce IS NOT NULL AND length(nonce) > 0
       AND length(granted_at) > 0 AND length(expires_at) > 0
       AND (revoked_at IS NULL OR length(revoked_at) > 0)
       AND ((consumed_at IS NULL AND claim_connection_id IS NULL
             AND claim_cap_revision IS NULL AND claim_call_id IS NULL)
         OR (consumed_at IS NOT NULL AND length(consumed_at) > 0
             AND claim_connection_id IS NOT NULL AND length(claim_connection_id) > 0
             AND claim_call_id IS NOT NULL AND length(claim_call_id) > 0
             AND claim_cap_revision IS NOT NULL AND typeof(claim_cap_revision) = 'text'
             AND length(claim_cap_revision) BETWEEN 1 AND 20
             AND claim_cap_revision NOT GLOB '*[^0-9]*'
             AND (claim_cap_revision = '0' OR substr(claim_cap_revision, 1, 1) BETWEEN '1' AND '9')
             AND (length(claim_cap_revision) < 20 OR claim_cap_revision <= '18446744073709551615'))))
    )
)";
const LEGACY_GRANTS: &str = "CREATE TABLE approval_grants (
    approval_id TEXT PRIMARY KEY, boot_id TEXT NOT NULL, run_id TEXT NOT NULL,
    tool_name TEXT NOT NULL, args_hash TEXT NOT NULL, granted_at TEXT NOT NULL,
    expires_at TEXT NOT NULL, consumed_at TEXT, approver TEXT NOT NULL, channel TEXT NOT NULL
)";
const AUDIT: &str = "CREATE TABLE approval_audit (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, ts TEXT NOT NULL, boot_id TEXT NOT NULL,
    run_id TEXT, agent TEXT, tool_name TEXT NOT NULL, args_hash TEXT NOT NULL,
    args_summary TEXT NOT NULL, decision TEXT NOT NULL, approver TEXT, channel TEXT
)";
const INDEXES: [(&str, &str); 3] = [
    (
        "idx_grants_lookup",
        "CREATE INDEX idx_grants_lookup ON approval_grants(boot_id, run_id, tool_name, args_hash)",
    ),
    (
        "idx_audit_ts",
        "CREATE INDEX idx_audit_ts ON approval_audit(ts)",
    ),
    (
        "idx_audit_run",
        "CREATE INDEX idx_audit_run ON approval_audit(run_id)",
    ),
];

// SQLite removes IF NOT EXISTS and quotes a table name after ALTER RENAME.
// Only these schema-owned statements are compared; no caller SQL is executed.
fn normalized(sql: &str) -> String {
    let sql = sql
        .replace("IF NOT EXISTS", "")
        .replace("if not exists", "");
    let mut literal = false;
    let mut token = String::new();
    let mut normalized = String::new();
    for ch in sql.chars() {
        if literal {
            normalized.push(ch);
            if ch == '\'' {
                literal = false;
                normalized.push('|');
            }
        } else if ch == '"' {
            // Only schema-owned identifier quoting differs after ALTER RENAME.
            continue;
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            token.push(ch.to_ascii_lowercase());
        } else {
            if !token.is_empty() {
                normalized.push_str(&token);
                normalized.push('|');
                token.clear();
            }
            if ch == '\'' {
                literal = true;
                normalized.push(ch);
            } else if !ch.is_ascii_whitespace() && ch != ';' {
                normalized.push(ch);
            }
        }
    }
    if !token.is_empty() {
        normalized.push_str(&token);
        normalized.push('|');
    }
    normalized
}

fn version(conn: &Connection) -> Result<i64> {
    let version = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    ensure!(
        matches!(version, 0 | 1),
        "unsupported approval store schema version"
    );
    Ok(version)
}

fn eligible(conn: &Connection, version: i64) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    if version == 0 && count == 0 {
        return Ok(true);
    }
    ensure!(count == 5, "unexpected approval store schema objects");
    let mut expected = vec![
        (
            "approval_grants",
            if version == 0 { LEGACY_GRANTS } else { GRANTS },
        ),
        ("approval_audit", AUDIT),
    ];
    expected.extend(INDEXES);
    for (name, sql) in expected {
        let actual: Option<String> = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            actual.is_some_and(|s| normalized(&s) == normalized(sql)),
            "unexpected approval store schema shape: {name}"
        );
    }
    Ok(false)
}

pub(super) fn initialize(conn: &mut Connection, persistent: bool) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    // Unknown versions/shapes are refused before any requested journal or schema mutation.
    let current = version(conn)?;
    eligible(conn, current)?;
    if persistent {
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        ensure!(
            mode.eq_ignore_ascii_case("wal"),
            "approval store requires WAL journal mode"
        );
    }
    conn.pragma_update(None, "synchronous", "FULL")?;
    let synchronous: i64 = conn.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
    ensure!(
        synchronous == 2,
        "approval store requires FULL synchronization"
    );
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Another opener may have migrated while this connection waited for the writer lock.
    let current = version(&tx)?;
    let empty = eligible(&tx, current)?;
    if current == 1 {
        tx.commit()?;
        return Ok(());
    }
    // Preserve the validated legacy index statement, including its stored SQL.
    // Dropping the old grant table also drops this index inside the transaction.
    let legacy_lookup_sql: Option<String> = if empty {
        None
    } else {
        Some(tx.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_grants_lookup'",
            [],
            |row| row.get(0),
        )?)
    };
    if empty {
        tx.execute_batch(GRANTS)?;
        tx.execute_batch(AUDIT)?;
    } else {
        tx.execute_batch(&GRANTS.replacen("approval_grants", "approval_grants_v1", 1))?;
        let copied = tx.execute(
            "INSERT INTO approval_grants_v1
            (approval_id, grant_kind, boot_id, run_id, tool_name, args_hash, granted_at,
             expires_at, consumed_at, approver, channel)
            SELECT approval_id, 'local_tool', boot_id, run_id, tool_name, args_hash, granted_at,
                   expires_at, consumed_at, approver, channel FROM approval_grants",
            [],
        )?;
        let original: i64 =
            tx.query_row("SELECT count(*) FROM approval_grants", [], |r| r.get(0))?;
        let migrated: i64 =
            tx.query_row("SELECT count(*) FROM approval_grants_v1", [], |r| r.get(0))?;
        if i64::try_from(copied)? != original || migrated != original {
            bail!("approval migration row count mismatch");
        }
        tx.execute_batch(
            "DROP TABLE approval_grants; ALTER TABLE approval_grants_v1 RENAME TO approval_grants;",
        )?;
    }
    for (name, sql) in INDEXES {
        if name == "idx_grants_lookup"
            && let Some(original) = legacy_lookup_sql.as_deref()
        {
            tx.execute_batch(original)?;
            continue;
        }
        tx.execute_batch(&sql.replacen("CREATE INDEX", "CREATE INDEX IF NOT EXISTS", 1))?;
    }
    tx.pragma_update(None, "user_version", 1)?;
    eligible(&tx, 1)?;
    tx.commit()?;
    Ok(())
}
