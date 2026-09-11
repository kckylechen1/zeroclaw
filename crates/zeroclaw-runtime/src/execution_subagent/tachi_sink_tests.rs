//! Exact-contract transport boundary tests for [`TachiSessionFactSink`].
//!
//! Authoritative Consumer Contract Map:
//! - Consumer Pin SHA: `1d32e63bfd11c14e1b306c576d1c677d567dbc6d`
//! - Authoritative sources:
//!   1. `crates/memcore/src/db/harness_session_events.rs`:
//!      Domain types ~202-330; no-event projection 474-498; text validators 548-566;
//!      validate_new_event 569-677; same_material 836-844; cancel binding 846-879;
//!      ingest replay branch 882-927; authoritative tests
//!      `replayed_facts_build_one_canonical_spine_without_duplicate_rows`:1438 and
//!      `writer_rejects_nul_and_control_characters_in_summary_and_digest`:2172.
//!   2. `crates/tachi-server/src/agent_eval/session_spine.rs`:
//!      Actual facade input projection 63-108; event JSON receipt 182-194; state JSON 197-207;
//!      get-state JSON 223-229; connection JSON 261-271; reconnect JSON 294-304;
//!      advertisement JSON 330-337. Tests:
//!      `control_only_summary_and_digest_refuse_instead_of_vanishing` and
//!      `oversize_summary_and_unknown_kinds_refuse_without_journaling`.
//!   3. `crates/tachi-server/src/agent_eval/attachment.rs`:
//!      Required text helper 16-20 rejects absent/trim-empty without otherwise trimming
//!      returned value; attach receipt 189-214.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};
use zeroclaw_api::session_exec::{
    AdapterConnectionRef, AuthorityConfirmationRef, HostIdentityRef, InterventionRequestIdRef,
    RemoteSessionRef, SessionAttachmentRef, SessionCanonicalStateV1, SessionConnectionFactV1,
    SessionEventIdRef, SessionEventKindV1, SessionFactError, SessionInterventionDispositionV1,
    SessionInterventionKindV1, SessionReceiptAdmissionV1, SessionTerminalOutcomeV1,
};
use zeroclaw_tools::mcp_protocol::{JsonRpcRequest, JsonRpcResponse};
use zeroclaw_tools::mcp_transport::McpTransportConn;

use super::super::facts::{SessionBinding, SessionEventFact, SessionFactSink};
use super::{
    SUMMARY_CEILING, TachiFactSinkConfig, TachiSessionFactSink, project_summary,
    validate_and_project_confirmation_ref, validate_and_project_payload_digest,
    validate_attachment_id, validate_event_id,
};

// ─────────────────────────────────────────────────────────────────────────
// Scripted Exact-Contract Tachi Spine Fixture
// ─────────────────────────────────────────────────────────────────────────

/// Stored event row in the authoritative spine store.
/// Implements `same_material` exact comparison from `harness_session_events.rs:836-844`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredEvent {
    attachment_id: String,
    session_event_id: String,
    session_event_kind: String,
    session_event_outcome: Option<String>,
    source_revision: i64,
    authority_confirmation_ref: Option<String>,
    event_summary: Option<String>,
    payload_digest: Option<String>,
    event_occurred_at: String,
}

impl StoredEvent {
    fn same_material(&self, other: &StoredEvent) -> bool {
        self.session_event_kind == other.session_event_kind
            && self.session_event_outcome == other.session_event_outcome
            && self.source_revision == other.source_revision
            && self.authority_confirmation_ref == other.authority_confirmation_ref
            && self.event_summary == other.event_summary
            && self.payload_digest == other.payload_digest
            && self.event_occurred_at == other.event_occurred_at
    }
}

/// Per-attachment canonical spine projection state.
/// Ensures session facts and revision monotone counters do not bleed across attachments.
#[derive(Clone, Debug, Default)]
struct AttachmentSpineProjection {
    revision: i64,
    canonical_state: Option<String>,
    cleanup_recorded: bool,
    conflicting_terminal: bool,
    last_event_id: Option<String>,
    terminal_outcome: Option<String>,
    pre_disconnect_rank: i64,
}

fn state_rank(state: Option<&str>) -> i64 {
    match state {
        Some("accepted") => 0,
        Some("started") => 1,
        Some("progressing" | "input_required") => 2,
        Some("completed" | "failed" | "cancelled" | "inconsistent_reconciling") => 3,
        _ => -1,
    }
}

/// Fault injections for testing transport failure, corruption, and edge cases.
#[derive(Clone, Debug, Default)]
enum ResponseCorruption {
    #[default]
    None,
    NullCanonicalState,
    EmptyLastEventId,
    MissingCanonicalStateField,
    MissingCanonicalRevision,
    NegativeCanonicalRevision,
    StringCanonicalRevision,
    MissingCleanupRecorded,
    StringCleanupRecorded,
    MissingConflictingTerminal,
    StringConflictingTerminal,
    MissingLastEventId,
    UnknownCanonicalState(String),
    UnknownAdmission(String),
    UnknownDisposition(String),
    MismatchedEventId(String),
    MismatchedAttachmentId(String),
    MissingAdvertisementSeq,
    MissingCapabilities,
    StringCapability,
    ArrayCapabilities,
    MissingReconnectField,
}

/// In-memory authoritative spine state for scripted MCP fixture.
#[derive(Default)]
struct TachiSpineState {
    /// Unique stored events: (attachment_id, session_event_id) -> StoredEvent
    stored_events: HashMap<(String, String), StoredEvent>,
    /// Recorded accepted cancel confirmation refs: attachment_id -> set of confirmation refs
    cancel_confirmations: HashMap<String, HashSet<String>>,
    /// Active attachments
    attachments: HashSet<String>,
    attachment_bindings: HashMap<(String, String), String>,
    cancel_requests: HashSet<(String, String)>,
    /// Call counts per action
    call_counts: HashMap<String, usize>,
    /// Projections per attachment (isolated, no cross-session bleeding)
    projections: HashMap<String, AttachmentSpineProjection>,
    /// Advertisement sequence counter
    advertisement_seq: u64,
    /// Injected transport failure: if true, drops transport on next ingest call after recording
    drop_next_ingest_response: bool,
    /// Injected response corruption
    corruption: ResponseCorruption,
}

impl TachiSpineState {
    fn record_call(&mut self, action: &str) {
        *self.call_counts.entry(action.to_string()).or_default() += 1;
    }

    fn call_count(&self, action: &str) -> usize {
        self.call_counts.get(action).copied().unwrap_or(0)
    }

    fn stored_events_count(&self) -> usize {
        self.stored_events.len()
    }

    fn inject_fault(&mut self, corruption: ResponseCorruption) {
        self.corruption = corruption;
    }

    fn canonical_state_object(&self, attachment_id: &str) -> Value {
        let proj = self
            .projections
            .get(attachment_id)
            .cloned()
            .unwrap_or_default();
        json!({
            "canonical_state": proj.canonical_state,
            "canonical_revision": proj.revision,
            "cleanup_recorded": proj.cleanup_recorded,
            "conflicting_terminal": proj.conflicting_terminal,
            "last_event_id": proj.last_event_id,
        })
    }
}

/// Scripted MCP transport enforcing the pinned event wire contract.
/// Admission is synthetic; this fixture is not proof of live Tachi admission.
struct ScriptedTachiMcpServer {
    state: Arc<Mutex<TachiSpineState>>,
    expected_host_identity: String,
    expected_admission_receipt_ref: String,
}

impl ScriptedTachiMcpServer {
    fn new(state: Arc<Mutex<TachiSpineState>>) -> Self {
        Self {
            state,
            expected_host_identity: "test-host".to_string(),
            expected_admission_receipt_ref: "test-receipt-ref".to_string(),
        }
    }

    fn make_tool_success(id: Option<Value>, body: Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(json!({
                "isError": false,
                "content": [
                    {
                        "type": "text",
                        "text": serde_json::to_string(&body).expect("serialize receipt"),
                    }
                ]
            })),
            error: None,
        }
    }

    fn make_tool_error(id: Option<Value>, error_text: &str) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(json!({
                "isError": true,
                "content": [
                    {
                        "type": "text",
                        "text": error_text,
                    }
                ]
            })),
            error: None,
        }
    }
}

#[async_trait::async_trait]
impl McpTransportConn for ScriptedTachiMcpServer {
    async fn send_and_recv(&mut self, request: &JsonRpcRequest) -> anyhow::Result<JsonRpcResponse> {
        if request.method == "initialize" {
            let protocol = request
                .params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if protocol != "2025-06-18" {
                return Ok(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id.clone(),
                    result: None,
                    error: Some(zeroclaw_tools::mcp_protocol::JsonRpcError {
                        code: -32602,
                        message: format!("unsupported protocolVersion {protocol}"),
                        data: None,
                    }),
                });
            }
            return Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                result: Some(json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "tachi-mock-spine", "version": "1.0.0"},
                })),
                error: None,
            });
        }

        if request.method == "notifications/initialized" {
            return Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: None,
                result: Some(json!({})),
                error: None,
            });
        }

        if request.method != "tools/call" {
            return Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id.clone(),
                result: None,
                error: Some(zeroclaw_tools::mcp_protocol::JsonRpcError {
                    code: -32601,
                    message: format!("method not found: {}", request.method),
                    data: None,
                }),
            });
        }

        let params = request.params.as_ref().expect("tools/call params");
        let tool_name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if tool_name != "tachi_agent_eval" {
            return Ok(Self::make_tool_error(
                request.id.clone(),
                &format!("unknown tool {tool_name}"),
            ));
        }

        let args = params
            .get("arguments")
            .and_then(Value::as_object)
            .expect("tool arguments");
        let host_identity = args
            .get("host_identity")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let admission_ref = args
            .get("admission_receipt_ref")
            .and_then(Value::as_str)
            .unwrap_or_default();

        if host_identity != self.expected_host_identity {
            return Ok(Self::make_tool_error(
                request.id.clone(),
                "host_identity does not match admitted host",
            ));
        }
        if admission_ref != self.expected_admission_receipt_ref {
            return Ok(Self::make_tool_error(
                request.id.clone(),
                "admission_receipt_ref does not match admitted receipt",
            ));
        }

        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut state = self.state.lock();
        state.record_call(action);

        match action {
            "attach_session" => {
                // attachment.rs:16-20 requires nonblank text fields
                let agent_id = args
                    .get("agent_identity_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let work_claim_id = args
                    .get("work_claim_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if agent_id.trim().is_empty() || work_claim_id.trim().is_empty() {
                    return Ok(Self::make_tool_error(
                        request.id.clone(),
                        "attach_session missing required binding fields",
                    ));
                }
                let attachment_id = format!("att-test-{}", state.attachments.len() + 1);
                state.attachments.insert(attachment_id.clone());
                state.attachment_bindings.insert(
                    (
                        args.get("adapter_connection_identity")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        args.get("remote_session_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    ),
                    attachment_id.clone(),
                );
                let receipt = json!({
                    "status": "completed",
                    "action": "attach_session",
                    "attachment_id": attachment_id,
                });
                Ok(Self::make_tool_success(request.id.clone(), receipt))
            }

            "advertise_session_capabilities" => {
                state.advertisement_seq += 1;
                let requested = args
                    .get("session_capabilities")
                    .and_then(Value::as_array)
                    .expect("capability names");
                let caps: serde_json::Map<String, Value> = [
                    "observe",
                    "wait",
                    "prompt",
                    "cancel",
                    "resume",
                    "load",
                    "events",
                    "artifacts",
                ]
                .into_iter()
                .map(|name| {
                    (
                        name.to_string(),
                        json!(
                            requested
                                .iter()
                                .any(|value| value.as_str().is_some_and(|v| v.trim() == name))
                        ),
                    )
                })
                .collect();
                let mut receipt = json!({
                    "status": "completed",
                    "action": "advertise_session_capabilities",
                    "advertisement_seq": state.advertisement_seq,
                    "session_capabilities": caps,
                });
                if matches!(
                    state.corruption,
                    ResponseCorruption::MissingAdvertisementSeq
                ) {
                    state.corruption = ResponseCorruption::None;
                    receipt.as_object_mut().unwrap().remove("advertisement_seq");
                }
                match std::mem::take(&mut state.corruption) {
                    ResponseCorruption::MissingCapabilities => {
                        receipt
                            .as_object_mut()
                            .unwrap()
                            .remove("session_capabilities");
                    }
                    ResponseCorruption::StringCapability => {
                        receipt["session_capabilities"]["observe"] = json!("true");
                    }
                    ResponseCorruption::ArrayCapabilities => {
                        receipt["session_capabilities"] = json!(["observe"]);
                    }
                    _ => {}
                }
                Ok(Self::make_tool_success(request.id.clone(), receipt))
            }

            "ingest_session_event" => {
                let attachment_id = match args.get("attachment_id").and_then(Value::as_str) {
                    Some(s)
                        if !s.trim().is_empty()
                            && s.chars().count() <= 128
                            && !s.chars().any(|c| c.is_control()) =>
                    {
                        s.to_string()
                    }
                    _ => {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "invalid attachment_id",
                        ));
                    }
                };
                let event_id = match args.get("session_event_id").and_then(Value::as_str) {
                    Some(s)
                        if !s.trim().is_empty()
                            && s.chars().count() <= 128
                            && !s.chars().any(|c| c.is_control()) =>
                    {
                        s.to_string()
                    }
                    _ => {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "invalid session_event_id",
                        ));
                    }
                };
                let kind = match args.get("session_event_kind").and_then(Value::as_str) {
                    Some(
                        k @ ("accepted" | "started" | "progress" | "input_required" | "terminal"
                        | "cleanup"),
                    ) => k.to_string(),
                    _ => {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "unknown session_event_kind",
                        ));
                    }
                };
                let outcome = args
                    .get("session_event_outcome")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if kind == "terminal" {
                    if outcome.is_none() {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "terminal event must carry outcome",
                        ));
                    }
                } else if outcome.is_some() {
                    return Ok(Self::make_tool_error(
                        request.id.clone(),
                        "non-terminal event must not carry outcome",
                    ));
                }

                let source_rev = match args.get("source_revision").and_then(Value::as_i64) {
                    Some(rev) if rev >= 0 => rev,
                    _ => {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "invalid source_revision",
                        ));
                    }
                };

                let auth_ref = args
                    .get("authority_confirmation_ref")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                if let Some(r) = &auth_ref {
                    if r.trim().is_empty()
                        || r.chars().count() > 128
                        || r.chars().any(|c| c.is_control())
                    {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "invalid authority_confirmation_ref",
                        ));
                    }
                }

                // Writer rejects any char::is_control and requires nonempty <=2000 chars
                let summary = args
                    .get("event_summary")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                if let Some(s) = &summary {
                    if s.chars().count() > SUMMARY_CEILING
                        || s.chars().any(|c| c.is_control())
                        || s.is_empty()
                    {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "writer rejects control characters, blank, or oversize summary",
                        ));
                    }
                }

                let digest = args
                    .get("payload_digest")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                if let Some(d) = &digest {
                    if d.chars().count() > 128
                        || !d.chars().all(|c| {
                            c.is_ascii_alphanumeric()
                                || matches!(c, '-' | '_' | '=' | '+' | '/' | ':')
                        })
                    {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "invalid payload_digest",
                        ));
                    }
                }

                if outcome
                    .as_deref()
                    .is_some_and(|value| !matches!(value, "completed" | "failed" | "cancelled"))
                {
                    return Ok(Self::make_tool_error(
                        request.id.clone(),
                        "invalid terminal outcome",
                    ));
                }
                let occurred_at = match args.get("event_occurred_at").and_then(Value::as_str) {
                    Some(t)
                        if !t.trim().is_empty()
                            && t.chars().count() <= 64
                            && !t.chars().any(|c| c.is_control()) =>
                    {
                        t.to_string()
                    }
                    _ => {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "invalid event_occurred_at",
                        ));
                    }
                };

                if chrono::DateTime::parse_from_rfc3339(&occurred_at).is_err() {
                    return Ok(Self::make_tool_error(
                        request.id.clone(),
                        "invalid RFC3339 timestamp",
                    ));
                }

                // Cancel outcome requires matching recorded accepted cancel intervention before both first ingest and replay
                if outcome.as_deref() == Some("cancelled") {
                    let has_match = match &auth_ref {
                        Some(r) => state
                            .cancel_confirmations
                            .get(&attachment_id)
                            .is_some_and(|set| set.contains(r)),
                        None => false,
                    };
                    if !has_match {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "cancel requires matching recorded accepted request_cancel intervention result",
                        ));
                    }
                }

                let incoming = StoredEvent {
                    attachment_id: attachment_id.clone(),
                    session_event_id: event_id.clone(),
                    session_event_kind: kind.clone(),
                    session_event_outcome: outcome.clone(),
                    source_revision: source_rev,
                    authority_confirmation_ref: auth_ref,
                    event_summary: summary,
                    payload_digest: digest,
                    event_occurred_at: occurred_at,
                };

                let key = (attachment_id.clone(), event_id.clone());
                let mut disposition = "advanced";
                let admission = if let Some(existing) = state.stored_events.get(&key) {
                    if !existing.same_material(&incoming) {
                        return Ok(Self::make_tool_error(
                            request.id.clone(),
                            "WorkClaimConflict: changed same-ID material for existing event",
                        ));
                    }
                    // Exact replay does NOT insert a row and does not advance revision
                    "replayed"
                } else {
                    state.stored_events.insert(key, incoming);
                    let proj = state.projections.entry(attachment_id.clone()).or_default();
                    // Mirror the pinned reducer's source-revision high-water and rank laws
                    // (harness_session_events.rs:930-1089); replay never calls this branch.
                    let current_rank = state_rank(proj.canonical_state.as_deref());
                    let event_rank = match kind.as_str() {
                        "accepted" => 0,
                        "started" => 1,
                        "progress" | "input_required" => 2,
                        "terminal" => 3,
                        "cleanup" => 4,
                        _ => unreachable!(),
                    };
                    let stale_early = (current_rank < 0
                        && (source_rev < proj.revision || event_rank < proj.pre_disconnect_rank))
                        || (current_rank >= 0
                            && kind == "terminal"
                            && proj.terminal_outcome.is_none()
                            && source_rev < proj.revision);
                    if stale_early {
                        disposition = "journaled_stale";
                    } else {
                        if kind == "terminal" {
                            if let Some(recorded) = &proj.terminal_outcome {
                                if Some(recorded) == outcome.as_ref() {
                                    disposition = "journaled_redundant_terminal";
                                } else {
                                    proj.canonical_state =
                                        Some("inconsistent_reconciling".to_string());
                                    proj.conflicting_terminal = true;
                                    disposition = "journaled_terminal_conflict";
                                }
                            } else {
                                proj.terminal_outcome = outcome.clone();
                                proj.canonical_state = outcome.clone();
                            }
                        } else if kind == "cleanup" && event_rank > current_rank {
                            if current_rank == 3 {
                                proj.cleanup_recorded = true;
                            } else {
                                disposition = "journaled_stale";
                            }
                        } else {
                            let target = if kind == "progress" {
                                "progressing"
                            } else {
                                kind.as_str()
                            };
                            let equal_flip = event_rank == 2
                                && current_rank == 2
                                && source_rev >= proj.revision
                                && proj.canonical_state.as_deref() != Some(target);
                            if event_rank > current_rank || equal_flip {
                                proj.canonical_state = Some(target.to_string());
                            } else {
                                disposition = "journaled_stale";
                            }
                        }
                        if proj.canonical_state.is_some() {
                            proj.revision = proj.revision.max(source_rev);
                            proj.last_event_id = Some(event_id.clone());
                        }
                    }
                    "journaled"
                };

                if state.drop_next_ingest_response {
                    state.drop_next_ingest_response = false;
                    return Err(anyhow::anyhow!("transport connection closed abruptly"));
                }

                let mut ret_att = attachment_id.clone();
                let mut ret_eid = event_id;
                let mut ret_admission = admission.to_string();
                let mut ret_disposition = disposition.to_string();
                let mut ret_state = state.canonical_state_object(&attachment_id);

                match std::mem::take(&mut state.corruption) {
                    ResponseCorruption::None => {}
                    ResponseCorruption::MismatchedEventId(wrong) => ret_eid = wrong,
                    ResponseCorruption::MismatchedAttachmentId(wrong) => ret_att = wrong,
                    ResponseCorruption::UnknownAdmission(wrong) => ret_admission = wrong,
                    ResponseCorruption::UnknownDisposition(wrong) => ret_disposition = wrong,
                    ResponseCorruption::NullCanonicalState => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("canonical_state".to_string(), Value::Null);
                    }
                    ResponseCorruption::EmptyLastEventId => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("last_event_id".to_string(), json!(""));
                    }
                    ResponseCorruption::MissingCanonicalStateField => {
                        ret_state.as_object_mut().unwrap().remove("canonical_state");
                    }
                    ResponseCorruption::MissingCanonicalRevision => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .remove("canonical_revision");
                    }
                    ResponseCorruption::NegativeCanonicalRevision => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("canonical_revision".to_string(), json!(-1));
                    }
                    ResponseCorruption::StringCanonicalRevision => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("canonical_revision".to_string(), json!("zero"));
                    }
                    ResponseCorruption::MissingCleanupRecorded => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .remove("cleanup_recorded");
                    }
                    ResponseCorruption::StringCleanupRecorded => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("cleanup_recorded".to_string(), json!("false"));
                    }
                    ResponseCorruption::MissingConflictingTerminal => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .remove("conflicting_terminal");
                    }
                    ResponseCorruption::StringConflictingTerminal => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("conflicting_terminal".to_string(), json!("false"));
                    }
                    ResponseCorruption::MissingLastEventId => {
                        ret_state.as_object_mut().unwrap().remove("last_event_id");
                    }
                    ResponseCorruption::UnknownCanonicalState(wrong) => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("canonical_state".to_string(), json!(wrong));
                    }
                    _ => {}
                }

                let receipt = json!({
                    "status": "completed",
                    "action": "ingest_session_event",
                    "attachment_id": ret_att,
                    "event_id": ret_eid,
                    "admission": ret_admission,
                    "disposition": ret_disposition,
                    "canonical_state": ret_state,
                });
                Ok(Self::make_tool_success(request.id.clone(), receipt))
            }

            "reconnect_session" => {
                let binding = (
                    args.get("adapter_connection_identity")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    args.get("remote_session_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                );
                let Some(attachment_id) = state.attachment_bindings.get(&binding).cloned() else {
                    return Ok(Self::make_tool_error(
                        request.id.clone(),
                        "unknown attachment binding",
                    ));
                };
                let proj = state
                    .projections
                    .get(&attachment_id)
                    .cloned()
                    .unwrap_or_default();
                let mut receipt = json!({
                    "status": "completed",
                    "action": "reconnect_session",
                    "attachment_id": attachment_id,
                    "attachment_state": "attached",
                    "previous_attachment_state": "unknown",
                    "reconnected": true,
                    "resume_from_revision": proj.revision,
                    "canonical_state": state.canonical_state_object(&attachment_id),
                });
                match std::mem::take(&mut state.corruption) {
                    ResponseCorruption::MissingReconnectField => {
                        receipt.as_object_mut().unwrap().remove("reconnected");
                    }
                    _ => {}
                }
                Ok(Self::make_tool_success(request.id.clone(), receipt))
            }

            "get_session_state" => {
                let attachment_id = args
                    .get("attachment_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut ret_state = state.canonical_state_object(attachment_id);
                match std::mem::take(&mut state.corruption) {
                    ResponseCorruption::NullCanonicalState => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("canonical_state".to_string(), Value::Null);
                    }
                    ResponseCorruption::EmptyLastEventId => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("last_event_id".to_string(), json!(""));
                    }
                    ResponseCorruption::MissingCanonicalStateField => {
                        ret_state.as_object_mut().unwrap().remove("canonical_state");
                    }
                    ResponseCorruption::MissingCanonicalRevision => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .remove("canonical_revision");
                    }
                    ResponseCorruption::NegativeCanonicalRevision => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("canonical_revision".to_string(), json!(-1));
                    }
                    ResponseCorruption::StringCanonicalRevision => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("canonical_revision".to_string(), json!("zero"));
                    }
                    ResponseCorruption::MissingCleanupRecorded => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .remove("cleanup_recorded");
                    }
                    ResponseCorruption::StringCleanupRecorded => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("cleanup_recorded".to_string(), json!("false"));
                    }
                    ResponseCorruption::MissingConflictingTerminal => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .remove("conflicting_terminal");
                    }
                    ResponseCorruption::StringConflictingTerminal => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("conflicting_terminal".to_string(), json!("false"));
                    }
                    ResponseCorruption::MissingLastEventId => {
                        ret_state.as_object_mut().unwrap().remove("last_event_id");
                    }
                    ResponseCorruption::UnknownCanonicalState(wrong) => {
                        ret_state
                            .as_object_mut()
                            .unwrap()
                            .insert("canonical_state".to_string(), json!(wrong));
                    }
                    _ => {}
                }
                let receipt = json!({
                    "status": "completed",
                    "action": "get_session_state",
                    "attachment_id": attachment_id,
                    "canonical_state": ret_state,
                });
                Ok(Self::make_tool_success(request.id.clone(), receipt))
            }

            "mark_session_connection" => {
                let attachment_id = args
                    .get("attachment_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let proj = state
                    .projections
                    .entry(attachment_id.to_string())
                    .or_default();
                if state_rank(proj.canonical_state.as_deref()) != 3 {
                    proj.pre_disconnect_rank = state_rank(proj.canonical_state.as_deref());
                    proj.canonical_state = Some("unknown_orphaned".to_string());
                }
                let receipt = json!({
                    "status": "completed",
                    "action": "mark_session_connection",
                    "attachment_id": attachment_id,
                });
                Ok(Self::make_tool_success(request.id.clone(), receipt))
            }

            "request_intervention" => {
                let attachment_id = args
                    .get("attachment_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if args.get("intervention_kind").and_then(Value::as_str) == Some("request_cancel") {
                    let request_id = args
                        .get("intervention_request_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    state
                        .cancel_requests
                        .insert((attachment_id.to_string(), request_id.to_string()));
                }
                let receipt = json!({
                    "status": "completed",
                    "action": "request_intervention",
                    "admission": "created",
                    "request": {
                        "attachment_id": attachment_id,
                        "request_id": args.get("intervention_request_id"),
                    },
                });
                Ok(Self::make_tool_success(request.id.clone(), receipt))
            }

            "record_intervention_result" => {
                let attachment_id = args
                    .get("attachment_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let disposition = args
                    .get("intervention_disposition")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let auth_ref = args
                    .get("authority_confirmation_ref")
                    .and_then(Value::as_str);
                let request_id = args
                    .get("intervention_request_id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if disposition == "accepted"
                    && state
                        .cancel_requests
                        .contains(&(attachment_id.to_string(), request_id.to_string()))
                {
                    if let Some(r) = auth_ref {
                        state
                            .cancel_confirmations
                            .entry(attachment_id.to_string())
                            .or_default()
                            .insert(r.to_string());
                    }
                }
                let receipt = json!({
                    "status": "completed",
                    "action": "record_intervention_result",
                    "admission": "created",
                    "result": {
                        "attachment_id": attachment_id,
                        "request_id": args.get("intervention_request_id"),
                    },
                });
                Ok(Self::make_tool_success(request.id.clone(), receipt))
            }

            other => Ok(Self::make_tool_error(
                request.id.clone(),
                &format!("unsupported action {other}"),
            )),
        }
    }

    async fn close(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Test Harness Helpers
// ─────────────────────────────────────────────────────────────────────────

fn test_sink_config() -> TachiFactSinkConfig {
    TachiFactSinkConfig {
        command: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/bin/sh")),
        args: vec!["serve".to_string()],
        env: HashMap::new(),
        host_identity: "test-host".to_string(),
        agent_identity_id: "test-agent".to_string(),
        admission_receipt_ref: "test-receipt-ref".to_string(),
        work_claim_id: "test-claim-1".to_string(),
        expected_transition_revision: 0,
        contract_digest: "test-digest".to_string(),
        tool_profile: "delegate".to_string(),
        capability_class: "tachi".to_string(),
        protocol_version: 1,
        call_timeout: Duration::from_secs(5),
    }
}

fn test_binding() -> SessionBinding {
    SessionBinding {
        host_identity: HostIdentityRef::from_opaque("test-host"),
        adapter_connection: AdapterConnectionRef::from_opaque("conn-1"),
        remote_session: RemoteSessionRef::from_opaque("session-1"),
        idempotency_key: "idem-1".to_string(),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 1: Summary Projection & Text Rules
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn summary_projection_empty_and_none() {
    assert_eq!(project_summary(None).unwrap(), None);
    assert_eq!(project_summary(Some("")).unwrap(), None);
}

#[test]
fn summary_projection_all_controls_refused() {
    assert!(project_summary(Some("\r\n\t")).is_err());
    assert!(project_summary(Some("\x01\x02\x1f")).is_err());
}

#[test]
fn summary_projection_blank_refused() {
    assert!(project_summary(Some("   ")).is_err());
    assert!(project_summary(Some("\t  \r")).is_err());
}

#[test]
fn summary_projection_prohibited_nul_refused() {
    assert!(project_summary(Some("hello\0world")).is_err());
}

#[test]
fn summary_projection_prohibited_c1_refused() {
    assert!(project_summary(Some("hello\u{0085}world")).is_err());
    assert!(project_summary(Some("test\u{009f}val")).is_err());
}

#[test]
fn summary_projection_multiline_crlf_tab_replaced() {
    assert_eq!(
        project_summary(Some("hello\nworld")).unwrap(),
        Some("hello world".to_string())
    );
    assert_eq!(
        project_summary(Some("step 1\r\nstep 2\ttab")).unwrap(),
        Some("step 1  step 2 tab".to_string())
    );
}

#[test]
fn summary_projection_multibyte_boundary_safe() {
    let emoji = "🦀"; // 4 bytes, 1 Unicode scalar character
    let long_emojis: String = std::iter::repeat(emoji).take(2005).collect();
    let projected = project_summary(Some(&long_emojis)).unwrap().unwrap();
    assert_eq!(projected.chars().count(), 2000);
    assert!(projected.ends_with("🦀"));
}

#[test]
fn summary_projection_preserves_redaction() {
    assert_eq!(
        project_summary(Some("[REDACTED: secret credentials]")).unwrap(),
        Some("[REDACTED: secret credentials]".to_string())
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 2: Input Field Bounds & Validators
// ─────────────────────────────────────────────────────────────────────────

#[test]
fn event_id_validation_empty_oversize_controls() {
    assert!(validate_event_id("   ").is_err());
    assert!(validate_event_id("").is_err());
    assert!(validate_event_id(&"a".repeat(129)).is_err());
    assert!(validate_event_id("evt\x01id").is_err());
    assert!(validate_event_id("valid-event-id-123").is_ok());
}

#[test]
fn attachment_id_validation_empty_oversize_controls() {
    assert!(validate_attachment_id("   ").is_err());
    assert!(validate_attachment_id("").is_err());
    assert!(validate_attachment_id(&"a".repeat(129)).is_err());
    assert!(validate_attachment_id("att\x01id").is_err());
    assert!(validate_attachment_id("att-test-01").is_ok());
}

#[test]
fn payload_digest_validation_allowed_chars_and_length() {
    assert_eq!(validate_and_project_payload_digest(None).unwrap(), None);
    assert_eq!(validate_and_project_payload_digest(Some("")).unwrap(), None);
    assert!(validate_and_project_payload_digest(Some("   ")).is_err());
    assert!(validate_and_project_payload_digest(Some(&"a".repeat(129))).is_err());
    assert!(validate_and_project_payload_digest(Some("bad char space")).is_err());
    assert!(validate_and_project_payload_digest(Some("bad$char")).is_err());
    assert_eq!(
        validate_and_project_payload_digest(Some("sha256:abc-123_456=+/:")).unwrap(),
        Some("sha256:abc-123_456=+/:".to_string())
    );
}

#[test]
fn authority_confirmation_ref_validation_bounds() {
    assert_eq!(validate_and_project_confirmation_ref(None).unwrap(), None);
    assert_eq!(
        validate_and_project_confirmation_ref(Some("")).unwrap(),
        None
    );
    assert!(validate_and_project_confirmation_ref(Some("   ")).is_err());
    assert!(validate_and_project_confirmation_ref(Some(&"a".repeat(129))).is_err());
    assert!(validate_and_project_confirmation_ref(Some("ctrl\x00ref")).is_err());
    assert_eq!(
        validate_and_project_confirmation_ref(Some("conf-ref-123")).unwrap(),
        Some("conf-ref-123".to_string())
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 3: Meaningful Replay After Injected Clock Advance
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_meaningful_replay_after_injected_clock_advance() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let clock_time = Arc::new(Mutex::new("2026-09-12T00:00:00Z".to_string()));
    let clock_clone = clock_time.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        })
        .with_clock(move || clock_clone.lock().clone());

    let binding = test_binding();
    let att = sink
        .attach(&binding, &["observe".to_string()])
        .await
        .unwrap();

    let fact = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-advance-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("started run".to_string()),
        payload_digest: None,
    };

    // First ingest at T0
    let rec1 = sink.ingest_event(&att, &fact).await.unwrap();
    assert_eq!(rec1.admission, SessionReceiptAdmissionV1::Created);
    assert_eq!(rec1.disposition, "advanced");
    assert_eq!(fixture_state.lock().stored_events_count(), 1);
    assert_eq!(fixture_state.lock().call_count("ingest_session_event"), 1);

    // Inject clock advance by 15 seconds (no blocking sleep)
    *clock_time.lock() = "2026-09-12T00:00:15Z".to_string();

    // Replay unchanged fact: must reuse frozen T0 occurred_at timestamp and accept replayed+advanced
    let rec2 = sink.ingest_event(&att, &fact).await.unwrap();
    assert_eq!(rec2.admission, SessionReceiptAdmissionV1::Replayed);
    assert_eq!(rec2.disposition, "advanced");
    // Stored unique events count does NOT increase
    assert_eq!(fixture_state.lock().stored_events_count(), 1);
    assert_eq!(fixture_state.lock().call_count("ingest_session_event"), 2);

    // Verify stored event preserved T0
    let stored = fixture_state
        .lock()
        .stored_events
        .get(&(att.as_str().to_string(), "evt-advance-1".to_string()))
        .cloned()
        .unwrap();
    assert_eq!(stored.event_occurred_at, "2026-09-12T00:00:00Z");

    // Next NEW event should receive the advanced clock time
    let fact2 = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-advance-2"),
        kind: SessionEventKindV1::Progress,
        outcome: None,
        source_revision: 2,
        authority_confirmation_ref: None,
        summary: Some("progress made".to_string()),
        payload_digest: None,
    };
    let rec3 = sink.ingest_event(&att, &fact2).await.unwrap();
    assert_eq!(rec3.admission, SessionReceiptAdmissionV1::Created);
    assert_eq!(fixture_state.lock().stored_events_count(), 2);
    assert_eq!(fixture_state.lock().call_count("ingest_session_event"), 3);

    let stored2 = fixture_state
        .lock()
        .stored_events
        .get(&(att.as_str().to_string(), "evt-advance-2".to_string()))
        .cloned()
        .unwrap();
    assert_eq!(stored2.event_occurred_at, "2026-09-12T00:00:15Z");
    let current = sink.ingest_event(&att, &fact).await.unwrap();
    assert_eq!(current.admission, SessionReceiptAdmissionV1::Replayed);
    assert_eq!(
        current.state.canonical_state,
        SessionCanonicalStateV1::Progressing
    );
    assert_eq!(current.state.canonical_revision, 2);
    assert_eq!(fixture_state.lock().stored_events_count(), 2);
    assert_eq!(fixture_state.lock().call_count("attach_session"), 1);
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 4: Lost Response Then Internal Retry Preserves Envelope
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_lost_response_then_internal_retry_preserves_envelope() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let clock_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let clock_counter = clock_calls.clone();
    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        })
        .with_clock(move || {
            let tick = clock_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            format!("2026-09-12T00:00:{tick:02}Z")
        });

    let binding = test_binding();
    let att = sink
        .attach(&binding, &["observe".to_string()])
        .await
        .unwrap();

    let fact = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-retry-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("initial start".to_string()),
        payload_digest: None,
    };

    // Instruct fixture to simulate transport failure on first attempt after recording
    fixture_state.lock().drop_next_ingest_response = true;

    // Sink should catch transport failure, reconnect, and retry with identical envelope
    let receipt = sink
        .ingest_event(&att, &fact)
        .await
        .expect("lost-response internal retry succeeds");
    assert_eq!(receipt.admission, SessionReceiptAdmissionV1::Replayed);
    assert_eq!(receipt.disposition, "advanced");

    // Call count is 2 (first attempt + 1 retry), but stored unique events is 1
    assert_eq!(fixture_state.lock().call_count("ingest_session_event"), 2);
    assert_eq!(fixture_state.lock().stored_events_count(), 1);
    assert_eq!(clock_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 5: Disconnect / Reconnect Replay Unchanged Envelope
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_reconnect_then_replay_unchanged_envelope() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let binding = test_binding();
    let att = sink
        .attach(&binding, &["observe".to_string()])
        .await
        .unwrap();

    let fact = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-recon-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("recon start".to_string()),
        payload_digest: None,
    };

    let rec1 = sink.ingest_event(&att, &fact).await.unwrap();
    assert_eq!(rec1.admission, SessionReceiptAdmissionV1::Created);
    assert_eq!(rec1.disposition, "advanced");
    assert_eq!(fixture_state.lock().stored_events_count(), 1);

    // Mark connection dropped and reconnect
    sink.mark_connection(&att, SessionConnectionFactV1::Disconnected)
        .await
        .unwrap();
    let recon = sink.reconnect(&binding).await.unwrap();
    assert!(recon.reconnected);
    assert_eq!(recon.attachment_ref, att);
    assert_eq!(
        recon.state.canonical_state,
        SessionCanonicalStateV1::UnknownOrphaned
    );

    // Replay unchanged fact post-reconnect
    let rec2 = sink.ingest_event(&att, &fact).await.unwrap();
    assert_eq!(rec2.admission, SessionReceiptAdmissionV1::Replayed);
    assert_eq!(rec2.disposition, "advanced");
    assert_eq!(
        rec2.state.canonical_state,
        SessionCanonicalStateV1::UnknownOrphaned
    );
    assert_eq!(fixture_state.lock().stored_events_count(), 1);
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 6: Changed Same-ID Material Refused
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_changed_material_for_existing_id_refused() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let binding = test_binding();
    let att = sink
        .attach(&binding, &["observe".to_string()])
        .await
        .unwrap();

    let fact1 = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-immutable-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("original material".to_string()),
        payload_digest: None,
    };

    sink.ingest_event(&att, &fact1).await.unwrap();

    // Changed summary for same event_id
    let mut fact2 = fact1.clone();
    fact2.summary = Some("changed material summary".to_string());
    let err = sink.ingest_event(&att, &fact2).await.unwrap_err();
    match err {
        SessionFactError::Refused(msg) => {
            assert!(msg.contains("cannot change frozen event material"));
        }
        other => panic!("expected Refused, got {other:?}"),
    }

    // Changed revision for same event_id
    let mut fact3 = fact1.clone();
    fact3.source_revision = 2;
    let err3 = sink.ingest_event(&att, &fact3).await.unwrap_err();
    match err3 {
        SessionFactError::Refused(msg) => {
            assert!(msg.contains("cannot change frozen event material"));
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 7: Per-Attachment Isolation For Same Event ID
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_per_attachment_isolation_for_same_event_id() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let att_a = SessionAttachmentRef::from_opaque("att-alpha");
    let att_b = SessionAttachmentRef::from_opaque("att-beta");

    let fact_a = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-common-01"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("alpha started".to_string()),
        payload_digest: None,
    };

    let fact_b = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-common-01"),
        kind: SessionEventKindV1::Progress,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("beta progress".to_string()),
        payload_digest: None,
    };

    let rec_a = sink.ingest_event(&att_a, &fact_a).await.unwrap();
    assert_eq!(rec_a.admission, SessionReceiptAdmissionV1::Created);
    assert_eq!(
        rec_a.state.canonical_state,
        SessionCanonicalStateV1::Started
    );
    assert_eq!(rec_a.state.canonical_revision, 1);

    let rec_b = sink.ingest_event(&att_b, &fact_b).await.unwrap();
    assert_eq!(rec_b.admission, SessionReceiptAdmissionV1::Created);
    assert_eq!(
        rec_b.state.canonical_state,
        SessionCanonicalStateV1::Progressing
    );
    assert_eq!(rec_b.state.canonical_revision, 1);

    // Fixture stored two distinct rows
    assert_eq!(fixture_state.lock().stored_events_count(), 2);

    // Replay on att_a returns att_a's state (Started, rev 1), NOT att_b's state
    let rec_a_replay = sink.ingest_event(&att_a, &fact_a).await.unwrap();
    assert_eq!(rec_a_replay.admission, SessionReceiptAdmissionV1::Replayed);
    assert_eq!(
        rec_a_replay.state.canonical_state,
        SessionCanonicalStateV1::Started
    );
    assert_eq!(rec_a_replay.state.canonical_revision, 1);
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 8: Bounded Envelope Capacity Fails Closed
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_bounded_envelope_capacity_fails_closed() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        })
        .with_max_envelopes_per_attachment(2);

    let att = SessionAttachmentRef::from_opaque("att-bound-test");

    let f1 = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-cap-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("e1".to_string()),
        payload_digest: None,
    };
    let f2 = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-cap-2"),
        kind: SessionEventKindV1::Progress,
        outcome: None,
        source_revision: 2,
        authority_confirmation_ref: None,
        summary: Some("e2".to_string()),
        payload_digest: None,
    };
    let f3 = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-cap-3"),
        kind: SessionEventKindV1::Progress,
        outcome: None,
        source_revision: 3,
        authority_confirmation_ref: None,
        summary: Some("e3".to_string()),
        payload_digest: None,
    };

    assert!(sink.ingest_event(&att, &f1).await.is_ok());
    assert!(sink.ingest_event(&att, &f2).await.is_ok());

    // 3rd event exceeds per-attachment capacity: must fail closed, NOT evict existing entries
    let err = sink.ingest_event(&att, &f3).await.unwrap_err();
    match err {
        SessionFactError::Refused(msg) => {
            assert!(msg.contains("capacity exhausted (fail closed)"));
        }
        other => panic!("expected Refused capacity exhaustion, got {other:?}"),
    }

    // Replay of existing event 1 must still succeed (was not evicted)
    assert!(sink.ingest_event(&att, &f1).await.is_ok());
}

#[tokio::test]
async fn test_bounded_total_envelopes_capacity_fails_closed() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        })
        .with_max_envelopes_per_attachment(10)
        .with_max_total_envelopes(2);

    let att1 = SessionAttachmentRef::from_opaque("att-tot-1");
    let att2 = SessionAttachmentRef::from_opaque("att-tot-2");
    let att3 = SessionAttachmentRef::from_opaque("att-tot-3");

    let f1 = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("e1".to_string()),
        payload_digest: None,
    };
    let f2 = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-2"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("e2".to_string()),
        payload_digest: None,
    };
    let f3 = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-3"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: Some("e3".to_string()),
        payload_digest: None,
    };

    assert!(sink.ingest_event(&att1, &f1).await.is_ok());
    assert!(sink.ingest_event(&att2, &f2).await.is_ok());

    // 3rd event across attachments exceeds total capacity (2): fails closed
    let err = sink.ingest_event(&att3, &f3).await.unwrap_err();
    match err {
        SessionFactError::Refused(msg) => {
            assert!(msg.contains("total capacity exhausted (fail closed)"));
        }
        other => panic!("expected Refused total capacity, got {other:?}"),
    }

    // Replay on att1 still succeeds (was not evicted)
    assert!(sink.ingest_event(&att1, &f1).await.is_ok());
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 9: Pre-Event Null Canonical State Incompatibility
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_parse_state_legitimate_null_canonical_state_typed_incompatibility() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let att = SessionAttachmentRef::from_opaque("att-null-state-test");

    // Inject null canonical_state
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::NullCanonicalState);

    let err = sink.get_state(&att).await.unwrap_err();
    match err {
        SessionFactError::Refused(msg) => {
            assert!(msg.contains("pre-event state unrepresentable in SessionStateView API"));
        }
        other => panic!("expected honest incompatibility Refused, got {other:?}"),
    }
}

#[tokio::test]
async fn test_parse_state_empty_last_event_id_refused() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let att = SessionAttachmentRef::from_opaque("att-empty-last-id-test");
    fixture_state.lock().projections.insert(
        att.as_str().to_string(),
        AttachmentSpineProjection {
            canonical_state: Some("started".to_string()),
            ..Default::default()
        },
    );
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::EmptyLastEventId);
    let err = sink.get_state(&att).await.unwrap_err();
    match err {
        SessionFactError::Refused(msg) => {
            assert!(msg.contains("last_event_id is invalid"));
        }
        other => panic!("expected Refused invalid last_event_id, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 10: Missing, Wrong-Typed, and Unknown Receipt Fields
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_parse_state_missing_and_wrong_typed_fields() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let att = SessionAttachmentRef::from_opaque("att-strict-test");
    fixture_state.lock().projections.insert(
        att.as_str().to_string(),
        AttachmentSpineProjection {
            canonical_state: Some("started".to_string()),
            ..Default::default()
        },
    );
    assert!(sink.get_state(&att).await.is_ok());

    // 1. Missing canonical_state field
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MissingCanonicalStateField);
    assert!(sink.get_state(&att).await.is_err());

    // 2. Missing canonical_revision
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MissingCanonicalRevision);
    assert!(sink.get_state(&att).await.is_err());

    // 3. Negative canonical_revision
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::NegativeCanonicalRevision);
    assert!(sink.get_state(&att).await.is_err());

    // 4. String canonical_revision
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::StringCanonicalRevision);
    assert!(sink.get_state(&att).await.is_err());

    // 5. Missing cleanup_recorded
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MissingCleanupRecorded);
    assert!(sink.get_state(&att).await.is_err());

    // 6. String cleanup_recorded
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::StringCleanupRecorded);
    assert!(sink.get_state(&att).await.is_err());

    // 7. Missing conflicting_terminal
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MissingConflictingTerminal);
    assert!(sink.get_state(&att).await.is_err());

    // 8. String conflicting_terminal
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::StringConflictingTerminal);
    assert!(sink.get_state(&att).await.is_err());

    // 9. Missing last_event_id
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MissingLastEventId);
    assert!(sink.get_state(&att).await.is_err());

    // 10. Unknown canonical_state
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::UnknownCanonicalState(
            "hovering".to_string(),
        ));
    let err_unknown_state = sink.get_state(&att).await.unwrap_err();
    assert!(matches!(err_unknown_state, SessionFactError::Refused(_)));
}

#[tokio::test]
async fn test_unknown_admission_and_disposition_refused() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let att = SessionAttachmentRef::from_opaque("att-disposition-test");
    let fact = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-dispo-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: None,
        payload_digest: None,
    };

    // Unknown admission class: "created" must be rejected on consumer wire
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::UnknownAdmission("created".to_string()));
    let err_adm = sink.ingest_event(&att, &fact).await.unwrap_err();
    assert!(matches!(err_adm, SessionFactError::Refused(_)));

    // Unknown disposition
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::UnknownDisposition(
            "in_limbo".to_string(),
        ));
    let err_disp = sink.ingest_event(&att, &fact).await.unwrap_err();
    assert!(matches!(err_disp, SessionFactError::Refused(_)));
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 11: Response ID Mismatch
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_response_id_mismatch() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let att = SessionAttachmentRef::from_opaque("att-mismatch-test");
    let fact = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-mismatch-1"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 1,
        authority_confirmation_ref: None,
        summary: None,
        payload_digest: None,
    };

    // Event ID mismatch
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MismatchedEventId(
            "evt-mismatch-WRONG".to_string(),
        ));
    let err_eid = sink.ingest_event(&att, &fact).await.unwrap_err();
    match err_eid {
        SessionFactError::Refused(msg) => assert!(msg.contains("event_id mismatch")),
        other => panic!("expected Refused event_id mismatch, got {other:?}"),
    }

    // Attachment ID mismatch
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MismatchedAttachmentId(
            "att-WRONG".to_string(),
        ));
    let err_att = sink.ingest_event(&att, &fact).await.unwrap_err();
    match err_att {
        SessionFactError::Refused(msg) => assert!(msg.contains("attachment_id mismatch")),
        other => panic!("expected Refused attachment_id mismatch, got {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 12: Action Specific Receipt Shapes
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_advertise_success_missing_seq_refused_no_attachment_id_requirement() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let att = SessionAttachmentRef::from_opaque("att-adv-test");

    // Normal advertise: receipt does NOT have attachment_id, should succeed
    let rec = sink
        .advertise_capabilities(&att, &["observe".to_string()])
        .await
        .unwrap();
    assert_eq!(rec.advertisement_seq, 1);
    assert_eq!(rec.capabilities, vec!["observe"]);
    for fault in [
        ResponseCorruption::MissingCapabilities,
        ResponseCorruption::StringCapability,
        ResponseCorruption::ArrayCapabilities,
    ] {
        fixture_state.lock().inject_fault(fault);
        assert!(
            sink.advertise_capabilities(&att, &["observe".to_string()])
                .await
                .is_err()
        );
    }
    let empty = sink.advertise_capabilities(&att, &[]).await.unwrap();
    assert!(empty.capabilities.is_empty());

    // Missing advertisement_seq in receipt must be refused, not defaulted to 0
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MissingAdvertisementSeq);
    let err = sink
        .advertise_capabilities(&att, &["observe".to_string()])
        .await
        .unwrap_err();
    assert!(matches!(err, SessionFactError::Refused(_)));
}

#[tokio::test]
async fn test_reconnect_success_missing_fields_refused() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let binding = test_binding();

    sink.attach(&binding, &[]).await.unwrap();
    // Missing reconnected field must be refused, not defaulted to false
    fixture_state
        .lock()
        .inject_fault(ResponseCorruption::MissingReconnectField);
    let err = sink.reconnect(&binding).await.unwrap_err();
    assert!(matches!(err, SessionFactError::Refused(_)));
}

// ─────────────────────────────────────────────────────────────────────────
// Discriminator 13: Cancelled Outcome Authority Confirmation Discipline
// ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_cancelled_outcome_requires_recorded_accepted_request_cancel() {
    let fixture_state = Arc::new(Mutex::new(TachiSpineState::default()));
    let fixture_clone = fixture_state.clone();

    let sink = TachiSessionFactSink::new(test_sink_config())
        .expect("construct sink")
        .with_transport_factory(move |_cfg| {
            Ok(Box::new(ScriptedTachiMcpServer::new(fixture_clone.clone())))
        });

    let att = SessionAttachmentRef::from_opaque("att-cancel-test");
    let confirm = AuthorityConfirmationRef::from_opaque("cancel-conf-999");

    let cancel_fact = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("evt-cancel-1"),
        kind: SessionEventKindV1::Terminal,
        outcome: Some(SessionTerminalOutcomeV1::Cancelled {
            confirmation: confirm.clone(),
        }),
        source_revision: 10,
        authority_confirmation_ref: Some("cancel-conf-999".to_string()),
        summary: Some("cancelled by operator".to_string()),
        payload_digest: None,
    };

    // Attempting to ingest cancelled terminal without prior recorded accepted intervention fails
    let err = sink.ingest_event(&att, &cancel_fact).await.unwrap_err();
    match err {
        SessionFactError::Refused(msg) => {
            assert_eq!(msg, "tachi facade refused the request");
        }
        other => panic!("expected cancel refusal, got {other:?}"),
    }

    // Now record intervention result with disposition accepted
    let req_id = InterventionRequestIdRef::from_opaque("req-cancel-01");
    sink.request_intervention(
        &att,
        &req_id,
        SessionInterventionKindV1::RequestCancel,
        "operator cancel",
    )
    .await
    .unwrap();
    sink.record_intervention_result(
        &att,
        &req_id,
        SessionInterventionDispositionV1::Accepted,
        Some("cancel-conf-999"),
        Some("operator confirmed cancel"),
    )
    .await
    .unwrap();

    // Now ingest cancelled terminal succeeds
    let rec = sink.ingest_event(&att, &cancel_fact).await.unwrap();
    assert_eq!(rec.admission, SessionReceiptAdmissionV1::Created);
    assert_eq!(
        rec.state.canonical_state,
        SessionCanonicalStateV1::Cancelled
    );

    // Replay of cancelled terminal also succeeds
    let rec_replay = sink.ingest_event(&att, &cancel_fact).await.unwrap();
    assert_eq!(rec_replay.admission, SessionReceiptAdmissionV1::Replayed);
    assert_eq!(
        rec_replay.state.canonical_state,
        SessionCanonicalStateV1::Cancelled
    );
}

#[tokio::test]
async fn zero_envelope_capacity_retains_no_empty_attachment_maps() {
    let sink = TachiSessionFactSink::new(test_sink_config())
        .unwrap()
        .with_max_total_envelopes(0);
    let fact = SessionEventFact {
        event_id: SessionEventIdRef::from_opaque("zero-event"),
        kind: SessionEventKindV1::Started,
        outcome: None,
        source_revision: 0,
        authority_confirmation_ref: None,
        summary: None,
        payload_digest: None,
    };
    for id in ["zero-a", "zero-b"] {
        let error = sink
            .ingest_event(&SessionAttachmentRef::from_opaque(id), &fact)
            .await
            .unwrap_err();
        assert!(matches!(error, SessionFactError::Refused(reason) if reason.contains("capacity")));
    }
    assert!(sink.retained_envelopes.read().is_empty());
}
