//! TG7: ModelProvider Schema Conformance Tests

use zeroclaw::providers::traits::{ChatMessage, ChatResponse, ToolCall};

// ─────────────────────────────────────────────────────────────────────────────
// ChatMessage serialization
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn chat_message_constructors_set_role_and_content() {
    for (msg, role) in [
        (ChatMessage::system("body"), "system"),
        (ChatMessage::user("body"), "user"),
        (ChatMessage::assistant("body"), "assistant"),
        (ChatMessage::tool("body"), "tool"),
    ] {
        assert_eq!(msg.role, role);
        assert_eq!(msg.content, "body");
    }
}

#[test]
fn chat_message_json_has_role_and_content_and_roundtrips() {
    let msg = ChatMessage::user("test message");
    let json = serde_json::to_value(&msg).unwrap();
    assert_eq!(json["role"], "user");
    assert_eq!(json["content"], "test message");

    let parsed: ChatMessage = serde_json::from_value(json).unwrap();
    assert_eq!(parsed.role, msg.role);
    assert_eq!(parsed.content, msg.content);
}

// ─────────────────────────────────────────────────────────────────────────────
// ToolCall serialization (tool_call_id field)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn tool_call_json_has_required_fields_and_preserves_id() {
    let tc = ToolCall {
        id: "call_deepseek_42".into(),
        name: "shell".into(),
        arguments: r#"{"command": "ls"}"#.into(),
        extra_content: None,
    };

    let json = serde_json::to_value(&tc).unwrap();
    for field in ["id", "name", "arguments"] {
        assert!(json.get(field).is_some(), "ToolCall must have '{field}'");
    }

    let parsed: ToolCall = serde_json::from_value(json).unwrap();
    assert_eq!(
        parsed.id, "call_deepseek_42",
        "tool_call_id must survive roundtrip"
    );
    assert_eq!(parsed.name, "shell");
}

// ─────────────────────────────────────────────────────────────────────────────
// ChatResponse structure
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn chat_response_text_accessors() {
    let text_only = ChatResponse {
        text: Some("Hello world".into()),
        tool_calls: vec![],
        usage: None,
        reasoning_content: None,
    };
    assert_eq!(text_only.text_or_empty(), "Hello world");
    assert!(!text_only.has_tool_calls());

    let no_text = ChatResponse {
        text: None,
        ..text_only
    };
    assert_eq!(no_text.text_or_empty(), "");
}

#[test]
fn chat_response_multiple_tool_calls() {
    let resp = ChatResponse {
        text: None,
        tool_calls: vec![
            ToolCall {
                id: "tc_1".into(),
                name: "shell".into(),
                arguments: r#"{"command": "ls"}"#.into(),
                extra_content: None,
            },
            ToolCall {
                id: "tc_2".into(),
                name: "file_read".into(),
                arguments: r#"{"path": "test.txt"}"#.into(),
                extra_content: None,
            },
        ],
        usage: None,
        reasoning_content: None,
    };

    assert!(resp.has_tool_calls());
    assert_eq!(resp.tool_calls.len(), 2);
    assert_eq!(resp.tool_calls[0].name, "shell");
    // Each tool call should have a distinct id
    assert_ne!(resp.tool_calls[0].id, resp.tool_calls[1].id);
}
