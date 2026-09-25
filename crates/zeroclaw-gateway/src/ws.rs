//! WebSocket agent chat handler.
//!
//! Approval summaries are operator-facing strings produced by the runtime's
//! key-name redaction heuristic. Approval decisions bind to `request_id`; this
//! transport forwards the summary without rebuilding it from raw arguments.

use super::AppState;
use crate::ws_approval::{PendingApprovals, WsApprovalChannel};
use crate::ws_conversation::{Conversation, FrameSink, Seed, Submitted, TurnClaim};
use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, header},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zeroclaw_api::channel::ChannelApprovalResponse;

/// Default wall-clock budget for the operator to answer an
/// `approval_request` frame before the channel auto-denies. Mirrors the
/// channel-side default on `TelegramConfig::approval_timeout_secs`.
const WS_APPROVAL_TIMEOUT_SECS: u64 = 120;

/// Single ingress identity for gateway WebSocket turns.
///
/// This name is used in three places that MUST agree:
///   1. `Agent.channel_name` — observer/attribution events for the turn
///   2. the turn span's `channel` field — tracing/log correlation
///   3. the interactive back-channel registration key — how `ask_user`,
///      `poll`, and `escalate_to_human` find this conversation
///
/// If (3) diverges from (1) and (2), one turn is split across two channel
/// names in observability while interactive tools still route correctly —
/// or, worse, tools route to an arbitrary seeded channel.
const WS_CHANNEL_KEY: &str = "wss";

/// Capture at turn settlement, before the outcome frame is transmitted.
/// Delivery failure does not roll the receipt back: the turn already happened.
fn persist_companion_capture(
    state: &AppState,
    agent_alias: &str,
    session_id: &str,
    turn_id: &str,
    auth_subject: Option<&str>,
) {
    let Some(store) = state.companion_store.as_ref() else {
        return;
    };
    let owner = state.config.read().companion_memory.owner.gate();
    let identity = match auth_subject.map(str::trim).filter(|s| !s.is_empty()) {
        Some(subject) => format!("{WS_CHANNEL_KEY}:{subject}"),
        None => WS_CHANNEL_KEY.to_string(),
    };
    let _ = zeroclaw_memory::capture_gateway_turn(
        Some(store.as_ref()),
        agent_alias,
        session_id,
        turn_id,
        &identity,
        &owner,
    );
}

#[derive(Debug, Deserialize)]
struct ConnectParams {
    #[serde(rename = "type")]
    msg_type: String,
    /// Client-chosen session ID for memory persistence
    #[serde(default)]
    session_id: Option<String>,
    /// Device name for device registry tracking
    #[serde(default)]
    device_name: Option<String>,
    /// Client capabilities
    #[serde(default)]
    capabilities: Vec<String>,
    /// Project root / working directory for this session.
    #[serde(default, alias = "workspaceDir", alias = "workspace_dir")]
    cwd: Option<String>,
}

/// The sub-protocol we support for the chat WebSocket.
const WS_PROTOCOL: &str = "zeroclaw.v1";

/// Prefix used in `Sec-WebSocket-Protocol` to carry a bearer token.
const BEARER_SUBPROTO_PREFIX: &str = "bearer.";

#[derive(Deserialize)]
pub struct WsQuery {
    pub token: Option<String>,
    pub session_id: Option<String>,
    /// Optional human-readable name for the session.
    pub name: Option<String>,
    /// Configured agent alias to run as. Required — every WebSocket
    /// session is bound to an explicit agent (no default agent exists).
    #[serde(default, alias = "agentAlias", alias = "agent")]
    pub agent_alias: Option<String>,
    /// Project root / working directory for this session.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default, alias = "workspaceDir", alias = "workspace_dir")]
    pub workspace_dir: Option<String>,
}

fn extract_ws_token<'a>(headers: &'a HeaderMap, query_token: Option<&'a str>) -> Option<&'a str> {
    // 1. Authorization header
    if let Some(t) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
        && !t.is_empty()
    {
        return Some(t);
    }

    // 2. Sec-WebSocket-Protocol: bearer.<token>
    if let Some(t) = headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .and_then(|protos| {
            protos
                .split(',')
                .map(|p| p.trim())
                .find_map(|p| p.strip_prefix(BEARER_SUBPROTO_PREFIX))
        })
        && !t.is_empty()
    {
        return Some(t);
    }

    // 3. ?token= query parameter
    if let Some(t) = query_token
        && !t.is_empty()
    {
        return Some(t);
    }

    None
}

fn client_offers_chat_protocol(headers: &HeaderMap) -> bool {
    headers
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|protos| protos.split(',').any(|p| p.trim() == WS_PROTOCOL))
}

/// GET /ws/chat — WebSocket upgrade for agent chat
pub async fn handle_ws_chat(
    State(state): State<AppState>,
    Query(params): Query<WsQuery>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Auth: check header, subprotocol, then query param (precedence order). On
    // success derive a STABLE transport-authenticated subject (the paired-token
    // hash) so a required-group approval policy can be satisfied over WS; an
    // operator grants approval rights to this paired device via a `ws:<token-hash>`
    // group member. `None` when pairing is not required (no auth identity).
    let auth_subject = if state.pairing.require_pairing() {
        let token = extract_ws_token(&headers, params.token.as_deref()).unwrap_or("");
        match state.pairing.authenticate_and_hash(token) {
            Some(hash) => Some(hash),
            None => {
                return (
                    axum::http::StatusCode::UNAUTHORIZED,
                    "Unauthorized: provide Authorization header, Sec-WebSocket-Protocol bearer, or ?token= query param",
                )
                    .into_response();
            }
        }
    } else {
        None
    };

    // Echo Sec-WebSocket-Protocol if the client requests our sub-protocol.
    // Absence is allowed: `/ws/chat` is not fail-closed on a missing token.
    // `/ws/nodes` v2 is the opposite — it rejects a missing or v1-only offer.
    let ws = if client_offers_chat_protocol(&headers) {
        ws.protocols([WS_PROTOCOL])
    } else {
        ws
    };

    // Reject the upgrade up-front when the client didn't pick an agent.
    // No default — every WS session is bound to an explicit agent.
    let Some(agent_alias) = params.agent_alias.filter(|s| !s.trim().is_empty()) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "Missing required `agent` query parameter — pass `?agent=<alias>` matching a configured [agents.<alias>] entry.",
        )
            .into_response();
    };
    {
        let cfg = state.config.read();
        if cfg.agent(&agent_alias).is_none() {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                format!(
                    "Unknown agent `{agent_alias}` — no [agents.{agent_alias}] entry configured."
                ),
            )
                .into_response();
        }
    }

    let session_id = params.session_id;
    let session_name = params.name;
    let session_cwd = params.cwd.or(params.workspace_dir);
    ws.on_upgrade(move |socket| {
        handle_socket(
            socket,
            state,
            agent_alias,
            session_id,
            session_name,
            session_cwd,
            auth_subject,
        )
    })
    .into_response()
}

/// Gateway session key prefix to avoid collisions with channel sessions.
const GW_SESSION_PREFIX: &str = "gw_";

async fn resolve_ws_memory_handle(
    config: &zeroclaw_config::schema::Config,
    agent_alias: &str,
) -> anyhow::Result<Option<Arc<dyn zeroclaw_memory::Memory>>> {
    if config.agent(agent_alias).is_some_and(|agent| {
        matches!(
            agent.memory.backend,
            zeroclaw_config::multi_agent::MemoryBackendKind::None
        )
    }) {
        return Ok(None);
    }

    let api_key = config
        .resolved_model_provider_for_agent(agent_alias)
        .and_then(|(_, _, cfg)| cfg.api_key.clone());
    zeroclaw_memory::create_memory_for_agent(config, agent_alias, api_key.as_deref())
        .await
        .map(Some)
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    agent_alias: String,
    session_id: Option<String>,
    session_name: Option<String>,
    session_cwd: Option<String>,
    // The transport-authenticated approval subject (paired-token hash), if the
    // connection was authenticated. Threaded to SOP approval frames so a policied
    // gate can be satisfied by an identified WS caller.
    auth_subject: Option<String>,
) {
    let (mut sender, mut receiver) = socket.split();

    // Resolve session ID: use provided or generate a new UUID
    let session_id = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_key = format!("{GW_SESSION_PREFIX}{session_id}");
    // Match the sanitized form persisted by memory backend migrations.
    let mut memory_session_id = zeroclaw_api::session_keys::sanitize_session_key(&session_id);

    // Hydrate session metadata from persistence (if available). Agent
    // construction is deferred until after the optional `connect` frame so the
    // client can provide a per-session cwd for the security sandbox root.
    let config = state.config.read().clone();
    let mut resumed = false;
    let mut message_count: usize = 0;
    let mut effective_name: Option<String> = None;
    let mut stored_messages = Vec::new();
    if let Some(ref backend) = state.session_backend {
        let messages = backend.load(&session_key);
        if !messages.is_empty() {
            message_count = messages.len();
            stored_messages = messages;
            resumed = true;
        }
        effective_name = stamp_session(
            backend.as_ref(),
            &session_key,
            &agent_alias,
            session_name.as_deref(),
        );
    }

    // Send session_start message to client
    let mut session_start = serde_json::json!({
        "type": "session_start",
        "session_id": session_id,
        "resumed": resumed,
        "message_count": message_count,
    });
    if let Some(ref name) = effective_name {
        session_start["name"] = serde_json::Value::String(name.clone());
    }
    let _ = sender
        .send(Message::Text(session_start.to_string().into()))
        .await;

    let mut first_msg_fallback: Option<String> = None;
    let mut requested_cwd = session_cwd;

    if let Some(first) = receiver.next().await {
        match first {
            Ok(Message::Text(text)) => {
                if let Ok(cp) = serde_json::from_str::<ConnectParams>(&text) {
                    if cp.msg_type == "connect" {
                        ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"session_id": cp.session_id, "device_name": cp.device_name, "capabilities": cp.capabilities, "cwd": cp.cwd})), "WebSocket connect params received");
                        if let Some(sid) = &cp.session_id {
                            memory_session_id =
                                zeroclaw_api::session_keys::sanitize_session_key(sid);
                            ::zeroclaw_log::record!(
                                DEBUG,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_attrs(::serde_json::json!({"session_id": sid})),
                                "WebSocket connect session override received"
                            );
                        }
                        if cp.cwd.is_some() {
                            requested_cwd = cp.cwd;
                        }
                        let ack = serde_json::json!({
                            "type": "connected",
                            "message": "Connection established"
                        });
                        let _ = sender.send(Message::Text(ack.to_string().into())).await;
                    } else {
                        // Not a connect message — fall through to normal processing
                        first_msg_fallback = Some(text.to_string());
                    }
                } else {
                    // Not parseable as ConnectParams — fall through
                    first_msg_fallback = Some(text.to_string());
                }
            }
            Ok(Message::Close(_)) | Err(_) => return,
            _ => {}
        }
    }

    let session_cwd = match resolve_ws_session_cwd(requested_cwd.as_deref(), &config, &agent_alias)
    {
        Ok(cwd) => cwd,
        Err(e) => {
            let err = serde_json::json!({
                "type": "error",
                "message": e.to_string(),
                "code": "INVALID_CWD"
            });
            let _ = sender.send(Message::Text(err.to_string().into())).await;
            return;
        }
    };

    if let Some(err) = needs_onboarding_ws_error(&config) {
        let _ = sender.send(Message::Text(err.to_string().into())).await;
        return;
    }

    // Every socket that opens this session with this agent shares one
    // conversation: one agent, one history, one running turn (#376).
    let conversation_key = format!("{session_key}\u{1f}{agent_alias}");
    let mut restore_trim_event = None;
    let attached = state
        .ws_conversations
        .attach(&conversation_key, |seed| {
            build_ws_session(
                &config,
                &state,
                &agent_alias,
                &session_key,
                &session_cwd,
                memory_session_id,
                &stored_messages,
                seed,
                &mut restore_trim_event,
            )
        })
        .await;
    let (mut subscription, created) = match attached {
        Ok(attached) => attached,
        Err(e) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                "Agent initialization failed"
            );
            let err = serde_json::json!({
                "type": "error",
                "message": format!("Failed to initialise agent: {e}"),
                "code": "AGENT_INIT_FAILED"
            });
            let _ = sender.send(Message::Text(err.to_string().into())).await;
            let _ = sender
                .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: 1011,
                    reason: axum::extract::ws::Utf8Bytes::from_static(
                        "Agent initialization failed",
                    ),
                })))
                .await;
            return;
        }
    };

    // Restoring history trims it at most once, when the conversation is
    // built. Only the socket that built it is told; later sockets join a
    // conversation whose history is already live.
    if created
        && let Some(zeroclaw_api::agent::TurnEvent::HistoryTrimmed {
            dropped_messages,
            kept_turns,
            reason,
        }) = restore_trim_event
    {
        let frame = history_trimmed_ws_frame(dropped_messages, kept_turns, &reason);
        let _ = sender.send(Message::Text(frame.to_string().into())).await;
    }

    let scope = WsTurnScope {
        session_key,
        session_id,
        auth_subject,
    };

    // Subscribe to the shared broadcast channel so cron/heartbeat events
    // are forwarded to this WebSocket client, during turns as well.
    let mut broadcast_rx = state.event_tx.subscribe();
    let mut next_text = first_msg_fallback;

    loop {
        let text = match next_text.take() {
            Some(text) => text,
            None => tokio::select! {
                // ── Client message ────────────────────────────────────
                client_msg = receiver.next() => match client_msg {
                    Some(Ok(Message::Text(text))) => text.to_string(),
                    // Closing a socket only unsubscribes; a running turn
                    // keeps going for the other sockets and the history.
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    Some(Ok(_)) => continue,
                },

                // ── Conversation frame (turn output, approvals) ───────
                frame = subscription.frames.recv() => {
                    match frame {
                        Ok(frame) => {
                            if sender.send(Message::Text(frame.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                    .with_attrs(::serde_json::json!({
                                        "session_key": scope.session_key,
                                        "skipped": skipped,
                                    })),
                                "WS subscriber fell behind; frames skipped"
                            );
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                    continue;
                }

                // ── Broadcast event (cron/heartbeat results) ──────────
                event = broadcast_rx.recv() => {
                    if let Ok(event) = event
                        && event_matches_session(&event, &scope.session_id)
                        && !is_observability_telemetry(&event)
                    {
                        let _ = sender.send(Message::Text(event.to_string().into())).await;
                    }
                    continue;
                }
            },
        };

        // ── Voice duplex event dispatch (gated by feature flag + runtime config) ──
        #[cfg(feature = "gateway-voice-duplex")]
        {
            // Multi-instance shape: presence in the map = enabled.
            let duplex_enabled = !state.config.read().channels.voice_duplex.is_empty();
            if duplex_enabled {
                if let Some(voice_event) = crate::voice_duplex::try_parse_voice_event(&text) {
                    if let Some(error_frame) = crate::voice_duplex::handle_voice_event(voice_event)
                    {
                        let _ = sender
                            .send(Message::Text(error_frame.to_string().into()))
                            .await;
                    }
                    continue;
                }
            }
        }

        if let Some(reply) = handle_client_text(&state, &subscription.conversation, &scope, &text) {
            let _ = sender.send(Message::Text(reply.to_string().into())).await;
        }
    }
}

/// The agent state one shared WS conversation holds.
pub(crate) struct WsSession {
    agent: zeroclaw_runtime::agent::Agent,
    /// Per-agent memory for turn-end consolidation; `None` disables it.
    ws_memory: Option<Arc<dyn zeroclaw_memory::Memory>>,
}

/// Identifies the session a turn runs for. Cloned into each turn task.
#[derive(Clone)]
struct WsTurnScope {
    session_key: String,
    session_id: String,
    // The transport-authenticated approval subject (paired-token hash) of
    // the socket that opened the conversation, if it was authenticated.
    auth_subject: Option<String>,
}

/// Build the agent for a new shared conversation and wire its approval
/// prompts to the conversation's subscribers.
#[allow(clippy::too_many_arguments)]
async fn build_ws_session(
    config: &zeroclaw_config::schema::Config,
    state: &AppState,
    agent_alias: &str,
    session_key: &str,
    session_cwd: &Path,
    memory_session_id: String,
    stored_messages: &[zeroclaw_providers::ChatMessage],
    seed: Seed,
    restore_trim_event: &mut Option<zeroclaw_api::agent::TurnEvent>,
) -> anyhow::Result<WsSession> {
    let ws_memory = match resolve_ws_memory_handle(config, agent_alias).await {
        Ok(memory) => memory,
        Err(e) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "agent": agent_alias,
                        "error": format!("{e:#}"),
                    })),
                "WS per-agent memory resolution failed; consolidation disabled for session"
            );
            None
        }
    };

    let mut agent =
        zeroclaw_runtime::agent::Agent::from_live_config_with_session_cwd_and_mcp_backchannel(
            Arc::clone(&state.config),
            agent_alias,
            Some(session_cwd),
            true,
            false,
        )
        .await?;
    // Keep ONE ingress identity for the WebSocket turn: the turn span records
    // `channel = "wss"`, and observer events derive from `Agent.channel_name`,
    // so this must stay `wss` or a single turn is split across two names.
    // The back-channel is registered under the same `wss` key below, which is
    // what lets ask_user/poll/escalate_to_human default to this conversation.
    agent.set_channel_name(WS_CHANNEL_KEY.to_string());
    agent.set_memory_session_id(Some(memory_session_id));
    if !stored_messages.is_empty() {
        *restore_trim_event = agent.seed_history_with_event(stored_messages);
    }

    let Seed {
        pending_approvals,
        frames,
    } = seed;
    let (approval_event_tx, approval_event_rx) =
        tokio::sync::mpsc::channel::<zeroclaw_api::agent::TurnEvent>(8);
    let approval_channel = Arc::new(WsApprovalChannel::new(
        approval_event_tx,
        pending_approvals.clone(),
        Duration::from_secs(WS_APPROVAL_TIMEOUT_SECS),
    ));
    agent
        .channel_handles()
        .register_channel(WS_CHANNEL_KEY, approval_channel);
    // Ends when the agent, and with it the approval channel, is dropped.
    zeroclaw_spawn::spawn!(relay_approval_requests(
        approval_event_rx,
        frames,
        pending_approvals
    ));

    let ch = agent.channel_handles();
    let channel_names = zeroclaw_channels::orchestrator::register_channels_for_tools(
        config,
        &ch.ask_user,
        &ch.channel_room,
        &Some(ch.reaction.clone()),
        &ch.poll,
        &ch.escalate,
    );
    if !channel_names.is_empty() {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({"channels": channel_names, "session": session_key})
            ),
            "Seeded {} channel(s) into dashboard agent session",
        );
    }

    Ok(WsSession { agent, ws_memory })
}

/// Publish the approval channel's prompts to every subscriber. With no
/// socket attached nobody can answer, so the prompt resolves as unreachable
/// at once instead of holding the turn until its timeout.
async fn relay_approval_requests(
    mut events: tokio::sync::mpsc::Receiver<zeroclaw_api::agent::TurnEvent>,
    frames: FrameSink,
    pending_approvals: PendingApprovals,
) {
    while let Some(event) = events.recv().await {
        // Forward the runtime-produced summary without inspecting or
        // reconstructing it from the raw argument object.
        let zeroclaw_api::agent::TurnEvent::ApprovalRequest {
            request_id,
            tool_name,
            arguments_summary,
            timeout_secs,
        } = event
        else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"kind": format!("{:?}", event)})),
                "non-ApprovalRequest event leaked into approval channel"
            );
            continue;
        };
        if !frames.has_subscribers() {
            pending_approvals.lock().remove(&request_id);
            continue;
        }
        frames.publish(&approval_request_ws_frame(
            &request_id,
            &tool_name,
            &arguments_summary,
            timeout_secs,
        ));
    }
}

fn approval_request_ws_frame(
    request_id: &str,
    tool_name: &str,
    arguments_summary: &str,
    timeout_secs: u64,
) -> serde_json::Value {
    serde_json::json!({
        "type": "approval_request",
        "request_id": request_id,
        "tool": tool_name,
        "arguments_summary": arguments_summary,
        "timeout_secs": timeout_secs,
    })
}

/// Act on one client text frame. Returns an error frame for this socket
/// only; everything else reaches the client through the conversation.
fn handle_client_text(
    state: &AppState,
    conversation: &Arc<Conversation<WsSession>>,
    scope: &WsTurnScope,
    text: &str,
) -> Option<serde_json::Value> {
    let error = |message: String, code: &str| {
        Some(serde_json::json!({ "type": "error", "message": message, "code": code }))
    };
    let parsed: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return error(format!("Invalid JSON: {e}"), "INVALID_JSON"),
    };
    match parsed["type"].as_str().unwrap_or("") {
        // ── approval_response (operator answered a tool prompt) ──
        "approval_response" => {
            let request_id = parsed["request_id"].as_str().unwrap_or("");
            let decision = match parsed["decision"].as_str().unwrap_or("") {
                "approve" => ChannelApprovalResponse::Approve,
                "always" => ChannelApprovalResponse::AlwaysApprove,
                "deny" => ChannelApprovalResponse::Deny,
                _ => {
                    return error(
                        "approval_response requires request_id and decision in {approve,deny,always}".into(),
                        "INVALID_APPROVAL_RESPONSE",
                    );
                }
            };
            if request_id.is_empty() {
                return error(
                    "approval_response requires request_id and decision in {approve,deny,always}"
                        .into(),
                    "INVALID_APPROVAL_RESPONSE",
                );
            }
            // Any subscriber may answer; only the first answer counts.
            if !conversation.resolve_approval(request_id, decision) {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"request_id": request_id})),
                    "approval_response with no matching pending request"
                );
            }
            None
        }
        // ── cancel (stop the running turn for every subscriber) ──
        "cancel" => {
            if conversation.cancel_current() {
                None
            } else {
                error("No turn is running".into(), "NO_ACTIVE_TURN")
            }
        }
        "message" => {
            let content = parsed["content"].as_str().unwrap_or("").to_string();
            if content.is_empty() {
                return error("Message content cannot be empty".into(), "EMPTY_CONTENT");
            }
            match conversation.submit(content) {
                Submitted::Start(claim) => {
                    let turns = run_ws_turns(
                        state.clone(),
                        Arc::clone(conversation),
                        scope.clone(),
                        claim,
                    );
                    zeroclaw_spawn::spawn!(turns);
                    None
                }
                Submitted::Steered => None,
                Submitted::SteeringFull => error(
                    "Steering queue is full for the running turn".into(),
                    "STEERING_QUEUE_FULL",
                ),
                Submitted::SteeringClosed => error(
                    "Running turn is no longer accepting steering messages".into(),
                    "STEERING_CLOSED",
                ),
            }
        }
        other => error(
            format!(
                "Unsupported message type \"{other}\". Send {{\"type\":\"message\",\"content\":\"your text\"}}"
            ),
            "UNKNOWN_MESSAGE_TYPE",
        ),
    }
}

/// Run a claimed turn to completion, then any messages that arrived as
/// steering too late for it to read, as one follow-up turn.
async fn run_ws_turns(
    state: AppState,
    conversation: Arc<Conversation<WsSession>>,
    scope: WsTurnScope,
    claim: TurnClaim,
) {
    let mut next = Some(claim);
    while let Some(TurnClaim {
        input,
        generation,
        cancel,
        mut steering,
    }) = next.take()
    {
        let late = match state.session_queue.acquire(&scope.session_key).await {
            Ok(_session_guard) => {
                let mut session = conversation.agent.lock().await;
                process_chat_message(
                    &state,
                    &conversation,
                    &mut session,
                    &scope,
                    &input,
                    generation,
                    cancel,
                    &mut steering,
                )
                .await
            }
            Err(e) => {
                conversation.finish_turn(generation);
                conversation.publish(&serde_json::json!({
                    "type": "error",
                    "message": e.to_string(),
                    "code": session_queue_ws_error_code(&e)
                }));
                Vec::new()
            }
        };
        if !late.is_empty()
            && let Submitted::Start(claim) = conversation.submit(late.join("\n\n"))
        {
            next = Some(claim);
        }
    }
    state.ws_conversations.release_if_unused(&conversation);
}

/// Record the owning agent and optional name for a session on connect, and
/// return the name to report to the client (the requested one, else the
/// stored one).
///
/// The alias is written first because it upserts the metadata row; the name
/// write only updates an existing row, so a new session's name would
/// otherwise be dropped.
fn stamp_session(
    backend: &dyn zeroclaw_infra::session_backend::SessionBackend,
    session_key: &str,
    agent_alias: &str,
    requested_name: Option<&str>,
) -> Option<String> {
    let _ = backend.set_session_agent_alias(session_key, agent_alias);
    if let Some(name) = requested_name.filter(|name| !name.is_empty()) {
        let _ = backend.set_session_name(session_key, name);
        return Some(name.to_string());
    }
    backend.get_session_name(session_key).unwrap_or(None)
}

fn resolve_session_cwd(
    requested_cwd: Option<&str>,
    default_workspace: &Path,
) -> anyhow::Result<PathBuf> {
    let cwd = requested_cwd
        .map(PathBuf::from)
        .unwrap_or_else(|| default_workspace.to_path_buf());
    std::fs::canonicalize(&cwd).map_err(|e| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "cwd": cwd.display().to_string(),
                    "error": format!("{}", e),
                })),
            "ws session cwd rejected"
        );
        anyhow::Error::msg(format!(
            "cwd is not a usable directory ({}): {e}",
            cwd.display()
        ))
    })
}

fn resolve_ws_session_cwd(
    requested_cwd: Option<&str>,
    config: &zeroclaw_config::schema::Config,
    agent_alias: &str,
) -> anyhow::Result<PathBuf> {
    let agent_workspace = config.agent_workspace_dir(agent_alias);
    if requested_cwd.is_none() {
        std::fs::create_dir_all(&agent_workspace).map_err(|e| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "agent": agent_alias,
                        "cwd": agent_workspace.display().to_string(),
                        "error": format!("{}", e),
                    })),
                "ws agent workspace cwd rejected"
            );
            anyhow::Error::msg(format!(
                "cwd is not a usable directory ({}): {e}",
                agent_workspace.display()
            ))
        })?;
    }
    resolve_session_cwd(requested_cwd, &agent_workspace)
}

fn session_queue_ws_error_code(error: &crate::session_queue::SessionQueueError) -> &'static str {
    match error {
        crate::session_queue::SessionQueueError::QueueFull { .. } => "SESSION_QUEUE_FULL",
        crate::session_queue::SessionQueueError::Timeout { .. } => "SESSION_QUEUE_TIMEOUT",
    }
}

fn persist_conversation_messages(
    backend: &dyn zeroclaw_infra::session_backend::SessionBackend,
    session_key: &str,
    messages: &[zeroclaw_providers::ConversationMessage],
) {
    // if the user deleted the session between the turn starting and
    // the post-turn persistence, don't resurrect it. The `aborted` / `done`
    // / `error` frames are still sent to the client; we just refuse to
    // re-create the row that `DELETE /api/sessions/{id}` just wiped.
    if !backend.session_exists(session_key) {
        return;
    }
    for message in messages {
        let zeroclaw_providers::ConversationMessage::Chat(message) = message else {
            continue;
        };
        if message.role == "system" {
            continue;
        }
        let _ = backend.append(session_key, message);
    }
}

fn has_assistant_chat_message(messages: &[zeroclaw_providers::ConversationMessage]) -> bool {
    messages.iter().any(|message| {
        matches!(
            message,
            zeroclaw_providers::ConversationMessage::Chat(message)
                if message.role == "assistant"
        )
    })
}

fn history_trimmed_ws_frame(
    dropped_messages: usize,
    kept_turns: usize,
    reason: &str,
) -> serde_json::Value {
    serde_json::json!({
        "type": "history_trimmed",
        "dropped_messages": dropped_messages,
        "kept_turns": kept_turns,
        "reason": reason,
    })
}

fn needs_onboarding_ws_error(
    config: &zeroclaw_config::schema::Config,
) -> Option<serde_json::Value> {
    let model = config.resolve_default_model().unwrap_or_default();
    crate::needs_quickstart_for(&model)?;
    Some(serde_json::json!({
        "type": "error",
        "error": "needs_onboarding",
        "code": "NEEDS_ONBOARDING",
        "message": crate::needs_quickstart_channel_reply(),
        "url": "/onboard",
    }))
}

fn event_matches_session(event: &serde_json::Value, session_id: &str) -> bool {
    match event.get("session_id").and_then(|value| value.as_str()) {
        Some(event_session_id) => event_session_id == session_id,
        None => is_global_chat_event(event),
    }
}

fn is_global_chat_event(event: &serde_json::Value) -> bool {
    matches!(
        event.get("type").and_then(serde_json::Value::as_str),
        Some("cron_result")
    )
}

fn is_observability_telemetry(event: &serde_json::Value) -> bool {
    event.get("source").and_then(serde_json::Value::as_str) == Some("observability")
}

/// Provider, model, and temperature for turn-end memory consolidation,
/// built from the agent's live `<family>.<alias>` reference and model as
/// they stand after the turn. A mid-session model switch changes the agent's
/// provider and model together, so consolidation follows that pair instead of
/// the gateway's boot default: the turn's content only goes to the provider
/// that already saw it. An empty model falls back to the entry's configured
/// model. `None` when the reference no longer resolves (for example after a
/// config reload removed the entry); consolidation is then skipped, never
/// routed to another provider.
fn ws_consolidation_model(
    config: &zeroclaw_config::schema::Config,
    provider_ref: &str,
    model: &str,
) -> Option<(
    Box<dyn zeroclaw_api::model_provider::ModelProvider>,
    String,
    Option<f64>,
)> {
    let (family, alias) = provider_ref.split_once('.')?;
    let temperature = config.providers.models.find(family, alias)?.temperature;
    let (provider, _, model) = zeroclaw_runtime::agent::agent::build_session_model_provider(
        config,
        provider_ref,
        Some(model),
    )
    .ok()?;
    Some((provider, model, temperature))
}

/// Run one chat turn through the conversation's agent and publish its
/// frames to every subscriber. Uses [`Agent::turn_streamed`] so that
/// intermediate text chunks, tool calls, and tool results reach the clients
/// in real time. Returns steering messages that arrived after the agent
/// stopped reading them; the caller runs those as the next turn.
#[allow(clippy::too_many_arguments)]
async fn process_chat_message(
    state: &AppState,
    conversation: &Conversation<WsSession>,
    session: &mut WsSession,
    scope: &WsTurnScope,
    content: &str,
    generation: u64,
    cancel_token: tokio_util::sync::CancellationToken,
    steering_rx: &mut tokio::sync::mpsc::Receiver<String>,
) -> Vec<String> {
    use zeroclaw_runtime::agent::TurnEvent;

    let WsSession { agent, ws_memory } = session;
    let session_key = scope.session_key.as_str();
    let session_id = scope.session_id.as_str();
    let auth_subject = scope.auth_subject.as_deref();

    let (turn_alias, turn_provider, turn_model) = agent.attribution_fields();
    let provider_label = turn_provider.clone();
    let cost_tracking_context = state.cost_tracker.as_ref().map(|tracker| {
        let config = state.config.read();
        let pricing = zeroclaw_runtime::agent::cost::build_model_provider_pricing(&config);
        zeroclaw_runtime::agent::cost::ToolLoopCostTrackingContext::new(
            tracker.clone(),
            Arc::new(pricing),
        )
        .with_agent_alias(&turn_alias)
    });
    let turn_usage = state.cost_tracker.as_ref().map(|_| {
        Arc::new(parking_lot::Mutex::new(
            zeroclaw_runtime::agent::cost::TurnUsage::default(),
        ))
    });

    // Resolve context budget for this agent. Wire field is named
    // `max_context_tokens` and must track the runtime-profile budget
    // (same source Zerocode's context meter uses), not the provider
    // model-window helper which falls back to 32_000 when unset.
    let max_context_tokens = {
        let cfg = state.config.read();
        cfg.effective_max_context_tokens(&turn_alias) as u64
    };

    // Broadcast agent_start event
    let _ = state.event_tx.send(serde_json::json!({
        "type": "agent_start",
        "model_provider": provider_label,
        "model": turn_model,
    }));

    // Set session state to running
    let turn_id = uuid::Uuid::new_v4().to_string();
    if let Some(ref backend) = state.session_backend {
        let _ = backend.set_session_state(session_key, "running", Some(&turn_id));
    }

    // ── Cancellation token lifecycle ─────────────────────────────
    // Register the turn's token so the abort endpoint can cancel it.
    // Remove it after the turn completes regardless of outcome
    // (normal, error, or cancelled).
    {
        state
            .cancel_tokens
            .lock()
            .expect("cancel_tokens lock poisoned")
            .insert(session_key.to_string(), cancel_token.clone());
    }

    // Channel for streaming turn events from the agent.
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<TurnEvent>(64);

    let content_owned = content.to_string();
    let session_key_owned = session_key.to_string();
    let turn_fut = async {
        use ::zeroclaw_log::Instrument as _;
        let span = ::zeroclaw_log::info_span!(
            target: "zeroclaw_log_internal_scope",
            "zeroclaw_scope",
            session_key = %session_key_owned,
            agent_alias = %turn_alias,
            model_provider = %turn_provider,
            model = %turn_model,
            channel = WS_CHANNEL_KEY,
        );
        zeroclaw_runtime::agent::loop_::scope_session_key(
            Some(session_key_owned.clone()),
            zeroclaw_runtime::agent::cost::TOOL_LOOP_TURN_USAGE.scope(
                turn_usage.clone(),
                zeroclaw_runtime::agent::cost::TOOL_LOOP_COST_TRACKING_CONTEXT.scope(
                    cost_tracking_context.clone(),
                    agent
                        .turn_streamed_with_steering_state(
                            &content_owned,
                            event_tx,
                            Some(cancel_token.clone()),
                            Some(&mut *steering_rx),
                        )
                        .instrument(span),
                ),
            ),
        )
        .await
    };

    // Drive both futures concurrently: the agent turn produces events
    // and we relay them over WebSocket. Track streamed chunks so we
    // can reconstruct partial content on cancellation.
    let mut accumulated_text = String::new();

    // Aggregate token usage across all LLM calls in this turn.
    // The agent emits TurnEvent::Usage once per LLM call when the provider
    // surfaces usage; we sum to produce a single done-frame total.
    let mut total_input_tokens: Option<u64> = None;
    let mut total_output_tokens: Option<u64> = None;

    // Track the most recent absolute provider-reported prompt size
    // (replaces on each TurnEvent::Usage; not accumulated).
    // Used for accurate context-bar rendering on the client.
    let mut last_input_tokens: Option<u64> = None;

    let forward_fut = async {
        let mut cancel_drained = false;
        loop {
            tokio::select! {
                biased;
                _ = cancel_token.cancelled(), if !cancel_drained => {
                    conversation.drain_approvals();
                    cancel_drained = true;
                    // Fall through; the agent loop will now wake from the
                    // approval await, see the cancel token, and propagate
                    // a ToolLoopCancelled error which closes event_rx and
                    // breaks this loop on the `event_rx.recv()` arm below.
                }
                event_opt = event_rx.recv() => {
                    let Some(event) = event_opt else { break };
                    let ws_msg = match event {
                        TurnEvent::Usage {
                            input_tokens,
                            cached_input_tokens: _,
                            output_tokens,
                            cost_usd: _,
                        } => {
                            if let Some(it) = input_tokens {
                                total_input_tokens = Some(total_input_tokens.unwrap_or(0) + it);
                                last_input_tokens = Some(it);
                            }
                            if let Some(ot) = output_tokens {
                                total_output_tokens = Some(total_output_tokens.unwrap_or(0) + ot);
                            }
                            continue;
                        }
                        TurnEvent::Chunk { ref delta } => {
                            accumulated_text.push_str(delta);
                            serde_json::json!({ "type": "chunk", "content": delta })
                        }
                        TurnEvent::Thinking { delta } => {
                            serde_json::json!({ "type": "thinking", "content": delta })
                        }
                        TurnEvent::ToolCall { id, name, args } => {
                            serde_json::json!({ "type": "tool_call", "id": id, "name": name, "args": args })
                        }
                        TurnEvent::ToolResult { id, name, output } => {
                            serde_json::json!({ "type": "tool_result", "id": id, "name": name, "output": output })
                        }
                        TurnEvent::ApprovalRequest {
                            request_id,
                            tool_name,
                            arguments_summary,
                            timeout_secs,
                        } => approval_request_ws_frame(
                            &request_id,
                            &tool_name,
                            &arguments_summary,
                            timeout_secs,
                        ),
                        TurnEvent::HistoryTrimmed {
                            dropped_messages,
                            kept_turns,
                            reason,
                        } => history_trimmed_ws_frame(dropped_messages, kept_turns, &reason),
                        TurnEvent::Plan { entries } => serde_json::json!({
                            "type": "plan",
                            "entries": entries,
                        }),
                    };
                    conversation.publish(&ws_msg);
                }
            }
        }
    };

    let (result, ()) = tokio::join!(turn_fut, forward_fut);

    // ── Remove cancel token (turn finished) ──────────────────────
    {
        state
            .cancel_tokens
            .lock()
            .expect("cancel_tokens lock poisoned")
            .remove(session_key);
    }

    // The agent no longer reads steering. Free the turn slot first, so a
    // message sent from here on starts a new turn instead of steering this
    // one, then collect what was queued but never read.
    conversation.finish_turn(generation);
    steering_rx.close();
    let mut late = Vec::new();
    while let Ok(message) = steering_rx.try_recv() {
        late.push(message);
    }

    // Check if this turn was cancelled. `turn_streamed` propagates
    // `ToolLoopCancelled` through anyhow, so we detect it here.
    let was_cancelled = match &result {
        Err(e) => zeroclaw_runtime::agent::loop_::is_tool_loop_cancelled(&e.error),
        Ok(_) => false,
    };

    if was_cancelled {
        if let Some(ref backend) = state.session_backend {
            let still_exists = backend.session_exists(session_key);
            if still_exists {
                match &result {
                    Err(error) if !error.new_messages.is_empty() => {
                        persist_conversation_messages(
                            backend.as_ref(),
                            session_key,
                            &error.new_messages,
                        );
                        if !has_assistant_chat_message(&error.new_messages) {
                            let marker = zeroclaw_runtime::i18n::get_required_cli_string(
                                "turn-interrupted-by-user",
                            );
                            let truncated = if accumulated_text.is_empty() {
                                marker
                            } else {
                                format!("{accumulated_text}\n\n{marker}")
                            };
                            let assistant_msg =
                                zeroclaw_providers::ChatMessage::assistant(&truncated);
                            // Re-check before the raw append — the user can
                            // delete the session between the outer check and
                            // here; `persist_conversation_messages` already
                            // re-checks internally.
                            if backend.session_exists(session_key) {
                                let _ = backend.append(session_key, &assistant_msg);
                            }
                        }
                    }
                    _ => {
                        let marker = zeroclaw_runtime::i18n::get_required_cli_string(
                            "turn-interrupted-by-user",
                        );
                        let truncated = if accumulated_text.is_empty() {
                            marker
                        } else {
                            format!("{accumulated_text}\n\n{marker}")
                        };
                        let assistant_msg = zeroclaw_providers::ChatMessage::assistant(&truncated);
                        if backend.session_exists(session_key) {
                            let _ = backend.append(session_key, &assistant_msg);
                        }
                    }
                }
            }
        }

        persist_companion_capture(state, &turn_alias, session_id, &turn_id, auth_subject);

        // Inform the client the turn was aborted
        conversation.publish(&serde_json::json!({ "type": "aborted" }));

        if let Some(ref backend) = state.session_backend
            && backend.session_exists(session_key)
        {
            let _ = backend.set_session_state(session_key, "idle", None);
        }

        // Broadcast agent_end event
        let _ = state.event_tx.send(serde_json::json!({
            "type": "agent_end",
            "model_provider": provider_label,
            "model": turn_model,
        }));

        // Trace the cancelled turn so the doctor / replay tool sees it
        // alongside successful turns.follow-through.
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Cancel)
                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                .with_attrs(::serde_json::json!({
                    "model_provider": provider_label,
                    "model": turn_model,
                    "session_key": session_key,
                    "reason": "interrupted by user",
                    "cancelled": true,
                    "trace_id": turn_id,
                })),
            "gateway_ws_turn"
        );

        // A cancel stops the conversation's work, queued steering included.
        return Vec::new();
    }

    match result {
        Ok(outcome) => {
            if let Some(ref backend) = state.session_backend {
                persist_conversation_messages(backend.as_ref(), session_key, &outcome.new_messages);
            }

            persist_companion_capture(state, &turn_alias, session_id, &turn_id, auth_subject);

            // Fire-and-forget curated-memory consolidation (sqlite Memory).
            // Companion capture is a separate seam and already ran above.
            if state.auto_save {
                if let Some(mem) = ws_memory.clone() {
                    // The agent's provider/model after the turn, not the
                    // gateway-wide boot default (upstream #10637).
                    let (_, live_provider_ref, live_model) = agent.attribution_fields();
                    let live_config = Arc::clone(&state.config);
                    let memory_config = state.config.read().memory.clone();
                    let user_msg = content.to_string();
                    let assistant_resp = outcome.response.clone();
                    zeroclaw_spawn::spawn!(async move {
                        let config = live_config.read().clone();
                        let Some((model_provider, model, temperature)) =
                            ws_consolidation_model(&config, &live_provider_ref, &live_model)
                        else {
                            ::zeroclaw_log::record!(
                                DEBUG,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_attrs(::serde_json::json!({
                                    "model_provider": &live_provider_ref,
                                })),
                                "WS memory consolidation skipped: provider no longer resolves"
                            );
                            return;
                        };
                        if let Err(e) = zeroclaw_memory::consolidation::consolidate_turn(
                            model_provider.as_ref(),
                            &model,
                            temperature,
                            mem.as_ref(),
                            &memory_config,
                            &user_msg,
                            &assistant_resp,
                        )
                        .await
                        {
                            ::zeroclaw_log::record!(
                                DEBUG,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                                "WS memory consolidation skipped"
                            );
                        }
                    });
                } else {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        "WS memory consolidation skipped"
                    );
                }
            }

            let total_tokens = match (total_input_tokens, total_output_tokens) {
                (Some(i), Some(o)) => Some(i.saturating_add(o)),
                (Some(i), None) => Some(i),
                (None, Some(o)) => Some(o),
                (None, None) => None,
            };
            let cost_usd = turn_usage
                .as_ref()
                .map(|usage| *usage.lock())
                .filter(|usage| usage.input_tokens > 0 || usage.output_tokens > 0)
                .map(|usage| usage.cost_usd);

            let done = serde_json::json!({
                "type": "done",
                "full_response": outcome.response,
                "input_tokens": total_input_tokens,
                "output_tokens": total_output_tokens,
                "tokens_used": total_tokens,
                "cost_usd": cost_usd,
                "model": turn_model,
                "provider": provider_label,
                "max_context_tokens": max_context_tokens,
                "last_input_tokens": last_input_tokens,
            });
            conversation.publish(&done);

            // Set session state to idle
            if let Some(ref backend) = state.session_backend {
                let _ = backend.set_session_state(session_key, "idle", None);
            }

            // Broadcast agent_end event
            let _ = state.event_tx.send(serde_json::json!({
                "type": "agent_end",
                "model_provider": provider_label,
                "model": turn_model,
            }));

            // Append a runtime-trace.jsonl record so a `zeroclaw doctor`
            // sweep sees gateway WS turns alongside channel and CLI turns.
            // Closes the gateway-side trace gap from
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_attrs(::serde_json::json!({
                        "model_provider": provider_label,
                        "model": turn_model,
                        "session_key": session_key,
                        "input_tokens": total_input_tokens,
                        "output_tokens": total_output_tokens,
                        "tokens_used": total_tokens,
                        "cost_usd": cost_usd,
                        "last_input_tokens": last_input_tokens,
                        "trace_id": turn_id,
                    })),
                "gateway_ws_turn"
            );
        }
        Err(e) => {
            if let Some(ref backend) = state.session_backend
                && !e.new_messages.is_empty()
            {
                persist_conversation_messages(backend.as_ref(), session_key, &e.new_messages);
            }

            persist_companion_capture(state, &turn_alias, session_id, &turn_id, auth_subject);

            // Set session state to error
            if let Some(ref backend) = state.session_backend {
                let _ = backend.set_session_state(session_key, "error", Some(&turn_id));
            }

            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({"error": format!("{}", e.error)})),
                "Agent turn failed"
            );
            let sanitized = zeroclaw_providers::sanitize_api_error(&e.error.to_string());
            let error_code = if sanitized.to_lowercase().contains("api key")
                || sanitized.to_lowercase().contains("authentication")
                || sanitized.to_lowercase().contains("unauthorized")
            {
                "AUTH_ERROR"
            } else if sanitized.to_lowercase().contains("model_provider")
                || sanitized.to_lowercase().contains("model")
            {
                "PROVIDER_ERROR"
            } else {
                "AGENT_ERROR"
            };
            let err = serde_json::json!({
                "type": "error",
                "message": sanitized,
                "code": error_code,
            });
            conversation.publish(&err);

            // Broadcast error event
            let _ = state.event_tx.send(serde_json::json!({
                "type": "error",
                "component": "ws_chat",
                "message": sanitized,
            }));

            // Trace the failed turn so the doctor / replay tool sees the
            // failure mode and the turn_id can be cross-referenced with
            // costs.jsonl.follow-through.
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model_provider": provider_label,
                        "model": turn_model,
                        "session_key": session_key,
                        "error": sanitized,
                        "error_code": error_code,
                        "trace_id": turn_id,
                    })),
                "gateway_ws_turn"
            );
        }
    }
    late
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    /// Consolidation follows the agent's live provider reference, with that
    /// entry's model and temperature, not the install-wide default.
    #[test]
    fn ws_consolidation_model_follows_the_live_provider_reference() {
        use zeroclaw_api::attribution::Attributable as _;
        use zeroclaw_config::schema::{
            ModelProviderConfig, OllamaModelProviderConfig, OpenAIModelProviderConfig,
        };
        let mut config = zeroclaw_config::schema::Config::default();
        config.providers.models.openai.insert(
            "install".to_string(),
            OpenAIModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("install-model".to_string()),
                    temperature: Some(0.9),
                    ..Default::default()
                },
            },
        );
        config.providers.models.ollama.insert(
            "local".to_string(),
            OllamaModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("local-model".to_string()),
                    temperature: Some(0.3),
                    ..Default::default()
                },
                ..OllamaModelProviderConfig::default()
            },
        );

        let (provider, model, temperature) =
            ws_consolidation_model(&config, "ollama.local", "llama3").unwrap();
        assert_eq!(provider.alias(), "local");
        assert_eq!(model, "llama3");
        assert_eq!(temperature, Some(0.3));

        let (provider, model, temperature) =
            ws_consolidation_model(&config, "ollama.local", "").unwrap();
        assert_eq!(provider.alias(), "local");
        assert_eq!(model, "local-model");
        assert_eq!(temperature, Some(0.3));

        // A reference that no longer resolves is skipped, not rerouted.
        assert!(ws_consolidation_model(&config, "ollama.removed", "llama3").is_none());
        assert!(ws_consolidation_model(&config, "not-dotted", "llama3").is_none());
    }

    #[test]
    fn consolidation_does_not_use_the_gateway_boot_provider() {
        let src = process_chat_message_src();
        let consolidation = src.find("consolidate_turn").expect("consolidation call");
        let block = &src[src[..consolidation].rfind("if state.auto_save").unwrap()..consolidation];
        assert!(!block.contains("state.model_provider"), "{block}");
        assert!(block.contains("ws_consolidation_model"), "{block}");
    }

    fn process_chat_message_src() -> &'static str {
        let src = include_str!("ws.rs");
        let start = src
            .find("async fn process_chat_message")
            .expect("process_chat_message");
        let rest = &src[start..];
        let end = rest
            .find("\n#[cfg(test)]")
            .expect("test module follows process_chat_message");
        &rest[..end]
    }

    #[test]
    fn cancel_path_captures_before_aborted_frame() {
        let src = process_chat_message_src();
        let cancel = src.find("if was_cancelled").expect("cancel branch");
        let match_result = src.find("match result").expect("match result");
        let block = &src[cancel..match_result];
        let capture = block
            .find("persist_companion_capture")
            .expect("cancel must call capture");
        let transmit = block
            .find("\"type\": \"aborted\"")
            .expect("cancel must send aborted");
        assert!(
            capture < transmit,
            "cancel must capture at settlement before transmitting aborted"
        );
    }

    #[test]
    fn error_path_captures_before_error_frame() {
        let src = process_chat_message_src();
        let err_arm = src.rfind("Err(e) =>").expect("error arm");
        let block = &src[err_arm..];
        let capture = block
            .find("persist_companion_capture")
            .expect("error must call capture");
        let transmit = block
            .find("\"type\": \"error\"")
            .expect("error must send error frame");
        assert!(
            capture < transmit,
            "error must capture at settlement before transmitting the error frame"
        );
    }

    #[test]
    fn success_path_captures_before_done_frame() {
        let src = process_chat_message_src();
        let ok_arm = src.find("Ok(outcome) =>").expect("success arm");
        let err_arm = src.rfind("Err(e) =>").expect("error arm");
        let block = &src[ok_arm..err_arm];
        let capture = block
            .find("persist_companion_capture")
            .expect("success must call capture");
        let transmit = block
            .find("\"type\": \"done\"")
            .expect("success must send done");
        assert!(
            capture < transmit,
            "success must capture at settlement before transmitting done"
        );
    }

    #[test]
    fn ws_turn_has_a_single_channel_identity() {
        // Regression: `Agent.channel_name` was set to "ws" to match the
        // back-channel registration key while the turn span still recorded
        // `channel = "wss"`, so one turn was attributed to two channel names.
        // All three uses now derive from WS_CHANNEL_KEY; this pins the value
        // to the historical ingress name so observability stays stable and
        // interactive-tool lookups still resolve.
        assert_eq!(
            WS_CHANNEL_KEY, "wss",
            "WS ingress identity must stay `wss` — it is the name already used by \
             the turn span and SSE `channel` field; changing it splits attribution"
        );
    }

    #[tokio::test]
    async fn ws_back_channel_registers_under_the_ingress_identity() {
        // The interactive tools (`ask_user`, `poll`, `escalate_to_human`) look
        // the channel up by the agent's channel name. If the registration key
        // and WS_CHANNEL_KEY ever diverge, that lookup misses and the tools
        // silently fall back to an arbitrary seeded channel — the original bug.
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let pending = crate::ws_approval::new_pending_approvals();
        let approval_channel = Arc::new(WsApprovalChannel::new(
            tx,
            pending,
            Duration::from_secs(WS_APPROVAL_TIMEOUT_SECS),
        ));

        let handle: zeroclaw_runtime::tools::PerToolChannelHandle =
            Arc::new(parking_lot::RwLock::new(std::collections::HashMap::new()));
        handle.write().insert(
            WS_CHANNEL_KEY.to_string(),
            approval_channel as Arc<dyn zeroclaw_api::channel::Channel>,
        );

        // Interactive tools resolve the back-channel by the agent's channel
        // name; this is the lookup `ask_user` / `poll` / `escalate_to_human`
        // perform against their shared channel map.
        let resolved = handle.read().get(WS_CHANNEL_KEY).cloned();
        assert!(
            resolved.is_some(),
            "back-channel must be resolvable by the same key the agent reports \
             as its channel name ({WS_CHANNEL_KEY})"
        );
        assert!(
            !resolved.unwrap().supports_outbound_send(),
            "WS approval channel must declare that `send` does not deliver, so \
             poll/escalate_to_human fail honestly instead of reporting false success"
        );
    }

    #[test]
    fn restore_trim_uses_live_history_trimmed_frame_shape() {
        let frame = history_trimmed_ws_frame(12, 3, "message limit");

        assert_eq!(
            frame,
            serde_json::json!({
                "type": "history_trimmed",
                "dropped_messages": 12,
                "kept_turns": 3,
                "reason": "message limit",
            })
        );
    }

    #[test]
    fn extract_ws_token_from_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer zc_test123".parse().unwrap());
        assert_eq!(extract_ws_token(&headers, None), Some("zc_test123"));
    }

    #[test]
    fn extract_ws_token_from_subprotocol() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "zeroclaw.v1, bearer.zc_sub456".parse().unwrap(),
        );
        assert_eq!(extract_ws_token(&headers, None), Some("zc_sub456"));
    }

    #[test]
    fn extract_ws_token_from_query_param() {
        let headers = HeaderMap::new();
        assert_eq!(
            extract_ws_token(&headers, Some("zc_query789")),
            Some("zc_query789")
        );
    }

    #[test]
    fn extract_ws_token_precedence_header_over_subprotocol() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer zc_header".parse().unwrap());
        headers.insert("sec-websocket-protocol", "bearer.zc_sub".parse().unwrap());
        assert_eq!(
            extract_ws_token(&headers, Some("zc_query")),
            Some("zc_header")
        );
    }

    #[test]
    fn extract_ws_token_precedence_subprotocol_over_query() {
        let mut headers = HeaderMap::new();
        headers.insert("sec-websocket-protocol", "bearer.zc_sub".parse().unwrap());
        assert_eq!(extract_ws_token(&headers, Some("zc_query")), Some("zc_sub"));
    }

    #[test]
    fn extract_ws_token_returns_none_when_empty() {
        let headers = HeaderMap::new();
        assert_eq!(extract_ws_token(&headers, None), None);
    }

    #[test]
    fn extract_ws_token_skips_empty_header_value() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(
            extract_ws_token(&headers, Some("zc_fallback")),
            Some("zc_fallback")
        );
    }

    #[test]
    fn extract_ws_token_skips_empty_query_param() {
        let headers = HeaderMap::new();
        assert_eq!(extract_ws_token(&headers, Some("")), None);
    }

    #[test]
    fn extract_ws_token_subprotocol_with_multiple_entries() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "sec-websocket-protocol",
            "zeroclaw.v1, bearer.zc_tok, other".parse().unwrap(),
        );
        assert_eq!(extract_ws_token(&headers, None), Some("zc_tok"));
    }

    #[test]
    fn chat_ws_unaffected_by_nodes_v2_subprotocol_requirement() {
        // `/ws/chat` still upgrades when Sec-WebSocket-Protocol is absent or
        // carries only the nodes token. The nodes v2 handler is fail-closed;
        // this path is not.
        assert!(!client_offers_chat_protocol(&HeaderMap::new()));
        let mut nodes_only = HeaderMap::new();
        nodes_only.insert(
            "sec-websocket-protocol",
            "zeroclaw.nodes.v2".parse().unwrap(),
        );
        assert!(!client_offers_chat_protocol(&nodes_only));
        let mut chat = HeaderMap::new();
        chat.insert("sec-websocket-protocol", "zeroclaw.v1".parse().unwrap());
        assert!(client_offers_chat_protocol(&chat));
    }

    #[test]
    fn session_scoped_events_only_match_their_session() {
        let target_event = serde_json::json!({
            "type": "message",
            "session_id": "operator-1",
            "content": "deploy finished"
        });
        let other_event = serde_json::json!({
            "type": "message",
            "session_id": "operator-2",
            "content": "different session"
        });
        // No session_id and not on the global whitelist → dropped.
        let nameless_observability = serde_json::json!({
            "type": "agent_start",
            "source": "observability",
            "model": "gpt-4o"
        });
        // No session_id but on the global whitelist (`cron_result`) → forwarded.
        let cron = serde_json::json!({
            "type": "cron_result",
            "output": "global notification"
        });

        assert!(event_matches_session(&target_event, "operator-1"));
        assert!(!event_matches_session(&other_event, "operator-1"));
        assert!(!event_matches_session(
            &nameless_observability,
            "operator-1"
        ));
        assert!(event_matches_session(&cron, "operator-1"));
    }

    #[test]
    fn event_matches_session_defaults_drops_unwhitelisted_no_session_frames() {
        // The pre-contract was `None => true`, which silently leaked
        // every BroadcastObserver telemetry frame (including `error`) into
        // every chat WebSocket. The fix flips the default; verify each
        // observed-in-the-wild leak shape is now blocked.
        for ty in [
            "agent_start",
            "agent_end",
            "llm_request",
            "tool_call",
            "tool_call_start",
            "error",
        ] {
            let frame = serde_json::json!({
                "type": ty,
                "source": "observability",
                "timestamp": "2026-06-04T00:00:00Z",
            });
            assert!(
                !event_matches_session(&frame, "operator-1"),
                "{ty} observability frame must be dropped from chat WS"
            );
        }
    }

    #[tokio::test]
    async fn ws_memory_resolution_honors_agent_backend_none_over_install_backend() {
        use tempfile::TempDir;
        use zeroclaw_config::multi_agent::MemoryBackendKind;
        use zeroclaw_config::schema::{AliasedAgentConfig, Config};

        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        config.memory.backend = "sqlite.default".to_string();

        let mut agent = AliasedAgentConfig::default();
        agent.memory.backend = MemoryBackendKind::None;
        config.agents.insert("web".to_string(), agent);

        let memory = resolve_ws_memory_handle(&config, "web")
            .await
            .expect("WS per-agent memory resolution");

        assert!(
            memory.is_none(),
            "WebSocket consolidation must disable memory when the agent backend is none"
        );
    }

    #[test]
    fn event_matches_session_passes_session_scoped_chat_messages() {
        // /api/sessions/{id}/messages broadcasts a session-scoped assistant
        // injection — that frame must reach the chat for its session.
        let assistant_inject = serde_json::json!({
            "type": "message",
            "session_id": "operator-1",
            "role": "assistant",
            "content": "hello",
        });
        assert!(event_matches_session(&assistant_inject, "operator-1"));
        assert!(!event_matches_session(&assistant_inject, "operator-2"));
    }

    #[test]
    fn observability_tagged_frames_are_filtered() {
        // The defense-in-depth helper: any frame with source="observability"
        // is telemetry, regardless of type or session_id presence.
        let obs = serde_json::json!({
            "type": "tool_call",
            "source": "observability",
            "tool": "shell",
        });
        assert!(is_observability_telemetry(&obs));

        let chat = serde_json::json!({
            "type": "tool_call",
            "id": "call-1",
            "name": "file_write",
            "args": {"path": "/tmp/x"},
        });
        assert!(!is_observability_telemetry(&chat));
    }

    #[test]
    fn observability_telemetry_filter_handles_malformed_source_field() {
        // Edge cases the previous tool-frame discriminator covered: ensure
        // the source-tag check doesn't false-positive on weird `source`
        // values that happen to coexist with chat-shaped frames.
        for source in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!(42),
            serde_json::json!("api"),
            serde_json::json!({"nested": "x"}),
        ] {
            let frame = serde_json::json!({
                "type": "tool_call",
                "id": "call-1",
                "name": "file_write",
                "source": source,
            });
            assert!(
                !is_observability_telemetry(&frame),
                "frame with source={frame:?} must not be flagged as observability telemetry",
            );
        }
    }

    #[test]
    fn chat_tool_frames_pass_through_when_session_scoped() {
        // Real chat tool frames (ws.rs process_chat_message) are streamed
        // over the per-turn channel, not the broadcast bus, but if anything
        // ever rebroadcasts one with the right session_id it must pass.
        let chat_tool_call = serde_json::json!({
            "type": "tool_call",
            "session_id": "operator-1",
            "id": "call-1",
            "name": "file_write",
            "args": {"path": "/tmp/x"},
        });
        assert!(event_matches_session(&chat_tool_call, "operator-1"));
        assert!(!is_observability_telemetry(&chat_tool_call));
    }

    #[test]
    fn resolve_session_cwd_uses_requested_cwd() {
        let requested = tempfile::tempdir().unwrap();
        let fallback = tempfile::tempdir().unwrap();

        let resolved =
            resolve_session_cwd(Some(requested.path().to_str().unwrap()), fallback.path()).unwrap();

        assert_eq!(resolved, requested.path().canonicalize().unwrap());
    }

    #[test]
    fn resolve_session_cwd_uses_default_workspace_without_request() {
        let fallback = tempfile::tempdir().unwrap();

        let resolved = resolve_session_cwd(None, fallback.path()).unwrap();

        assert_eq!(resolved, fallback.path().canonicalize().unwrap());
    }

    #[test]
    fn resolve_ws_session_cwd_defaults_to_agent_workspace_without_request() {
        use tempfile::TempDir;
        use zeroclaw_config::schema::{AliasedAgentConfig, Config};

        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config
            .agents
            .insert("web".to_string(), AliasedAgentConfig::default());
        std::fs::create_dir_all(&config.data_dir).unwrap();
        let agent_workspace = config.agent_workspace_dir("web");
        assert!(!agent_workspace.exists());

        let resolved = resolve_ws_session_cwd(None, &config, "web").unwrap();

        assert!(agent_workspace.exists());
        assert_eq!(resolved, agent_workspace.canonicalize().unwrap());
        assert_ne!(resolved, config.data_dir.canonicalize().unwrap());
    }

    #[test]
    fn resolve_ws_session_cwd_keeps_requested_cwd_strict() {
        use tempfile::TempDir;
        use zeroclaw_config::schema::{AliasedAgentConfig, Config};

        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config
            .agents
            .insert("web".to_string(), AliasedAgentConfig::default());
        let agent_workspace = config.agent_workspace_dir("web");
        let missing_requested = tmp.path().join("missing");

        let err = resolve_ws_session_cwd(Some(missing_requested.to_str().unwrap()), &config, "web")
            .expect_err("explicit missing cwd should be rejected");

        assert!(!agent_workspace.exists());
        assert!(err.to_string().contains("cwd is not a usable directory"));
    }

    #[test]
    fn resolve_session_cwd_rejects_missing_directory() {
        let fallback = tempfile::tempdir().unwrap();
        let missing = fallback.path().join("missing");

        let err = resolve_session_cwd(Some(missing.to_str().unwrap()), fallback.path())
            .expect_err("missing cwd should be rejected");

        assert!(err.to_string().contains("cwd is not a usable directory"));
    }

    #[test]
    fn needs_onboarding_ws_error_points_to_onboard() {
        let config = zeroclaw_config::schema::Config::default();
        let frame = needs_onboarding_ws_error(&config)
            .expect("empty model must produce a WS onboarding error");

        assert_eq!(frame["type"], "error");
        assert_eq!(frame["error"], "needs_onboarding");
        assert_eq!(frame["code"], "NEEDS_ONBOARDING");
        assert_eq!(frame["url"], "/onboard");
        let message = frame["message"]
            .as_str()
            .expect("onboarding WS error must include a message");
        assert!(
            !message.starts_with('{') && !message.ends_with('}'),
            "missing Fluent key fallback leaked into WS error message: {message:?}"
        );
        assert!(
            message.to_lowercase().contains("quickstart"),
            "WS setup-gap message must explain the setup gap: {message:?}"
        );
    }

    #[test]
    fn needs_onboarding_ws_error_uses_current_configured_model() {
        let mut config = zeroclaw_config::schema::Config::default();
        config.providers.models.openai.insert(
            "default".to_string(),
            zeroclaw_config::schema::OpenAIModelProviderConfig {
                base: zeroclaw_config::schema::ModelProviderConfig {
                    model: Some("openai/gpt-4o-mini".to_string()),
                    api_key: Some("sk-test".to_string()),
                    ..Default::default()
                },
            },
        );

        assert!(
            needs_onboarding_ws_error(&config).is_none(),
            "current configured model must allow WebSocket agent construction to continue"
        );
    }

    /// Answers every call with `reply N`, recording how many messages the
    /// request carried. Holds each call until `gate` has a permit.
    struct ScriptedProvider {
        gate: Arc<tokio::sync::Semaphore>,
        seen: Arc<parking_lot::Mutex<Vec<Vec<String>>>>,
    }

    #[async_trait::async_trait]
    impl zeroclaw_api::model_provider::ModelProvider for ScriptedProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            request: zeroclaw_providers::ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<zeroclaw_providers::ChatResponse> {
            self.gate.acquire().await?.forget();
            let mut seen = self.seen.lock();
            seen.push(
                request
                    .messages
                    .iter()
                    .filter(|m| m.role == "user")
                    .map(|m| m.content.clone())
                    .collect(),
            );
            Ok(zeroclaw_providers::ChatResponse {
                text: Some(format!("reply {}", seen.len())),
                tool_calls: vec![],
                usage: None,
                reasoning_content: None,
            })
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for ScriptedProvider {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "scripted"
        }
    }

    struct SharedChat {
        state: AppState,
        scope: WsTurnScope,
        gate: Arc<tokio::sync::Semaphore>,
        seen: Arc<parking_lot::Mutex<Vec<Vec<String>>>>,
        _tmp: tempfile::TempDir,
    }

    impl SharedChat {
        fn new() -> Self {
            let tmp = tempfile::TempDir::new().unwrap();
            let state = crate::tests::admin_paircode_state(&tmp, false, false);
            Self {
                state,
                scope: WsTurnScope {
                    session_key: "gw_shared".into(),
                    session_id: "shared".into(),
                    auth_subject: None,
                },
                gate: Arc::new(tokio::sync::Semaphore::new(0)),
                seen: Arc::default(),
                _tmp: tmp,
            }
        }

        async fn attach(&self) -> crate::ws_conversation::Subscription<WsSession> {
            let provider = ScriptedProvider {
                gate: Arc::clone(&self.gate),
                seen: Arc::clone(&self.seen),
            };
            let workspace = self._tmp.path().to_path_buf();
            let (subscription, _) = self
                .state
                .ws_conversations
                .attach(&self.scope.session_key, |_seed| async move {
                    let memory_cfg = zeroclaw_config::schema::MemoryConfig {
                        backend: "none".into(),
                        ..Default::default()
                    };
                    let mem: Arc<dyn zeroclaw_memory::Memory> = Arc::from(
                        zeroclaw_memory::create_memory(&memory_cfg, &workspace, None).unwrap(),
                    );
                    let agent = zeroclaw_runtime::agent::Agent::builder()
                        .model_provider(Box::new(provider))
                        .tools(vec![])
                        .memory(mem)
                        .observer(Arc::new(zeroclaw_runtime::observability::NoopObserver))
                        .tool_dispatcher(Box::new(
                            zeroclaw_runtime::agent::dispatcher::NativeToolDispatcher,
                        ))
                        .workspace_dir(workspace)
                        .model_name("test-model".into())
                        .model_provider_name("scripted".into())
                        .agent_alias("web".into())
                        .build()?;
                    Ok::<_, anyhow::Error>(WsSession {
                        agent,
                        ws_memory: None,
                    })
                })
                .await
                .unwrap();
            subscription
        }

        fn send(
            &self,
            from: &crate::ws_conversation::Subscription<WsSession>,
            frame: serde_json::Value,
        ) -> Option<serde_json::Value> {
            handle_client_text(
                &self.state,
                &from.conversation,
                &self.scope,
                &frame.to_string(),
            )
        }
    }

    /// Frames up to and including the first terminal one.
    async fn frames_until_end(
        sub: &mut crate::ws_conversation::Subscription<WsSession>,
    ) -> Vec<serde_json::Value> {
        let mut frames = Vec::new();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(10), sub.frames.recv())
                .await
                .expect("turn must settle")
                .expect("frame");
            let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
            let end = matches!(frame["type"].as_str(), Some("done" | "aborted" | "error"));
            frames.push(frame);
            if end {
                return frames;
            }
        }
    }

    fn message(content: &str) -> serde_json::Value {
        serde_json::json!({ "type": "message", "content": content })
    }

    #[tokio::test]
    async fn sockets_on_one_session_share_its_turns_and_history() {
        let chat = SharedChat::new();
        let mut a = chat.attach().await;
        let mut b = chat.attach().await;
        chat.gate.add_permits(2);

        assert!(chat.send(&a, message("first")).is_none());
        for sub in [&mut a, &mut b] {
            let done = frames_until_end(sub).await.pop().unwrap();
            assert_eq!(done["type"], "done");
            assert_eq!(done["full_response"], "reply 1");
        }

        // The other socket continues the same history instead of a fork.
        assert!(chat.send(&b, message("second")).is_none());
        for sub in [&mut a, &mut b] {
            let done = frames_until_end(sub).await.pop().unwrap();
            assert_eq!(done["full_response"], "reply 2");
        }
        let history = chat.seen.lock()[1].clone();
        assert_eq!(history.len(), 2, "{history:?}");
        assert!(history[0].ends_with("first") && history[1].ends_with("second"));
    }

    #[tokio::test]
    async fn closing_a_socket_does_not_cancel_the_turn() {
        let chat = SharedChat::new();
        let a = chat.attach().await;
        let mut b = chat.attach().await;

        assert!(chat.send(&a, message("keep going")).is_none());
        drop(a);
        chat.gate.add_permits(1);

        let done = frames_until_end(&mut b).await.pop().unwrap();
        assert_eq!(done["type"], "done");
        assert_eq!(done["full_response"], "reply 1");
    }

    #[tokio::test]
    async fn a_cancel_frame_aborts_the_turn_for_every_socket() {
        let chat = SharedChat::new();
        let mut a = chat.attach().await;
        let mut b = chat.attach().await;

        assert!(chat.send(&a, message("long job")).is_none());
        while !a.conversation.is_running() {
            tokio::task::yield_now().await;
        }
        assert!(
            chat.send(&b, serde_json::json!({ "type": "cancel" }))
                .is_none()
        );
        chat.gate.add_permits(1);

        for sub in [&mut a, &mut b] {
            let end = frames_until_end(sub).await.pop().unwrap();
            assert_eq!(end["type"], "aborted");
        }
        let reply = chat
            .send(&a, serde_json::json!({ "type": "cancel" }))
            .unwrap();
        assert_eq!(reply["code"], "NO_ACTIVE_TURN");
    }

    #[tokio::test]
    async fn invalid_client_frames_are_answered_on_that_socket_only() {
        let chat = SharedChat::new();
        let a = chat.attach().await;
        for (frame, code) in [
            (
                serde_json::json!({ "type": "message", "content": "" }),
                "EMPTY_CONTENT",
            ),
            (
                serde_json::json!({ "type": "nope" }),
                "UNKNOWN_MESSAGE_TYPE",
            ),
            (
                serde_json::json!({ "type": "approval_response", "request_id": "r" }),
                "INVALID_APPROVAL_RESPONSE",
            ),
        ] {
            assert_eq!(chat.send(&a, frame).unwrap()["code"], code);
        }
        let reply = handle_client_text(&chat.state, &a.conversation, &chat.scope, "{");
        assert_eq!(reply.unwrap()["code"], "INVALID_JSON");
        assert!(!a.conversation.is_running());
    }

    #[test]
    fn session_queue_errors_map_to_explicit_websocket_codes() {
        use crate::session_queue::SessionQueueError;

        assert_eq!(
            session_queue_ws_error_code(&SessionQueueError::QueueFull {
                session_id: "gw_test".into(),
                depth: 2,
            }),
            "SESSION_QUEUE_FULL"
        );
        assert_eq!(
            session_queue_ws_error_code(&SessionQueueError::Timeout {
                session_id: "gw_test".into(),
            }),
            "SESSION_QUEUE_TIMEOUT"
        );
    }

    struct DeletedSessionBackend {
        append_calls: std::sync::Mutex<Vec<String>>,
    }

    impl zeroclaw_infra::session_backend::SessionBackend for DeletedSessionBackend {
        fn load(&self, _session_key: &str) -> Vec<zeroclaw_providers::ChatMessage> {
            Vec::new()
        }
        fn append(
            &self,
            session_key: &str,
            message: &zeroclaw_providers::ChatMessage,
        ) -> std::io::Result<()> {
            self.append_calls.lock().unwrap().push(format!(
                "{}:{}:{}",
                session_key, message.role, message.content
            ));
            Ok(())
        }
        fn remove_last(&self, _session_key: &str) -> std::io::Result<bool> {
            Ok(false)
        }
        fn list_sessions(&self) -> Vec<String> {
            Vec::new()
        }
        fn session_exists(&self, _session_key: &str) -> bool {
            // The user deleted the session between cancel and append.
            false
        }
    }

    #[test]
    fn persist_conversation_messages_skips_deleted_session() {
        use zeroclaw_providers::{ChatMessage, ConversationMessage};
        let backend = DeletedSessionBackend {
            append_calls: std::sync::Mutex::new(Vec::new()),
        };
        let messages = vec![
            ConversationMessage::Chat(ChatMessage::user("hi")),
            ConversationMessage::Chat(ChatMessage::assistant("[interrupted by user]")),
        ];

        persist_conversation_messages(&backend, "gw_deleted", &messages);

        assert!(
            backend.append_calls.lock().unwrap().is_empty(),
            "persist_conversation_messages must not resurrect a session whose \
             session_exists() returned false (see #7126)"
        );
    }

    #[test]
    fn stamp_session_keeps_the_name_of_a_new_session() {
        let dir = tempfile::tempdir().unwrap();
        let backend = zeroclaw_infra::make_session_backend(dir.path(), "sqlite").unwrap();
        let reported = stamp_session(backend.as_ref(), "gw_new1", "default", Some("Foo"));
        assert_eq!(reported.as_deref(), Some("Foo"));
        assert_eq!(
            backend.get_session_name("gw_new1").unwrap().as_deref(),
            Some("Foo"),
            "the name of a session created on connect must be stored"
        );
        // Reconnecting without a name reports the stored one.
        let reported = stamp_session(backend.as_ref(), "gw_new1", "default", None);
        assert_eq!(reported.as_deref(), Some("Foo"));
    }
}
