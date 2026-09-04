//! Run/process_message shared overrides and agent/provider resolution helpers.
//!
//! Extracted from `loop_/mod.rs` so entry-point assembly can import a small
//! resolve surface without owning the full interactive loop body.

use anyhow::{Context, Result};
use std::sync::Arc;
#[cfg(test)]
use std::sync::{LazyLock, Mutex};
use zeroclaw_memory::Memory;

use crate::security::SecurityPolicy;

#[derive(Default)]
pub struct AgentRunOverrides {
    pub security: Option<Arc<SecurityPolicy>>,
    pub memory: Option<Arc<dyn Memory>>,
    pub is_subagent: bool,
    /// Spawn-site opt-out of the engine's memory-context injection (e.g. a
    /// cron job configured with `uses_memory = false`). Default `false`.
    pub suppress_memory_inject: bool,
    pub memory_free: bool,
    /// Pre-built MCP registry supplied by the caller. The daemon heartbeat
    /// worker constructs this once at worker start and shares it across
    /// every tick so that stdio MCP children live for the daemon's
    /// lifetime rather than being orphaned and re-spawned per
    /// `agent::run` call. When `Some`, the loop MUST use this
    /// `Arc<McpRegistry>` and MUST NOT call `McpRegistry::connect_all`
    /// itself; `None` preserves the legacy per-call connect path
    /// (CLI / one-shot), which is correct for callers that have no
    /// cross-turn reuse contract.
    pub mcp_registry: Option<Arc<crate::tools::McpRegistry>>,
    /// Unified spawn lineage (SA-9): the ONE depth authority carried by
    /// the spawning context. Spawn sites MUST pass `parent_lineage.child()`
    /// here; a registry rebuild inside the child then cannot reset depth
    /// (SA-11) and every spawn surface increments the same ledger (SA-10;
    /// the retired `delegate`/`spawn_subagent` hopped the same ledger). `None` means this run is a genuine root (interactive
    /// top-level turn, cron job) — the run mints a root lineage from its
    /// session key.
    pub lineage: Option<zeroclaw_api::subagent_v1::LineageRef>,
}

pub(crate) fn agent_provider_composite(
    config: &zeroclaw_config::schema::Config,
    agent_alias: &str,
) -> Option<String> {
    config
        .resolved_model_provider_for_agent(agent_alias)
        .map(|(ty, alias, _)| format!("{ty}.{alias}"))
}

/// Return the owned agent config direct-turn setup needs, with runtime-profile
/// values baked into `resolved`.
pub(crate) fn resolved_agent_for_turn(
    config: &zeroclaw_config::schema::Config,
    agent_alias: &str,
) -> Result<zeroclaw_config::schema::AliasedAgentConfig> {
    let agent = config
        .resolved_agent_config(agent_alias)
        .with_context(|| format!("agents.{agent_alias} is not configured"))?;
    #[cfg(test)]
    if let Some(hook) = RESOLVED_AGENT_FOR_TURN_TEST_HOOK
        .lock()
        .expect("resolved-agent test hook lock should not be poisoned")
        .as_ref()
        .cloned()
    {
        hook(agent_alias, agent.resolved.max_tool_iterations);
    }
    Ok(agent)
}

#[cfg(test)]
type ResolvedAgentForTurnTestHook = Arc<dyn Fn(&str, usize) + Send + Sync>;

#[cfg(test)]
pub(crate) static RESOLVED_AGENT_FOR_TURN_TEST_HOOK: LazyLock<
    Mutex<Option<ResolvedAgentForTurnTestHook>>,
> = LazyLock::new(|| Mutex::new(None));

pub(crate) fn api_key_and_uri_for_provider(
    config: &zeroclaw_config::schema::Config,
    provider_name: &str,
    fallback: Option<&zeroclaw_config::schema::ModelProviderConfig>,
) -> (Option<String>, Option<String>) {
    if let Some((fam, al)) = provider_name.split_once('.')
        && let Some(entry) = config.providers.models.find(fam, al)
    {
        return (entry.api_key.clone(), entry.uri.clone());
    }
    (
        fallback.and_then(|e| e.api_key.clone()),
        fallback.and_then(|e| e.uri.clone()),
    )
}
