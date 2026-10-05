//! Source-bound intake in sessions.db, independent of short-lived WS receipts.
//!
//! Source watermarks never expire. Under pressure only settled, old inputs
//! below the watermark can be reclaimed; a missing old update then fails
//! closed instead of becoming executable again. Pending and uncertain inputs
//! are never reclaimed automatically.

use super::SqliteSessionBackend;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::io;
use zeroclaw_api::bridge_intake::{BridgeInput, BridgeReceipt, BridgeResume, BridgeSource};

const MAX_SOURCES: i64 = 4096;
const MAX_INPUTS_PER_SOURCE: i64 = 128;
const MAX_INPUTS: i64 = 4096;
const MAX_PAYLOAD_BYTES: i64 = 64 * 1024 * 1024;
const RETAIN_SECONDS: i64 = 16 * 60;

pub(super) fn initialize(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS bridge_sources (
            source_key TEXT PRIMARY KEY,
            session_key TEXT NOT NULL,
            agent_alias TEXT NOT NULL,
            cursor INTEGER NOT NULL DEFAULT 0 CHECK (cursor >= 0)
         );
         CREATE TABLE IF NOT EXISTS bridge_inputs (
            source_key TEXT NOT NULL,
            update_id INTEGER NOT NULL,
            previous_cursor INTEGER NOT NULL,
            request_id TEXT NOT NULL,
            payload TEXT NOT NULL,
            initial_state TEXT NOT NULL,
            state TEXT NOT NULL,
            recorded_at INTEGER NOT NULL,
            PRIMARY KEY (source_key, update_id),
            UNIQUE (source_key, previous_cursor),
            UNIQUE (source_key, request_id),
            CHECK (previous_cursor >= 0 AND update_id >= previous_cursor),
            FOREIGN KEY (source_key) REFERENCES bridge_sources(source_key)
         );",
    )
}

/// An explicit session deletion/clear removes the input body without erasing
/// its source identity or making uncertain work executable again. The caller
/// includes this operation in its session-history write transaction.
pub(super) fn redact_session_inputs(conn: &Connection, session_key: &str) -> io::Result<()> {
    conn.execute(
        "UPDATE bridge_inputs
         SET payload = '',
             state = CASE
                 WHEN state = 'pending' THEN 'rejected'
                 WHEN state IN ('running', 'steered') THEN 'outcome_unknown'
                 ELSE state END
         WHERE source_key IN (SELECT source_key FROM bridge_sources WHERE session_key = ?1)",
        [session_key],
    )
    .map_err(io::Error::other)?;
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn validate_source(source: &BridgeSource) -> io::Result<()> {
    for (value, limit) in [
        (&source.key, 1024),
        (&source.session_key, 512),
        (&source.agent_alias, 128),
    ] {
        if value.is_empty() || value.len() > limit || value.chars().any(char::is_control) {
            return Err(invalid("invalid bridge source binding"));
        }
    }
    Ok(())
}

fn validate_input(input: &BridgeInput) -> io::Result<()> {
    if input.previous_cursor < 0
        || input.update_id < input.previous_cursor
        || input.update_id == i64::MAX
        || input.request_id.is_empty()
        || input.request_id.len() > 128
        || input.request_id.chars().any(char::is_control)
        || !matches!(
            input.state.as_str(),
            "pending" | "ignored" | "rejected" | "control"
        )
    {
        return Err(invalid("invalid bridge input"));
    }
    Ok(())
}

/// Reads the canonical immutable binding, without adopting a foreign session.
fn source_cursor(conn: &Connection, source: &BridgeSource) -> io::Result<Option<i64>> {
    validate_source(source)?;
    let binding: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT session_key, agent_alias, cursor FROM bridge_sources WHERE source_key = ?1",
            [&source.key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(io::Error::other)?;
    match binding {
        Some((session, agent, cursor)) => {
            if session != source.session_key || agent != source.agent_alias {
                return Err(invalid("bridge source is bound to another conversation"));
            }
            Ok(Some(cursor))
        }
        None => Ok(None),
    }
}

/// Caller holds an IMMEDIATE transaction so source creation and caps serialize
/// across independently opened database handles as well as one Rust mutex.
fn ensure_source(conn: &Connection, source: &BridgeSource) -> io::Result<i64> {
    if let Some(cursor) = source_cursor(conn, source)? {
        return Ok(cursor);
    }
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM bridge_sources", [], |row| row.get(0))
        .map_err(io::Error::other)?;
    if count >= MAX_SOURCES {
        return Err(io::Error::other("bridge source capacity exhausted"));
    }
    conn.execute(
        "INSERT INTO bridge_sources (source_key, session_key, agent_alias) VALUES (?1, ?2, ?3)",
        params![source.key, source.session_key, source.agent_alias],
    )
    .map_err(io::Error::other)?;
    Ok(0)
}

fn read_input(row: &rusqlite::Row<'_>) -> rusqlite::Result<BridgeInput> {
    Ok(BridgeInput {
        update_id: row.get(0)?,
        previous_cursor: row.get(1)?,
        request_id: row.get(2)?,
        payload: row.get(3)?,
        state: row.get(4)?,
    })
}

fn has_capacity(conn: &Connection, source: &BridgeSource, bytes: i64) -> io::Result<bool> {
    let (per_source, total, stored_bytes): (i64, i64, i64) = conn
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM bridge_inputs WHERE source_key = ?1),
                COUNT(*), COALESCE(SUM(LENGTH(CAST(payload AS BLOB))), 0)
             FROM bridge_inputs",
            [&source.key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(io::Error::other)?;
    Ok(per_source < MAX_INPUTS_PER_SOURCE
        && total < MAX_INPUTS
        && stored_bytes <= MAX_PAYLOAD_BYTES - bytes)
}

impl SqliteSessionBackend {
    pub(super) fn bridge_resume_impl(&self, source: &BridgeSource) -> io::Result<BridgeResume> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;
        let cursor = ensure_source(&tx, source)?;
        let inputs = {
            let mut query = tx
                .prepare(
                    "SELECT update_id, previous_cursor, request_id, payload, state
                     FROM bridge_inputs WHERE source_key = ?1 ORDER BY update_id",
                )
                .map_err(io::Error::other)?;
            let rows = query
                .query_map([&source.key], read_input)
                .map_err(io::Error::other)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(io::Error::other)?
        };
        tx.commit().map_err(io::Error::other)?;
        Ok(BridgeResume { cursor, inputs })
    }

    pub(super) fn bridge_receipt_impl(
        &self,
        source: &BridgeSource,
        update_id: i64,
    ) -> io::Result<Option<BridgeReceipt>> {
        if update_id < 0 {
            return Err(invalid("invalid bridge update id"));
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(io::Error::other)?;
        let Some(cursor) = source_cursor(&tx, source)? else {
            return Ok(None);
        };
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM bridge_inputs WHERE source_key = ?1 AND update_id = ?2",
                params![source.key, update_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(io::Error::other)?;
        if state.is_none() && update_id < cursor {
            return Err(invalid(
                "bridge update is older than the retained source watermark",
            ));
        }
        Ok(state.map(|state| BridgeReceipt {
            cursor,
            state,
            duplicate: true,
        }))
    }

    pub(super) fn bridge_record_impl(
        &self,
        source: &BridgeSource,
        input: &BridgeInput,
    ) -> io::Result<BridgeReceipt> {
        validate_input(input)?;
        let bytes = i64::try_from(input.payload.len())
            .map_err(|_| invalid("bridge input payload exceeds capacity"))?;
        if bytes > MAX_PAYLOAD_BYTES {
            return Err(invalid("bridge input payload exceeds capacity"));
        }
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;
        let mut cursor = ensure_source(&tx, source)?;
        let known: Option<(BridgeInput, String)> = tx
            .query_row(
                "SELECT update_id, previous_cursor, request_id, payload, state, initial_state
                 FROM bridge_inputs WHERE source_key = ?1 AND update_id = ?2",
                params![source.key, input.update_id],
                |row| Ok((read_input(row)?, row.get(5)?)),
            )
            .optional()
            .map_err(io::Error::other)?;
        if let Some((known, initial_state)) = known {
            if known.previous_cursor != input.previous_cursor
                || known.request_id != input.request_id
                || known.payload != input.payload
                || initial_state != input.state
            {
                return Err(invalid("bridge update conflicts with its persisted input"));
            }
            return Ok(BridgeReceipt {
                cursor,
                state: known.state,
                duplicate: true,
            });
        }
        if input.update_id < cursor || input.previous_cursor < cursor {
            return Err(invalid(
                "bridge update is older than the retained source watermark",
            ));
        }
        let overlaps: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM bridge_inputs
                 WHERE source_key = ?1 AND previous_cursor <= ?2 AND update_id >= ?3)",
                params![source.key, input.update_id, input.previous_cursor],
                |row| row.get(0),
            )
            .map_err(io::Error::other)?;
        if overlaps {
            return Err(invalid("bridge input forks the observed source sequence"));
        }
        if !has_capacity(&tx, source, bytes)? {
            // No pending/running/steered/unknown input can disappear here.
            // The immutable source watermark survives all receipt pruning.
            tx.execute(
                "DELETE FROM bridge_inputs
                 WHERE recorded_at < ?1
                   AND state IN ('done', 'error', 'aborted', 'rejected', 'ignored', 'control')
                   AND update_id < (
                       SELECT cursor FROM bridge_sources
                       WHERE bridge_sources.source_key = bridge_inputs.source_key
                   )",
                [Utc::now().timestamp() - RETAIN_SECONDS],
            )
            .map_err(io::Error::other)?;
            if !has_capacity(&tx, source, bytes)? {
                return Err(io::Error::other(
                    "protected bridge input capacity exhausted",
                ));
            }
        }
        tx.execute(
            "INSERT INTO bridge_inputs
             (source_key, update_id, previous_cursor, request_id, payload, initial_state, state, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
            params![
                source.key,
                input.update_id,
                input.previous_cursor,
                input.request_id,
                input.payload,
                input.state,
                Utc::now().timestamp()
            ],
        )
        .map_err(io::Error::other)?;
        loop {
            let next: Option<i64> = tx
                .query_row(
                    "SELECT update_id FROM bridge_inputs WHERE source_key = ?1 AND previous_cursor = ?2",
                    params![source.key, cursor],
                    |row| row.get(0),
                )
                .optional()
                .map_err(io::Error::other)?;
            let Some(next) = next else { break };
            cursor = next
                .checked_add(1)
                .ok_or_else(|| invalid("invalid bridge source cursor"))?;
        }
        tx.execute(
            "UPDATE bridge_sources SET cursor = ?1 WHERE source_key = ?2",
            params![cursor, source.key],
        )
        .map_err(io::Error::other)?;
        tx.commit().map_err(io::Error::other)?;
        Ok(BridgeReceipt {
            cursor,
            state: input.state.clone(),
            duplicate: false,
        })
    }

    pub(super) fn bridge_claim_impl(
        &self,
        source: &BridgeSource,
        update_id: i64,
    ) -> io::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;
        if source_cursor(&tx, source)?.is_none() {
            return Ok(false);
        }
        let changed = tx
            .execute(
                "UPDATE bridge_inputs SET state = 'running'
                 WHERE source_key = ?1 AND update_id = ?2 AND state = 'pending'",
                params![source.key, update_id],
            )
            .map_err(io::Error::other)?;
        tx.commit().map_err(io::Error::other)?;
        Ok(changed == 1)
    }

    pub(super) fn bridge_finish_impl(
        &self,
        source: &BridgeSource,
        update_id: i64,
        state: &str,
    ) -> io::Result<()> {
        if !matches!(
            state,
            "done" | "error" | "aborted" | "rejected" | "steered" | "outcome_unknown"
        ) {
            return Err(invalid("invalid bridge terminal state"));
        }
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(io::Error::other)?;
        if source_cursor(&tx, source)?.is_none() {
            return Err(invalid("unknown bridge source"));
        }
        let current: Option<String> = tx
            .query_row(
                "SELECT state FROM bridge_inputs WHERE source_key = ?1 AND update_id = ?2",
                params![source.key, update_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(io::Error::other)?;
        match current.as_deref() {
            Some(current) if current == state => return Ok(()),
            Some("pending" | "running") => {}
            _ => {
                return Err(invalid(
                    "bridge input cannot enter the requested terminal state",
                ));
            }
        }
        tx.execute(
            "UPDATE bridge_inputs SET state = ?1 WHERE source_key = ?2 AND update_id = ?3",
            params![state, source.key, update_id],
        )
        .map_err(io::Error::other)?;
        tx.commit().map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_backend::SessionBackend;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;

    fn source() -> BridgeSource {
        BridgeSource {
            key: "telegram:bridge:bot:owner".into(),
            session_key: "gw_main".into(),
            agent_alias: "agent".into(),
        }
    }

    fn input(update_id: i64, previous_cursor: i64) -> BridgeInput {
        BridgeInput {
            update_id,
            previous_cursor,
            request_id: format!("request-{update_id}"),
            payload: format!("input {update_id}"),
            state: "pending".into(),
        }
    }

    #[test]
    fn persisted_input_survives_acceptance_without_an_ack_or_execution() {
        let tmp = TempDir::new().unwrap();
        let source = source();
        let input = input(41, 0);
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            let receipt = store.bridge_record(&source, &input).unwrap();
            assert_eq!(receipt.cursor, 42);
            assert!(!receipt.duplicate);
            // No execution claim or transport ACK occurs before the DB closes.
        }
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let resumed = store.bridge_resume(&source).unwrap();
        assert_eq!(resumed.cursor, 42);
        assert_eq!(resumed.inputs.as_slice(), std::slice::from_ref(&input));
        let duplicate = store.bridge_record(&source, &input).unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.state, "pending");
        assert!(store.bridge_claim(&source, 41).unwrap());
        assert!(!store.bridge_claim(&source, 41).unwrap());
    }

    #[test]
    fn cursor_waits_for_the_observed_prefix_and_allows_numeric_gaps() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        // Telegram's observed sequence can jump from 10 to 25 to 90.
        assert_eq!(
            store.bridge_record(&source, &input(90, 26)).unwrap().cursor,
            0
        );
        assert_eq!(
            store.bridge_record(&source, &input(25, 11)).unwrap().cursor,
            0
        );
        assert_eq!(
            store.bridge_record(&source, &input(10, 0)).unwrap().cursor,
            91
        );
        assert_eq!(store.bridge_resume(&source).unwrap().inputs.len(), 3);
    }

    #[test]
    fn duplicate_compares_original_input_after_state_changes() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        let original = input(9, 0);
        store.bridge_record(&source, &original).unwrap();
        store.bridge_claim(&source, 9).unwrap();
        store.bridge_finish(&source, 9, "done").unwrap();
        assert_eq!(
            store.bridge_record(&source, &original).unwrap().state,
            "done"
        );
        for changed in [
            BridgeInput {
                payload: "different".into(),
                ..original.clone()
            },
            BridgeInput {
                request_id: "other".into(),
                ..original.clone()
            },
            BridgeInput {
                previous_cursor: 1,
                ..original.clone()
            },
            BridgeInput {
                state: "ignored".into(),
                ..original.clone()
            },
        ] {
            assert!(store.bridge_record(&source, &changed).is_err());
        }
        assert_eq!(store.bridge_resume(&source).unwrap().inputs.len(), 1);
    }

    #[test]
    fn observed_sequence_rejects_forks_overlaps_and_request_id_reuse() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        store.bridge_record(&source, &input(20, 11)).unwrap();
        assert!(store.bridge_record(&source, &input(25, 11)).is_err());
        assert!(store.bridge_record(&source, &input(15, 0)).is_err());
        let reused = BridgeInput {
            request_id: "request-20".into(),
            ..input(10, 0)
        };
        assert!(store.bridge_record(&source, &reused).is_err());
        assert_eq!(store.bridge_resume(&source).unwrap().cursor, 0);
        assert_eq!(
            store.bridge_record(&source, &input(10, 0)).unwrap().cursor,
            21
        );
    }

    #[test]
    fn every_operation_checks_immutable_source_scope() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        store.bridge_record(&source, &input(0, 0)).unwrap();
        for foreign in [
            BridgeSource {
                session_key: "gw_other".into(),
                ..source.clone()
            },
            BridgeSource {
                agent_alias: "other".into(),
                ..source.clone()
            },
        ] {
            assert!(store.bridge_resume(&foreign).is_err());
            assert!(store.bridge_receipt(&foreign, 0).is_err());
            assert!(store.bridge_record(&foreign, &input(0, 0)).is_err());
            assert!(store.bridge_claim(&foreign, 0).is_err());
            assert!(store.bridge_finish(&foreign, 0, "done").is_err());
        }
        assert_eq!(
            store.bridge_resume(&source).unwrap().inputs[0].state,
            "pending"
        );
    }

    #[test]
    fn independent_sqlite_handles_cannot_both_claim_one_input() {
        let tmp = TempDir::new().unwrap();
        let source = source();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        first.bridge_record(&source, &input(0, 0)).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let tasks: Vec<_> = [first, second]
            .into_iter()
            .map(|store| {
                let source = source.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store.bridge_claim(&source, 0).unwrap()
                })
            })
            .collect();
        let winners = tasks
            .into_iter()
            .map(|task| usize::from(task.join().unwrap()))
            .sum::<usize>();
        assert_eq!(winners, 1);
    }

    #[test]
    fn reopen_never_replays_running_controls_or_uncertain_inputs() {
        let tmp = TempDir::new().unwrap();
        let source = source();
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            for id in 0..4 {
                let mut input = input(id, id);
                if id == 1 {
                    input.state = "control".into();
                }
                store.bridge_record(&source, &input).unwrap();
            }
            store.bridge_claim(&source, 0).unwrap();
            store.bridge_finish(&source, 2, "steered").unwrap();
            store.bridge_finish(&source, 3, "outcome_unknown").unwrap();
        }
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let states: Vec<_> = store
            .bridge_resume(&source)
            .unwrap()
            .inputs
            .into_iter()
            .map(|i| i.state)
            .collect();
        assert_eq!(states, ["running", "control", "steered", "outcome_unknown"]);
        for id in 0..4 {
            assert!(!store.bridge_claim(&source, id).unwrap());
        }
        assert!(store.bridge_finish(&source, 0, "pending").is_err());
        assert!(store.bridge_finish(&source, 2, "done").is_err());
    }

    #[test]
    fn capacity_preserves_pending_and_uncertain_inputs_but_reclaims_old_settled_rows() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        for id in 0..MAX_INPUTS_PER_SOURCE {
            store.bridge_record(&source, &input(id, id)).unwrap();
        }
        store.bridge_finish(&source, 1, "outcome_unknown").unwrap();
        store.bridge_finish(&source, 2, "steered").unwrap();
        store
            .conn
            .lock()
            .execute("UPDATE bridge_inputs SET recorded_at = 0", [])
            .unwrap();
        let overflow = input(MAX_INPUTS_PER_SOURCE, MAX_INPUTS_PER_SOURCE);
        assert!(store.bridge_record(&source, &overflow).is_err());
        assert_eq!(
            store.bridge_resume(&source).unwrap().inputs.len(),
            usize::try_from(MAX_INPUTS_PER_SOURCE).unwrap()
        );
        store.bridge_finish(&source, 0, "done").unwrap();
        assert_eq!(
            store.bridge_record(&source, &overflow).unwrap().cursor,
            MAX_INPUTS_PER_SOURCE + 1
        );
        let resumed = store.bridge_resume(&source).unwrap();
        assert!(
            resumed
                .inputs
                .iter()
                .any(|i| i.update_id == 1 && i.state == "outcome_unknown")
        );
        assert!(
            resumed
                .inputs
                .iter()
                .any(|i| i.update_id == 2 && i.state == "steered")
        );
        assert!(store.bridge_record(&source, &input(0, 0)).is_err());
        assert!(store.bridge_receipt(&source, 0).is_err());
        // Stale weekly-reset source IDs require explicit reconciliation.
        assert!(store.bridge_record(&source, &input(5, 0)).is_err());
    }

    #[test]
    fn fresh_terminal_receipts_are_protected_and_invalid_states_fail_closed() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        for id in 0..MAX_INPUTS_PER_SOURCE {
            let mut input = input(id, id);
            input.state = "ignored".into();
            store.bridge_record(&source, &input).unwrap();
        }
        assert!(
            store
                .bridge_record(
                    &source,
                    &input(MAX_INPUTS_PER_SOURCE, MAX_INPUTS_PER_SOURCE)
                )
                .is_err()
        );
        assert!(store.bridge_finish(&source, 0, "done").is_err());
        assert!(store.bridge_finish(&source, 99_999, "done").is_err());
        let mut invalid_input = input(1000, 1000);
        invalid_input.state = "running".into();
        assert!(store.bridge_record(&source, &invalid_input).is_err());
    }

    #[test]
    fn global_row_capacity_does_not_move_or_create_a_source_cursor() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        {
            let mut conn = store.conn.lock();
            let tx = conn.transaction().unwrap();
            for group in 0..(MAX_INPUTS / MAX_INPUTS_PER_SOURCE) {
                let key = format!("source-{group}");
                tx.execute(
                    "INSERT INTO bridge_sources (source_key, session_key, agent_alias, cursor)
                     VALUES (?1, 'gw_main', 'agent', ?2)",
                    params![key, MAX_INPUTS_PER_SOURCE],
                )
                .unwrap();
                for id in 0..MAX_INPUTS_PER_SOURCE {
                    tx.execute(
                        "INSERT INTO bridge_inputs
                         (source_key, update_id, previous_cursor, request_id, payload, initial_state, state, recorded_at)
                         VALUES (?1, ?2, ?2, ?3, 'pending input', 'pending', 'pending', 0)",
                        params![key, id, format!("request-{id}")],
                    ).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        let source = source();
        assert!(store.bridge_record(&source, &input(0, 0)).is_err());
        assert!(
            source_cursor(&store.conn.lock(), &source)
                .unwrap()
                .is_none()
        );
        let total: i64 = store
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM bridge_inputs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, MAX_INPUTS);
    }

    #[test]
    fn global_payload_capacity_counts_bytes_and_preserves_pending_input() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        store.bridge_record(&source, &input(0, 0)).unwrap();
        // A valid UTF-8 payload with exactly the store's byte budget. Use a
        // fixture write to avoid another equally large Rust string allocation.
        store
            .conn
            .lock()
            .execute(
                "UPDATE bridge_inputs SET payload = CAST(zeroblob(?1) AS TEXT)",
                [MAX_PAYLOAD_BYTES],
            )
            .unwrap();
        assert!(store.bridge_record(&source, &input(1, 1)).is_err());
        assert_eq!(store.bridge_receipt(&source, 0).unwrap().unwrap().cursor, 1);
        assert!(store.bridge_receipt(&source, 1).unwrap().is_none());
        let bytes: i64 = store
            .conn
            .lock()
            .query_row(
                "SELECT LENGTH(CAST(payload AS BLOB)) FROM bridge_inputs",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bytes, MAX_PAYLOAD_BYTES);
    }

    #[test]
    fn deleting_or_clearing_history_tombstones_intake_and_preserves_watermarks() {
        for delete in [true, false] {
            let tmp = TempDir::new().unwrap();
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            let source = source();
            store
                .append(
                    &source.session_key,
                    &zeroclaw_api::model_provider::ChatMessage::user("history body"),
                )
                .unwrap();
            for id in 0..7 {
                let mut input = input(id, id);
                if id == 5 {
                    input.state = "ignored".into();
                }
                if id == 6 {
                    input.state = "control".into();
                }
                store.bridge_record(&source, &input).unwrap();
            }
            store.bridge_claim(&source, 1).unwrap();
            store.bridge_finish(&source, 2, "steered").unwrap();
            store.bridge_finish(&source, 3, "done").unwrap();
            store.bridge_finish(&source, 4, "outcome_unknown").unwrap();
            if delete {
                assert!(store.delete_session(&source.session_key).unwrap());
            } else {
                assert_eq!(store.clear_messages(&source.session_key).unwrap(), 1);
            }
            assert!(store.load(&source.session_key).is_empty());
            let resumed = store.bridge_resume(&source).unwrap();
            assert_eq!(resumed.cursor, 7);
            assert!(resumed.inputs.iter().all(|i| i.payload.is_empty()));
            let states: Vec<_> = resumed.inputs.iter().map(|i| i.state.as_str()).collect();
            assert_eq!(
                states,
                [
                    "rejected",
                    "outcome_unknown",
                    "outcome_unknown",
                    "done",
                    "outcome_unknown",
                    "ignored",
                    "control"
                ]
            );
            for id in 0..7 {
                assert!(!store.bridge_claim(&source, id).unwrap());
            }
            assert!(store.bridge_record(&source, &input(0, 0)).is_err());
            assert!(store.bridge_finish(&source, 1, "done").is_err());
        }
    }

    #[test]
    fn deleting_a_session_without_metadata_still_erases_pending_input() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        store.bridge_record(&source, &input(7, 0)).unwrap();
        assert!(!store.delete_session(&source.session_key).unwrap());
        let resumed = store.bridge_resume(&source).unwrap();
        assert_eq!(resumed.cursor, 8);
        assert!(resumed.inputs[0].payload.is_empty());
        assert_eq!(resumed.inputs[0].state, "rejected");
        assert!(store.bridge_record(&source, &input(7, 0)).is_err());
    }

    #[test]
    fn ttl_retains_unresolved_intake_and_redacts_settled_session_bodies() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let source = source();
        store
            .set_session_agent_alias(&source.session_key, &source.agent_alias)
            .unwrap();
        store.bridge_record(&source, &input(0, 0)).unwrap();
        store
            .conn
            .lock()
            .execute(
                "UPDATE session_metadata SET last_activity = '2000-01-01T00:00:00+00:00'",
                [],
            )
            .unwrap();
        assert_eq!(store.cleanup_stale(1).unwrap(), 0);
        assert_eq!(
            store.bridge_resume(&source).unwrap().inputs[0].payload,
            "input 0"
        );
        store.bridge_finish(&source, 0, "done").unwrap();
        assert_eq!(store.cleanup_stale(1).unwrap(), 1);
        let resumed = store.bridge_resume(&source).unwrap();
        assert_eq!(resumed.cursor, 1);
        assert_eq!(resumed.inputs[0].state, "done");
        assert!(resumed.inputs[0].payload.is_empty());
    }
}
