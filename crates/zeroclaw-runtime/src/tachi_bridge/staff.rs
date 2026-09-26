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

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use zeroclaw_config::schema::{McpServerConfig, McpTransport};
use zeroclaw_config::tachi::TachiConfig;
use zeroclaw_tools::mcp_protocol::{JsonRpcRequest, MCP_PROTOCOL_VERSION};
use zeroclaw_tools::mcp_transport::{McpTransportConn, McpTransportError, create_transport};

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

/// Why the body is handing work to Tachi instead of doing it itself. Mirrors
/// Tachi's `TachiDispatchReason`; Tachi refuses a start without one.
///
/// Use [`StaffingReason::ExplicitUserRequest`] when the owner asked for the
/// delegation. Otherwise the model picks the reason that is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaffingReason {
    /// The owner explicitly asked for this work to be delegated.
    ExplicitUserRequest,
    /// The work must outlive this conversation or process.
    DurableCrossSession,
    /// The work must run on, or be reachable from, another device.
    CrossDeviceRemote,
    /// The body has no native way to do this work itself.
    NativeSubagentUnavailable,
}

impl StaffingReason {
    /// Every reason, in Tachi's order.
    pub const ALL: [Self; 4] = [
        Self::ExplicitUserRequest,
        Self::DurableCrossSession,
        Self::CrossDeviceRemote,
        Self::NativeSubagentUnavailable,
    ];

    /// The wire spelling Tachi expects.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitUserRequest => "explicit_user_request",
            Self::DurableCrossSession => "durable_cross_session",
            Self::CrossDeviceRemote => "cross_device_remote",
            Self::NativeSubagentUnavailable => "native_subagent_unavailable",
        }
    }

    /// Parse a wire spelling (for a model-chosen reason). Unknown text is
    /// `None`, never a default reason.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|reason| reason.as_str() == value.trim())
    }
}

/// Optional references bound to a start. Tachi stores them on the run; they
/// never select how the run executes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaffRefs {
    /// GitHub issue reference, for example `owner/repo#123`.
    pub issue_ref: Option<String>,
    /// GitHub pull request reference.
    pub pr_ref: Option<String>,
    /// Tachi flow id for feature-scoped linkage.
    pub flow_id: Option<String>,
}

/// A Tachi task state (`TASK_STATE_*`). Unknown states are kept verbatim so
/// a newer Tachi never makes a read fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    /// `TASK_STATE_SUBMITTED`.
    Submitted,
    /// `TASK_STATE_WORKING`.
    Working,
    /// `TASK_STATE_INPUT_REQUIRED`: terminal only when `closure_kind` is
    /// `partial`.
    InputRequired,
    /// `TASK_STATE_COMPLETED`.
    Completed,
    /// `TASK_STATE_FAILED`.
    Failed,
    /// `TASK_STATE_CANCELED`.
    Canceled,
    /// Any other state string.
    Other(String),
}

impl RunState {
    /// Parse a Tachi state string.
    #[must_use]
    pub fn from_wire(value: &str) -> Self {
        match value {
            "TASK_STATE_SUBMITTED" => Self::Submitted,
            "TASK_STATE_WORKING" => Self::Working,
            "TASK_STATE_INPUT_REQUIRED" => Self::InputRequired,
            "TASK_STATE_COMPLETED" => Self::Completed,
            "TASK_STATE_FAILED" => Self::Failed,
            "TASK_STATE_CANCELED" => Self::Canceled,
            other => Self::Other(other.to_string()),
        }
    }

    /// The Tachi state string.
    #[must_use]
    pub fn as_wire(&self) -> &str {
        match self {
            Self::Submitted => "TASK_STATE_SUBMITTED",
            Self::Working => "TASK_STATE_WORKING",
            Self::InputRequired => "TASK_STATE_INPUT_REQUIRED",
            Self::Completed => "TASK_STATE_COMPLETED",
            Self::Failed => "TASK_STATE_FAILED",
            Self::Canceled => "TASK_STATE_CANCELED",
            Self::Other(other) => other,
        }
    }
}

impl<'de> Deserialize<'de> for RunState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&raw))
    }
}

impl fmt::Display for RunState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// What Tachi answers to an accepted start. Tachi guarantees these three
/// fields and a durable `status.json` before it answers.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StaffReceipt {
    /// Canonical run id; every later call names the run by it.
    pub dispatch_id: String,
    /// Initial state, `TASK_STATE_WORKING` for a fresh run.
    pub state: RunState,
    /// Tachi's run directory (on the Tachi machine).
    pub run_dir: String,
}

/// A projection of Tachi's canonical `status.json` for one run.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunStatus {
    /// Canonical run id.
    pub dispatch_id: String,
    /// Current state.
    pub state: RunState,
    /// Receipt revision; `cancel` must quote it. Historical receipts may not
    /// carry one.
    #[serde(default, rename = "status_revision")]
    pub revision: Option<u64>,
    /// `partial` closes an `INPUT_REQUIRED` run.
    #[serde(default)]
    pub closure_kind: Option<String>,
    /// Whether the run has written `result.md`.
    #[serde(default)]
    pub result_written: Option<bool>,
    /// Last update time as Tachi wrote it.
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl RunStatus {
    /// True when Tachi will not move this run any further: completed,
    /// failed, canceled, or input-required with a partial closure.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        match self.state {
            RunState::Completed | RunState::Failed | RunState::Canceled => true,
            RunState::InputRequired => self.closure_kind.as_deref() == Some("partial"),
            _ => false,
        }
    }
}

/// A run's report (`result.md`), as far as Tachi serves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    /// State at the time of the read.
    pub state: RunState,
    /// Report text; `None` when the run has not written one.
    pub body: Option<String>,
    /// True when Tachi cut the report at its response cap.
    pub truncated: bool,
}

/// How far an accepted cancellation got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Recorded; the run has not stopped yet.
    Requested,
    /// The run is stopped and canceled.
    Confirmed,
    /// Tachi could not prove the process stopped; the run is failed.
    TerminationUnconfirmed,
}

/// Tachi's cancellation receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelReceipt {
    /// What happened.
    pub outcome: CancelOutcome,
    /// Run state the receipt reports, when it reports one.
    pub state: Option<RunState>,
}

/// Every way a `tachi_staff` call can fail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TachiStaffError {
    /// Tachi is disabled, not configured, or not reachable. Delegation stops
    /// here; nothing is run locally instead.
    #[error("Tachi unavailable: {0}")]
    Unavailable(String),
    /// Tachi answered and refused (validation, admission, unknown run, …).
    #[error("Tachi refused the request: {0}")]
    Refused(String),
    /// The harness name is not listed in `[tachi.harnesses]`.
    #[error("unknown harness `{0}`: not listed in [tachi.harnesses]")]
    UnknownHarness(String),
    /// Tachi cannot cancel this run (today: every CLI-backed run; only
    /// same-daemon managed-custom runs are cancellable).
    #[error("Tachi cannot cancel this run ({reason})")]
    CancelUnsupported { reason: String },
    /// `cancel` quoted a revision the run has moved past; read status again.
    #[error("stale status revision: expected {expected}, run is at {observed:?}")]
    StaleRevision {
        expected: u64,
        observed: Option<u64>,
    },
    /// Tachi answered with something this client cannot read.
    #[error("unreadable Tachi response: {0}")]
    Protocol(String),
}

fn log_failure(op: &str, error: &TachiStaffError) {
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
            .with_attrs(json!({ "op": op, "error": error.to_string() })),
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
/// transport. One MCP session is opened lazily and reused; a transport
/// failure drops it so the next call opens a fresh one.
pub struct TachiStaffClient {
    settings: TachiStaffSettings,
    session: Mutex<Option<Box<dyn McpTransportConn>>>,
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
            next_id: AtomicU64::new(1),
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
    /// contacted; otherwise `Unavailable`, `Refused`, or `Protocol`.
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
        decode("start", payload)
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
        decode("status", payload)
    }

    /// Read a run's report through `tachi_task(status, include_result)`.
    ///
    /// # Errors
    /// `Unavailable`, `Refused`, or `Protocol`.
    pub async fn result(&self, dispatch_id: &str) -> Result<RunResult, TachiStaffError> {
        #[derive(Deserialize)]
        struct TaskStatus {
            state: RunState,
            #[serde(default)]
            result: Option<ResultBody>,
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
        let (body, truncated) = status
            .result
            .map_or((None, false), |result| (result.body, result.truncated));
        Ok(RunResult {
            state: status.state,
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
        loop {
            let status = self.status(dispatch_id).await?;
            let now = tokio::time::Instant::now();
            if status.is_terminal() || now >= deadline {
                return Ok(status);
            }
            let remaining = deadline.saturating_duration_since(now);
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

    async fn open_session(&self) -> Result<Box<dyn McpTransportConn>, TachiStaffError> {
        let mut conn = create_transport(&self.transport_config())
            .map_err(|err| TachiStaffError::Unavailable(format!("transport: {err:#}")))?;
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        let response = tokio::time::timeout(
            Duration::from_secs(CALL_TIMEOUT_SECS),
            conn.send_and_recv(&init),
        )
        .await
        .map_err(|_| TachiStaffError::Unavailable("initialize timed out".to_string()))?
        .map_err(|err| TachiStaffError::Unavailable(format!("{err:#}")))?;
        if let Some(error) = response.error {
            return Err(TachiStaffError::Refused(format!(
                "initialize rejected ({}): {}",
                error.code, error.message
            )));
        }
        // Notifications carry no answer; a failure here surfaces on the
        // first real call instead.
        let initialized = JsonRpcRequest::notification("notifications/initialized", json!({}));
        let _ = conn.send_and_recv(&initialized).await;
        Ok(conn)
    }

    async fn exchange(
        &self,
        conn: &mut Box<dyn McpTransportConn>,
        tool: &str,
        args: &Value,
    ) -> Result<Exchange, TachiStaffError> {
        let request = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": args.clone() }),
        );
        let sent = tokio::time::timeout(
            Duration::from_secs(CALL_TIMEOUT_SECS),
            conn.send_and_recv(&request),
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
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            let detail = if text.is_empty() {
                format!("{tool} returned an error without detail")
            } else {
                text
            };
            return Err(TachiStaffError::Refused(detail));
        }
        serde_json::from_str(&text)
            .map(Exchange::Answered)
            .map_err(|err| TachiStaffError::Protocol(format!("{tool}: result is not JSON: {err}")))
    }

    /// Call one Tachi tool and return its JSON payload. A stale MCP session
    /// (Tachi restarted) is reopened once; the request never reached a live
    /// session, so resending it cannot double-start a run.
    async fn call(&self, op: &str, tool: &str, args: Value) -> Result<Value, TachiStaffError> {
        let mut session = self.session.lock().await;
        let mut reopened = false;
        loop {
            let mut conn = match session.take() {
                Some(conn) => conn,
                None => match self.open_session().await {
                    Ok(conn) => conn,
                    Err(error) => {
                        log_failure(op, &error);
                        return Err(error);
                    }
                },
            };
            match self.exchange(&mut conn, tool, &args).await {
                Ok(Exchange::Answered(payload)) => {
                    *session = Some(conn);
                    return Ok(payload);
                }
                Ok(Exchange::StaleSession) if !reopened => {
                    reopened = true;
                }
                Ok(Exchange::StaleSession) => {
                    let error =
                        TachiStaffError::Unavailable("MCP session went stale twice".to_string());
                    log_failure(op, &error);
                    return Err(error);
                }
                Err(error) => {
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
