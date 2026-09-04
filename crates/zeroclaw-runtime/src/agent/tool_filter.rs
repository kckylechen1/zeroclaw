//! MCP / policy tool-filter admission helpers for the agent loop.
//!
//! Extracted from `loop_.rs` so allowlist/glob/keyword gating stays testable
//! without sitting next to the interactive `run` / `process_message` assembly.

use crate::tools::{self, Tool};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

pub(crate) fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.find('*') {
        None => pattern == name,
        Some(star) => {
            let prefix = &pattern[..star];
            let suffix = &pattern[star + 1..];
            name.starts_with(prefix)
                && name.ends_with(suffix)
                && name.len() >= prefix.len() + suffix.len()
        }
    }
}

pub fn apply_policy_tool_filter(
    tools: &mut Vec<Box<dyn Tool>>,
    policy: Option<&zeroclaw_config::policy::SecurityPolicy>,
    caller_allowed: Option<&[String]>,
) {
    tools.retain(|t| {
        let name = t.name();
        let policy_ok = policy.is_none_or(|p| p.is_tool_allowed(name));
        let caller_ok = caller_allowed.is_none_or(|list| list.iter().any(|n| n == name));
        policy_ok && caller_ok
    });
}

/// Build the MCP tool-access policy for an agent from its `SecurityPolicy`
/// (`allowed_tools` + `excluded_tools`) and an optional caller-supplied
/// allowlist. Shared by the runtime agent loop and the channels orchestrator
/// so every MCP registration site gates through identical logic.
pub fn mcp_tool_access_policy(
    security: &zeroclaw_config::policy::SecurityPolicy,
    caller_allowed: Option<&[String]>,
) -> Option<zeroclaw_tools::tool_search::ToolAccessPolicy> {
    zeroclaw_tools::tool_search::ToolAccessPolicy::from_security(
        security.allowed_tools.as_deref(),
        security.excluded_tools.as_deref(),
        caller_allowed,
        security.mcp_discovered_tool_policy,
    )
}

/// Whether an MCP tool name is admitted by `policy` (a `None` policy admits
/// everything). The risk-profile denylist always wins. Whether the allowlist
/// also admits unlisted `<server>__<tool>` names is the profile's
/// `mcp_discovered_tool_policy`; under the default `explicit_only` it does
/// not, so a server that grows a tool gains no reach until it is named.
pub fn eager_mcp_tool_allowed(
    name: &str,
    policy: Option<&zeroclaw_tools::tool_search::ToolAccessPolicy>,
) -> bool {
    policy.is_none_or(|policy| policy.is_tool_allowed(name))
}

pub(crate) fn mcp_allowed_tool_count<'a>(
    names: impl IntoIterator<Item = &'a str>,
    policy: Option<&zeroclaw_tools::tool_search::ToolAccessPolicy>,
) -> usize {
    names
        .into_iter()
        .filter(|name| eager_mcp_tool_allowed(name, policy))
        .count()
}

/// Append a pre-rendered pinned-MCP-resources section onto the system-prompt
/// MCP accumulator (`deferred_section`).
///
/// This MUST be called *after* the `deferred_loading` branch, which reassigns
/// `deferred_section` with `=` (via `build_deferred_tools_section_filtered`)
/// and would otherwise clobber any earlier-pushed pinned content. Centralizing
/// the append keeps both `run()` and `process_message()` consistent and pins
/// the ordering invariant in one testable place. No-op for an empty section.
pub fn append_pinned_mcp_section(deferred_section: &mut String, pinned_section: &str) {
    if pinned_section.is_empty() {
        return;
    }
    deferred_section.push_str("\n\n");
    deferred_section.push_str(pinned_section);
}

/// Register an eager MCP tool wrapper into `tools` only if `policy` admits
/// it. Returns `true` when the tool was registered, `false` when the policy
/// dropped it.
pub fn register_eager_mcp_tool_if_allowed(
    wrapper: std::sync::Arc<dyn Tool>,
    tools: &mut Vec<Box<dyn Tool>>,
    policy: Option<&zeroclaw_tools::tool_search::ToolAccessPolicy>,
) -> bool {
    if !eager_mcp_tool_allowed(wrapper.name(), policy) {
        return false;
    }
    tools.push(Box::new(tools::ArcToolRef(wrapper)));
    true
}

pub(crate) fn preactivate_always_filter_groups(
    deferred: &crate::tools::DeferredMcpToolSet,
    activated: &Arc<Mutex<crate::tools::ActivatedToolSet>>,
    groups: &[zeroclaw_config::schema::ToolFilterGroup],
    policy: Option<&zeroclaw_tools::tool_search::ToolAccessPolicy>,
) -> HashSet<String> {
    use zeroclaw_config::schema::ToolFilterGroupMode;

    let mut activated_names: HashSet<String> = HashSet::new();
    let always_patterns: Vec<&str> = groups
        .iter()
        .filter(|group| matches!(group.mode, ToolFilterGroupMode::Always))
        .flat_map(|group| group.tools.iter().map(String::as_str))
        .collect();
    if always_patterns.is_empty() {
        return activated_names;
    }
    // A poisoned mutex only means another thread panicked mid-update; the
    // activated map itself stays coherent (inserts are atomic), so recover.
    let mut guard = match activated.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    for stub in &deferred.stubs {
        if guard.is_activated(&stub.prefixed_name) {
            continue;
        }
        if !eager_mcp_tool_allowed(&stub.prefixed_name, policy) {
            continue;
        }
        if !always_patterns
            .iter()
            .any(|pat| glob_match(pat, &stub.prefixed_name))
        {
            continue;
        }
        if let Some(tool) = deferred.activate(&stub.prefixed_name) {
            let tool: Arc<dyn Tool> = Arc::from(tool);
            guard.activate(stub.prefixed_name.clone(), tool);
            activated_names.insert(stub.prefixed_name.clone());
        }
    }
    activated_names
}

pub fn filter_tool_specs_for_turn(
    tool_specs: Vec<crate::tools::ToolSpec>,
    groups: &[zeroclaw_config::schema::ToolFilterGroup],
    user_message: &str,
    mcp_tool_names: &HashSet<String>,
) -> Vec<crate::tools::ToolSpec> {
    if groups.is_empty() {
        return tool_specs;
    }

    let msg_lower = user_message.to_ascii_lowercase();

    tool_specs
        .into_iter()
        .filter(|spec| {
            if !mcp_tool_names.contains(&spec.name) {
                return true;
            }
            mcp_tool_included_for_turn(&spec.name, groups, &msg_lower)
        })
        .collect()
}

fn mcp_tool_included_for_turn(
    name: &str,
    groups: &[zeroclaw_config::schema::ToolFilterGroup],
    msg_lower: &str,
) -> bool {
    use zeroclaw_config::schema::ToolFilterGroupMode;

    groups.iter().any(|group| {
        let pattern_matches = group.tools.iter().any(|pat| glob_match(pat, name));
        if !pattern_matches {
            return false;
        }
        match group.mode {
            ToolFilterGroupMode::Always => true,
            ToolFilterGroupMode::Dynamic => group
                .keywords
                .iter()
                .any(|kw| msg_lower.contains(&kw.to_ascii_lowercase())),
        }
    })
}

pub fn filter_by_allowed_tools(
    specs: Vec<crate::tools::ToolSpec>,
    allowed: Option<&[String]>,
) -> Vec<crate::tools::ToolSpec> {
    match allowed {
        None => specs,
        Some(list) => specs
            .into_iter()
            .filter(|spec| list.iter().any(|name| name == &spec.name))
            .collect(),
    }
}

pub(crate) fn compute_excluded_mcp_tools(
    tools_registry: &[Box<dyn Tool>],
    groups: &[zeroclaw_config::schema::ToolFilterGroup],
    user_message: &str,
    mcp_tool_names: &HashSet<String>,
) -> Vec<String> {
    if groups.is_empty() {
        return Vec::new();
    }
    let msg_lower = user_message.to_ascii_lowercase();
    tools_registry
        .iter()
        .map(|t| t.name())
        .filter(|name| {
            mcp_tool_names.contains(*name) && !mcp_tool_included_for_turn(name, groups, &msg_lower)
        })
        .map(str::to_string)
        .collect()
}
