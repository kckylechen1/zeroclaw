//! Durable agent task list (#567): cross-session commitments owned by the
//! companion agent identity (a mint-once UUID string, resolved by the caller —
//! the store is identity-agnostic). Item identity is a server-minted UUID,
//! never reused; every mutation batch is transactional and checked against
//! the owner's monotonic list revision.

use super::SqliteSessionBackend;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior, params};
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
}

/// One batched operation. Adds mint identity server-side; updates and
/// removes address items by their stable id — never by description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOp {
    Add {
        description: String,
        status: String,
    },
    Update {
        task_id: String,
        description: Option<String>,
        status: Option<String>,
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

fn validate_description(description: &str) -> Result<()> {
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
    /// The committed list for an owner, ordered by ordinal.
    pub fn task_list(&self, agent_identity: &str) -> Result<TaskList> {
        let conn = self.conn.lock();
        let revision: i64 = conn
            .query_row(
                "SELECT revision FROM agent_task_revisions WHERE agent_identity = ?1",
                params![agent_identity],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let mut query = conn.prepare(
            "SELECT task_id, ordinal, description, status FROM agent_tasks
             WHERE agent_identity = ?1 ORDER BY ordinal, rowid",
        )?;
        let items = query
            .query_map(params![agent_identity], |row| {
                Ok(TaskItem {
                    task_id: row.get(0)?,
                    ordinal: row.get(1)?,
                    description: row.get(2)?,
                    status: row.get(3)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(TaskList { revision, items })
    }

    /// Apply one batched, transactional mutation. `expected_revision` < 0
    /// means "accept any current revision" (first write / forced repair by
    /// the model after a conflict round-trip is NOT this — the model must
    /// echo the returned revision).
    pub fn apply_task_ops(
        &self,
        agent_identity: &str,
        expected_revision: i64,
        ops: &[TaskOp],
    ) -> Result<ApplyOutcome> {
        if ops.len() > MAX_ITEMS {
            bail!("a batch may carry at most {MAX_ITEMS} operations");
        }
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("opening agent_tasks transaction")?;
        let revision: i64 = tx
            .query_row(
                "SELECT revision FROM agent_task_revisions WHERE agent_identity = ?1",
                params![agent_identity],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let list = read_items(&tx, agent_identity)?;
        if expected_revision >= 0 && expected_revision != revision {
            return Ok(ApplyOutcome::Conflict(TaskList {
                revision,
                items: list,
            }));
        }
        if list.len()
            + ops
                .iter()
                .filter(|op| matches!(op, TaskOp::Add { .. }))
                .count()
            > MAX_ITEMS
        {
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
                }
                TaskOp::Update {
                    task_id,
                    description,
                    status,
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
                }
            }
        }
        tx.execute(
            "INSERT INTO agent_task_revisions (agent_identity, revision) VALUES (?1, 1)
             ON CONFLICT(agent_identity) DO UPDATE SET revision = revision + 1",
            params![agent_identity],
        )
        .context("bumping the task list revision")?;
        let revision = revision + 1;
        let items = read_items(&tx, agent_identity)?;
        tx.commit().context("committing agent task operations")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(ApplyOutcome::Applied(TaskList { revision, items }))
    }
}

fn read_items(tx: &rusqlite::Transaction, agent_identity: &str) -> Result<Vec<TaskItem>> {
    let mut query = tx.prepare(
        "SELECT task_id, ordinal, description, status FROM agent_tasks
         WHERE agent_identity = ?1 ORDER BY ordinal, rowid",
    )?;
    let items = query
        .query_map(params![agent_identity], |row| {
            Ok(TaskItem {
                task_id: row.get(0)?,
                ordinal: row.get(1)?,
                description: row.get(2)?,
                status: row.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(items)
}

fn next_ordinal(tx: &rusqlite::Transaction, agent_identity: &str) -> Result<i64> {
    let max: i64 = tx
        .query_row(
            "SELECT COALESCE(MAX(ordinal), 0) FROM agent_tasks WHERE agent_identity = ?1",
            params![agent_identity],
            |row| row.get(0),
        )
        .unwrap_or(0);
    Ok(max + 1)
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
                }],
            )
            .unwrap()
        {
            ApplyOutcome::Applied(list) => list,
            ApplyOutcome::Conflict(_) => panic!("first write must apply"),
        }
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
