//! Opt-in non-Telegram turn transport. Conversation owns execution; this bounded
//! process-local journal owns only correlation and replay. Losing it never grants
//! permission to repeat an effect. Existing session receipts remain authoritative.
use super::*;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

const VERSION: u64 = 1;
const MAX_REQUESTS: usize = 256;
const MAX_CONTENT: usize = 64 * 1024;
const MAX_REPLAY_BYTES: usize = 64 * 1024;
const MAX_EVENTS: usize = 128;
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Store {
    epoch: String,
    entries: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    redacted: bool,
    conversation: String,
    subject: String,
    request: String,
    turn: String,
    content: String,
    attachments: Vec<String>,
    sequence: u64,
    events: Vec<Value>,
    bytes: usize,
    gap: bool,
    terminal: Option<Value>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            epoch: uuid::Uuid::new_v4().to_string(),
            entries: Mutex::new(HashMap::new()),
        }
    }
}

fn error(code: &str) -> Value {
    json!({"type":"turn_error", "version":VERSION, "code":code})
}

/// Brain-monitor wire events (#564): every S2 admission lands in the
/// structured log with identities and SIZES ONLY — message content and
/// attachment bytes are never logged. This is the S5 switch timeline's
/// authoritative feed (viewed via `/api/logs`, category `session`).
fn log_admission(
    scope: &WsTurnScope,
    alias: &str,
    request: &str,
    status: &str,
    content_bytes: usize,
    attachments: usize,
    turn: Option<&str>,
) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Inbound)
            .with_category(::zeroclaw_log::EventCategory::Session)
            .with_outcome(if matches!(status, "accepted" | "duplicate") {
                ::zeroclaw_log::EventOutcome::Success
            } else {
                ::zeroclaw_log::EventOutcome::Failure
            })
            .with_attrs(::serde_json::json!({
                "session_id": scope.session_id,
                "agent": alias,
                "request_id": request,
                "status": status,
                "content_bytes": content_bytes,
                "attachments": attachments,
                "turn_id": turn,
            })),
        "S2 turn admission"
    );
}

fn log_refusal(scope: &WsTurnScope, alias: &str, request: &str, code: &str, attachments: usize) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
            .with_category(::zeroclaw_log::EventCategory::Session)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(::serde_json::json!({
                "session_id": scope.session_id,
                "agent": alias,
                "request_id": request,
                "code": code,
                "attachments": attachments,
            })),
        "S2 turn admission refused"
    );
}

/// Terminal lifecycle event for BOTH paths (#564). `conversation` is the
/// gateway conversation key (`gw_<session>\x1f<agent>`); the logged
/// `session_id` strips the transport prefix so admission, cancel and
/// terminal events for one turn carry the SAME identity. Outcome mapping:
/// `done` succeeds; `aborted`/`error` fail (matching the legacy cancel
/// logging); anything else is unknown — never a silent success.
fn log_terminal(conversation: &str, request: Option<&str>, turn: &str, outcome: &str) {
    let (session_key, agent) = conversation
        .rsplit_once('\u{1f}')
        .unwrap_or((conversation, ""));
    let session_id = session_key
        .strip_prefix(super::GW_SESSION_PREFIX)
        .unwrap_or(session_key);
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Complete)
            .with_category(::zeroclaw_log::EventCategory::Session)
            .with_outcome(match outcome {
                "done" => ::zeroclaw_log::EventOutcome::Success,
                "aborted" | "error" => ::zeroclaw_log::EventOutcome::Failure,
                _ => ::zeroclaw_log::EventOutcome::Unknown,
            })
            .with_attrs(::serde_json::json!({
                "session_id": session_id,
                "agent": agent,
                "request_id": request,
                "turn_id": turn,
                "outcome": outcome,
            })),
        "turn terminal"
    );
}

fn key(conversation: &str, subject: &str, epoch: &str, request: &str) -> String {
    // Length-delimited identity cannot collide with another principal or with
    // the legacy caller-chosen request namespace by accidental concatenation.
    let mut hash = Sha256::new();
    for part in [conversation, subject, epoch, request] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("tc1:{}", hex::encode(hash.finalize()))
}

fn ack(store: &Store, request: &str, turn: Option<&str>, status: &str) -> Value {
    json!({"type":"turn_ack", "version":VERSION, "epoch":store.epoch,
        "request_id":request, "turn_id":turn, "status":status})
}

/// Unknown operations are parsed/refused before taking any admission action.
/// A negotiated socket cannot fall through to legacy message/cancel semantics.
pub(super) fn handle(
    state: &AppState,
    conversation: &Arc<Conversation<WsSession>>,
    scope: &WsTurnScope,
    negotiated: &mut bool,
    text: &str,
) -> (Option<Value>, Option<TurnClaim>) {
    let parsed = match serde_json::from_str::<Value>(text) {
        Ok(value) => value,
        Err(_) => return (Some(error("INVALID_JSON")), None),
    };
    if parsed["type"] != "turn_control" {
        // Owner question/approval capabilities retain their established gate.
        if matches!(
            parsed["type"].as_str(),
            Some("answer" | "approval_response")
        ) {
            return handle_client_text(state, conversation, scope, text);
        }
        return (Some(error("UNKNOWN_OPERATION")), None);
    }
    if parsed["version"].as_u64() != Some(VERSION) {
        return (Some(error("UNSUPPORTED_VERSION")), None);
    }
    let operation = parsed["op"].as_str().unwrap_or("");
    if !matches!(operation, "negotiate" | "message" | "resume" | "cancel") {
        return (Some(error("UNKNOWN_OPERATION")), None);
    }
    // Errors past this point name the operation so a client can tell an
    // admission failure (its turn will never start) from a failed control.
    let error_op = |code: &str| -> Value {
        json!({"type":"turn_error", "version":VERSION, "op":operation, "code":code})
    };
    let alias = conversation
        .key()
        .rsplit_once('\u{1f}')
        .map_or("", |(_, alias)| alias);
    // Bridge-operation parity: the legacy classifier routes attachment-bearing
    // messages to "unsupported", so a restricted bridge cannot carry them.
    // Negotiate/resume are transport bookkeeping and stay "chat". The
    // classifier reads the legacy frame shape, so project this op's payload
    // into that view — a turn_control envelope itself would classify as
    // "unsupported" wholesale.
    let permission = match operation {
        "cancel" => "cancel",
        "message" => device::frame_operation(&json!({
            "type": "message",
            "attachments": parsed.get("attachments"),
        })),
        _ => "chat",
    };
    if !device::permitted(state, scope, alias, permission)
        || !question_answer_authorized(state, scope)
    {
        return (Some(device::refusal("BRIDGE_OPERATION_DENIED")), None);
    }
    let Some(subject) = scope.auth_subject.as_deref() else {
        return (Some(error_op("AUTHENTICATION_REQUIRED")), None);
    };
    let store = &state.ws_conversations.turn_control;
    if operation == "negotiate" {
        *negotiated = true;
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Connect)
                .with_category(::zeroclaw_log::EventCategory::Session)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": scope.session_id,
                    "agent": alias,
                    "epoch": store.epoch,
                    "surface": scope.surface,
                })),
            "S2 turn-control negotiated"
        );
        return (
            Some(
                json!({"type":"turn_ready", "version":VERSION, "epoch":store.epoch,
            "limits":{"requests_per_epoch":MAX_REQUESTS,"content_bytes":MAX_CONTENT,
                "replay_bytes":MAX_REPLAY_BYTES,"replay_events":MAX_EVENTS,
                "write_timeout_ms":WRITE_TIMEOUT.as_millis()},
            "recovery":"process_epoch", "busy":"reject"}),
            ),
            None,
        );
    }
    if !*negotiated {
        return (Some(error_op("NEGOTIATION_REQUIRED")), None);
    }
    if parsed["epoch"].as_str() != Some(store.epoch.as_str()) {
        return (Some(error_op("EPOCH_MISMATCH")), None);
    }
    let Some(request) = parsed["request_id"]
        .as_str()
        .filter(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
    else {
        return (Some(error_op("INVALID_REQUEST_ID")), None);
    };
    let receipt_key = key(conversation.key(), subject, &store.epoch, request);
    let mut entries = store.entries.lock();
    if operation == "resume" {
        let Some(after) = parsed["after_seq"].as_u64() else {
            return (Some(error_op("INVALID_SEQUENCE")), None);
        };
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Query)
                .with_category(::zeroclaw_log::EventCategory::Session)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": scope.session_id,
                    "agent": alias,
                    "request_id": request,
                    "after_seq": after,
                })),
            "S2 turn replay requested"
        );
        return (
            Some(match entries.get(&receipt_key) {
                None => ack(store, request, None, "unknown"),
                Some(entry) if entry.redacted => ack(store, request, None, "unknown"),
                Some(entry) if after > entry.sequence => error_op("INVALID_SEQUENCE"),
                Some(entry) => {
                    let mut events: Vec<_> = entry
                        .events
                        .iter()
                        .filter(|event| event["seq"].as_u64().is_some_and(|seq| seq > after))
                        .cloned()
                        .collect();
                    if let Some(terminal) = &entry.terminal
                        && terminal["seq"].as_u64().is_some_and(|seq| seq > after)
                    {
                        events.push(terminal.clone());
                    }
                    json!({"type":"turn_replay", "version":VERSION, "epoch":store.epoch,
                    "request_id":request, "turn_id":entry.turn, "events":events,
                    "gap":entry.gap, "terminal":entry.terminal.is_some(), "last_seq":entry.sequence})
                }
            }),
            None,
        );
    }
    if operation == "cancel" {
        let Some(target) = parsed["turn_id"]
            .as_str()
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
        else {
            return (Some(error_op("INVALID_TURN_ID")), None);
        };
        let status = match entries.get(&receipt_key) {
            None => "unknown",
            Some(entry) if entry.redacted => "unknown",
            Some(entry) if entry.turn != target => "stale",
            Some(_) => conversation.cancel_turn(target),
        };
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Cancel)
                .with_category(::zeroclaw_log::EventCategory::Session)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": scope.session_id,
                    "agent": alias,
                    "request_id": request,
                    "turn_id": target,
                    "status": status,
                })),
            "S2 turn cancellation"
        );
        return (
            Some(
                json!({"type":"turn_cancel_ack", "version":VERSION,"epoch":store.epoch,
            "request_id":request,"turn_id":target,"status":status}),
            ),
            None,
        );
    }
    let Some(content) = parsed["content"]
        .as_str()
        .filter(|s| s.len() <= MAX_CONTENT)
    else {
        log_refusal(scope, alias, request, "INVALID_CONTENT", 0);
        return (Some(error_op("INVALID_CONTENT")), None);
    };
    // `attachments` is the sole admitted extra field: the same #491 scoped
    // handles, shape, limits and authorization as the legacy path — it grants
    // no new source or identity authority.
    let attachments = match parsed.get("attachments") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(ids))
            if ids.len() <= crate::api_attachments::MAX_MESSAGE_ITEMS =>
        {
            let Some(ids) = ids
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .filter(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok())
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()
            else {
                log_refusal(scope, alias, request, "INVALID_ATTACHMENTS", 0);
                return (Some(error_op("INVALID_ATTACHMENTS")), None);
            };
            let unique: std::collections::HashSet<_> = ids.iter().collect();
            if unique.len() != ids.len() {
                log_refusal(scope, alias, request, "INVALID_ATTACHMENTS", ids.len());
                return (Some(error_op("INVALID_ATTACHMENTS")), None);
            }
            ids
        }
        Some(_) => {
            log_refusal(scope, alias, request, "INVALID_ATTACHMENTS", 0);
            return (Some(error_op("INVALID_ATTACHMENTS")), None);
        }
    };
    if content.is_empty() && attachments.is_empty() {
        log_refusal(scope, alias, request, "INVALID_CONTENT", 0);
        return (Some(error_op("INVALID_CONTENT")), None);
    }
    // Unsupported input fields must not silently acquire source/attachment authority.
    if parsed.as_object().is_none_or(|fields| {
        fields.keys().any(|field| {
            !matches!(
                field.as_str(),
                "type" | "version" | "op" | "epoch" | "request_id" | "content" | "attachments"
            )
        })
    }) {
        log_refusal(scope, alias, request, "UNSUPPORTED_FIELD", 0);
        return (Some(error_op("UNSUPPORTED_FIELD")), None);
    }
    if let Some(entry) = entries.get(&receipt_key) {
        if entry.redacted {
            log_admission(
                scope,
                alias,
                request,
                "unknown",
                content.len(),
                attachments.len(),
                None,
            );
            return (Some(ack(store, request, None, "unknown")), None);
        }
        let status = if entry.content == content && entry.attachments == attachments {
            "duplicate"
        } else {
            "conflict"
        };
        log_admission(
            scope,
            alias,
            request,
            status,
            content.len(),
            attachments.len(),
            Some(&entry.turn),
        );
        return (Some(ack(store, request, Some(&entry.turn), status)), None);
    }
    // No eviction: forgetting accepted identities would permit an old request
    // to run again once a session backend's finite receipt window expires.
    if entries.len() >= MAX_REQUESTS {
        log_refusal(scope, alias, request, "REQUEST_CAPACITY", 0);
        return (Some(error_op("REQUEST_CAPACITY")), None);
    }
    // Resolve #491 scoped handles exactly as the legacy message path does:
    // the attachments store owns materialization; authorization is consulted
    // before and after resolution because a revoke can race the encode.
    // Nothing here runs when materialization refuses, so a refused request
    // never occupies the journal.
    let mut input = content.to_string();
    if !attachments.is_empty() {
        if alias.is_empty() {
            log_refusal(
                scope,
                alias,
                request,
                "ATTACHMENT_UNAVAILABLE",
                attachments.len(),
            );
            return (Some(error_op("ATTACHMENT_UNAVAILABLE")), None);
        }
        let payload_scope = crate::api_attachments::Scope {
            subject: subject.to_string(),
            session: scope.session_id.clone(),
            agent: alias.to_string(),
        };
        if !crate::api_attachments::scope_authorized(state, &payload_scope) {
            log_refusal(
                scope,
                alias,
                request,
                "UNAUTHORIZED_ATTACHMENTS",
                attachments.len(),
            );
            return (Some(error_op("UNAUTHORIZED_ATTACHMENTS")), None);
        }
        input = match state.ws_conversations.attachments.materialize(
            &payload_scope,
            &attachments,
            content,
        ) {
            Ok(materialized) => materialized,
            Err(_) => {
                log_refusal(
                    scope,
                    alias,
                    request,
                    "ATTACHMENT_UNAVAILABLE",
                    attachments.len(),
                );
                return (Some(error_op("ATTACHMENT_UNAVAILABLE")), None);
            }
        };
        if !crate::api_attachments::scope_authorized(state, &payload_scope) {
            log_refusal(
                scope,
                alias,
                request,
                "UNAUTHORIZED_ATTACHMENTS",
                attachments.len(),
            );
            return (Some(error_op("UNAUTHORIZED_ATTACHMENTS")), None);
        }
    }
    let Some(mut claim) = conversation.start_if_idle(input) else {
        log_admission(
            scope,
            alias,
            request,
            "busy",
            content.len(),
            attachments.len(),
            None,
        );
        return (Some(ack(store, request, None, "busy")), None);
    };
    // Holding the journal lock serializes reserve/record/map versus other
    // extension admissions. The Conversation lock itself is not held here.
    match accept_request(state, conversation, &scope.session_key, &receipt_key) {
        Ok((RequestReceipt::Recorded, _)) => {}
        Ok((RequestReceipt::Duplicate { .. }, _)) => {
            conversation.finish_turn(claim.generation);
            log_admission(
                scope,
                alias,
                request,
                "unknown",
                content.len(),
                attachments.len(),
                None,
            );
            return (Some(ack(store, request, None, "unknown")), None);
        }
        Err(_) => {
            conversation.finish_turn(claim.generation);
            log_admission(
                scope,
                alias,
                request,
                "refused",
                content.len(),
                attachments.len(),
                None,
            );
            return (Some(error_op("REQUEST_NOT_RECORDED")), None);
        }
    }
    let turn = claim.turn_id.clone();
    log_admission(
        scope,
        alias,
        request,
        "accepted",
        content.len(),
        attachments.len(),
        Some(&turn),
    );
    entries.insert(
        receipt_key.clone(),
        Entry {
            redacted: false,
            conversation: conversation.key().to_string(),
            subject: subject.to_string(),
            request: request.to_string(),
            turn: turn.clone(),
            content: content.to_string(),
            attachments: attachments.clone(),
            sequence: 0,
            events: Vec::new(),
            bytes: 0,
            gap: false,
            terminal: None,
        },
    );
    claim.original_input = Some(content.to_string());
    claim.request_id = Some(receipt_key);
    claim.surface = scope.surface;
    (
        Some(ack(store, request, Some(&turn), "accepted")),
        Some(claim),
    )
}

impl Store {
    /// Session deletion removes transport-held content too. Keep only the
    /// opaque receipt key as a tombstone: deleting a receipt must not let the
    /// same accepted request execute again. Admission and this hook share the
    /// journal lock; neither a late publisher nor conversation recreation can
    /// restore a tombstone's content.
    pub(crate) fn redact_session(&self, session_key: &str) {
        let mut entries = self.entries.lock();
        for entry in entries.values_mut().filter(|entry| {
            entry
                .conversation
                .rsplit_once('\u{1f}')
                .is_some_and(|(key, _)| key == session_key)
        }) {
            *entry = Entry {
                redacted: true,
                conversation: String::new(),
                subject: String::new(),
                request: String::new(),
                turn: String::new(),
                content: String::new(),
                attachments: Vec::new(),
                sequence: 0,
                events: Vec::new(),
                bytes: 0,
                gap: true,
                terminal: None,
            };
        }
    }

    /// A registry entry, not the currently active turn, owns correlation. The
    /// normal turn releases its active slot before emitting its final response.
    fn event(&self, request: &str, turn: &str, payload: &Value, terminal: bool) -> Option<Value> {
        let mut entries = self.entries.lock();
        let entry = entries
            .get_mut(request)
            .filter(|entry| !entry.redacted && entry.turn == turn && entry.terminal.is_none())?;
        entry.sequence += 1;
        let mut event = json!({"type":"turn_event", "version":VERSION, "epoch":self.epoch,
            "request_id":entry.request,"turn_id":turn,"seq":entry.sequence,
            "event":payload,"terminal":terminal});
        // Do not expose the internal receipt identity in the projected
        // payload. Match EXACTLY this entry's receipt key: provider-sourced
        // tool-call ids pass through verbatim (they are client-facing
        // correlation), and a prefix test would strip a legitimate id that
        // happens to start with the journal namespace.
        if let Some(fields) = event["event"].as_object_mut()
            && fields.get("id").and_then(Value::as_str) == Some(request)
        {
            fields.remove("id");
        }
        if terminal {
            event["stream_complete"] = (!entry.gap).into();
            entry.terminal = Some(event.clone());
            // Brain-monitor terminal (#564), S2 journal path: log the CLIENT's
            // request id (entry.request), not the internal receipt key.
            log_terminal(
                &entry.conversation,
                Some(&entry.request),
                turn,
                payload["outcome"].as_str().unwrap_or("unknown"),
            );
        } else {
            let bytes = event.to_string().len();
            if bytes > MAX_REPLAY_BYTES {
                entry.gap = true;
                event["event"] = json!({"type":"turn_gap","code":"OUTPUT_TOO_LARGE"});
            } else if !entry.gap
                && entry.bytes + bytes <= MAX_REPLAY_BYTES
                && entry.events.len() < MAX_EVENTS
            {
                entry.bytes += bytes;
                entry.events.push(event.clone());
            } else {
                entry.gap = true;
            }
        }
        Some(event)
    }

    pub(super) fn visible(&self, conversation: &str, subject: Option<&str>, frame: &Value) -> bool {
        let Some(subject) = subject else {
            return false;
        };
        self.entries.lock().values().any(|entry| {
            !entry.redacted
                && entry.conversation == conversation
                && entry.subject == subject
                && frame["turn_id"].as_str() == Some(entry.turn.as_str())
        })
    }
}

/// Publish both unchanged legacy output and the separate negotiated projection.
pub(super) fn publish(
    state: &AppState,
    conversation: &Conversation<WsSession>,
    request: Option<&str>,
    turn: &str,
    payload: &Value,
) {
    conversation.publish(payload);
    if let Some(request) = request
        && let Some(event) = state
            .ws_conversations
            .turn_control
            .event(request, turn, payload, false)
    {
        conversation.publish_control(&event);
    }
}

pub(super) fn finish(
    state: &AppState,
    conversation: &Conversation<WsSession>,
    request: Option<&str>,
    turn: &str,
    outcome: &str,
) {
    let journaled = if let Some(request) = request
        && let Some(event) = state.ws_conversations.turn_control.event(
            request,
            turn,
            &json!({"type":"turn_terminal","outcome":outcome}),
            true,
        ) {
        conversation.publish_control(&event);
        true
    } else {
        false
    };
    if !journaled {
        // Legacy turns carry no S2 journal entry (and legacy callers may send
        // no id at all) — event() could not record them, so the terminal
        // lifecycle log happens here (#564). Identities only, never content.
        log_terminal(conversation.key(), request, turn, outcome);
    }
}

pub(super) fn gap() -> Value {
    json!({"type":"turn_gap","version":VERSION,"code":"STREAM_GAP","recover":"resume"})
}

pub(super) async fn send(
    sender: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: Message,
    negotiated: bool,
) -> bool {
    if negotiated {
        tokio::time::timeout(WRITE_TIMEOUT, sender.send(message))
            .await
            .is_ok_and(|result| result.is_ok())
    } else {
        sender.send(message).await.is_ok()
    }
}
