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
//! - **Explicit replay policy.** Attachments, events and intervention receipts
//!   deduplicate by identity; connection facts and state reads are safe to
//!   retry once. Advertisements and reconnect return unavailable after response
//!   loss: the former appends another row, and the latter replaces evidence of
//!   the transition with a new receipt. Future actions default to no retry.
//! - **Typed failures.** Transport death surfaces
//!   [`SessionFactError::Unavailable`]; spine refusals (including the
//!   spine-gate's `unsupported_by_lifecycle_owner` refusals) surface as
//!   [`SessionFactError::Refused`] carrying the typed text. Nothing is
//!   fabricated on failure.
//! - **No new durable store.** This adapter opens no database and owns no
//!   DDL; the only persistence is the tachi-owned spine across the wire.

use std::collections::HashMap;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::{Value, json};
use zeroclaw_api::session_exec::{
    InterventionRequestIdRef, SessionAdvertiseReceiptView, SessionAttachmentRef,
    SessionCanonicalStateV1, SessionConnectionFactV1, SessionEventIdRef, SessionEventKindV1,
    SessionEventReceiptView, SessionFactError, SessionInterventionDispositionV1,
    SessionInterventionKindV1, SessionInterventionRequestView, SessionReceiptAdmissionV1,
    SessionReconnectReceiptView, SessionStateView, SessionTerminalOutcomeV1,
};
use zeroclaw_config::schema::McpServerConfig;
use zeroclaw_tools::mcp_protocol::JsonRpcRequest;
use zeroclaw_tools::mcp_transport::{McpTransportConn, create_transport};

use super::facts::{SessionBinding, SessionEventFact, SessionFactSink};

/// The tachi facade tool this carrier consumes (the attached-session
/// receipt spine surface).
const TACHI_AGENT_EVAL_TOOL: &str = "tachi_agent_eval";
/// Maximum summary length in Unicode scalar values on the event wire.
const SUMMARY_CEILING: usize = 2000;
/// The MCP protocol revision the spine handshake negotiates.
const SPINE_PROTOCOL_VERSION: &str = "2025-06-18";
/// Default capacity ceiling for retained event envelopes per attachment.
/// When exhausted, the sink fails closed to avoid evicting replay identities.
const DEFAULT_MAX_ENVELOPES_PER_ATTACHMENT: usize = 10_000;
/// Default capacity ceiling for retained event envelopes across all attachments.
const DEFAULT_MAX_TOTAL_ENVELOPES: usize = 50_000;

/// Retained canonical event envelope for an (attachment_id, event_id) pair.
///
/// Single Source Of Truth:
/// Holds the single canonical JSON payload for an event transmission envelope.
/// Material fields and the occurrence timestamp are serialized once into this
/// payload upon first transmission. Subsequent attempts (replays, lost-response
/// retries) derive comparison directly from this canonical retained payload
/// against newly projected incoming material using the retained timestamp,
/// eliminating duplicate state fields.
///
/// In-process Bounds and Memory:
/// Retained envelopes are bounded both per-attachment and globally across all
/// attachments. When capacity is reached, the sink fails closed rather than
/// evicting entries, preserving replay identities and timestamps.
///
/// Lifetime and Durability:
/// In-process memory only for the lifetime of this sink instance.
/// No restart durability or local database.
#[derive(Clone, Debug)]
struct RetainedEventEnvelope {
    payload: Value,
}

impl RetainedEventEnvelope {
    fn occurred_at(&self) -> Option<&str> {
        self.payload
            .get("event_occurred_at")
            .and_then(Value::as_str)
    }

    fn matches_material(&self, candidate: &Value) -> bool {
        &self.payload == candidate
    }
}

#[cfg(test)]
type TransportFactory =
    Arc<dyn Fn(&McpServerConfig) -> Result<Box<dyn McpTransportConn>, anyhow::Error> + Send + Sync>;

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
    pub env: HashMap<String, String>,
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
    /// Maximum number of retained envelopes across all attachments before failing closed.
    max_total_envelopes: usize,
    /// Optional transport factory for tests. In production, `None` uses `create_transport`.
    #[cfg(test)]
    transport_factory: Option<TransportFactory>,
    /// Optional clock for tests to verify replay without blocking sleeps.
    #[cfg(test)]
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
            last_revisions: RwLock::new(HashMap::new()),
            next_id: RwLock::new(1),
            retained_envelopes: RwLock::new(HashMap::new()),
            max_envelopes_per_attachment: DEFAULT_MAX_ENVELOPES_PER_ATTACHMENT,
            max_total_envelopes: DEFAULT_MAX_TOTAL_ENVELOPES,
            #[cfg(test)]
            transport_factory: None,
            #[cfg(test)]
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
    pub(crate) fn with_max_total_envelopes(mut self, max: usize) -> Self {
        self.max_total_envelopes = max;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_clock(mut self, clock: impl Fn() -> String + Send + Sync + 'static) -> Self {
        self.clock = Some(Arc::new(clock));
        self
    }

    fn now_timestamp(&self) -> String {
        #[cfg(test)]
        {
            if let Some(clock) = &self.clock {
                return clock();
            }
        }
        now_rfc3339()
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
        #[cfg(test)]
        let mut conn = match &self.transport_factory {
            Some(factory) => factory(&server_cfg).map_err(|_error| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(serde_json::json!({ "error_kind": "transport_failure" })),
                    "tachi spine transport spawn failed",
                );
                SessionFactError::Unavailable
            })?,
            None => create_transport(&server_cfg).map_err(|_error| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(serde_json::json!({ "error_kind": "transport_failure" })),
                    "tachi spine transport spawn failed",
                );
                SessionFactError::Unavailable
            })?,
        };
        #[cfg(not(test))]
        let mut conn = create_transport(&server_cfg).map_err(|_error| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(serde_json::json!({ "error_kind": "transport_failure" })),
                "tachi spine transport spawn failed",
            );
            SessionFactError::Unavailable
        })?;
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

    /// Drop the transport (the child is the client's ownership). A later
    /// call establishes a fresh connection, independently of retry eligibility.
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
                self.drop_transport().await;
                // Attachment/event/request/result identities deduplicate at the
                // consumer; connection facts are idempotent and state reads do
                // not mutate. Advertisements and reconnect receipts cannot be
                // replayed transparently. Unknown actions default to no retry.
                if matches!(
                    action,
                    "attach_session"
                        | "ingest_session_event"
                        | "request_intervention"
                        | "record_intervention_result"
                        | "mark_session_connection"
                        | "get_session_state"
                ) {
                    self.call_once(action, params).await
                } else {
                    Err(SessionFactError::Unavailable)
                }
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
                                    "error_kind": "transport_failure",
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
            return Err(facade_refusal(&error.message));
        }
        // The facade answers with one JSON document in content[0].text;
        // tool-level failures arrive as isError envelopes and surface as
        // typed refusals carrying the facade's own reason.
        let result = response.result.clone().unwrap_or(Value::Null);
        let is_error = match result.get("isError") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "malformed MCP isError field".to_string(),
                ));
            }
        };
        let text = result
            .pointer("/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if is_error {
            return Err(facade_refusal(&text));
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
        Self::validate_state(body)?.ok_or_else(|| SessionFactError::Refused(
            "spine canonical_state is null (pre-event state unrepresentable in SessionStateView API)".to_string(),
        ))
    }

    /// Unit-returning receipts may legally describe the state before any event.
    /// Validate every projection field before accepting that explicit absence.
    fn validate_state(body: &Value) -> Result<Option<SessionStateView>, SessionFactError> {
        let state = body.get("canonical_state").ok_or_else(|| {
            SessionFactError::Refused("spine receipt carries no canonical_state".to_string())
        })?;
        if !state.is_object() {
            return Err(SessionFactError::Refused(
                "spine receipt canonical_state is not an object".to_string(),
            ));
        }
        let canonical = match state.get("canonical_state") {
            Some(Value::String(raw)) if raw == raw.trim() => {
                Some(SessionCanonicalStateV1::parse(raw).map_err(|_| {
                    SessionFactError::Refused("spine returned unknown canonical state".to_string())
                })?)
            }
            Some(Value::Null) => None,
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
            Some(val) => {
                if let Some(rev) = val.as_i64() {
                    if rev < 0 {
                        return Err(SessionFactError::Refused(
                            "spine canonical_revision is negative".to_string(),
                        ));
                    }
                    rev as u64
                } else if let Some(rev) = val.as_u64() {
                    if rev > i64::MAX as u64 {
                        return Err(SessionFactError::Refused(
                            "spine canonical_revision exceeds i64::MAX".to_string(),
                        ));
                    }
                    rev
                } else {
                    return Err(SessionFactError::Refused(
                        "spine canonical_revision is not a valid integer".to_string(),
                    ));
                }
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
                if s.trim().is_empty()
                    || s.chars().count() > 128
                    || s.chars().any(|c| c.is_control())
                {
                    return Err(SessionFactError::Refused(
                        "spine last_event_id is invalid".to_string(),
                    ));
                }
                Some(s.clone())
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
        Ok(canonical.map(|canonical_state| SessionStateView {
            canonical_state,
            canonical_revision,
            cleanup_recorded,
            conflicting_terminal,
            last_event_id,
        }))
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

fn require_receipt_value(
    body: &Value,
    path: &str,
    expected: &Value,
) -> Result<(), SessionFactError> {
    if body.pointer(path) != Some(expected) {
        return Err(SessionFactError::Refused(format!(
            "spine receipt field mismatch or missing: {path}"
        )));
    }
    Ok(())
}

fn validate_intervention_admission(body: &Value) -> Result<(), SessionFactError> {
    if !matches!(
        body.get("admission").and_then(Value::as_str),
        Some("created" | "replayed")
    ) {
        return Err(SessionFactError::Refused(
            "intervention receipt has invalid admission".to_string(),
        ));
    }
    Ok(())
}

/// Preserve a known refusal code without returning arbitrary peer text.
fn facade_refusal(text: &str) -> SessionFactError {
    let reason = if text.contains("unsupported_by_lifecycle_owner") {
        "unsupported_by_lifecycle_owner"
    } else {
        "tachi facade refused the request"
    };
    SessionFactError::Refused(reason.to_string())
}

fn map_spine_error(error: anyhow::Error) -> SessionFactError {
    if error.to_string().contains("unsupported_by_lifecycle_owner") {
        facade_refusal("unsupported_by_lifecycle_owner")
    } else {
        SessionFactError::Unavailable
    }
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
/// - Rejects summaries consisting entirely of control characters or blank after trim.
/// - Deterministically replaces remaining control characters (CRLF, tab, C0 controls) with spaces.
/// - Bounds Unicode scalar count at `SUMMARY_CEILING` (2000 characters) preserving character boundaries.
fn project_summary(raw: Option<&str>) -> Result<Option<String>, SessionFactError> {
    let raw = match raw {
        None => return Ok(None),
        Some("") => return Ok(None),
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
    if raw.trim().is_empty() {
        return Err(SessionFactError::Refused(
            "summary must not be blank".to_string(),
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
    if projected.trim().is_empty() {
        Err(SessionFactError::Refused(
            "projected summary must not be blank".to_string(),
        ))
    } else {
        Ok(Some(projected))
    }
}

fn validate_and_project_confirmation_ref(
    raw: Option<&str>,
) -> Result<Option<String>, SessionFactError> {
    match raw {
        None => Ok(None),
        Some("") => Ok(None),
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
        Some("") => Ok(None),
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
            if !s.chars().all(|c| {
                c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '=' | '+' | '/' | ':')
            }) {
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
            return Err(SessionFactError::Refused(
                "attach_session did not complete".to_string(),
            ));
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
        Ok(SessionAttachmentRef::from_opaque(attachment_id))
    }

    async fn advertise_capabilities(
        &self,
        attachment: &SessionAttachmentRef,
        capabilities: &[String],
    ) -> Result<SessionAdvertiseReceiptView, SessionFactError> {
        let params = json!({
            "attachment_id": attachment.as_str(),
            "session_capabilities": capabilities,
        });
        let receipt = self.call("advertise_session_capabilities", params).await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(
                "advertise_session_capabilities did not complete".to_string(),
            ));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("advertise_session_capabilities") {
            return Err(SessionFactError::Refused(
                "advertise receipt action mismatch or missing".to_string(),
            ));
        }
        let advertisement_seq = match receipt.body.get("advertisement_seq") {
            Some(val) => {
                if let Some(seq) = val.as_i64() {
                    if seq < 0 {
                        return Err(SessionFactError::Refused(
                            "advertisement_seq is negative".to_string(),
                        ));
                    }
                    seq as u64
                } else if let Some(seq) = val.as_u64() {
                    if seq > i64::MAX as u64 {
                        return Err(SessionFactError::Refused(
                            "advertisement_seq exceeds i64::MAX".to_string(),
                        ));
                    }
                    seq
                } else {
                    return Err(SessionFactError::Refused(
                        "advertisement_seq is not a valid integer".to_string(),
                    ));
                }
            }
            None => {
                return Err(SessionFactError::Refused(
                    "advertise receipt carries no advertisement_seq".to_string(),
                ));
            }
        };
        // Tachi serializes HarnessSessionAttachmentCapabilities as eight booleans,
        // not the input array of capability names.
        let capability_fields = [
            "observe",
            "wait",
            "prompt",
            "cancel",
            "resume",
            "load",
            "events",
            "artifacts",
        ];
        let object = receipt
            .body
            .get("session_capabilities")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                SessionFactError::Refused(
                    "advertise receipt carries no capability object".to_string(),
                )
            })?;
        if object.len() != capability_fields.len() {
            return Err(SessionFactError::Refused(
                "advertise receipt has unexpected capability fields".to_string(),
            ));
        }
        let mut ret_capabilities = Vec::new();
        for name in capability_fields {
            match object.get(name) {
                Some(Value::Bool(true)) => ret_capabilities.push(name.to_string()),
                Some(Value::Bool(false)) => {}
                _ => {
                    return Err(SessionFactError::Refused(
                        "advertise receipt has missing or non-boolean capability".to_string(),
                    ));
                }
            }
        }
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
        let projected_digest = validate_and_project_payload_digest(fact.payload_digest.as_deref())?;

        // Envelope retention and same-material detection
        let payload = {
            let mut envelopes_guard = self.retained_envelopes.write();
            if let Some(existing) = envelopes_guard
                .get(attachment.as_str())
                .and_then(|events| events.get(fact.event_id.as_str()))
            {
                let retained_occurred_at = existing.occurred_at().ok_or_else(|| {
                    SessionFactError::Refused(
                        "retained envelope missing event_occurred_at".to_string(),
                    )
                })?;
                let candidate = json!({
                    "attachment_id": attachment.as_str(),
                    "session_event_id": fact.event_id.as_str(),
                    "session_event_kind": fact.kind.as_str(),
                    "session_event_outcome": fact.outcome.as_ref().map(|o| o.kind_name()),
                    "source_revision": fact.source_revision as i64,
                    "authority_confirmation_ref": projected_auth_ref,
                    "event_summary": projected_summary,
                    "payload_digest": projected_digest,
                    "event_occurred_at": retained_occurred_at,
                });
                if !existing.matches_material(&candidate) {
                    return Err(SessionFactError::Refused(
                        "same-ID material conflict: cannot change frozen event material"
                            .to_string(),
                    ));
                }
                existing.payload.clone()
            } else {
                let total_envelopes: usize = envelopes_guard.values().map(HashMap::len).sum();
                if total_envelopes >= self.max_total_envelopes {
                    return Err(SessionFactError::Refused(
                        "in-process event envelope total capacity exhausted (fail closed)"
                            .to_string(),
                    ));
                }
                if envelopes_guard
                    .get(attachment.as_str())
                    .map_or(0, HashMap::len)
                    >= self.max_envelopes_per_attachment
                {
                    return Err(SessionFactError::Refused(
                        "in-process event envelope capacity exhausted (fail closed)".to_string(),
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
                envelopes_guard
                    .entry(attachment.as_str().to_string())
                    .or_default()
                    .insert(
                        fact.event_id.as_str().to_string(),
                        RetainedEventEnvelope {
                            payload: serialized_payload.clone(),
                        },
                    );
                serialized_payload
            }
        };

        let receipt = self.call("ingest_session_event", payload).await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(
                "ingest_session_event did not complete".to_string(),
            ));
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
            return Err(SessionFactError::Refused(
                "event receipt attachment_id mismatch".to_string(),
            ));
        }
        let ret_event_id = receipt
            .body
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused("event receipt carries no event_id".to_string())
            })?;
        if ret_event_id != fact.event_id.as_str() {
            return Err(SessionFactError::Refused(
                "event receipt event_id mismatch".to_string(),
            ));
        }
        let admission = match receipt.body.get("admission").and_then(Value::as_str) {
            Some("journaled") => SessionReceiptAdmissionV1::Created,
            Some("replayed") => SessionReceiptAdmissionV1::Replayed,
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "event receipt carries unknown admission class".to_string(),
                ));
            }
            None => {
                return Err(SessionFactError::Refused(
                    "event receipt carries no admission class".to_string(),
                ));
            }
        };
        let disposition = match receipt.body.get("disposition").and_then(Value::as_str) {
            Some(
                d @ ("advanced"
                | "journaled_stale"
                | "journaled_terminal_conflict"
                | "journaled_redundant_terminal"),
            ) => d.to_string(),
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "event receipt carries unknown disposition".to_string(),
                ));
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
        let expected_revision = self.revision_for(attachment);
        let receipt = self
            .call(
                "request_intervention",
                json!({
                    "attachment_id": attachment.as_str(),
                    "intervention_request_id": request_id.as_str(),
                    "intervention_kind": kind.as_str(),
                    "intervention_reason": reason,
                    "expected_session_revision": expected_revision,
                }),
            )
            .await?;
        if receipt.status == "unsupported_by_lifecycle_owner" {
            return Err(facade_refusal("unsupported_by_lifecycle_owner"));
        }
        if receipt.status != "completed" {
            // The spine-gate's typed `unsupported_by_lifecycle_owner`
            // refusal arrives verbatim and is re-raised typed below.
            return Err(SessionFactError::Refused(
                "request_intervention did not complete".to_string(),
            ));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("request_intervention") {
            return Err(SessionFactError::Refused(
                "request_intervention receipt action mismatch or missing".to_string(),
            ));
        }
        let ret_att = receipt
            .body
            .pointer("/request/attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused(
                    "request_intervention receipt carries no attachment_id".to_string(),
                )
            })?;
        if ret_att != attachment.as_str() {
            return Err(SessionFactError::Refused(
                "request_intervention receipt attachment_id mismatch".to_string(),
            ));
        }
        if receipt
            .body
            .pointer("/request/request_id")
            .and_then(Value::as_str)
            != Some(request_id.as_str())
        {
            return Err(SessionFactError::Refused(
                "intervention receipt request_id mismatch or missing".to_string(),
            ));
        }
        validate_intervention_admission(&receipt.body)?;
        for (path, expected) in [
            ("/request/kind", json!(kind.as_str())),
            ("/request/reason", json!(reason)),
            (
                "/request/expected_session_revision",
                json!(expected_revision),
            ),
            ("/request/requested_by", json!(self.config.host_identity)),
        ] {
            require_receipt_value(&receipt.body, path, &expected)?;
        }
        let capability_source = receipt
            .body
            .get("capability_source")
            .and_then(Value::as_str);
        let known_source = matches!(capability_source, Some("advertised" | "declared"));
        // Legacy stored requests keep their historical source on replay;
        // fresh requests only resolve advertised or declared capabilities.
        let legacy_replay = capability_source == Some("legacy_unknown")
            && receipt.body.get("admission").and_then(Value::as_str) == Some("replayed");
        if !known_source && !legacy_replay {
            return Err(SessionFactError::Refused(
                "intervention receipt has invalid capability_source".to_string(),
            ));
        }
        let _ = Self::validate_state(&receipt.body)?;
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
            return Err(SessionFactError::Refused(
                "record_intervention_result did not complete".to_string(),
            ));
        }
        let action = receipt.body.get("action").and_then(Value::as_str);
        if action != Some("record_intervention_result") {
            return Err(SessionFactError::Refused(
                "record_intervention_result receipt action mismatch or missing".to_string(),
            ));
        }
        let ret_att = receipt
            .body
            .pointer("/result/attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused(
                    "record_intervention_result receipt carries no attachment_id".to_string(),
                )
            })?;
        if ret_att != attachment.as_str() {
            return Err(SessionFactError::Refused(
                "record_intervention_result receipt attachment_id mismatch".to_string(),
            ));
        }
        if receipt
            .body
            .pointer("/result/request_id")
            .and_then(Value::as_str)
            != Some(request_id.as_str())
        {
            return Err(SessionFactError::Refused(
                "intervention receipt request_id mismatch or missing".to_string(),
            ));
        }
        validate_intervention_admission(&receipt.body)?;
        if !matches!(
            receipt
                .body
                .pointer("/result/request_kind")
                .and_then(Value::as_str),
            Some(
                "request_status"
                    | "prompt_or_correct"
                    | "request_pause"
                    | "request_cancel"
                    | "request_resume"
            )
        ) {
            return Err(SessionFactError::Refused(
                "intervention result has invalid request_kind".to_string(),
            ));
        }
        for (path, expected) in [
            ("/result/disposition", json!(disposition.as_str())),
            (
                "/result/authority_confirmation_ref",
                json!(authority_confirmation_ref.filter(|value| !value.is_empty())),
            ),
            (
                "/result/detail",
                json!(detail.filter(|value| !value.is_empty())),
            ),
        ] {
            require_receipt_value(&receipt.body, path, &expected)?;
        }
        let _ = Self::validate_state(&receipt.body)?;
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
            return Err(SessionFactError::Refused(
                "mark_session_connection did not complete".to_string(),
            ));
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
            return Err(SessionFactError::Refused(
                "mark_session_connection receipt attachment_id mismatch".to_string(),
            ));
        }
        require_receipt_value(&receipt.body, "/fact", &json!(fact.as_str()))?;
        let target = match fact {
            SessionConnectionFactV1::Disconnected => "unknown",
            SessionConnectionFactV1::ReconnectFailed => "reconnect_failed",
        };
        require_receipt_value(&receipt.body, "/attachment_state", &json!(target))?;
        if !matches!(
            receipt
                .body
                .get("previous_attachment_state")
                .and_then(Value::as_str),
            Some("attached" | "unknown" | "reconnect_failed")
        ) || receipt
            .body
            .get("changed")
            .and_then(Value::as_bool)
            .is_none()
        {
            return Err(SessionFactError::Refused(
                "connection receipt has invalid transition fields".to_string(),
            ));
        }
        let _ = Self::validate_state(&receipt.body)?;
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
            return Err(SessionFactError::Refused(
                "reconnect_session did not complete".to_string(),
            ));
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
        require_receipt_value(&receipt.body, "/attachment_state", &json!("attached"))?;
        if !matches!(
            receipt
                .body
                .get("previous_attachment_state")
                .and_then(Value::as_str),
            Some("attached" | "reconnect_failed" | "unknown")
        ) {
            return Err(SessionFactError::Refused(
                "reconnect receipt has missing or unknown previous attachment state".to_string(),
            ));
        }
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
            Some(val) => {
                if let Some(rev) = val.as_i64() {
                    if rev < 0 {
                        return Err(SessionFactError::Refused(
                            "reconnect resume_from_revision is negative".to_string(),
                        ));
                    }
                    rev as u64
                } else if let Some(rev) = val.as_u64() {
                    if rev > i64::MAX as u64 {
                        return Err(SessionFactError::Refused(
                            "reconnect resume_from_revision exceeds i64::MAX".to_string(),
                        ));
                    }
                    rev
                } else {
                    return Err(SessionFactError::Refused(
                        "reconnect resume_from_revision is not a valid integer".to_string(),
                    ));
                }
            }
            None => {
                return Err(SessionFactError::Refused(
                    "reconnect receipt carries no resume_from_revision".to_string(),
                ));
            }
        };
        let state = {
            let state = Self::parse_state(&receipt.body)?;
            if resume_from_revision != state.canonical_revision {
                return Err(SessionFactError::Refused(
                    "reconnect resume revision does not match canonical projection".to_string(),
                ));
            }
            self.note_revision(&SessionAttachmentRef::from_opaque(attachment_id), &state);
            state
        };
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
            return Err(SessionFactError::Refused(
                "get_session_state did not complete".to_string(),
            ));
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
            return Err(SessionFactError::Refused(
                "get_session_state receipt attachment_id mismatch".to_string(),
            ));
        }
        let state = Self::parse_state(&receipt.body)?;
        self.note_revision(attachment, &state);
        Ok(state)
    }
}

#[cfg(test)]
#[path = "tachi_sink_tests.rs"]
mod tachi_sink_tests;
