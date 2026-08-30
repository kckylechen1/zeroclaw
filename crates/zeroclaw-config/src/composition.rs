//! Install-wide tool composition selector (`composition`).
//!
//! The composition names which tool surface an install assembles. It is the
//! single canonical source for minimal-profile membership: assembly retains
//! only members of [`MINIMAL_TOOL_MEMBERSHIP`] under `minimal`, and the CI
//! wire-surface guard will consume the same table once it lands, so there
//! is no second list to drift.
//!
//! Semantics (owner-ratified):
//!
//! - `composition = "minimal"` — assemble only the explicit membership table.
//!   Individual tool `enabled` flags do not widen it back; non-members are
//!   excluded with a warning instead.
//! - `composition = "full"` (alias `legacy`) — today's assembly. This is a
//!   transitional opt-in compatibility profile, not a target.
//! - field absent — resolve as `full`. Existing installs must not silently
//!   lose tools on upgrade; reinterpreting absence is a separate, later
//!   migration decision.
//!
//! Membership is an allowlist on purpose: a newly registered tool is absent
//! from the minimal surface until it is added here, so the fail direction is
//! closed. Do not add placeholder tools to hit a count; entries follow
//! measured provider-wire cost.

use serde::{Deserialize, Serialize};

/// The minimal companion profile's explicit tool membership.
///
/// Entries are model-visible tool names as registered by the assembly. The
/// initial set is anchored on the measured lean assembly (12 tools, ~4k
/// provider-wire tokens) plus the companion primitives the minimal profile
/// must preserve: bounded user interaction and the SubAgent entry point.
/// The Tachi bridge seam joins here when its tool lands; scheduling stays
/// one compact primitive rather than the full cron family.
pub const MINIMAL_TOOL_MEMBERSHIP: &[&str] = &[
    // conversation/session/context — workspace basics
    "shell",
    "file_read",
    "file_write",
    "file_edit",
    "glob_search",
    "content_search",
    // personal-memory domain access
    "memory_recall",
    "memory_store",
    // skill discovery/read/applicability
    "read_skill",
    // extension discovery under effective policy
    "tool_search",
    // universal web primitives
    "web_search_tool",
    "web_fetch",
    // bounded interaction / ask-user behavior
    "ask_user",
    // reasoning/supervisor SubAgent entry point (V1; the sole spawn
    // surface — the legacy `spawn_subagent` is retired)
    "reasoning_subagent",
    // attention/scheduling semantics
    "schedule",
];

/// The documented values accepted for the root `composition` key, joined
/// for error messages. Kept beside the enum so the accepted set and its
/// human-facing spelling cannot drift apart; the enum's own deserializer
/// remains the single source of truth for validity.
pub const DOCUMENTED_VALUES: &str = "\"minimal\", \"full\", or \"legacy\" (alias of \"full\")";

/// Install-wide composition selector value.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, zeroclaw_macros::ConfigEnum,
)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Composition {
    /// Assemble only [`MINIMAL_TOOL_MEMBERSHIP`].
    Minimal,
    /// Today's assembly — a transitional opt-in compatibility profile.
    #[serde(alias = "legacy")]
    #[default]
    Full,
}

impl Composition {
    /// Resolve the effective composition from the configured value.
    ///
    /// `None` (field absent) resolves to [`Composition::Full`]: existing
    /// installs keep today's assembly across upgrades.
    pub fn effective(configured: Option<Composition>) -> Composition {
        configured.unwrap_or_default()
    }

    /// Whether `name` is a member of the minimal companion profile.
    pub fn is_minimal_member(name: &str) -> bool {
        is_minimal_member(name)
    }
}

/// Membership check against [`MINIMAL_TOOL_MEMBERSHIP`].
pub fn is_minimal_member(name: &str) -> bool {
    MINIMAL_TOOL_MEMBERSHIP.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composition_parses_documented_values() {
        assert_eq!(
            serde_json::from_str::<Composition>("\"minimal\"").unwrap(),
            Composition::Minimal
        );
        assert_eq!(
            serde_json::from_str::<Composition>("\"full\"").unwrap(),
            Composition::Full
        );
        // Transitional alias: explicit legacy configs keep full semantics.
        assert_eq!(
            serde_json::from_str::<Composition>("\"legacy\"").unwrap(),
            Composition::Full
        );
        assert!(serde_json::from_str::<Composition>("\"everything\"").is_err());
    }

    #[test]
    fn absent_field_resolves_full_and_explicit_wins() {
        assert_eq!(Composition::effective(None), Composition::Full);
        assert_eq!(
            Composition::effective(Some(Composition::Minimal)),
            Composition::Minimal
        );
        assert_eq!(
            Composition::effective(Some(Composition::Full)),
            Composition::Full
        );
    }

    #[test]
    fn root_config_field_wiring() {
        // Config's TOML parse requires these tables; mirror the lean-profile
        // parse helper's minimum fragment.
        let tables =
            "\n[data_retention]\n[cloud_ops]\n[conversational_ai]\n[security]\n[security_ops]\n";

        let absent: crate::schema::Config =
            toml::from_str(tables).expect("absent composition must parse");
        assert!(absent.composition.is_none());
        assert_eq!(
            Composition::effective(absent.composition),
            Composition::Full
        );

        let minimal: crate::schema::Config =
            toml::from_str(&format!("composition = \"minimal\"\n{tables}"))
                .expect("explicit minimal must parse");
        assert_eq!(minimal.composition, Some(Composition::Minimal));

        let legacy: crate::schema::Config =
            toml::from_str(&format!("composition = \"legacy\"\n{tables}"))
                .expect("legacy alias must parse");
        assert_eq!(legacy.composition, Some(Composition::Full));
    }

    #[test]
    fn membership_table_is_exact_and_sorted_by_concern() {
        // Every entry distinct; no accidental duplicates that would mask a
        // real membership decision.
        let mut sorted = MINIMAL_TOOL_MEMBERSHIP.to_vec();
        sorted.sort_unstable();
        let deduped_len = {
            let mut seen = std::collections::HashSet::new();
            sorted.iter().filter(|n| seen.insert(**n)).count()
        };
        assert_eq!(deduped_len, MINIMAL_TOOL_MEMBERSHIP.len());
        assert!(is_minimal_member("file_read"));
        // The minimal composition fronts the V1 SubAgent entrypoint; the
        // legacy `spawn_subagent` is retired on every composition.
        assert!(is_minimal_member("reasoning_subagent"));
        assert!(!is_minimal_member("spawn_subagent"));
        assert!(!is_minimal_member("model_routing_config"));
        assert!(!is_minimal_member("claude_code"));
        assert!(!is_minimal_member("delegate"));
        assert!(!is_minimal_member(""));
    }

    /// Banned tool categories for the minimal companion profile, as
    /// registered tool names. This list is the CI wire-surface tripwire's
    /// single explicit input: additions to it are reviewed via the diff on
    /// this file, and a name joining the membership table is a widening
    /// change that this test exists to catch.
    const BANNED_MINIMAL_TOOL_NAMES: &[&str] = &[
        // direct coding-harness launchers
        "claude_code",
        "claude_code_runner",
        "codex_cli",
        "gemini_cli",
        "opencode_cli",
        "coding_cli",
        "coding_cli_executor",
        "browser_delegate",
        // repo/git mutation
        "git_operations",
        "git_forge",
        // operator/admin mutation
        "model_routing_config",
        "proxy_config",
        "security_ops",
        "backup",
        "data_management",
        // concrete SaaS/vendor business adapters
        "jira",
        "notion",
        "google_workspace",
        "microsoft365",
        "linkedin",
        "composio",
        "pushover",
    ];

    /// Banned name prefixes (category-level bans that cover families).
    const BANNED_MINIMAL_TOOL_NAME_PREFIXES: &[&str] = &["hardware_"];

    #[test]
    fn membership_table_admits_no_banned_category() {
        // Tripwire: nobody adds a banned name (or a banned-prefix family)
        // to the membership table. The runtime's assembly totality test
        // proves assembly stays within the table, so table purity is what
        // keeps these categories off the minimal provider wire.
        for banned in BANNED_MINIMAL_TOOL_NAMES {
            assert!(
                !is_minimal_member(banned),
                "`{banned}` is a banned category and must never join the minimal membership table"
            );
        }
        for member in MINIMAL_TOOL_MEMBERSHIP {
            for prefix in BANNED_MINIMAL_TOOL_NAME_PREFIXES {
                assert!(
                    !member.starts_with(prefix),
                    "`{member}` matches banned prefix `{prefix}` and must never join the minimal membership table"
                );
            }
        }
    }

    #[test]
    fn no_bypass_via_skill_or_prefixed_names() {
        // Excluded and banned tools cannot be elevated by skill prefixing or
        // alias disguising. `is_minimal_member` must reject them.
        for banned in BANNED_MINIMAL_TOOL_NAMES {
            assert!(!is_minimal_member(&format!("skill__{banned}")));
            assert!(!is_minimal_member(&format!("custom__{banned}")));
        }
    }

    #[test]
    fn no_bypass_subagent_and_spawn_surfaces() {
        // Only the V1 reasoning_subagent is admitted; legacy spawn_subagent,
        // direct coding runners, and arbitrary subagent elevation stay denied.
        assert!(is_minimal_member("reasoning_subagent"));
        assert!(!is_minimal_member("spawn_subagent"));
        assert!(!is_minimal_member("coding_subagent"));
        assert!(!is_minimal_member("exec_subagent"));
    }
}
