//! Task-context prompt section (#567): projects the committed durable task
//! list into the system prompt as its OWN section, refreshed whenever the
//! prompt is rebuilt. Strictly separate from Soul/Tone authority content.
//! A read failure renders `unavailable` — never a fake empty list.

use zeroclaw_infra::session_sqlite::SqliteSessionBackend;
use zeroclaw_infra::session_sqlite::agent_tasks::{MAX_ITEMS, TaskList};

use crate::agent::prompt::{PromptContext, PromptSection};

tokio::task_local! {
    /// Per-turn tasks re-render scope (#567): the owning Agent sets this
    /// around its tool loop; each model request re-renders the `## Tasks`
    /// region through it so cross-session commits are visible mid-turn.
    static TASKS_REFRESH: std::sync::Arc<dyn Fn() -> String + Send + Sync>;
}

/// Scope guard: clears the register when dropped. The Agent creates one
/// per turn via [`set_tasks_refresh_scope`] inside `scope_async`.
pub struct TasksRefreshGuard;

/// Run `f` with the tasks re-render scope active.
pub async fn with_tasks_refresh_scope<R>(
    render: std::sync::Arc<dyn Fn() -> String + Send + Sync>,
    f: impl std::future::Future<Output = R>,
) -> R {
    TASKS_REFRESH.scope(render, f).await
}

/// Swap the `## Tasks` region of a system prompt using the scoped
/// register's renderer; a no-op when no scope is set (other surfaces/tests).
pub fn refresh_tasks_region(system_prompt: &mut String) {
    TASKS_REFRESH
        .try_with(|render| {
            let block = render();
            *system_prompt = swap_or_append(system_prompt, &block);
        })
        .ok();
}

pub struct TaskListSection {
    /// Same resolver family as TaskListTool: (owner identity, data dir).
    pub resolve:
        std::sync::Arc<dyn Fn() -> Result<(String, std::path::PathBuf), String> + Send + Sync>,
}

impl TaskListSection {
    /// Re-render the `## Tasks` block on demand (per model request). The
    /// prompt-anchor refresh path calls this so another session's committed
    /// update is visible on the NEXT request, not only on a full rebuild.
    /// Delimited region swap: if the system prompt carries a tasks block,
    /// replace it; append otherwise. Failures render `unavailable`.
    pub fn refresh_into(&self, system_prompt: &str) -> String {
        match self.render_block() {
            Ok(block) => swap_or_append(system_prompt, &block),
            Err(_) => system_prompt.to_string(),
        }
    }

    fn render_block(&self) -> anyhow::Result<String> {
        // Reuse build() with an empty context placeholder.
        use crate::agent::prompt::PromptContext;
        let ctx = PromptContext::placeholder();
        self.build(&ctx)
    }
}

/// Replace the existing delimited `## Tasks` region or append the block.
fn swap_or_append(system_prompt: &str, block: &str) -> String {
    const START: &str = "## Tasks";
    if let Some(start) = system_prompt.find(START) {
        // The region ends at the next top-level section header after start.
        let tail = &system_prompt[start + START.len()..];
        if let Some(offset) = tail.find("\n## ") {
            let end = start + START.len() + offset;
            let mut out = String::with_capacity(system_prompt.len() + block.len());
            out.push_str(&system_prompt[..start]);
            out.push_str(block.trim_start_matches(START).trim_start());
            out.push_str("\n\n");
            out.push_str(&system_prompt[end + 1..]);
            return out;
        }
        // Tasks is the last section: replace the remainder.
        let mut out = String::with_capacity(system_prompt.len());
        out.push_str(&system_prompt[..start]);
        out.push_str(block.trim_start_matches(START).trim_start());
        return out;
    }
    let mut out = String::from(system_prompt);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(block);
    out
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
