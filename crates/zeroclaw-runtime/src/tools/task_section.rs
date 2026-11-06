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
/// The block keeps its own `## Tasks` header, so the region can be found and
/// replaced again on every refresh (idempotent; no growth across refreshes).
/// Item lines are single-line (see `build`), so `\n## ` can only be a real
/// section boundary — item text cannot forge one.
fn swap_or_append(system_prompt: &str, block: &str) -> String {
    const START: &str = "## Tasks";
    const NEXT_SECTION: &str = "\n## ";
    if let Some(start) = system_prompt.find(START) {
        // The region runs from the tasks header to the NEXT section header
        // (searched after the header text itself) or to the end.
        if let Some(offset) = system_prompt[start + 1..].find(NEXT_SECTION) {
            let end = start + 1 + offset; // at the '\n' of the next header
            let mut out = String::with_capacity(system_prompt.len() + block.len());
            out.push_str(&system_prompt[..start]);
            out.push_str(block);
            out.push('\n');
            out.push_str(&system_prompt[end..]);
            return out;
        }
        // Tasks is the last section: replace the remainder.
        let mut out = String::with_capacity(system_prompt.len() + block.len());
        out.push_str(&system_prompt[..start]);
        out.push_str(block);
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
        // A query failure renders unavailable (never a fake empty list and
        // never a build error that would kill the turn); the empty list
        // carries its revision so the model can echo it without guessing.
        let TaskList { revision, items } = match backend.task_list(&owner) {
            Ok(list) => list,
            Err(_) => {
                return Ok(
                    "## Tasks\n\nTask list unavailable (storage not reachable). Do not assume it is empty."
                        .to_string(),
                );
            }
        };
        if items.is_empty() {
            return Ok(format!(
                "## Tasks\n\nNo committed tasks (revision {revision}). Use task_list to add a commitment."
            ));
        }
        let mut lines = Vec::with_capacity(items.len() + 1);
        lines.push(format!(
            "## Tasks\n\nDurable commitments (revision {revision}; update via task_list with expected_revision={revision}):"
        ));
        // Total render length is bounded by the item cap and description cap.
        // Descriptions render SINGLE-LINE: embedded newlines would let item
        // text forge a `\n## ` section header and corrupt the delimited
        // region the per-request refresh swaps.
        for (shown, item) in items.iter().enumerate() {
            if shown >= MAX_ITEMS {
                lines.push(format!("… {} more items omitted", items.len() - shown));
                break;
            }
            let one_line = item.description.replace(['\n', '\r'], " ");
            lines.push(format!(
                "- [{}] {} ({})",
                item.status, one_line, item.task_id
            ));
        }
        Ok(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(body: &str) -> String {
        format!("## Tasks\n\n{body}")
    }

    #[test]
    fn swap_is_idempotent_across_repeated_refreshes() {
        let base = "## Identity\n\nYou are ZeroClaw.\n\n".to_string() + &block("one");
        let updated = swap_or_append(&base, &block("two"));
        assert!(updated.starts_with("## Identity"));
        assert!(updated.contains("two") && !updated.contains("one"));
        // Repeated refreshes must not grow the prompt or lose the marker.
        let again = swap_or_append(&updated, &block("three"));
        let third = swap_or_append(&again, &block("four"));
        assert!(third.contains("## Tasks"));
        assert_eq!(
            third.match_indices("## Tasks").count(),
            1,
            "exactly one tasks region survives repeated swaps"
        );
        assert!(third.len() < base.len() + 4 * block("x").len());
    }

    #[test]
    fn swap_between_sections_preserves_neighbors() {
        let prompt = format!("## A\n\nfirst\n\n{}\n\n## B\n\nlast", block("mid"));
        let swapped = swap_or_append(&prompt, &block("new"));
        assert!(swapped.starts_with("## A\n\nfirst"));
        assert!(swapped.contains("## B\n\nlast"));
        assert!(swapped.contains("new"));
        assert!(!swapped.contains("mid"));
    }

    #[test]
    fn item_text_cannot_forge_a_section_boundary() {
        // The build() sanitizer renders descriptions single-line; verify the
        // raw description with an embedded header stays inside the region.
        let rendered = format!(
            "- [pending] {} (t1)",
            "evil\n## Hijack".replace(['\n', '\r'], " ")
        );
        let prompt = format!("## A\n\n{}\n\n## B", block(&rendered));
        assert!(!prompt.contains("evil\n##"));
        let swapped = swap_or_append(&prompt, &block("clean"));
        assert!(swapped.contains("## B"));
    }
}
