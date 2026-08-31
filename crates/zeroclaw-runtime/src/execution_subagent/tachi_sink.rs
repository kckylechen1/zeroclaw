//! The production fact-sink transport — a real [`SessionFactSink`] that
//! consumes the PUBLIC tachi MCP facade (`tachi_agent_eval`, the #1678
//! attached-session receipt spine) over the existing MCP stdio client.
//! This is the production wire the stage-b JSON stand-in pointed at
//! (#261's honest gap); every sink operation is a real MCP `tools/call`
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

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::{Value, json};
use zeroclaw_api::session_exec::{
    InterventionRequestIdRef, SessionAdvertiseReceiptView, SessionAttachmentRef,
    SessionCanonicalStateV1, SessionConnectionFactV1, SessionEventReceiptView, SessionFactError,
    SessionInterventionDispositionV1, SessionInterventionKindV1, SessionInterventionRequestView,
    SessionReconnectReceiptView, SessionStateView,
};
use zeroclaw_config::schema::McpServerConfig;
use zeroclaw_tools::mcp_protocol::JsonRpcRequest;
use zeroclaw_tools::mcp_transport::{McpTransportConn, create_transport};

use super::facts::{SessionBinding, SessionEventFact, SessionFactSink};

/// The tachi facade tool this carrier consumes (the #1678 spine surface).
const TACHI_AGENT_EVAL_TOOL: &str = "tachi_agent_eval";
/// The refusal-text ceiling (mirrors the fact-summary bound).
const SUMMARY_CEILING: usize = 2000;
/// The MCP protocol revision the spine handshake negotiates.
const SPINE_PROTOCOL_VERSION: &str = "2025-06-18";

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
    /// Last observed canonical revision (the intervention gate's
    /// expected-session-revision input).
    last_revision: RwLock<u64>,
    next_id: RwLock<u64>,
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
            last_revision: RwLock::new(0),
            next_id: RwLock::new(1),
        })
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
        let mut conn = create_transport(&self.config.mcp_server_config()).map_err(|error| {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(serde_json::json!({ "detail": error.to_string() })),
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
        let response = conn
            .send_and_recv(&initialize)
            .await
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
        let state = body.get("canonical_state").ok_or_else(|| {
            SessionFactError::Refused("spine receipt carries no state".to_string())
        })?;
        let canonical = match state.get("canonical_state") {
            Some(Value::String(raw)) => SessionCanonicalStateV1::parse(raw)?,
            Some(Value::Null) | None => {
                return Err(SessionFactError::Refused(
                    "spine state projection has no canonical state".to_string(),
                ));
            }
            Some(_) => {
                return Err(SessionFactError::Refused(
                    "spine state projection is malformed".to_string(),
                ));
            }
        };
        Ok(SessionStateView {
            canonical_state: canonical,
            canonical_revision: state
                .get("canonical_revision")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cleanup_recorded: state
                .get("cleanup_recorded")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            conflicting_terminal: state
                .get("conflicting_terminal")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            last_event_id: state
                .get("last_event_id")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    fn note_revision(&self, view: &SessionStateView) {
        *self.last_revision.write() = view.canonical_revision.max(*self.last_revision.read());
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
        let attachment_id = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused("attach receipt carries no attachment id".to_string())
            })?;
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
        Ok(SessionAdvertiseReceiptView {
            attachment_ref: attachment.clone(),
            advertisement_seq: receipt
                .body
                .get("advertisement_seq")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            capabilities: capabilities.to_vec(),
        })
    }

    async fn ingest_event(
        &self,
        attachment: &SessionAttachmentRef,
        fact: &SessionEventFact,
    ) -> Result<SessionEventReceiptView, SessionFactError> {
        let receipt = self
            .call(
                "ingest_session_event",
                json!({
                    "attachment_id": attachment.as_str(),
                    "session_event_id": fact.event_id.as_str(),
                    "session_event_kind": fact.kind.as_str(),
                    "session_event_outcome": fact.outcome.as_ref().map(|outcome| outcome.kind_name()),
                    "source_revision": fact.source_revision,
                    "authority_confirmation_ref": fact.authority_confirmation_ref,
                    "event_summary": fact.summary,
                    "payload_digest": fact.payload_digest,
                    "event_occurred_at": now_rfc3339(),
                }),
            )
            .await?;
        if receipt.status != "completed" {
            return Err(SessionFactError::Refused(format!(
                "ingest_session_event status {}",
                receipt.status
            )));
        }
        let admission = match receipt.body.get("admission").and_then(Value::as_str) {
            Some("journaled") | Some("created") => SessionReceiptAdmissionLocal::Created,
            Some("replayed") => SessionReceiptAdmissionLocal::Replayed,
            _ => {
                return Err(SessionFactError::Refused(
                    "event receipt carries no admission class".to_string(),
                ));
            }
        };
        Ok(SessionEventReceiptView {
            attachment_ref: attachment.clone(),
            event_id: fact.event_id.clone(),
            admission: match admission {
                SessionReceiptAdmissionLocal::Created => {
                    zeroclaw_api::session_exec::SessionReceiptAdmissionV1::Created
                }
                SessionReceiptAdmissionLocal::Replayed => {
                    zeroclaw_api::session_exec::SessionReceiptAdmissionV1::Replayed
                }
            },
            disposition: receipt
                .body
                .get("disposition")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            state: {
                let state = Self::parse_state(&receipt.body)?;
                self.note_revision(&state);
                state
            },
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
                    "expected_session_revision": *self.last_revision.read(),
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
        let attachment_id = receipt
            .body
            .get("attachment_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                SessionFactError::Refused("reconnect receipt carries no attachment id".to_string())
            })?;
        *self.attachment.write() = Some(attachment_id.to_string());
        Ok(SessionReconnectReceiptView {
            attachment_ref: SessionAttachmentRef::from_opaque(attachment_id),
            reconnected: receipt
                .body
                .get("reconnected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            resume_from_revision: receipt
                .body
                .get("resume_from_revision")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            state: {
                let state = Self::parse_state(&receipt.body)?;
                self.note_revision(&state);
                state
            },
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
        let state = Self::parse_state(&receipt.body)?;
        self.note_revision(&state);
        Ok(state)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionReceiptAdmissionLocal {
    Created,
    Replayed,
}
