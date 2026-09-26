//! Channel-side outbound helpers. The sanitization and leak-redaction passes
//! live in [`zeroclaw_runtime::security::outbound`], shared with the gateway
//! stream and the bridge outbox; this module re-exports them for the
//! orchestrator and keeps the channel-only empty-reply fallback.

#[cfg(feature = "channel-telegram")]
pub(crate) use zeroclaw_runtime::security::outbound::strip_tool_call_tags;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use zeroclaw_runtime::security::outbound::{
    OutboundContentFormat, channel_outbound_protected_spans, strip_isolated_tool_json_artifacts,
    strip_think_tags_inline,
};
pub(crate) use zeroclaw_runtime::security::outbound::{
    outbound_content_format_for_channel, redact_channel_outbound_leaks,
    sanitize_channel_response_for_format_with_leak_detection, sanitize_streaming_draft_text,
    strip_tool_result_content, strip_tool_summary_prefix,
};

#[cfg(all(test, feature = "heavy-tests"))]
use zeroclaw_runtime::tools::Tool;

#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) fn sanitize_channel_response(response: &str, tools: &[Box<dyn Tool>]) -> String {
    sanitize_channel_response_with_leak_detection(
        response,
        tools,
        &zeroclaw_config::schema::LeakDetectionConfig::default(),
    )
}

#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) fn sanitize_channel_response_with_leak_detection(
    response: &str,
    tools: &[Box<dyn Tool>],
    leak_detection: &zeroclaw_config::schema::LeakDetectionConfig,
) -> String {
    sanitize_channel_response_for_format_with_leak_detection(
        response,
        tools,
        leak_detection,
        OutboundContentFormat::Markdown,
    )
}

/// Shown when the agent turn completes but no visible text remains after sanitization.
pub(crate) const EMPTY_CHANNEL_REPLY_FALLBACK: &str =
    "I couldn't produce a visible reply for that message. Please try again.";

/// Ensure channel outbound text is never empty so users don't see typing with no message.
pub(crate) fn ensure_nonempty_channel_reply(
    delivered_response: String,
    outbound_response: &str,
    channel: &str,
    reply_target: &str,
) -> String {
    if !delivered_response.trim().is_empty() {
        return delivered_response;
    }
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "channel": channel,
                "reply_target": reply_target,
                "outbound_len": outbound_response.len(),
            })),
        "channel_reply_empty; substituting fallback"
    );
    EMPTY_CHANNEL_REPLY_FALLBACK.to_string()
}
