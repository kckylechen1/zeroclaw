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
///   this warning (see `RETIRED_ENV_TOMBSTONES` in `env_overrides.rs` and
///   the retired-section tombstone in `Config::load_or_init`). The
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
