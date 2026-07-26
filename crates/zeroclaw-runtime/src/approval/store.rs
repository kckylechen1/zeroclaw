//! Durable one-shot approvals and an audit trail that survives a restart.
//!
//! The in-memory [`ApprovalManager`](super::ApprovalManager) answers "does this
//! call need a prompt". It cannot answer the two questions an unattended agent
//! raises once it can move money:
//!
//! - **Was this specific call approved?** A session allow-list keyed on a tool
//!   *name* grants every future call to that tool, with any arguments, for as
//!   long as the process lives. Approving one order approves the next thousand.
//! - **Who approved what, a month ago?** The audit log is a `Vec` in memory.
//!   A restart erases the evidence.
//!
//! This store closes both. A grant is bound to one boot, one run, one tool, and
//! one argument hash, and is consumed by its first use. Everything that reaches
//! the gate — granted, denied, timed out, auto-approved, blocked — is appended
//! to a durable trail whether or not a human was involved.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
CREATE TABLE IF NOT EXISTS approval_grants (
    approval_id TEXT PRIMARY KEY,
    boot_id     TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    tool_name   TEXT NOT NULL,
    args_hash   TEXT NOT NULL,
    granted_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    consumed_at TEXT,
    approver    TEXT NOT NULL,
    channel     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_grants_lookup
    ON approval_grants(boot_id, run_id, tool_name, args_hash);
CREATE TABLE IF NOT EXISTS approval_audit (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           TEXT NOT NULL,
    boot_id      TEXT NOT NULL,
    run_id       TEXT,
    agent        TEXT,
    tool_name    TEXT NOT NULL,
    args_hash    TEXT NOT NULL,
    args_summary TEXT NOT NULL,
    decision     TEXT NOT NULL,
    approver     TEXT,
    channel      TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON approval_audit(ts);
CREATE INDEX IF NOT EXISTS idx_audit_run ON approval_audit(run_id);
";

/// Default grant lifetime. Short on purpose: an approval is permission to do
/// one thing now, not standing authority.
pub const DEFAULT_GRANT_TTL_SECS: i64 = 300;

/// Canonical hash of a tool call's arguments.
///
/// Serialised through `serde_json::Value`'s ordered map so that two logically
/// equal argument sets hash the same regardless of key order in the wire form.
#[must_use]
pub fn args_hash(args: &serde_json::Value) -> String {
    let canonical = canonicalize(args);
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn canonicalize(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            // BTreeMap ordering: serde_json's Map iterates in insertion order
            // unless the preserve_order feature is off, so sort explicitly
            // rather than trusting the map's own iteration.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonicalize(&map[k])))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(canonicalize).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

/// What happened at the gate. Recorded whether or not a human was involved —
/// "nobody was asked" is itself evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditDecision {
    /// A human approved this specific call.
    Granted,
    /// A human refused.
    Denied,
    /// Nobody answered before the deadline; treated as refusal.
    TimedOut,
    /// `auto_approve` let it through with no human in the loop.
    AutoApproved,
    /// The profile's tool gate rejected it before any prompt.
    Blocked,
    /// The call ran without needing approval under this profile.
    NotRequired,
}

impl AuditDecision {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::TimedOut => "timed_out",
            Self::AutoApproved => "auto_approved",
            Self::Blocked => "blocked",
            Self::NotRequired => "not_required",
        }
    }
}

/// One durable grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub approval_id: String,
    pub boot_id: String,
    pub run_id: String,
    pub tool_name: String,
    pub args_hash: String,
    pub granted_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub consumed_at: Option<DateTime<Utc>>,
    pub approver: String,
    pub channel: String,
}

/// Why a redemption attempt failed. The distinction matters for the audit
/// trail: an expired grant and a replayed grant are different incidents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemFailure {
    /// No grant matches this boot, run, tool, and argument hash.
    NoGrant,
    /// A grant exists but its window has closed.
    Expired,
    /// A grant exists and was already used. One-shot means one shot.
    AlreadyConsumed,
}

pub struct ApprovalStore {
    conn: Arc<Mutex<Connection>>,
    boot_id: String,
}

impl ApprovalStore {
    /// Open (creating if absent) the store under `data_dir`, scoped to
    /// `boot_id`. Grants written by an earlier boot stay in the table as
    /// evidence but can never be redeemed again.
    pub fn open(data_dir: &Path, boot_id: impl Into<String>) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating approval store dir {}", data_dir.display()))?;
        let conn = Connection::open(data_dir.join("approvals.db"))
            .context("opening approvals.db")?;
        conn.execute_batch(SCHEMA)
            .context("applying approval store schema")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            boot_id: boot_id.into(),
        })
    }

    #[cfg(test)]
    fn open_in_memory(boot_id: impl Into<String>) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            boot_id: boot_id.into(),
        })
    }

    #[must_use]
    pub fn boot_id(&self) -> &str {
        &self.boot_id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a human approval for exactly one call.
    pub fn grant(
        &self,
        run_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        approver: &str,
        channel: &str,
        ttl: Duration,
    ) -> Result<Grant> {
        let now = Utc::now();
        let grant = Grant {
            approval_id: uuid::Uuid::new_v4().to_string(),
            boot_id: self.boot_id.clone(),
            run_id: run_id.to_string(),
            tool_name: tool_name.to_string(),
            args_hash: args_hash(args),
            granted_at: now,
            expires_at: now + ttl,
            consumed_at: None,
            approver: approver.to_string(),
            channel: channel.to_string(),
        };

        self.lock().execute(
            "INSERT INTO approval_grants
                 (approval_id, boot_id, run_id, tool_name, args_hash,
                  granted_at, expires_at, consumed_at, approver, channel)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9)",
            params![
                grant.approval_id,
                grant.boot_id,
                grant.run_id,
                grant.tool_name,
                grant.args_hash,
                grant.granted_at.to_rfc3339(),
                grant.expires_at.to_rfc3339(),
                grant.approver,
                grant.channel,
            ],
        )?;
        Ok(grant)
    }

    /// Consume a grant for this exact call, or explain why not.
    ///
    /// The consume is a single conditional `UPDATE`: the row is claimed only if
    /// it is still unconsumed at write time, so two racing calls cannot both
    /// redeem one grant. Diagnosis of a failure is a separate read and is
    /// advisory only — by then the answer is already "no".
    pub fn redeem(
        &self,
        run_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<std::result::Result<String, RedeemFailure>> {
        let hash = args_hash(args);
        let now = Utc::now();
        let conn = self.lock();

        let approval_id: Option<String> = conn
            .query_row(
                "UPDATE approval_grants
                    SET consumed_at = ?1
                  WHERE approval_id = (
                        SELECT approval_id FROM approval_grants
                         WHERE boot_id = ?2 AND run_id = ?3
                           AND tool_name = ?4 AND args_hash = ?5
                           AND consumed_at IS NULL
                           AND expires_at > ?1
                         ORDER BY granted_at
                         LIMIT 1)
              RETURNING approval_id",
                params![now.to_rfc3339(), self.boot_id, run_id, tool_name, hash],
                |row| row.get(0),
            )
            .optional()?;

        if let Some(id) = approval_id {
            return Ok(Ok(id));
        }

        // Nothing claimable. Say which kind of "no" this is.
        let consumed: Option<String> = conn
            .query_row(
                "SELECT consumed_at FROM approval_grants
                  WHERE boot_id = ?1 AND run_id = ?2
                    AND tool_name = ?3 AND args_hash = ?4
                    AND consumed_at IS NOT NULL
                  ORDER BY consumed_at DESC LIMIT 1",
                params![self.boot_id, run_id, tool_name, hash],
                |row| row.get(0),
            )
            .optional()?;
        if consumed.is_some() {
            return Ok(Err(RedeemFailure::AlreadyConsumed));
        }

        let expired: Option<String> = conn
            .query_row(
                "SELECT approval_id FROM approval_grants
                  WHERE boot_id = ?1 AND run_id = ?2
                    AND tool_name = ?3 AND args_hash = ?4
                    AND expires_at <= ?5
                  LIMIT 1",
                params![self.boot_id, run_id, tool_name, hash, now.to_rfc3339()],
                |row| row.get(0),
            )
            .optional()?;
        if expired.is_some() {
            return Ok(Err(RedeemFailure::Expired));
        }

        Ok(Err(RedeemFailure::NoGrant))
    }

    /// Append to the durable trail. Every gate outcome goes here, including
    /// the ones nobody was asked about.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        run_id: Option<&str>,
        agent: Option<&str>,
        tool_name: &str,
        args: &serde_json::Value,
        args_summary: &str,
        decision: AuditDecision,
        approver: Option<&str>,
        channel: Option<&str>,
    ) -> Result<()> {
        self.lock().execute(
            "INSERT INTO approval_audit
                 (ts, boot_id, run_id, agent, tool_name, args_hash,
                  args_summary, decision, approver, channel)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                Utc::now().to_rfc3339(),
                self.boot_id,
                run_id,
                agent,
                tool_name,
                args_hash(args),
                args_summary,
                decision.as_str(),
                approver,
                channel,
            ],
        )?;
        Ok(())
    }

    /// Every audit row for a run, oldest first. Reads across boots on purpose:
    /// the point of a durable trail is that a restart does not hide anything.
    pub fn audit_for_run(&self, run_id: &str) -> Result<Vec<(String, String, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT ts, tool_name, decision FROM approval_audit
              WHERE run_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![run_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    #[cfg(test)]
    fn audit_len(&self) -> usize {
        self.lock()
            .query_row("SELECT COUNT(*) FROM approval_audit", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or(0) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ttl() -> Duration {
        Duration::seconds(DEFAULT_GRANT_TTL_SECS)
    }

    #[test]
    fn a_grant_is_redeemable_exactly_once() {
        let store = ApprovalStore::open_in_memory("boot-1").unwrap();
        let args = json!({"symbol": "600519", "qty": 100});
        store
            .grant("run-1", "portfolio_buy", &args, "owner", "wechat", ttl())
            .unwrap();

        assert!(
            store.redeem("run-1", "portfolio_buy", &args).unwrap().is_ok(),
            "first redemption must succeed"
        );
        assert_eq!(
            store.redeem("run-1", "portfolio_buy", &args).unwrap(),
            Err(RedeemFailure::AlreadyConsumed),
            "a second call must not ride the same approval"
        );
    }

    #[test]
    fn a_grant_does_not_cover_different_arguments() {
        let store = ApprovalStore::open_in_memory("boot-1").unwrap();
        let approved = json!({"symbol": "600519", "qty": 100});
        store
            .grant("run-1", "portfolio_buy", &approved, "owner", "wechat", ttl())
            .unwrap();

        let bigger = json!({"symbol": "600519", "qty": 100_000});
        assert_eq!(
            store.redeem("run-1", "portfolio_buy", &bigger).unwrap(),
            Err(RedeemFailure::NoGrant),
            "approving 100 shares must not approve 100000"
        );
    }

    #[test]
    fn a_grant_does_not_cover_a_different_run() {
        let store = ApprovalStore::open_in_memory("boot-1").unwrap();
        let args = json!({"symbol": "600519"});
        store
            .grant("run-1", "portfolio_buy", &args, "owner", "wechat", ttl())
            .unwrap();

        assert_eq!(
            store.redeem("run-2", "portfolio_buy", &args).unwrap(),
            Err(RedeemFailure::NoGrant)
        );
    }

    #[test]
    fn a_grant_does_not_cover_a_different_tool() {
        let store = ApprovalStore::open_in_memory("boot-1").unwrap();
        let args = json!({"symbol": "600519"});
        store
            .grant("run-1", "portfolio_buy", &args, "owner", "wechat", ttl())
            .unwrap();

        assert_eq!(
            store.redeem("run-1", "portfolio_sell", &args).unwrap(),
            Err(RedeemFailure::NoGrant)
        );
    }

    #[test]
    fn an_expired_grant_is_refused_and_named_as_expired() {
        let store = ApprovalStore::open_in_memory("boot-1").unwrap();
        let args = json!({"symbol": "600519"});
        store
            .grant(
                "run-1",
                "portfolio_buy",
                &args,
                "owner",
                "wechat",
                Duration::seconds(-1),
            )
            .unwrap();

        assert_eq!(
            store.redeem("run-1", "portfolio_buy", &args).unwrap(),
            Err(RedeemFailure::Expired)
        );
    }

    /// The restart-epoch rule. A grant issued before the process died must not
    /// still be standing when it comes back.
    #[test]
    fn a_grant_from_a_previous_boot_cannot_be_redeemed() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"symbol": "600519"});

        let before = ApprovalStore::open(dir.path(), "boot-1").unwrap();
        before
            .grant("run-1", "portfolio_buy", &args, "owner", "wechat", ttl())
            .unwrap();
        drop(before);

        let after = ApprovalStore::open(dir.path(), "boot-2").unwrap();
        assert_eq!(
            after.redeem("run-1", "portfolio_buy", &args).unwrap(),
            Err(RedeemFailure::NoGrant),
            "a restart must invalidate outstanding approvals"
        );
    }

    #[test]
    fn argument_hashing_ignores_key_order_but_not_values() {
        let a = json!({"symbol": "600519", "qty": 100});
        let b = json!({"qty": 100, "symbol": "600519"});
        let c = json!({"qty": 101, "symbol": "600519"});

        assert_eq!(args_hash(&a), args_hash(&b), "key order must not matter");
        assert_ne!(args_hash(&a), args_hash(&c), "values must matter");
    }

    /// The audit trail is the half that has to survive when the grant does not.
    #[test]
    fn the_audit_trail_outlives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"symbol": "600519"});

        let before = ApprovalStore::open(dir.path(), "boot-1").unwrap();
        before
            .record(
                Some("run-1"),
                Some("trader"),
                "portfolio_buy",
                &args,
                "symbol=600519",
                AuditDecision::Granted,
                Some("owner"),
                Some("wechat"),
            )
            .unwrap();
        drop(before);

        let after = ApprovalStore::open(dir.path(), "boot-2").unwrap();
        let rows = after.audit_for_run("run-1").unwrap();
        assert_eq!(rows.len(), 1, "the record must survive the restart");
        assert_eq!(rows[0].1, "portfolio_buy");
        assert_eq!(rows[0].2, "granted");
    }

    /// Denials and unattended auto-approvals are exactly the rows an
    /// after-the-fact investigation needs, so they must be written too.
    #[test]
    fn decisions_with_no_human_are_recorded_too() {
        let store = ApprovalStore::open_in_memory("boot-1").unwrap();
        let args = json!({});

        for decision in [
            AuditDecision::Denied,
            AuditDecision::TimedOut,
            AuditDecision::AutoApproved,
            AuditDecision::Blocked,
            AuditDecision::NotRequired,
        ] {
            store
                .record(
                    Some("run-1"),
                    Some("trader"),
                    "shell",
                    &args,
                    "-",
                    decision,
                    None,
                    None,
                )
                .unwrap();
        }

        assert_eq!(store.audit_len(), 5);
        let rows = store.audit_for_run("run-1").unwrap();
        let decisions: Vec<&str> = rows.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(
            decisions,
            vec![
                "denied",
                "timed_out",
                "auto_approved",
                "blocked",
                "not_required"
            ]
        );
    }
}
