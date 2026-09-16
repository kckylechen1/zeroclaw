#[cfg(test)]
use super::test_util::{EnvGuard, env_lock};
use super::*;

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
        Some(zeroclaw_config::presets::LOCAL_SMALL_RUNTIME_PRESET_NAME),
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

// Compile-time proof that both reqwest TLS-root features are enabled.
// `tls_built_in_webpki_certs` is gated on `rustls-tls-webpki-roots-no-provider`;
// `tls_built_in_native_certs` is gated on `rustls-tls-native-roots-no-provider`.
// If either feature were dropped, this test would fail to compile.
#[test]
fn provider_http_client_trusts_both_webpki_and_native_roots() {
    let _client = reqwest::Client::builder()
        .tls_built_in_webpki_certs(true)
        .tls_built_in_native_certs(true)
        .build()
        .expect("client builder should succeed with both root sets enabled");
}

#[test]
fn resolve_provider_credential_returns_trimmed_override() {
    let resolved = resolve_model_provider_credential("openrouter", Some("  explicit-key  "));
    assert_eq!(resolved, Some("explicit-key".to_string()));
}

#[test]
fn resolve_provider_credential_filters_empty_override() {
    assert!(resolve_model_provider_credential("openrouter", Some("   ")).is_none());
    assert!(resolve_model_provider_credential("openrouter", None).is_none());
}

#[test]
fn resolve_qwen_oauth_context_prefers_explicit_override() {
    let _env_lock = env_lock();
    let context = resolve_qwen_oauth_context(Some("  explicit-qwen-token  "));
    assert_eq!(context.credential.as_deref(), Some("explicit-qwen-token"));
    assert!(context.base_url.is_none());
}

#[test]
fn resolve_qwen_oauth_context_reads_cached_credentials_file() {
    let _env_lock = env_lock();
    let fake_home = format!("/tmp/zeroclaw-qwen-oauth-home-{}-file", std::process::id());
    let creds_dir = PathBuf::from(&fake_home).join(".qwen");
    std::fs::create_dir_all(&creds_dir).unwrap();
    let creds_path = creds_dir.join("oauth_creds.json");
    std::fs::write(
            &creds_path,
            r#"{"access_token":"cached-token","refresh_token":"cached-refresh","resource_url":"https://resource.example.com","expiry_date":4102444800000}"#,
        )
        .unwrap();

    let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));

    let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER));

    assert_eq!(context.credential.as_deref(), Some("cached-token"));
    assert_eq!(
        context.base_url.as_deref(),
        Some("https://resource.example.com/v1")
    );
}

#[test]
fn resolve_qwen_oauth_context_returns_none_without_cache() {
    let _env_lock = env_lock();
    let fake_home = format!("/tmp/zeroclaw-qwen-oauth-home-{}-empty", std::process::id());
    let _home_guard = EnvGuard::set("HOME", Some(fake_home.as_str()));

    let context = resolve_qwen_oauth_context(Some(QWEN_OAUTH_PLACEHOLDER));
    assert!(context.credential.is_none());
}

#[test]
fn regional_alias_predicates_cover_expected_variants() {
    assert!(is_moonshot_alias("moonshot"));
    assert!(is_moonshot_alias("kimi-global"));
    assert!(is_glm_alias("glm"));
    assert!(is_glm_alias("bigmodel"));
    assert!(is_minimax_alias("minimax-io"));
    assert!(is_minimax_alias("minimaxi"));
    assert!(is_minimax_alias("minimax-oauth"));
    assert!(is_minimax_alias("minimax-portal-cn"));
    assert!(is_qwen_alias("dashscope"));
    assert!(is_qwen_alias("qwen-us"));
    assert!(is_qwen_alias("qwen-code"));
    assert!(is_qwen_oauth_alias("qwen-code"));
    assert!(is_qwen_oauth_alias("qwen_oauth"));
    assert!(is_zai_alias("z.ai"));
    assert!(is_zai_alias("zai-cn"));
    assert!(is_qianfan_alias("qianfan"));
    assert!(is_qianfan_alias("baidu"));
    assert!(is_doubao_alias("doubao"));
    assert!(is_doubao_alias("volcengine"));
    assert!(is_doubao_alias("ark"));
    assert!(is_doubao_alias("doubao-cn"));

    assert!(!is_moonshot_alias("openrouter"));
    assert!(!is_glm_alias("openai"));
    assert!(!is_qwen_alias("gemini"));
    assert!(!is_zai_alias("anthropic"));
    assert!(!is_qianfan_alias("cohere"));
    assert!(!is_doubao_alias("deepseek"));
}

// ── Primary model_providers ────────────────────────────────────

#[test]
fn factory_openrouter() {
    assert!(create_model_provider("openrouter", Some("provider-test-credential")).is_ok());
    assert!(create_model_provider("openrouter", None).is_ok());
}

#[test]
fn factory_anthropic() {
    assert!(create_model_provider("anthropic", Some("provider-test-credential")).is_ok());
}

#[test]
fn factory_openai() {
    assert!(create_model_provider("openai", Some("provider-test-credential")).is_ok());
}

#[test]
fn factory_openai_codex() {
    let options = ModelProviderRuntimeOptions::default();
    assert!(create_model_provider_with_options("openai-codex", None, &options).is_ok());
}

#[test]
fn factory_ollama() {
    assert!(create_model_provider("ollama", None).is_ok());
    // Ollama may use API key when a remote endpoint is configured.
    assert!(create_model_provider("ollama", Some("dummy")).is_ok());
    assert!(create_model_provider("ollama", Some("any-value-here")).is_ok());
}

#[test]
fn factory_gemini() {
    assert!(create_model_provider("gemini", Some("test-key")).is_ok());
    // Should also work without key (will try CLI auth)
    assert!(create_model_provider("gemini", None).is_ok());
}

#[test]
fn factory_telnyx() {
    assert!(create_model_provider("telnyx", Some("test-key")).is_ok());
    assert!(create_model_provider("telnyx", None).is_ok());
}

// ── OpenAI-compatible model_providers ──────────────────────────

#[test]
fn factory_venice() {
    let model_provider = create_model_provider("venice", Some("vn-key")).unwrap();
    assert!(
        !model_provider.capabilities().native_tool_calling,
        "Venice should use prompt-guided tools, not native tool calling"
    );
}

#[test]
fn factory_nearai() {
    let model_provider = create_model_provider("nearai", Some("nearai-key")).unwrap();
    // NEAR AI Cloud is OpenAI-protocol-compatible: default Bearer auth +
    // native OpenAI-style tool calling. No .without_native_tools() override.
    assert!(
        model_provider.capabilities().native_tool_calling,
        "NEAR AI Cloud should use OpenAI-compatible native tool calling"
    );
}

#[test]
fn factory_vercel() {
    assert!(create_model_provider("vercel", Some("key")).is_ok());
}

#[test]
fn vercel_gateway_base_url_matches_public_gateway_endpoint() {
    assert_eq!(
        VERCEL_AI_GATEWAY_BASE_URL,
        "https://ai-gateway.vercel.sh/v1"
    );
}

#[test]
fn factory_cloudflare() {
    assert!(create_model_provider("cloudflare", Some("key")).is_ok());
}

#[test]
fn factory_moonshot() {
    assert!(create_model_provider("moonshot", Some("key")).is_ok());
}

#[test]
fn factory_kimi_code_supports_vision() {
    for alias in ["kimi-code", "kimi_coding", "kimi_for_coding"] {
        let provider =
            create_model_provider(alias, Some("key")).expect("legacy kimi-code alias should build");
        assert!(
            provider.supports_vision(),
            "alias `{alias}` should report vision capability"
        );
        // Kimi Code moved to api.kimi.com.
        assert_eq!(
            moonshot_code_base_url(),
            "https://api.kimi.com/coding/v1",
            "alias `{alias}` should resolve to the Kimi Code endpoint"
        );
    }
}

#[test]
fn factory_kimi_code_preserves_semantics_with_url_overrides() {
    let custom_url = "https://proxy.example.test/v1";

    let provider = create_model_provider_with_url("kimi-code", Some("key"), Some(custom_url))
        .expect("legacy kimi-code alias with custom URL should build");
    assert!(provider.supports_vision());

    let provider = create_model_provider_with_options(
        "kimi-code",
        Some("key"),
        &ModelProviderRuntimeOptions {
            provider_api_url: Some(custom_url.to_string()),
            ..ModelProviderRuntimeOptions::default()
        },
    )
    .expect("legacy kimi-code alias with options URL should build");
    assert!(provider.supports_vision());
}

#[test]
fn moonshot_code_endpoint_supports_vision() {
    use zeroclaw_config::schema::{Config, MoonshotEndpoint, MoonshotModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.moonshot.insert(
        "code".to_string(),
        MoonshotModelProviderConfig {
            endpoint: MoonshotEndpoint::Code,
            ..MoonshotModelProviderConfig::default()
        },
    );
    let options = provider_runtime_options_for_alias(&config, "moonshot", "code");
    assert_eq!(
        options.provider_api_url.as_deref(),
        Some(moonshot_code_base_url())
    );

    let provider =
        create_model_provider_for_alias(&config, "moonshot", "code", Some("key"), &options)
            .expect("moonshot code endpoint should build");
    assert!(provider.supports_vision());
}

#[test]
fn factory_synthetic() {
    assert!(create_model_provider("synthetic", Some("key")).is_ok());
}

#[test]
fn factory_opencode() {
    assert!(create_model_provider("opencode", Some("key")).is_ok());
}

#[test]
fn factory_opencode_go() {}

#[test]
fn factory_zai() {
    assert!(create_model_provider("zai", Some("key")).is_ok());
}

#[test]
fn factory_glm() {
    assert!(create_model_provider("glm", Some("key")).is_ok());
}

#[test]
fn factory_minimax() {
    assert!(create_model_provider("minimax", Some("key")).is_ok());
}

#[test]
fn factory_minimax_supports_native_tool_calling() {
    let minimax =
        create_model_provider("minimax", Some("key")).expect("model_provider should resolve");
    assert!(minimax.supports_native_tools());
}

#[test]
fn factory_bedrock() {
    // Bedrock uses AWS env vars for credentials, not API key.
    assert!(create_model_provider("bedrock", None).is_ok());
    // Passing an api_key is harmless (ignored).
    assert!(create_model_provider("bedrock", Some("ignored")).is_ok());
}

#[test]
fn factory_qianfan() {
    assert!(create_model_provider("qianfan", Some("key")).is_ok());
}

#[test]
fn factory_doubao() {
    assert!(create_model_provider("doubao", Some("key")).is_ok());
}

#[test]
fn factory_qwen() {
    assert!(create_model_provider("qwen", Some("key")).is_ok());
}

#[test]
fn qwen_provider_supports_vision() {
    let model_provider =
        create_model_provider("qwen", Some("key")).expect("qwen model_provider should build");
    assert!(model_provider.supports_vision());
}

#[test]
fn glm_provider_supports_vision() {
    // GLM exposes vision-capable models (e.g. `glm-4.5v`). The provider
    // must therefore report `supports_vision()` so multimodal routing
    // can target it; the model field selects the actual variant.
    for alias in ["glm", "zhipu", "glm-cn", "zhipu-cn"] {
        let provider =
            create_model_provider(alias, Some("id.secret")).expect("glm provider should build");
        assert!(
            provider.supports_vision(),
            "alias `{alias}` should report vision capability"
        );
    }
}

#[test]
fn factory_lmstudio() {
    assert!(create_model_provider("lmstudio", Some("key")).is_ok());
    assert!(create_model_provider("lmstudio", None).is_ok());
}

#[test]
fn factory_llamacpp() {
    assert!(create_model_provider("llamacpp", Some("key")).is_ok());
    assert!(create_model_provider("llamacpp", None).is_ok());
}

#[test]
fn vision_override_applies_once_at_construction_for_any_family() {
    // llama.cpp and the generic custom endpoint both default to
    // vision-capable. `vision = Some(false)` must mark the constructed
    // provider non-vision regardless of family, and show up in BOTH
    // `supports_vision()` and `capabilities().vision` so every consumer
    // (routing gate, media pipeline, model router) agrees.
    for name in ["llamacpp", "custom:http://localhost:8080/v1"] {
        let off = ModelProviderRuntimeOptions {
            vision: Some(false),
            ..Default::default()
        };
        let provider =
            create_model_provider_inner(None, name, "default", None, None, &off).unwrap();
        assert!(
            !provider.supports_vision(),
            "{name}: vision=false should mark the provider non-vision"
        );
        assert!(
            !provider.capabilities().vision,
            "{name}: capabilities().vision must stay consistent with supports_vision()"
        );

        // `None` preserves the family default (vision-capable here).
        let provider = create_model_provider_inner(
            None,
            name,
            "default",
            None,
            None,
            &ModelProviderRuntimeOptions::default(),
        )
        .unwrap();
        assert!(
            provider.supports_vision(),
            "{name}: no override should keep the family default"
        );
    }
}

#[test]
fn vision_config_field_maps_into_runtime_options() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig};
    let entry = ModelProviderConfig {
        vision: Some(false),
        ..Default::default()
    };
    let opts =
        model_provider_runtime_options_from_model_provider_entry(&Config::default(), Some(&entry));
    assert_eq!(opts.vision, Some(false));
}

#[test]
fn openai_responses_alias_honors_configured_vision_capability() {
    use zeroclaw_config::schema::{
        Config, ModelProviderConfig, OpenAIModelProviderConfig, WireApi,
    };

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "responses_vision".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                wire_api: Some(WireApi::Responses),
                vision: Some(true),
                ..Default::default()
            },
        },
    );
    let options = provider_runtime_options_for_alias(&config, "openai", "responses_vision");
    let provider = create_model_provider_for_alias(
        &config,
        "openai",
        "responses_vision",
        Some("sk-test"),
        &options,
    )
    .expect("configured OpenAI Responses alias builds");

    assert_eq!(provider.default_wire_api(), "responses");
    assert!(
        provider.capabilities().native_tool_calling,
        "the alias must use the Responses provider"
    );
    assert!(
        provider.capabilities_for_model("gpt-4o").vision,
        "vision=true must reach the exact OpenAI Responses production factory path"
    );
}

#[test]
fn options_for_bare_provider_ref_does_not_inherit_fallback_vision() {
    use zeroclaw_config::schema::Config;
    // A bare family ref (no alias) must not inherit the fallback provider's
    // `vision` flag — otherwise a `-p llamacpp` override would carry the
    // agent provider's capability. Falls back to the family default.
    let fallback = ModelProviderRuntimeOptions {
        vision: Some(false),
        ..Default::default()
    };
    let resolved = options_for_provider_ref(&Config::default(), "llamacpp", &fallback);
    assert_eq!(resolved.vision, None);
}

#[test]
fn factory_sglang() {
    assert!(create_model_provider("sglang", None).is_ok());
    assert!(create_model_provider("sglang", Some("key")).is_ok());
}

#[test]
fn factory_vllm() {
    assert!(create_model_provider("vllm", None).is_ok());
    assert!(create_model_provider("vllm", Some("key")).is_ok());
}

#[test]
fn factory_osaurus() {
    // Osaurus works without an explicit key (defaults to "osaurus").
    assert!(create_model_provider("osaurus", None).is_ok());
    // Osaurus also works with an explicit key.
    assert!(create_model_provider("osaurus", Some("custom-key")).is_ok());
}

#[test]
fn factory_osaurus_uses_default_key_when_none() {
    // Verify that osaurus construction succeeds even without an API
    // key — the impl provides a default placeholder.
    let p = create_model_provider_with_url("osaurus", None, None);
    assert!(p.is_ok());
}

#[test]
fn factory_osaurus_custom_url() {
    // Verify that a custom api_url overrides the default localhost endpoint.
    let p = create_model_provider_with_url(
        "osaurus",
        Some("key"),
        Some("http://192.168.1.100:1337/v1"),
    );
    assert!(p.is_ok());
}

#[test]
fn resolve_provider_credential_osaurus_env_deleted() {}

#[test]
fn resolve_provider_credential_doubao_volcengine_env_deleted() {}

#[test]
fn resolve_provider_credential_aihubmix_env_deleted() {}

#[test]
fn resolve_provider_credential_siliconflow_env_deleted() {}

#[test]
fn factory_aihubmix() {
    assert!(create_model_provider("aihubmix", Some("key")).is_ok());
}

#[test]
fn factory_siliconflow() {
    assert!(create_model_provider("siliconflow", Some("key")).is_ok());
}

#[test]
fn factory_codex_dispatches_via_requires_openai_auth_flag() {
    let options = ModelProviderRuntimeOptions::default();
    assert!(create_model_provider_with_options("openai-codex", None, &options).is_ok());
}

#[test]
fn factory_atomic_chat() {
    assert!(create_model_provider("atomic_chat", Some("key")).is_ok());
}

#[test]
fn factory_atomic_chat_allows_missing_key() {
    // Local provider — empty key is acceptable; the runtime still
    // attaches a placeholder Bearer header.
    assert!(create_model_provider("atomic_chat", None).is_ok());
}

#[test]
fn atomic_chat_is_listed_as_local_provider() {
    let providers = list_model_providers();
    let provider = providers
        .iter()
        .find(|p| p.name == "atomic_chat")
        .expect("atomic_chat must be listed");
    assert!(provider.local, "atomic_chat must be a local provider");
}

// ── Extended ecosystem ───────────────────────────────────

#[test]
fn factory_groq() {
    assert!(create_model_provider("groq", Some("key")).is_ok());
}

#[test]
fn factory_groq_disables_native_tools_by_default() {
    // Default behavior preserves the blanket disable: llama-family
    // Groq models reject native tool calls with HTTP 400.
    let model_provider = create_model_provider_with_options(
        "groq",
        Some("key"),
        &ModelProviderRuntimeOptions::default(),
    )
    .expect("groq factory must succeed");
    assert!(
        !model_provider.supports_native_tools(),
        "Groq must default to text-fallback for llama-family compatibility"
    );
}

#[test]
fn factory_groq_honors_native_tools_override_true() {
    // Operator opt-in via `[providers.models.groq.<alias>] native_tools = true`
    // skips the default disable so non-llama Groq models can use native
    // tool calling.
    let options = ModelProviderRuntimeOptions {
        native_tools: Some(true),
        ..Default::default()
    };
    let model_provider = create_model_provider_with_options("groq", Some("key"), &options)
        .expect("groq factory must succeed");
    assert!(
        model_provider.supports_native_tools(),
        "Groq with `native_tools = true` must enable native tool calling"
    );
}

#[test]
fn factory_groq_native_tools_override_false_keeps_disable() {
    // Explicit `native_tools = false` matches the default behavior; this
    // documents that the option is tri-state and `Some(false)` is not a
    // no-op surprise.
    let options = ModelProviderRuntimeOptions {
        native_tools: Some(false),
        ..Default::default()
    };
    let model_provider = create_model_provider_with_options("groq", Some("key"), &options)
        .expect("groq factory must succeed");
    assert!(
        !model_provider.supports_native_tools(),
        "Groq with explicit `native_tools = false` must remain text-fallback"
    );
}

#[test]
fn provider_runtime_options_from_config_propagates_native_tools() {
    use zeroclaw_config::schema::{GroqModelProviderConfig, ModelProviderConfig};
    let mut config = zeroclaw_config::schema::Config::default();
    config.providers.models.groq.insert(
        "default".to_string(),
        GroqModelProviderConfig {
            base: ModelProviderConfig {
                uri: Some("https://api.groq.com/openai/v1".to_string()),
                native_tools: Some(true),
                ..Default::default()
            },
        },
    );

    let entry = config.providers.models.find("groq", "default");
    let options = model_provider_runtime_options_from_model_provider_entry(&config, entry);
    assert_eq!(
        options.native_tools,
        Some(true),
        "native_tools must propagate from the active model_provider entry to runtime options"
    );
}

#[test]
fn provider_runtime_options_from_config_propagates_tls_ca_cert_path() {
    // Regression guard: tls_ca_cert_path on a ModelProviderConfig entry must
    // reach ModelProviderRuntimeOptions so apply_compat_options can call
    // with_tls_ca_cert_path on the provider. Same pattern as native_tools above.
    use zeroclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};
    let mut config = zeroclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "corp".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                tls_ca_cert_path: Some("/tmp/example-ca.pem".to_string()),
                ..Default::default()
            },
        },
    );

    let entry = config.providers.models.find("openai", "corp");
    let options = model_provider_runtime_options_from_model_provider_entry(&config, entry);
    assert_eq!(
        options.tls_ca_cert_path.as_deref(),
        Some("/tmp/example-ca.pem"),
        "tls_ca_cert_path must propagate from ModelProviderConfig to ModelProviderRuntimeOptions"
    );
}

#[test]
fn provider_runtime_options_from_config_propagates_provider_kind() {
    use zeroclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};
    let mut config = zeroclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                kind: Some("openai-compatible".to_string()),
                uri: Some("http://primary.example/v1".to_string()),
                ..Default::default()
            },
        },
    );

    let options = provider_runtime_options_for_alias(&config, "openai", "primary");
    assert_eq!(options.provider_kind.as_deref(), Some("openai-compatible"));
    assert_eq!(
        options.provider_api_url.as_deref(),
        Some("http://primary.example/v1")
    );
}

#[tokio::test]
async fn routed_alias_uses_call_model_instead_of_configured_pin() {
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use zeroclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};

    type Capture = Arc<Mutex<Option<String>>>;

    async fn capture_chat_request(
        State(capture): State<Capture>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        *capture.lock().expect("capture lock poisoned") = Some(model);
        (
            StatusCode::OK,
            Json(json!({
                "choices": [{"message": {"content": "ok"}}]
            })),
        )
    }

    let capture: Capture = Arc::new(Mutex::new(None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let app = Router::new()
        .route("/v1/chat/completions", post(capture_chat_request))
        .with_state(capture.clone());
    let server = ::zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.expect("serve test server");
    });

    let mut config = zeroclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                api_key: Some("sk-test".to_string()),
                uri: Some(format!("http://{addr}/v1")),
                model: Some("old-model".to_string()),
                ..Default::default()
            },
        },
    );
    let provider = create_routed_model_provider_with_options(
        &config,
        "openai.primary",
        Some("sk-test"),
        Some(&format!("http://{addr}/v1")),
        &config.reliability,
        &[],
        "new-model",
        &ModelProviderRuntimeOptions::default(),
    )
    .expect("provider should build");
    let messages = vec![ChatMessage::user("hello")];
    let request = ChatRequest {
        messages: &messages,
        tools: None,
        thinking: None,
    };

    let response = provider
        .chat(request, "new-model", None)
        .await
        .expect("chat should succeed");

    assert_eq!(response.text.as_deref(), Some("ok"));
    let model = capture
        .lock()
        .expect("capture lock poisoned")
        .take()
        .expect("server should capture request");
    assert_eq!(model, "new-model");
    server.abort();
}

#[test]
fn route_provider_options_clear_primary_only_state_for_bare_routes() {
    let inherited = ModelProviderRuntimeOptions {
        provider_kind: Some("openai-compatible".to_string()),
        provider_api_url: Some("http://primary.example/v1".to_string()),
        ..Default::default()
    };
    let config = zeroclaw_config::schema::Config::default();

    let route_options = options_for_provider_ref(&config, "openrouter", &inherited);

    assert_eq!(route_options.provider_kind, None);
    assert_eq!(route_options.provider_api_url, None);
}

#[test]
fn routed_bare_provider_does_not_inherit_primary_endpoint() {
    use zeroclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};
    let mut config = zeroclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                kind: Some("openai-compatible".to_string()),
                uri: Some("http://primary.example/v1".to_string()),
                ..Default::default()
            },
        },
    );
    let options = provider_runtime_options_for_alias(&config, "openai", "primary");
    assert_eq!(
        options.provider_api_url.as_deref(),
        Some("http://primary.example/v1")
    );

    let route_options = options_for_provider_ref(&config, "openrouter", &options);

    assert_eq!(route_options.provider_kind, None);
    assert_eq!(route_options.provider_api_url, None);
}

#[test]
fn routed_primary_alias_kind_does_not_leak_to_canonical_route_provider() {
    use zeroclaw_config::schema::{
        ModelProviderConfig, ModelRouteConfig, OpenAIModelProviderConfig,
        OpenRouterModelProviderConfig,
    };

    let mut config = zeroclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                kind: Some("openai-compatible".to_string()),
                uri: Some("http://primary.example/v1".to_string()),
                ..Default::default()
            },
        },
    );
    config.providers.models.openrouter.insert(
        "route".to_string(),
        OpenRouterModelProviderConfig {
            base: ModelProviderConfig::default(),
        },
    );
    let options = provider_runtime_options_for_alias(&config, "openai", "primary");
    assert_eq!(options.provider_kind.as_deref(), Some("openai-compatible"));

    let provider = create_routed_model_provider_with_options(
        &config,
        "openai.primary",
        Some("sk-test"),
        None,
        &config.reliability,
        &[ModelRouteConfig {
            hint: "fast".to_string(),
            model_provider: "openrouter.route".to_string(),
            model: "openrouter/auto".to_string(),
            api_key: None,
        }],
        "gpt-test",
        &options,
    )
    .expect("primary alias kind should build without poisoning route provider kind");

    assert!(
        provider.supports_vision(),
        "primary openai-compatible provider should remain the router default"
    );
}

#[test]
fn factory_mistral() {
    assert!(create_model_provider("mistral", Some("key")).is_ok());
}

#[test]
fn factory_xai() {
    assert!(create_model_provider("xai", Some("key")).is_ok());
}

#[test]
fn factory_deepseek() {
    assert!(create_model_provider("deepseek", Some("key")).is_ok());
}

#[test]
fn deepseek_provider_keeps_vision_disabled() {
    let model_provider = create_model_provider("deepseek", Some("key"))
        .expect("deepseek model_provider should build");
    assert!(!model_provider.supports_vision());
}

#[test]
fn factory_together() {
    assert!(create_model_provider("together", Some("key")).is_ok());
}

#[test]
fn factory_fireworks() {
    assert!(create_model_provider("fireworks", Some("key")).is_ok());
}

#[test]
fn factory_novita() {
    assert!(create_model_provider("novita", Some("key")).is_ok());
}

#[test]
fn factory_perplexity() {
    assert!(create_model_provider("perplexity", Some("key")).is_ok());
}

#[test]
fn factory_cohere() {
    assert!(create_model_provider("cohere", Some("key")).is_ok());
}

#[test]
fn factory_copilot() {
    assert!(create_model_provider("copilot", Some("key")).is_ok());
}

#[test]
fn factory_gemini_cli() {}

#[test]
fn factory_kilocli() {
    assert!(create_model_provider("kilocli", None).is_ok());
}

#[test]
fn factory_kilo() {
    assert!(create_model_provider("kilo", Some("kilo-test-key")).is_ok());
}

#[test]
fn factory_nvidia() {
    assert!(create_model_provider("nvidia", Some("nvapi-test")).is_ok());
}

#[test]
fn factory_nvidia_supports_vision() {
    let provider = create_model_provider("nvidia", Some("nvapi-test")).unwrap();
    assert!(
        provider.supports_vision(),
        "nvidia provider must report supports_vision()=true for multimodal models"
    );
}

// ── AI inference routers ─────────────────────────────────

#[test]
fn factory_astrai() {
    assert!(create_model_provider("astrai", Some("sk-astrai-test")).is_ok());
}

#[test]
fn factory_avian() {
    assert!(create_model_provider("avian", Some("sk-avian-test")).is_ok());
}

#[test]
fn factory_deepmyst() {
    assert!(create_model_provider("deepmyst", Some("key")).is_ok());
}

#[test]
fn resolve_provider_credential_deepmyst_env_deleted() {}

// ── OpenAI-compatible aggregators & inference hosts ──────

#[test]
fn factory_morph() {
    assert!(create_model_provider("morph", Some("sk-morph-test")).is_ok());
}

#[test]
fn factory_github_models() {
    assert!(create_model_provider("github_models", Some("ghp_test_token")).is_ok());
    // Hyphenated form canonicalizes to the underscore slot.
    assert!(create_model_provider("github-models", Some("ghp_test_token")).is_ok());
}

#[test]
fn factory_upstage() {
    assert!(create_model_provider("upstage", Some("up-test-key")).is_ok());
}

#[test]
fn factory_featherless() {
    assert!(create_model_provider("featherless", Some("featherless-test")).is_ok());
}

#[test]
fn factory_arcee() {
    assert!(create_model_provider("arcee", Some("arcee-test")).is_ok());
}

#[test]
fn factory_lambda_ai() {
    assert!(create_model_provider("lambda_ai", Some("lambda-test")).is_ok());
    // Hyphenated form canonicalizes to the underscore slot.
    assert!(create_model_provider("lambda-ai", Some("lambda-test")).is_ok());
}

#[test]
fn factory_inception() {
    assert!(create_model_provider("inception", Some("inception-test")).is_ok());
}

#[test]
fn default_url_matches_compat_spec_for_new_providers() {
    assert_eq!(
        default_model_provider_url("morph"),
        Some("https://api.morphllm.com/v1")
    );
    assert_eq!(
        default_model_provider_url("github_models"),
        Some("https://models.github.ai/inference")
    );
    assert_eq!(
        default_model_provider_url("upstage"),
        Some("https://api.upstage.ai/v1")
    );
    assert_eq!(
        default_model_provider_url("featherless"),
        Some("https://api.featherless.ai/v1")
    );
    // Arcee publishes at the non-standard `/api/v1` path.
    assert_eq!(
        default_model_provider_url("arcee"),
        Some("https://api.arcee.ai/api/v1")
    );
    assert_eq!(
        default_model_provider_url("lambda_ai"),
        Some("https://api.lambda.ai/v1")
    );
    assert_eq!(
        default_model_provider_url("inception"),
        Some("https://api.inceptionlabs.ai/v1")
    );
}

#[test]
fn factory_custom_with_resolved_uri() {
    let options = ModelProviderRuntimeOptions {
        provider_api_url: Some("https://my-llm.example.com".to_string()),
        ..ModelProviderRuntimeOptions::default()
    };
    assert!(create_model_provider_with_options("custom", Some("key"), &options).is_ok());
}

#[test]
fn factory_custom_without_uri_errors() {
    match create_model_provider("custom", Some("key")) {
        Err(e) => assert!(
            e.to_string().contains("requires `uri`"),
            "Expected `uri` error, got: {e}"
        ),
        Ok(_) => {
            panic!("Expected error when custom model model_provider has no URI configured")
        }
    }
}

#[test]
fn create_model_provider_from_ref_preserves_custom_url_with_dotted_host() {
    use zeroclaw_api::attribution::Attributable;
    use zeroclaw_config::schema::Config;
    // A `custom:<url>` vision route whose host contains dots (e.g. 127.0.0.1)
    // must NOT be split on '.' into a bogus `<family>.<alias>` - the whole ref
    // must reach the factory intact, exactly as the legacy
    // `create_model_provider(vp, None)` did. Regression for the alias-aware ref
    // factory added for the dedicated vision route.
    let config = Config::default();
    let url = "custom:http://127.0.0.1:9999/v1";
    let via_ref =
        create_model_provider_from_ref(&config, url).expect("ref factory builds custom URL");
    let via_legacy = create_model_provider(url, None).expect("legacy factory builds custom URL");
    assert_eq!(
        via_ref.alias(),
        via_legacy.alias(),
        "custom:<url> with a dotted host must build identically to the legacy \
             factory (not be mis-split into a bogus family/alias)"
    );
}

#[test]
fn create_model_provider_from_ref_fails_closed_on_nonexistent_dotted_alias() {
    use zeroclaw_config::schema::Config;
    // A dotted `vision_model_provider` that names no configured alias (e.g. a
    // typo) must fail CLOSED - an error the operator sees - not silently fall
    // open to a family-default provider. `llamacpp` builds a default endpoint
    // for a bare family, so without the entry-exists guard `llamacpp.typo`
    // would (wrongly) succeed and route images to a default llama.cpp.
    let config = Config::default();
    assert!(
        create_model_provider_from_ref(&config, "llamacpp.typo").is_err(),
        "a dotted ref to a non-existent alias must fail closed, not fall open to a default provider"
    );
    // Same fail-closed behavior as the legacy factory the vision route used.
    assert!(create_model_provider("llamacpp.typo", None).is_err());
}

// ── Error cases ──────────────────────────────────────────

#[test]
fn factory_unknown_provider_errors() {
    let p = create_model_provider("nonexistent", None);
    assert!(p.is_err());
    let msg = p.err().unwrap().to_string();
    assert!(msg.contains("Unknown model_provider family"));
    assert!(msg.contains("nonexistent"));
}

#[test]
fn factory_empty_name_errors() {
    assert!(create_model_provider("", None).is_err());
}

#[test]
fn ollama_with_custom_url() {
    let model_provider =
        create_model_provider_with_url("ollama", None, Some("http://10.100.2.32:11434"));
    assert!(model_provider.is_ok());
}

#[test]
fn ollama_cloud_with_custom_url() {
    let model_provider =
        create_model_provider_with_url("ollama", Some("ollama-key"), Some("https://ollama.com"));
    assert!(model_provider.is_ok());
}

#[tokio::test]
async fn ollama_private_remote_cloud_request_omits_auth_and_preserves_model() {
    use axum::{
        Json, Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    type Capture = Arc<Mutex<Option<(Option<String>, String)>>>;

    async fn capture_chat_request(
        State(capture): State<Capture>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        *capture.lock().expect("capture lock poisoned") = Some((auth, model));
        (
            StatusCode::OK,
            Json(json!({
                "choices": [{"message": {"content": "ok"}}]
            })),
        )
    }

    let capture: Capture = Arc::new(Mutex::new(None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let app = Router::new()
        .route("/v1/chat/completions", post(capture_chat_request))
        .with_state(capture.clone());
    let server = ::zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.expect("serve test server");
    });

    let base_url = format!("http://{addr}");
    let model_provider = create_model_provider_with_url("ollama", None, Some(&base_url))
        .expect("ollama provider should build");
    let response = model_provider
        .chat_with_system(None, "hello", "qwen3:cloud", Some(0.7))
        .await
        .expect("chat request should succeed");

    assert_eq!(response, "ok");
    let (auth, model) = capture
        .lock()
        .expect("capture lock poisoned")
        .take()
        .expect("server should capture request");
    assert_eq!(auth, None);
    assert_eq!(model, "qwen3:cloud");
    server.abort();
}

#[tokio::test]
async fn ollama_private_remote_lists_models_without_auth() {
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    type Capture = Arc<Mutex<Option<Option<String>>>>;

    async fn capture_models_request(
        State(capture): State<Capture>,
        headers: HeaderMap,
    ) -> Json<Value> {
        let auth = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        *capture.lock().expect("capture lock poisoned") = Some(auth);
        Json(json!({
            "data": [{"id": "qwen3:cloud"}]
        }))
    }

    let capture: Capture = Arc::new(Mutex::new(None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    let app = Router::new()
        .route("/v1/models", get(capture_models_request))
        .with_state(capture.clone());
    let server = ::zeroclaw_spawn::spawn!(async move {
        axum::serve(listener, app).await.expect("serve test server");
    });

    let base_url = format!("http://{addr}");
    let model_provider = create_model_provider_with_url("ollama", None, Some(&base_url))
        .expect("ollama provider should build");
    let models = model_provider
        .list_models()
        .await
        .expect("model list should succeed");

    assert_eq!(models, vec!["qwen3:cloud".to_string()]);
    let auth = capture
        .lock()
        .expect("capture lock poisoned")
        .take()
        .expect("server should capture request");
    assert_eq!(auth, None);
    server.abort();
}

#[test]
fn factory_all_canonical_model_providers_create_successfully() {
    let canonical = [
        "openrouter",
        "anthropic",
        "openai",
        "ollama",
        "gemini",
        "venice",
        "nearai",
        "vercel",
        "cloudflare",
        "moonshot",
        "synthetic",
        "opencode",
        "zai",
        "glm",
        "minimax",
        "bedrock",
        "qianfan",
        "doubao",
        "qwen",
        "lmstudio",
        "llamacpp",
        "sglang",
        "vllm",
        "osaurus",
        "telnyx",
        "groq",
        "mistral",
        "xai",
        "deepseek",
        "together",
        "fireworks",
        "novita",
        "perplexity",
        "cohere",
        "copilot",
        "gemini_cli",
        "kilocli",
        "nvidia",
        "astrai",
        "avian",
        "ovh",
    ];
    for name in canonical {
        assert!(
            create_model_provider(name, Some("test-key")).is_ok(),
            "Canonical model model_provider '{name}' should create successfully"
        );
    }
}

#[test]
fn listed_model_providers_have_unique_canonical_ids() {
    let model_providers = list_model_providers();
    let mut canonical_ids = std::collections::HashSet::new();

    for model_provider in model_providers {
        assert!(
            canonical_ids.insert(model_provider.name),
            "Duplicate canonical model model_provider id: {}",
            model_provider.name
        );
    }
}

#[test]
fn listed_model_providers_match_canonical_slots() {
    let listed: std::collections::BTreeSet<&str> =
        list_model_providers().iter().map(|p| p.name).collect();
    let canonical: std::collections::BTreeSet<&str> =
        canonical_model_provider_slots().into_iter().collect();
    let missing: Vec<&&str> = canonical.difference(&listed).collect();
    let phantom: Vec<&&str> = listed.difference(&canonical).collect();
    assert!(
        missing.is_empty() && phantom.is_empty(),
        "list_model_providers() drift — missing display entries: {missing:?}; \
             phantom entries (no factory slot): {phantom:?}"
    );
}

#[test]
fn listed_model_providers_are_constructible() {
    for model_provider in list_model_providers() {
        if model_provider.name == "azure" {
            continue;
        }
        // The custom slot requires a uri (no family-default endpoint);
        // covered by dedicated factory tests.
        if model_provider.name == "custom" {
            continue;
        }
        assert!(
            create_model_provider(model_provider.name, Some("provider-test-credential")).is_ok(),
            "Canonical model model_provider id should be constructible: {}",
            model_provider.name
        );
    }
}

// ── API error sanitization ───────────────────────────────

#[test]
fn format_error_chain_includes_sources_and_sanitizes_output() {
    #[derive(Debug)]
    struct ChainError {
        message: &'static str,
        source: Option<Box<dyn std::error::Error + 'static>>,
    }

    impl std::fmt::Display for ChainError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl std::error::Error for ChainError {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source.as_deref()
        }
    }

    let error = ChainError {
        message: "outer context",
        source: Some(Box::new(ChainError {
            message: "middle context",
            source: Some(Box::new(ChainError {
                message: "inner source leaked sk-1234567890abcdef",
                source: None,
            })),
        })),
    };

    let result = format_error_chain(&error);

    assert!(result.contains("outer context"));
    assert!(result.contains("middle context"));
    assert!(result.contains("inner source leaked [REDACTED]"));
    assert!(!result.contains("sk-1234567890abcdef"));
}

#[test]
fn sanitize_scrubs_sk_prefix() {
    let input = "request failed: sk-1234567890abcdef";
    let out = sanitize_api_error(input);
    assert!(!out.contains("sk-1234567890abcdef"));
    assert!(out.contains("[REDACTED]"));
}

#[test]
fn sanitize_scrubs_multiple_prefixes() {
    let input = "keys sk-abcdef xoxb-12345 xoxp-67890";
    let out = sanitize_api_error(input);
    assert!(!out.contains("sk-abcdef"));
    assert!(!out.contains("xoxb-12345"));
    assert!(!out.contains("xoxp-67890"));
}

#[test]
fn sanitize_short_prefix_then_real_key() {
    let input = "error with sk- prefix and key sk-1234567890";
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-1234567890"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn sanitize_sk_proj_comment_then_real_key() {
    let input = "note: sk- then sk-proj-abc123def456";
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-proj-abc123def456"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn sanitize_keeps_bare_prefix() {
    let input = "only prefix sk- present";
    let result = sanitize_api_error(input);
    assert!(result.contains("sk-"));
}

#[test]
fn sanitize_handles_json_wrapped_key() {
    let input = r#"{"error":"invalid key sk-abc123xyz"}"#;
    let result = sanitize_api_error(input);
    assert!(!result.contains("sk-abc123xyz"));
}

#[test]
fn sanitize_handles_delimiter_boundaries() {
    let input = "bad token xoxb-abc123}; next";
    let result = sanitize_api_error(input);
    assert!(!result.contains("xoxb-abc123"));
    assert!(result.contains("};"));
}

#[test]
fn sanitize_truncates_long_error() {
    let long = "a".repeat(600);
    let result = sanitize_api_error(&long);
    assert!(result.len() <= 503);
    assert!(result.ends_with("..."));
}

#[test]
fn sanitize_truncates_after_scrub() {
    let input = format!("{} sk-abcdef123456 {}", "a".repeat(290), "b".repeat(290));
    let result = sanitize_api_error(&input);
    assert!(!result.contains("sk-abcdef123456"));
    assert!(result.len() <= 503);
}

#[test]
fn sanitize_preserves_unicode_boundaries() {
    let input = format!("{} sk-abcdef123", "hello🙂".repeat(80));
    let result = sanitize_api_error(&input);
    assert!(std::str::from_utf8(result.as_bytes()).is_ok());
    assert!(!result.contains("sk-abcdef123"));
}

#[test]
fn sanitize_scrubs_gemini_key_in_reqwest_url() {
    let key = "AIzaSyDUMMYKEYFORTESTINGONLY1234567890";
    let input = format!(
        "error sending request for url (https://127.0.0.1:1/v1beta/models/x:generateContent?key={key})"
    );
    let result = sanitize_api_error(&input);
    assert!(!result.contains(key), "key leaked: {result}");
    assert!(result.contains("generateContent"));
    assert!(!result.contains("?key="));
}

#[test]
fn sanitize_scrubs_bare_gemini_key_prefix() {
    // An `AIza` key that appears outside a query string (e.g. a JSON body)
    // is still redacted by the prefix rule.
    let input = r#"{"error":"invalid api key AIzaSyABCDEF0123456789abcdefGHIJKLmnopqrs"}"#;
    let result = sanitize_api_error(input);
    assert!(!result.contains("AIzaSyABCDEF0123456789abcdefGHIJKLmnopqrs"));
    assert!(result.contains("[REDACTED]"));
}

#[test]
fn sanitize_removes_complete_url_query_regardless_of_parameter_name() {
    let input = "GET HTTPS://api.example.com/v1/thing?region=us&access_token=hunter2secret failed";
    let result = sanitize_api_error(input);
    assert!(!result.contains("hunter2secret"), "{result}");
    assert!(!result.contains("region=us"), "{result}");
    assert!(result.contains("HTTPS://api.example.com/v1/thing"));
}

#[test]
fn sanitize_removes_query_values_containing_url_punctuation() {
    let secret = "abc,def'ghi(jkl)";
    let input = format!("GET https://api.example.com/v1/thing?api_key={secret} failed");
    let result = sanitize_api_error(&input);

    assert!(!result.contains(secret), "{result}");
    assert!(!result.contains("def'ghi(jkl)"), "{result}");
    assert!(result.contains("https://api.example.com/v1/thing"));
}

#[test]
fn sanitize_leaves_non_url_key_value_text_unchanged() {
    let input = "playing monkey=banana in the query";
    let result = sanitize_api_error(input);
    assert_eq!(result, input);
}

#[test]
fn sanitize_no_secret_no_change() {
    let input = "simple upstream timeout";
    let result = sanitize_api_error(input);
    assert_eq!(result, input);
}

#[test]
fn scrub_github_personal_access_token() {
    let input = "auth failed with token ghp_abc123def456";
    let result = scrub_secret_patterns(input);
    assert_eq!(result, "auth failed with token [REDACTED]");
}

#[test]
fn scrub_github_oauth_token() {
    let input = "Bearer gho_1234567890abcdef";
    let result = scrub_secret_patterns(input);
    assert_eq!(result, "Bearer [REDACTED]");
}

#[test]
fn scrub_github_user_token() {
    let input = "token ghu_sessiontoken123";
    let result = scrub_secret_patterns(input);
    assert_eq!(result, "token [REDACTED]");
}

#[test]
fn scrub_github_fine_grained_pat() {
    let input = "failed: github_pat_11AABBC_xyzzy789";
    let result = scrub_secret_patterns(input);
    assert_eq!(result, "failed: [REDACTED]");
}

// ── API key prefix pre-flight ───────────────────────────

#[test]
fn api_key_prefix_cross_provider_mismatch() {
    // Anthropic key used with openrouter
    assert_eq!(
        check_api_key_prefix("openrouter", "sk-ant-api03-xyz"),
        Some("anthropic")
    );
    // OpenRouter key used with anthropic
    assert_eq!(
        check_api_key_prefix("anthropic", "sk-or-v1-xyz"),
        Some("openrouter")
    );
    // Anthropic key used with openai
    assert_eq!(
        check_api_key_prefix("openai", "sk-ant-xyz"),
        Some("anthropic")
    );
    // Groq key used with openai
    assert_eq!(check_api_key_prefix("openai", "gsk_xyz"), Some("groq"));
}

#[test]
fn api_key_prefix_correct_match() {
    assert_eq!(check_api_key_prefix("anthropic", "sk-ant-api03-xyz"), None);
    assert_eq!(check_api_key_prefix("openrouter", "sk-or-v1-xyz"), None);
    assert_eq!(check_api_key_prefix("openai", "sk-proj-xyz"), None);
    assert_eq!(check_api_key_prefix("groq", "gsk_xyz"), None);
}

#[test]
fn api_key_prefix_unknown_provider_skips() {
    // Providers without known key formats should never flag a mismatch.
    assert_eq!(check_api_key_prefix("deepseek", "sk-ant-xyz"), None);
    assert_eq!(check_api_key_prefix("ollama", "anything"), None);
}

#[test]
fn api_key_prefix_unknown_key_format_skips() {
    // Keys without a recognisable prefix should never flag a mismatch.
    assert_eq!(check_api_key_prefix("openai", "my-custom-key-123"), None);
    assert_eq!(check_api_key_prefix("anthropic", "some-random-key"), None);
}

#[test]
fn provider_runtime_options_default_has_empty_extra_headers() {
    let options = ModelProviderRuntimeOptions::default();
    assert!(options.extra_headers.is_empty());
}

#[test]
fn provider_runtime_options_extra_headers_passed_through() {
    let mut extra_headers = std::collections::HashMap::new();
    extra_headers.insert("X-Title".to_string(), "zeroclaw".to_string());
    let options = ModelProviderRuntimeOptions {
        extra_headers,
        ..ModelProviderRuntimeOptions::default()
    };
    assert_eq!(options.extra_headers.len(), 1);
    assert_eq!(options.extra_headers.get("X-Title").unwrap(), "zeroclaw");
}

#[test]
fn ollama_uses_resolved_url_from_runtime_options() {
    // V0.8.0: `ZEROCLAW_PROVIDER_URL` env-var override eradicated. Ollama
    // base URL flows through the typed alias's `api_url`/`uri` field which
    // pre-populates `provider_api_url` on `ModelProviderRuntimeOptions`.
    let model_provider =
        create_model_provider_with_url("ollama", None, Some("http://config-ollama:11434"));
    assert!(model_provider.is_ok());
}

// ── Per-alias provider_runtime_options resolution ──

/// Build a `Config` with two `anthropic` aliases at different base_urls
/// so the test can prove `provider_runtime_options_for_agent` selects
/// the alias-specific entry via explicit `<type>.<alias>` resolution.
fn config_with_two_anthropic_aliases() -> zeroclaw_config::schema::Config {
    use zeroclaw_config::schema::{
        AliasedAgentConfig, AnthropicModelProviderConfig, Config, ModelProviderConfig,
    };
    let mut config = Config::default();
    let default_alias = AnthropicModelProviderConfig {
        base: ModelProviderConfig {
            model: Some("claude-default".into()),
            api_key: Some("default-key".into()),
            uri: Some("https://api.default.example/v1/messages".into()),
            ..ModelProviderConfig::default()
        },
    };
    let work_alias = AnthropicModelProviderConfig {
        base: ModelProviderConfig {
            model: Some("claude-work".into()),
            api_key: Some("work-key".into()),
            uri: Some("https://work-proxy.example/v1/v1/anthropic/messages".into()),
            ..ModelProviderConfig::default()
        },
    };
    config
        .providers
        .models
        .anthropic
        .insert("default".to_string(), default_alias);
    config
        .providers
        .models
        .anthropic
        .insert("work".to_string(), work_alias);
    let work_agent = AliasedAgentConfig {
        model_provider: "anthropic.work".into(),
        ..AliasedAgentConfig::default()
    };
    config.agents.insert("work_agent".to_string(), work_agent);
    let default_agent = AliasedAgentConfig {
        model_provider: "anthropic.default".into(),
        ..AliasedAgentConfig::default()
    };
    config
        .agents
        .insert("default_agent".to_string(), default_agent);
    config
}

#[test]
fn provider_runtime_options_for_agent_resolves_alias_specific_uri() {
    let config = config_with_two_anthropic_aliases();
    let work = provider_runtime_options_for_agent(&config, "work_agent");
    let dflt = provider_runtime_options_for_agent(&config, "default_agent");

    assert_eq!(
        work.provider_api_url.as_deref(),
        Some("https://work-proxy.example/v1/v1/anthropic/messages"),
        "work agent must resolve to the work alias's full uri (with merged path)"
    );
    assert_eq!(
        dflt.provider_api_url.as_deref(),
        Some("https://api.default.example/v1/messages"),
        "default agent must resolve to the default alias's full uri (with merged path)"
    );
}

#[test]
fn provider_runtime_options_for_agent_unknown_agent_returns_safe_defaults() {
    let config = config_with_two_anthropic_aliases();
    let opts = provider_runtime_options_for_agent(&config, "nonexistent");
    assert!(
        opts.provider_api_url.is_none(),
        "unknown agent must not silently inherit any configured provider; got `{:?}`",
        opts.provider_api_url
    );
}

#[test]
fn ollama_alias_tuning_fields_populate_tuning_struct() {
    let alias = zeroclaw_config::schema::OllamaModelProviderConfig {
        num_ctx: Some(16384),
        num_predict: Some(4096),
        temperature_override: Some(0.5),
        ..zeroclaw_config::schema::OllamaModelProviderConfig::default()
    };

    let tuning = ollama::OllamaTuning::from_runtime_overrides(
        alias.num_ctx,
        alias.num_predict,
        alias.temperature_override,
    );
    assert_eq!(tuning.num_ctx, 16384);
    assert_eq!(tuning.num_predict, 4096);
    assert_eq!(tuning.temperature_override, Some(0.5));

    let provider = ollama::OllamaModelProvider::builder("test")
        .tuning(tuning)
        .build();
    assert_eq!(provider.tuning(), tuning);
}

#[test]
fn ollama_alias_tuning_defaults_leave_temperature_override_unset() {
    let alias = zeroclaw_config::schema::OllamaModelProviderConfig::default();
    let tuning = ollama::OllamaTuning::from_runtime_overrides(
        alias.num_ctx,
        alias.num_predict,
        alias.temperature_override,
    );
    assert!(tuning.temperature_override.is_none());
    assert_eq!(tuning.num_ctx, ollama::OLLAMA_DEFAULT_NUM_CTX);
    assert_eq!(tuning.num_predict, ollama::OLLAMA_DEFAULT_NUM_PREDICT);
}

fn config_with_openai_alias() -> zeroclaw_config::schema::Config {
    use zeroclaw_config::schema::{
        AliasedAgentConfig, Config, ModelProviderConfig, OpenAIModelProviderConfig,
    };
    let mut config = Config::default();
    let alias = OpenAIModelProviderConfig {
        base: ModelProviderConfig {
            api_key: Some("openai-alias-key".into()),
            model: Some("gpt-4o".into()),
            ..ModelProviderConfig::default()
        },
    };
    config
        .providers
        .models
        .openai
        .insert("alias".to_string(), alias);
    let agent = AliasedAgentConfig {
        model_provider: "openai.alias".into(),
        ..AliasedAgentConfig::default()
    };
    config.agents.insert("test_agent".to_string(), agent);
    config
}

#[test]
fn routed_model_provider_credential_precedence_uses_route_key_first() {
    let config = config_with_openai_alias();
    let reliability = zeroclaw_config::schema::ReliabilityConfig::default();
    let routes = [zeroclaw_config::schema::ModelRouteConfig {
        hint: "test".into(),
        model_provider: "openai.alias".into(),
        model: "gpt-4o".into(),
        api_key: Some("route-key".into()),
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai.alias",
        Some("fallback-key"),
        None,
        &reliability,
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_ok(),
        "route-key should succeed: {}",
        result.err().unwrap()
    );
}

#[test]
fn routed_model_provider_credential_precedence_uses_config_entry_key() {
    let config = config_with_openai_alias();
    let reliability = zeroclaw_config::schema::ReliabilityConfig::default();
    // Route has no api_key — should fall back to config entry key "openai-alias-key"
    let routes = [zeroclaw_config::schema::ModelRouteConfig {
        hint: "test".into(),
        model_provider: "openai.alias".into(),
        model: "gpt-4o".into(),
        api_key: None,
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai.alias",
        Some("fallback-key"),
        None,
        &reliability,
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_ok(),
        "config-entry key should succeed: {}",
        result.err().unwrap()
    );
}

#[test]
fn routed_model_provider_credential_precedence_falls_back_to_api_key_param() {
    let config = zeroclaw_config::schema::Config::default(); // no entry in config.models
    let reliability = zeroclaw_config::schema::ReliabilityConfig::default();
    // Neither route nor config entry has api_key — should use the param "fallback-key"
    let routes = [zeroclaw_config::schema::ModelRouteConfig {
        hint: "test".into(),
        model_provider: "openai".into(),
        model: "gpt-4o".into(),
        api_key: None,
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai",
        Some("fallback-key"),
        None,
        &reliability,
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_ok(),
        "fallback-key should succeed: {}",
        result.err().unwrap()
    );
}

#[test]
fn routed_model_provider_credential_skips_config_entry_for_non_dotted_name() {
    let config = zeroclaw_config::schema::Config::default();
    let reliability = zeroclaw_config::schema::ReliabilityConfig::default();
    // Non-dotted name "openai" — split_once('.') returns None, so config entry
    // lookup is skipped entirely. Falls back to api_key param.
    let routes = [zeroclaw_config::schema::ModelRouteConfig {
        hint: "test".into(),
        model_provider: "openai".into(),
        model: "gpt-4o".into(),
        api_key: None,
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai",
        Some("direct-key"),
        None,
        &reliability,
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_ok(),
        "direct-key should succeed: {}",
        result.err().unwrap()
    );
}

#[test]
fn routed_model_provider_fails_when_routed_provider_fallback_lacks_profile_credential() {
    use zeroclaw_config::schema::{
        Config, ModelProviderConfig, ModelRouteConfig, OpenAIModelProviderConfig, ReliabilityConfig,
    };

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "routed".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                api_key: Some("route-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "openai.bad",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "bad".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1-mini".to_string()),
                ..Default::default()
            },
        },
    );
    let routes = [ModelRouteConfig {
        hint: "test".to_string(),
        model_provider: "openai.routed".to_string(),
        model: "gpt-4.1".to_string(),
        api_key: None,
    }];

    let result = create_routed_model_provider_with_options(
        &config,
        "openai.primary",
        Some("primary-key"),
        None,
        &ReliabilityConfig::default(),
        &routes,
        "gpt-4o",
        &ModelProviderRuntimeOptions::default(),
    );

    assert!(
        result.is_err(),
        "route provider fallback failures must not be silently ignored"
    );
    let message = result.err().unwrap().to_string();
    assert!(
        message.contains("openai.routed"),
        "error must name the routed provider that failed: {message}"
    );
    assert!(
        message.contains("openai.bad"),
        "error must preserve the failed fallback alias: {message}"
    );
}

#[test]
fn dotted_alias_routes_openai_codex_via_requires_openai_auth() {
    use zeroclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};

    // Use an intentionally arbitrary alias to prove the routing is alias-agnostic.
    let arbitrary_alias = "qwertfoozp";

    let mut config = zeroclaw_config::schema::Config::default();
    config.providers.models.openai.insert(
        arbitrary_alias.to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                requires_openai_auth: true,
                ..Default::default()
            },
        },
    );

    // Verify the alias-aware factory path sees `requires_openai_auth = true`
    // and routes to OpenAiCodexModelProvider. `dispatch_family_factory` is
    // called directly (no ReliableModelProvider wrapper) so `capabilities()`
    // reflects the inner provider's values.
    let result = factory::dispatch_family_factory(
        Some(&config),
        "openai",
        arbitrary_alias,
        None,
        None,
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "codex alias construction should succeed: {}",
        result.err().unwrap()
    );
    assert!(
        result.unwrap().capabilities().native_tool_calling,
        "openai.{arbitrary_alias} with requires_openai_auth=true must route to \
             OpenAiCodexModelProvider (native_tool_calling=true), not the standard provider"
    );
}

#[test]
fn resilient_alias_builds_with_fallback_chain() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback_models: vec!["gpt-4o-mini".to_string()],
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "openai.backup",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "backup".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                api_key: Some("backup-key".to_string()),
                ..Default::default()
            },
        },
    );

    let reliability = zeroclaw_config::schema::ReliabilityConfig::default();
    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        None,
        None,
        &reliability,
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "multi-alias fallback chain must build: {}",
        result.err().unwrap()
    );
}

#[test]
fn resilient_alias_fails_when_resolved_fallback_lacks_profile_credential() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "openai.backup",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "backup".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_err(),
        "missing fallback credential must fail loudly"
    );
    let err = result.err().unwrap();
    let message = err.to_string();
    assert!(
        message.contains("openai.backup"),
        "error must name resolved fallback alias: {message}"
    );
    assert!(
        message.contains("[providers.models.openai.backup]"),
        "error must name the profile to fix: {message}"
    );
    assert!(
        message.contains("api_key"),
        "error must name the credential field to set: {message}"
    );
}

#[test]
fn resilient_alias_fails_when_resolved_fallback_uri_lacks_profile_credential() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "openai.backup",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "backup".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                kind: Some("openai-compatible".to_string()),
                uri: Some("https://api.openai.com/v1".to_string()),
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_err(),
        "uri and kind alone must not make a fallback auth-ready"
    );
    let message = result.err().unwrap().to_string();
    assert!(
        message.contains("openai.backup"),
        "error must name resolved fallback alias: {message}"
    );
    assert!(
        message.contains("[providers.models.openai.backup]"),
        "error must name the profile to fix: {message}"
    );
    assert!(
        message.contains("api_key"),
        "error must name the credential field to set: {message}"
    );
}

#[test]
fn resilient_alias_fails_when_minimax_fallback_lacks_auth_source() {
    use zeroclaw_config::schema::{
        Config, MinimaxModelProviderConfig, ModelProviderConfig, OpenAIModelProviderConfig,
    };

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "minimax.backup",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.minimax.insert(
        "backup".to_string(),
        MinimaxModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("minimax-text-01".to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_err(),
        "MiniMax fallback without api_key or oauth_refresh_token must fail loudly"
    );
    let message = result.err().unwrap().to_string();
    assert!(
        message.contains("minimax.backup"),
        "error must name resolved fallback alias: {message}"
    );
    assert!(
        message.contains("api_key"),
        "error must name the credential field to set: {message}"
    );
}

#[test]
fn resilient_alias_fails_when_resolved_fallback_factory_fails() {
    use zeroclaw_config::schema::{Config, CustomModelProviderConfig, ModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        zeroclaw_config::schema::OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "custom.backup",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.custom.insert(
        "backup".to_string(),
        CustomModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("custom-model".to_string()),
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_err(),
        "resolved fallback factory failure must abort build"
    );
    let err = result.err().unwrap();
    let message = err.to_string();
    assert!(
        message.contains("custom.backup"),
        "error must name resolved fallback alias: {message}"
    );
    assert!(
        message.contains("[providers.models.custom.backup]"),
        "error must name the profile to fix: {message}"
    );
    assert!(
        message.contains("uri"),
        "factory error must preserve the missing field detail: {message}"
    );
}

#[test]
fn resilient_alias_allows_openai_external_auth_fallback_without_api_key() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "openai.codex",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "codex".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-5-codex".to_string()),
                requires_openai_auth: true,
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "OpenAI external-auth fallbacks may intentionally omit api_key: {}",
        result.err().unwrap()
    );
}

#[test]
fn resilient_alias_allows_xai_oauth_fallback_without_api_key() {
    use zeroclaw_config::schema::{
        Config, ModelProviderConfig, OpenAIModelProviderConfig, XaiModelProviderConfig,
    };

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "xai.oauth",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.xai.insert(
        "oauth".to_string(),
        XaiModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("grok-4.3".to_string()),
                ..Default::default()
            },
        },
    );

    let temp = tempfile::tempdir().expect("temp zeroclaw dir");
    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions {
            zeroclaw_dir: Some(temp.path().to_path_buf()),
            ..Default::default()
        },
    );
    assert!(
        result.is_ok(),
        "xAI OAuth fallbacks may intentionally omit api_key: {}",
        result.err().unwrap()
    );
}

#[test]
fn resilient_alias_allows_local_fallback_without_profile_api_key() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig, OllamaModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        zeroclaw_config::schema::OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("primary-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "ollama.local",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.ollama.insert(
        "local".to_string(),
        OllamaModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("llama3.2".to_string()),
                ..Default::default()
            },
            ..Default::default()
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        Some("primary-key"),
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "local fallback providers may intentionally omit api_key: {}",
        result.err().unwrap()
    );
}

#[test]
fn resilient_alias_dangling_fallback_does_not_abort_build() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "primary".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "openai.ghost",
                )],
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "primary",
        None,
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "a dangling fallback ref must be skipped, never abort the build"
    );
}

#[test]
fn resilient_alias_cyclic_fallback_does_not_loop_or_abort() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    config.providers.models.openai.insert(
        "a".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4o".to_string()),
                api_key: Some("a-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "openai.b",
                )],
                ..Default::default()
            },
        },
    );
    config.providers.models.openai.insert(
        "b".to_string(),
        OpenAIModelProviderConfig {
            base: ModelProviderConfig {
                model: Some("gpt-4.1".to_string()),
                api_key: Some("b-key".to_string()),
                fallback: vec![zeroclaw_config::providers::ModelProviderRef::new(
                    "openai.a",
                )],
                ..Default::default()
            },
        },
    );

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "a",
        None,
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "a fallback cycle must be pruned, never loop or abort the build"
    );
}

#[test]
fn resilient_alias_deep_acyclic_fallback_does_not_overflow() {
    use zeroclaw_config::schema::{Config, ModelProviderConfig, OpenAIModelProviderConfig};

    let mut config = Config::default();
    let n = zeroclaw_config::providers::MAX_FALLBACK_DEPTH + 50;
    for i in 0..n {
        let fallback = if i + 1 < n {
            vec![zeroclaw_config::providers::ModelProviderRef::new(format!(
                "openai.a{}",
                i + 1
            ))]
        } else {
            vec![]
        };
        config.providers.models.openai.insert(
            format!("a{i}"),
            OpenAIModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("gpt-4o".to_string()),
                    api_key: Some(format!("a{i}-key")),
                    fallback,
                    ..Default::default()
                },
            },
        );
    }

    let result = create_resilient_model_provider_for_alias(
        &config,
        "openai",
        "a0",
        None,
        None,
        &zeroclaw_config::schema::ReliabilityConfig::default(),
        &ModelProviderRuntimeOptions::default(),
    );
    assert!(
        result.is_ok(),
        "a deep acyclic chain must be depth-capped, never overflow or abort the build"
    );
}
