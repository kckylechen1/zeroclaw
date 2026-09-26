//! Durable outbox for proactive messages to channel bridges.
//!
//! Cron results, heartbeat alerts and the `notify` tool write here when
//! their target names a `[gateway.bridges.<name>]` entry. The gateway's
//! `/ws/bridge` control socket drains it: it sends each row as a `deliver`
//! frame and deletes the row when the bridge answers `delivered`.
//!
//! - At least once: a row stays until it is acknowledged, so a bridge that
//!   reconnects gets every unacknowledged row again, oldest first. Bridges
//!   dedupe on the row id.
//! - FIFO per `(bridge, to)`: rows are replayed and sent in insertion order.
//! - Bounded: at most [`MAX_ROWS_PER_BRIDGE`] rows per bridge (the oldest are
//!   dropped) and rows older than [`ROW_TTL`] are purged on every write and
//!   whenever a bridge connects.
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

/// Rows kept per bridge; older rows are dropped when a write exceeds it.
pub const MAX_ROWS_PER_BRIDGE: usize = 1000;
/// Rows older than this are purged unsent.
pub const ROW_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const DB_FILE: &str = "bridge_outbox.db";

/// One queued message.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        let conn = Connection::open(&db_path)
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
        let now = now_secs();
        let dropped = {
            let conn = self.conn.lock();
            self.purge_expired_locked(&conn, now)?;
            conn.execute(
                "INSERT INTO bridge_outbox (id, bridge, recipient, thread_id, content, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, bridge, to, thread_id, content, now],
            )
            .context("writing to the bridge outbox")?;
            conn.execute(
                "DELETE FROM bridge_outbox WHERE bridge = ?1 AND seq NOT IN (
                    SELECT seq FROM bridge_outbox WHERE bridge = ?1
                    ORDER BY seq DESC LIMIT ?2)",
                params![bridge, self.max_rows as i64],
            )
            .context("capping the bridge outbox")?
        };
        crate::sqlite_perms::harden_sqlite_owner_only(&self.db_path);
        if dropped > 0 {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "bridge": bridge,
                        "dropped": dropped,
                        "cap": self.max_rows,
                    })),
                "bridge outbox full; dropped the oldest undelivered messages"
            );
        }
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
        Ok(id)
    }

    /// Rows for `bridge` after `after_seq`, oldest first, at most `limit`.
    pub fn pending(&self, bridge: &str, after_seq: i64, limit: usize) -> Result<Vec<OutboxItem>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT seq, id, bridge, recipient, thread_id, content FROM bridge_outbox
             WHERE bridge = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading the bridge outbox")?;
        Ok(rows)
    }

    /// Delete an acknowledged row. Returns whether it was still queued; a
    /// second ack for the same id is harmless.
    pub fn ack(&self, bridge: &str, id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let deleted = conn
            .execute(
                "DELETE FROM bridge_outbox WHERE bridge = ?1 AND id = ?2",
                params![bridge, id],
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
                "SELECT COUNT(*) FROM bridge_outbox WHERE bridge = ?1",
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
        let cutoff = now - self.ttl.as_secs() as i64;
        let purged = conn
            .execute(
                "DELETE FROM bridge_outbox WHERE created_at < ?1",
                params![cutoff],
            )
            .context("purging expired bridge outbox rows")?;
        if purged > 0 {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "purged": purged,
                        "ttl_secs": self.ttl.as_secs(),
                    })),
                "bridge outbox purged messages no bridge picked up in time"
            );
        }
        Ok(purged)
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
    fn a_full_bridge_drops_its_oldest_rows_only() {
        let (_dir, mut outbox) = outbox();
        outbox.max_rows = 3;
        outbox.enqueue("other", "1", None, "untouched").unwrap();
        for n in 0..5 {
            outbox.enqueue("tg", "42", None, &format!("m{n}")).unwrap();
        }
        assert_eq!(
            contents(&outbox.pending("tg", 0, 100).unwrap()),
            ["m2", "m3", "m4"]
        );
        assert_eq!(outbox.len("other").unwrap(), 1);
    }

    #[test]
    fn expired_rows_are_purged_on_write_and_on_request() {
        let (_dir, outbox) = outbox();
        outbox.enqueue("tg", "42", None, "old").unwrap();
        let stale = now_secs() - ROW_TTL.as_secs() as i64 - 1;
        outbox
            .conn
            .lock()
            .execute("UPDATE bridge_outbox SET created_at = ?1", params![stale])
            .unwrap();
        assert_eq!(outbox.purge_expired().unwrap(), 1);
        assert_eq!(outbox.len("tg").unwrap(), 0);

        outbox.enqueue("tg", "42", None, "old again").unwrap();
        outbox
            .conn
            .lock()
            .execute("UPDATE bridge_outbox SET created_at = ?1", params![stale])
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
}
