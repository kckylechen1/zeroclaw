//! Thinking/Reasoning Level Control

// Re-exported from zeroclaw-config.
pub use zeroclaw_config::scattered_types::{ThinkingConfig, ThinkingLevel};

/// Parameters derived from a thinking level, applied to the LLM request.
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkingParams {
    /// Temperature adjustment (added to the base temperature, clamped to 0.0..=2.0).
    pub temperature_adjustment: f64,
    /// Maximum tokens adjustment (added to any existing max_tokens setting).
    pub max_tokens_adjustment: i64,
    /// Optional system prompt prefix injected before the existing system prompt.
    pub system_prompt_prefix: Option<String>,
    /// Native extended thinking parameters, populated when the config enables
    /// native thinking and the level has a `budget_tokens` value.
    pub native_thinking: Option<zeroclaw_config::scattered_types::NativeThinkingParams>,
}

pub fn apply_thinking_level(level: ThinkingLevel) -> ThinkingParams {
    match level {
        ThinkingLevel::Off => ThinkingParams {
            temperature_adjustment: -0.2,
            max_tokens_adjustment: -1000,
            system_prompt_prefix: Some(
                "Be extremely concise. Give direct answers without explanation \
                 unless explicitly asked. No preamble."
                    .into(),
            ),
            native_thinking: None,
        },
        ThinkingLevel::Minimal => ThinkingParams {
            temperature_adjustment: -0.1,
            max_tokens_adjustment: -500,
            system_prompt_prefix: Some(
                "Be concise and fast. Keep explanations brief. \
                 Prioritize speed over thoroughness."
                    .into(),
            ),
            native_thinking: None,
        },
        ThinkingLevel::Low => ThinkingParams {
            temperature_adjustment: -0.05,
            max_tokens_adjustment: 0,
            system_prompt_prefix: Some("Keep reasoning light. Explain only when helpful.".into()),
            native_thinking: None,
        },
        ThinkingLevel::Medium => ThinkingParams {
            temperature_adjustment: 0.0,
            max_tokens_adjustment: 0,
            system_prompt_prefix: None,
            native_thinking: None,
        },
        ThinkingLevel::High => ThinkingParams {
            temperature_adjustment: 0.05,
            max_tokens_adjustment: 1000,
            system_prompt_prefix: Some(
                "Think step by step. Provide thorough analysis and \
                 consider edge cases before answering."
                    .into(),
            ),
            native_thinking: None,
        },
        ThinkingLevel::Max => ThinkingParams {
            temperature_adjustment: 0.1,
            max_tokens_adjustment: 2000,
            system_prompt_prefix: Some(
                "Think very carefully and exhaustively. Break down the problem \
                 into sub-problems, consider all angles, verify your reasoning, \
                 and provide the most thorough analysis possible."
                    .into(),
            ),
            native_thinking: None,
        },
    }
}

/// Convert a `ThinkingLevel` into parameters, resolving native extended
/// thinking from the provided config.
pub fn apply_thinking_level_with_config(
    level: ThinkingLevel,
    config: &ThinkingConfig,
) -> ThinkingParams {
    use zeroclaw_config::scattered_types::{MAX_BUDGET_TOKENS, MIN_BUDGET_TOKENS};
    let mut params = apply_thinking_level(level);
    if config.native_thinking
        && let Some(budget) = config.budget_tokens_for(level)
    {
        let clamped = budget.clamp(MIN_BUDGET_TOKENS, MAX_BUDGET_TOKENS);
        if clamped != budget {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_attrs(::serde_json::json!({
                        "requested": budget,
                        "clamped": clamped,
                        "min": MIN_BUDGET_TOKENS,
                        "max": MAX_BUDGET_TOKENS
                    })),
                "budget_tokens outside accepted range; clamping"
            );
        }
        params.native_thinking = Some(zeroclaw_config::scattered_types::NativeThinkingParams {
            budget_tokens: clamped,
        });
    }
    params
}

/// Clamp a temperature value to the valid range `[0.0, 2.0]`.
pub fn clamp_temperature(temp: f64) -> f64 {
    temp.clamp(0.0, 2.0)
}

/// Validate thinking config at startup. Call once during agent
/// initialization to warn about unrecognized budget_tokens keys.
pub fn validate_thinking_config(config: &ThinkingConfig) {
    config.warn_unknown_budget_keys();
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Level application ────────────────────────────────────────

    #[test]
    fn apply_thinking_level_off_is_concise() {
        let params = apply_thinking_level(ThinkingLevel::Off);
        assert!(params.temperature_adjustment < 0.0);
        assert!(params.max_tokens_adjustment < 0);
        assert!(params.system_prompt_prefix.is_some());
        assert!(
            params
                .system_prompt_prefix
                .unwrap()
                .to_lowercase()
                .contains("concise")
        );
    }

    #[test]
    fn apply_thinking_level_medium_is_neutral() {
        let params = apply_thinking_level(ThinkingLevel::Medium);
        assert!((params.temperature_adjustment - 0.0).abs() < f64::EPSILON);
        assert_eq!(params.max_tokens_adjustment, 0);
        assert!(params.system_prompt_prefix.is_none());
    }

    #[test]
    fn apply_thinking_level_high_adds_step_by_step() {
        let params = apply_thinking_level(ThinkingLevel::High);
        assert!(params.temperature_adjustment > 0.0);
        assert!(params.max_tokens_adjustment > 0);
        let prefix = params.system_prompt_prefix.unwrap();
        assert!(prefix.to_lowercase().contains("step by step"));
    }

    #[test]
    fn apply_thinking_level_max_is_most_thorough() {
        let params = apply_thinking_level(ThinkingLevel::Max);
        assert!(params.temperature_adjustment > 0.0);
        assert!(params.max_tokens_adjustment > 0);
        let prefix = params.system_prompt_prefix.unwrap();
        assert!(prefix.to_lowercase().contains("exhaustively"));
    }

    // ── Temperature clamping ─────────────────────────────────────

    #[test]
    fn clamp_temperature_within_range() {
        assert!((clamp_temperature(0.7) - 0.7).abs() < f64::EPSILON);
        assert!((clamp_temperature(0.0) - 0.0).abs() < f64::EPSILON);
        assert!((clamp_temperature(2.0) - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_temperature_below_minimum() {
        assert!((clamp_temperature(-0.5) - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn clamp_temperature_above_maximum() {
        assert!((clamp_temperature(3.0) - 2.0).abs() < f64::EPSILON);
    }

    // ── Budget-token clamping ────────────────────────────────────

    #[test]
    fn budget_tokens_clamped_to_min_when_below() {
        use std::collections::HashMap;
        use zeroclaw_config::scattered_types::MIN_BUDGET_TOKENS;
        let mut overrides = HashMap::new();
        overrides.insert("high".to_string(), 100);
        let config = ThinkingConfig {
            default_level: ThinkingLevel::High,
            native_thinking: true,
            budget_tokens: overrides,
        };
        let params = apply_thinking_level_with_config(ThinkingLevel::High, &config);
        let native = params
            .native_thinking
            .expect("native thinking should be set");
        assert_eq!(native.budget_tokens, MIN_BUDGET_TOKENS);
    }

    #[test]
    fn budget_tokens_preserved_within_range() {
        use std::collections::HashMap;
        let mut overrides = HashMap::new();
        overrides.insert("high".to_string(), 8_000);
        let config = ThinkingConfig {
            default_level: ThinkingLevel::High,
            native_thinking: true,
            budget_tokens: overrides,
        };
        let params = apply_thinking_level_with_config(ThinkingLevel::High, &config);
        let native = params
            .native_thinking
            .expect("native thinking should be set");
        assert_eq!(native.budget_tokens, 8_000);
    }

    #[test]
    fn budget_tokens_clamped_to_max_when_above() {
        use std::collections::HashMap;
        use zeroclaw_config::scattered_types::MAX_BUDGET_TOKENS;
        let mut overrides = HashMap::new();
        overrides.insert("high".to_string(), MAX_BUDGET_TOKENS + 1_000);
        let config = ThinkingConfig {
            default_level: ThinkingLevel::High,
            native_thinking: true,
            budget_tokens: overrides,
        };
        let params = apply_thinking_level_with_config(ThinkingLevel::High, &config);
        let native = params
            .native_thinking
            .expect("native thinking should be set");
        assert_eq!(native.budget_tokens, MAX_BUDGET_TOKENS);
    }

    // ── Serde round-trip ─────────────────────────────────────────

    #[test]
    fn thinking_config_deserializes_from_toml() {
        let toml_str = r#"default_level = "high""#;
        let config: ThinkingConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_level, ThinkingLevel::High);
    }

    #[test]
    fn thinking_config_default_level_deserializes() {
        let toml_str = "";
        let config: ThinkingConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.default_level, ThinkingLevel::Medium);
    }

    #[test]
    fn thinking_level_serializes_lowercase() {
        let level = ThinkingLevel::High;
        let json = serde_json::to_string(&level).unwrap();
        assert_eq!(json, "\"high\"");
    }

    #[tokio::test]
    async fn native_thinking_override_round_trips_through_scope() {
        use zeroclaw_config::scattered_types::NativeThinkingParams;
        let installed = Some(NativeThinkingParams {
            budget_tokens: 32_000,
        });
        let read_back = zeroclaw_api::NATIVE_THINKING_OVERRIDE
            .scope(installed, async {
                zeroclaw_api::NATIVE_THINKING_OVERRIDE
                    .try_with(Clone::clone)
                    .ok()
                    .flatten()
            })
            .await;
        assert_eq!(
            read_back, installed,
            "NATIVE_THINKING_OVERRIDE.scope must round-trip params to the inner read-back"
        );
    }

    #[tokio::test]
    async fn native_thinking_override_returns_none_outside_scope() {
        let read_back = async {
            zeroclaw_api::NATIVE_THINKING_OVERRIDE
                .try_with(Clone::clone)
                .ok()
                .flatten()
        }
        .await;
        assert!(
            read_back.is_none(),
            "NATIVE_THINKING_OVERRIDE outside a scope must read None, got: {read_back:?}"
        );
    }

    #[test]
    fn validate_thinking_config_accepts_arbitrary_inputs_without_panicking() {
        let mut cfg_with_unknown_key = ThinkingConfig::default();
        cfg_with_unknown_key
            .budget_tokens
            .insert("turbo".to_string(), 5_000); // not a valid ThinkingLevel
        validate_thinking_config(&cfg_with_unknown_key);

        let cfg_default = ThinkingConfig::default();
        validate_thinking_config(&cfg_default);

        let mut cfg_all_valid = ThinkingConfig::default();
        for level in ["off", "minimal", "low", "medium", "high", "max"] {
            cfg_all_valid
                .budget_tokens
                .insert(level.to_string(), 10_000);
        }
        validate_thinking_config(&cfg_all_valid);
    }
}
