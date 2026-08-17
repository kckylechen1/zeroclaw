//! Durable inbound-message inbox: bounded, stateful
//! redelivery suppression for the at-least-once channel world.
//!
//! Channels deliver at-least-once across restarts (cursor persistence).
//! Suppressing a redelivery is only safe once the original turn
//! COMPLETED; a message recorded but unfinished (a crash mid-turn) must
//! re-process on redelivery rather than vanish. States:
//!
//! - `completed` (durable) — the turn finished; redelivery is dropped.
//! - `received` (durable) + in-flight (memory) — a live turn is working on
//!   it right now; a concurrent duplicate is dropped (the in-flight
//!   registry owns interruption semantics).
//! - `received` (durable) + NOT in-flight — the turn died with the process
//!   (or never started); redelivery re-processes (at-least-once).

use std::collections::HashSet;
use std::path::Path;

use parking_lot::Mutex;
use rusqlite::Connection;
use zeroclaw_infra::sqlite_perms::harden_sqlite_owner_only;

/// Recent ids retained per channel account. Covers restart redelivery
/// windows; oldest entries are evicted FIFO past this bound.
const PER_ACCOUNT_CAP: usize = 1024;

/// The outcome of offering a message to the inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Not seen before, or seen but never completed — process it.
    Fresh,
    /// A previous turn completed for this id — drop the redelivery.
    DuplicateCompleted,
    /// A live turn in this process is working on this id — drop the
    /// concurrent duplicate (interruption semantics belong to the
    /// in-flight registry, not the inbox).
    DuplicateInFlight,
}

pub(crate) struct MessageInbox {
    conn: Mutex<Connection>,
    in_flight: Mutex<HashSet<(String, String)>>,
}

impl MessageInbox {
    /// Open (or create) the inbox as `channel_seen_ids.db` under
    /// `data_dir`. Blocking; call before the async dispatch loop starts.
    /// Tolerates pre-rework databases by adding the `state` column with
    /// its default ('received') — old "seen" rows degrade to re-process
    /// once, which is the safe direction.
    pub(crate) fn open(data_dir: &Path) -> Result<Self, rusqlite::Error> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let db_path = data_dir.join("channel_seen_ids.db");
        create_owner_only_file(&db_path)?;
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS seen_message_ids (
                account TEXT NOT NULL,
                message_id TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'received',
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                UNIQUE(account, message_id)
             );",
        )?;
        let has_state: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('seen_message_ids') WHERE name = 'state'")?
            .exists([])?;
        if !has_state {
            conn.execute(
                "ALTER TABLE seen_message_ids ADD COLUMN state TEXT NOT NULL DEFAULT 'received'",
                [],
            )?;
        }
        harden_sqlite_owner_only(&db_path);
        Ok(Self {
            conn: Mutex::new(conn),
            in_flight: Mutex::new(HashSet::new()),
        })
    }

    /// Offer a message for dispatch. `Fresh` must be followed (eventually)
    /// by [`MessageInbox::mark_completed`] or the id stays `received` and a
    /// post-restart redelivery will re-process it.
    pub(crate) fn admit(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<Admission, rusqlite::Error> {
        let key = (account.to_string(), message_id.to_string());
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO seen_message_ids (account, message_id) VALUES (?1, ?2)",
            rusqlite::params![account, message_id],
        )?;
        if inserted == 0 {
            let state: String = tx.query_row(
                "SELECT state FROM seen_message_ids WHERE account = ?1 AND message_id = ?2",
                rusqlite::params![account, message_id],
                |row| row.get(0),
            )?;
            tx.commit()?;
            if state == "completed" {
                return Ok(Admission::DuplicateCompleted);
            }
            // recorded but unfinished: only a LIVE in-flight turn justifies
            // dropping the duplicate; after a restart this is Fresh.
            let duplicate_in_flight = self.in_flight.lock().contains(&key);
            return Ok(if duplicate_in_flight {
                Admission::DuplicateInFlight
            } else {
                Admission::Fresh
            });
        }
        tx.execute(
            "DELETE FROM seen_message_ids WHERE account = ?1 AND seq NOT IN (
                 SELECT seq FROM seen_message_ids WHERE account = ?1
                 ORDER BY seq DESC LIMIT ?2
             )",
            rusqlite::params![account, PER_ACCOUNT_CAP],
        )?;
        tx.commit()?;
        drop(conn);
        let _ = self.in_flight.lock().insert(key);
        Ok(Admission::Fresh)
    }

    /// Mark the turn finished: durable `completed` and removed from the
    /// in-flight set. Best-effort — a failure means a future redelivery
    /// re-processes (at-least-once), never a silent drop.
    pub(crate) fn mark_completed(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<(), rusqlite::Error> {
        self.in_flight
            .lock()
            .remove(&(account.to_string(), message_id.to_string()));
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE seen_message_ids SET state = 'completed'
             WHERE account = ?1 AND message_id = ?2",
            rusqlite::params![account, message_id],
        )?;
        Ok(())
    }
}

fn create_owner_only_file(path: &Path) -> Result<(), rusqlite::Error> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    harden_sqlite_owner_only(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_redelivery_is_dropped_live_and_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        assert_eq!(inbox.admit("wechat.main", "m-1").unwrap(), Admission::Fresh);
        inbox.mark_completed("wechat.main", "m-1").unwrap();
        assert_eq!(
            inbox.admit("wechat.main", "m-1").unwrap(),
            Admission::DuplicateCompleted
        );
        drop(inbox);
        let reopened = MessageInbox::open(dir.path()).unwrap();
        assert_eq!(
            reopened.admit("wechat.main", "m-1").unwrap(),
            Admission::DuplicateCompleted,
            "completion must be durable across restarts"
        );
    }

    /// The crash window: a message admitted but never completed (the
    /// process died mid-turn) must RE-PROCESS on redelivery, not vanish.
    #[test]
    fn uncompleted_message_reprocesses_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        {
            let inbox = MessageInbox::open(dir.path()).unwrap();
            assert_eq!(
                inbox.admit("telegram.bot", "m-2").unwrap(),
                Admission::Fresh
            );
            // no mark_completed: the turn died with the process
        }
        let reopened = MessageInbox::open(dir.path()).unwrap();
        assert_eq!(
            reopened.admit("telegram.bot", "m-2").unwrap(),
            Admission::Fresh,
            "a received-but-unfinished message must survive the crash window"
        );
    }

    /// Within one live process, a concurrent duplicate of an in-flight
    /// message is dropped (the in-flight registry owns interruption).
    #[test]
    fn concurrent_duplicate_of_in_flight_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        assert_eq!(inbox.admit("slack.team", "m-3").unwrap(), Admission::Fresh);
        assert_eq!(
            inbox.admit("slack.team", "m-3").unwrap(),
            Admission::DuplicateInFlight
        );
        inbox.mark_completed("slack.team", "m-3").unwrap();
        // after completion the in-flight set is cleared; durable state rules
        assert_eq!(
            inbox.admit("slack.team", "m-3").unwrap(),
            Admission::DuplicateCompleted
        );
    }

    #[test]
    fn same_id_across_accounts_is_independent() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        assert!(inbox.admit("wechat.main", "m-1").unwrap() == Admission::Fresh);
        assert_eq!(
            inbox.admit("telegram.bot", "m-1").unwrap(),
            Admission::Fresh,
            "an id is scoped to its account, not global"
        );
    }

    #[test]
    fn per_account_cap_evicts_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        for i in 0..(PER_ACCOUNT_CAP + 2) {
            assert!(inbox.admit("wechat.main", &format!("m-{i}")).unwrap() == Admission::Fresh);
        }
        assert!(
            inbox.admit("wechat.main", "m-0").unwrap() == Admission::Fresh,
            "the oldest entry must have been evicted past the cap"
        );
    }

    /// Pre-rework databases (no state column) migrate in place; their old
    /// "seen" rows degrade to received (re-process once) — the safe
    /// direction.
    #[test]
    fn legacy_database_migrates_to_received() {
        let dir = tempfile::tempdir().unwrap();
        {
            let conn = Connection::open(dir.path().join("channel_seen_ids.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE seen_message_ids (
                    account TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    seq INTEGER PRIMARY KEY AUTOINCREMENT,
                    UNIQUE(account, message_id)
                 );
                 INSERT INTO seen_message_ids (account, message_id) VALUES ('old.bot', 'legacy-1');",
            )
            .unwrap();
        }
        let inbox = MessageInbox::open(dir.path()).unwrap();
        assert_eq!(
            inbox.admit("old.bot", "legacy-1").unwrap(),
            Admission::Fresh,
            "a legacy seen row must not suppress a redelivery forever"
        );
    }
}
