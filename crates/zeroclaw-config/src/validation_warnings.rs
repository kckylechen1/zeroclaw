//! Non-fatal validation warnings — config that loads and validates
//! successfully (i.e. `Config::validate()` returns `Ok(())`) but will fail
//! at agent runtime because of a logical inconsistency the schema can't
//! enforce structurally.

use serde::{Deserialize, Serialize};

/// One non-fatal validation issue surfaced after a successful save.
///
/// Stable codes (extend as new warnings are added):
/// - `memory_semantic_search_without_embedder`: `memory.search_mode` requests
///   vector search on sqlite memory, but no effective embedder is configured.
/// - `memory_config_knob_inert`: a `[memory]` knob is set to a non-default
///   value but has no runtime consumer yet, so it currently has no effect
///   (see `validate_memory_semantics` in `schema.rs` for the current list).
/// - `context_compression_unsupported`: a `runtime_profiles.<alias>.context_compression`
///   knob (`enabled = true`, or any other field set to a non-default value)
///   has no runtime consumer — the context compressor was removed —
///   so it currently has no effect. One warning per non-default field (see
///   `collect_context_compression_ignored_warnings` in `schema.rs`).
/// - `otp_action_gating_unsupported`: a `[security.otp]` action-gating knob
///   (`gated_actions`, `gated_domains`, `gated_domain_categories`, or
///   `challenge_max_attempts`) is set to a non-default value but ZeroClaw
///   has no OTP action-gating, so it is parsed for compatibility and never
///   enforced (see `collect_deprecated_otp_action_gating_warnings` in
///   `schema.rs`); live OTP authentication (`enabled`, `token_ttl_secs`,
///   `cache_valid_secs`) is unaffected.
/// - `gateway_pairing_dashboard_removed`: the `[gateway.pairing_dashboard]`
///   config section was removed from the schema; a `ZEROCLAW_gateway__
///   pairing_dashboard__*` env override (ignored by the env-override
///   tombstone before the unknown-path rejection) or a leftover
///   `[gateway.pairing_dashboard]` file section is ignored and reported via
///   this warning (see `RETIRED_CONFIG_SURFACES` below, consumed by the
///   env-override tombstone and `retired_section_tombstones`). The
///   compatibility shims will be removed in a later announced window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema-export", derive(schemars::JsonSchema))]
pub struct ValidationWarning {
    /// Stable machine-readable identifier for the warning class.
    pub code: String,
    /// Human-readable description suitable for direct display.
    pub message: String,
    /// Dotted property path the warning concerns
    /// (e.g. `"agents.researcher.model_provider"`).
    pub path: String,
}

impl ValidationWarning {
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            path: path.into(),
        }
    }
}

/// Retired config surfaces: dotted section path + stable warning code.
/// Single source of truth for both compatibility shims — the env-override
/// tombstone (`env_overrides.rs`) derives its env-form prefix from the
/// dotted path, and `retired_section_tombstones` matches the path against
/// config-file contents. Each hit is ignored (never applied / silently
/// dropped by serde as an unknown section) and reported as a structured
/// warning so existing deployments keep parsing instead of failing config
/// load after the backing schema was deleted.
///
/// Sunset: these tombstones are compatibility shims; they will be removed
/// in a later announced window, after which the env paths hard-error like
/// any other unknown path and the file sections parse-drop silently.
pub const RETIRED_CONFIG_SURFACES: &[(&str, &str)] = &[(
    "gateway.pairing_dashboard",
    "gateway_pairing_dashboard_removed",
)];

/// Structured tombstone warnings for retired sections still present in a
/// config file. `GatewayConfig`-style structs do not use
/// `deny_unknown_fields`, so serde silently drops an unknown nested
/// section and configs carrying a retired section keep parsing; this makes
/// the retirement visible instead of a silent no-op. Shared by every
/// config-file load path (`Config::load_or_init` and the channels
/// standalone loader) so no live loader can miss it.
pub fn retired_section_tombstones(contents: &str) -> Vec<ValidationWarning> {
    let Ok(root) = toml::from_str::<toml::Value>(contents) else {
        return Vec::new();
    };
    RETIRED_CONFIG_SURFACES
        .iter()
        .filter(|(path, _)| table_path_exists(root.as_table(), path))
        .map(|(path, code)| {
            ValidationWarning::new(
                *code,
                format!(
                    "[{path}] in config.toml is ignored: the section was removed from the \
                     schema and has no runtime consumer. This compatibility shim will be \
                     removed in a later announced window. Remove the section."
                ),
                (*path).to_string(),
            )
        })
        .collect()
}

/// True when a dotted path resolves to a table (section) in the parsed
/// root — a scalar planted where the section used to be is not a section
/// tombstone hit.
fn table_path_exists(root: Option<&toml::value::Table>, path: &str) -> bool {
    let mut current = root;
    for segment in path.split('.') {
        current = match current.and_then(|table| table.get(segment)) {
            Some(value) => value.as_table(),
            None => return false,
        };
    }
    current.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_section_tombstones_flags_section_but_not_scalar_or_absent() {
        let with_section = "[gateway]\npairing_dashboard = { code_length = 8 }\n";
        let warnings = retired_section_tombstones(with_section);
        assert_eq!(warnings.len(), 1, "section hit must warn: {warnings:?}");
        assert_eq!(warnings[0].code, "gateway_pairing_dashboard_removed");
        assert_eq!(warnings[0].path, "gateway.pairing_dashboard");

        // A scalar planted where the section lived is not a section
        // tombstone hit — nothing to keep parsing, nothing to warn about.
        assert!(retired_section_tombstones("[gateway]\npairing_dashboard = 3\n").is_empty());
        assert!(retired_section_tombstones("default_temperature = 0.7\n").is_empty());
        assert!(retired_section_tombstones("not toml {{{").is_empty());
    }

    #[test]
    fn env_tombstone_prefix_is_derived_from_the_retired_surface_table() {
        // SSOT guard: the env-form prefixes used by the walker must be the
        // retired table's dotted paths translated to env form — if a future
        // surface joins the table, both shims pick it up without edits.
        for (path, _) in RETIRED_CONFIG_SURFACES {
            let env_prefix = format!("{}__", path.replace('.', "__"));
            assert!(
                crate::env_overrides::retired_env_tombstone_prefixes()
                    .iter()
                    .any(|(prefix, _)| *prefix == env_prefix),
                "env walker must carry the derived prefix for {env_prefix}"
            );
        }
    }
}
