//! TG1: ModelProvider End-to-End Resolution Tests

use zeroclaw::providers::create_model_provider_with_url;

/// Helper: assert model_provider creation succeeds
fn assert_provider_ok(name: &str, key: Option<&str>, url: Option<&str>) {
    let result = create_model_provider_with_url(name, key, url);
    assert!(
        result.is_ok(),
        "{name} model_provider should resolve: {}",
        result.err().map(|e| e.to_string()).unwrap_or_default()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Factory resolution: each major model_provider name resolves without error
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Factory resolution: alias variants map to same model_provider
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_grok_alias_resolves_to_xai() {
    assert_provider_ok("grok", Some("test-key"), None);
}

#[test]
fn factory_kimi_alias_resolves_to_moonshot() {
    assert_provider_ok("kimi", Some("test-key"), None);
}

// ─────────────────────────────────────────────────────────────────────────────
// Custom URL model_provider creation
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_custom_https_url_resolves() {
    assert_provider_ok("custom:https://api.example.com/v1", Some("test-key"), None);
}

#[test]
fn factory_custom_ftp_url_rejected() {
    let result = create_model_provider_with_url("custom:ftp://example.com", None, None);
    assert!(result.is_err(), "ftp scheme should be rejected");
    let err_msg = result.err().unwrap().to_string();
    assert!(
        err_msg.contains("http://") || err_msg.contains("https://"),
        "error should mention valid schemes: {err_msg}"
    );
}

#[test]
fn factory_custom_empty_url_rejected() {
    let result = create_model_provider_with_url("custom:", None, None);
    assert!(result.is_err(), "empty custom URL should be rejected");
}

// ─────────────────────────────────────────────────────────────────────────────
// OpenAiCompatibleModelProvider: credential and auth style wiring
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// ModelProvider with api_url override (simulates- Ollama api_url config)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_openai_with_custom_api_url() {
    assert_provider_ok(
        "openai",
        Some("test-key"),
        Some("https://custom-openai-proxy.example.com/v1"),
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// ModelProvider default convenience factory
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Primary model_providers with custom implementations
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// OpenAI-compatible ecosystem model_providers
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_resolves_opencode_go_provider() {
    assert_provider_ok("opencode-go", Some("test-key"), None);
}

// ─────────────────────────────────────────────────────────────────────────────
// China region model_providers
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Local/self-hosted model_providers
// ─────────────────────────────────────────────────────────────────────────────

// ─────────────────────────────────────────────────────────────────────────────
// Cloud AI endpoints
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_resolves_ovhcloud_provider() {
    assert_provider_ok("ovhcloud", Some("test-key"), None);
}

// ─────────────────────────────────────────────────────────────────────────────
// Alias resolution tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_google_alias_resolves_to_gemini() {
    assert_provider_ok("google", Some("test-key"), None);
}

#[test]
fn factory_google_gemini_alias_resolves_to_gemini() {
    assert_provider_ok("google-gemini", Some("test-key"), None);
}

#[test]
fn factory_aws_bedrock_alias_resolves_to_bedrock() {
    assert_provider_ok("aws-bedrock", None, None);
}

#[test]
fn factory_github_copilot_alias_resolves_to_copilot() {
    assert_provider_ok("github-copilot", Some("test-key"), None);
}

#[test]
fn factory_vercel_ai_alias_resolves_to_vercel() {
    assert_provider_ok("vercel-ai", Some("test-key"), None);
}

#[test]
fn factory_cloudflare_ai_alias_resolves_to_cloudflare() {
    assert_provider_ok("cloudflare-ai", Some("test-key"), None);
}

#[test]
fn factory_opencode_zen_alias_resolves_to_opencode() {
    assert_provider_ok("opencode-zen", Some("test-key"), None);
}

#[test]
fn factory_lm_studio_alias_resolves_to_lmstudio() {
    assert_provider_ok("lm-studio", None, None);
}

#[test]
fn factory_llama_cpp_alias_resolves_to_llamacpp() {
    assert_provider_ok("llama.cpp", None, None);
}

#[test]
fn factory_nvidia_nim_alias_resolves_to_nvidia() {
    assert_provider_ok("nvidia-nim", Some("test-key"), None);
}

#[test]
fn factory_build_nvidia_com_alias_resolves_to_nvidia() {
    assert_provider_ok("build.nvidia.com", Some("test-key"), None);
}

// ─────────────────────────────────────────────────────────────────────────────
// Custom endpoint tests
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn factory_anthropic_custom_endpoint_resolves() {
    assert_provider_ok(
        "anthropic-custom:https://api.example.com",
        Some("test-key"),
        None,
    );
}
