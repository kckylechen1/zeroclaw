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
