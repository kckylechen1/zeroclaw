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

/// Completed and stale-received ids retained per channel account. Covers
/// restart redelivery windows; oldest entries past this bound are evicted
/// FIFO within their state class. A live turn's `received` row is never
/// evicted from under its in-flight owner.
const PER_ACCOUNT_CAP: usize = 1024;

/// Ownership of an admitted message until its turn completes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct InboxReceipt {
    account: String,
    message_id: String,
}

/// The outcome of offering a message to the inbox.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Not seen before, or seen but never completed — process it.
    Fresh(InboxReceipt),
    /// A previous turn completed for this id — drop the redelivery.
    DuplicateCompleted,
    /// A live turn in this process is working on this id — drop the
    /// concurrent duplicate (interruption semantics belong to the
    /// in-flight registry, not the inbox).
    DuplicateInFlight,
}

struct InboxState {
    conn: Connection,
    in_flight: HashSet<(String, String)>,
}

pub(crate) struct MessageInbox {
    state: Mutex<InboxState>,
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
            state: Mutex::new(InboxState {
                conn,
                in_flight: HashSet::new(),
            }),
        })
    }

    /// Offer a message for dispatch. `Fresh` must be followed (eventually)
    /// by [`MessageInbox::mark_completed_batch`] or the id stays `received`
    /// and a post-restart redelivery will re-process it.
    pub(crate) fn admit(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<Admission, rusqlite::Error> {
        let key = (account.to_string(), message_id.to_string());
        let mut state = self.state.lock();
        let InboxState { conn, in_flight } = &mut *state;
        let completed = {
            let tx = conn.unchecked_transaction()?;
            let inserted = tx.execute(
                "INSERT OR IGNORE INTO seen_message_ids (account, message_id) VALUES (?1, ?2)",
                rusqlite::params![account, message_id],
            )?;
            let completed = if inserted == 0 {
                let persisted_state: String = tx.query_row(
                    "SELECT state FROM seen_message_ids WHERE account = ?1 AND message_id = ?2",
                    rusqlite::params![account, message_id],
                    |row| row.get(0),
                )?;
                persisted_state == "completed"
            } else {
                tx.execute(
                    "DELETE FROM seen_message_ids
                     WHERE account = ?1 AND state = 'completed' AND seq NOT IN (
                         SELECT seq FROM seen_message_ids WHERE account = ?1
                         ORDER BY seq DESC LIMIT ?2
                     )",
                    rusqlite::params![account, PER_ACCOUNT_CAP],
                )?;
                // Age crash-orphaned `received` rows the same way: past the
                // newest PER_ACCOUNT_CAP received entries, a row that no live
                // turn in this process owns can never complete (its process
                // died), so keeping it forever would grow the table without
                // bound. Evicting it is safe — a later delivery of the same
                // id is admitted Fresh and re-processed, the at-least-once
                // direction. In-flight owners are excluded so their
                // completion UPDATE always finds its row.
                let stale: Vec<(i64, String)> = {
                    let mut stmt = tx.prepare(
                        "SELECT seq, message_id FROM seen_message_ids
                         WHERE account = ?1 AND state = 'received'
                         ORDER BY seq DESC LIMIT -1 OFFSET ?2",
                    )?;
                    stmt.query_map(rusqlite::params![account, PER_ACCOUNT_CAP], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                };
                for (seq, stale_message_id) in stale {
                    let owned_by_live_turn =
                        in_flight.contains(&(account.to_string(), stale_message_id.clone()));
                    if !owned_by_live_turn {
                        tx.execute(
                            "DELETE FROM seen_message_ids WHERE account = ?1 AND seq = ?2",
                            rusqlite::params![account, seq],
                        )?;
                    }
                }
                false
            };
            tx.commit()?;
            completed
        };

        if completed {
            return Ok(Admission::DuplicateCompleted);
        }
        if !in_flight.insert(key.clone()) {
            return Ok(Admission::DuplicateInFlight);
        }
        Ok(Admission::Fresh(InboxReceipt {
            account: key.0,
            message_id: key.1,
        }))
    }

    /// Complete every message represented by one combined turn atomically.
    /// On failure, all live claims are released so redelivery can retry.
    pub(crate) fn mark_completed_batch(
        &self,
        receipts: &[InboxReceipt],
    ) -> Result<(), rusqlite::Error> {
        if receipts.is_empty() {
            return Ok(());
        }

        let mut state = self.state.lock();
        let result = (|| {
            let tx = state.conn.unchecked_transaction()?;
            for receipt in receipts {
                let updated = tx.execute(
                    "UPDATE seen_message_ids SET state = 'completed'
                     WHERE account = ?1 AND message_id = ?2",
                    rusqlite::params![receipt.account, receipt.message_id],
                )?;
                if updated != 1 {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
            }
            tx.commit()
        })();
        for receipt in receipts {
            state
                .in_flight
                .remove(&(receipt.account.clone(), receipt.message_id.clone()));
        }
        result
    }

    /// Release live claims without completing their rows: the turn was
    /// abandoned before it processed anything, so its ids stay `received`
    /// and a redelivery is admitted fresh. Durable state is untouched.
    pub(crate) fn release_claims(&self, receipts: &[InboxReceipt]) {
        let mut state = self.state.lock();
        for receipt in receipts {
            state
                .in_flight
                .remove(&(receipt.account.clone(), receipt.message_id.clone()));
        }
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

    fn expect_fresh(admission: Admission) -> InboxReceipt {
        match admission {
            Admission::Fresh(receipt) => receipt,
            other => panic!("expected fresh admission, got {other:?}"),
        }
    }

    #[test]
    fn completed_redelivery_is_dropped_live_and_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        let receipt = expect_fresh(inbox.admit("wechat.main", "m-1").unwrap());
        inbox.mark_completed_batch(&[receipt]).unwrap();
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
            expect_fresh(inbox.admit("telegram.bot", "m-2").unwrap());
            // no mark_completed: the turn died with the process
        }
        let reopened = MessageInbox::open(dir.path()).unwrap();
        expect_fresh(reopened.admit("telegram.bot", "m-2").unwrap());
    }

    /// Within one live process, a concurrent duplicate of an in-flight
    /// message is dropped (the in-flight registry owns interruption).
    #[test]
    fn concurrent_duplicate_of_in_flight_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        let receipt = expect_fresh(inbox.admit("slack.team", "m-3").unwrap());
        assert_eq!(
            inbox.admit("slack.team", "m-3").unwrap(),
            Admission::DuplicateInFlight
        );
        inbox.mark_completed_batch(&[receipt]).unwrap();
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
        expect_fresh(inbox.admit("wechat.main", "m-1").unwrap());
        expect_fresh(inbox.admit("telegram.bot", "m-1").unwrap());
    }

    #[test]
    fn per_account_cap_evicts_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        for i in 0..(PER_ACCOUNT_CAP + 2) {
            let message_id = format!("m-{i}");
            let receipt = expect_fresh(inbox.admit("wechat.main", &message_id).unwrap());
            inbox.mark_completed_batch(&[receipt]).unwrap();
        }
        expect_fresh(inbox.admit("wechat.main", "m-0").unwrap());
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
        expect_fresh(inbox.admit("old.bot", "legacy-1").unwrap());
    }

    #[test]
    fn received_redelivery_has_one_live_owner() {
        let dir = tempfile::tempdir().unwrap();
        {
            let inbox = MessageInbox::open(dir.path()).unwrap();
            expect_fresh(inbox.admit("wechat.main", "m-race").unwrap());
        }
        let inbox = std::sync::Arc::new(MessageInbox::open(dir.path()).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let inbox = std::sync::Arc::clone(&inbox);
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    inbox.admit("wechat.main", "m-race").unwrap()
                })
            })
            .collect();
        let admissions: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert_eq!(
            admissions
                .iter()
                .filter(|admission| matches!(admission, Admission::Fresh(_)))
                .count(),
            1
        );
        assert_eq!(
            admissions
                .iter()
                .filter(|admission| matches!(admission, Admission::DuplicateInFlight))
                .count(),
            7
        );
    }

    #[test]
    fn batch_completion_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        let first = expect_fresh(inbox.admit("wechat.main", "m-1").unwrap());
        let second = expect_fresh(inbox.admit("wechat.main", "m-2").unwrap());
        inbox
            .state
            .lock()
            .conn
            .execute_batch(
                "CREATE TRIGGER fail_second_completion
                 BEFORE UPDATE OF state ON seen_message_ids
                 WHEN NEW.message_id = 'm-2'
                 BEGIN SELECT RAISE(ABORT, 'injected completion failure'); END;",
            )
            .unwrap();

        assert!(
            inbox.mark_completed_batch(&[first, second]).is_err(),
            "the injected second update must fail the whole batch"
        );
        drop(inbox);

        let reopened = MessageInbox::open(dir.path()).unwrap();
        expect_fresh(reopened.admit("wechat.main", "m-1").unwrap());
        expect_fresh(reopened.admit("wechat.main", "m-2").unwrap());
    }

    fn received_row_count(inbox: &MessageInbox, account: &str) -> i64 {
        inbox
            .state
            .lock()
            .conn
            .query_row(
                "SELECT COUNT(*) FROM seen_message_ids
                 WHERE account = ?1 AND state = 'received'",
                rusqlite::params![account],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn crash_orphaned_received_rows_are_aged_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        {
            // A crashed process: it admitted rows without completing any,
            // so they persist as 'received' with no live owner anywhere.
            let inbox = MessageInbox::open(dir.path()).unwrap();
            for i in 0..(PER_ACCOUNT_CAP + 8) {
                expect_fresh(inbox.admit("wechat.main", &format!("orphan-{i}")).unwrap());
            }
        }

        let inbox = MessageInbox::open(dir.path()).unwrap();
        // The next fresh admission in a new process ages the stale rows.
        expect_fresh(inbox.admit("wechat.main", "later-1").unwrap());
        assert_eq!(
            received_row_count(&inbox, "wechat.main"),
            PER_ACCOUNT_CAP as i64,
            "crash-orphaned received rows must be bounded at the cap"
        );
        let oldest_gone: i64 = inbox
            .state
            .lock()
            .conn
            .query_row(
                "SELECT COUNT(*) FROM seen_message_ids
                 WHERE account = 'wechat.main' AND message_id = 'orphan-0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(oldest_gone, 0, "the oldest orphan must have been evicted");
        let newest_kept: i64 = inbox
            .state
            .lock()
            .conn
            .query_row(
                "SELECT COUNT(*) FROM seen_message_ids
                 WHERE account = 'wechat.main' AND message_id = 'later-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(newest_kept, 1, "the newest received row must be retained");
    }

    #[test]
    fn live_in_flight_received_rows_survive_aging() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        // Same process: every admitted id is a live in-flight owner, so
        // aging must not evict them even past the cap — their completion
        // UPDATE is still owed.
        for i in 0..(PER_ACCOUNT_CAP + 2) {
            expect_fresh(inbox.admit("wechat.main", &format!("live-{i}")).unwrap());
        }
        assert_eq!(
            received_row_count(&inbox, "wechat.main"),
            (PER_ACCOUNT_CAP + 2) as i64,
            "live in-flight owners must never be aged out"
        );
        assert_eq!(
            inbox.admit("wechat.main", "live-0").unwrap(),
            Admission::DuplicateInFlight,
            "a live owner's redelivery is still an in-flight duplicate"
        );
    }

    #[test]
    fn release_claims_keeps_rows_received() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = MessageInbox::open(dir.path()).unwrap();
        let first = expect_fresh(inbox.admit("wechat.main", "m-abandoned").unwrap());
        let second = expect_fresh(inbox.admit("wechat.main", "m-kept").unwrap());
        inbox.release_claims(&[first, second]);

        // Live claims are gone: a same-process redelivery is not an
        // in-flight duplicate anymore.
        assert!(
            matches!(
                inbox.admit("wechat.main", "m-abandoned").unwrap(),
                Admission::Fresh(_)
            ),
            "a released claim must admit fresh, not in-flight-duplicate"
        );
        drop(inbox);

        // Durable state was untouched: after a restart the rows are still
        // `received`, never `completed`.
        let reopened = MessageInbox::open(dir.path()).unwrap();
        assert!(
            matches!(
                reopened.admit("wechat.main", "m-kept").unwrap(),
                Admission::Fresh(_)
            ),
            "a released claim's row must survive as received, not completed"
        );
    }
}
