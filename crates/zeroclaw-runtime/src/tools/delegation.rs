//! Shared live authorization for model tools and the bounded edge facade.

use std::path::{Path, PathBuf};

use zeroclaw_config::{schema::Config, tachi::TachiConfig};
use zeroclaw_policy::{PerSenderTracker, SecurityPolicy, ToolOperation};

/// The caller owns approval handling and the tracker. Every ingress applies
/// the same configured agent/card permission, storage identity and limit checks;
/// this helper does not merge independently owned transport/sender counters.
pub fn resolve(
    current: &Config,
    selected_data_dir: &Path,
    agent: &str,
    tracker: &PerSenderTracker,
    name: &str,
    operation: ToolOperation,
) -> Result<(TachiConfig, PathBuf), String> {
    if current.data_dir != selected_data_dir {
        return Err(zeroclaw_tools::tachi_delegation::STORAGE_ROOT_CHANGED.to_string());
    }
    if !current.agents.get(agent).is_some_and(|entry| entry.enabled) {
        return Err("delegation denied by current agent policy".to_string());
    }
    if current.tachi.enabled {
        let mut policy = SecurityPolicy::for_agent(current, agent).map_err(|e| e.to_string())?;
        if !policy.is_tool_allowed(name)
            || (matches!(operation, ToolOperation::Act) && !policy.can_act())
        {
            return Err("delegation denied by current agent policy".to_string());
        }
        policy.tracker = tracker.clone();
        policy.enforce_tool_operation(operation, name)?;
        if current.channels.session_backend != "sqlite" {
            return Err(
                "durable delegation bindings require the SQLite session backend".to_string(),
            );
        }
    }
    Ok((current.tachi.clone(), selected_data_dir.to_path_buf()))
}
