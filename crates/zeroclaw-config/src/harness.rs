//! External agent harness configurations for ZeroClaw Mission Control.
//!
//! Defines external worker harnesses that ZeroClaw orchestrates:
//! - Direct child process harnesses (OpenAI Codex app-server, ACP agents like DeepSeek Harness, Claude Code)
//! - Durable delegated harnesses (Tachi task bridge via TaskIntent)

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zeroclaw_macros::Configurable;

/// The communication driver / protocol family for the external harness.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HarnessKind {
    /// OpenAI Codex app-server stdio JSON-RPC protocol (`codex app-server --stdio`).
    Codex,
    /// Agent Client Protocol (ACP) JSON-RPC protocol (`dsh --profile acp`, `codex-acp`, etc.).
    #[default]
    Acp,
    /// Tachi task bridge (durable asynchronous TaskIntent delegation).
    Tachi,
    /// Claude Code CLI / Agent SDK.
    ClaudeCode,
    /// Custom external command.
    Custom,
}

/// The dispatch routing preference for this harness.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HarnessRoutePreference {
    /// Automatically determine route: interactive/short turns -> ephemeral, batch/durable -> durable.
    #[default]
    Auto,
    /// Ephemeral direct execution (local child process session).
    Ephemeral,
    /// Durable delegated execution (persisted task intent via task bridge).
    Durable,
}

/// Permission policy for autonomous tool calls from this harness.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HarnessPermissionPolicy {
    /// Deny all permission requests (default fail-closed).
    #[default]
    DenyAll,
    /// Auto-allow permission requests (suitable for autonomous worker subagents).
    AllowAll,
}

/// Resume method for ACP harnesses (`session/load` vs `session/resume`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum HarnessResumeMethod {
    /// Standard ACP / codex-acp `session/load`.
    #[default]
    Load,
    /// DeepSeek Harness `session/resume`.
    Resume,
}

/// Authored definition of one external agent harness (`[harnesses.<id>]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Configurable)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(default, deny_unknown_fields)]
#[prefix = "harness"]
pub struct HarnessDefinition {
    /// The driver kind for this harness (acp, codex, tachi, claude_code, custom).
    pub kind: HarnessKind,
    /// Whether this harness is enabled and available for dispatch.
    pub enabled: bool,
    /// Executable binary path or command name.
    pub command: Option<String>,
    /// Additional arguments passed to the harness executable.
    pub args: Vec<String>,
    /// Environment variables provided to the harness child process.
    pub env: HashMap<String, String>,
    /// Dedicated workspace directory (falls back to runtime workspace if unset).
    pub workspace_root: Option<String>,
    /// Approval preset / mode id passed to harness (e.g. "workspace-write").
    pub session_mode: Option<String>,
    /// Whether the harness supports `session/set_mode` (default: true; false for DSH).
    pub supports_set_mode: bool,
    /// Method used when resuming sessions (`load` vs `resume`).
    pub resume_method: HarnessResumeMethod,
    /// Policy for responding to harness permission requests.
    pub permission_policy: HarnessPermissionPolicy,
    /// Maximum concurrent sessions admitted for this harness.
    pub max_concurrent_sessions: usize,
    /// Task domains handled by this harness (e.g. ["code", "review", "trading"]).
    pub domains: Vec<String>,
    /// Routing preference (auto, ephemeral, durable).
    pub route_preference: HarnessRoutePreference,
    /// Startup / handshake timeout in seconds (default: 60).
    pub startup_timeout_secs: u64,
    /// Single prompt turn timeout in seconds (default: 300).
    pub turn_timeout_secs: u64,
}

impl Default for HarnessDefinition {
    fn default() -> Self {
        Self {
            kind: HarnessKind::default(),
            enabled: true,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            workspace_root: None,
            session_mode: None,
            supports_set_mode: true,
            resume_method: HarnessResumeMethod::default(),
            permission_policy: HarnessPermissionPolicy::default(),
            max_concurrent_sessions: 2,
            domains: Vec::new(),
            route_preference: HarnessRoutePreference::default(),
            startup_timeout_secs: 60,
            turn_timeout_secs: 300,
        }
    }
}

impl HarnessDefinition {
    /// Create a preset definition for DeepSeek Harness (`dsh --profile acp`).
    #[must_use]
    pub fn dsh(command: impl Into<String>) -> Self {
        Self {
            kind: HarnessKind::Acp,
            enabled: true,
            command: Some(command.into()),
            args: vec!["--profile".to_string(), "acp".to_string()],
            env: HashMap::new(),
            workspace_root: None,
            session_mode: None,
            supports_set_mode: false,
            resume_method: HarnessResumeMethod::Resume,
            permission_policy: HarnessPermissionPolicy::AllowAll,
            max_concurrent_sessions: 2,
            domains: vec!["code".to_string(), "refactor".to_string()],
            route_preference: HarnessRoutePreference::Ephemeral,
            startup_timeout_secs: 60,
            turn_timeout_secs: 600,
        }
    }

    /// Create a preset definition for OpenAI Codex app-server.
    #[must_use]
    pub fn codex(command: impl Into<String>) -> Self {
        Self {
            kind: HarnessKind::Codex,
            enabled: true,
            command: Some(command.into()),
            args: vec!["app-server".to_string(), "--stdio".to_string()],
            env: HashMap::new(),
            workspace_root: None,
            session_mode: None,
            supports_set_mode: true,
            resume_method: HarnessResumeMethod::Load,
            permission_policy: HarnessPermissionPolicy::AllowAll,
            max_concurrent_sessions: 2,
            domains: vec!["code".to_string(), "analysis".to_string()],
            route_preference: HarnessRoutePreference::Ephemeral,
            startup_timeout_secs: 60,
            turn_timeout_secs: 600,
        }
    }

    /// Create a preset definition for Tachi task bridge (durable dispatch).
    #[must_use]
    pub fn tachi() -> Self {
        Self {
            kind: HarnessKind::Tachi,
            enabled: true,
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            workspace_root: None,
            session_mode: None,
            supports_set_mode: false,
            resume_method: HarnessResumeMethod::default(),
            permission_policy: HarnessPermissionPolicy::default(),
            max_concurrent_sessions: 4,
            domains: vec![
                "offline".to_string(),
                "batch".to_string(),
                "audit".to_string(),
            ],
            route_preference: HarnessRoutePreference::Durable,
            startup_timeout_secs: 30,
            turn_timeout_secs: 1800,
        }
    }

    /// Create a preset definition for Claude Code.
    #[must_use]
    pub fn claude_code(command: impl Into<String>) -> Self {
        Self {
            kind: HarnessKind::ClaudeCode,
            enabled: true,
            command: Some(command.into()),
            args: Vec::new(),
            env: HashMap::new(),
            workspace_root: None,
            session_mode: None,
            supports_set_mode: false,
            resume_method: HarnessResumeMethod::default(),
            permission_policy: HarnessPermissionPolicy::AllowAll,
            max_concurrent_sessions: 2,
            domains: vec!["code".to_string(), "review".to_string()],
            route_preference: HarnessRoutePreference::Ephemeral,
            startup_timeout_secs: 60,
            turn_timeout_secs: 600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_harness_definition_defaults() {
        let def = HarnessDefinition::default();
        assert_eq!(def.kind, HarnessKind::Acp);
        assert!(def.enabled);
        assert!(def.command.is_none());
        assert!(def.supports_set_mode);
        assert_eq!(def.resume_method, HarnessResumeMethod::Load);
        assert_eq!(def.permission_policy, HarnessPermissionPolicy::DenyAll);
        assert_eq!(def.route_preference, HarnessRoutePreference::Auto);
    }

    #[test]
    fn test_dsh_preset() {
        let def = HarnessDefinition::dsh("dsh");
        assert_eq!(def.kind, HarnessKind::Acp);
        assert_eq!(def.command, Some("dsh".to_string()));
        assert_eq!(def.args, vec!["--profile", "acp"]);
        assert!(!def.supports_set_mode);
        assert_eq!(def.resume_method, HarnessResumeMethod::Resume);
        assert_eq!(def.permission_policy, HarnessPermissionPolicy::AllowAll);
        assert_eq!(def.route_preference, HarnessRoutePreference::Ephemeral);
    }

    #[test]
    fn test_tachi_preset() {
        let def = HarnessDefinition::tachi();
        assert_eq!(def.kind, HarnessKind::Tachi);
        assert_eq!(def.route_preference, HarnessRoutePreference::Durable);
        assert_eq!(def.max_concurrent_sessions, 4);
    }

    #[test]
    fn test_toml_deserialization() {
        let toml_str = r#"
            kind = "acp"
            enabled = true
            command = "/usr/local/bin/dsh"
            args = ["--profile", "acp"]
            supports_set_mode = false
            resume_method = "resume"
            permission_policy = "allow_all"
            domains = ["code", "refactor"]
            route_preference = "ephemeral"
        "#;
        let def: HarnessDefinition = toml::from_str(toml_str).expect("deserialize harness");
        assert_eq!(def.kind, HarnessKind::Acp);
        assert_eq!(def.command.as_deref(), Some("/usr/local/bin/dsh"));
        assert_eq!(def.resume_method, HarnessResumeMethod::Resume);
        assert_eq!(def.permission_policy, HarnessPermissionPolicy::AllowAll);
        assert_eq!(def.route_preference, HarnessRoutePreference::Ephemeral);
        assert_eq!(def.domains, vec!["code", "refactor"]);
    }
}
