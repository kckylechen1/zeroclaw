//! Model-facing durable task-list tool (#567/#568): cross-session
//! commitments with server-minted stable ids and revision-checked batched
//! writes, each optionally carrying a reminder occurrence. This is NOT the
//! current-step plan (that is `TodoWrite`) — items here are multi-day
//! commitments that survive restarts and session changes.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_infra::session_sqlite::SqliteSessionBackend;
use zeroclaw_infra::session_sqlite::agent_tasks::{
    ApplyOutcome, STATUS_COMPLETED, STATUS_IN_PROGRESS, STATUS_PENDING, TaskOp,
};
use zeroclaw_infra::session_sqlite::reminder_occurrences::{
    ArmReminder, REMINDER_VALIDITY_SECS, ReminderChange,
};

/// Resolve the owning identity and the storage data dir for a call. The
/// OWNER never comes from tool arguments — it is derived from the
/// authenticated execution context (the agent identity). The concrete
/// SQLite backend is opened from the data dir per call (the tachi-tools
/// pattern: one bounded open, no long-lived handle).
pub type TaskOwnerResolver =
    Arc<dyn Fn() -> Result<(String, std::path::PathBuf), String> + Send + Sync>;

/// Resolve the owner-configured reminder destination (#568): the occurrence
/// COPIES these values at creation; a later config change never rewrites
/// an armed occurrence. Errors explain why reminders are unavailable.
pub type ReminderTargetResolver =
    Arc<dyn Fn() -> Result<(String, String, Option<String>), String> + Send + Sync>;

pub struct TaskListTool {
    resolve: TaskOwnerResolver,
    resolve_target: ReminderTargetResolver,
}

impl TaskListTool {
    #[must_use]
    pub fn new(resolve: TaskOwnerResolver, resolve_target: ReminderTargetResolver) -> Self {
        Self {
            resolve,
            resolve_target,
        }
    }
}

fn parse_ops(
    args: &Value,
    resolve_target: &ReminderTargetResolver,
    now: i64,
) -> anyhow::Result<Vec<TaskOp>> {
    let ops = args
        .get("ops")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::Error::msg("`ops` must be a non-empty array"))?;
    if ops.is_empty() {
        anyhow::bail!("`ops` must contain at least one operation");
    }
    // The destination resolves ONCE per call, and only when some op arms a
    // reminder (the occurrence copies these values at creation).
    let mut destination: Option<(String, String, Option<String>)> = None;
    let mut arm_reminder = |raw: &str| -> anyhow::Result<ArmReminder> {
        let due_at = parse_remind_at(raw)?;
        anyhow::ensure!(
            due_at + REMINDER_VALIDITY_SECS > now,
            "remind_at is more than 24h in the past; a reminder stays valid for 24h after \
             its due time"
        );
        if destination.is_none() {
            destination = Some(resolve_target().map_err(anyhow::Error::msg)?);
        }
        let (bridge, recipient, thread_id) =
            destination.as_ref().expect("destination just resolved");
        Ok(ArmReminder {
            due_at,
            bridge: bridge.clone(),
            recipient: recipient.clone(),
            thread_id: thread_id.clone(),
        })
    };
    let mut parsed = Vec::with_capacity(ops.len());
    for (index, op) in ops.iter().enumerate() {
        let kind = match op.get("op") {
            Some(Value::String(kind)) => kind.as_str(),
            Some(other) => {
                anyhow::bail!("ops[{index}].op must be a string, got {other}")
            }
            None => anyhow::bail!("ops[{index}].op is required"),
        };
        // Strict schema: unknown fields are rejected, never silently dropped.
        let allowed: &[&str] = match kind {
            "add" => &["op", "description", "status", "remind_at"],
            "update" => &["op", "task_id", "description", "status", "remind_at"],
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
        // Presence-vs-type discipline: a field present but not a string is a
        // TYPE error, never "absent" (silent defaulting loses caller intent).
        let required_str = |field: &str| -> anyhow::Result<String> {
            match op.get(field) {
                Some(Value::String(value)) => Ok(value.clone()),
                Some(other) => anyhow::bail!("ops[{index}].{field} must be a string, got {other}"),
                None => anyhow::bail!("ops[{index}].{field} is required"),
            }
        };
        let optional_str = |field: &str| -> anyhow::Result<Option<String>> {
            match op.get(field) {
                Some(Value::String(value)) => Ok(Some(value.clone())),
                Some(other) => anyhow::bail!("ops[{index}].{field} must be a string, got {other}"),
                None => Ok(None),
            }
        };
        let status_for_add = || -> anyhow::Result<String> {
            match optional_str("status")? {
                None => Ok(STATUS_PENDING.to_string()),
                Some(status)
                    if [STATUS_PENDING, STATUS_IN_PROGRESS, STATUS_COMPLETED]
                        .contains(&status.as_str()) =>
                {
                    Ok(status)
                }
                Some(other) => {
                    anyhow::bail!(
                        "ops[{index}].status must be pending|in_progress|completed, got {other:?}"
                    )
                }
            }
        };
        // `remind_at` tri-state: absent = keep the current reminder; null
        // (update only) = clear it; a string = arm/re-arm at that time.
        // The RFC3339 OFFSET is mandatory — a naive timestamp is an error
        // telling the model to ask, never a guess about the timezone.
        let remind_at_add = || -> anyhow::Result<Option<String>> {
            match op.get("remind_at") {
                Some(Value::String(value)) => Ok(Some(value.clone())),
                Some(other) => {
                    anyhow::bail!("ops[{index}].remind_at must be a string, got {other}")
                }
                None => Ok(None),
            }
        };
        let remind_at_update = || -> anyhow::Result<Option<Option<String>>> {
            match op.get("remind_at") {
                Some(Value::String(value)) => Ok(Some(Some(value.clone()))),
                Some(Value::Null) => Ok(Some(None)),
                Some(other) => {
                    anyhow::bail!("ops[{index}].remind_at must be a string or null, got {other}")
                }
                None => Ok(None),
            }
        };
        let parsed_op = match kind {
            "add" => TaskOp::Add {
                description: required_str("description")?,
                status: status_for_add()?,
                reminder: match remind_at_add()? {
                    Some(raw) => Some(arm_reminder(&raw)?),
                    None => None,
                },
            },
            "update" => TaskOp::Update {
                task_id: required_str("task_id")?,
                description: optional_str("description")?,
                status: optional_str("status")?,
                reminder: match remind_at_update()? {
                    None => ReminderChange::Keep,
                    Some(None) => ReminderChange::Clear,
                    Some(Some(raw)) => ReminderChange::Arm(arm_reminder(&raw)?),
                },
            },
            "remove" => TaskOp::Remove {
                task_id: required_str("task_id")?,
            },
            _ => unreachable!("kind validated above"),
        };
        parsed.push(parsed_op);
    }
    Ok(parsed)
}

/// A raw remind_at string → epoch seconds. Naive timestamps are refused.
fn parse_remind_at(raw: &str) -> anyhow::Result<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw).map_err(|error| {
        anyhow::Error::msg(format!(
            "remind_at must be RFC3339 WITH a UTC offset (e.g. 2026-11-07T09:00:00Z or \
             2026-11-07T17:00:00+08:00); ask the user for the timezone if unclear: {error}"
        ))
    })?;
    Ok(parsed.timestamp())
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
                    "remind_at": item.remind_at,
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
(several allowed), completed. `remind_at` (RFC3339 WITH UTC offset; ask \
when the timezone is unclear) arms one reminder per task that fires as a \
notice to the owner-configured destination, valid for 24h past its due \
time; setting it again reschedules; null clears it; completing or removing \
the task cancels it."
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
                                    "status": {"enum": ["pending", "in_progress", "completed"]},
                                    "remind_at": {"type": "string", "description": "RFC3339 with UTC offset; arms a reminder at this time"}
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
                                    "status": {"enum": ["pending", "in_progress", "completed"]},
                                    "remind_at": {"type": ["string", "null"], "description": "RFC3339 with UTC offset to (re)arm; null clears; absent keeps"}
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
        // Strict execution-side validation mirrors the published schema:
        // unknown top-level fields and mistyped values are rejected rather
        // than silently ignored (`and_then(as_str)` cannot distinguish
        // "missing" from "wrong type").
        if let Some(fields) = args.as_object() {
            for field in fields.keys() {
                if !matches!(field.as_str(), "expected_revision" | "ops") {
                    return Ok(ToolResult::err(format!("unsupported field {field:?}")));
                }
            }
        }
        let expected_revision = match args.get("expected_revision").and_then(Value::as_i64) {
            Some(revision) if revision >= 0 => revision,
            Some(_) => {
                return Ok(ToolResult::err("`expected_revision` must be >= 0"));
            }
            None => {
                return Ok(ToolResult::err("`expected_revision` is required"));
            }
        };
        let ops = match parse_ops(&args, &self.resolve_target, chrono::Utc::now().timestamp()) {
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

    fn fixture() -> (tempfile::TempDir, (), TaskListTool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let tool = TaskListTool::new(
            Arc::new(move || Ok(("identity-uuid".to_string(), data_dir.clone()))),
            Arc::new(|| {
                Err(
                    "no [tasks.reminder_target] configured: reminders are unavailable until \
                     the owner sets one"
                        .to_string(),
                )
            }),
        );
        (tmp, (), tool)
    }

    fn fixture_with_target() -> (tempfile::TempDir, (), TaskListTool) {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let tool = TaskListTool::new(
            Arc::new(move || Ok(("identity-uuid".to_string(), data_dir.clone()))),
            Arc::new(|| Ok(("tg".to_string(), "owner".to_string(), None))),
        );
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

    #[tokio::test]
    async fn remind_at_arms_clears_and_requires_destination() {
        // Without a configured target, arming is refused with a clear error.
        let (_tmp, _b, tool) = fixture();
        let refused = tool
            .execute(json!({
                "expected_revision": 0,
                "ops": [{"op": "add", "description": "x", "remind_at": "2027-01-01T09:00:00Z"}]
            }))
            .await
            .unwrap();
        let message = refused.error.expect("arming without a target fails");
        assert!(message.contains("reminder_target"), "{message}");

        // With a target: arm at add, echo in the list, reschedule, clear.
        let (_tmp, _b, tool) = fixture_with_target();
        let armed = tool
            .execute(json!({
                "expected_revision": 0,
                "ops": [{"op": "add", "description": "call the vet",
                         "remind_at": "2027-01-01T09:00:00Z"}]
            }))
            .await
            .unwrap();
        let payload = armed.output.data().cloned().unwrap();
        assert_eq!(payload["applied"], true);
        assert_eq!(payload["list"]["items"][0]["remind_at"], 1_798_794_000_i64);
        let task_id = payload["list"]["items"][0]["task_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Reschedule: a new due time replaces the old one.
        let rescheduled = tool
            .execute(json!({
                "expected_revision": 1,
                "ops": [{"op": "update", "task_id": task_id,
                         "remind_at": "2027-01-02T09:00:00+08:00"}]
            }))
            .await
            .unwrap();
        let payload = rescheduled.output.data().cloned().unwrap();
        assert_eq!(
            payload["list"]["items"][0]["remind_at"], 1_798_851_600_i64,
            "offset-aware RFC3339 reschedules the occurrence"
        );

        // null clears; the description edit kept it until now.
        let cleared = tool
            .execute(json!({
                "expected_revision": 2,
                "ops": [{"op": "update", "task_id": task_id, "remind_at": null}]
            }))
            .await
            .unwrap();
        let payload = cleared.output.data().cloned().unwrap();
        assert!(payload["list"]["items"][0]["remind_at"].is_null());

        // Completing the task invalidates a re-armed reminder.
        let rearmed = tool
            .execute(json!({
                "expected_revision": 3,
                "ops": [{"op": "update", "task_id": task_id,
                         "remind_at": "2027-01-03T09:00:00Z",
                         "status": "completed"}]
            }))
            .await
            .unwrap();
        assert!(
            rearmed.error.is_some(),
            "arming a completed task is refused"
        );
    }

    #[tokio::test]
    async fn naive_or_stale_remind_at_is_rejected() {
        let (_tmp, _b, tool) = fixture_with_target();
        // Naive timestamp (no offset): clarified, never guessed.
        let naive = tool
            .execute(json!({
                "expected_revision": 0,
                "ops": [{"op": "add", "description": "x",
                         "remind_at": "2027-01-01T09:00:00"}]
            }))
            .await
            .unwrap();
        let message = naive.error.expect("naive remind_at fails");
        assert!(message.contains("UTC offset"), "{message}");
        // Far past: outside the 24h validity, refused as dead-on-arrival.
        let stale = tool
            .execute(json!({
                "expected_revision": 0,
                "ops": [{"op": "add", "description": "x",
                         "remind_at": "2020-01-01T09:00:00Z"}]
            }))
            .await
            .unwrap();
        assert!(stale.error.is_some());
    }
}
