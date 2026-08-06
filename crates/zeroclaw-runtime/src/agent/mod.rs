#[allow(clippy::module_inception)]
pub mod agent;
pub mod announce_claim;
pub(crate) mod approval_bridge;
pub mod capped_line;
pub mod channel_factories;
pub mod classifier;
pub mod context_analyzer;
pub mod cost;
pub mod dispatcher;
pub mod eval;
pub mod history;
pub mod history_pruner;
pub mod history_trim;
pub mod loop_;
pub mod loop_detector;
pub mod memory_inject;
pub mod memory_strategy;
pub mod personality;
pub mod personality_templates;
pub mod pricing_catalog;
pub mod prompt;
pub mod prompt_helpers;
pub mod system_prompt;
pub mod text_tool_prompt;
pub mod thinking;
pub mod tool_execution;
pub mod tool_filter;
pub mod tool_receipts;
pub(crate) mod turn;

pub use turn::context::TurnMeta;

pub(crate) fn is_runtime_approved_arg_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "shell" | "schedule" | "cron_add" | "cron_update" | "cron_run"
    )
}

pub(crate) fn set_runtime_approved_arg(
    tool_name: &str,
    args: &mut serde_json::Value,
    approved: bool,
) {
    if is_runtime_approved_arg_tool(tool_name)
        && let Some(args) = args.as_object_mut()
    {
        args.insert("approved".to_string(), serde_json::Value::Bool(approved));
    }
}

/// Borrow-only Attributable holding an agent alias.
/// Used by entry points (loop_::run, process_message, cron dispatch)
/// that don't construct a full `Agent` but still need to open an
/// `attribution_span!` carrying the agent's role + alias.
pub struct AgentAttribution<'a>(pub &'a str);

impl ::zeroclaw_api::attribution::Attributable for AgentAttribution<'_> {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        ::zeroclaw_api::attribution::Role::Agent
    }
    fn alias(&self) -> &str {
        self.0
    }
}

#[cfg(test)]
mod tests;

#[allow(unused_imports)]
pub use agent::{Agent, AgentBuilder, TurnEvent};
/// The background-child waker's out-of-crate surface: the claim entry point for
/// outer turn shapes that scope the session key around the tool loop rather than
/// around turn assembly, plus the guard that hands the claim back when such a
/// turn never reaches its provider. Today's only consumer is the channel
/// orchestrator (`zeroclaw-channels`).
#[allow(unused_imports)]
pub use loop_::{UnclaimOnDrop, claim_announcements_for_scoped_turn};
#[allow(unused_imports)]
pub use loop_::{process_message, run};
