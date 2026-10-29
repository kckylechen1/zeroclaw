//! Attachment-scoped bridge identity. Live reductions apply immediately;
//! widening policy or removing a restriction never elevates an old socket.
use super::*;
use zeroclaw_config::schema::{Config, GatewayBridgeConfig};

#[derive(Clone)]
pub(super) struct Binding {
    pub name: String,
    pub initial: GatewayBridgeConfig,
}

impl Binding {
    pub fn restricted(&self) -> bool {
        self.initial.scoped()
    }

    pub fn allows(&self, config: &Config, agent: &str, session: &str, operation: &str) -> bool {
        let Some(current) = config.gateway.bridges.get(&self.name) else {
            return false;
        };
        let hash = self.initial.token_hash.to_ascii_lowercase();
        if !zeroclaw_config::tokens::constant_time_eq(
            &hash,
            &current.token_hash.to_ascii_lowercase(),
        ) || config
            .gateway
            .bridges
            .values()
            .filter(|bridge| {
                zeroclaw_config::tokens::constant_time_eq(
                    &hash,
                    &bridge.token_hash.to_ascii_lowercase(),
                )
            })
            .count()
            != 1
            || !self.initial.allows_session(session)
            || !current.allows_session(session)
            || (self.initial.device.is_some() != current.device.is_some())
            || (self.initial.owner.is_some() != current.owner.is_some())
        {
            return false;
        }
        [&self.initial, current].into_iter().all(|bridge| {
            if bridge.device.is_some() && bridge.owner.is_some() {
                return false;
            }
            if !bridge.scoped() {
                return true;
            }
            !session.is_empty()
                && zeroclaw_api::session_keys::sanitize_session_key(session) == session
                && config.agents.get(agent).is_some_and(|entry| entry.enabled)
                && bridge
                    .device
                    .as_ref()
                    .is_none_or(|device| device.allows(agent, operation))
                && bridge
                    .owner
                    .as_ref()
                    .is_none_or(|owner| owner.allows(agent, operation))
        })
    }
}

pub(super) fn permitted(
    state: &AppState,
    scope: &WsTurnScope,
    agent: &str,
    operation: &str,
) -> bool {
    let config = state.config.read();
    authenticated_mode(state, &config)
        && scope
            .bridge
            .as_ref()
            .is_none_or(|binding| binding.allows(&config, agent, &scope.session_id, operation))
}

/// Do not let a restricted caller drop its bearer and enter anonymous owner chat.
/// Also closes old anonymous sockets after an incompatible policy reload.
pub(super) fn authenticated_mode(state: &AppState, config: &Config) -> bool {
    state.pairing.require_pairing()
        || config
            .gateway
            .bridges
            .values()
            .all(|bridge| !bridge.scoped())
}

pub(super) fn frame_operation(frame: &serde_json::Value) -> &'static str {
    match frame["type"].as_str() {
        Some("message")
            if frame.get("source").is_none()
                && frame
                    .get("attachments")
                    .is_none_or(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty)) =>
        {
            "chat"
        }
        Some("answer") => "answer",
        Some("cancel") => "cancel",
        Some("approval_response")
            if matches!(frame["decision"].as_str(), Some("approve" | "deny")) =>
        {
            "approval_response"
        }
        _ => "unsupported",
    }
}

/// Restricted clients cannot create a second memory identity or choose an OS root.
pub(super) fn connect_matches(frame: &serde_json::Value, agent: &str, session: &str) -> bool {
    ["session_id"]
        .into_iter()
        .all(|key| frame.get(key).is_none_or(|v| v.as_str() == Some(session)))
        && ["agent", "agent_alias", "agentAlias"]
            .into_iter()
            .all(|key| frame.get(key).is_none_or(|v| v.as_str() == Some(agent)))
        && ["cwd", "workspace_dir", "workspaceDir"]
            .into_iter()
            .all(|key| frame.get(key).is_none())
}

pub(super) fn refusal(code: &str) -> serde_json::Value {
    serde_json::json!({"type":"error", "code":code,
        "message":zeroclaw_runtime::i18n::get_required_cli_string("gateway-bridge-access-denied")})
}
