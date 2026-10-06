//! Durable outbox for proactive messages to channel bridges.
//!
//! Cron results, heartbeat alerts and the `notify` tool write here when
//! their target names a `[gateway.bridges.<name>]` entry. The gateway's
//! `/ws/bridge` control socket drains it: it sends each row as a `deliver`
//! frame and retains a receipt when the bridge answers `delivered`.
//!
//! Delivery attempts are claimed durably before socket I/O. Uncertain sends
//! are held for owner reconciliation, never blindly replayed. A platform receipt
//! marks a row confirmed; confirmed does not mean the owner read the message.
//! Pending rows expire after 24 hours; uncertain rows never expire automatically.
//! Terminal tombstones retain source deduplication for up to 30 days or the
//! latest 10,000 terminal rows per bridge, whichever is shorter. Capacity rejects
//! new rows instead of evicting active, deferred, or uncertain notifications.
//!
//! The store is a separate SQLite file next to the session database
//! (`<data_dir>/sessions/bridge_outbox.db`). The gateway owns it; writers in
//! the same process share one handle through [`BridgeOutbox::shared`] so a
//! write wakes the connected control socket at once. Writers in other
//! processes are picked up by the socket's periodic poll.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::watch;

/// Active rows per bridge; new writes are refused at capacity.
pub const MAX_ROWS_PER_BRIDGE: usize = 1000;
/// Accepted rows expire after this duration; unknown attempts never expire.
pub const ROW_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const DB_FILE: &str = "bridge_outbox.db";

/// One queued message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OutboxItem {
    /// Insertion order; replay and delivery follow it.
    pub seq: i64,
    /// Stable id the bridge acknowledges and dedupes on.
    pub id: String,
    pub bridge: String,
    /// Platform recipient, as the bridge understands it (a chat id).
    pub to: String,
    pub thread_id: Option<String>,
    pub content: String,
    pub source_kind: String,
    pub source_id: String,
    pub event_id: String,
    pub delivery_state: String,
    pub snoozed_until: Option<i64>,
    pub expires_at: i64,
}

/// The outbox table and a change signal for connected control sockets.
pub struct BridgeOutbox {
    conn: Mutex<Connection>,
    db_path: PathBuf,
    changed: watch::Sender<u64>,
    max_rows: usize,
    ttl: Duration,
}

impl BridgeOutbox {
    /// Open (or create) the outbox under `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let dir = data_dir.join("sessions");
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let db_path = dir.join(DB_FILE);
        let mut conn = Connection::open(&db_path)
            .with_context(|| format!("opening bridge outbox {}", db_path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS bridge_outbox (
                seq        INTEGER PRIMARY KEY AUTOINCREMENT,
                id         TEXT NOT NULL UNIQUE,
                bridge     TEXT NOT NULL,
                recipient  TEXT NOT NULL,
                thread_id  TEXT,
                content    TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_bridge_outbox_bridge_seq
                ON bridge_outbox(bridge, seq);",
        )
        .context("initializing the bridge outbox schema")?;
        // Additive migration preserves queued rows from the original outbox.
        let migration = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let columns = {
            let mut statement = migration.prepare("PRAGMA table_info(bridge_outbox)")?;
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (name, definition) in [
            ("source_kind", "TEXT NOT NULL DEFAULT 'legacy'"),
            ("source_id", "TEXT NOT NULL DEFAULT ''"),
            ("event_id", "TEXT NOT NULL DEFAULT ''"),
            ("delivery_state", "TEXT NOT NULL DEFAULT 'accepted'"),
            ("snoozed_until", "INTEGER"),
            ("expires_at", "INTEGER NOT NULL DEFAULT 0"),
            ("resolved_at", "INTEGER"),
            ("owner_action_by", "TEXT"),
        ] {
            if !columns.iter().any(|column| column == name) {
                migration.execute_batch(&format!(
                    "ALTER TABLE bridge_outbox ADD COLUMN {name} {definition}"
                ))?;
            }
        }
        migration.execute_batch(
            "UPDATE bridge_outbox SET source_id = id, event_id = id WHERE event_id = '';
             UPDATE bridge_outbox SET expires_at = created_at + 86400 WHERE expires_at = 0;
             CREATE UNIQUE INDEX IF NOT EXISTS idx_bridge_outbox_source
             ON bridge_outbox(bridge, recipient, source_kind, source_id, event_id);
             CREATE TABLE IF NOT EXISTS bridge_attention_mutes (
                bridge TEXT NOT NULL, recipient TEXT NOT NULL,
                source_kind TEXT NOT NULL, source_id TEXT NOT NULL,
                owner_action_by TEXT NOT NULL, candidate_id TEXT NOT NULL,
                PRIMARY KEY (bridge, recipient, source_kind, source_id)
             );",
        )?;
        migration.commit()?;
        crate::sqlite_perms::harden_sqlite_owner_only(&db_path);
        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
            changed: watch::channel(0).0,
            max_rows: MAX_ROWS_PER_BRIDGE,
            ttl: ROW_TTL,
        })
    }

    /// The process-wide handle for `data_dir`, opened on first use. Every
    /// writer and the gateway in one process share it, so a write wakes the
    /// control socket without waiting for its poll.
    pub fn shared(data_dir: &Path) -> Result<Arc<Self>> {
        static HANDLES: OnceLock<Mutex<HashMap<PathBuf, Arc<BridgeOutbox>>>> = OnceLock::new();
        let mut handles = HANDLES.get_or_init(Default::default).lock();
        if let Some(handle) = handles.get(data_dir) {
            return Ok(Arc::clone(handle));
        }
        let handle = Arc::new(Self::open(data_dir)?);
        handles.insert(data_dir.to_path_buf(), Arc::clone(&handle));
        Ok(handle)
    }

    /// Queue `content` for `bridge` to deliver to `to`. Returns the row id.
    pub fn enqueue(
        &self,
        bridge: &str,
        to: &str,
        thread_id: Option<&str>,
        content: &str,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.enqueue_source(bridge, to, thread_id, content, "notice", &id, &id)
    }

    /// Queue one canonical source event. Repeated observations return the same id
    /// through the bounded terminal retention window; execution remains source-owned.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_source(
        &self,
        bridge: &str,
        to: &str,
        thread_id: Option<&str>,
        content: &str,
        source_kind: &str,
        source_id: &str,
        event_id: &str,
    ) -> Result<String> {
        anyhow::ensure!(
            !source_kind.is_empty() && !source_id.is_empty() && !event_id.is_empty(),
            "attention_source_required"
        );
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        {
            let mut conn = self.conn.lock();
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            self.purge_expired_locked(&tx, now)?;
            let existing: Option<String> = tx.query_row(
                "SELECT id FROM bridge_outbox WHERE bridge=?1 AND recipient=?2 AND source_kind=?3 AND source_id=?4 AND event_id=?5",
                params![bridge, to, source_kind, source_id, event_id], |row| row.get(0),
            ).optional()?;
            if let Some(existing) = existing {
                return Ok(existing);
            }
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM bridge_outbox WHERE bridge=?1 AND delivery_state IN ('accepted','sent','unknown')", [bridge], |row| row.get(0))?;
            anyhow::ensure!(
                count < self.max_rows as i64,
                "bridge_outbox_capacity_reached"
            );
            tx.execute(
                "INSERT INTO bridge_outbox (id, bridge, recipient, thread_id, content, created_at, source_kind, source_id, event_id, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![id, bridge, to, thread_id, content, now, source_kind, source_id, event_id, now + self.ttl.as_secs() as i64],
            ).context("writing to the bridge outbox")?;
            tx.commit()?;
        }
        crate::sqlite_perms::harden_sqlite_owner_only(&self.db_path);
        self.wake();
        Ok(id)
    }

    fn wake(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    /// Rows for `bridge` after `after_seq`, oldest first, at most `limit`.
    pub fn pending(&self, bridge: &str, after_seq: i64, limit: usize) -> Result<Vec<OutboxItem>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT seq, id, bridge, recipient, thread_id, content, source_kind, source_id, event_id, delivery_state, snoozed_until, expires_at FROM bridge_outbox
             WHERE bridge = ?1 AND seq > ?2 AND delivery_state = 'accepted' ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![bridge, after_seq, limit as i64], |row| {
                Ok(OutboxItem {
                    seq: row.get(0)?,
                    id: row.get(1)?,
                    bridge: row.get(2)?,
                    to: row.get(3)?,
                    thread_id: row.get(4)?,
                    content: row.get(5)?,
                    source_kind: row.get(6)?,
                    source_id: row.get(7)?,
                    event_id: row.get(8)?,
                    delivery_state: row.get(9)?,
                    snoozed_until: row.get(10)?,
                    expires_at: row.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading the bridge outbox")?;
        Ok(rows)
    }

    /// Retain a confirmed receipt for an attempted row. Duplicate receipts
    /// are harmless; unattempted candidates cannot be acknowledged.
    pub fn ack(&self, bridge: &str, id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let deleted = conn
            .execute(
                "UPDATE bridge_outbox SET delivery_state = 'confirmed', resolved_at = ?3, content = '' WHERE bridge = ?1 AND id = ?2 AND delivery_state IN ('sent','unknown')",
                params![bridge, id, now_secs()],
            )
            .context("acknowledging a bridge outbox row")?;
        Ok(deleted > 0)
    }

    /// Purge rows past the TTL. Returns how many were dropped.
    pub fn purge_expired(&self) -> Result<usize> {
        let conn = self.conn.lock();
        self.purge_expired_locked(&conn, now_secs())
    }

    /// Queued rows for `bridge`.
    pub fn len(&self, bridge: &str) -> Result<usize> {
        let conn = self.conn.lock();
        let count: Option<i64> = conn
            .query_row(
                "SELECT COUNT(*) FROM bridge_outbox WHERE bridge = ?1 AND delivery_state IN ('accepted','sent','unknown')",
                params![bridge],
                |row| row.get(0),
            )
            .optional()?;
        Ok(count.unwrap_or(0) as usize)
    }

    /// A receiver that changes whenever a row is written in this process.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn purge_expired_locked(&self, conn: &Connection, now: i64) -> Result<usize> {
        let expired = conn.execute(
            "UPDATE bridge_outbox SET delivery_state='expired', resolved_at=?1, content='' WHERE delivery_state='accepted' AND expires_at <= ?1", [now],
        )?;
        conn.execute("DELETE FROM bridge_outbox WHERE resolved_at < ?1 AND delivery_state IN ('confirmed','dismissed','expired')", [now - 30 * 86400])?;
        // Keep at most 10,000 terminal receipts per bridge. This bounds the
        // dedup window by both age and volume, never by evicting active attempts.
        conn.execute("DELETE FROM bridge_outbox WHERE seq IN (SELECT seq FROM (SELECT seq, ROW_NUMBER() OVER (PARTITION BY bridge ORDER BY resolved_at DESC, seq DESC) AS rank FROM bridge_outbox WHERE delivery_state IN ('confirmed','dismissed','expired')) WHERE rank > 10000)", [])?;
        Ok(expired)
    }

    /// Atomically reserve immediately before I/O. Policy mutations and competing
    /// sockets cannot make a second reservation. An interrupted attempt is unknown.
    pub fn claim(&self, bridge: &str, id: &str, now: i64) -> Result<bool> {
        let changed = self.conn.lock().execute(
            "UPDATE bridge_outbox SET delivery_state='unknown' WHERE bridge=?1 AND id=?2
             AND delivery_state='accepted' AND expires_at > ?3
             AND (snoozed_until IS NULL OR snoozed_until <= ?3)
             AND NOT EXISTS (SELECT 1 FROM bridge_attention_mutes m WHERE
               m.bridge=bridge_outbox.bridge AND m.recipient=bridge_outbox.recipient AND
               m.source_kind=bridge_outbox.source_kind AND m.source_id=bridge_outbox.source_id)",
            params![bridge, id, now],
        )?;
        Ok(changed != 0)
    }

    /// `sent` means only socket handoff, never platform delivery. A receipt wins.
    pub fn mark_sent(&self, bridge: &str, id: &str) -> Result<()> {
        self.conn.lock().execute("UPDATE bridge_outbox SET delivery_state='sent' WHERE bridge=?1 AND id=?2 AND delivery_state='unknown'", params![bridge,id])?;
        Ok(())
    }

    /// Call only while exclusively registering the first socket for a bridge.
    pub fn mark_unknown(&self, bridge: &str) -> Result<()> {
        self.conn.lock().execute("UPDATE bridge_outbox SET delivery_state='unknown' WHERE bridge=?1 AND delivery_state='sent'", [bridge])?;
        Ok(())
    }

    /// Socket teardown may update only the attempt that socket handed off.
    pub fn mark_attempt_unknown(&self, bridge: &str, id: &str) -> Result<()> {
        self.conn.lock().execute("UPDATE bridge_outbox SET delivery_state='unknown' WHERE bridge=?1 AND id=?2 AND delivery_state='sent'", params![bridge,id])?;
        Ok(())
    }

    /// Resolve only the current transport attempt's canonical receipt.
    pub fn is_resolved(&self, bridge: &str, id: &str) -> Result<bool> {
        Ok(self.conn.lock().query_row(
            "SELECT delivery_state IN ('confirmed','dismissed','expired') FROM bridge_outbox WHERE bridge=?1 AND id=?2",
            params![bridge,id], |row| row.get::<_,bool>(0),
        ).optional()?.unwrap_or(false))
    }

    /// Owner actions are scoped to the exact recipient and canonical source.
    /// Caller must authenticate operator authority; no bridge token can call this API.
    #[allow(clippy::too_many_arguments)]
    pub fn owner_action(
        &self,
        bridge: &str,
        recipient: &str,
        source_kind: &str,
        source_id: &str,
        id: &str,
        action: &str,
        until: Option<i64>,
        owner: &str,
    ) -> Result<bool> {
        anyhow::ensure!(!owner.is_empty(), "attention_owner_required");
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let state: Option<String> = tx.query_row(
            "SELECT delivery_state FROM bridge_outbox WHERE bridge=?1 AND recipient=?2 AND source_kind=?3 AND source_id=?4 AND id=?5",
            params![bridge,recipient,source_kind,source_id,id], |row| row.get(0),
        ).optional()?;
        // The mute fact survives terminal receipt cleanup and retains the exact
        // candidate that created it, so an owner can still undo it later.
        if action == "unmute" && state.is_none() {
            let removed = tx.execute("DELETE FROM bridge_attention_mutes WHERE bridge=?1 AND recipient=?2 AND source_kind=?3 AND source_id=?4 AND candidate_id=?5", params![bridge,recipient,source_kind,source_id,id])?;
            tx.commit()?;
            self.wake();
            return Ok(removed != 0);
        }
        let Some(state) = state else { return Ok(false) };
        match action {
            "mute" => {
                let count: i64 = tx.query_row("SELECT COUNT(*) FROM bridge_attention_mutes WHERE bridge=?1 AND NOT (recipient=?2 AND source_kind=?3 AND source_id=?4)", params![bridge,recipient,source_kind,source_id], |row| row.get(0))?;
                anyhow::ensure!(count < 1000, "attention_mute_capacity_reached");
                tx.execute(
                    "INSERT OR REPLACE INTO bridge_attention_mutes VALUES (?1,?2,?3,?4,?5,?6)",
                    params![bridge, recipient, source_kind, source_id, owner, id],
                )?;
            }
            "unmute" => {
                tx.execute("DELETE FROM bridge_attention_mutes WHERE bridge=?1 AND recipient=?2 AND source_kind=?3 AND source_id=?4", params![bridge,recipient,source_kind,source_id])?;
            }
            "snooze" => {
                anyhow::ensure!(state == "accepted", "attention_candidate_already_attempted");
                let until = until.context("attention_snooze_until_required")?;
                anyhow::ensure!(until > now_secs(), "attention_snooze_must_be_future");
                tx.execute(
                    "UPDATE bridge_outbox SET snoozed_until=?1, owner_action_by=?2 WHERE id=?3",
                    params![until, owner, id],
                )?;
            }
            "dismiss" => {
                anyhow::ensure!(
                    matches!(state.as_str(), "accepted" | "unknown" | "sent"),
                    "attention_candidate_resolved"
                );
                tx.execute("UPDATE bridge_outbox SET delivery_state='dismissed', resolved_at=?1, owner_action_by=?2, content='' WHERE id=?3", params![now_secs(),owner,id])?;
            }
            _ => anyhow::bail!("attention_unknown_action"),
        }
        tx.commit()?;
        self.wake();
        Ok(true)
    }

    /// Bounded operator inbox ordered by insertion; payload content stays private.
    pub fn list(
        &self,
        bridge: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        anyhow::ensure!((1..=200).contains(&limit), "attention_invalid_limit");
        let ids = {
            let conn = self.conn.lock();
            let mut query = conn.prepare(
                "SELECT seq,id FROM bridge_outbox WHERE bridge=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
            )?;
            query
                .query_map(params![bridge, after_seq, limit as i64], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut items = Vec::new();
        for (seq, id) in ids {
            if let Some(mut item) = self.inspect(bridge, &id)? {
                item["seq"] = seq.into();
                items.push(item);
            }
        }
        Ok(items)
    }

    /// Persistent source mute facts; at most 1000 per bridge.
    pub fn mutes(&self, bridge: &str) -> Result<Vec<serde_json::Value>> {
        let conn = self.conn.lock();
        let mut query = conn.prepare("SELECT recipient,source_kind,source_id,candidate_id FROM bridge_attention_mutes WHERE bridge=?1 ORDER BY recipient,source_kind,source_id LIMIT 1000")?;
        let rows = query.query_map([bridge], |row| Ok(serde_json::json!({"recipient":row.get::<_,String>(0)?,"source_kind":row.get::<_,String>(1)?,"source_id":row.get::<_,String>(2)?,"id":row.get::<_,String>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Operator inspection includes terminal receipts and uncertain sends.
    pub fn inspect(&self, bridge: &str, id: &str) -> Result<Option<serde_json::Value>> {
        self.conn.lock().query_row(
            "SELECT id, recipient, source_kind, source_id, event_id, delivery_state, snoozed_until, expires_at, owner_action_by,
             EXISTS(SELECT 1 FROM bridge_attention_mutes m WHERE m.bridge=bridge_outbox.bridge AND m.recipient=bridge_outbox.recipient AND m.source_kind=bridge_outbox.source_kind AND m.source_id=bridge_outbox.source_id)
             FROM bridge_outbox WHERE bridge=?1 AND id=?2", params![bridge,id], |row| {
                Ok(serde_json::json!({"id": row.get::<_,String>(0)?, "recipient": row.get::<_,String>(1)?, "source_kind": row.get::<_,String>(2)?, "source_id": row.get::<_,String>(3)?, "event_id": row.get::<_,String>(4)?, "delivery_state": row.get::<_,String>(5)?, "snoozed_until": row.get::<_,Option<i64>>(6)?, "expires_at": row.get::<_,i64>(7)?, "owner_action_recorded": row.get::<_,Option<String>>(8)?.is_some(), "muted": row.get::<_,bool>(9)?}))
            },
        ).optional().context("reading attention receipt")
    }
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outbox() -> (tempfile::TempDir, BridgeOutbox) {
        let dir = tempfile::tempdir().unwrap();
        let outbox = BridgeOutbox::open(dir.path()).unwrap();
        (dir, outbox)
    }

    fn contents(items: &[OutboxItem]) -> Vec<&str> {
        items.iter().map(|item| item.content.as_str()).collect()
    }

    #[test]
    fn rows_come_back_in_order_per_bridge_until_acked() {
        let (_dir, outbox) = outbox();
        outbox.enqueue("tg", "42", None, "one").unwrap();
        outbox.enqueue("other", "7", None, "elsewhere").unwrap();
        outbox.enqueue("tg", "42", Some("9"), "two").unwrap();
        outbox.enqueue("tg", "43", None, "three").unwrap();

        let all = outbox.pending("tg", 0, 100).unwrap();
        assert_eq!(contents(&all), ["one", "two", "three"]);
        assert_eq!(all[1].thread_id.as_deref(), Some("9"));
        assert_eq!(all[2].to, "43");
        // Resuming after a sequence number skips what was already sent.
        let rest = outbox.pending("tg", all[0].seq, 100).unwrap();
        assert_eq!(contents(&rest), ["two", "three"]);

        // A replay (from 0) still has every unacked row, oldest first.
        assert!(outbox.claim("tg", &all[1].id, now_secs()).unwrap());
        assert!(outbox.ack("tg", &all[1].id).unwrap());
        assert!(
            !outbox.ack("tg", &all[1].id).unwrap(),
            "second ack is a no-op"
        );
        assert!(
            !outbox.ack("other", &all[0].id).unwrap(),
            "acks are per bridge"
        );
        let replay = outbox.pending("tg", 0, 100).unwrap();
        assert_eq!(contents(&replay), ["one", "three"]);
        assert_eq!(outbox.len("other").unwrap(), 1);
    }

    #[test]
    fn the_outbox_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let id = BridgeOutbox::open(dir.path())
            .unwrap()
            .enqueue("tg", "42", None, "kept")
            .unwrap();
        let reopened = BridgeOutbox::open(dir.path()).unwrap();
        let rows = reopened.pending("tg", 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
    }

    #[test]
    fn capacity_refuses_new_rows_without_losing_deferred_or_unknown() {
        let (_dir, mut outbox) = outbox();
        outbox.max_rows = 2;
        let first = outbox.enqueue("tg", "42", None, "one").unwrap();
        outbox.enqueue("tg", "42", None, "two").unwrap();
        assert!(outbox.claim("tg", &first, now_secs()).unwrap());
        assert!(outbox.enqueue("tg", "42", None, "third").is_err());
        assert_eq!(outbox.len("tg").unwrap(), 2);
        assert_eq!(
            outbox.inspect("tg", &first).unwrap().unwrap()["delivery_state"],
            "unknown"
        );
    }

    #[test]
    fn expired_rows_are_purged_on_write_and_on_request() {
        let (_dir, outbox) = outbox();
        outbox.enqueue("tg", "42", None, "old").unwrap();
        let stale = now_secs() - ROW_TTL.as_secs() as i64 - 1;
        outbox
            .conn
            .lock()
            .execute("UPDATE bridge_outbox SET expires_at = ?1", params![stale])
            .unwrap();
        assert_eq!(outbox.purge_expired().unwrap(), 1);
        assert_eq!(outbox.len("tg").unwrap(), 0);

        outbox.enqueue("tg", "42", None, "old again").unwrap();
        outbox
            .conn
            .lock()
            .execute("UPDATE bridge_outbox SET expires_at = ?1", params![stale])
            .unwrap();
        outbox.enqueue("tg", "42", None, "fresh").unwrap();
        assert_eq!(contents(&outbox.pending("tg", 0, 100).unwrap()), ["fresh"]);
    }

    #[test]
    fn writes_wake_subscribers_and_shared_handles_are_one() {
        let dir = tempfile::tempdir().unwrap();
        let a = BridgeOutbox::shared(dir.path()).unwrap();
        let b = BridgeOutbox::shared(dir.path()).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        let mut rx = a.subscribe();
        assert!(!rx.has_changed().unwrap());
        b.enqueue("tg", "42", None, "wake").unwrap();
        assert!(rx.has_changed().unwrap());
        rx.mark_unchanged();
        assert!(!rx.has_changed().unwrap());
    }
    #[test]
    fn source_dedup_actions_restart_and_lost_receipt_are_durable() {
        let (dir, outbox) = outbox();
        let enqueue = |store: &BridgeOutbox, event: &str| {
            store
                .enqueue_source("tg", "owner", None, "notice", "cron", "job", event)
                .unwrap()
        };
        let id = enqueue(&outbox, "run1");
        assert_eq!(enqueue(&outbox, "run1"), id);
        assert!(
            !outbox
                .owner_action(
                    "tg", "wrong", "cron", "job", &id, "dismiss", None, "operator"
                )
                .unwrap()
        );
        assert!(
            outbox
                .owner_action(
                    "tg",
                    "owner",
                    "cron",
                    "job",
                    &id,
                    "snooze",
                    Some(now_secs() + 3600),
                    "operator"
                )
                .unwrap()
        );
        assert!(!outbox.claim("tg", &id, now_secs()).unwrap());
        drop(outbox);
        let outbox = BridgeOutbox::open(dir.path()).unwrap();
        assert_eq!(enqueue(&outbox, "run1"), id);
        assert!(!outbox.claim("tg", &id, now_secs()).unwrap());
        assert!(outbox.claim("tg", &id, now_secs() + 3601).unwrap());
        // Simulated platform send succeeded but its response was lost.
        outbox.mark_sent("tg", &id).unwrap();
        outbox.mark_unknown("tg").unwrap();
        assert!(!outbox.claim("tg", &id, now_secs() + 3602).unwrap());
        outbox
            .conn
            .lock()
            .execute("UPDATE bridge_outbox SET expires_at=1", [])
            .unwrap();
        outbox.purge_expired().unwrap();
        assert_eq!(
            outbox.inspect("tg", &id).unwrap().unwrap()["delivery_state"],
            "unknown"
        );
        assert!(outbox.pending("tg", 0, 100).unwrap().is_empty());
        assert!(outbox.ack("tg", &id).unwrap());
        outbox.mark_sent("tg", &id).unwrap();
        assert_eq!(
            outbox.inspect("tg", &id).unwrap().unwrap()["delivery_state"],
            "confirmed"
        );
        assert_eq!(enqueue(&outbox, "run1"), id);
        let next = enqueue(&outbox, "run2");
        assert!(
            outbox
                .owner_action(
                    "tg", "owner", "cron", "job", &next, "mute", None, "operator"
                )
                .unwrap()
        );
        assert!(!outbox.claim("tg", &next, now_secs()).unwrap());
        let other = outbox
            .enqueue_source("tg", "owner", None, "other", "cron", "job-other", "run1")
            .unwrap();
        assert!(outbox.claim("tg", &other, now_secs()).unwrap());
        outbox
            .owner_action(
                "tg", "owner", "cron", "job", &next, "unmute", None, "operator",
            )
            .unwrap();
        outbox
            .owner_action(
                "tg", "owner", "cron", "job", &next, "dismiss", None, "operator",
            )
            .unwrap();
        assert_eq!(enqueue(&outbox, "run2"), next);
        assert!(!outbox.claim("tg", &next, now_secs()).unwrap());
    }
    #[test]
    fn legacy_database_migration_preserves_queued_identity_order_and_payloads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sessions")).unwrap();
        let legacy = Connection::open(dir.path().join("sessions").join(DB_FILE)).unwrap();
        legacy.execute_batch("CREATE TABLE bridge_outbox (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, bridge TEXT NOT NULL, recipient TEXT NOT NULL, thread_id TEXT, content TEXT NOT NULL, created_at INTEGER NOT NULL)").unwrap();
        legacy.execute("INSERT INTO bridge_outbox (id,bridge,recipient,thread_id,content,created_at) VALUES ('old-1','tg','owner','thread','first',?1),('old-2','tg','other',NULL,'second',?1)", [now_secs()]).unwrap();
        drop(legacy);
        let store = BridgeOutbox::open(dir.path()).unwrap();
        let rows = store.pending("tg", 0, 100).unwrap();
        assert_eq!(contents(&rows), ["first", "second"]);
        assert_eq!(
            (&rows[0].id, &rows[0].to, rows[0].thread_id.as_deref()),
            (&"old-1".to_string(), &"owner".to_string(), Some("thread"))
        );
        assert_eq!(
            (&rows[1].source_id, &rows[1].event_id),
            (&"old-2".to_string(), &"old-2".to_string())
        );
        assert_ne!(rows[0].source_id, rows[1].source_id);
        assert_eq!(rows[0].source_kind, "legacy");
        drop(store);
        let store = BridgeOutbox::open(dir.path()).unwrap();
        assert_eq!(store.pending("tg", 0, 100).unwrap(), rows);
    }

    #[test]
    fn source_unmute_survives_terminal_receipt_retention() {
        let (_dir, store) = outbox();
        let id = store
            .enqueue_source("tg", "owner", None, "notice", "cron", "job", "run")
            .unwrap();
        store
            .owner_action("tg", "owner", "cron", "job", &id, "mute", None, "operator")
            .unwrap();
        store
            .owner_action(
                "tg", "owner", "cron", "job", &id, "dismiss", None, "operator",
            )
            .unwrap();
        store
            .conn
            .lock()
            .execute("UPDATE bridge_outbox SET resolved_at=1", [])
            .unwrap();
        store.purge_expired().unwrap();
        assert!(store.inspect("tg", &id).unwrap().is_none());
        assert_eq!(store.mutes("tg").unwrap()[0]["id"], id);
        assert!(
            !store
                .owner_action(
                    "tg", "wrong", "cron", "job", &id, "unmute", None, "operator"
                )
                .unwrap()
        );
        assert!(
            store
                .owner_action(
                    "tg", "owner", "cron", "job", &id, "unmute", None, "operator"
                )
                .unwrap()
        );
        assert!(store.mutes("tg").unwrap().is_empty());
    }
}
