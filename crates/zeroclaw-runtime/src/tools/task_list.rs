//! Model-facing durable task-list tool (#567): cross-session commitments
//! with server-minted stable ids and revision-checked batched writes. This
//! is NOT the current-step plan (that is `TodoWrite`) — items here are
//! multi-day commitments that survive restarts and session changes.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_infra::session_sqlite::SqliteSessionBackend;
use zeroclaw_infra::session_sqlite::agent_tasks::{
    ApplyOutcome, STATUS_COMPLETED, STATUS_IN_PROGRESS, STATUS_PENDING, TaskOp,
};

/// Resolve the owning identity and the storage data dir for a call. The
/// OWNER never comes from tool arguments — it is derived from the
/// authenticated execution context (the agent identity). The concrete
/// SQLite backend is opened from the data dir per call (the tachi-tools
/// pattern: one bounded open, no long-lived handle).
pub type TaskOwnerResolver =
    Arc<dyn Fn() -> Result<(String, std::path::PathBuf), String> + Send + Sync>;

pub struct TaskListTool {
    resolve: TaskOwnerResolver,
}

impl TaskListTool {
    #[must_use]
    pub fn new(resolve: TaskOwnerResolver) -> Self {
        Self { resolve }
    }
}

fn parse_ops(args: &Value) -> anyhow::Result<Vec<TaskOp>> {
    let ops = args
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::Error::msg("`ops` must be a non-empty array"))?;
    if ops.is_empty() {
        anyhow::bail!("`ops` must contain at least one operation");
    }
    let mut parsed = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        let kind = op
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::Error::msg(format!("ops[{index}].op is required")))?;
        // Strict schema: unknown fields are rejected, never silently dropped.
        let allowed: &[&str] = match kind {
            "add" => &["op", "description", "status"],
            "update" => &["op", "task_id", "description", "status"],
            "remove" => &["op", "task_id"],
            other => anyhow::bail!("ops[{index}].op must be add|update|remove, got {other:?}"),
        };
        if let Some(fields) = op.as_object() {
            for field in fields.keys() {
                if !allowed.contains(&field.as_str()) {
                    anyhow::bail!("ops[{index}] carries unsupported field {field:?}");
                }
            }
        }
        let status_for_add = || -> anyhow::Result<String> {
            match op.get("status").and_then(Value::as_str) {
                None => Ok(STATUS_PENDING.to_string()),
                Some(status)
                    if [STATUS_PENDING, STATUS_IN_PROGRESS, STATUS_COMPLETED].contains(&status) =>
                {
                    Ok(status.to_string())
                }
                Some(other) => {
                    anyhow::bail!(
                        "ops[{index}].status must be pending|in_progress|completed, got {other:?}"
                    )
                }
            }
        };
        let parsed_op = match kind {
            "add" => TaskOp::Add {
                description: op
                    .get("description")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow::Error::msg(format!("ops[{index}].description is required"))
                    })?
                    .to_string(),
                status: status_for_add()?,
            },
            "update" => TaskOp::Update {
                task_id: op
                    .get("task_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::Error::msg(format!("ops[{index}].task_id is required")))?
                    .to_string(),
                description: op
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                status: op.get("status").and_then(Value::as_str).map(str::to_string),
            },
            "remove" => TaskOp::Remove {
                task_id: op
                    .get("task_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::Error::msg(format!("ops[{index}].task_id is required")))?
                    .to_string(),
            },
            _ => unreachable!("kind validated above"),
        };
        parsed.push(parsed_op);
    }
    Ok(parsed)
}

fn render_list(
    revision: i64,
    items: &[zeroclaw_infra::session_sqlite::agent_tasks::TaskItem],
) -> Value {
    json!({
        "revision": revision,
        "items": items
            .iter()
            .map(|item| {
                json!({
                    "task_id": item.task_id,
                    "description": item.description,
                    "status": item.status,
                })
            })
            .collect::<Vec<_>>(),
    })
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "task_list"
    }

    fn description(&self) -> &str {
        // Distinct from TodoWrite: durable commitments, not current steps.
        "Manage your durable commitment list across sessions and days \
(multi-day promises; NOT the current-step plan — use TodoWrite for that). \
Batched per-item operations with stable server-issued task ids; every call \
must echo the current `expected_revision` (from the task context or the \
last result). On a revision conflict the current list is returned — \
re-judge and retry with its revision. Statuses: pending, in_progress \
(several allowed), completed."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "expected_revision": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "The revision this batch is based on"
                },
                "ops": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "oneOf": [
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "add"},
                                    "description": {"type": "string", "minLength": 1},
                                    "status": {"enum": ["pending", "in_progress", "completed"]}
                                },
                                "required": ["op", "description"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "update"},
                                    "task_id": {"type": "string"},
                                    "description": {"type": "string", "minLength": 1},
                                    "status": {"enum": ["pending", "in_progress", "completed"]}
                                },
                                "required": ["op", "task_id"],
                                "additionalProperties": false
                            },
                            {
                                "type": "object",
                                "properties": {
                                    "op": {"const": "remove"},
                                    "task_id": {"type": "string"}
                                },
                                "required": ["op", "task_id"],
                                "additionalProperties": false
                            }
                        ]
                    }
                }
            },
            "required": ["expected_revision", "ops"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        let expected_revision = match args.get("expected_revision").and_then(Value::as_i64) {
            Some(revision) if revision >= 0 => revision,
            Some(_) => {
                return Ok(ToolResult::err("`expected_revision` must be >= 0"));
            }
            None => {
                return Ok(ToolResult::err("`expected_revision` is required"));
            }
        };
        let ops = match parse_ops(&args) {
            Ok(ops) => ops,
            Err(error) => return Ok(ToolResult::err(error.to_string())),
        };
        // Owner and storage root from the execution context; the model
        // cannot choose whose list it edits.
        let (owner, data_dir) = match (self.resolve)() {
            Ok(resolved) => resolved,
            Err(error) => return Ok(ToolResult::err(error)),
        };
        let sqlite = match tokio::task::spawn_blocking(move || {
            SqliteSessionBackend::new(&data_dir)
                .map_err(|error| format!("task store unavailable: {error}"))
        })
        .await
        {
            Ok(Ok(backend)) => backend,
            Ok(Err(error)) => return Ok(ToolResult::err(error)),
            Err(join) => {
                return Ok(ToolResult::err(format!("task store task failed: {join}")));
            }
        };
        Ok(
            match sqlite.apply_task_ops(&owner, expected_revision, &ops) {
                Ok(ApplyOutcome::Applied(list)) => ToolResult::ok(json!({
                    "applied": true,
                    "list": render_list(list.revision, &list.items),
                })),
                Ok(ApplyOutcome::Conflict(list)) => ToolResult::ok(json!({
                    "applied": false,
                    "conflict": true,
                    "hint": "the list changed; re-judge and retry with this revision",
                    "list": render_list(list.revision, &list.items),
                })),
                Err(error) => ToolResult::err(format!("Error: {error}")),
            },
        )
    }
}

zeroclaw_api::tool_attribution!(TaskListTool, zeroclaw_api::attribution::ToolKind::Plugin);

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_infra::session_sqlite::SqliteSessionBackend;

    fn fixture() -> (tempfile::TempDir, (), TaskListTool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let tool = TaskListTool::new(Arc::new(move || {
            Ok(("identity-uuid".to_string(), data_dir.clone()))
        }));
        (tmp, (), tool)
    }

    fn add_args(revision: i64, description: &str) -> Value {
        json!({
            "expected_revision": revision,
            "ops": [{"op": "add", "description": description}]
        })
    }

    #[tokio::test]
    async fn add_conflict_update_remove_roundtrip() {
        let (_tmp, _backend, tool) = fixture();
        let first = tool.execute(add_args(0, "call the vet")).await.unwrap();
        let payload = first.output.data().cloned().unwrap();
        assert_eq!(payload["applied"], true);
        let list = &payload["list"];
        assert_eq!(list["revision"], 1);
        let task_id = list["items"][0]["task_id"].as_str().unwrap().to_string();

        // Stale revision conflicts with the current list returned.
        let stale = tool.execute(add_args(0, "stale write")).await.unwrap();
        let payload = stale.output.data().cloned().unwrap();
        assert_eq!(payload["applied"], false);
        assert_eq!(payload["conflict"], true);
        assert_eq!(payload["list"]["revision"], 1);

        // Update by stable id; the id survives.
        let updated = tool
            .execute(json!({
                "expected_revision": 1,
                "ops": [{"op": "update", "task_id": task_id, "status": "completed"}]
            }))
            .await
            .unwrap();
        let payload = updated.output.data().cloned().unwrap();
        assert_eq!(payload["list"]["items"][0]["status"], "completed");
        assert_eq!(payload["list"]["items"][0]["task_id"], task_id);

        // Remove; then a repeat remove of the same id errors cleanly.
        let removed = tool
            .execute(json!({
                "expected_revision": 2,
                "ops": [{"op": "remove", "task_id": task_id}]
            }))
            .await
            .unwrap();
        assert_eq!(
            removed.output.data().cloned().unwrap()["list"]["items"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        let repeat = tool
            .execute(json!({
                "expected_revision": 3,
                "ops": [{"op": "remove", "task_id": task_id}]
            }))
            .await
            .unwrap();
        assert!(repeat.error.is_some(), "unknown id must error");
    }

    #[tokio::test]
    async fn strict_schema_and_owner_from_context() {
        let (_tmp, _backend, tool) = fixture();
        // Unknown field rejected.
        let bad = tool
            .execute(json!({
                "expected_revision": 0,
                "ops": [{"op": "add", "description": "x", "owner": "someone-else"}]
            }))
            .await
            .unwrap();
        assert!(bad.error.is_some());
        // Invalid status rejected.
        let bad = tool
            .execute(json!({
                "expected_revision": 0,
                "ops": [{"op": "add", "description": "x", "status": "done"}]
            }))
            .await
            .unwrap();
        assert!(bad.error.is_some());
        // Empty ops rejected.
        let bad = tool
            .execute(json!({"expected_revision": 0, "ops": []}))
            .await
            .unwrap();
        assert!(bad.error.is_some());
    }
}
