//! Thin `agent_turn` entry that brackets a [`ToolLoop`] with AgentStart/AgentEnd.
//!
//! Extracted from `loop_/mod.rs` so channel/gateway turn dispatch is not
//! interleaved with the interactive CLI `run` assembly.

use crate::approval::ApprovalManager;
use crate::observability::Observer;
use crate::tools::Tool;
use anyhow::Result;
use zeroclaw_api::channel::Channel;
use zeroclaw_api::ingress::{IngressContext, TurnOrigin};
use zeroclaw_providers::{ChatMessage, ModelProvider};

use crate::agent::cost::TOOL_LOOP_COST_TRACKING_CONTEXT;
use crate::agent::turn::{
    LoopKnobs, ModelSwitchCallback, ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess,
    ResolvedRuntimeKnobs, ToolLoop, run_tool_call_loop,
};

/// Execute a single turn of the agent loop: send messages, parse tool calls,
/// execute tools, and loop until the LLM produces a final text response.
/// When `silent` is true, suppresses stdout (for channel use).
/// `agent_alias`, when the caller has resolved one, is threaded onto the
/// turn's `AgentStart`/`AgentEnd` brackets and onto the inner `ToolLoop`, so
/// every lifecycle observer event of the turn (agent_start, llm_request,
/// llm_response, tool_call_start, tool_call, agent_end) carries the full
/// `(channel, agent_alias, turn_id)` correlation triple that observer
/// consumers (Prometheus, OTel, the gateway `/api/events` stream) rely on for
/// per-agent attribution. `None` opts out for callers without a resolved
/// alias (tests, benches). `turn_id` follows the same pattern: `Some` reuses
/// a caller-minted id so pre-turn events (the `process_message` RAG
/// retrieval) join the bracket; `None` self-mints.
#[allow(clippy::too_many_arguments)]
pub async fn agent_turn(
    config: Option<&zeroclaw_config::schema::Config>,
    model_provider: &dyn ModelProvider,
    history: &mut Vec<ChatMessage>,
    tools_registry: &[Box<dyn Tool>],
    observer: &dyn Observer,
    provider_name: &str,
    model: &str,
    temperature: Option<f64>,
    silent: bool,
    channel_name: &str,
    channel_reply_target: Option<&str>,
    multimodal_config: &zeroclaw_config::schema::MultimodalConfig,
    max_tool_iterations: usize,
    approval: Option<&ApprovalManager>,
    excluded_tools: &[String],
    dedup_exempt_tools: &[String],
    activated_tools: Option<&std::sync::Arc<std::sync::Mutex<crate::tools::ActivatedToolSet>>>,
    model_switch_callback: Option<ModelSwitchCallback>,
    strict_tool_parsing: bool,
    parallel_tools: bool,
    max_tool_result_chars: usize,
    context_token_budget: usize,
    channel: Option<&dyn Channel>,
    origin: TurnOrigin,
    memory: Option<crate::agent::memory_inject::TurnMemory<'_>>,
    agent_alias: Option<&str>,
    turn_id: Option<&str>,
) -> Result<String> {
    let turn_id = turn_id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string);
    // Bracket the turn with AgentStart/AgentEnd so entry points that dispatch
    // through `agent_turn` (gateway webhook chat via `process_message`, peer
    // messages) surface turn lifecycle events to observers — mirroring the
    // CLI `run` and `Agent::turn_streamed` entry points. The brackets carry
    // the caller's resolved alias, so they agree with the inner events on the
    // full (channel, agent_alias, turn_id) triple.
    let mut turn_guard = crate::observability::AgentTurnGuard::start(
        observer,
        provider_name,
        model,
        Some(channel_name.to_string()),
        agent_alias.map(str::to_string),
        Some(turn_id.clone()),
    );
    let result = run_tool_call_loop(ToolLoop {
        exec: ResolvedAgentExecution::resolve(
            ResolvedModelAccess {
                model_provider,
                provider_name,
                model,
                temperature,
            },
            ResolvedIo {
                tools_registry,
                observer,
                silent,
                approval,
                multimodal_config,
                config,
                hooks: None,
                activated_tools,
                model_switch_callback,
                receipt_generator: None,
            },
            ResolvedRuntimeKnobs {
                max_tool_iterations,
                excluded_tools,
                dedup_exempt_tools,
                pacing: &zeroclaw_config::schema::PacingConfig::default(),
                strict_tool_parsing,
                parallel_tools,
                max_tool_result_chars,
                context_token_budget,
                knobs: &LoopKnobs::default(),
            },
        ),
        history,
        channel_name,
        channel_reply_target,
        cancellation_token: None,
        on_delta: None,
        shared_budget: None, // no shared budget for agent_turn callers
        channel,
        collected_receipts: None,
        event_tx: None,
        steering: None,
        new_messages_out: None,
        image_cache: None,
        // Origin and the per-turn memory half are threaded from the entry
        // point; source/transport/trust stay phase-1 placeholders until
        // per-transport stamping lands.
        memory,
        ingress: IngressContext::from_origin(origin),
        agent_alias,
        parent_agent_alias: None,
        turn_id: &turn_id,
    })
    .await;
    // Snapshot token usage from the task-local cost context when the caller
    // scoped one around this call (the gateway scopes both
    // `TOOL_LOOP_TURN_USAGE` and `TOOL_LOOP_COST_TRACKING_CONTEXT` around
    // `process_message`); unscoped callers report `None`. When this runs
    // nested inside a parent turn's scoped context (peer-message-as-tool),
    // `snapshot_turn_usage` prefers the caller-scoped task-local and may
    // report the parent turn's cumulative usage — pre-existing cost
    // attribution semantics, kept as-is.
    let tokens_used = TOOL_LOOP_COST_TRACKING_CONTEXT
        .try_with(std::clone::Clone::clone)
        .ok()
        .flatten()
        .and_then(|ctx| {
            let usage = ctx.snapshot_turn_usage();
            (usage.input_tokens > 0 || usage.output_tokens > 0).then_some(
                zeroclaw_api::observability_traits::TurnTokenUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                },
            )
        });
    turn_guard.set_usage(tokens_used, None);
    turn_guard.finish();
    result
}
