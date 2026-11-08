//! Durable agent task list (#567): cross-session commitments owned by the
//! companion agent identity (a mint-once UUID string, resolved by the caller —
//! the store is identity-agnostic). Item identity is a server-minted UUID,
//! never reused; every mutation batch is transactional and checked against
//! the owner's monotonic list revision.

use super::SqliteSessionBackend;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

/// Hard bounds keep a whole list renderable in one prompt section.
pub const MAX_ITEMS: usize = 32;
pub const MAX_DESCRIPTION_BYTES: usize = 512;

pub const STATUS_PENDING: &str = "pending";
pub const STATUS_IN_PROGRESS: &str = "in_progress";
pub const STATUS_COMPLETED: &str = "completed";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskItem {
    pub task_id: String,
    pub ordinal: i64,
    pub description: String,
    pub status: String,
    /// The task's scheduled reminder due time, when one is armed (the
    /// "has a reminder" projection; the occurrence stays the truth).
    pub remind_at: Option<i64>,
}

/// One batched operation. Adds mint identity server-side; updates and
/// removes address items by their stable id — never by description.
/// The optional reminder field arms/clears the task's reminder occurrence
/// in the SAME transaction (see `reminder_occurrences`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOp {
    Add {
        description: String,
        status: String,
        reminder: Option<super::reminder_occurrences::ArmReminder>,
    },
    Update {
        task_id: String,
        description: Option<String>,
        status: Option<String>,
        reminder: super::reminder_occurrences::ReminderChange,
    },
    Remove {
        task_id: String,
    },
}

pub struct TaskList {
    pub revision: i64,
    pub items: Vec<TaskItem>,
}

pub enum ApplyOutcome {
    Applied(TaskList),
    /// The caller's `expected_revision` is stale; the current state is
    /// returned for the model to re-judge. No write happened.
    Conflict(TaskList),
}

pub(super) fn migrate_agent_tasks(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "CREATE TABLE IF NOT EXISTS agent_tasks (
            agent_identity TEXT NOT NULL,
            task_id        TEXT NOT NULL,
            ordinal        INTEGER NOT NULL,
            description    TEXT NOT NULL,
            status         TEXT NOT NULL,
            created_at     INTEGER NOT NULL,
            updated_at     INTEGER NOT NULL,
            PRIMARY KEY (agent_identity, task_id)
        )",
        [],
    )
    .context("creating agent_tasks")?;
    tx.execute(
        "CREATE TABLE IF NOT EXISTS agent_task_revisions (
            agent_identity TEXT PRIMARY KEY,
            revision       INTEGER NOT NULL
        )",
        [],
    )
    .context("creating agent_task_revisions")?;
    tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_agent_tasks_owner_order
         ON agent_tasks(agent_identity, ordinal)",
        [],
    )
    .context("indexing agent_tasks order")?;
    tx.commit().context("committing agent_tasks schema")
}

pub(super) fn validate_description(description: &str) -> Result<()> {
    if description.is_empty() || description.len() > MAX_DESCRIPTION_BYTES {
        bail!(
            "task description must be 1..={MAX_DESCRIPTION_BYTES} bytes, got {}",
            description.len()
        );
    }
    Ok(())
}

fn validate_status(status: &str) -> Result<()> {
    if !matches!(
        status,
        STATUS_PENDING | STATUS_IN_PROGRESS | STATUS_COMPLETED
    ) {
        bail!("task status must be pending|in_progress|completed, got {status:?}");
    }
    Ok(())
}

impl SqliteSessionBackend {
    /// The committed list for an owner, ordered by ordinal. The revision
    /// and items are read inside ONE read transaction so a concurrent
    /// writer cannot split them.
    pub fn task_list(&self, agent_identity: &str) -> Result<TaskList> {
        let mut conn = self.conn.lock();
        read_committed_list(&mut conn, agent_identity)
    }

    /// Read an already-initialized store without any schema, journal-mode or
    /// filesystem setup. Prompt projection uses this per request; startup and
    /// mutation owners retain `new()` and its migrations. No handle or list is
    /// cached, so the next call observes newly committed state.
    ///
    /// Missing/old/corrupt stores are errors, never fabricated empty lists.
    /// A WAL writer does not block this read. Other lock contention fails
    /// immediately so a synchronous prompt render cannot spend the normal
    /// five-second initialization timeout blocking an executor worker.
    pub fn read_task_list(
        workspace_dir: &std::path::Path,
        agent_identity: &str,
    ) -> Result<TaskList> {
        let mut conn = Connection::open_with_flags(
            Self::database_path(workspace_dir),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .context("opening the existing task store for projection")?;
        conn.busy_timeout(std::time::Duration::ZERO)
            .context("setting the task projection read timeout")?;
        read_committed_list(&mut conn, agent_identity)
    }

    /// Apply one batched, transactional mutation. The caller must echo a
    /// non-negative revision from the last read or conflict response; there
    /// is no unchecked write or automatic repair of corrupt stored state.
    pub fn apply_task_ops(
        &self,
        agent_identity: &str,
        expected_revision: i64,
        ops: &[TaskOp],
    ) -> Result<ApplyOutcome> {
        anyhow::ensure!(
            expected_revision >= 0,
            "expected_revision must be non-negative"
        );
        if ops.len() > MAX_ITEMS {
            bail!("a batch may carry at most {MAX_ITEMS} operations");
        }
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("opening agent_tasks transaction")?;
        let TaskList {
            revision,
            items: list,
        } = read_list(&tx, agent_identity)?;
        if expected_revision != revision {
            return Ok(ApplyOutcome::Conflict(TaskList {
                revision,
                items: list,
            }));
        }
        let next_revision = revision
            .checked_add(1)
            .context("task list revision exhausted")?;
        // The cap applies to the POST-BATCH size: removes in the same batch
        // free room for adds (a legal replacement stays one transaction).
        let adds = ops
            .iter()
            .filter(|op| matches!(op, TaskOp::Add { .. }))
            .count();
        let removes = ops
            .iter()
            .filter(|op| matches!(op, TaskOp::Remove { .. }))
            .count();
        if list.len() + adds > MAX_ITEMS + removes {
            bail!("the list is capped at {MAX_ITEMS} items");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        for op in ops {
            match op {
                TaskOp::Add {
                    description,
                    status,
                    reminder,
                } => {
                    validate_description(description)?;
                    validate_status(status)?;
                    let task_id = uuid::Uuid::new_v4().to_string();
                    let next_ordinal = next_ordinal(&tx, agent_identity)?;
                    tx.execute(
                        "INSERT INTO agent_tasks
                         (agent_identity, task_id, ordinal, description, status, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                        params![agent_identity, task_id, next_ordinal, description, status, now],
                    )
                    .context("inserting an agent task")?;
                    if let Some(arm) = reminder {
                        // An Add MAY carry status=completed (the tool schema
                        // allows it); arming onto it is refused exactly like
                        // an Update would be, rolling back the WHOLE batch.
                        anyhow::ensure!(
                            status != STATUS_COMPLETED,
                            "cannot arm a reminder on a completed task"
                        );
                        super::reminder_occurrences::arm_reminder_tx(
                            &tx,
                            agent_identity,
                            &task_id,
                            arm,
                            now,
                        )?;
                    }
                }
                TaskOp::Update {
                    task_id,
                    description,
                    status,
                    reminder,
                } => {
                    if let Some(description) = description {
                        validate_description(description)?;
                    }
                    if let Some(status) = status {
                        validate_status(status)?;
                    }
                    let updated = tx
                        .execute(
                            "UPDATE agent_tasks SET description = COALESCE(?3, description),
                             status = COALESCE(?4, status), updated_at = ?5
                             WHERE agent_identity = ?1 AND task_id = ?2",
                            params![
                                agent_identity,
                                task_id,
                                description.as_deref(),
                                status.as_deref(),
                                now
                            ],
                        )
                        .context("updating an agent task")?;
                    if updated == 0 {
                        bail!("unknown task id {task_id}");
                    }
                    // The reminder change is judged against the task's
                    // status AFTER this update: completing in the same batch
                    // invalidates; arming onto a completed task is refused.
                    let now_status: String = tx
                        .query_row(
                            "SELECT status FROM agent_tasks
                             WHERE agent_identity = ?1 AND task_id = ?2",
                            params![agent_identity, task_id],
                            |row| row.get(0),
                        )
                        .context("re-reading the updated task status")?;
                    super::reminder_occurrences::apply_reminder_change_tx(
                        &tx,
                        agent_identity,
                        task_id,
                        reminder,
                        now_status == STATUS_COMPLETED,
                        now,
                    )?;
                }
                TaskOp::Remove { task_id } => {
                    let removed = tx
                        .execute(
                            "DELETE FROM agent_tasks WHERE agent_identity = ?1 AND task_id = ?2",
                            params![agent_identity, task_id],
                        )
                        .context("removing an agent task")?;
                    if removed == 0 {
                        bail!("unknown task id {task_id}");
                    }
                    // Deleting the task invalidates its pending reminders in
                    // the same transaction; a reopen cannot revive them.
                    super::reminder_occurrences::invalidate_task_reminders_tx(
                        &tx,
                        agent_identity,
                        task_id,
                    )?;
                }
            }
        }
        tx.execute(
            "INSERT INTO agent_task_revisions (agent_identity, revision) VALUES (?1, ?2)
             ON CONFLICT(agent_identity) DO UPDATE SET revision = excluded.revision",
            params![agent_identity, next_revision],
        )
        .context("bumping the task list revision")?;
        let items = read_items(&tx, agent_identity)?;
        tx.commit().context("committing agent task operations")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(ApplyOutcome::Applied(TaskList {
            revision: next_revision,
            items,
        }))
    }
}

fn read_committed_list(conn: &mut Connection, agent_identity: &str) -> Result<TaskList> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Deferred)
        .context("opening a task list read")?;
    let list = read_list(&tx, agent_identity)?;
    tx.commit().context("closing a task list read")?;
    Ok(list)
}

fn read_list(tx: &rusqlite::Transaction, agent_identity: &str) -> Result<TaskList> {
    let revision: Option<i64> = tx
        .query_row(
            "SELECT revision FROM agent_task_revisions WHERE agent_identity = ?1",
            params![agent_identity],
            |row| row.get(0),
        )
        .optional()
        .context("reading task list revision")?;
    if let Some(revision) = revision {
        anyhow::ensure!(revision > 0, "stored task list revision must be positive");
    }
    let items = read_items(tx, agent_identity)?;
    anyhow::ensure!(
        revision.is_some() || items.is_empty(),
        "stored task items have no list revision"
    );
    Ok(TaskList {
        revision: revision.unwrap_or(0),
        items,
    })
}

fn read_items(tx: &rusqlite::Transaction, agent_identity: &str) -> Result<Vec<TaskItem>> {
    let mut query = tx.prepare(
        "SELECT a.task_id, a.ordinal, a.description, a.status, r.due_at
         FROM agent_tasks a
         LEFT JOIN reminder_occurrences r
           ON r.agent_identity = a.agent_identity AND r.task_id = a.task_id
          AND r.state = 'scheduled'
         WHERE a.agent_identity = ?1 ORDER BY a.ordinal, a.rowid LIMIT ?2",
    )?;
    let items = query
        .query_map(params![agent_identity, MAX_ITEMS + 1], |row| {
            Ok(TaskItem {
                task_id: row.get(0)?,
                ordinal: row.get(1)?,
                description: row.get(2)?,
                status: row.get(3)?,
                remind_at: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        items.len() <= MAX_ITEMS,
        "stored task list exceeds {MAX_ITEMS} items"
    );
    for item in &items {
        uuid::Uuid::parse_str(&item.task_id).context("stored task id is not a UUID")?;
        anyhow::ensure!(item.ordinal > 0, "stored task ordinal must be positive");
        validate_description(&item.description).context("invalid stored task description")?;
        validate_status(&item.status).context("invalid stored task status")?;
    }
    Ok(items)
}

fn next_ordinal(tx: &rusqlite::Transaction, agent_identity: &str) -> Result<i64> {
    let max: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(ordinal), 0) FROM agent_tasks WHERE agent_identity = ?1",
            params![agent_identity],
            |row| row.get(0),
        )
        .context("reading task ordinal")?;
    max.checked_add(1).context("task ordinal exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend() -> (tempfile::TempDir, SqliteSessionBackend) {
        let tmp = tempfile::tempdir().unwrap();
        let backend = SqliteSessionBackend::new(tmp.path()).unwrap();
        (tmp, backend)
    }

    fn add(backend: &SqliteSessionBackend, owner: &str, description: &str) -> TaskList {
        match backend
            .apply_task_ops(
                owner,
                0,
                &[TaskOp::Add {
                    description: description.into(),
                    status: STATUS_PENDING.into(),
                    reminder: None,
                }],
            )
            .unwrap()
        {
            ApplyOutcome::Applied(list) => list,
            ApplyOutcome::Conflict(_) => panic!("first write must apply"),
        }
    }

    #[test]
    fn projection_does_not_create_or_migrate_a_store() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        assert!(SqliteSessionBackend::read_task_list(&missing, "owner-a").is_err());
        assert!(
            !missing.exists(),
            "reading must not create storage directories"
        );

        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let old = Connection::open(sessions.join("sessions.db")).unwrap();
        old.execute_batch(
            "CREATE TABLE legacy_marker (value TEXT); INSERT INTO legacy_marker VALUES ('keep');",
        )
        .unwrap();
        assert!(SqliteSessionBackend::read_task_list(tmp.path(), "owner-a").is_err());
        let tables: i64 = old
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "projection must not run schema setup");
        let journal: String = old
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal, "delete", "projection must not change journal mode");
        drop(old);

        // The existing startup/mutation owner still performs initialization.
        let _initialized = SqliteSessionBackend::new(tmp.path()).unwrap();
        let empty = SqliteSessionBackend::read_task_list(tmp.path(), "owner-a").unwrap();
        assert_eq!(empty.revision, 0);
        assert!(empty.items.is_empty());
    }

    #[test]
    fn projection_reads_committed_owner_snapshot_while_a_wal_writer_is_active() {
        let (tmp, backend) = backend();
        add(&backend, "owner-a", "committed-a");
        add(&backend, "owner-b", "committed-b");
        let writer = Connection::open(SqliteSessionBackend::database_path(tmp.path())).unwrap();
        writer
            .execute_batch(
                "BEGIN IMMEDIATE;
             UPDATE agent_tasks SET description = 'next-commit' WHERE agent_identity = 'owner-a';
             UPDATE agent_task_revisions SET revision = 2 WHERE agent_identity = 'owner-a';",
            )
            .unwrap();

        let before = SqliteSessionBackend::read_task_list(tmp.path(), "owner-a").unwrap();
        assert_eq!(before.revision, 1);
        assert_eq!(before.items[0].description, "committed-a");
        let other = SqliteSessionBackend::read_task_list(tmp.path(), "owner-b").unwrap();
        assert_eq!(other.revision, 1);
        assert_eq!(other.items[0].description, "committed-b");

        writer.execute_batch("COMMIT").unwrap();
        let after = SqliteSessionBackend::read_task_list(tmp.path(), "owner-a").unwrap();
        assert_eq!(after.revision, 2);
        assert_eq!(after.items[0].description, "next-commit");
    }

    #[test]
    fn revision_conflicts_reject_stale_writes_without_losing_updates() {
        let (_tmp, backend) = backend();
        let first = add(&backend, "owner-a", "one");
        assert_eq!(first.revision, 1);
        // A stale writer against revision 0 conflicts and writes nothing.
        match backend
            .apply_task_ops(
                "owner-a",
                0,
                &[TaskOp::Add {
                    description: "stale".into(),
                    status: STATUS_PENDING.into(),
                    reminder: None,
                }],
            )
            .unwrap()
        {
            ApplyOutcome::Conflict(current) => {
                assert_eq!(current.revision, 1);
                assert_eq!(current.items.len(), 1);
            }
            ApplyOutcome::Applied(_) => panic!("stale revision must conflict"),
        }
        // The current writer succeeds against revision 1.
        match backend
            .apply_task_ops(
                "owner-a",
                1,
                &[TaskOp::Add {
                    description: "two".into(),
                    status: STATUS_IN_PROGRESS.into(),
                    reminder: None,
                }],
            )
            .unwrap()
        {
            ApplyOutcome::Applied(list) => {
                assert_eq!(list.revision, 2);
                assert_eq!(list.items.len(), 2);
            }
            ApplyOutcome::Conflict(_) => panic!("current revision must apply"),
        }
    }

    #[test]
    fn corrupt_revisions_never_become_initial_revision_zero() {
        use rusqlite::types::Value;

        for corrupt in [
            Value::Text("corrupt revision".into()),
            Value::Real(1.5),
            Value::Integer(-1),
            Value::Integer(0),
        ] {
            let (_tmp, backend) = backend();
            add(&backend, "owner-a", "keep");
            backend
                .conn
                .lock()
                .execute(
                    "UPDATE agent_task_revisions SET revision = ?1",
                    params![corrupt],
                )
                .unwrap();

            assert!(backend.task_list("owner-a").is_err());
            assert!(
                backend
                    .apply_task_ops(
                        "owner-a",
                        0,
                        &[TaskOp::Add {
                            description: "must not be written".into(),
                            status: STATUS_PENDING.into(),
                            reminder: None,
                        }],
                    )
                    .is_err()
            );
            let conn = backend.conn.lock();
            let (revision, count): (Value, i64) = conn
                .query_row(
                    "SELECT revision, (SELECT COUNT(*) FROM agent_tasks)
                     FROM agent_task_revisions",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(
                revision, corrupt,
                "a corrupt revision must not be rewritten"
            );
            assert_eq!(count, 1, "a failed read must not admit a write");
        }
    }

    #[test]
    fn revision_query_error_text_is_not_treated_as_a_missing_row() {
        let (_tmp, backend) = backend();
        add(&backend, "owner-a", "keep");
        backend
            .conn
            .lock()
            .execute_batch(
                "ALTER TABLE agent_task_revisions RENAME TO saved_task_revisions;
                 CREATE VIEW agent_task_revisions AS
                 SELECT agent_identity, revision FROM \"no revision row\";",
            )
            .unwrap();

        // The SQLite error deliberately contains the former sentinel text.
        // It is a failed query, not QueryReturnedNoRows.
        let Err(error) = backend.task_list("owner-a") else {
            panic!("read must fail");
        };
        assert!(format!("{error:#}").contains("no revision row"));
        assert!(
            backend
                .apply_task_ops(
                    "owner-a",
                    0,
                    &[TaskOp::Add {
                        description: "must not be written".into(),
                        status: STATUS_PENDING.into(),
                        reminder: None,
                    }],
                )
                .is_err()
        );
        let conn = backend.conn.lock();
        let (revision, count): (i64, i64) = conn
            .query_row(
                "SELECT revision, (SELECT COUNT(*) FROM agent_tasks)
                 FROM saved_task_revisions",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((revision, count), (1, 1));
    }

    #[test]
    fn only_an_empty_list_can_have_no_revision_row() {
        let (_tmp, backend) = backend();
        let empty = backend.task_list("owner-a").unwrap();
        assert_eq!(empty.revision, 0);
        assert!(empty.items.is_empty());
        let first = add(&backend, "owner-a", "keep");
        backend
            .conn
            .lock()
            .execute("DELETE FROM agent_task_revisions", [])
            .unwrap();

        assert!(backend.task_list("owner-a").is_err());
        assert!(
            backend
                .apply_task_ops(
                    "owner-a",
                    0,
                    &[TaskOp::Remove {
                        task_id: first.items[0].task_id.clone(),
                    }],
                )
                .is_err()
        );
        let count: i64 = backend
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM agent_tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn corrupt_task_rows_are_neither_projected_nor_mutated() {
        use rusqlite::types::Value;

        for (column, value) in [
            ("task_id", Value::Text("invalid task id".into())),
            ("ordinal", Value::Integer(0)),
            ("description", Value::Text(String::new())),
            (
                "description",
                Value::Text("x".repeat(MAX_DESCRIPTION_BYTES + 1)),
            ),
            ("status", Value::Text("invalid status".into())),
        ] {
            let (_tmp, backend) = backend();
            let first = add(&backend, "owner-a", "keep");
            backend
                .conn
                .lock()
                .execute(
                    &format!("UPDATE agent_tasks SET {column} = ?1"),
                    params![value],
                )
                .unwrap();

            assert!(backend.task_list("owner-a").is_err(), "{column}");
            assert!(
                backend
                    .apply_task_ops(
                        "owner-a",
                        first.revision,
                        &[TaskOp::Add {
                            description: "must not be written".into(),
                            status: STATUS_PENDING.into(),
                            reminder: None,
                        }],
                    )
                    .is_err(),
                "{column}"
            );
            let conn = backend.conn.lock();
            let (revision, count): (i64, i64) = conn
                .query_row(
                    "SELECT revision, (SELECT COUNT(*) FROM agent_tasks)
                     FROM agent_task_revisions",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((revision, count), (first.revision, 1));
        }
    }

    #[test]
    fn overfull_stored_list_is_rejected_without_silent_truncation() {
        let (_tmp, backend) = backend();
        let first = add(&backend, "owner-a", "keep");
        {
            let conn = backend.conn.lock();
            for ordinal in 2..=MAX_ITEMS + 1 {
                conn.execute(
                    "INSERT INTO agent_tasks
                     (agent_identity, task_id, ordinal, description, status, created_at, updated_at)
                     VALUES ('owner-a', ?1, ?2, 'extra', 'pending', 0, 0)",
                    params![uuid::Uuid::new_v4().to_string(), ordinal],
                )
                .unwrap();
            }
        }
        assert!(backend.task_list("owner-a").is_err());
        assert!(
            backend
                .apply_task_ops(
                    "owner-a",
                    first.revision,
                    &[TaskOp::Remove {
                        task_id: first.items[0].task_id.clone(),
                    }],
                )
                .is_err()
        );
        let count: usize = backend
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM agent_tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, MAX_ITEMS + 1);
    }

    #[test]
    fn exhausted_counters_reject_writes_without_partial_changes() {
        for counter in ["revision", "ordinal"] {
            let (_tmp, backend) = backend();
            add(&backend, "owner-a", "keep");
            let table = if counter == "revision" {
                "agent_task_revisions"
            } else {
                "agent_tasks"
            };
            backend
                .conn
                .lock()
                .execute(
                    &format!("UPDATE {table} SET {counter} = ?1"),
                    params![i64::MAX],
                )
                .unwrap();
            let before = backend.task_list("owner-a").unwrap();
            let Err(error) = backend.apply_task_ops(
                "owner-a",
                before.revision,
                &[TaskOp::Add {
                    description: "must not be written".into(),
                    status: STATUS_PENDING.into(),
                    reminder: None,
                }],
            ) else {
                panic!("counter exhaustion must fail");
            };
            assert!(error.to_string().contains("exhausted"));
            let after = backend.task_list("owner-a").unwrap();
            assert_eq!(after.revision, before.revision);
            assert_eq!(after.items, before.items);
        }
    }

    #[test]
    fn identity_scopes_lists_and_ids_stay_stable_across_edits() {
        let (_tmp, backend) = backend();
        let a = add(&backend, "owner-a", "shared description");
        let b = add(&backend, "owner-b", "shared description");
        assert_ne!(a.items[0].task_id, b.items[0].task_id, "per-owner ids");
        let task_id = a.items[0].task_id.clone();
        match backend
            .apply_task_ops(
                "owner-a",
                a.revision,
                &[TaskOp::Update {
                    task_id: task_id.clone(),
                    description: Some("renamed".into()),
                    status: Some(STATUS_COMPLETED.into()),
                    reminder: super::super::reminder_occurrences::ReminderChange::Keep,
                }],
            )
            .unwrap()
        {
            ApplyOutcome::Applied(list) => {
                assert_eq!(list.items[0].task_id, task_id, "id survives edits");
                assert_eq!(list.items[0].description, "renamed");
                assert_eq!(list.items[0].status, STATUS_COMPLETED);
            }
            ApplyOutcome::Conflict(_) => panic!(""),
        }
        // The same id under the OTHER owner is unknown — no cross-owner leak.
        assert!(
            backend
                .apply_task_ops("owner-b", b.revision, &[TaskOp::Remove { task_id }],)
                .is_err()
        );
    }

    #[test]
    fn caps_and_validation_reject_cleanly() {
        let (_tmp, backend) = backend();
        assert!(
            backend
                .apply_task_ops(
                    "owner-a",
                    0,
                    &[TaskOp::Add {
                        description: "".into(),
                        status: STATUS_PENDING.into(),
                        reminder: None,
                    }],
                )
                .is_err()
        );
        assert!(
            backend
                .apply_task_ops(
                    "owner-a",
                    0,
                    &[TaskOp::Add {
                        description: "ok".into(),
                        status: "done".into(),
                        reminder: None,
                    }],
                )
                .is_err()
        );
        let mut list = add(&backend, "owner-a", "seed");
        for index in 1..MAX_ITEMS {
            list = match backend
                .apply_task_ops(
                    "owner-a",
                    list.revision,
                    &[TaskOp::Add {
                        description: format!("item {index}"),
                        status: STATUS_PENDING.into(),
                        reminder: None,
                    }],
                )
                .unwrap()
            {
                ApplyOutcome::Applied(list) => list,
                ApplyOutcome::Conflict(_) => panic!(""),
            };
        }
        assert_eq!(list.items.len(), MAX_ITEMS);
        assert!(
            backend
                .apply_task_ops(
                    "owner-a",
                    list.revision,
                    &[TaskOp::Add {
                        description: "over cap".into(),
                        status: STATUS_PENDING.into(),
                        reminder: None,
                    }],
                )
                .is_err()
        );
    }

    #[test]
    fn parallel_in_progress_is_allowed() {
        let (_tmp, backend) = backend();
        let mut list = add(&backend, "owner-a", "first");
        list = match backend
            .apply_task_ops(
                "owner-a",
                list.revision,
                &[TaskOp::Add {
                    description: "second".into(),
                    status: STATUS_IN_PROGRESS.into(),
                    reminder: None,
                }],
            )
            .unwrap()
        {
            ApplyOutcome::Applied(list) => list,
            ApplyOutcome::Conflict(_) => panic!(""),
        };
        // Multiple in_progress items are legal: these are multi-day
        // commitments, not the current-step focus of TodoWrite.
        let updated = match backend
            .apply_task_ops(
                "owner-a",
                list.revision,
                &[TaskOp::Update {
                    task_id: list.items[0].task_id.clone(),
                    description: None,
                    status: Some(STATUS_IN_PROGRESS.into()),
                    reminder: super::super::reminder_occurrences::ReminderChange::Keep,
                }],
            )
            .unwrap()
        {
            ApplyOutcome::Applied(list) => list,
            ApplyOutcome::Conflict(_) => panic!(""),
        };
        assert_eq!(
            updated
                .items
                .iter()
                .filter(|item| item.status == STATUS_IN_PROGRESS)
                .count(),
            2
        );
    }
}
