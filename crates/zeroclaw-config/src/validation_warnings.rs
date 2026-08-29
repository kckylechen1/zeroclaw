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
pub const RETIRED_CONFIG_SURFACES: &[(&str, &str)] = &[
    (
        "gateway.pairing_dashboard",
        "gateway_pairing_dashboard_removed",
    ),
    ("delegate", "delegate_config_removed"),
    ("claude_code", "raw_launcher_config_removed"),
    ("claude_code_runner", "raw_launcher_config_removed"),
    ("codex_cli", "raw_launcher_config_removed"),
    ("gemini_cli", "raw_launcher_config_removed"),
    ("opencode_cli", "raw_launcher_config_removed"),
    ("browser_delegate", "raw_launcher_config_removed"),
];

/// Retired config FIELDS: dotted path (one `*` wildcard segment allowed for
/// map-keyed aliases, e.g. `[agents.<alias>]`) + stable warning code. Same
/// contract as `RETIRED_CONFIG_SURFACES`, one level deeper: the field was
/// deleted from its struct, serde silently drops the unknown key, and this
/// checker makes the retirement visible instead of a silent no-op.
pub const RETIRED_CONFIG_FIELDS: &[(&str, &str)] = &[
    ("agents.*.delegates", "delegate_config_removed"),
    (
        "agents.*.delegate_same_risk_profile",
        "delegate_config_removed",
    ),
    (
        "risk_profiles.*.delegation_policy",
        "delegate_config_removed",
    ),
    (
        "runtime_profiles.*.delegation_timeout_secs",
        "delegate_config_removed",
    ),
    (
        "runtime_profiles.*.agentic_timeout_secs",
        "delegate_config_removed",
    ),
];

/// Structured tombstone warnings for retired fields still present in a
/// config file. Companion to [`retired_section_tombstones`]: same load-path
/// wiring, same warning shape, same sunset. A hit means the key was dropped
/// by serde as unknown while every other key in its section kept working —
/// the warning says so explicitly.
pub fn retired_field_tombstones(contents: &str) -> Vec<ValidationWarning> {
    let Ok(root) = toml::from_str::<toml::Value>(contents) else {
        return Vec::new();
    };
    let Some(root) = root.as_table() else {
        return Vec::new();
    };
    let mut warnings = Vec::new();
    for (path, code) in RETIRED_CONFIG_FIELDS {
        if !field_path_exists(root, path) {
            continue;
        }
        let warning = ValidationWarning::new(
            *code,
            format!(
                "[{path}] in config.toml is ignored: the legacy delegation key was removed \
                 from the schema with the retired delegate tool and has no runtime \
                 consumer. Remove the key.",
            ),
            (*path).to_string(),
        );
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({ "path": path, "code": code })),
            &warning.message
        );
        warnings.push(warning);
    }
    warnings
}

/// True when a dotted path with at most one `*` wildcard segment (matching a
/// map key) resolves to any value in the parsed root.
fn field_path_exists(root: &toml::value::Table, path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    match segments.iter().position(|segment| *segment == "*") {
        None => value_at_exists(root, &segments),
        Some(star) => {
            let Some(parent) = value_at_table(root, &segments[..star]) else {
                return false;
            };
            let suffix = &segments[star + 1..];
            parent.values().any(|value| match value.as_table() {
                Some(table) => value_at_exists(table, suffix),
                None => suffix.is_empty(),
            })
        }
    }
}

/// Resolve a wildcard-free segment list to a table, if present.
fn value_at_table<'a>(
    table: &'a toml::value::Table,
    segments: &[&str],
) -> Option<&'a toml::value::Table> {
    let mut current = table;
    for segment in segments {
        current = current.get(*segment)?.as_table()?;
    }
    Some(current)
}

/// True when a wildcard-free segment list resolves to a value. Only the
/// intermediate segments must be tables; the terminal segment may be any
/// value kind (the retired delegate keys are bools, arrays, and tables).
fn value_at_exists(mut current: &toml::value::Table, segments: &[&str]) -> bool {
    let Some((&last, intermediates)) = segments.split_last() else {
        return false;
    };
    for segment in intermediates {
        let Some(value) = current.get(*segment) else {
            return false;
        };
        match value.as_table() {
            Some(table) => current = table,
            // A non-table mid-path value cannot contain the terminal.
            None => return false,
        }
    }
    current.contains_key(last)
}

/// Structured tombstone warnings for retired sections still present in a
/// config file. `GatewayConfig`-style structs do not use
/// `deny_unknown_fields`, so serde silently drops an unknown nested
/// section and configs carrying a retired section keep parsing; this makes
/// the retirement visible instead of a silent no-op. Called by the
/// config-file LOAD paths (`Config::load_or_init`, the channels standalone
/// loader, and the gateway migrate handler); incidental single-key file
/// readers (e.g. the web-search Brave-key reload) never participated in
/// any config warning machinery and are out of this boundary.
///
/// Each hit also logs a WARN at detection time, symmetric with the env
/// tombstone's apply-time log: loaders that never call
/// `validate()`/`collect_warnings()` (the channels per-message reload)
/// still surface the retirement, and the warning is not gated behind
/// `validate()`'s replay loop (which an earlier validation error skips).
/// Ordering caveat, stated honestly: in `Config::load_or_init`, detection
/// runs before the `composition` hard-error gate, so a config with both
/// problems still warns before the load fails; on the channels loader,
/// strict migration runs first, so a file that fails strict migration
/// (e.g. invalid `composition`) errors before detection. A failed load
/// has its own error to fix; after the fix, the tombstone surfaces.
/// Successful loads additionally replay the structured warning through
/// `collect_warnings()` — the same documented dual emission as the env
/// tombstone.
pub fn retired_section_tombstones(contents: &str) -> Vec<ValidationWarning> {
    let Ok(root) = toml::from_str::<toml::Value>(contents) else {
        return Vec::new();
    };
    RETIRED_CONFIG_SURFACES
        .iter()
        .filter(|(path, _)| table_path_exists(root.as_table(), path))
        .map(|(path, code)| {
            let warning = ValidationWarning::new(
                *code,
                format!(
                    "[{path}] in config.toml is ignored: the section was removed from the \
                     schema and has no runtime consumer. This compatibility shim will be \
                     removed in a later announced window. Remove the section."
                ),
                (*path).to_string(),
            );
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"path": path, "code": code})),
                &warning.message
            );
            warning
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
    fn retired_field_tombstones_flags_field_under_wildcard_alias() {
        let contents = r#"
[agents.lead]
model_provider = "openai.default"
delegates = ["peer"]
delegate_same_risk_profile = true

[risk_profiles.shared]
delegation_policy = { mode = "allow" }
"#;
        let warnings = retired_field_tombstones(contents);
        let paths: Vec<_> = warnings.iter().map(|w| w.path.as_str()).collect();
        assert!(paths.contains(&"agents.*.delegates"), "{paths:?}");
        assert!(
            paths.contains(&"agents.*.delegate_same_risk_profile"),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"risk_profiles.*.delegation_policy"),
            "{paths:?}"
        );
        // Untouched fields never warn.
        assert_eq!(warnings.len(), 3, "{paths:?}");
    }

    #[test]
    fn retired_field_tombstones_silent_when_absent() {
        let contents = "[agents.lead]\nmodel_provider = \"openai.default\"\n";
        assert!(retired_field_tombstones(contents).is_empty());
    }

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
    fn retired_section_tombstones_warn_for_every_table_entry() {
        // Discriminator for the wall 2 entries: the shared table is only as
        // good as the detector actually firing for each of its paths. Build
        // a minimal doc carrying every retired section and require exactly
        // one warning with that entry's code and path.
        for (path, code) in RETIRED_CONFIG_SURFACES {
            let segments: Vec<&str> = path.split('.').collect();
            let mut doc = String::new();
            for depth in 1..=segments.len() {
                doc.push_str(&format!("[{}]\n", segments[..depth].join(".")));
            }
            doc.push_str("legacy_key = true\n");
            let warnings = retired_section_tombstones(&doc);
            assert_eq!(warnings.len(), 1, "{path}: {warnings:?}");
            assert_eq!(warnings[0].code, *code);
            assert_eq!(warnings[0].path, *path);
        }
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
