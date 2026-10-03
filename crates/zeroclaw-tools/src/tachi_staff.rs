//! `tachi_staff` client — the production transport of the Tachi bridge
//! (ADR-017 §3, §5; "ZeroClaw ↔ tachi_staff mapping").
//!
//! ```text
//! start(harness, task, reason, refs) → tachi_staff(action=start, profile=harnesses[harness], …)
//!                                      → StaffReceipt { dispatch_id, state, run_dir }
//! status(dispatch_id)                → tachi_staff(action=status)       → RunStatus
//! result(dispatch_id)                → tachi_task(action=status, include_result=true)
//!                                      → RunResult (result.md, Tachi caps it at 8000 chars)
//! cancel(dispatch_id, revision)      → tachi_staff(action=cancel, expected_status_revision)
//!                                      → CancelReceipt, or CancelUnsupported for CLI runs
//! ```
//!
//! Rules this client keeps:
//!
//! - **Fail closed, never fall back.** A disabled `[tachi]` section, a daemon
//!   that is down, or a broken transport is typed [`TachiStaffError::Unavailable`].
//!   This module holds no process or command capability; there is nothing
//!   local to fall back TO (source scan in `tests.rs`).
//! - **Tachi owns run truth.** The client keeps no ledger, cursor, or cache
//!   of runs. Every answer is read from Tachi when asked.
//! - **The caller names a harness, not an execution.** A harness name is
//!   resolved to a Tachi profile from `[tachi.harnesses]`; an unlisted name is
//!   [`TachiStaffError::UnknownHarness`] and Tachi is never contacted. Tachi
//!   resolves command, working directory, credentials, and sandbox from that
//!   profile.
//! - **Tolerant reads.** Responses are parsed for the fields used here only;
//!   extra fields are ignored so Tachi can grow its receipts freely.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
pub use zeroclaw_api::tachi_staff::*;

use crate::mcp_era::{PeerEra, PeerProtocol, attach_request_meta};
use crate::mcp_protocol::JsonRpcRequest;
use crate::mcp_transport::{
    McpRequestLifecycle, McpTransportError, SharedMcpTransportConn, create_shared_transport,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use zeroclaw_config::schema::{McpServerConfig, McpTransport};
use zeroclaw_config::tachi::TachiConfig;

/// The one Tachi tool that starts, reads, and cancels delegated runs.
pub const TACHI_STAFF_TOOL: &str = "tachi_staff";
/// Tachi's task read model; the only place `result.md` is served today
/// (kckylechen1/Tachi#2003 tracks moving it onto the staff facade).
pub const TACHI_TASK_TOOL: &str = "tachi_task";

/// Tool profile header. ZeroClaw always asks for the ordinary `standard`
/// profile; privileged Tachi profiles need a local proxy capability.
pub const HEADER_PROFILE: &str = "x-tachi-profile";
/// Caller identity header (`zeroclaw:<agent alias>` unless configured).
pub const HEADER_AGENT_IDENTITY: &str = "x-tachi-agent-identity";
/// Client product header.
pub const HEADER_CLIENT: &str = "x-tachi-client";
/// Project binding header, sent only when `[tachi].project` is set.
pub const HEADER_PROJECT: &str = "x-tachi-project";

const PROFILE_STANDARD: &str = "standard";
const CLIENT_NAME: &str = "zeroclaw";
/// Budget for one Tachi call. A start provisions an environment before it
/// returns its receipt, so it gets more room than a read.
const CALL_TIMEOUT_SECS: u64 = 120;

// ─────────────────────────────────────────────────────────────────────────
// Wire vocabulary
// ─────────────────────────────────────────────────────────────────────────

fn log_failure(op: &str, error: &TachiStaffError) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(json!({ "op": op, "error": zeroclaw_providers::sanitize_api_error(&error.to_string()) })),
        "tachi_staff: call failed"
    );
}

// ─────────────────────────────────────────────────────────────────────────
// Client
// ─────────────────────────────────────────────────────────────────────────

/// Settings the client runs with, resolved from `[tachi]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TachiStaffSettings {
    /// Loopback MCP endpoint.
    pub endpoint: String,
    /// `x-tachi-agent-identity` value.
    pub agent_identity: String,
    /// Tachi project name, if bound.
    pub project: Option<String>,
    /// Status poll interval.
    pub poll_interval: Duration,
    /// Harness name → Tachi profile.
    pub harnesses: HashMap<String, String>,
}

impl TachiStaffSettings {
    /// Resolve settings from `[tachi]` for the agent `agent_alias`.
    ///
    /// # Errors
    /// [`TachiStaffError::Unavailable`] when the section is disabled or
    /// invalid: delegation fails closed instead of guessing.
    pub fn from_config(config: &TachiConfig, agent_alias: &str) -> Result<Self, TachiStaffError> {
        if !config.enabled {
            return Err(TachiStaffError::Unavailable(
                "[tachi] is not enabled".to_string(),
            ));
        }
        if let Err((path, reason)) = config.validate() {
            return Err(TachiStaffError::Unavailable(format!(
                "invalid {path}: {reason}"
            )));
        }
        Ok(Self {
            endpoint: config.endpoint.trim().to_string(),
            agent_identity: config.resolved_agent_identity(agent_alias),
            project: config
                .project
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string),
            poll_interval: Duration::from_secs(config.poll_secs),
            harnesses: config
                .harnesses
                .iter()
                .map(|(name, profile)| (name.trim().to_string(), profile.trim().to_string()))
                .collect(),
        })
    }
}

/// Client of Tachi's `tachi_staff` MCP tool over ZeroClaw's MCP HTTP
/// transport. One MCP session is opened lazily and reused for this client's
/// lifetime. A transport failure drops it so the next call opens a fresh one.
pub struct TachiStaffClient {
    settings: TachiStaffSettings,
    session: Mutex<Option<StaffSession>>,
    next_id: AtomicU64,
}

impl fmt::Debug for TachiStaffClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TachiStaffClient")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// Outcome of one JSON-RPC exchange inside an open session.
enum Exchange {
    Answered(Value),
    StaleSession,
}

/// Negotiated wire facts belong to the transport session, never to a run.
struct StaffSession {
    transport: Box<dyn SharedMcpTransportConn>,
    peer: PeerProtocol,
}

impl TachiStaffClient {
    /// Build a client from `[tachi]` for the agent `agent_alias`. No network
    /// traffic happens until the first call.
    ///
    /// # Errors
    /// [`TachiStaffError::Unavailable`] when the section is disabled or
    /// invalid.
    pub fn from_config(config: &TachiConfig, agent_alias: &str) -> Result<Self, TachiStaffError> {
        Ok(Self::new(TachiStaffSettings::from_config(
            config,
            agent_alias,
        )?))
    }

    /// Build a client from resolved settings.
    #[must_use]
    pub fn new(settings: TachiStaffSettings) -> Self {
        Self {
            settings,
            session: Mutex::new(None),
            // Canonical bootstrap reserves discover=0 and initialize=1.
            next_id: AtomicU64::new(2),
        }
    }

    /// The configured status poll interval.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        self.settings.poll_interval
    }

    /// The Tachi profile a harness name maps to.
    ///
    /// # Errors
    /// [`TachiStaffError::UnknownHarness`] for an unlisted name.
    pub fn profile_for(&self, harness: &str) -> Result<&str, TachiStaffError> {
        self.settings
            .harnesses
            .get(harness.trim())
            .map(String::as_str)
            .ok_or_else(|| TachiStaffError::UnknownHarness(harness.trim().to_string()))
    }

    /// Start a delegated run on `harness`.
    ///
    /// # Errors
    /// `UnknownHarness` and an empty task are refused before Tachi is
    /// contacted. `Unavailable` means session setup failed before the start
    /// POST. After transmission an unreadable/missing answer is always
    /// `SubmissionUnknown`; an explicit refusal remains `Refused`.
    pub async fn start(
        &self,
        harness: &str,
        task: &str,
        reason: StaffingReason,
        refs: &StaffRefs,
    ) -> Result<StaffReceipt, TachiStaffError> {
        let profile = self.profile_for(harness)?.to_string();
        let task = task.trim();
        if task.is_empty() {
            return Err(TachiStaffError::Refused(
                "task must not be empty".to_string(),
            ));
        }
        let mut args = json!({
            "action": "start",
            "format": "json",
            "task": task,
            "staffing_reason": reason.as_str(),
            "profile": profile,
        });
        if let Some(object) = args.as_object_mut() {
            let optional = [
                ("project", self.settings.project.as_ref()),
                ("issue_ref", refs.issue_ref.as_ref()),
                ("pr_ref", refs.pr_ref.as_ref()),
                ("flow_id", refs.flow_id.as_ref()),
            ];
            for (key, value) in optional {
                if let Some(value) = value.map(|v| v.trim()).filter(|v| !v.is_empty()) {
                    object.insert(key.to_string(), Value::String(value.to_string()));
                }
            }
        }
        let payload = self.call("start", TACHI_STAFF_TOOL, args).await?;
        let receipt: StaffReceipt = decode("start", payload)
            .map_err(|error| TachiStaffError::SubmissionUnknown(error.to_string()))?;
        if receipt.dispatch_id.trim().is_empty() || receipt.run_dir.trim().is_empty() {
            return Err(TachiStaffError::SubmissionUnknown(
                "start receipt has no dispatch reference".to_string(),
            ));
        }
        Ok(receipt)
    }

    /// Read a run's canonical status.
    ///
    /// # Errors
    /// `Unavailable`, `Refused` (for example an unknown `dispatch_id`), or
    /// `Protocol`.
    pub async fn status(&self, dispatch_id: &str) -> Result<RunStatus, TachiStaffError> {
        let args = json!({
            "action": "status",
            "format": "json",
            "dispatch_id": dispatch_id,
        });
        let payload = self.call("status", TACHI_STAFF_TOOL, args).await?;
        let status: RunStatus = decode("status", payload)?;
        if status.dispatch_id != dispatch_id {
            return Err(TachiStaffError::Protocol(
                "status dispatch mismatch".to_string(),
            ));
        }
        Ok(status)
    }

    /// Read a run's report through `tachi_task(status, include_result)`.
    ///
    /// # Errors
    /// `Unavailable`, `Refused`, or `Protocol`.
    pub async fn result(&self, dispatch_id: &str) -> Result<RunResult, TachiStaffError> {
        #[derive(Deserialize)]
        struct TaskStatus {
            run_status: RunStatus,
            state: RunState,
            #[serde(default)]
            task: Option<TaskProjection>,
            #[serde(default)]
            managed_run: Option<Value>,
            #[serde(default)]
            result: Option<ResultBody>,
        }
        #[derive(Deserialize)]
        struct TaskProjection {
            #[serde(default)]
            state_basis: Option<String>,
        }
        #[derive(Deserialize)]
        struct ResultBody {
            #[serde(default)]
            body: Option<String>,
            #[serde(default)]
            truncated: bool,
        }
        let args = json!({
            "action": "status",
            "format": "json",
            "dispatch_id": dispatch_id,
            "include_result": true,
        });
        let payload = self.call("result", TACHI_TASK_TOOL, args).await?;
        let status: TaskStatus = decode("result", payload)?;
        if status.run_status.dispatch_id != dispatch_id {
            return Err(TachiStaffError::Protocol(
                "result dispatch mismatch".to_string(),
            ));
        }
        let (body, truncated) = status
            .result
            .map_or((None, false), |result| (result.body, result.truncated));
        Ok(RunResult {
            state: status.run_status.state.clone(),
            status: status.run_status,
            task_state: status.state,
            state_basis: status.task.and_then(|task| task.state_basis),
            managed_run: status.managed_run,
            body,
            truncated,
        })
    }

    /// Ask Tachi to cancel a run, quoting the revision last read by
    /// [`Self::status`].
    ///
    /// # Errors
    /// `CancelUnsupported` for runs Tachi cannot cancel (every CLI-backed run
    /// today), `StaleRevision` when the run moved on, `Refused` for any other
    /// typed refusal, plus `Unavailable` / `Protocol`.
    pub async fn cancel(
        &self,
        dispatch_id: &str,
        expected_revision: u64,
    ) -> Result<CancelReceipt, TachiStaffError> {
        #[derive(Deserialize)]
        struct Wire {
            receipt: String,
            #[serde(default)]
            reason: Option<String>,
            #[serde(default)]
            observed_status_revision: Option<u64>,
            #[serde(default)]
            state: Option<RunState>,
        }
        let args = json!({
            "action": "cancel",
            "dispatch_id": dispatch_id,
            "expected_status_revision": expected_revision,
        });
        let payload = self.call("cancel", TACHI_STAFF_TOOL, args).await?;
        let wire: Wire = decode("cancel", payload)?;
        let outcome = match wire.receipt.as_str() {
            "cancellation_requested" => CancelOutcome::Requested,
            "cancellation_confirmed" => CancelOutcome::Confirmed,
            "termination_unconfirmed" => CancelOutcome::TerminationUnconfirmed,
            "cancellation_unavailable" => {
                let reason = wire.reason.unwrap_or_else(|| "unspecified".to_string());
                let error = match reason.as_str() {
                    "stale_status_revision" => TachiStaffError::StaleRevision {
                        expected: expected_revision,
                        observed: wire.observed_status_revision,
                    },
                    "non_managed_custom_execution"
                    | "non_managed_custom_owner"
                    | "unsupported_platform"
                    | "controller_epoch_mismatch"
                    | "absent_same_daemon_handle" => TachiStaffError::CancelUnsupported { reason },
                    _ => TachiStaffError::Refused(format!("cancellation unavailable: {reason}")),
                };
                log_failure("cancel", &error);
                return Err(error);
            }
            other => {
                let error = TachiStaffError::Protocol(format!("unknown cancel receipt `{other}`"));
                log_failure("cancel", &error);
                return Err(error);
            }
        };
        Ok(CancelReceipt {
            outcome,
            state: wire.state,
        })
    }

    /// Poll [`Self::status`] every [`Self::poll_interval`] until the run is
    /// terminal or `max_wait` has passed; returns the last status read.
    ///
    /// # Errors
    /// The first failing status read.
    pub async fn wait_until_terminal(
        &self,
        dispatch_id: &str,
        max_wait: Duration,
    ) -> Result<RunStatus, TachiStaffError> {
        let deadline = tokio::time::Instant::now() + max_wait;
        let mut last = None;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return last.ok_or_else(|| {
                    TachiStaffError::Unavailable(
                        "status watch deadline elapsed before the first receipt".to_string(),
                    )
                });
            }
            let status = match tokio::time::timeout(remaining, self.status(dispatch_id)).await {
                Ok(result) => result?,
                Err(_) => {
                    return last.ok_or_else(|| {
                        TachiStaffError::Unavailable(
                            "status watch deadline elapsed before the first receipt".to_string(),
                        )
                    });
                }
            };
            if status.is_terminal() || tokio::time::Instant::now() >= deadline {
                return Ok(status);
            }
            last = Some(status);
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(self.settings.poll_interval.min(remaining)).await;
        }
    }

    // ── transport ───────────────────────────────────────────────────────

    fn transport_config(&self) -> McpServerConfig {
        let mut headers = HashMap::from([
            (HEADER_PROFILE.to_string(), PROFILE_STANDARD.to_string()),
            (
                HEADER_AGENT_IDENTITY.to_string(),
                self.settings.agent_identity.clone(),
            ),
            (HEADER_CLIENT.to_string(), CLIENT_NAME.to_string()),
        ]);
        if let Some(project) = &self.settings.project {
            headers.insert(HEADER_PROJECT.to_string(), project.clone());
        }
        McpServerConfig {
            name: "tachi".to_string(),
            transport: McpTransport::Http,
            url: Some(self.settings.endpoint.clone()),
            headers,
            tool_timeout_secs: Some(CALL_TIMEOUT_SECS),
            ..McpServerConfig::default()
        }
    }

    fn request(&self, method: &str, params: Value) -> JsonRpcRequest {
        JsonRpcRequest::new(self.next_id.fetch_add(1, Ordering::Relaxed), method, params)
    }

    async fn open_session(&self) -> Result<StaffSession, TachiStaffError> {
        let transport = create_shared_transport(&self.transport_config())
            .map_err(|err| TachiStaffError::Unavailable(format!("transport: {err:#}")))?;
        // Reuse the canonical discover/legacy bootstrap. Directly sending the
        // modern revision in initialize is rejected by modern Tachi peers.
        let opened = tokio::time::timeout(
            Duration::from_secs(CALL_TIMEOUT_SECS),
            crate::mcp_client::open_session(transport.as_ref(), "tachi", 0),
        )
        .await
        .map_err(|_| TachiStaffError::Unavailable("MCP bootstrap timed out".to_string()))?
        .map_err(|err| TachiStaffError::Unavailable(format!("{err:#}")))?;
        Ok(StaffSession {
            transport,
            peer: opened.peer,
        })
    }

    async fn exchange(
        &self,
        session: &StaffSession,
        tool: &str,
        args: &Value,
    ) -> Result<Exchange, TachiStaffError> {
        let params = json!({ "name": tool, "arguments": args.clone() });
        let params = match session.peer.era {
            PeerEra::Modern => attach_request_meta(params, &session.peer.version),
            PeerEra::Legacy => params,
        };
        let request = self.request("tools/call", params);
        let lifecycle = McpRequestLifecycle::uncoordinated_for_peer(0, &session.peer);
        let sent = tokio::time::timeout(
            Duration::from_secs(CALL_TIMEOUT_SECS),
            session.transport.send_and_recv(&request, &lifecycle),
        )
        .await
        .map_err(|_| TachiStaffError::Unavailable(format!("{tool} timed out")))?;
        let response = match sent {
            Ok(response) => response,
            Err(err)
                if matches!(
                    err.downcast_ref::<McpTransportError>(),
                    Some(McpTransportError::StaleSession { .. })
                ) =>
            {
                return Ok(Exchange::StaleSession);
            }
            Err(err) => return Err(TachiStaffError::Unavailable(format!("{err:#}"))),
        };
        if let Some(error) = response.error {
            return Err(TachiStaffError::Refused(format!(
                "{tool} ({}): {}",
                error.code, error.message
            )));
        }
        let result = response
            .result
            .ok_or_else(|| TachiStaffError::Protocol(format!("{tool}: empty result")))?;
        let content = result.get("content").and_then(Value::as_array);
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let text = content
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
            let detail = if text.is_empty() {
                format!("{tool} returned an error without detail")
            } else {
                text
            };
            return Err(TachiStaffError::Refused(detail));
        }
        // Tachi emits the canonical JSON in the first content block and may
        // append independent call diagnostics (including on cache hits).
        // Later blocks neither extend nor replace that authoritative payload.
        let text = content
            .and_then(|items| items.first())
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|item| item.get("text").and_then(Value::as_str))
            .ok_or_else(|| {
                TachiStaffError::Protocol(format!("{tool}: first content block is not text"))
            })?;
        serde_json::from_str(text)
            .map(Exchange::Answered)
            .map_err(|err| TachiStaffError::Protocol(format!("{tool}: result is not JSON: {err}")))
    }

    /// Retry stale sessions for reads only. A 404/410 arrives after the POST;
    /// it does not prove that a mutating request was never dispatched.
    async fn call(&self, op: &str, tool: &str, args: Value) -> Result<Value, TachiStaffError> {
        let mut session = self.session.lock().await;
        let mut reopened = false;
        loop {
            let conn = match session.take() {
                Some(conn) => conn,
                None => match self.open_session().await {
                    Ok(conn) => conn,
                    Err(error) => {
                        // Only this path can return Unavailable for start:
                        // tools/call has not been constructed or transmitted.
                        let error = if op == "start" {
                            TachiStaffError::Unavailable(error.to_string())
                        } else {
                            error
                        };
                        log_failure(op, &error);
                        return Err(error);
                    }
                },
            };
            match self.exchange(&conn, tool, &args).await {
                Ok(Exchange::Answered(payload)) => {
                    *session = Some(conn);
                    return Ok(payload);
                }
                Ok(Exchange::StaleSession) if matches!(op, "status" | "result") && !reopened => {
                    reopened = true;
                }
                Ok(Exchange::StaleSession) => {
                    let error = if op == "start" {
                        TachiStaffError::SubmissionUnknown(
                            "MCP session expired after start POST".to_string(),
                        )
                    } else {
                        TachiStaffError::Unavailable("MCP session expired".to_string())
                    };
                    log_failure(op, &error);
                    return Err(error);
                }
                Err(error) => {
                    let error = if op == "start" && !matches!(error, TachiStaffError::Refused(_)) {
                        TachiStaffError::SubmissionUnknown(error.to_string())
                    } else {
                        error
                    };
                    // A refusal leaves the session healthy; anything else
                    // drops it so the next call starts clean.
                    if matches!(error, TachiStaffError::Refused(_)) {
                        *session = Some(conn);
                    }
                    log_failure(op, &error);
                    return Err(error);
                }
            }
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(op: &str, payload: Value) -> Result<T, TachiStaffError> {
    serde_json::from_value(payload).map_err(|err| {
        let error = TachiStaffError::Protocol(format!("{op}: {err}"));
        log_failure(op, &error);
        error
    })
}
