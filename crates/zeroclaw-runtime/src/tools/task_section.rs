//! Task-context prompt section (#567): projects the committed durable task
//! list into the system prompt as its OWN section, refreshed whenever the
//! prompt is rebuilt. Strictly separate from Soul/Tone authority content.
//! A read failure renders `unavailable` — never a fake empty list.

use zeroclaw_infra::session_sqlite::SqliteSessionBackend;
use zeroclaw_infra::session_sqlite::agent_tasks::{MAX_ITEMS, TaskList};

use crate::agent::prompt::{PromptContext, PromptSection};

pub struct TaskListSection {
    /// Same resolver family as TaskListTool: (owner identity, data dir).
    pub resolve:
        std::sync::Arc<dyn Fn() -> Result<(String, std::path::PathBuf), String> + Send + Sync>,
}

impl PromptSection for TaskListSection {
    fn name(&self) -> &str {
        "tasks"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> anyhow::Result<String> {
        let (owner, data_dir) = match (self.resolve)() {
            Ok(resolved) => resolved,
            Err(_) => {
                return Ok(
                    "## Tasks\n\nTask list unavailable (storage not reachable). Do not assume it is empty."
                        .to_string(),
                );
            }
        };
        let backend = match SqliteSessionBackend::new(&data_dir) {
            Ok(backend) => backend,
            Err(_) => {
                return Ok(
                    "## Tasks\n\nTask list unavailable (storage not reachable). Do not assume it is empty."
                        .to_string(),
                );
            }
        };
        let TaskList { revision, items } = backend.task_list(&owner)?;
        if items.is_empty() {
            return Ok(
                "## Tasks\n\nNo committed tasks. Use task_list to add a commitment.".to_string(),
            );
        }
        let mut lines = Vec::with_capacity(items.len() + 1);
        lines.push(format!(
            "## Tasks\n\nDurable commitments (revision {revision}; update via task_list with expected_revision={revision}):"
        ));
        // Total render length is bounded by the item cap and description cap.
        for (shown, item) in items.iter().enumerate() {
            if shown >= MAX_ITEMS {
                lines.push(format!("… {} more items omitted", items.len() - shown));
                break;
            }
            lines.push(format!(
                "- [{}] {} ({})",
                item.status, item.description, item.task_id
            ));
        }
        Ok(lines.join("\n"))
    }
}
