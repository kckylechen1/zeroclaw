//! The production fact-sink transport — a real [`SessionFactSink`] that
//! consumes the PUBLIC tachi MCP facade (`tachi_agent_eval`, the
//! attached-session receipt spine) over the existing MCP stdio client.
//! This is the production wire the stage-b JSON stand-in pointed at
//! (the 261-vertical's honest gap); every sink operation is a real MCP `tools/call`
//! to a spawned `tachi serve` child.
//!
//! ```text
//! SessionFactSink port (receipts only)          (facts.rs)
//!   → TachiSessionFactSink (THIS FILE)
//!     → McpServer (zeroclaw-tools stdio client, transport-owned child)
//!       → tachi serve (the tachi-owned spine; receipts only)
//! ```
//!
//! Transport laws encoded here:
//!
//! - **Public facade only.** Every fact moves through the documented
//!   `tachi_agent_eval` action surface (`attach_session`,
//!   `advertise_session_capabilities`, `ingest_session_event`,
//!   `record_intervention_result`, `mark_session_connection`,
//!   `reconnect_session`, `get_session_state`). No DB access, no
//!   tachi-internal seam, no second transport family.
//! - **Admission context is operator-configured, never model-supplied.**
//!   The host identity, admitted agent identity, admission receipt ref,
//!   and work-claim binding come from the embedder-constructed
//!   [`TachiFactSinkConfig`]; env values (e.g. the isolated spine home)
//!   are secrets — redacted from `Debug`, never logged.
//! - **Replay-idempotent and source-revision bound.** Every operation is
//!   safe to re-send (attach replays by idempotency key; events dedup by
//!   event id), so a dropped transport is repaired by ONE reconnect +
//!   retry of the failed call — exactly-once at the spine, from the last
//!   observed revision via `reconnect_session`.
//! - **Typed failures.** Transport death surfaces
//!   [`SessionFactError::Unavailable`]; spine refusals (including the
//!   spine-gate's `unsupported_by_lifecycle_owner` refusals) surface as
//!   [`SessionFactError::Refused`] carrying the typed text. Nothing is
//!   fabricated on failure.
//! - **No new durable store.** This adapter opens no database and owns no
//!   DDL; the only persistence is the tachi-owned spine across the wire.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::{Value, json};
use zeroclaw_api::session_exec::{
    InterventionRequestIdRef, SessionAdvertiseReceiptView, SessionAttachmentRef,
    SessionCanonicalStateV1, SessionConnectionFactV1, SessionEventIdRef, SessionEventReceiptView,
    SessionFactError, SessionInterventionDispositionV1, SessionInterventionKindV1,
    SessionInterventionRequestView, SessionReceiptAdmissionV1, SessionReconnectReceiptView,
    SessionStateView, SessionTerminalOutcomeV1,
};
use zeroclaw_config::schema::McpServerConfig;
use zeroclaw_tools::mcp_protocol::JsonRpcRequest;
use zeroclaw_tools::mcp_transport::{McpTransportConn, create_transport};

use super::facts::{SessionBinding, SessionEventFact, SessionFactSink};

/// The tachi facade tool this carrier consumes (the attached-session
/// receipt spine surface).
const TACHI_AGENT_EVAL_TOOL: &str = "tachi_agent_eval";
/// The refusal-text ceiling (mirrors the fact-summary bound).
const SUMMARY_CEILING: usize = 2000;
/// The MCP protocol revision the spine handshake negotiates.
const SPINE_PROTOCOL_VERSION: &str = "2025-06-18";
/// Default capacity ceiling for retained event envelopes per attachment.
/// When exhausted, the sink fails closed to avoid evicting replay identities.
const DEFAULT_MAX_ENVELOPES_PER_ATTACHMENT: usize = 10_000;

/// Retained canonical event envelope for an (attachment_id, event_id) pair.
///
/// Single Source Of Truth:
/// This struct holds the canonical client-side fact for an event's immutable
/// transmission envelope. When an event is first ingested, its source material
/// is validated, its summary is projected deterministically, and its occurrence
/// timestamp is frozen. The serialized payload is materialized once and reused
/// across all subsequent attempts (lost-response internal retries, reconnects,
/// and separate replay calls).
///
/// In-process Bounds and Memory:
/// In-process memory footprint is bounded per attachment by
/// `max_envelopes_per_attachment`. When capacity is exhausted, the sink fails
/// closed (`SessionFactError::Refused`) rather than evicting existing entries.
/// Evicting replay identity would permit later events to regenerate their
/// occurrence timestamp, violating the consumer contract.
///
/// Lifetime and Durability:
/// In-process memory only for the lifetime of this [`TachiSessionFactSink`]
/// instance. Does not promise restart durability; no local task/result database.
#[derive(Clone, Debug)]
struct RetainedEventEnvelope {
    kind: SessionEventKindV1,
    outcome: Option<SessionTerminalOutcomeV1>,
    source_revision: u64,
    authority_confirmation_ref: Option<String>,
    projected_summary: Option<String>,
    payload_digest: Option<String>,
    occurred_at: String,
    serialized_payload: Value,
}

impl RetainedEventEnvelope {
    fn matches_material(
        &self,
        kind: SessionEventKindV1,
        outcome: &Option<SessionTerminalOutcomeV1>,
        source_revision: u64,
        authority_confirmation_ref: &Option<String>,
        projected_summary: &Option<String>,
        payload_digest: &Option<String>,
    ) -> bool {
        self.kind == kind
            && &self.outcome == outcome
            && self.source_revision == source_revision
            && &self.authority_confirmation_ref == authority_confirmation_ref
            && &self.projected_summary == projected_summary
            && &self.payload_digest == payload_digest
    }
}

type TransportFactory = Arc<
    dyn Fn(&McpServerConfig) -> Result<Box<dyn McpTransportConn>, anyhow::Error> + Send + Sync,
>;

/// Operator/embedder-constructed admission and transport binding. The
/// port cannot widen any field; values here are configuration facts.
#[derive(Clone)]
pub struct TachiFactSinkConfig {
    /// Absolute path of the tachi MCP server binary. Verified at
    /// construction.
    pub command: PathBuf,
    /// Fixed server argv (the MCP serve mode).
    pub args: Vec<String>,
    /// Operator-managed server env (e.g. an isolated spine home).
    /// Values are secrets: redacted from `Debug`, never logged.
    pub env: std::collections::HashMap<String, String>,
    /// The admitted host identity (must match the spine's host
    /// connection).
    pub host_identity: String,
    /// The admitted agent identity the attachment binds to.
    pub agent_identity_id: String,
    /// The host admission receipt reference every spine call carries.
    pub admission_receipt_ref: String,
    /// The work-claim binding the spine's fresh-claim re-admission
    /// verifies (claim id + expected transition revision).
    pub work_claim_id: String,
    pub expected_transition_revision: i64,
    /// The frozen assignment/contract digest the attachment carries.
    pub contract_digest: String,
    /// Requested policy tool profile (canonical name, e.g. `delegate`).
    pub tool_profile: String,
    /// Requested policy capability class (canonical name, e.g. `tachi`).
    pub capability_class: String,
    /// The negotiated ACP protocol version (the spine pins `1`).
    pub protocol_version: i64,
    /// Per-call ceiling.
    pub call_timeout: Duration,
}

impl std::fmt::Debug for TachiFactSinkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the env map wholesale: values are operator secrets.
        f.debug_struct("TachiFactSinkConfig")
            .field("command", &self.command)
            .field("args", &self.args)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("host_identity", &self.host_identity)
            .field("agent_identity_id", &self.agent_identity_id)
            .field("admission_receipt_ref", &"<configured>".to_string())
            .field("work_claim_id", &self.work_claim_id)
            .field(
                "expected_transition_revision",
                &self.expected_transition_revision,
            )
            .field("tool_profile", &self.tool_profile)
            .field("capability_class", &self.capability_class)
            .field("protocol_version", &self.protocol_version)
            .field("call_timeout", &self.call_timeout)
            .finish()
    }
}

impl TachiFactSinkConfig {
    fn verify(&self) -> Result<(), SessionFactError> {
        if !self.command.is_absolute() || !self.command.is_file() {
            return Err(SessionFactError::Refused(
                "tachi spine command is not an absolute existing file (fail closed)".to_string(),
            ));
        }
        for (name, value) in [
            ("host_identity", &self.host_identity),
            ("agent_identity_id", &self.agent_identity_id),
            ("admission_receipt_ref", &self.admission_receipt_ref),
            ("work_claim_id", &self.work_claim_id),
            ("contract_digest", &self.contract_digest),
            ("tool_profile", &self.tool_profile),
            ("capability_class", &self.capability_class),
        ] {
            if value.trim().is_empty() {
                return Err(SessionFactError::Refused(format!(
                    "tachi spine binding is missing {name} (fail closed)"
                )));
            }
        }
        if self.protocol_version != 1 {
            return Err(SessionFactError::Refused(
                "the spine pins negotiated ACP protocol_version 1".to_string(),
            ));
        }
        Ok(())
    }

    fn mcp_server_config(&self) -> McpServerConfig {
        McpServerConfig {
            name: "tachi-spine".to_string(),
            transport: Default::default(),
            url: None,
            command: self.command.display().to_string(),
            args: self.args.clone(),
            env: self.env.clone(),
            headers: Default::default(),
            tool_timeout_secs: Some(self.call_timeout.as_secs().max(1)),
            pinned_resources: Vec::new(),
        }
    }
}

/// One spine receipt envelope parsed from the facade's JSON answer.
#[derive(Clone, Debug)]
struct SpineReceipt {
    status: String,
    body: Value,
}

/// The production [`SessionFactSink`] over the tachi MCP facade.
pub struct TachiSessionFactSink {
    config: TachiFactSinkConfig,
    conn: tokio::sync::Mutex<Option<Box<dyn McpTransportConn>>>,
    attachment: RwLock<Option<String>>,
    /// Last observed canonical revision PER ATTACHMENT (the intervention
    /// gate's expected-session-revision input). Keyed by attachment id so
    /// a second run on the same sink can never inherit another session's
    /// revision high-water.
    last_revisions: RwLock<HashMap<String, u64>>,
    next_id: RwLock<u64>,
    /// Retained canonical event envelopes per attachment:
    /// attachment_id -> event_id -> RetainedEventEnvelope.
    /// Source of truth for client-side event transmission envelopes.
    retained_envelopes: RwLock<HashMap<String, HashMap<String, RetainedEventEnvelope>>>,
    /// Maximum number of retained envelopes per attachment before failing closed.
    max_envelopes_per_attachment: usize,
    /// Optional transport factory for tests. In production, `None` uses `create_transport`.
    transport_factory: Option<TransportFactory>,
    /// Optional clock for tests to verify replay without blocking sleeps.
    clock: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}

impl TachiSessionFactSink {
    /// Construct the sink. Fails closed when the transport binding is
    /// incomplete — never lazily at the first fact.
    pub fn new(config: TachiFactSinkConfig) -> Result<Self, SessionFactError> {
        config.verify()?;
        Ok(Self {
            config,
            conn: tokio::sync::Mutex::new(None),
            attachment: RwLock::new(None),
            last_revisions: RwLock::new(HashMap::new()),
            next_id: RwLock::new(1),
            retained_envelopes: RwLock::new(HashMap::new()),
            max_envelopes_per_attachment: DEFAULT_MAX_ENVELOPES_PER_ATTACHMENT,
            transport_factory: None,
            clock: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_transport_factory(
        mut self,
        factory: impl Fn(&McpServerConfig) -> Result<Box<dyn McpTransportConn>, anyhow::Error>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.transport_factory = Some(Arc::new(factory));
        self
    }

    #[cfg(test)]
    pub(crate) fn with_max_envelopes_per_attachment(mut self, max: usize) -> Self {
        self.max_envelopes_per_attachment = max;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_clock(
        mut self,
        clock: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        self.clock = Some(Arc::new(clock));
        self
    }

    fn now_timestamp(&self) -> String {
        match &self.clock {
            Some(clock) => clock(),
            None => now_rfc3339(),
        }
    }

    /// The live transport, spawning the spine child and running the
    /// initialize-FIRST handshake on demand. The handshake initializes
    /// before anything else because strict rmcp peers (tachi among them)
    /// abort on any pre-initialize request — the generic client's
    /// `server/discover` era probe is not tolerated on this surface.
    async fn conn(&self) -> Result<(), SessionFactError> {
        if let Some(conn) = self.conn.lock().await.as_mut()
            && conn.health_check()
        {
            return Ok(());
        }
        let server_cfg = self.config.mcp_server_config();
        let mut conn = match &self.transport_factory {
            Some(factory) => factory(&server_cfg).map_err(|error| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(serde_json::json!({ "detail": error.to_string() })),
                    "tachi spine transport spawn failed",
                );
                SessionFactError::Unavailable
            })?,
            None => create_transport(&server_cfg).map_err(|error| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(serde_json::json!({ "detail": error.to_string() })),
                    "tachi spine transport spawn failed",
                );
                SessionFactError::Unavailable
            })?,
        };
        let initialize = JsonRpcRequest::new(
            1,
            "initialize",
            json!({
                "protocolVersion": SPINE_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "zeroclaw-exec-subagent", "version": env!("CARGO_PKG_VERSION")},
            }),
        );
        // The handshake shares the per-call ceiling: a child that never
        // answers initialize fails closed instead of hanging the run.
        let response =
            tokio::time::timeout(self.config.call_timeout, conn.send_and_recv(&initialize))
                .await
                .map_err(|_| SessionFactError::Unavailable)?
                .map_err(|_| SessionFactError::Unavailable)?;
        if response.error.is_some() {
            return Err(SessionFactError::Refused(
                "the spine refused the MCP initialize handshake".to_string(),
            ));
        }
        // notifications expect no response; the stdio transport returns
        // immediately for id-less writes.
        let initialized = JsonRpcRequest::notification("notifications/initialized", json!({}));
        let _ = conn.send_and_recv(&initialized).await;
        *self.conn.lock().await = Some(conn);
        Ok(())
    }

    /// Drop the transport (the child is the client's ownership). The next
    /// call re-spawns; every operation is replay-idempotent, so the ONE
    /// retry after a drop re-delivers exactly the un-acked fact.
    async fn drop_transport(&self) {
        *self.conn.lock().await = None;
    }

    /// Call one facade action. The first argument list is the fixed
    /// admission context; `extra` carries the action payload.
    async fn call(&self, action: &str, extra: Value) -> Result<SpineReceipt, SessionFactError> {
        let mut params = json!({
            "action": action,
            "host_identity": self.config.host_identity,
            "admission_receipt_ref": self.config.admission_receipt_ref,
        });
        if let (Some(object), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        match self.call_once(action, params.clone()).await {
            Ok(receipt) => Ok(receipt),
            Err(SessionFactError::Unavailable) => {
                // Transport-level failure: repair the transport and retry
                // ONCE. Safe because every action here is
                // replay-idempotent at the spine (attach by idempotency
                // key, events by event id, receipts by request id).
                self.drop_transport().await;
                self.call_once(action, params).await
            }
            Err(error) => Err(error),
        }
    }

    async fn call_once(
        &self,
        action: &str,
        params: Value,
    ) -> Result<SpineReceipt, SessionFactError> {
        self.conn().await?;
        let request = JsonRpcRequest::new(
            self.next_call_id(),
            "tools/call",
            json!({
                "name": TACHI_AGENT_EVAL_TOOL,
                "arguments": params,
            }),
        );
        let response = {
            let mut conn = self.conn.lock().await;
            match conn.as_mut() {
                Some(conn) => {
                    tokio::time::timeout(self.config.call_timeout, conn.send_and_recv(&request))
                        .await
                        .map_err(|_| SessionFactError::Unavailable)?
                        .map_err(|error| {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Fail,
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(serde_json::json!({
                                    "spine_action": action,
                                    "detail": error.to_string(),
                                })),
                                "tachi facade call failed",
                            );
                            map_spine_error(error)
                        })?
                }
                None => return Err(SessionFactError::Unavailable),
            }
        };
        if let Some(error) = &response.error {
            return Err(SessionFactError::Refused(format!(
                "spine action {action} failed: {}",
                bounded_reason(&error.message)
            )));
        }
        // The facade answers with one JSON document in content[0].text;
        // tool-level failures arrive as isError envelopes and surface as
        // typed refusals carrying the facade's own reason.
        let result = response.result.clone().unwrap_or(Value::Null);
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if is_error {
            return Err(SessionFactError::Refused(bounded_reason(&text)));
        }
        if text.is_empty() {
            return Err(SessionFactError::Refused(format!(
                "spine action {action} returned no receipt"
            )));
        }
        let body: Value = serde_json::from_str(&text).map_err(|_| {
            SessionFactError::Refused(format!(
                "spine action {action} returned an unparseable receipt"
            ))
        })?;
        let status = body
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(SpineReceipt { status, body })
    }

    /// Monotone per-connection JSON-RPC id (1 is the initialize).
    fn next_call_id(&self) -> u64 {
        let mut id = self.next_id.write();
        *id += 1;
        *id
    }

    fn parse_state(body: &Value) -> Result<SessionStateView, SessionFactError> {
        let state = if body.get("canonical_revision").is_some()
            && body.get("cleanup_recorded").is_some()
        {
            body
        } else {
            body.get("canonical_state").ok_or_else(|| {
                SessionFactError::Refused("spine receipt carries no canonical_state".to_string())
            })?
        };
        if !state.is_object() {
            return Err(SessionFactError::Refused(
                "spine receipt canonical_state is not an object".to_string(),
            ));
        }
        let canonical = match state.get("canonical_state") {
            Some(Value::String(raw)) => SessionCanonicalStateV1::parse(raw)?,
            Some(Value::Null) => {
                // Honest typed incompatibility when observed:
                // Pre-event null canonical state is legitimately null in Tachi before any fact,
                // but the current SessionStateView API cannot represent a null canonical state.
                // We return an honest typed refusal rather than fabricating Accepted or Completed.
                return Err(SessionFactError::Refused(
                    "spine canonical_state is null (pre-event state unrepresentable in SessionStateView API)"
                        .to_string(),
                ));
            }
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "spine state projection canonical_state is not a string or null".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "spine state projection is missing canonical_state field".to_string(),
                ));
            }
        };
        let canonical_revision = match state.get("canonical_revision") {
            Some(val) if val.is_i64() => {
                let rev = val.as_i64().unwrap();
                if rev < 0 {
                    return Err(SessionFactError::Refused(
                        "spine canonical_revision is negative".to_string(),
                    ));
                }
                rev as u64
            }
            Some(val) if val.is_u64() => {
                let rev = val.as_u64().unwrap();
                if rev > i64::MAX as u64 {
                    return Err(SessionFactError::Refused(
                        "spine canonical_revision exceeds i64::MAX".to_string(),
                    ));
                }
                rev
            }
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "spine canonical_revision is not a valid integer".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "spine state projection is missing canonical_revision".to_string(),
                ));
            }
        };
        let cleanup_recorded = match state.get("cleanup_recorded") {
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "spine cleanup_recorded is not a boolean".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "spine state projection is missing cleanup_recorded".to_string(),
                ));
            }
        };
        let conflicting_terminal = match state.get("conflicting_terminal") {
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "spine conflicting_terminal is not a boolean".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "spine state projection is missing conflicting_terminal".to_string(),
                ));
            }
        };
        let last_event_id = match state.get("last_event_id") {
            Some(Value::String(s)) => {
                if s.trim().is_empty() {
                    None
                } else {
                    Some(s.clone())
                }
            }
            Some(Value::Null) => None,
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "spine last_event_id is not a string or null".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "spine state projection is missing last_event_id".to_string(),
                ));
            }
        };
        Ok(SessionStateView {
            canonical_state: canonical,
            canonical_revision,
            cleanup_recorded,
            conflicting_terminal,
            last_event_id,
        })
    }

    fn note_revision(&self, attachment: &SessionAttachmentRef, view: &SessionStateView) {
        self.last_revisions
            .write()
            .entry(attachment.as_str().to_string())
            .and_modify(|current| *current = (*current).max(view.canonical_revision))
            .or_insert(view.canonical_revision);
    }

    fn revision_for(&self, attachment: &SessionAttachmentRef) -> u64 {
        self.last_revisions
            .read()
            .get(attachment.as_str())
            .copied()
            .unwrap_or(0)
    }
}

/// Map a facade failure to the typed port error. Spine-gate typed
/// refusals (`unsupported_by_lifecycle_owner`) are carried verbatim in
/// [`SessionFactError::Refused`] — never flattened into unavailable,
/// never fabricated into success.
fn map_spine_error(error: anyhow::Error) -> SessionFactError {
    let text = error.to_string();
    if text.contains("unsupported_by_lifecycle_owner") {
        SessionFactError::Refused(text)
    } else {
        SessionFactError::Unavailable
    }
}

/// Bound a facade refusal text at the fact-summary ceiling (refusal
/// texts are spine-authored and bounded at the same law as summaries).
fn bounded_reason(text: &str) -> String {
    let mut boundary = text.len().min(SUMMARY_CEILING);
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text[..boundary].to_string()
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn validate_attachment_id(id: &str) -> Result<(), SessionFactError> {
    if id.trim().is_empty() {
        return Err(SessionFactError::Refused(
            "attachment_id must be nonblank after trim".to_string(),
        ));
    }
    if id.chars().count() > 128 {
        return Err(SessionFactError::Refused(
            "attachment_id exceeds 128 characters ceiling".to_string(),
        ));
    }
    if id.chars().any(|c| c.is_control()) {
        return Err(SessionFactError::Refused(
            "attachment_id must not contain control characters".to_string(),
        ));
    }
    Ok(())
}

fn validate_event_id(id: &str) -> Result<(), SessionFactError> {
    if id.trim().is_empty() {
        return Err(SessionFactError::Refused(
            "event_id must be nonblank after trim".to_string(),
        ));
    }
    if id.chars().count() > 128 {
        return Err(SessionFactError::Refused(
            "event_id exceeds 128 characters ceiling".to_string(),
        ));
    }
    if id.chars().any(|c| c.is_control()) {
        return Err(SessionFactError::Refused(
            "event_id must not contain control characters".to_string(),
        ));
    }
    Ok(())
}

/// Deterministic public-safe summary projection BEFORE freezing into the
/// retained canonical envelope.
///
/// Rules:
/// - `None` or exact-empty string becomes `Ok(None)`.
/// - Rejects summaries containing prohibited NUL (`\0`) or C1 control characters (`\u{0080}`..=`\u{009F}`).
/// - Rejects summaries consisting entirely of control characters.
/// - Deterministically replaces remaining control characters (CRLF, tab, C0 controls) with spaces.
/// - Bounds Unicode scalar count at `SUMMARY_CEILING` (2000 characters) preserving character boundaries.
fn project_summary(raw: Option<&str>) -> Result<Option<String>, SessionFactError> {
    let raw = match raw {
        None => return Ok(None),
        Some(s) if s.is_empty() => return Ok(None),
        Some(s) => s,
    };
    if raw.contains('\0') {
        return Err(SessionFactError::Refused(
            "summary contains prohibited NUL character".to_string(),
        ));
    }
    if raw.chars().any(|c| ('\u{0080}'..='\u{009F}').contains(&c)) {
        return Err(SessionFactError::Refused(
            "summary contains prohibited C1 control character".to_string(),
        ));
    }
    if raw.chars().all(|c| c.is_control()) {
        return Err(SessionFactError::Refused(
            "summary consists entirely of control characters".to_string(),
        ));
    }
    let mut projected = String::with_capacity(raw.len().min(SUMMARY_CEILING * 4));
    for c in raw.chars().take(SUMMARY_CEILING) {
        if c.is_control() {
            projected.push(' ');
        } else {
            projected.push(c);
        }
    }
    if projected.is_empty() {
        Ok(None)
    } else {
        Ok(Some(projected))
    }
}

fn validate_and_project_confirmation_ref(
    raw: Option<&str>,
) -> Result<Option<String>, SessionFactError> {
    match raw {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => {
            if s.trim().is_empty() {
                return Err(SessionFactError::Refused(
                    "authority_confirmation_ref must not be blank".to_string(),
                ));
            }
            if s.chars().count() > 128 {
                return Err(SessionFactError::Refused(
                    "authority_confirmation_ref exceeds 128 characters ceiling".to_string(),
                ));
            }
            if s.chars().any(|c| c.is_control()) {
                return Err(SessionFactError::Refused(
                    "authority_confirmation_ref must not contain control characters".to_string(),
                ));
            }
            Ok(Some(s.to_string()))
        }
    }
}

fn validate_and_project_payload_digest(
    raw: Option<&str>,
) -> Result<Option<String>, SessionFactError> {
    match raw {
        None => Ok(None),
        Some(s) if s.is_empty() => Ok(None),
        Some(s) => {
            if s.trim().is_empty() {
                return Err(SessionFactError::Refused(
                    "payload_digest must not be blank".to_string(),
                ));
            }
            if s.chars().count() > 128 {
                return Err(SessionFactError::Refused(
                    "payload_digest exceeds 128 characters ceiling".to_string(),
                ));
            }
            if !s
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '+' | '/' | ':'))
            {
                return Err(SessionFactError::Refused(
                    "payload_digest contains invalid character (only ASCII alphanumeric and -_=+/: allowed)"
                        .to_string(),
                ));
            }
            Ok(Some(s.to_string()))
        }
    }
}

#[async_trait]
impl SessionFactSink for TachiSessionFactSink {
    async fn attach(
        &self,
        binding: &SessionBinding,
        capabilities: &[String],
    ) -> Result<SessionAttachmentRef, SessionFactError> {
        let receipt = self
            .call(
                "attach_session",
                json!({
                    "agent_identity_id": self.config.agent_identity_id,
                    "work_claim_id": self.config.work_claim_id,
                    "expected_transition_revision": self.config.expected_transition_revision,
                    "protocol_version": self.config.protocol_version,
                    "adapter_connection_identity": binding.adapter_connection.as_str(),
                    "remote_session_id": binding.remote_session.as_str(),
                    "contract_digest": self.config.contract_digest,
                    "session_capabilities": capabilities,
                    "tool_profile": self.config.tool_profile,
                    "capability_class": self.config.capability_class,
                    "idempotency_key": binding.idempotency_key,
                }),
            )
            .await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(format!(
                "attach_session status {}",
                receipt.status
            )));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("attach_session") {
            return Err(SessionFactError::Refused(
                "attach receipt action mismatch or missing".to_string(),
            ));
        }
        let attachment_id = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused("attach receipt carries no attachment id".to_string())
            })?;
        validate_attachment_id(attachment_id)?;
        *self.attachment.write() = Some(attachment_id.to_string());
        Ok(SessionAttachmentRef::from_opaque(attachment_id))
    }

    async fn advertise_capabilities(
        &self,
        attachment: &SessionAttachmentRef,
        capabilities: &[String],
    ) -> Result<SessionAdvertiseReceiptView, SessionFactError> {
        let mut params = json!({
            "attachment_id": attachment.as_str(),
            "session_capabilities": capabilities,
        });
        if self.attachment.read().is_none() {
            // No cached id: address by the binding's natural key.
            params = json!({ "session_capabilities": capabilities });
        }
        let receipt = self.call("advertise_session_capabilities", params).await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(format!(
                "advertise_session_capabilities status {}",
                receipt.status
            )));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("advertise_session_capabilities") {
            return Err(SessionFactError::Refused(
                "advertise receipt action mismatch or missing".to_string(),
            ));
        }
        let advertisement_seq = match receipt.body.get("advertisement_seq") {
            Some(val) if val.is_i64() => {
                let seq = val.as_i64().unwrap();
                if seq < 0 {
                    return Err(SessionFactError::Refused(
                        "advertisement_seq is negative".to_string(),
                    ));
                }
                seq as u64
            }
            Some(val) if val.is_u64() => {
                let seq = val.as_u64().unwrap();
                if seq > i64::MAX as u64 {
                    return Err(SessionFactError::Refused(
                        "advertisement_seq exceeds i64::MAX".to_string(),
                    ));
                }
                seq
            }
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "advertisement_seq is not a valid integer".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "advertise receipt carries no advertisement_seq".to_string(),
                ));
            }
        };
        let ret_capabilities = match receipt.body.get("session_capabilities").and_then(Value::as_array) {
            Some(arr) => {
                let mut caps = Vec::with_capacity(arr.len());
                for item in arr {
                    if let Some(s) = item.as_str() {
                        caps.push(s.to_string());
                    } else {
                        return Err(SessionFactError::Refused(
                            "session_capabilities item is not a string".to_string(),
                        ));
                    }
                }
                caps
            }
            None => capabilities.to_vec(),
        };
        Ok(SessionAdvertiseReceiptView {
            attachment_ref: attachment.clone(),
            advertisement_seq,
            capabilities: ret_capabilities,
        })
    }

    async fn ingest_event(
        &self,
        attachment: &SessionAttachmentRef,
        fact: &SessionEventFact,
    ) -> Result<SessionEventReceiptView, SessionFactError> {
        validate_attachment_id(attachment.as_str())?;
        validate_event_id(fact.event_id.as_str())?;

        // source_revision: signed i64 on consumer wire, nonnegative
        if fact.source_revision > i64::MAX as u64 {
            return Err(SessionFactError::Refused(
                "source_revision exceeds i64::MAX".to_string(),
            ));
        }

        // Event kinds: only terminal carries outcome; outcome completed/failed/cancelled
        if fact.kind == SessionEventKindV1::Terminal {
            if fact.outcome.is_none() {
                return Err(SessionFactError::Refused(
                    "terminal event must carry an outcome".to_string(),
                ));
            }
        } else if fact.outcome.is_some() {
            return Err(SessionFactError::Refused(
                "non-terminal event must not carry an outcome".to_string(),
            ));
        }

        // Deterministic public-safe summary projection BEFORE freezing
        let projected_summary = project_summary(fact.summary.as_deref())?;

        // Normalize and validate authority_confirmation_ref
        let effective_auth_ref = match (&fact.outcome, &fact.authority_confirmation_ref) {
            (Some(SessionTerminalOutcomeV1::Cancelled { confirmation }), Some(explicit)) => {
                if confirmation.as_str() != explicit.as_str() {
                    return Err(SessionFactError::Refused(
                        "outcome confirmation ref does not match fact authority_confirmation_ref"
                            .to_string(),
                    ));
                }
                Some(confirmation.as_str())
            }
            (Some(SessionTerminalOutcomeV1::Cancelled { confirmation }), None) => {
                Some(confirmation.as_str())
            }
            (_, Some(explicit)) => Some(explicit.as_str()),
            (_, None) => None,
        };
        let projected_auth_ref = validate_and_project_confirmation_ref(effective_auth_ref)?;

        // Normalize and validate payload_digest
        let projected_digest =
            validate_and_project_payload_digest(fact.payload_digest.as_deref())?;

        // Envelope retention and same-material detection
        let payload = {
            let mut envelopes_guard = self.retained_envelopes.write();
            let attachment_envelopes = envelopes_guard
                .entry(attachment.as_str().to_string())
                .or_default();

            if let Some(existing) = attachment_envelopes.get(fact.event_id.as_str()) {
                if !existing.matches_material(
                    fact.kind,
                    &fact.outcome,
                    fact.source_revision,
                    &projected_auth_ref,
                    &projected_summary,
                    &projected_digest,
                ) {
                    return Err(SessionFactError::Refused(format!(
                        "same-ID material conflict for event_id {:?}: cannot change frozen event material",
                        fact.event_id.as_str()
                    )));
                }
                existing.serialized_payload.clone()
            } else {
                if attachment_envelopes.len() >= self.max_envelopes_per_attachment {
                    return Err(SessionFactError::Refused(
                        "in-process event envelope cache capacity exhausted (fail closed)".to_string(),
                    ));
                }
                let occurred_at = self.now_timestamp();
                let serialized_payload = json!({
                    "attachment_id": attachment.as_str(),
                    "session_event_id": fact.event_id.as_str(),
                    "session_event_kind": fact.kind.as_str(),
                    "session_event_outcome": fact.outcome.as_ref().map(|o| o.kind_name()),
                    "source_revision": fact.source_revision as i64,
                    "authority_confirmation_ref": projected_auth_ref,
                    "event_summary": projected_summary,
                    "payload_digest": projected_digest,
                    "event_occurred_at": occurred_at,
                });
                let envelope = RetainedEventEnvelope {
                    kind: fact.kind,
                    outcome: fact.outcome.clone(),
                    source_revision: fact.source_revision,
                    authority_confirmation_ref: projected_auth_ref,
                    projected_summary,
                    payload_digest: projected_digest,
                    occurred_at,
                    serialized_payload: serialized_payload.clone(),
                };
                attachment_envelopes.insert(fact.event_id.as_str().to_string(), envelope);
                serialized_payload
            }
        };

        let receipt = self.call("ingest_session_event", payload).await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(format!(
                "ingest_session_event status {}",
                receipt.status
            )));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("ingest_session_event") {
            return Err(SessionFactError::Refused(
                "event receipt action mismatch or missing".to_string(),
            ));
        }
        let ret_attachment = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused("event receipt carries no attachment_id".to_string())
            })?;
        if ret_attachment != attachment.as_str() {
            return Err(SessionFactError::Refused(format!(
                "event receipt attachment_id mismatch: expected {}, got {ret_attachment}",
                attachment.as_str()
            )));
        }
        let ret_event_id = receipt
            .body
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused("event receipt carries no event_id".to_string())
            })?;
        if ret_event_id != fact.event_id.as_str() {
            return Err(SessionFactError::Refused(format!(
                "event receipt event_id mismatch: expected {}, got {ret_event_id}",
                fact.event_id.as_str()
            )));
        }
        let admission = match receipt.body.get("admission").and_then(Value::as_str) {
            Some("journaled") => SessionReceiptAdmissionV1::Created,
            Some("replayed") => SessionReceiptAdmissionV1::Replayed,
            Some(other) => {
                return Err(SessionFactError::Refused(format!(
                    "event receipt carries unknown admission class: {other}"
                )));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "event receipt carries no admission class".to_string(),
                ));
            }
        };
        let disposition = match receipt.body.get("disposition").and_then(Value::as_str) {
            Some(d @ ("advanced" | "journaled_stale" | "journaled_terminal_conflict" | "journaled_redundant_terminal")) => {
                d.to_string()
            }
            Some(other) => {
                return Err(SessionFactError::Refused(format!(
                    "event receipt carries unknown disposition: {other}"
                )));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "event receipt carries no disposition".to_string(),
                ));
            }
        };
        let state = {
            let state = Self::parse_state(&receipt.body)?;
            self.note_revision(attachment, &state);
            state
        };
        Ok(SessionEventReceiptView {
            attachment_ref: SessionAttachmentRef::from_opaque(ret_attachment),
            event_id: SessionEventIdRef::from_opaque(ret_event_id),
            admission,
            disposition,
            state,
        })
    }

    async fn request_intervention(
        &self,
        attachment: &SessionAttachmentRef,
        request_id: &InterventionRequestIdRef,
        kind: SessionInterventionKindV1,
        reason: &str,
    ) -> Result<(), SessionFactError> {
        let receipt = self
            .call(
                "request_intervention",
                json!({
                    "attachment_id": attachment.as_str(),
                    "intervention_request_id": request_id.as_str(),
                    "intervention_kind": kind.as_str(),
                    "intervention_reason": reason,
                    "expected_session_revision": self.revision_for(attachment),
                }),
            )
            .await?;
        if receipt.status != "completed" {
            // The spine-gate's typed `unsupported_by_lifecycle_owner`
            // refusal arrives verbatim and is re-raised typed below.
            return Err(SessionFactError::Refused(format!(
                "request_intervention status {}",
                receipt.status
            )));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("request_intervention") {
            return Err(SessionFactError::Refused(
                "request_intervention receipt action mismatch or missing".to_string(),
            ));
        }
        let ret_att = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused(
                    "request_intervention receipt carries no attachment_id".to_string(),
                )
            })?;
        if ret_att != attachment.as_str() {
            return Err(SessionFactError::Refused(format!(
                "request_intervention receipt attachment_id mismatch: expected {}, got {ret_att}",
                attachment.as_str()
            )));
        }
        Ok(())
    }

    async fn get_intervention(
        &self,
        _attachment: &SessionAttachmentRef,
        request_id: &InterventionRequestIdRef,
    ) -> Result<Option<SessionInterventionRequestView>, SessionFactError> {
        // The public facade mints intervention asks (request_intervention)
        // and records their outcomes; there is no pickup-by-id READ
        // action, and replaying `request_intervention` for an unknown id
        // would MINT an ask this host never received. Refuse typed rather
        // than fabricate one. The vertical's run consumes interventions
        // through the cancel receipt chain instead.
        let _ = request_id;
        Err(SessionFactError::Refused(
            "intervention pickup is spine-initiated; this carrier consumes the cancel receipt \
             chain (no public pickup-by-id read exists)"
                .to_string(),
        ))
    }

    async fn record_intervention_result(
        &self,
        attachment: &SessionAttachmentRef,
        request_id: &InterventionRequestIdRef,
        disposition: SessionInterventionDispositionV1,
        authority_confirmation_ref: Option<&str>,
        detail: Option<&str>,
    ) -> Result<(), SessionFactError> {
        let receipt = self
            .call(
                "record_intervention_result",
                json!({
                    "attachment_id": attachment.as_str(),
                    "intervention_request_id": request_id.as_str(),
                    "intervention_disposition": disposition.as_str(),
                    "authority_confirmation_ref": authority_confirmation_ref,
                    "intervention_detail": detail,
                }),
            )
            .await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(format!(
                "record_intervention_result status {}",
                receipt.status
            )));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("record_intervention_result") {
            return Err(SessionFactError::Refused(
                "record_intervention_result receipt action mismatch or missing".to_string(),
            ));
        }
        let ret_att = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused(
                    "record_intervention_result receipt carries no attachment_id".to_string(),
                )
            })?;
        if ret_att != attachment.as_str() {
            return Err(SessionFactError::Refused(format!(
                "record_intervention_result receipt attachment_id mismatch: expected {}, got {ret_att}",
                attachment.as_str()
            )));
        }
        Ok(())
    }

    async fn mark_connection(
        &self,
        attachment: &SessionAttachmentRef,
        fact: SessionConnectionFactV1,
    ) -> Result<(), SessionFactError> {
        let receipt = self
            .call(
                "mark_session_connection",
                json!({
                    "attachment_id": attachment.as_str(),
                    "connection_fact": fact.as_str(),
                }),
            )
            .await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(format!(
                "mark_session_connection status {}",
                receipt.status
            )));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("mark_session_connection") {
            return Err(SessionFactError::Refused(
                "mark_session_connection receipt action mismatch or missing".to_string(),
            ));
        }
        let ret_att = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused(
                    "mark_session_connection receipt carries no attachment_id".to_string(),
                )
            })?;
        if ret_att != attachment.as_str() {
            return Err(SessionFactError::Refused(format!(
                "mark_session_connection receipt attachment_id mismatch: expected {}, got {ret_att}",
                attachment.as_str()
            )));
        }
        Ok(())
    }

    async fn reconnect(
        &self,
        binding: &SessionBinding,
    ) -> Result<SessionReconnectReceiptView, SessionFactError> {
        let receipt = self
            .call(
                "reconnect_session",
                json!({
                    "adapter_connection_identity": binding.adapter_connection.as_str(),
                    "remote_session_id": binding.remote_session.as_str(),
                }),
            )
            .await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(format!(
                "reconnect_session status {}",
                receipt.status
            )));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("reconnect_session") {
            return Err(SessionFactError::Refused(
                "reconnect receipt action mismatch or missing".to_string(),
            ));
        }
        let attachment_id = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused("reconnect receipt carries no attachment id".to_string())
            })?;
        validate_attachment_id(attachment_id)?;
        let reconnected = match receipt.body.get("reconnected") {
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "reconnect receipt carries non-boolean reconnected".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "reconnect receipt carries no reconnected field".to_string(),
                ));
            }
        };
        let resume_from_revision = match receipt.body.get("resume_from_revision") {
            Some(val) if val.is_i64() => {
                let rev = val.as_i64().unwrap();
                if rev < 0 {
                    return Err(SessionFactError::Refused(
                        "reconnect resume_from_revision is negative".to_string(),
                    ));
                }
                rev as u64
            }
            Some(val) if val.is_u64() => {
                let rev = val.as_u64().unwrap();
                if rev > i64::MAX as u64 {
                    return Err(SessionFactError::Refused(
                        "reconnect resume_from_revision exceeds i64::MAX".to_string(),
                    ));
                }
                rev
            }
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "reconnect resume_from_revision is not a valid integer".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "reconnect receipt carries no resume_from_revision".to_string(),
                ));
            }
        };
        let state = {
            let state = Self::parse_state(&receipt.body)?;
            self.note_revision(&SessionAttachmentRef::from_opaque(attachment_id), &state);
            state
        };
        *self.attachment.write() = Some(attachment_id.to_string());
        Ok(SessionReconnectReceiptView {
            attachment_ref: SessionAttachmentRef::from_opaque(attachment_id),
            reconnected,
            resume_from_revision,
            state,
        })
    }

    async fn get_state(
        &self,
        attachment: &SessionAttachmentRef,
    ) -> Result<SessionStateView, SessionFactError> {
        let receipt = self
            .call(
                "get_session_state",
                json!({ "attachment_id": attachment.as_str() }),
            )
            .await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(format!(
                "get_session_state status {}",
                receipt.status
            )));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("get_session_state") {
            return Err(SessionFactError::Refused(
                "get_session_state receipt action mismatch or missing".to_string(),
            ));
        }
        let ret_att = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused(
                    "get_session_state receipt carries no attachment_id".to_string(),
                )
            })?;
        if ret_att != attachment.as_str() {
            return Err(SessionFactError::Refused(format!(
                "get_session_state receipt attachment_id mismatch: expected {}, got {ret_att}",
                attachment.as_str()
            )));
        }
        let state = Self::parse_state(&receipt.body)?;
        self.note_revision(attachment, &state);
        Ok(state)
    }
}

#[cfg(test)]
mod tachi_sink_tests;

