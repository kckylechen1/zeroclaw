//! Bounded durable set of recently seen inbound message IDs.
//!
//! Closes the at-least-once redelivery window opened by cursor persistence:
//! a crash between batch enqueue and cursor persist replays already
//! processed messages on restart. The dispatch loop records every accepted
//! message ID here before dispatch, so a redelivery is dropped instead of
//! starting a second agent turn with duplicate tool side effects.

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::Connection;
use zeroclaw_infra::sqlite_perms::harden_sqlite_owner_only;

/// Recent IDs retained per channel account. Sized to cover restart
/// redelivery windows; oldest entries are evicted FIFO past this bound.
const PER_ACCOUNT_CAP: usize = 1024;

pub(crate) struct SeenMessageStore {
    conn: Mutex<Connection>,
}

impl SeenMessageStore {
    /// Open (or create) the store as `channel_seen_ids.db` under
    /// `data_dir`. Blocking; call before the async dispatch loop starts.
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
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                UNIQUE(account, message_id)
             );",
        )?;
        harden_sqlite_owner_only(&db_path);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record `message_id` for `account`. Returns `Ok(true)` when fresh
    /// (and now recorded), `Ok(false)` when already seen. Blocking; call
    /// via `spawn_blocking` from async contexts.
    pub(crate) fn check_and_record(
        &self,
        account: &str,
        message_id: &str,
    ) -> Result<bool, rusqlite::Error> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO seen_message_ids (account, message_id) VALUES (?1, ?2)",
            rusqlite::params![account, message_id],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM seen_message_ids WHERE account = ?1 AND seq NOT IN (
                 SELECT seq FROM seen_message_ids WHERE account = ?1
                 ORDER BY seq DESC LIMIT ?2
             )",
            rusqlite::params![account, PER_ACCOUNT_CAP],
        )?;
        tx.commit()?;
        Ok(true)
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
    fn fresh_then_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenMessageStore::open(dir.path()).unwrap();
        assert!(
            store.check_and_record("wechat.main", "m-1").unwrap(),
            "first sight must be fresh"
        );
        assert!(
            !store.check_and_record("wechat.main", "m-1").unwrap(),
            "redelivery must not be fresh"
        );
    }

    #[test]
    fn same_id_across_accounts_is_independent() {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenMessageStore::open(dir.path()).unwrap();
        assert!(store.check_and_record("wechat.main", "m-1").unwrap());
        assert!(
            store.check_and_record("telegram.bot", "m-1").unwrap(),
            "an id is scoped to its account, not global"
        );
    }

    #[test]
    fn per_account_cap_evicts_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = SeenMessageStore::open(dir.path()).unwrap();
        for i in 0..(PER_ACCOUNT_CAP + 2) {
            assert!(
                store
                    .check_and_record("wechat.main", &format!("m-{i}"))
                    .unwrap()
            );
        }
        assert!(
            store.check_and_record("wechat.main", "m-0").unwrap(),
            "the oldest entry must have been evicted past the cap"
        );
        assert!(
            !store
                .check_and_record("wechat.main", &format!("m-{}", PER_ACCOUNT_CAP + 1))
                .unwrap(),
            "the newest entries must still be remembered"
        );
    }

    #[test]
    fn store_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = SeenMessageStore::open(dir.path()).unwrap();
            assert!(store.check_and_record("slack.team", "m-9").unwrap());
        }
        let reopened = SeenMessageStore::open(dir.path()).unwrap();
        assert!(
            !reopened.check_and_record("slack.team", "m-9").unwrap(),
            "seen IDs must be durable across restarts"
        );
    }
}
