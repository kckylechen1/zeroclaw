//! Config Schema Boundary Tests
//! Validates: config defaults, backward compatibility, invalid input rejection,
//! and gateway/security/agent config boundary conditions.

use zeroclaw::config::migration;
use zeroclaw::config::{Config, GatewayConfig, RiskProfileConfig, SecurityConfig};

fn migrate(toml_str: &str) -> Config {
    migration::migrate_to_current(toml_str).expect("migration succeeds")
}

// ─────────────────────────────────────────────────────────────────────────────
// Invalid value fail-fast
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_valid_keys_not_flagged_as_unknown() {
    // api_key: Option<T> defaulting to None — TOML omits it.
    // model_provider: serde alias for default_model_provider.
    let unknown = Config::unknown_keys("api_key = \"sk-test\"\nmodel_provider = \"ollama\"\n");
    assert!(
        unknown.is_empty(),
        "api_key and model_provider should not be flagged as unknown, got: {unknown:?}",
    );
}

#[test]
fn config_unknown_keys_parse_without_error() {
    let config = migrate(
        r#"
default_temperature = 0.7
default_model_provider = "openai"
totally_unknown_key = "should be ignored"
another_fake = 42
"#,
    );
    assert!(
        (config
            .providers
            .models
            .find("openai", "default")
            .and_then(|e| e.temperature)
            .unwrap_or(0.7)
            - 0.7)
            .abs()
            < f64::EPSILON
    );
}

#[test]
fn config_invalid_gateway_port_fails() {
    for (value, why) in [
        ("\"not_a_number\"", "string for u16 port"),
        ("-1", "negative port"),
        ("99999", "port > 65535"),
    ] {
        let toml_str = format!("[gateway]\nport = {value}\n");
        let result: Result<Config, _> = toml::from_str(&toml_str);
        assert!(result.is_err(), "{why} should fail to parse");
    }
}

#[test]
fn config_wrong_type_for_temperature_fails() {
    let toml_str = r#"
default_temperature = "hot"
"#;
    // V1's `default_temperature` is folded into model_providers.<x>.default
    // by `migrate_to_current`. A non-f64 value should fail at the migration
    // boundary because the synthesized model_provider entry can't deserialize
    // a string into Option<f64>.
    let result = migration::migrate_to_current(toml_str);
    assert!(
        result.is_err(),
        "string for f64 temperature should fail migration"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// GatewayConfig boundary tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn gateway_config_defaults_are_secure() {
    let gw = GatewayConfig::default();
    assert_eq!(gw.port, 42617);
    assert_eq!(gw.host, "127.0.0.1");
    assert!(gw.require_pairing, "pairing should be required by default");
    assert!(
        !gw.allow_public_bind,
        "public bind should be denied by default"
    );
    assert!(
        !gw.trust_forwarded_headers,
        "forwarded headers should be untrusted by default"
    );
    assert!(
        gw.path_prefix.is_none(),
        "path_prefix should default to None"
    );
}

#[test]
fn gateway_config_missing_section_uses_defaults() {
    let toml_str = r#"
default_temperature = 0.5
"#;
    let parsed: Config = toml::from_str(toml_str).expect("missing gateway section should parse");
    assert_eq!(parsed.gateway.port, 42617);
    assert_eq!(parsed.gateway.host, "127.0.0.1");
    assert!(parsed.gateway.require_pairing);
    assert!(!parsed.gateway.allow_public_bind);
}

#[test]
fn gateway_config_partial_section_fills_defaults() {
    let toml_str = r#"
default_temperature = 0.7

[gateway]
port = 9090
"#;
    let parsed: Config = toml::from_str(toml_str).expect("partial gateway should parse");
    assert_eq!(parsed.gateway.port, 9090);
    assert_eq!(parsed.gateway.host, "127.0.0.1");
    assert!(parsed.gateway.require_pairing);
    assert_eq!(parsed.gateway.pair_rate_limit_per_minute, 10);
}

// ─────────────────────────────────────────────────────────────────────────────
// GatewayConfig path_prefix validation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn gateway_path_prefix_rejects_bad_slashes() {
    for (prefix, expected) in [
        ("zeroclaw", "must start with '/'"),
        ("/zeroclaw/", "must not end with '/'"),
        ("/", "must not end with '/'"),
    ] {
        let mut config = Config::default();
        config.gateway.path_prefix = Some(prefix.into());
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains(expected),
            "prefix {prefix:?}: expected {expected:?}, got: {err}"
        );
    }
}

#[test]
fn gateway_path_prefix_accepts_valid_prefixes() {
    for prefix in ["/zeroclaw", "/apps/zeroclaw", "/api/hassio_ingress/abc123"] {
        let mut config = Config::default();
        config.gateway.path_prefix = Some(prefix.into());
        config
            .validate()
            .unwrap_or_else(|e| panic!("prefix {prefix:?} should be valid, got: {e}"));
    }
}

#[test]
fn gateway_path_prefix_rejects_unsafe_characters() {
    for prefix in [
        "/zero claw",
        "/zero<claw",
        "/zero>claw",
        "/zero\"claw",
        "/zero?query",
        "/zero#frag",
    ] {
        let mut config = Config::default();
        config.gateway.path_prefix = Some(prefix.into());
        let err = config.validate().unwrap_err();
        assert!(
            err.to_string().contains("invalid character"),
            "prefix {prefix:?} should be rejected, got: {err}"
        );
    }
    // Leading/trailing whitespace is rejected by the starts_with('/') or
    // invalid-character check — either way it must not pass validation.
    for prefix in [" /zeroclaw ", " /zeroclaw"] {
        let mut config = Config::default();
        config.gateway.path_prefix = Some(prefix.into());
        assert!(
            config.validate().is_err(),
            "whitespace-padded prefix {prefix:?} should be rejected"
        );
    }
}

#[test]
fn gateway_path_prefix_accepts_none() {
    let config = Config::default();
    assert!(config.gateway.path_prefix.is_none());
    config
        .validate()
        .expect("absent path_prefix should be valid");
}

// ─────────────────────────────────────────────────────────────────────────────
// SecurityConfig boundary tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn security_config_defaults() {
    let sec = SecurityConfig::default();
    assert!(sec.audit.enabled, "audit should be enabled by default");
    // V3: sandbox/resource limits live on risk_profiles entries, not SecurityConfig.
    let profile = RiskProfileConfig::default();
    assert!(
        profile.sandbox_enabled.is_none(),
        "sandbox enabled should auto-detect (None) by default"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// RiskProfileConfig boundary tests (security policy via Config.autonomy)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn runtime_profile_max_actions_per_hour_round_trips_through_toml() {
    let mut config = Config::default();
    config.runtime_profiles.insert(
        "clamps".into(),
        zeroclaw_config::schema::RuntimeProfileConfig {
            max_actions_per_hour: 50,
            ..Default::default()
        },
    );
    let toml_str = toml::to_string(&config).expect("config should serialize");
    let parsed: Config = toml::from_str(&toml_str).expect("should deserialize back");
    let profile = parsed.runtime_profiles.get("clamps").unwrap();
    assert_eq!(profile.max_actions_per_hour, 50);
}

#[test]
fn risk_profile_allowed_path_alias_maps_to_allowed_roots() {
    let toml_str = r#"
[risk_profiles.default]
allowed_path = ["~/work", "~/"]
"#;
    let parsed: Config = toml::from_str(toml_str).expect("allowed_path alias should parse");
    assert_eq!(
        parsed.risk_profiles["default"].allowed_roots,
        vec!["~/work", "~/"]
    );
}

#[test]
fn risk_profile_allowed_paths_alias_maps_to_allowed_roots() {
    let toml_str = r#"
[risk_profiles.default]
allowed_paths = ["/tmp/data"]
"#;
    let parsed: Config = toml::from_str(toml_str).expect("allowed_paths alias should parse");
    assert_eq!(
        parsed.risk_profiles["default"].allowed_roots,
        vec!["/tmp/data"]
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Backward compatibility
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_minimal_toml_with_temperature_uses_defaults() {
    let config = migrate("default_temperature = 0.7\ndefault_provider = \"openai\"\n");
    // Migration synthesizes [agents.default] from the V2 shape.
    config
        .agent("default")
        .expect("migration synthesized agents.default");
    assert_eq!(config.gateway.port, 42617);
}

#[test]
fn config_only_temperature_parses() {
    let config = migrate("default_temperature = 1.2\ndefault_provider = \"openai\"\n");
    assert!(
        (config
            .providers
            .models
            .find("openai", "default")
            .and_then(|e| e.temperature)
            .unwrap_or(0.7)
            - 1.2)
            .abs()
            < f64::EPSILON
    );
    let agent = config
        .agent("default")
        .expect("migration synthesized agents.default");
    assert!(agent.enabled);
}

#[test]
fn config_extra_unknown_keys_ignored() {
    let config = migrate(
        r#"
default_temperature = 0.5
default_provider = "openai"
future_feature = true
[some_future_section]
value = 123
"#,
    );
    assert!(
        (config
            .providers
            .models
            .find("openai", "default")
            .and_then(|e| e.temperature)
            .unwrap_or(0.7)
            - 0.5)
            .abs()
            < f64::EPSILON
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Config merging edge cases
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_multiple_channels_coexist() {
    let toml_str = r#"
default_temperature = 0.7

[channels.telegram.default]
bot_token = "test_token"
allowed_users = ["zeroclaw_user"]

[channels.discord.default]
bot_token = "test_token"
"#;
    let parsed: Config = toml::from_str(toml_str).expect("multi-channel config should parse");
    assert!(!parsed.channels.telegram.is_empty());
    assert!(!parsed.channels.discord.is_empty());
    assert!(parsed.channels.slack.is_empty());
}

#[test]
fn config_memory_defaults_when_section_absent() {
    let toml_str = "default_temperature = 0.7\n";
    let parsed: Config = toml::from_str(toml_str).expect("minimal TOML should parse");
    let mem = &parsed.memory;
    assert!(!mem.backend.is_empty());
    assert!(!mem.embedding_provider.is_empty());
    let weight_sum = mem.vector_weight + mem.keyword_weight;
    assert!(
        (weight_sum - 1.0).abs() < 0.01,
        "vector + keyword weights should sum to ~1.0"
    );
}

#[test]
fn config_channels_without_cli_field() {
    let toml_str = r#"
default_temperature = 0.7

[channels.matrix.default]
homeserver = "https://matrix.example.com"
access_token = "syt_test_token"
allowed_rooms = ["!abc123:example.com"]
allowed_users = ["@user:example.com"]
"#;
    let parsed: Config = toml::from_str(toml_str)
        .expect("channels with only a Matrix section (no explicit cli field) should parse");
    assert!(
        parsed.channels.cli,
        "cli should default to true when omitted"
    );
    assert!(!parsed.channels.matrix.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// – top-level [cli] section must not clash with channels.cli
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn config_toplevel_cli_section_with_whatsapp_parses() {
    // Exact config from
    let toml_str = r#"
[cli]

[channels.whatsapp.default]
session_path = "~/.zeroclaw/state/whatsapp-web/session.db"
allowed_numbers = ["*"]
"#;
    let parsed: Config = toml::from_str(toml_str)
        .expect("top-level [cli] section with [channels.whatsapp] should parse");
    assert!(!parsed.channels.whatsapp.is_empty());
    let wa = parsed.channels.whatsapp.get("default").unwrap();
    assert_eq!(
        wa.session_path.as_deref(),
        Some("~/.zeroclaw/state/whatsapp-web/session.db")
    );
}

#[test]
fn config_channels_explicit_cli_true_with_whatsapp() {
    let toml_str = r#"
[channels]
cli = true

[channels.whatsapp.default]
session_path = "~/.zeroclaw/state/whatsapp-web/session.db"
allowed_numbers = ["*"]
"#;
    let parsed: Config =
        toml::from_str(toml_str).expect("explicit channels.cli=true with whatsapp should parse");
    assert!(parsed.channels.cli);
    assert!(!parsed.channels.whatsapp.is_empty());
}

#[test]
fn config_empty_parses_with_all_defaults() {
    let config = migrate("");
    assert!(config.channels.cli);
    assert!(config.channels.whatsapp.is_empty());
    assert!(
        (config
            .providers
            .models
            .iter_entries()
            .next()
            .map(|(_, _, e)| e)
            .and_then(|e| e.temperature)
            .unwrap_or(0.7)
            - 0.7)
            .abs()
            < f64::EPSILON
    );
}
