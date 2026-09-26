//! TG2: Config Load/Save Round-Trip Tests

use zeroclaw::config::Config;

// ─────────────────────────────────────────────────────────────────────────────
// Config default construction
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_default_validates_without_provider_profiles() {
    let config = Config::default();
    config
        .validate()
        .expect("default config should validate without provider profiles");
}

// ─────────────────────────────────────────────────────────────────────────────
// Config TOML serialization round-trip
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_toml_roundtrip_preserves_provider() {
    use zeroclaw::config::{DeepseekModelProviderConfig, ModelProviderConfig};
    let mut config = Config::default();
    config.providers.models.deepseek.insert(
        "default".to_string(),
        DeepseekModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("deepseek-chat".into()),
                temperature: Some(0.5),
                ..Default::default()
            },
        },
    );

    let toml_str = toml::to_string(&config).expect("config should serialize to TOML");
    let parsed = zeroclaw::config::migration::migrate_to_current(&toml_str)
        .expect("TOML should round-trip through migration");

    assert!(
        parsed
            .providers
            .models
            .find("deepseek", "default")
            .is_some(),
        "deepseek.default entry should survive round-trip"
    );
    assert_eq!(
        parsed
            .providers
            .models
            .find("deepseek", "default")
            .and_then(|e| e.model.as_deref()),
        Some("deepseek-chat")
    );
    assert!(
        (parsed
            .providers
            .models
            .find("deepseek", "default")
            .and_then(|e| e.temperature)
            .unwrap_or(0.7)
            - 0.5)
            .abs()
            < f64::EPSILON
    );
}

#[test]
fn config_toml_roundtrip_preserves_agent_config() {
    let mut config = Config::default();
    let agent = config.agents.entry("default".into()).or_default();
    agent.risk_profile = "tight".into();
    agent.runtime_profile = "fast".into();
    agent.enabled = false;

    let toml_str = toml::to_string(&config).expect("config should serialize to TOML");
    let parsed: Config = toml::from_str(&toml_str).expect("TOML should deserialize back");

    let agent = parsed
        .agents
        .get("default")
        .expect("default agent survived round-trip");
    assert_eq!(agent.risk_profile, "tight");
    assert_eq!(agent.runtime_profile, "fast");
    assert!(!agent.enabled);
}

// ─────────────────────────────────────────────────────────────────────────────
// Config file parsing
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_file_with_missing_optional_fields_uses_defaults() {
    // Simulate a minimal config TOML that omits optional sections
    let minimal_toml = r#"
default_temperature = 0.7
"#;
    let parsed: Config = toml::from_str(minimal_toml).expect("minimal TOML should parse");

    // V3 has no static-default agent shim. With no `[agents.<alias>]`
    // defined the lookup misses; the test asserts the absence rather
    // than the previous shim's defaults.
    assert!(
        parsed.agents.is_empty(),
        "minimal TOML should not synthesize any agent"
    );
}

#[test]
fn config_file_with_custom_agent_section() {
    // V3 lifts the old global `[agent]` settings into `[agents.<alias>]`.
    let toml_with_agent = r#"
default_temperature = 0.7

[agents.default]
risk_profile = "tight"
enabled = true
"#;
    let parsed: Config =
        toml::from_str(toml_with_agent).expect("TOML with [agents.default] should parse");

    let agent = parsed.agents.get("default").expect("default agent parsed");
    assert_eq!(agent.risk_profile, "tight");
    assert!(agent.enabled);
    // runtime_profile is omitted, so it stays the empty default.
    assert_eq!(agent.runtime_profile, "");
}
