//! Scheduler-owned reminder occurrences (#568): the durable intent that a
//! committed task list item deserves attention at a time. The store is owned
//! by the reminder scheduler — the `task_list` tool arms occurrences inside
//! the task batch transaction, and the gateway producer
//! (`reminder_notifications`) is the only writer of `enqueued`/`invalidated`
//! beyond that path. Per ADR-020 §5 this is NOT a column on the task table:
//! the occurrence is the modifiable truth, the task only projects "has a
//! reminder".
//!
//! Fire-once: the occurrence row itself is the durable marker (no eviction);
//! the outbox's `(source_kind, source_id, event_id)` dedup only covers the
//! enqueue→mark crash window. `expires_at = due_at + 24h` is ABSOLUTE — an
//! occurrence first observed by a producer long after its due time is past
//! validity and never enqueues (today's enqueue-relative outbox TTL would
//! otherwise let a month-old reminder open a fresh delivery window).

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use super::agent_tasks::STATUS_COMPLETED;

/// How long past its due time an occurrence stays deliverable (absolute).
pub const REMINDER_VALIDITY_SECS: i64 = 24 * 60 * 60;

pub const OCC_STATE_SCHEDULED: &str = "scheduled";
pub const OCC_STATE_ENQUEUED: &str = "enqueued";
pub const OCC_STATE_INVALIDATED: &str = "invalidated";

/// One armed occurrence. `bridge`/`recipient`/`thread_id` are the
/// destination BOUND AT CREATION — a later config change never rewrites
/// them; removal of the stored bridge withholds delivery instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReminderOccurrence {
    pub occurrence_id: String,
    pub agent_identity: String,
    pub task_id: String,
    pub due_at: i64,
    pub expires_at: i64,
    pub bridge: String,
    pub recipient: String,
    pub thread_id: Option<String>,
    pub state: String,
}

/// A reminder destination plus due time, resolved by the CALLER (the tool
/// layer binds it from the execution context or a configured target) — the
/// store only persists what it is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArmReminder {
    pub due_at: i64,
    pub bridge: String,
    pub recipient: String,
    pub thread_id: Option<String>,
}

pub(super) fn migrate_reminder_occurrences(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "CREATE TABLE IF NOT EXISTS reminder_occurrences (
            occurrence_id  TEXT PRIMARY KEY,
            agent_identity TEXT NOT NULL,
            task_id        TEXT NOT NULL,
            due_at         INTEGER NOT NULL,
            expires_at     INTEGER NOT NULL,
            bridge         TEXT NOT NULL,
            recipient      TEXT NOT NULL,
            thread_id      TEXT,
            state          TEXT NOT NULL,
            created_at     INTEGER NOT NULL,
            enqueued_at    INTEGER
        )",
        [],
    )
    .context("creating reminder_occurrences")?;
    tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_reminder_occurrences_scan
         ON reminder_occurrences(state, due_at)",
        [],
    )
    .context("indexing reminder_occurrences scan")?;
    tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_reminder_occurrences_task
         ON reminder_occurrences(agent_identity, task_id, state)",
        [],
    )
    .context("indexing reminder_occurrences by task")?;
    tx.commit()
        .context("committing reminder_occurrences schema")
}

/// Validate an arm request. Bounded destination strings keep a rendered
/// notice sane; the due time only needs to be a real epoch second.
fn validate_arm(arm: &ArmReminder) -> Result<()> {
    if arm.bridge.is_empty() || arm.bridge.len() > 64 {
        bail!("reminder bridge must be 1..=64 bytes");
    }
    if arm.recipient.is_empty() || arm.recipient.len() > 128 {
        bail!("reminder recipient must be 1..=128 bytes");
    }
    if arm.thread_id.as_deref().is_some_and(|t| t.len() > 128) {
        bail!("reminder thread id must be at most 128 bytes");
    }
    Ok(())
}

/// Invalidate every not-yet-delivered occurrence of one task — `scheduled`
/// AND `enqueued` (an enqueued occurrence still has an unclaimed outbox row;
/// invalidating it lets the delivery claim gate withhold that row). A row
/// already CLAIMED by a bridge socket is past this gate by design: claim
/// made ⇒ no promise of retraction. Called inside the caller's transaction
/// (complete/cancel/delete/re-arm keep their atomicity with the task batch)
/// and standalone by the producer when a re-check finds the task closed.
pub(super) fn invalidate_task_reminders_tx(
    tx: &rusqlite::Transaction<'_>,
    agent_identity: &str,
    task_id: &str,
) -> Result<usize> {
    let invalidated = tx
        .execute(
            "UPDATE reminder_occurrences SET state = ?4
             WHERE agent_identity = ?1 AND task_id = ?2 AND state IN (?3, ?5)",
            params![
                agent_identity,
                task_id,
                OCC_STATE_SCHEDULED,
                OCC_STATE_INVALIDATED,
                OCC_STATE_ENQUEUED
            ],
        )
        .context("invalidating task reminders")?;
    Ok(invalidated)
}

/// Arm ONE new occurrence for a task, replacing any still-scheduled one.
/// Runs inside the caller's task-batch transaction so arming is atomic with
/// the revision-checked write that carries it.
pub(super) fn arm_reminder_tx(
    tx: &rusqlite::Transaction<'_>,
    agent_identity: &str,
    task_id: &str,
    arm: &ArmReminder,
    now: i64,
) -> Result<String> {
    validate_arm(arm)?;
    let occurrence_id = uuid::Uuid::new_v4().to_string();
    // Re-arm replaces: exactly one scheduled occurrence per task, ever the
    // newest generation.
    invalidate_task_reminders_tx(tx, agent_identity, task_id)?;
    tx.execute(
        "INSERT INTO reminder_occurrences
         (occurrence_id, agent_identity, task_id, due_at, expires_at, bridge,
          recipient, thread_id, state, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            occurrence_id,
            agent_identity,
            task_id,
            arm.due_at,
            arm.due_at + REMINDER_VALIDITY_SECS,
            arm.bridge,
            arm.recipient,
            arm.thread_id,
            OCC_STATE_SCHEDULED,
            now
        ],
    )
    .context("inserting a reminder occurrence")?;
    Ok(occurrence_id)
}

impl super::SqliteSessionBackend {
    /// Occurrences whose delivery window is open: due, not yet expired,
    /// still scheduled. The producer's scan input.
    pub fn due_reminder_occurrences(
        &self,
        now: i64,
        limit: usize,
    ) -> Result<Vec<ReminderOccurrence>> {
        anyhow::ensure!(limit > 0, "limit must be positive");
        let conn = self.conn.lock();
        let mut query = conn.prepare(
            "SELECT occurrence_id, agent_identity, task_id, due_at, expires_at,
                    bridge, recipient, thread_id, state
             FROM reminder_occurrences
             WHERE state = ?1 AND due_at <= ?2 AND expires_at > ?2
             ORDER BY due_at LIMIT ?3",
        )?;
        let rows = query
            .query_map(params![OCC_STATE_SCHEDULED, now, limit as i64], |row| {
                Ok(ReminderOccurrence {
                    occurrence_id: row.get(0)?,
                    agent_identity: row.get(1)?,
                    task_id: row.get(2)?,
                    due_at: row.get(3)?,
                    expires_at: row.get(4)?,
                    bridge: row.get(5)?,
                    recipient: row.get(6)?,
                    thread_id: row.get(7)?,
                    state: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether the linked task still exists and is not completed — the
    /// producer re-checks before enqueueing and the delivery gate re-checks
    /// before the send claim. A removed task is closed by definition.
    pub fn reminder_task_open(&self, agent_identity: &str, task_id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let status: Option<String> = conn
            .query_row(
                "SELECT status FROM agent_tasks WHERE agent_identity = ?1 AND task_id = ?2",
                params![agent_identity, task_id],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .context("reading the reminder's task status")?;
        Ok(status.is_some_and(|status| status != STATUS_COMPLETED))
    }

    /// Mark one occurrence enqueued AFTER its outbox row committed. Returns
    /// false when the occurrence is no longer scheduled (raced invalidation
    /// or a duplicate pass) — the already-committed outbox row stays valid
    /// through its dedup identity either way.
    pub fn mark_reminder_enqueued(&self, occurrence_id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let changed = conn
            .execute(
                "UPDATE reminder_occurrences SET state = ?2, enqueued_at = ?3
                 WHERE occurrence_id = ?1 AND state = ?4",
                params![
                    occurrence_id,
                    OCC_STATE_ENQUEUED,
                    chrono::Utc::now().timestamp(),
                    OCC_STATE_SCHEDULED
                ],
            )
            .context("marking a reminder enqueued")?;
        Ok(changed != 0)
    }

    /// Invalidate one not-yet-claimed occurrence (producer re-check found
    /// the task closed, or an explicit cancel outside a task batch). Covers
    /// `scheduled` and `enqueued`; a claimed row is past recall by design.
    pub fn invalidate_reminder(&self, occurrence_id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let changed = conn
            .execute(
                "UPDATE reminder_occurrences SET state = ?2
                 WHERE occurrence_id = ?1 AND state IN (?3, ?4)",
                params![
                    occurrence_id,
                    OCC_STATE_INVALIDATED,
                    OCC_STATE_SCHEDULED,
                    OCC_STATE_ENQUEUED
                ],
            )
            .context("invalidating a reminder occurrence")?;
        Ok(changed != 0)
    }

    /// The delivery-claim gate: hold this store's write transaction (an
    /// IMMEDIATE transaction performs no writes of its own — it is a lock)
    /// across the permission check AND the caller's outbox claim, so a
    /// concurrent complete/cancel — which invalidates occurrences inside
    /// its own IMMEDIATE transaction — cannot commit between them.
    /// Completion before the claim ⇒ no send; claim already made ⇒ no
    /// promise of retraction. The callback must be synchronous (the store
    /// lock is a parking_lot mutex, never held across an await).
    pub fn with_reminder_gate(
        &self,
        occurrence_id: &str,
        now: i64,
        claim: impl FnOnce(&str) -> Result<bool>,
    ) -> Result<bool> {
        let mut guard = self.conn.lock();
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("opening a reminder claim gate transaction")?;
        let permitted: Option<String> = tx
            .query_row(
                "SELECT t.description FROM reminder_occurrences o
                 JOIN agent_tasks t
                   ON t.agent_identity = o.agent_identity AND t.task_id = o.task_id
                 WHERE o.occurrence_id = ?1
                   AND o.state IN (?2, ?3)
                   AND o.expires_at > ?4
                   AND t.status != ?5",
                params![
                    occurrence_id,
                    OCC_STATE_SCHEDULED,
                    OCC_STATE_ENQUEUED,
                    now,
                    STATUS_COMPLETED
                ],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .context("checking a reminder's delivery permission")?;
        let Some(description) = permitted else {
            // An unpermitted occurrence whose linked task is now missing or
            // completed is terminal — retire it out of `scheduled` so the
            // due-occurrence scan is not blocked by stale rows.
            tx.execute(
                "UPDATE reminder_occurrences SET state = ?2
                 WHERE occurrence_id = ?1 AND state IN (?3, ?4)",
                params![
                    occurrence_id,
                    OCC_STATE_INVALIDATED,
                    OCC_STATE_SCHEDULED,
                    OCC_STATE_ENQUEUED
                ],
            )
            .context("invalidating an unpermitted reminder occurrence")?;
            tx.commit()
                .context("committing reminder gate invalidation")?;
            return Ok(false);
        };
        super::agent_tasks::validate_description(&description)
            .context("invalid stored task description")?;
        let claimed = claim(&description)?;
        tx.commit()
            .context("closing a reminder claim gate transaction")?;
        Ok(claimed)
    }

    /// The scheduled occurrence of a task, if any — the task list's
    /// "has a reminder" projection.
    pub fn scheduled_reminder_for_task(
        &self,
        agent_identity: &str,
        task_id: &str,
    ) -> Result<Option<ReminderOccurrence>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT occurrence_id, agent_identity, task_id, due_at, expires_at,
                        bridge, recipient, thread_id, state
                 FROM reminder_occurrences
                 WHERE agent_identity = ?1 AND task_id = ?2 AND state = ?3",
                params![agent_identity, task_id, OCC_STATE_SCHEDULED],
                |row| {
                    Ok(ReminderOccurrence {
                        occurrence_id: row.get(0)?,
                        agent_identity: row.get(1)?,
                        task_id: row.get(2)?,
                        due_at: row.get(3)?,
                        expires_at: row.get(4)?,
                        bridge: row.get(5)?,
                        recipient: row.get(6)?,
                        thread_id: row.get(7)?,
                        state: row.get(8)?,
                    })
                },
            )
            .map(Some)
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
            .context("reading a task's scheduled reminder")?;
        Ok(row)
    }
}

/// The reminder side of one task batch: keep / clear / (re)arm. `Keep` is
/// the absence of any reminder field — description edits must not disturb
/// an armed reminder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReminderChange {
    Keep,
    Clear,
    Arm(ArmReminder),
}

pub(super) fn apply_reminder_change_tx(
    tx: &rusqlite::Transaction<'_>,
    agent_identity: &str,
    task_id: &str,
    change: &ReminderChange,
    task_is_now_completed: bool,
    now: i64,
) -> Result<()> {
    match change {
        ReminderChange::Keep => {
            if task_is_now_completed {
                // Completing a task invalidates its pending reminders; a
                // later reopen never revives them.
                invalidate_task_reminders_tx(tx, agent_identity, task_id)?;
            }
            Ok(())
        }
        ReminderChange::Clear => {
            invalidate_task_reminders_tx(tx, agent_identity, task_id)?;
            Ok(())
        }
        ReminderChange::Arm(arm) => {
            // Arming a completed task is refused by the tool layer; the
            // store still refuses to arm a task the batch just completed.
            anyhow::ensure!(
                !task_is_now_completed,
                "cannot arm a reminder on a completed task"
            );
            arm_reminder_tx(tx, agent_identity, task_id, arm, now)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_sqlite::SqliteSessionBackend;
    use crate::session_sqlite::agent_tasks::{ApplyOutcome, TaskOp};

    fn backend() -> (tempfile::TempDir, SqliteSessionBackend) {
        let tmp = tempfile::tempdir().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        (tmp, backend)
    }

    fn add_task(backend: &SqliteSessionBackend, description: &str) -> String {
        match backend
            .apply_task_ops(
                "owner",
                0,
                &[TaskOp::Add {
                    description: description.to_string(),
                    status: "pending".to_string(),
                    reminder: None,
                }],
            )
            .unwrap()
        {
            ApplyOutcome::Applied(list) => list.items[0].task_id.clone(),
            ApplyOutcome::Conflict(_) => panic!("unexpected conflict"),
        }
    }

    fn arm(due_at: i64) -> ArmReminder {
        ArmReminder {
            due_at,
            bridge: "tg".to_string(),
            recipient: "owner".to_string(),
            thread_id: None,
        }
    }

    #[test]
    fn arm_invalidate_and_expiry_window() {
        let (_tmp, backend) = backend();
        let task = add_task(&backend, "call the vet");
        let now = 1_000_000;
        let mut guard = backend.conn.lock();
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let occurrence = arm_reminder_tx(&tx, "owner", &task, &arm(now + 60), now).unwrap();
        tx.commit().unwrap();
        drop(guard);
        // Before due: not returned.
        assert!(
            backend
                .due_reminder_occurrences(now, 10)
                .unwrap()
                .is_empty()
        );
        // Due and within validity: returned exactly once per scan.
        let due = backend.due_reminder_occurrences(now + 61, 10).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].occurrence_id, occurrence);
        assert_eq!(due[0].expires_at, now + 60 + REMINDER_VALIDITY_SECS);
        // Past the ABSOLUTE expiry: never again, even long after.
        assert!(
            backend
                .due_reminder_occurrences(now + 60 + REMINDER_VALIDITY_SECS + 1, 10)
                .unwrap()
                .is_empty(),
            "an expired occurrence must never re-enter the scan"
        );
    }

    #[test]
    fn rearm_replaces_and_invalidate_is_terminal() {
        let (_tmp, backend) = backend();
        let task = add_task(&backend, "renew passport");
        let now = 500;
        let arm_once = |due: i64| {
            let mut guard = backend.conn.lock();
            let tx = guard
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let id = arm_reminder_tx(&tx, "owner", &task, &arm(due), now).unwrap();
            tx.commit().unwrap();
            drop(guard);
            id
        };
        let first = arm_once(1_000);
        let second = arm_once(2_000);
        assert_ne!(first, second, "re-arm mints a new generation");
        let scheduled = backend.scheduled_reminder_for_task("owner", &task).unwrap();
        assert_eq!(scheduled.unwrap().occurrence_id, second);
        // Invalidate is terminal: a reopen must not revive either generation.
        assert!(backend.invalidate_reminder(&second).unwrap());
        assert!(!backend.invalidate_reminder(&second).unwrap());
        assert!(
            backend
                .scheduled_reminder_for_task("owner", &task)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn add_completed_with_reminder_rolls_back_the_batch() {
        // R2 P2 regression: add(status=completed, remind_at) used to arm
        // silently; a later reopen-to-pending (reminder Keep) then fired
        // the stale occurrence.
        let (_tmp, backend) = backend();
        let arm = ArmReminder {
            due_at: 9_000_000,
            bridge: "tg".to_string(),
            recipient: "owner".to_string(),
            thread_id: None,
        };
        let outcome = backend.apply_task_ops(
            "owner",
            0,
            &[
                TaskOp::Add {
                    description: "done already".to_string(),
                    status: STATUS_COMPLETED.to_string(),
                    reminder: Some(arm),
                },
                TaskOp::Add {
                    description: "innocent sibling".to_string(),
                    status: "pending".to_string(),
                    reminder: None,
                },
            ],
        );
        assert!(
            outcome.is_err(),
            "arming on a completed add must fail the batch"
        );
        // The whole batch rolled back: no task, no revision, no occurrence.
        let list = backend.task_list("owner").unwrap();
        assert_eq!(list.revision, 0);
        assert!(list.items.is_empty());
        assert!(
            backend
                .due_reminder_occurrences(9_000_001, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn mark_enqueued_races_and_task_open() {
        let (_tmp, backend) = backend();
        let task = add_task(&backend, "ship the report");
        let now = 7_000;
        let mut guard = backend.conn.lock();
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let occurrence = arm_reminder_tx(&tx, "owner", &task, &arm(now), now).unwrap();
        tx.commit().unwrap();
        drop(guard);
        assert!(backend.reminder_task_open("owner", &task).unwrap());
        assert!(backend.mark_reminder_enqueued(&occurrence).unwrap());
        assert!(!backend.mark_reminder_enqueued(&occurrence).unwrap());
        // Completing the task closes it for reminders; removal does too.
        let _ = backend.apply_task_ops(
            "owner",
            1,
            &[TaskOp::Update {
                task_id: task.clone(),
                description: None,
                status: Some("completed".to_string()),
                reminder: ReminderChange::Keep,
            }],
        );
        assert!(!backend.reminder_task_open("owner", &task).unwrap());
        let _ = backend.apply_task_ops("owner", 2, &[TaskOp::Remove { task_id: task }]);
        assert!(!backend.reminder_task_open("owner", "missing").unwrap());
    }

    #[test]
    fn gate_retires_orphaned_scheduled_occurrence() {
        let (_tmp, backend) = backend();
        let now = 8_000;
        let mut guard = backend.conn.lock();
        let tx = guard
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let occurrence = arm_reminder_tx(&tx, "owner", "missing-task-id", &arm(now), now).unwrap();
        tx.commit().unwrap();
        drop(guard);
        assert_eq!(backend.due_reminder_occurrences(now, 10).unwrap().len(), 1);
        let permitted = backend
            .with_reminder_gate(&occurrence, now, |_| Ok(true))
            .unwrap();
        assert!(!permitted);
        // The orphaned scheduled occurrence was invalidated by the gate so it
        // no longer appears in due_reminder_occurrences.
        assert!(
            backend
                .due_reminder_occurrences(now, 10)
                .unwrap()
                .is_empty()
        );
    }
}
