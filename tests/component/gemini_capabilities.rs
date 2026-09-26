//! Gemini model_provider capabilities and contract tests.

use zeroclaw::providers::create_model_provider_with_url;
use zeroclaw::providers::traits::ModelProvider;

fn gemini_model_provider() -> Box<dyn ModelProvider> {
    create_model_provider_with_url("gemini", Some("test-key"), None)
        .expect("Gemini model_provider should resolve with test key")
}

#[test]
fn gemini_capabilities_are_prompt_guided_with_vision() {
    let model_provider = gemini_model_provider();
    let caps = model_provider.capabilities();
    assert!(
        !caps.native_tool_calling && !model_provider.supports_native_tools(),
        "Gemini should use prompt-guided tool calling, not native"
    );
    assert!(
        caps.vision && model_provider.supports_vision(),
        "Gemini should report vision support"
    );
}

#[test]
fn gemini_convert_tools_returns_prompt_guided() {
    use zeroclaw::providers::traits::ToolsPayload;
    use zeroclaw::tools::ToolSpec;

    let model_provider = gemini_model_provider();
    let tools = vec![ToolSpec::new(
        "memory_store".to_string(),
        "Store a value in memory".to_string(),
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "value": {"type": "string"}
            },
            "required": ["key", "value"]
        }),
    )];

    let payload = model_provider.convert_tools(&tools);
    assert!(
        matches!(payload, ToolsPayload::PromptGuided { .. }),
        "Gemini should return PromptGuided payload since native_tool_calling is false"
    );
}
