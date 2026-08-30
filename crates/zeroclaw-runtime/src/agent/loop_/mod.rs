/// Format token count with thousands separators.
pub(crate) fn format_tokens(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

// Test suites under `loop_/tests.rs` pull these through `use super::*`.
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use crate::agent::TurnMeta;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use crate::approval::ApprovalManager;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use crate::observability::{self as observability, Observer, ObserverEvent};
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use crate::tools;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use crate::tools::Tool;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use std::collections::HashSet;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use tokio_util::sync::CancellationToken;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use zeroclaw_api::ingress::{IngressContext, TurnOrigin};
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use zeroclaw_config::schema::Config;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use zeroclaw_providers::{ChatRequest, ModelProvider};

mod agent_turn;
mod process_message;
mod run;
mod run_overrides;

// Cost tracking moved to `super::cost`.
pub use super::cost::{
    TOOL_LOOP_COST_TRACKING_CONTEXT, ToolLoopCostTrackingContext, TurnUsage,
    check_tool_loop_budget, record_tool_loop_cost_usage,
};

// History management moved to `super::history`.
pub use super::history::{
    append_or_merge_system_message, canonicalize_tool_result_media_markers,
    estimate_history_tokens, load_interactive_session_history, normalize_system_messages,
    save_interactive_session_history, trim_history, truncate_tool_result,
};

// Tool / MCP filter admission moved to `super::tool_filter`.
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use super::tool_filter::glob_match;
pub use super::tool_filter::{
    append_pinned_mcp_section, apply_policy_tool_filter, eager_mcp_tool_allowed,
    filter_by_allowed_tools, filter_tool_specs_for_turn, mcp_tool_access_policy,
    register_eager_mcp_tool_if_allowed,
};
pub(crate) use super::tool_filter::{
    compute_excluded_mcp_tools, mcp_allowed_tool_count, preactivate_always_filter_groups,
};

// Text-protocol tool prompt helpers moved to `super::text_tool_prompt`.
pub(crate) use super::text_tool_prompt::retain_registered_tool_descriptions;

// Bounded interactive line IO moved to `super::capped_line`.
pub(crate) use super::capped_line::{CappedLine, MAX_INTERACTIVE_INPUT_BYTES, read_capped_line};

// Channel / peripheral factories moved to `super::channel_factories`.
pub use super::channel_factories::{
    CLI_CHANNEL_FN, PeripheralToolsFn, load_peripheral_tools, register_channel_map_fn,
    register_cli_channel_fn, register_peripheral_tools_fn,
};
pub(crate) use super::channel_factories::{live_channel_registry, seed_channel_handles};

// Prompt / export helpers moved to `super::prompt_helpers`.
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use super::prompt_helpers::tools_to_openai_format;
pub(crate) use super::prompt_helpers::{
    autosave_memory_key, build_hardware_context, build_system_prompt_for_turn, capture_llm_messages,
};
pub use super::prompt_helpers::{make_query_summary, native_tool_specs_present_for_turn};

pub use super::text_tool_prompt::{
    apply_text_tool_prompt_policy, build_tool_instructions, build_tool_instructions_for_names,
};

/// Minimum user-message length (in chars) for auto-save to memory.
/// Matches the channel-side constant in `channels/mod.rs`.
pub(crate) const AUTOSAVE_MIN_MESSAGE_CHARS: usize = 20;

// Session-key scoping lives in `super::announce_claim`; the background-child
// announcement claim machinery that shared the module retired with the durable
// control plane (migration wall 4).
pub use super::announce_claim::{
    TOOL_LOOP_SESSION_KEY, TOOL_LOOP_THREAD_ID, scope_session_key, scope_thread_id,
};
pub(crate) use super::announce_claim::{current_session_key, synthetic_session_key_for_run};

// Re-export tool call parsing from the standalone parser crate.
pub use zeroclaw_tool_call_parser::{
    ParsedToolCall, ToolProtocolEnvelopeKind, build_native_assistant_history_from_parsed_calls,
    canonicalize_json_for_tool_signature, classify_tool_protocol_envelope,
    contains_tool_protocol_tag_call, detect_tool_call_parse_issue,
    looks_like_malformed_tool_protocol_envelope,
    looks_like_malformed_tool_protocol_envelope_for_known_tools, looks_like_tool_protocol_envelope,
    looks_like_tool_protocol_example, parse_tool_calls, strip_think_tags, strip_tool_result_blocks,
    tool_protocol_envelope_mentions_known_tool,
};

pub use zeroclaw_api::TOOL_CHOICE_OVERRIDE;

// Tool execution moved to `super::tool_execution`.
pub use super::tool_execution::{ToolExecutionOutcome, should_execute_tools_in_parallel};

// agent_turn entry moved to `agent_turn`.
pub use self::agent_turn::agent_turn;

// Run overrides / resolve helpers moved to `run_overrides`.
pub use self::process_message::process_message;
pub use self::run::run;
pub use self::run_overrides::AgentRunOverrides;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use self::run_overrides::RESOLVED_AGENT_FOR_TURN_TEST_HOOK;
pub(crate) use self::run_overrides::{
    agent_provider_composite, api_key_and_uri_for_provider, resolved_agent_for_turn,
};

// ── Agent Tool-Call Loop ──────────────────────────────────────────────────
// The turn engine lives in `super::turn` — `run_tool_call_loop` plus one
// file per step (run sheet in agent/turn/mod.rs). `crate::agent::loop_`
// stays the canonical public path via these re-exports.
pub(crate) use super::turn::StreamCancelledAfterOutput;
#[cfg(all(test, feature = "heavy-tests"))]
pub(crate) use super::turn::{
    DEFAULT_MAX_TOOL_ITERATIONS, MAX_MALFORMED_TOOL_PROTOCOL_RETRIES,
    build_native_assistant_history, consume_provider_streaming_response,
    maybe_inject_channel_delivery_defaults, resolve_display_text,
};
pub use super::turn::{
    DraftEvent, LoopKnobs, MaxIterationBehavior, ModelSwitchCallback, ModelSwitchRequested,
    PROGRESS_MIN_INTERVAL_MS, ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess,
    ResolvedRuntimeKnobs, StreamDelta, ToolLoop, ToolLoopCancelled, drain_steering_messages,
    is_model_switch_requested, is_tool_loop_cancelled, run_tool_call_loop, scrub_credentials,
};

// Heavy suite gated so lib-test iteration does not pay 13.9k lines; CI runtime leg enables it.
#[cfg(all(test, feature = "heavy-tests"))]
mod tests;
