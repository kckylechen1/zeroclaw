//! The typed SessionController port — the ZeroClaw-owned lifecycle seam
//! between the ExecutionSubAgent and the ephemeral harness session over
//! ACPX (the ephemeral-execution vertical; the tachi attached-session spine's host-side contract).
//!
//! ```text
//! ExecutionSubagentTool (parent-side, bounded)
//!   → SessionController port                    (THIS FILE)
//!      start / watch / prompt / interrupt / stop / collect / reattach
//!   → ACPX transport (host-configured; NOT a port parameter)
//!   → Codex / Claude / GLM session (operates the repository)
//! ```
//!
//! Authority boundaries encoded here:
//!
//! - **The host owns the lifecycle; the port owns the vocabulary.** The
//!   transport implementation is constructed with the host's own
//!   workspace/transport binding. The port's request types carry NO
//!   workspace path, no CLI flags, no credentials — the compile-level
//!   signature is the negative-capability evidence.
//! - **Capability-gated operations.** Every lifecycle op checks the
//!   session's advertised capability set and returns a typed
//!   [`ControllerError::UnsupportedByLifecycleOwner`] when the set does
//!   not admit it. There is no "do it anyway" path and no fake success.
//! - **Fail closed.** [`ControllerError::Unavailable`] is terminal for
//!   the ephemeral path: callers must not degrade to local execution.
//! - **Reconnect honors the spine's recovery semantics.**
//!   [`SessionController::reattach`] resumes from the spine-issued
//!   `resume_from_revision`; facts after that revision replay exactly
//!   once (the sink dedups by event id), and an `unknown_orphaned`
//!   canonical state is recoverable by authoritative facts — never
//!   guessed into failed/completed.
//! - **Bounded collect.** [`SessionCollectView`] carries a bounded
//!   summary, a digest, and evidence refs — never a transcript.

use async_trait::async_trait;
use zeroclaw_api::session_exec::{
    AdapterConnectionRef, AuthorityConfirmationRef, RemoteSessionRef, SessionEventIdRef,
    SessionEventKindV1, SessionTerminalOutcomeV1,
};

/// The closed ACP session capability vocabulary (mirrors the spine's
/// declared/advertised set: observe / wait / prompt / cancel / resume /
/// load / events / artifacts).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionCapabilities {
    pub observe: bool,
    pub wait: bool,
    pub prompt: bool,
    pub cancel: bool,
    pub resume: bool,
    pub load: bool,
    pub events: bool,
    pub artifacts: bool,
}

impl SessionCapabilities {
    /// Parse the closed name set; unknown names refuse (deny-by-default —
    /// the same closed set the spine enforces on attachments).
    pub fn from_names(names: &[String]) -> Result<Self, ControllerError> {
        let mut caps = Self::default();
        for name in names {
            match name.trim() {
                "observe" => caps.observe = true,
                "wait" => caps.wait = true,
                "prompt" => caps.prompt = true,
                "cancel" => caps.cancel = true,
                "resume" => caps.resume = true,
                "load" => caps.load = true,
                "events" => caps.events = true,
                "artifacts" => caps.artifacts = true,
                "" => {
                    return Err(ControllerError::Refused(
                        "session capability cannot be empty".to_string(),
                    ));
                }
                other => {
                    return Err(ControllerError::Refused(format!(
                        "unsupported session capability {other:?}"
                    )));
                }
            }
        }
        Ok(caps)
    }

    #[must_use]
    pub fn as_names(&self) -> Vec<&'static str> {
        [
            ("observe", self.observe),
            ("wait", self.wait),
            ("prompt", self.prompt),
            ("cancel", self.cancel),
            ("resume", self.resume),
            ("load", self.load),
            ("events", self.events),
            ("artifacts", self.artifacts),
        ]
        .into_iter()
        .filter_map(|(name, on)| on.then_some(name))
        .collect()
    }

    fn admits(&self, name: &str) -> bool {
        match name {
            "observe" => self.observe,
            "wait" => self.wait,
            "prompt" => self.prompt,
            "cancel" => self.cancel,
            "resume" => self.resume,
            "load" => self.load,
            "events" => self.events,
            "artifacts" => self.artifacts,
            _ => false,
        }
    }

    /// The intersection of two capability sets: a session carries a
    /// capability only when BOTH sources admit it.
    #[must_use]
    pub fn intersection(self, other: Self) -> Self {
        Self {
            observe: self.observe && other.observe,
            wait: self.wait && other.wait,
            prompt: self.prompt && other.prompt,
            cancel: self.cancel && other.cancel,
            resume: self.resume && other.resume,
            load: self.load && other.load,
            events: self.events && other.events,
            artifacts: self.artifacts && other.artifacts,
        }
    }

    /// The typed operation gate: the SINGLE mapping from the six execution-vertical
    /// operations to the closed capability set. `Some(operation)` when the
    /// set does not admit the operation (typed refusal); `None` when it
    /// does. start/reattach-admission are host-minting operations and are
    /// not gated here.
    #[must_use]
    pub fn unsupported_operation(&self, operation: &str) -> Option<String> {
        let (required, op) = match operation {
            "watch" => ("events", "watch"),
            "prompt" => ("prompt", "prompt"),
            "interrupt" | "stop" => ("cancel", "stop"),
            "collect" => ("observe", "collect"),
            other => ("", other),
        };
        if !required.is_empty() && self.admits(required) {
            None
        } else {
            Some(op.to_string())
        }
    }
}

/// What the host asks the controller to start: an ephemeral harness
/// session bound to the objective's bounded prompt. NO workspace path,
/// NO CLI flags, NO credentials — the transport was constructed with the
/// host's own binding; the request cannot widen it. The remote session
/// identity is MINTED BY THE TRANSPORT at start (the consumer cannot
/// choose it — it only observes it on the returned handle).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStartSpec {
    pub adapter_connection: AdapterConnectionRef,
    /// Bounded objective prompt (already admission-scanned by the tool).
    pub prompt: String,
    /// The context projection digest carried for provenance only.
    pub context_digest: String,
    /// The capability set the host declares for this session (the set the
    /// spine attachment will carry).
    pub capabilities: SessionCapabilities,
    /// Bounded prompt ceiling enforcement happens at the port boundary.
    pub max_prompt_bytes: usize,
}

/// A host-minted handle for one live session. The remote session identity
/// is minted by the transport at `start` and is OBSERVED here, never
/// chosen by the caller. Opaque: the subagent can hold it and pass it
/// back, not introspect transport details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionHandle {
    pub remote_session: RemoteSessionRef,
    pub capabilities: SessionCapabilities,
}

/// One event observed from the harness session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControllerEvent {
    pub seq: u64,
    pub event_id: SessionEventIdRef,
    pub kind: SessionEventKindV1,
    pub outcome: Option<SessionTerminalOutcomeV1>,
    /// Bounded progress text (no transcript; transport-bounded).
    pub summary: Option<String>,
}

/// One page of watched events. `next_seq` is monotone: it never goes
/// backwards, even when a transport returns stale pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEventPage {
    pub events: Vec<ControllerEvent>,
    pub next_seq: u64,
}

/// Receipt for a prompt/correct delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptReceipt {
    pub accepted: bool,
    pub detail: Option<String>,
}

/// Receipt for a stop/cancel request. A request is never a confirmation:
/// `confirmed` carries the authority confirmation reference that the
/// eventual terminal `cancelled` fact must bind to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStopReceipt {
    pub confirmed: bool,
    pub authority_confirmation_ref: Option<AuthorityConfirmationRef>,
    pub detail: Option<String>,
}

/// The bounded terminal projection from `collect`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionCollectView {
    /// Bounded terminal summary (transport-bounded, presence-blind to
    /// any transcript).
    pub summary: Option<String>,
    /// Digest over the terminal projection (hex).
    pub digest: String,
    /// Evidence refs (artifact refs, no content).
    pub evidence_refs: Vec<String>,
}

/// Typed controller failures. `Unavailable` is fail-closed (no local
/// fallback exists); `UnsupportedByLifecycleOwner` is the typed refusal
/// for capability-gated ops (never fake success).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControllerError {
    Unavailable,
    UnsupportedByLifecycleOwner { operation: String },
    Refused(String),
}

impl std::fmt::Display for ControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "session controller unavailable (fail closed)"),
            Self::UnsupportedByLifecycleOwner { operation } => write!(
                f,
                "operation {operation:?} is unsupported by this session's lifecycle owner"
            ),
            Self::Refused(reason) => write!(f, "session controller refused: {reason}"),
        }
    }
}

impl std::error::Error for ControllerError {}

/// The typed SessionController port. Exactly the six execution-vertical operations
/// (start / watch / prompt / interrupt+stop / collect) plus the reconnect
/// consumption op (`reattach`) — no raw process control surface exists on
/// the trait, so no caller of the port can acquire one.
#[async_trait]
pub trait SessionController: Send + Sync {
    /// Start an ephemeral harness session (host-minted handle).
    async fn start(&self, spec: &SessionStartSpec) -> Result<SessionHandle, ControllerError>;

    /// Watch events after `after_seq` (durable backfill / reconnect leg).
    async fn watch(
        &self,
        handle: &SessionHandle,
        after_seq: u64,
        limit: usize,
    ) -> Result<SessionEventPage, ControllerError>;

    /// Deliver a prompt or correction to the session.
    async fn prompt(
        &self,
        handle: &SessionHandle,
        text: &str,
    ) -> Result<PromptReceipt, ControllerError>;

    /// Request an interrupt of the current turn (best-effort; distinct
    /// from stop). Capability-gated.
    async fn interrupt(&self, handle: &SessionHandle) -> Result<(), ControllerError>;

    /// Stop the session. Capability-gated; a receipt is not a terminal
    /// fact — the spine's terminal event remains the truth.
    async fn stop(
        &self,
        handle: &SessionHandle,
        graceful: bool,
    ) -> Result<SessionStopReceipt, ControllerError>;

    /// Collect the bounded terminal projection.
    async fn collect(&self, handle: &SessionHandle) -> Result<SessionCollectView, ControllerError>;

    /// Reattach after attachment loss: resumes from `resume_from_revision`
    /// (the spine-issued recovery revision). Returns the live handle when
    /// the session still exists; a typed refusal when it cannot be
    /// recovered.
    async fn reattach(
        &self,
        adapter_connection: &AdapterConnectionRef,
        remote_session: &RemoteSessionRef,
        resume_from_revision: u64,
    ) -> Result<SessionHandle, ControllerError>;
}

// ─────────────────────────────────────────────────────────────────────────
// Transport-independent client law (capability gates + monotone cursor)
// ─────────────────────────────────────────────────────────────────────────

/// The gated client every consumer of the port MUST hold (the tool holds
/// exactly this type, never a raw `dyn SessionController`). It enforces,
/// once and transport-independently:
///
/// 1. **Capability gates.** watch/prompt/interrupt/stop/collect consult
///    the handle's advertised set via
///    [`SessionCapabilities::unsupported_operation`] and return the typed
///    [`ControllerError::UnsupportedByLifecycleOwner`] refusal — never a
///    degraded attempt, never fake success.
/// 2. **Monotone watch cursor.** `watch_events` never returns a page
///    whose `next_seq` regresses below the caller's `after_seq`; a stale
///    transport page is clamped, so consumers cannot replay facts they
///    already advanced past.
#[derive(Clone)]
pub struct GatedSessionController {
    inner: std::sync::Arc<dyn SessionController>,
}

impl GatedSessionController {
    #[must_use]
    pub fn new(inner: std::sync::Arc<dyn SessionController>) -> Self {
        Self { inner }
    }

    #[must_use]
    pub fn binding_label(&self) -> &'static str {
        // Non-secret binding label carried in reports; the transport's
        // own identity stays opaque behind the port.
        "typed-session-controller"
    }

    pub async fn start(&self, spec: &SessionStartSpec) -> Result<SessionHandle, ControllerError> {
        if spec.prompt.len() > spec.max_prompt_bytes {
            return Err(ControllerError::Refused(format!(
                "prompt of {} bytes exceeds the bounded ceiling {}",
                spec.prompt.len(),
                spec.max_prompt_bytes
            )));
        }
        self.inner.start(spec).await
    }

    pub async fn watch_events(
        &self,
        handle: &SessionHandle,
        after_seq: u64,
        limit: usize,
    ) -> Result<SessionEventPage, ControllerError> {
        if let Some(op) = handle.capabilities.unsupported_operation("watch") {
            return Err(ControllerError::UnsupportedByLifecycleOwner { operation: op });
        }
        let page = self.inner.watch(handle, after_seq, limit).await?;
        // Monotone law: the cursor never regresses.
        Ok(SessionEventPage {
            events: page.events,
            next_seq: page.next_seq.max(after_seq),
        })
    }

    pub async fn prompt(
        &self,
        handle: &SessionHandle,
        text: &str,
    ) -> Result<PromptReceipt, ControllerError> {
        if let Some(op) = handle.capabilities.unsupported_operation("prompt") {
            return Err(ControllerError::UnsupportedByLifecycleOwner { operation: op });
        }
        self.inner.prompt(handle, text).await
    }

    pub async fn interrupt(&self, handle: &SessionHandle) -> Result<(), ControllerError> {
        if let Some(op) = handle.capabilities.unsupported_operation("interrupt") {
            return Err(ControllerError::UnsupportedByLifecycleOwner { operation: op });
        }
        self.inner.interrupt(handle).await
    }

    pub async fn stop(
        &self,
        handle: &SessionHandle,
        graceful: bool,
    ) -> Result<SessionStopReceipt, ControllerError> {
        if let Some(op) = handle.capabilities.unsupported_operation("stop") {
            return Err(ControllerError::UnsupportedByLifecycleOwner { operation: op });
        }
        self.inner.stop(handle, graceful).await
    }

    pub async fn collect(
        &self,
        handle: &SessionHandle,
    ) -> Result<SessionCollectView, ControllerError> {
        if let Some(op) = handle.capabilities.unsupported_operation("collect") {
            return Err(ControllerError::UnsupportedByLifecycleOwner { operation: op });
        }
        self.inner.collect(handle).await
    }

    pub async fn reattach(
        &self,
        adapter_connection: &AdapterConnectionRef,
        remote_session: &RemoteSessionRef,
        resume_from_revision: u64,
    ) -> Result<SessionHandle, ControllerError> {
        self.inner
            .reattach(adapter_connection, remote_session, resume_from_revision)
            .await
    }
}
