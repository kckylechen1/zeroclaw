use super::*;
use tempfile::TempDir;

#[test]
fn collapse_model_probes_groups_identical_and_breaks_divergent() {
    use ModelProbe::{Err as E, Ok as K};
    let probes = vec![
        ("ollama.a".to_string(), K(8)),
        ("ollama.b".to_string(), K(8)),
        ("ollama.c".to_string(), K(8)),
        ("opencode.x".to_string(), K(18)),
        ("opencode.y".to_string(), K(18)),
        ("kilo.solo".to_string(), K(335)),
        ("openai.a".to_string(), K(10)),
        ("openai.b".to_string(), K(12)),
        (
            "kilocli.free".to_string(),
            E(Severity::Error, "not supported".to_string()),
        ),
    ];
    let msgs: Vec<String> = collapse_model_probes(probes)
        .into_iter()
        .map(|r| r.message)
        .collect();
    assert_eq!(
        msgs,
        vec![
            "ollama: 8 models",      // 3 identical aliases → collapsed to type
            "opencode: 18 models",   // 2 identical → collapsed
            "kilo.solo: 335 models", // single alias → kept per-alias
            "openai.a: 10 models",   // divergent counts → broken out
            "openai.b: 12 models",
            "kilocli.free: not supported", // single alias → kept
        ]
    );
}

#[test]
fn model_in_catalog_requires_exact_id_match() {
    let catalog = vec![
        "anthropic/claude-sonnet-4.5".to_string(),
        "openai/gpt-5".to_string(),
    ];
    assert!(model_in_catalog("openai/gpt-5", &catalog));
    // Not present, partial, and empty all fail — no fuzzy/suffix matching.
    assert!(!model_in_catalog("openai/gpt-4", &catalog));
    assert!(!model_in_catalog("gpt-5", &catalog));
    assert!(!model_in_catalog("", &catalog));
    assert!(!model_in_catalog("anthropic/claude-sonnet-4.5", &[]));
}

#[test]
fn provider_validation_checks_custom_url_shape() {
    let config = Config::default();
    assert!(provider_validation_error(&config, "openrouter").is_none());
    assert!(provider_validation_error(&config, "custom:https://example.com").is_none());
    assert!(provider_validation_error(&config, "anthropic-custom:https://example.com").is_none());

    let invalid_custom = provider_validation_error(&config, "custom:").unwrap_or_default();
    assert!(invalid_custom.contains("requires a URL"));

    let invalid_unknown = provider_validation_error(&config, "totally-fake").unwrap_or_default();
    assert!(invalid_unknown.contains("Unknown model_provider"));
}

#[test]
fn provider_validation_accepts_custom_with_uri_in_config() {
    // Regression: the Doctor previously called create_model_provider(name, None)
    // without config, causing custom providers with uri defined in config to
    // fail validation with "Custom model_provider requires `uri`".
    let mut config = Config::default();
    let profile = config
        .providers
        .models
        .ensure("custom", "vllm")
        .expect("known model_provider type");
    profile.uri = Some("http://10.0.0.15:8000/v1".to_string());
    profile.model = Some("Qwen3.6-27B".to_string());

    // Full label (type.alias) should validate successfully when uri is in config.
    assert!(
        provider_validation_error(&config, "custom.vllm").is_none(),
        "custom.vllm should be valid when uri is defined in config"
    );

    // Bare "custom" without alias should still fail (no config entry to resolve).
    let bare_error = provider_validation_error(&config, "custom").unwrap_or_default();
    assert!(
        bare_error.contains("requires `uri`"),
        "bare 'custom' without alias should require uri"
    );
}

#[test]
fn diag_item_icons() {
    assert_eq!(DiagItem::ok("t", "m").icon(), "✅");
    assert_eq!(DiagItem::warn("t", "m").icon(), "⚠️ ");
    assert_eq!(DiagItem::error("t", "m").icon(), "❌");
}

#[test]
fn config_validation_catches_bad_temperature() {
    // Single model_provider entry with an out-of-range temperature so the
    // doctor's `iter_entries()` walk deterministically finds it
    // (HashMap iteration order is unspecified — multiple entries
    // produce a coin-flip iteration order).
    let mut config = Config::default();
    config
        .providers
        .models
        .ensure("openrouter", "default")
        .expect("known model_provider type")
        .temperature = Some(5.0);
    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let temp_item = items.iter().find(|i| i.message.contains("temperature"));
    assert!(temp_item.is_some());
    assert_eq!(temp_item.unwrap().severity, Severity::Error);
}

#[test]
fn config_validation_accepts_valid_temperature() {
    let mut config = Config::default();
    config
        .providers
        .models
        .ensure("openrouter", "default")
        .expect("known model_provider type")
        .temperature = Some(0.7);
    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let temp_item = items.iter().find(|i| i.message.contains("temperature"));
    assert!(temp_item.is_some());
    assert_eq!(temp_item.unwrap().severity, Severity::Ok);
}

#[test]
fn context_window_diagnostics_distinguish_unset_explicit_and_zero_profiles() {
    let mut unset = Config::default();
    let profile = unset
        .providers
        .models
        .ensure("ollama", "local")
        .expect("known model provider type");
    profile.model = Some("qwen3".to_string());

    let mut unset_items = Vec::new();
    check_config_semantics(&unset, &mut unset_items);
    let unset_message = crate::i18n::get_required_cli_string_with_args(
        "cli-doctor-context-window-unset",
        &[
            ("provider_ref", "ollama.local"),
            (
                "fallback",
                &UNCONFIGURED_CONTEXT_WINDOW_FALLBACK.to_string(),
            ),
        ],
    );
    let unset_item = unset_items
        .iter()
        .find(|item| item.message == unset_message)
        .expect("unset profile must produce the localized warning");
    assert_eq!(unset_item.severity, Severity::Warn);

    let mut explicit = unset.clone();
    explicit
        .providers
        .models
        .ensure("ollama", "local")
        .expect("known model provider type")
        .context_window = Some(32_000);
    let mut explicit_items = Vec::new();
    check_config_semantics(&explicit, &mut explicit_items);
    let explicit_message = crate::i18n::get_required_cli_string_with_args(
        "cli-doctor-context-window-ok",
        &[
            ("provider_ref", "ollama.local"),
            ("context_window", "32000"),
        ],
    );
    let explicit_item = explicit_items
        .iter()
        .find(|item| item.message == explicit_message)
        .expect("explicit profile must produce the localized OK result");
    assert_eq!(explicit_item.severity, Severity::Ok);

    let unset_warnings = unset_items
        .iter()
        .filter(|item| item.severity == Severity::Warn)
        .count();
    let explicit_warnings = explicit_items
        .iter()
        .filter(|item| item.severity == Severity::Warn)
        .count();
    assert_eq!(
        unset_warnings,
        explicit_warnings + 1,
        "an unset context window must add exactly one doctor warning"
    );

    let mut zero = explicit;
    zero.providers
        .models
        .ensure("ollama", "local")
        .expect("known model provider type")
        .context_window = Some(0);
    let mut zero_items = Vec::new();
    check_config_semantics(&zero, &mut zero_items);
    let zero_message = crate::i18n::get_required_cli_string_with_args(
        "cli-doctor-context-window-zero",
        &[("provider_ref", "ollama.local")],
    );
    let zero_item = zero_items
        .iter()
        .find(|item| item.message == zero_message)
        .expect("zero context window must produce the localized error");
    assert_eq!(zero_item.severity, Severity::Error);
}

#[test]
fn config_validation_warns_no_channels() {
    let config = Config::default();
    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let ch_item = items.iter().find(|i| i.message.contains("channel"));
    assert!(ch_item.is_some());
    assert_eq!(ch_item.unwrap().severity, Severity::Warn);
}

#[test]
fn degraded_sections_reported_as_warning() {
    let config = Config {
        degraded_sections: vec!["channels.telegram.default".to_string()],
        ..Default::default()
    };
    let mut items = Vec::new();
    check_degraded_sections(&config, &mut items);
    let item = items
        .iter()
        .find(|i| i.message.contains("channels.telegram.default"));
    assert!(
        item.is_some(),
        "expected a diagnostic naming the degraded section, got messages: {:?}",
        items.iter().map(|i| &i.message).collect::<Vec<_>>()
    );
    assert_eq!(item.unwrap().severity, Severity::Warn);
}

#[test]
fn degraded_security_reported_as_error() {
    let config = Config {
        degraded_security: vec!["security".to_string()],
        ..Default::default()
    };
    let mut items = Vec::new();
    check_degraded_sections(&config, &mut items);
    let item = items.iter().find(|i| {
        i.category == "config" && i.severity == Severity::Error && i.message.contains("security")
    });
    assert!(
        item.is_some(),
        "expected an error diagnostic naming the degraded security section, got messages: {:?}",
        items.iter().map(|i| &i.message).collect::<Vec<_>>()
    );
}

#[test]
fn clean_config_reports_no_degraded_sections() {
    let config = Config::default();
    let mut items = Vec::new();
    check_degraded_sections(&config, &mut items);
    assert!(
        items.is_empty(),
        "a config with no degraded sections must not produce degraded-section diagnostics, got messages: {:?}",
        items.iter().map(|i| &i.message).collect::<Vec<_>>()
    );
}

#[test]
fn configured_model_provider_api_key_uses_alias_profile() {
    let mut config = Config::default();
    config
        .providers
        .models
        .ensure("custom", "local")
        .expect("known model_provider type")
        .api_key = Some("redacted-test-key".to_string());

    assert_eq!(
        configured_model_provider_api_key(&config, "custom.local"),
        Some("redacted-test-key")
    );
    assert_eq!(configured_model_provider_api_key(&config, "custom"), None);
}

#[test]
fn doctor_model_provider_uses_alias_profile() {
    let mut config = Config::default();
    let profile = config
        .providers
        .models
        .ensure("custom", "local")
        .expect("known model_provider type");
    profile.api_key = Some("redacted-test-key".to_string());
    profile.uri = Some("https://models.example.test/v1".to_string());

    if let Err(error) = create_doctor_model_provider(&config, "custom.local") {
        panic!("doctor model probe should build custom providers from alias config: {error}");
    }
}

#[tokio::test]
async fn structured_run_includes_model_probe_results() {
    let mut config = Config::default();
    let profile = config
        .providers
        .models
        .ensure("custom", "local")
        .expect("known model_provider type");
    profile.api_key = Some("redacted-test-key".to_string());
    profile.uri = Some("http://127.0.0.1:9/v1".to_string());

    let baseline = diagnose(&config);
    assert!(
        !baseline
            .iter()
            .any(|item| item.category == "providers.models")
    );

    let full = run_structured(&config).await;
    assert!(
        full.iter().any(|item| item.category == "providers.models"),
        "shared structured runner should include the same model probe rows as the CLI"
    );
}

#[test]
fn config_validation_catches_unknown_provider() {
    // Typed slots can only hold canonical family names, so an unknown
    // family can no longer reach `iter_entries()`. The
    // remaining reachable path is `agent.model_provider`, which is a
    // free-form `String` an operator can set to any dotted ref.
    let mut config = Config::default();
    config.agents.insert(
        "broken".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            model_provider: "totally-fake.default".into(),
            risk_profile: "default".into(),
            ..Default::default()
        },
    );
    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let prov_item = items.iter().find(|i| {
        i.message
            .contains("agent \"broken\" uses invalid model_provider \"totally-fake.default\"")
    });
    assert!(
        prov_item.is_some(),
        "doctor should flag unknown agent model_provider"
    );
    assert_eq!(prov_item.unwrap().severity, Severity::Warn);
}

#[test]
fn check_bootstrap_truncation_reports_over_cap_files_only() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        config_path: tmp.path().join("config.toml"),
        data_dir: tmp.path().to_path_buf(),
        ..Config::default()
    };
    config.agents.insert(
        "alpha".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            ..Default::default()
        },
    );
    let ws = config.agent_workspace_dir("alpha");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("AGENTS.md"), "a".repeat(7000)).unwrap();
    std::fs::write(ws.join("SOUL.md"), "s".repeat(100)).unwrap();

    let mut items = Vec::new();
    check_bootstrap_truncation(&config, &mut items);

    assert_eq!(items.len(), 1, "only the over-cap file is reported");
    assert_eq!(items[0].severity, Severity::Warn);
    assert_eq!(items[0].category, "agent.prompt");
    assert_eq!(
        items[0].message,
        "alpha/AGENTS.md: compact-context cap 6000 vs 7000 chars (1000 would be discarded)"
    );
}

#[test]
fn check_bootstrap_truncation_matches_runtime_trim_and_injection_honesty() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        config_path: tmp.path().join("config.toml"),
        data_dir: tmp.path().to_path_buf(),
        ..Config::default()
    };
    config.agents.insert(
        "beta".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            ..Default::default()
        },
    );
    let ws = config.agent_workspace_dir("beta");
    std::fs::create_dir_all(&ws).unwrap();
    // Over-cap only in surrounding whitespace: the runtime trims
    // first, so this is not a finding.
    std::fs::write(
        ws.join("SOUL.md"),
        format!("{}{}", " ".repeat(7000), "x".repeat(10)),
    )
    .unwrap();
    // MEMORY.md over cap: doctor cannot know a session's injection
    // mode, so it must describe the cap without claiming injection.
    std::fs::write(ws.join("MEMORY.md"), "m".repeat(7000)).unwrap();

    let mut items = Vec::new();
    check_bootstrap_truncation(&config, &mut items);

    assert_eq!(
        items.len(),
        1,
        "whitespace-only over-cap content is not a finding"
    );
    assert!(
        items[0]
            .message
            .starts_with("beta/MEMORY.md: compact-context cap"),
        "the finding must describe the cap, not claim actual injection: {}",
        items[0].message
    );
    assert!(
        !items[0].message.contains("injected"),
        "doctor runs offline and must not claim injection: {}",
        items[0].message
    );
}

#[test]
fn check_personality_truncation_reports_over_cap_with_compact_off() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut config = Config {
        config_path: tmp.path().join("config.toml"),
        data_dir: tmp.path().to_path_buf(),
        ..Config::default()
    };
    let runtime = zeroclaw_config::schema::RuntimeProfileConfig {
        compact_context: Some(false),
        ..Default::default()
    };
    config
        .runtime_profiles
        .insert("custom".to_string(), runtime);

    config.agents.insert(
        "gamma".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            runtime_profile: "custom".into(),
            ..Default::default()
        },
    );
    let ws = config.agent_workspace_dir("gamma");
    std::fs::create_dir_all(&ws).unwrap();

    // Exact cap: no finding
    std::fs::write(
        ws.join("IDENTITY.md"),
        "x".repeat(crate::agent::personality::MAX_FILE_CHARS),
    )
    .unwrap();

    // Whitespace-only over-cap: trimmed length is exact cap -> no finding
    std::fs::write(
        ws.join("USER.md"),
        format!(
            "  \n{} \t ",
            "u".repeat(crate::agent::personality::MAX_FILE_CHARS)
        ),
    )
    .unwrap();

    // Over-cap personality file: reports finding
    std::fs::write(
        ws.join("SOUL.md"),
        "s".repeat(crate::agent::personality::MAX_FILE_CHARS + 500),
    )
    .unwrap();

    let results = diagnose(&config);

    let prompt_warnings: Vec<_> = results
        .iter()
        .filter(|r| r.category == "agent.prompt" && r.severity == Severity::Warn)
        .collect();

    assert_eq!(
        prompt_warnings.len(),
        1,
        "only over-cap personality file is reported when compact is off"
    );
    assert!(
        prompt_warnings[0].message.contains("gamma/SOUL.md"),
        "warning should cite alias and filename: {}",
        prompt_warnings[0].message
    );
    assert!(
        prompt_warnings[0]
            .message
            .contains("personality cap 20000 vs 20500 chars (500 would be discarded)"),
        "message must match localized string with potential discard: {}",
        prompt_warnings[0].message
    );
    assert!(
        !prompt_warnings[0].message.contains("injected"),
        "offline diagnostic must describe potential discard without claiming actual injection: {}",
        prompt_warnings[0].message
    );
}

#[test]
fn config_validation_warns_empty_model_route() {
    let config = Config {
        model_routes: vec![zeroclaw_config::schema::ModelRouteConfig {
            hint: "fast".into(),
            model_provider: "groq".into(),
            model: String::new(),
            api_key: None,
        }],
        ..Config::default()
    };
    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let route_item = items.iter().find(|i| i.message.contains("empty model"));
    assert!(route_item.is_some());
    assert_eq!(route_item.unwrap().severity, Severity::Warn);
}

#[test]
fn config_validation_warns_empty_embedding_route_model() {
    let config = Config {
        embedding_routes: vec![zeroclaw_config::schema::EmbeddingRouteConfig {
            hint: "semantic".into(),
            model_provider: "openai".into(),
            model: String::new(),
            dimensions: Some(1536),
            api_key: None,
        }],
        ..Config::default()
    };

    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let route_item = items.iter().find(|item| {
        item.message
            .contains("embedding route \"semantic\" has empty model")
    });
    assert!(route_item.is_some());
    assert_eq!(route_item.unwrap().severity, Severity::Warn);
}

#[test]
fn config_validation_warns_invalid_embedding_route_provider() {
    let config = Config {
        embedding_routes: vec![zeroclaw_config::schema::EmbeddingRouteConfig {
            hint: "semantic".into(),
            model_provider: "groq".into(),
            model: "text-embedding-3-small".into(),
            dimensions: None,
            api_key: None,
        }],
        ..Config::default()
    };

    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let route_item = items.iter().find(|item| {
        item.message
            .contains("uses invalid model_provider \"groq\"")
    });
    assert!(route_item.is_some());
    assert_eq!(route_item.unwrap().severity, Severity::Warn);
}

#[test]
fn config_validation_surfaces_dangling_fallback_ref() {
    use zeroclaw_config::schema::{ModelProviderConfig, NvidiaModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.nvidia.insert(
        "nvidia".to_string(),
        NvidiaModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("stepfun-ai/step-3.5-flash".into()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "deepseek-ai/deepseek-v4-flash",
                )],
                ..Default::default()
            },
        },
    );

    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let fallback_item = items.iter().find(|item| {
        item.message
            .contains("does not resolve to a configured providers.models entry")
            && item
                .message
                .contains("providers.models.nvidia.nvidia.fallback[0]")
    });
    assert!(
        fallback_item.is_some(),
        "doctor should surface dangling fallback refs"
    );
    assert_eq!(fallback_item.unwrap().severity, Severity::Warn);
}

#[test]
fn config_validation_warns_enabled_tokenless_bot_channels() {
    // A partial (tokenless) alias survives the resilient load and
    // never reaches degraded_sections, so doctor must flag the unset
    // bot_token itself when the alias is enabled.
    let mut config = Config::default();
    config.channels.telegram.insert(
        "default".to_string(),
        zeroclaw_config::schema::TelegramConfig {
            enabled: true,
            ..Default::default()
        },
    );
    config.channels.discord.insert(
        "default".to_string(),
        zeroclaw_config::schema::DiscordConfig {
            enabled: true,
            ..Default::default()
        },
    );

    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    for path in [
        "channels.telegram.default.bot_token",
        "channels.discord.default.bot_token",
    ] {
        let item = items.iter().find(|i| i.message.contains(path));
        assert!(
            item.is_some(),
            "doctor should flag enabled tokenless alias at {path}, got {:?}",
            items.iter().map(|i| &i.message).collect::<Vec<_>>()
        );
        assert_eq!(item.unwrap().severity, Severity::Warn);
    }
}

#[test]
fn config_validation_ignores_disabled_tokenless_bot_channels() {
    // A staged (disabled) tokenless alias is a normal intermediate state
    // — quickstart and `config set` create exactly this — so doctor must
    // not warn about it.
    let mut config = Config::default();
    config.channels.telegram.insert(
        "default".to_string(),
        zeroclaw_config::schema::TelegramConfig::default(),
    );
    config.channels.discord.insert(
        "default".to_string(),
        zeroclaw_config::schema::DiscordConfig::default(),
    );

    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    assert!(
        !items.iter().any(|i| i.message.contains(".bot_token")),
        "doctor must not flag disabled tokenless aliases, got {:?}",
        items.iter().map(|i| &i.message).collect::<Vec<_>>()
    );
}

#[test]
fn config_validation_warns_missing_embedding_hint_target() {
    let mut config = Config::default();
    config.memory.embedding_model = "hint:semantic".into();

    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);
    let route_item = items.iter().find(|item| {
        item.message
            .contains("no matching [[embedding_routes]] entry exists")
    });
    assert!(route_item.is_some());
    assert_eq!(route_item.unwrap().severity, Severity::Warn);
}

#[test]
fn environment_check_finds_git() {
    let mut items = Vec::new();
    check_environment(&mut items);
    let git_item = items.iter().find(|i| i.message.starts_with("git:"));
    // git should be available in any CI/dev environment
    assert!(git_item.is_some());
    assert_eq!(git_item.unwrap().severity, Severity::Ok);
}

#[test]
fn systemd_linger_diag_reports_disabled_user_service() {
    let item = systemd_linger_diag_item(crate::service::SystemdUserLinger::Disabled {
        user: "alice".to_string(),
    });

    assert_eq!(item.severity, Severity::Warn);
    assert_eq!(item.category, "environment");
    assert!(item.message.contains("may stop after logout"));
    assert!(item.message.contains("loginctl enable-linger alice"));
}

#[test]
fn systemd_linger_diag_reports_enabled_and_unknown() {
    let enabled = systemd_linger_diag_item(crate::service::SystemdUserLinger::Enabled);
    assert_eq!(enabled.severity, Severity::Ok);
    assert_eq!(enabled.message, "systemd user lingering enabled");

    let unknown = systemd_linger_diag_item(crate::service::SystemdUserLinger::Unknown);
    assert_eq!(unknown.severity, Severity::Warn);
    assert!(
        unknown
            .message
            .contains("could not be checked with loginctl")
    );
}

#[test]
fn parse_df_available_mb_uses_last_data_line() {
    let stdout =
        "Filesystem 1M-blocks Used Available Use% Mounted on\n/dev/sda1 1000 500 500 50% /\n";
    assert_eq!(parse_df_available_mb(stdout), Some(500));
}

#[test]
fn truncate_for_display_preserves_utf8_boundaries() {
    let preview = truncate_for_display("🙂example-alpha-build", 3);
    assert_eq!(preview, "🙂ex…");
}

#[test]
fn workspace_probe_path_is_hidden_and_unique() {
    let tmp = TempDir::new().unwrap();
    let first = workspace_probe_path(tmp.path());
    let second = workspace_probe_path(tmp.path());

    assert_ne!(first, second);
    assert!(
        first
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".zeroclaw_doctor_probe_"))
    );
}

/// Build a Config whose install root is `root`, with an existing
/// `data_dir` (so `check_workspace` doesn't early-return) and no agents.
/// `config_path` anchors `install_root_dir()` → `agent_workspace_dir()`.
fn workspace_test_config(root: &Path) -> Config {
    let mut config = Config {
        config_path: root.join("config.toml"),
        data_dir: root.join("data"),
        ..Config::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    config.agents.clear();
    config
}

fn add_enabled_agent(config: &mut Config, alias: &str) {
    config.agents.insert(
        alias.to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            enabled: true,
            ..Default::default()
        },
    );
}

#[test]
fn check_workspace_finds_soul_in_agent_workspace_not_data_dir() {
    let tmp = TempDir::new().unwrap();
    let mut config = workspace_test_config(tmp.path());
    add_enabled_agent(&mut config, "default");

    // SOUL.md lives in the agent workspace — the real load location.
    let ws = config.agent_workspace_dir("default");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("SOUL.md"), b"# soul").unwrap();
    // A decoy in data_dir must NOT satisfy the check (proves we don't
    // probe data_dir for personality files).
    std::fs::write(config.data_dir.join("SOUL.md"), b"# decoy").unwrap();

    let mut items = Vec::new();
    check_workspace(&config, &mut items);

    let soul = items
        .iter()
        .find(|i| i.message.contains("SOUL.md"))
        .expect("SOUL.md diagnostic present");
    assert_eq!(soul.severity, Severity::Ok);
    assert_eq!(soul.message, "[default] SOUL.md present");
    // No bare data_dir-style message ever surfaces.
    assert!(
        !items.iter().any(|i| i.message == "SOUL.md present"),
        "doctor must not report SOUL.md from data_dir"
    );
}

#[test]
fn check_workspace_warns_when_agent_soul_missing() {
    let tmp = TempDir::new().unwrap();
    let mut config = workspace_test_config(tmp.path());
    add_enabled_agent(&mut config, "default");
    // Workspace dir need not exist; the file simply isn't there.

    let mut items = Vec::new();
    check_workspace(&config, &mut items);

    let soul = items
        .iter()
        .find(|i| i.message.contains("SOUL.md"))
        .expect("SOUL.md diagnostic present");
    assert_eq!(soul.severity, Severity::Warn);
    assert_eq!(soul.message, "[default] SOUL.md not found (optional)");
}

#[test]
fn check_workspace_skips_disabled_agents() {
    let tmp = TempDir::new().unwrap();
    let mut config = workspace_test_config(tmp.path());
    config.agents.insert(
        "dormant".to_string(),
        zeroclaw_config::schema::AliasedAgentConfig {
            enabled: false,
            ..Default::default()
        },
    );

    let mut items = Vec::new();
    check_workspace(&config, &mut items);

    assert!(
        !items.iter().any(|i| i.message.contains("dormant")),
        "disabled agents must not produce workspace-file diagnostics"
    );
}

#[test]
fn check_workspace_checks_each_enabled_agent() {
    let tmp = TempDir::new().unwrap();
    let mut config = workspace_test_config(tmp.path());
    add_enabled_agent(&mut config, "alpha");
    add_enabled_agent(&mut config, "zeta");

    let mut items = Vec::new();
    check_workspace(&config, &mut items);

    // Each enabled agent gets its own SOUL.md + AGENTS.md probe, named.
    let messages: Vec<&str> = items.iter().map(|i| i.message.as_str()).collect();
    for alias in ["alpha", "zeta"] {
        let expected = format!("[{alias}] SOUL.md not found (optional)");
        assert!(
            messages.contains(&expected.as_str()),
            "expected per-agent SOUL.md diagnostic for {alias}; got {messages:?}"
        );
    }
}

#[test]
fn check_workspace_writable_probe_is_cleaned_up_and_preserves_files() {
    let tmp = TempDir::new().unwrap();
    let config = workspace_test_config(tmp.path());
    let sentinel = config.data_dir.join("keep.txt");
    std::fs::write(&sentinel, b"unrelated sentinel").unwrap();

    let mut items = Vec::new();
    check_workspace(&config, &mut items);

    assert!(
        items.iter().any(|i| i.message == "directory is writable"),
        "writable workspace must report success; got {:?}",
        items.iter().map(|i| &i.message).collect::<Vec<_>>()
    );

    let leftovers: Vec<String> = std::fs::read_dir(&config.data_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".zeroclaw_doctor_probe_"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "writability probe must remove its own file; found {leftovers:?}"
    );
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"unrelated sentinel",
        "probe must not alter unrelated workspace files"
    );
}

#[test]
fn diagnose_flags_web_dist_dir_with_tilde() {
    // Asserts the localized Fluent message resolves and inlines the path +
    // the tilde reason — the diagnostic now goes through Fluent per
    // AGENTS.mdRound 3).
    let mut config = Config::default();
    config.gateway.web_dist_dir = Some("~/web-dist".to_string());

    let expected_reason = crate::i18n::get_required_cli_string("cli-web-dist-dir-reason-tilde");
    let expected_message = crate::i18n::get_required_cli_string_with_args(
        "cli-doctor-web-dist-dir-expansion-warning",
        &[("path", "~/web-dist"), ("reason", expected_reason.as_str())],
    );

    let results = diagnose(&config);
    let hit = results
        .iter()
        .find(|item| item.category == "config" && item.message == expected_message);
    assert!(
        hit.is_some(),
        "doctor should flag web_dist_dir = \"~/web-dist\" with the localized warning; \
         expected message: {expected_message:?}; got: {results:?}"
    );
    assert_eq!(hit.unwrap().severity, Severity::Warn);
}

#[test]
fn diagnose_flags_web_dist_dir_with_env_var() {
    let mut config = Config::default();
    config.gateway.web_dist_dir = Some("$HOME/web-dist".to_string());

    let expected_reason = crate::i18n::get_required_cli_string("cli-web-dist-dir-reason-dollar");
    let expected_message = crate::i18n::get_required_cli_string_with_args(
        "cli-doctor-web-dist-dir-expansion-warning",
        &[
            ("path", "$HOME/web-dist"),
            ("reason", expected_reason.as_str()),
        ],
    );

    let results = diagnose(&config);
    let hit = results
        .iter()
        .find(|item| item.category == "config" && item.message == expected_message);
    assert!(hit.is_some());
    assert_eq!(hit.unwrap().severity, Severity::Warn);
}

#[test]
fn diagnose_accepts_literal_web_dist_dir() {
    let mut config = Config::default();
    config.gateway.web_dist_dir = Some("/srv/zeroclaw/web-dist".to_string());

    let results = diagnose(&config);
    assert!(
        !results
            .iter()
            .any(|item| item.message.contains("gateway.web_dist_dir")),
        "literal web_dist_dir paths should produce no doctor diagnostic"
    );
}

fn openai_codex_slot() -> zeroclaw_config::schema::OpenAIModelProviderConfig {
    let mut slot = zeroclaw_config::schema::OpenAIModelProviderConfig::default();
    slot.base.requires_openai_auth = true;
    slot
}

#[test]
fn codex_wiring_warns_when_profile_has_no_slot() {
    // Credential imported, no slot opts into it — the silent gap.
    let config = Config::default();
    let items = codex_auth_wiring_items(true, &config);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].severity, Severity::Warn);
    assert_eq!(items[0].category, "providers.auth");
}

#[test]
fn codex_wiring_warns_when_slot_has_no_profile() {
    let mut config = Config::default();
    config
        .providers
        .models
        .openai
        .insert("codex".to_string(), openai_codex_slot());
    let items = codex_auth_wiring_items(false, &config);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].severity, Severity::Warn);
    assert!(
        items[0].message.contains("openai.codex"),
        "slot warning should name the offending slot; got: {:?}",
        items[0].message
    );
}

#[test]
fn codex_wiring_ok_when_profile_and_slot_present() {
    let mut config = Config::default();
    config
        .providers
        .models
        .openai
        .insert("codex".to_string(), openai_codex_slot());
    let items = codex_auth_wiring_items(true, &config);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].severity, Severity::Ok);
}

#[test]
fn codex_wiring_silent_when_codex_unused() {
    // No credential and no requiring slot — the common case; no noise.
    let config = Config::default();
    let items = codex_auth_wiring_items(false, &config);
    assert!(items.is_empty());
}

#[test]
fn web_dist_dir_expansion_reason_key_detects_tilde_and_env() {
    assert_eq!(
        web_dist_dir_expansion_reason_key("~/web-dist"),
        Some("cli-web-dist-dir-reason-tilde")
    );
    assert_eq!(
        web_dist_dir_expansion_reason_key("$HOME/web-dist"),
        Some("cli-web-dist-dir-reason-dollar")
    );
    assert_eq!(
        web_dist_dir_expansion_reason_key("${HOME}/web-dist"),
        Some("cli-web-dist-dir-reason-dollar")
    );
    assert!(web_dist_dir_expansion_reason_key("/srv/zeroclaw/web-dist").is_none());
    assert!(web_dist_dir_expansion_reason_key("./dist").is_none());
}

#[test]
fn config_validation_reports_delegate_agents_in_sorted_order() {
    let mut config = Config::default();
    config.agents.insert(
        "zeta".into(),
        zeroclaw_config::schema::AliasedAgentConfig {
            model_provider: "totally-fake.default".into(),
            ..Default::default()
        },
    );
    config.agents.insert(
        "alpha".into(),
        zeroclaw_config::schema::AliasedAgentConfig {
            model_provider: "totally-fake.default".into(),
            ..Default::default()
        },
    );

    let mut items = Vec::new();
    check_config_semantics(&config, &mut items);

    let agent_messages: Vec<_> = items
        .iter()
        .filter(|item| item.message.starts_with("agent \""))
        .map(|item| item.message.as_str())
        .collect();

    assert_eq!(agent_messages.len(), 2);
    assert!(agent_messages[0].contains("agent \"alpha\""));
    assert!(agent_messages[1].contains("agent \"zeta\""));
}

#[tokio::test]
async fn update_context_windows_uses_exact_alias_not_model_uri() {
    let temp_dir = tempfile::TempDir::new().expect("temp dir");
    let mut config = Config {
        config_path: temp_dir.path().join("config.toml"),
        ..Default::default()
    };

    // Create two groq provider aliases with SAME model and URI
    // This simulates the bug scenario where multiple aliases share the same
    // model/endpoint but should be updated independently
    {
        let entry1 = config
            .providers
            .models
            .ensure("groq", "alias1")
            .expect("groq provider type exists");
        entry1.model = Some("llama-3.1-8b-instant".into());
        entry1.context_window = Some(8192);
    }
    {
        let entry2 = config
            .providers
            .models
            .ensure("groq", "alias2")
            .expect("groq provider type exists");
        entry2.model = Some("llama-3.1-8b-instant".into());
    }

    // Call update_context_windows for alias2 only
    // This should ONLY update alias2, leaving alias1 at 8192
    let mock_fetch: FetchContextWindowFn = Box::new(
        |_type: &str, _config: &zeroclaw_config::schema::ModelProviderConfig| {
            Box::pin(async move { Some(4096usize) })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Option<usize>> + Send>>
        },
    );
    let updated = update_context_windows(&mut config, Some("groq.alias2"), false, Some(mock_fetch))
        .await
        .expect("update_context_windows should succeed");

    // Should have updated exactly 1 entry (alias2)
    assert_eq!(updated, 1);

    // alias1 should remain unchanged at 8192
    let alias1_ctx = config
        .providers
        .models
        .find("groq", "alias1")
        .expect("alias1 should exist")
        .context_window;
    assert_eq!(
        alias1_ctx,
        Some(8192),
        "alias1 context_window should not be modified"
    );

    // alias2 should be updated to the mock fetch value (4096)
    let alias2_ctx = config
        .providers
        .models
        .find("groq", "alias2")
        .expect("alias2 should exist")
        .context_window;
    assert_eq!(
        alias2_ctx,
        Some(4096),
        "alias2 context_window should be set to mock fetch value"
    );
}
