//! Quickstart selection policy: preset catalogue, factories and recommendations.

use zeroclaw_config::autonomy::AutonomyLevel;
use zeroclaw_config::autonomy::McpDiscoveredToolPolicy;
use zeroclaw_config::policy_defaults::{default_allowed_commands, default_forbidden_paths};
use zeroclaw_config::schema::{RiskProfileConfig, RuntimeProfileConfig};

// ─────────────────────────────────────────────────────────────────────
// Risk presets
// ─────────────────────────────────────────────────────────────────────

/// One row in the Risk preset table. The Quickstart UI renders the
/// `label`, the runtime writes `values` to
/// `risk-profiles.<preset_name>` on apply.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RiskPreset {
    /// Alias key written to `risk-profiles.<preset_name>`. Doubles as
    /// the stable wire identifier (`BuilderSubmission.risk_preset`).
    pub preset_name: &'static str,
    /// Short label rendered in the picker UI.
    pub label: &'static str,
    /// One-line help blurb rendered next to the label.
    pub help: &'static str,
    /// Factory that produces the [`RiskProfileConfig`] this preset
    /// installs. A function (not a const value) because
    /// `RiskProfileConfig` has owned `Vec<String>` fields that cannot
    /// live in a `const`.
    #[serde(skip)]
    pub values: fn() -> RiskProfileConfig,
}

/// Canonical Risk preset table. Order is the order the picker
/// renders. Add or remove a preset by editing one row here; every
/// consumer reads from the slice so nothing else has to change.
pub const RISK_PRESETS: &[RiskPreset] = &[
    RiskPreset {
        preset_name: "locked_down",
        label: "Locked Down",
        help: "Tightest defaults. Workspace-only filesystem access, approval \
               required for medium and high risk, no shell environment passthrough.",
        values: locked_down_risk,
    },
    RiskPreset {
        preset_name: "balanced",
        label: "Balanced",
        help: "Trusted daily driver for a personal dev box. Supervised, \
               workspace-scoped with sensitive paths blocked and the sandbox \
               on. Any command runs without an allowlist, but high-risk \
               commands stay blocked unless explicitly allowlisted. \
               Recommended for most users.",
        values: balanced_risk,
    },
    RiskPreset {
        preset_name: "yolo",
        label: "YOLO",
        help: "Full autonomy. No approval gates, no command denylist, no \
               workspace scoping. Only pick this if you know what you're \
               doing on a machine you don't mind breaking.",
        values: yolo_risk,
    },
];

/// Look up a Risk preset by its `preset_name`. Returns `None` for
/// unknown keys.
#[must_use]
pub fn risk_preset(preset_name: &str) -> Option<&'static RiskPreset> {
    RISK_PRESETS.iter().find(|p| p.preset_name == preset_name)
}

fn locked_down_risk() -> RiskProfileConfig {
    RiskProfileConfig {
        level: AutonomyLevel::Supervised,
        workspace_only: true,
        allowed_commands: default_allowed_commands(),
        forbidden_paths: default_forbidden_paths(),
        require_approval_for_medium_risk: true,
        block_high_risk_commands: true,
        shell_env_passthrough: vec![],
        auto_approve: vec![],
        always_ask: vec![],
        allowed_roots: vec![],
        approval_route: None,
        allowed_tools: None,
        excluded_tools: vec![],
        mcp_discovered_tool_policy: McpDiscoveredToolPolicy::default(),
        sandbox_enabled: Some(true),
        sandbox_backend: None,
        firejail_args: vec![],
    }
}

fn balanced_risk() -> RiskProfileConfig {
    RiskProfileConfig {
        level: AutonomyLevel::Supervised,
        workspace_only: true,
        allowed_commands: vec!["*".to_string()],
        forbidden_paths: default_forbidden_paths(),
        require_approval_for_medium_risk: false,
        block_high_risk_commands: true,
        shell_env_passthrough: vec![],
        auto_approve: vec![],
        always_ask: vec![],
        allowed_roots: vec![],
        approval_route: None,
        allowed_tools: None,
        excluded_tools: vec![],
        mcp_discovered_tool_policy: McpDiscoveredToolPolicy::default(),
        sandbox_enabled: Some(true),
        sandbox_backend: None,
        firejail_args: vec![],
    }
}

fn yolo_risk() -> RiskProfileConfig {
    RiskProfileConfig {
        level: AutonomyLevel::Full,
        workspace_only: false,
        allowed_commands: vec!["*".to_string()],
        forbidden_paths: vec![],
        require_approval_for_medium_risk: false,
        block_high_risk_commands: false,
        shell_env_passthrough: vec![],
        auto_approve: vec!["*".to_string()],
        always_ask: vec![],
        allowed_roots: vec![],
        approval_route: None,
        allowed_tools: None,
        excluded_tools: vec![],
        mcp_discovered_tool_policy: McpDiscoveredToolPolicy::default(),
        sandbox_enabled: Some(false),
        sandbox_backend: None,
        firejail_args: vec![],
    }
}

// ─────────────────────────────────────────────────────────────────────
// Runtime presets
// ─────────────────────────────────────────────────────────────────────

/// Canonical fallback used when no provider-specific runtime recommendation exists.
pub const DEFAULT_RUNTIME_PRESET_NAME: &str = "unbounded";

/// Canonical compact runtime recommended by local inference backends.
pub const LOCAL_SMALL_RUNTIME_PRESET_NAME: &str = "local_small";

/// One row in the Runtime preset table. Same shape and contract as
/// [`RiskPreset`] — see its docs for the per-field semantics.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct RuntimePreset {
    /// Alias key written to `runtime-profiles.<preset_name>`. Doubles
    /// as the stable wire identifier (`BuilderSubmission.runtime_preset`).
    pub preset_name: &'static str,
    /// Short label rendered in the picker UI.
    pub label: &'static str,
    /// One-line help blurb rendered next to the label.
    pub help: &'static str,
    /// Factory that produces the [`RuntimeProfileConfig`] this preset
    /// installs.
    #[serde(skip)]
    pub values: fn() -> RuntimeProfileConfig,
}

/// Canonical Runtime preset table. See [`RISK_PRESETS`] for the
/// ordering / consumer contract.
pub const RUNTIME_PRESETS: &[RuntimePreset] = &[
    RuntimePreset {
        preset_name: "tight",
        label: "Tight",
        help: "Small budgets and short timeouts. Good for cheap models, \
               metered API keys, or tight feedback loops where you want \
               the agent to stop and ask early rather than burn budget.",
        values: tight_runtime,
    },
    RuntimePreset {
        preset_name: LOCAL_SMALL_RUNTIME_PRESET_NAME,
        label: "Local Small",
        help: "Compact no-text-fallback profile for smaller local models. \
               Keeps context and tool results small, disables parallel tool \
               fan-out, and requires native or structured tool calls.",
        values: local_small_runtime,
    },
    RuntimePreset {
        preset_name: "balanced",
        label: "Balanced",
        help: "Middle-of-the-road operational defaults. Suits most users \
               most of the time.",
        values: balanced_runtime,
    },
    RuntimePreset {
        preset_name: DEFAULT_RUNTIME_PRESET_NAME,
        label: "Unbounded",
        help: "Wide-open budgets and long timeouts. Pick this when you're \
               actively driving the agent through a hard task and don't \
               want it to throttle.",
        values: unbounded_runtime,
    },
];

/// Look up a Runtime preset by its `preset_name`. Returns `None` for
/// unknown keys.
#[must_use]
pub fn runtime_preset(preset_name: &str) -> Option<&'static RuntimePreset> {
    RUNTIME_PRESETS
        .iter()
        .find(|p| p.preset_name == preset_name)
}

/// Resolve an optional provider recommendation against the canonical preset table.
/// Unknown or absent recommendations fall back to `unbounded`, then to the first
/// available preset so consumers never duplicate fallback policy.
#[must_use]
pub fn recommended_runtime_preset(recommendation: Option<&str>) -> Option<&'static RuntimePreset> {
    recommendation
        .and_then(runtime_preset)
        .or_else(|| runtime_preset(DEFAULT_RUNTIME_PRESET_NAME))
        .or_else(|| RUNTIME_PRESETS.first())
}

fn tight_runtime() -> RuntimeProfileConfig {
    RuntimeProfileConfig {
        agentic: false,
        max_tool_iterations: 10,
        max_actions_per_hour: 10,
        max_cost_per_day_cents: 100,
        shell_timeout_secs: 30,
        max_history_messages: Some(20),
        max_context_tokens: Some(8_000),
        compact_context: Some(true),
        parallel_tools: Some(false),
        tool_dispatcher: None,
        tool_call_dedup_exempt: vec![],
        max_system_prompt_chars: Some(4_000),
        max_tool_result_chars: Some(8_000),
        keep_tool_context_turns: Some(2),
        memory_recall_limit: Some(3),
        ..RuntimeProfileConfig::default()
    }
}

fn local_small_runtime() -> RuntimeProfileConfig {
    RuntimeProfileConfig {
        agentic: true,
        max_tool_iterations: 4,
        max_actions_per_hour: 10,
        max_cost_per_day_cents: 100,
        shell_timeout_secs: 30,
        max_history_messages: Some(20),
        max_context_tokens: Some(8_000),
        compact_context: Some(true),
        parallel_tools: Some(false),
        tool_dispatcher: None,
        tool_call_dedup_exempt: vec![],
        max_system_prompt_chars: Some(4_000),
        max_tool_result_chars: Some(4_000),
        keep_tool_context_turns: Some(1),
        memory_recall_limit: Some(3),
        strict_tool_parsing: true,
        ..RuntimeProfileConfig::default()
    }
}

fn balanced_runtime() -> RuntimeProfileConfig {
    // Schema default is already the Balanced shape. Use it directly so
    // the preset can't drift from the schema default.
    RuntimeProfileConfig::default()
}

fn unbounded_runtime() -> RuntimeProfileConfig {
    RuntimeProfileConfig {
        agentic: true,
        max_tool_iterations: 100,
        max_actions_per_hour: u32::MAX,
        max_cost_per_day_cents: u32::MAX,
        shell_timeout_secs: 600,
        max_history_messages: Some(200),
        max_context_tokens: Some(128_000),
        compact_context: Some(false),
        parallel_tools: Some(true),
        tool_dispatcher: None,
        tool_call_dedup_exempt: vec![],
        max_system_prompt_chars: Some(64_000),
        max_tool_result_chars: Some(64_000),
        keep_tool_context_turns: Some(8),
        memory_recall_limit: Some(10),
        ..RuntimeProfileConfig::default()
    }
}

/// Provider-specific runtime recommendations. This is deliberately separate
/// from `local`: credential-free CLI shims can call cloud models, and local
/// providers without native tool support must retain text-fallback prompting.
#[must_use]
pub fn recommended_runtime_profile(name: &str) -> Option<&'static str> {
    match name {
        "lmstudio" | "llamacpp" | "sglang" | "vllm" | "osaurus" => {
            Some(LOCAL_SMALL_RUNTIME_PRESET_NAME)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_providers::{create_model_provider, list_model_providers};

    #[test]
    fn risk_preset_names_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for p in RISK_PRESETS {
            assert!(
                seen.insert(p.preset_name),
                "duplicate risk preset_name: {}",
                p.preset_name
            );
        }
    }

    #[test]
    fn runtime_preset_names_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for p in RUNTIME_PRESETS {
            assert!(
                seen.insert(p.preset_name),
                "duplicate runtime preset_name: {}",
                p.preset_name
            );
        }
    }

    #[test]
    fn risk_preset_lookup_round_trips() {
        for p in RISK_PRESETS {
            let found = risk_preset(p.preset_name).expect("preset present");
            assert_eq!(found.preset_name, p.preset_name);
            assert_eq!(found.label, p.label);
        }
        assert!(risk_preset("not-a-real-preset").is_none());
    }

    #[test]
    fn runtime_preset_lookup_round_trips() {
        for p in RUNTIME_PRESETS {
            let found = runtime_preset(p.preset_name).expect("preset present");
            assert_eq!(found.preset_name, p.preset_name);
            assert_eq!(found.label, p.label);
        }
        assert!(runtime_preset("not-a-real-preset").is_none());
    }

    #[test]
    fn no_preset_uses_default_alias() {
        for p in RISK_PRESETS {
            assert_ne!(
                p.preset_name, "default",
                "risk preset alias must never be `default`",
            );
        }
        for p in RUNTIME_PRESETS {
            assert_ne!(
                p.preset_name, "default",
                "runtime preset alias must never be `default`",
            );
        }
    }

    #[test]
    fn preset_names_are_valid_alias_keys() {
        for p in RISK_PRESETS {
            zeroclaw_config::helpers::validate_alias_key(p.preset_name).unwrap_or_else(|e| {
                panic!(
                    "risk preset_name `{}` is not a valid alias key: {e}",
                    p.preset_name
                )
            });
        }
        for p in RUNTIME_PRESETS {
            zeroclaw_config::helpers::validate_alias_key(p.preset_name).unwrap_or_else(|e| {
                panic!(
                    "runtime preset_name `{}` is not a valid alias key: {e}",
                    p.preset_name
                )
            });
        }
    }

    #[test]
    fn balanced_risk_is_trusted_local_shape() {
        let preset = risk_preset("balanced").unwrap();
        let v = (preset.values)();
        // Supervised, workspace-scoped, sandbox on: a trusted personal dev box.
        assert_eq!(v.level, AutonomyLevel::Supervised);
        assert!(v.workspace_only);
        assert_eq!(v.sandbox_enabled, Some(true));
        // Any command runs without an allowlist, but high-risk is blocked, not
        // prompted: the `*` wildcard is not an explicit exemption, so
        // block_high_risk_commands rejects high-risk commands outright while
        // medium-risk friction is off.
        assert_eq!(v.allowed_commands, vec!["*".to_string()]);
        assert!(v.block_high_risk_commands);
        assert!(!v.require_approval_for_medium_risk);
    }

    #[test]
    fn balanced_runtime_matches_schema_default() {
        let preset = runtime_preset("balanced").unwrap();
        let preset_values = (preset.values)();
        let schema_default = RuntimeProfileConfig::default();
        assert_eq!(format!("{preset_values:?}"), format!("{schema_default:?}"),);
    }

    #[test]
    fn local_small_runtime_matches_documented_small_model_shape() {
        let preset = runtime_preset("local_small").expect("local_small preset");
        let values = (preset.values)();

        assert!(values.agentic);
        assert_eq!(values.max_tool_iterations, 4);
        assert_eq!(values.max_actions_per_hour, 10);
        assert_eq!(values.max_cost_per_day_cents, 100);
        assert_eq!(values.shell_timeout_secs, 30);
        assert_eq!(values.max_history_messages, Some(20));
        assert_eq!(values.max_context_tokens, Some(8_000));
        assert_eq!(values.compact_context, Some(true));
        assert_eq!(values.parallel_tools, Some(false));
        assert_eq!(values.max_system_prompt_chars, Some(4_000));
        assert_eq!(values.max_tool_result_chars, Some(4_000));
        assert_eq!(values.keep_tool_context_turns, Some(1));
        assert_eq!(values.memory_recall_limit, Some(3));
        assert!(values.strict_tool_parsing);
    }

    #[test]
    fn runtime_recommendation_resolution_uses_canonical_fallback() {
        assert_eq!(
            recommended_runtime_preset(Some(LOCAL_SMALL_RUNTIME_PRESET_NAME))
                .map(|preset| preset.preset_name),
            Some(LOCAL_SMALL_RUNTIME_PRESET_NAME),
        );
        for recommendation in [None, Some("unknown-runtime-profile")] {
            assert_eq!(
                recommended_runtime_preset(recommendation).map(|preset| preset.preset_name),
                Some(DEFAULT_RUNTIME_PRESET_NAME),
            );
        }
    }

    #[test]
    fn local_small_runtime_resolves_to_strict_compact_agent_policy() {
        let preset = runtime_preset("local_small").expect("local_small preset");
        let mut config = zeroclaw_config::schema::Config::default();
        config
            .runtime_profiles
            .insert("local_small".into(), (preset.values)());
        config.agents.insert(
            "local_agent".into(),
            zeroclaw_config::schema::AliasedAgentConfig {
                runtime_profile: zeroclaw_config::providers::RuntimeProfileRef::new("local_small"),
                ..zeroclaw_config::schema::AliasedAgentConfig::default()
            },
        );

        let resolved = config
            .resolved_agent_config("local_agent")
            .expect("agent should resolve");

        assert!(resolved.resolved.strict_tool_parsing);
        assert_eq!(resolved.resolved.max_tool_iterations, 4);
        assert_eq!(resolved.resolved.max_history_messages, 20);
        assert_eq!(resolved.resolved.max_context_tokens, 8_000);
        assert!(resolved.resolved.compact_context);
        assert!(!resolved.resolved.parallel_tools);
        assert_eq!(resolved.resolved.max_system_prompt_chars, 4_000);
        assert_eq!(resolved.resolved.max_tool_result_chars, 4_000);
        assert_eq!(resolved.resolved.keep_tool_context_turns, 1);
        assert_eq!(config.effective_memory_recall_limit("local_agent"), 3);
    }

    #[test]
    fn yolo_risk_auto_approves_everything_with_no_forbidden_paths() {
        let preset = risk_preset("yolo").unwrap();
        let values = (preset.values)();
        assert_eq!(
            values.auto_approve,
            vec!["*".to_string()],
            "yolo must auto-approve every tool via the `*` wildcard",
        );
        assert!(
            values.forbidden_paths.is_empty(),
            "yolo must have no forbidden paths",
        );
        assert!(
            values.always_ask.is_empty(),
            "yolo must never force an approval prompt",
        );
    }

    #[test]
    fn runtime_recommendations_distinguish_local_backends_from_cli_shims() {
        let providers = list_model_providers();
        let recommendation = |name| {
            providers
                .iter()
                .find(|provider| provider.name == name)
                .and_then(|provider| recommended_runtime_profile(provider.name))
        };

        assert_eq!(
            recommendation("lmstudio"),
            Some(LOCAL_SMALL_RUNTIME_PRESET_NAME),
        );
        assert_eq!(recommendation("ollama"), None);
        assert_eq!(recommendation("atomic_chat"), None);
        assert_eq!(recommendation("gemini_cli"), None);
        assert_eq!(recommendation("kilocli"), None);
        assert_eq!(recommendation("anthropic"), None);
    }

    #[test]
    fn recommended_runtime_profiles_require_native_tool_support() {
        for provider in list_model_providers()
            .into_iter()
            .filter(|provider| recommended_runtime_profile(provider.name).is_some())
        {
            let instance = create_model_provider(provider.name, None).unwrap_or_else(|error| {
                panic!(
                    "recommended local provider {} should construct without credentials: {error}",
                    provider.name,
                )
            });
            assert!(
                instance.supports_native_tools(),
                "provider {} must not receive strict local_small defaults without native tools",
                provider.name,
            );
        }
    }
}
