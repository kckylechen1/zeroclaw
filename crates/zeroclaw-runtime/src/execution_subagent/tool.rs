//! The ExecutionSubAgent tool surface — the Parent-side bounded
//! supervisor that runs one ephemeral harness session end to end
//! (zeroclaw the vertical; the V1 the SubAgent freeze pattern: bounded profile, digest-bound
//! bundle, per-run meters, lineage depth 1, structured report).
//!
//! The run's ONLY outbound surfaces are the typed [`GatedSessionController`]
//! (lifecycle vocabulary) and the receipts-only [`SessionFactSink`]. The
//! tool drives: start → attach → advertise → accepted → watch loop (facts
//! mirrored to the spine as they happen) → corrections within the frozen
//! per-run ceiling → terminal (with the spine's cancel receipt chain when
//! the run stops the session) → cleanup → collect → report.
//!
//! Fail-closed law: controller or spine unavailability ends the run
//! typed (`Refused`/`Failed`) — there is NO local-execution path behind
//! this tool, and an unsupported lifecycle operation surfaces as the
//! typed `UnsupportedOperation` status, never a fake success.
//!
//! Negative capability: the child context this tool creates is the
//! [`ExecutionSessionInventory`] — a serialized-key-pinned type with no
//! field for shell, file write/edit, git, worktree, CLI flags, or
//! credentials. The harness behind the session operates the repository;
//! the subagent is a supervisor.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::subagent_v1::SubAgentBudgetMeter;
use async_trait::async_trait;
use zeroclaw_api::session_exec::{
    AdapterConnectionRef, ExecutionInterventionRecordV1, ExecutionRouteV1, ExecutionRunStatusV1,
    ExecutionSessionReportV1, ExecutionUsageV1, HostIdentityRef, InterventionRequestIdRef,
    RemoteSessionRef, SessionAttachmentRef, SessionCanonicalStateV1, SessionConnectionFactV1,
    SessionEventIdRef, SessionEventKindV1, SessionEventReceiptView,
    SessionInterventionDispositionV1, SessionInterventionKindV1, SessionTerminalOutcomeV1,
};
use zeroclaw_api::subagent_v1::{
    BundleRedactionPolicy, ContextBundleV1, ContextClassV1, LineageRef, ParentRunRef,
    SubAgentBudgetV1,
};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};

use super::controller::{
    ControllerError, GatedSessionController, SessionCapabilities, SessionStartSpec,
};
use super::facts::{SessionBinding, SessionEventFact, SessionFactSink};

// ─────────────────────────────────────────────────────────────────────────
// The frozen execution profile (the SubAgent freeze pattern: admitted, immutable)
// ─────────────────────────────────────────────────────────────────────────

/// The frozen v1 execution profile. Immutable for one run; any capability
/// change is a new revision with a new digest (the SubAgent freeze capability law).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionSubagentProfile {
    pub profile_id: String,
    pub revision: u32,
    pub digest: String,
    pub budget: SubAgentBudgetV1,
    /// Maximum prompt/correct deliveries in one run (the bounded
    /// correction ceiling).
    pub max_corrections: u32,
    /// Prompt ceiling enforced at the controller port boundary.
    pub max_prompt_bytes: usize,
    /// The capability set the host declares for sessions it starts (and
    /// the set the spine attachment carries).
    pub declared_capabilities: Vec<&'static str>,
}

impl ExecutionSubagentProfile {
    /// The frozen default: short-lived supervision ceiling — small action
    /// budget, two corrections, bounded prompts, observe/prompt/cancel/
    /// resume/events. NO load/artifacts in v1: collect is read-only and
    /// evidence refs arrive through events, not bulk fetches.
    #[must_use]
    pub fn default_execution_profile() -> Self {
        let profile = Self {
            profile_id: "execution-subagent-v1".to_string(),
            revision: 1,
            digest: String::new(),
            budget: SubAgentBudgetV1 {
                time_limit_secs: 600,
                max_tokens: 200_000,
                max_actions: 200,
            },
            max_corrections: 2,
            max_prompt_bytes: 16_384,
            declared_capabilities: vec!["observe", "prompt", "cancel", "resume", "events"],
        };
        let mut pinned = profile;
        pinned.digest = pinned.compute_digest();
        pinned
    }

    fn compute_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let material = format!(
            "{}|{}|{:?}|{}|{}|{:?}",
            self.profile_id,
            self.revision,
            self.budget,
            self.max_corrections,
            self.max_prompt_bytes,
            self.declared_capabilities
        );
        let mut hasher = Sha256::new();
        hasher.update(material.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    fn capabilities(&self) -> Result<SessionCapabilities, String> {
        SessionCapabilities::from_names(
            &self
                .declared_capabilities
                .iter()
                .map(|name| (*name).to_string())
                .collect::<Vec<String>>(),
        )
        .map_err(|error| error.to_string())
    }
}

// ─────────────────────────────────────────────────────────────────────────
// The tool
// ─────────────────────────────────────────────────────────────────────────

/// The Parent-side ephemeral execution tool. Constructed by the host with
/// the typed ports; NOT auto-registered in the model-visible registry in
/// this vertical (default closed — wiring is gated on this vertical's
/// green per the vertical gate).
pub struct ExecutionSubagentTool {
    controller: Arc<GatedSessionController>,
    sink: Arc<dyn SessionFactSink>,
    host_identity: HostIdentityRef,
    lineage: Option<LineageRef>,
    profile: ExecutionSubagentProfile,
}

impl ExecutionSubagentTool {
    pub const NAME: &'static str = "execution_subagent";

    #[must_use]
    pub fn new(
        controller: Arc<GatedSessionController>,
        sink: Arc<dyn SessionFactSink>,
        host_identity: HostIdentityRef,
    ) -> Self {
        Self {
            controller,
            sink,
            host_identity,
            lineage: None,
            profile: ExecutionSubagentProfile::default_execution_profile(),
        }
    }

    /// Pin a NON-default admitted profile (host embedder seam): the
    /// profile's digest is frozen at admission. The declared capability
    /// set MUST match what the bound controller actually supports — the
    /// spine gates interventions against it.
    #[must_use]
    pub fn with_profile(mut self, profile: ExecutionSubagentProfile) -> Self {
        let mut pinned = profile;
        pinned.digest = pinned.compute_digest();
        self.profile = pinned;
        self
    }

    /// Carry the spawning context's lineage (SubAgent-contract D1: depth-1 — a child
    /// context cannot run execution subagents).
    #[must_use]
    pub fn with_lineage(mut self, lineage: Option<LineageRef>) -> Self {
        self.lineage = lineage;
        self
    }

    #[must_use]
    pub fn carried_lineage(&self) -> Option<&LineageRef> {
        self.lineage.as_ref()
    }

    #[must_use]
    pub fn profile(&self) -> &ExecutionSubagentProfile {
        &self.profile
    }

    /// Typed inventory of what ONE run holds (the negative-capability
    /// evidence). The serialized key set is pinned by tests: adding a
    /// field (a credential, a workspace path, a CLI flag) becomes
    /// observable.
    #[must_use]
    pub fn run_inventory(&self, request: &ExecutionRunRequest) -> ExecutionSessionInventory {
        ExecutionSessionInventory {
            objective_bytes: request.objective.len(),
            correction_authorized: request.correction_prompt.is_some(),
            profile_id: self.profile.profile_id.clone(),
            profile_revision: self.profile.revision,
            profile_digest: self.profile.digest.clone(),
            budget_max_actions: self.profile.budget.max_actions,
            declared_capabilities: self
                .profile
                .declared_capabilities
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            outbound_surfaces: vec!["session-controller", "fact-sink"],
            lineage_depth: self.effective_lineage().depth(),
        }
    }

    /// Run one bounded ephemeral execution. This is the whole tool: one
    /// objective in, one structured report out (plus the receipt trail on
    /// the spine).
    pub async fn run(&self, request: &ExecutionRunRequest) -> ExecutionSessionReportV1 {
        let started = Instant::now();
        let lineage = self.effective_lineage();
        let run_ref = format!("exec-{}", uuid::Uuid::new_v4().simple());
        let mut usage = ExecutionUsageV1 {
            max_actions: self.profile.budget.max_actions,
            ..ExecutionUsageV1::default()
        };
        let meter = Arc::new(SubAgentBudgetMeter::new(self.profile.budget));

        // The route is pinned: a run executed by THIS tool is always
        // ephemeral (the router sends durable work to the bridge).
        let report_route = ExecutionRouteV1::EphemeralExec;

        // the SubAgent freeze D1: depth-1 — a child context cannot run execution
        // subagents. Refused before any port is touched.
        if lineage.depth() > 0 {
            return self.refused_report(
                &run_ref,
                report_route,
                &mut usage,
                started,
                0,
                format!(
                    "execution subagents cannot run from a child context (lineage depth {}, D1)",
                    lineage.depth()
                ),
            );
        }

        // Bounded, digest-bound context (SA-18/SA-16 pattern): the
        // objective is the context; the parent transcript is excluded.
        let bundle = ContextBundleV1 {
            bundle_id: format!("bundle-{}", uuid::Uuid::new_v4()),
            revision: 1,
            digest: String::new(),
            parent_ref: ParentRunRef::from_opaque(lineage.root_ref().as_str()),
            objective_context: request.objective.clone(),
            source_refs: Vec::new(),
            applicable_user_model: Vec::new(),
            skill_refs: Vec::new(),
            procedure_refs: Vec::new(),
            explicit_exclusions: vec![ContextClassV1::ParentTranscript],
            redaction_policy: BundleRedactionPolicy::default(),
        };
        // Pin the digest BEFORE admission (the admitted bundle refuses a
        // mismatched/absent pin).
        let mut bundle = bundle;
        bundle.digest = bundle.compute_digest();
        let bundle = match bundle.admit() {
            Ok(admitted) => admitted,
            Err(error) => {
                return self.refused_report(
                    &run_ref,
                    report_route,
                    &mut usage,
                    started,
                    0,
                    format!("context bundle refused: {error}"),
                );
            }
        };

        if meter.exhausted() {
            return self.refused_report(
                &run_ref,
                report_route,
                &mut usage,
                started,
                0,
                "budget exhausted before start".to_string(),
            );
        }

        let declared = match self.profile.capabilities() {
            Ok(caps) => caps,
            Err(error) => {
                return self.refused_report(
                    &run_ref,
                    report_route,
                    &mut usage,
                    started,
                    0,
                    format!("profile capability admission failed: {error}"),
                );
            }
        };

        // 1. START — the transport mints the remote session identity.
        let spec = SessionStartSpec {
            adapter_connection: AdapterConnectionRef::from_opaque(format!("conn-{run_ref}")),
            prompt: request.objective.clone(),
            context_digest: bundle.digest().to_string(),
            capabilities: declared,
            max_prompt_bytes: self.profile.max_prompt_bytes,
        };
        meter.try_record_action();
        usage.actions += 1;
        // The frozen run ceiling bounds STARTUP too: a transport that
        // cannot open a session inside the budget ends the run typed
        // instead of stretching it.
        let handle = match tokio::time::timeout(
            Duration::from_secs(self.profile.budget.time_limit_secs),
            self.controller.start(&spec),
        )
        .await
        {
            Ok(Ok(handle)) => handle,
            Ok(Err(error)) => {
                return self.typed_failure_report(
                    &run_ref,
                    report_route,
                    &mut usage,
                    started,
                    0,
                    None,
                    ExecutionRunStatusV1::Refused,
                    format!("controller refused start (fail closed): {error}"),
                );
            }
            Err(_) => {
                return self.typed_failure_report(
                    &run_ref,
                    report_route,
                    &mut usage,
                    started,
                    0,
                    None,
                    ExecutionRunStatusV1::TimedOut,
                    "controller start exceeded the frozen run ceiling (fail closed)".to_string(),
                );
            }
        };

        // 2. ATTACH — bind the minted session to the spine (fail closed:
        // an unavailable sink means the facts cannot flow, and facts ARE
        // the product; the session is stopped, never left unobserved).
        let binding = SessionBinding {
            host_identity: self.host_identity.clone(),
            adapter_connection: spec.adapter_connection.clone(),
            remote_session: handle.remote_session.clone(),
            idempotency_key: format!("exec-attach-{run_ref}"),
        };
        let declared_names: Vec<String> = self
            .profile
            .declared_capabilities
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        meter.try_record_action();
        usage.actions += 1;
        let attachment = match self.sink.attach(&binding, &declared_names).await {
            Ok(attachment) => attachment,
            Err(error) => {
                // Best-effort stop so no unobserved session survives; the
                // refusal still reports typed.
                let _ = self.controller.stop(&handle, true).await;
                return self.typed_failure_report(
                    &run_ref,
                    report_route,
                    &mut usage,
                    started,
                    0,
                    Some(handle.remote_session.clone()),
                    ExecutionRunStatusV1::Refused,
                    format!("fact sink unavailable at attach (fail closed): {error}"),
                );
            }
        };

        // 3. ADVERTISE — the capability set the host actually supports.
        meter.try_record_action();
        usage.actions += 1;
        if let Err(error) = self
            .sink
            .advertise_capabilities(&attachment, &declared_names)
            .await
        {
            return self
                .abandon(
                    &handle,
                    &attachment,
                    &run_ref,
                    &mut usage,
                    started,
                    format!("fact sink unavailable at advertise (fail closed): {error}"),
                )
                .await;
        }

        // 4. ACCEPTED — the verbatim attach fact opens the stream.
        let mut source_revision: u64 = 1;
        meter.try_record_action();
        usage.actions += 1;
        if let Err(error) = self
            .sink
            .ingest_event(
                &attachment,
                &SessionEventFact {
                    event_id: SessionEventIdRef::from_opaque(format!("{run_ref}-accepted")),
                    kind: SessionEventKindV1::Accepted,
                    outcome: None,
                    source_revision,
                    authority_confirmation_ref: None,
                    summary: None,
                    payload_digest: None,
                },
            )
            .await
        {
            return self
                .abandon(
                    &handle,
                    &attachment,
                    &run_ref,
                    &mut usage,
                    started,
                    format!("fact sink unavailable at accepted (fail closed): {error}"),
                )
                .await;
        }

        // The host-side revision of each FACT (event id -> source
        // revision). A replayed fact keeps its ORIGINAL revision — it is
        // the same fact; a fresh fact takes the next one. In-memory only.
        let mut fact_revisions: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();

        // 5. WATCH LOOP — facts mirrored as they happen; corrections
        // bounded by the profile; terminal honored from the harness.
        let deadline = started + Duration::from_secs(self.profile.budget.time_limit_secs);
        let mut cursor: u64 = 0;
        let mut corrections_used: u32 = 0;
        let mut facts_reported: u64 = 1; // accepted
        let mut interventions: Vec<ExecutionInterventionRecordV1> = Vec::new();
        let mut collected: Option<super::controller::SessionCollectView> = None;
        let mut final_state: Option<SessionCanonicalStateV1> = None;
        let mut terminal_outcome: Option<SessionTerminalOutcomeV1> = None;
        // Status is decided at every loop exit below (never left from a
        // previous iteration).
        let status;
        let mut refusal: Option<String> = None;
        // A decision made INSIDE the watch loop (correction ceiling, stop
        // chain) — it wins over terminal-outcome derivation because those
        // paths report their own spine terminal.
        let mut decided_status: Option<ExecutionRunStatusV1> = None;

        let mut deadline_hit = false;
        loop {
            if Instant::now() >= deadline {
                if deadline_hit {
                    // Already drained once past the deadline with no
                    // authoritative terminal: end typed.
                    status = ExecutionRunStatusV1::TimedOut;
                    break;
                }
                deadline_hit = true;
            }
            if meter.exhausted() {
                status = ExecutionRunStatusV1::Aborted;
                break;
            }
            meter.try_record_action();
            usage.actions += 1;
            let page = match self.controller.watch_events(&handle, cursor, 32).await {
                Ok(page) => page,
                Err(ControllerError::Unavailable) => {
                    // Dropout: report the connection fact, attempt one
                    // reconnect, resume from the spine's revision.
                    usage.actions += 1;
                    let _ = self
                        .sink
                        .mark_connection(&attachment, SessionConnectionFactV1::Disconnected)
                        .await;
                    match self.sink.reconnect(&binding).await {
                        Ok(receipt) => {
                            // The watch cursor is CONTROLLER-scoped: facts
                            // already consumed stay consumed; the spine's
                            // resume_from_revision scopes its own replay
                            // dedup window, not our stream position.
                            let _ = receipt.resume_from_revision;
                            facts_reported += 1;
                            final_state = Some(receipt.state.canonical_state);
                            continue;
                        }
                        Err(error) => {
                            status = ExecutionRunStatusV1::Failed;
                            refusal = Some(format!(
                                "connection lost and reconnect failed (fail closed): {error}"
                            ));
                            break;
                        }
                    }
                }
                Err(error) => {
                    status = ExecutionRunStatusV1::Failed;
                    refusal = Some(format!("controller watch failed: {error}"));
                    break;
                }
            };
            cursor = page.next_seq;
            let mut saw_terminal = false;
            for event in &page.events {
                usage.events_observed += 1;
                let spine_event_id = format!("{}-{}", run_ref, event.event_id.as_str());
                let fact_revision = match fact_revisions.get(&spine_event_id) {
                    Some(revision) => *revision,
                    None => {
                        source_revision += 1;
                        fact_revisions.insert(spine_event_id.clone(), source_revision);
                        source_revision
                    }
                };
                // Harness-emitted `accepted`/cleanup-derived facts map 1:1;
                // the host never mints a lifecycle kind it did not observe.
                meter.try_record_action();
                usage.actions += 1;
                let receipt: SessionEventReceiptView = match self
                    .sink
                    .ingest_event(
                        &attachment,
                        &SessionEventFact {
                            event_id: SessionEventIdRef::from_opaque(spine_event_id),
                            kind: event.kind,
                            outcome: event.outcome.clone(),
                            source_revision: fact_revision,
                            authority_confirmation_ref: event
                                .outcome
                                .as_ref()
                                .and_then(|outcome| outcome.authority_confirmation_ref())
                                .map(|reference| reference.as_str().to_string()),
                            summary: event.summary.clone(),
                            payload_digest: None,
                        },
                    )
                    .await
                {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        refusal = Some(format!("fact sink unavailable mid-run: {error}"));
                        break;
                    }
                };
                facts_reported += 1;
                final_state = Some(receipt.state.canonical_state);
                match event.kind {
                    SessionEventKindV1::InputRequired => {
                        // Bounded correction: at most max_corrections
                        // deliveries, from the parent-authorized prompt.
                        if corrections_used >= self.profile.max_corrections {
                            // Ceiling reached: stop the session through
                            // the cancel chain; never fake completion.
                            let (stop_status, stop_refusal, records) = self
                                .stop_via_cancel_chain(
                                    &handle,
                                    &attachment,
                                    &run_ref,
                                    &mut source_revision,
                                    &mut facts_reported,
                                    "correction ceiling reached",
                                )
                                .await;
                            decided_status = Some(stop_status);
                            refusal = stop_refusal;
                            interventions.extend(records);
                            saw_terminal = true;
                            break;
                        }
                        let Some(correction) = request.correction_prompt.as_deref() else {
                            let (stop_status, stop_refusal, records) = self
                                .stop_via_cancel_chain(
                                    &handle,
                                    &attachment,
                                    &run_ref,
                                    &mut source_revision,
                                    &mut facts_reported,
                                    "input required and no correction authorized",
                                )
                                .await;
                            decided_status = Some(stop_status);
                            refusal = stop_refusal;
                            interventions.extend(records);
                            saw_terminal = true;
                            break;
                        };
                        corrections_used += 1;
                        usage.actions += 1;
                        if deadline_hit {
                            // No new correction legs past the ceiling: the
                            // fact is already ingested above; the bottom of
                            // the loop ends the run typed (TimedOut) — no
                            // fabricated Failed.
                            break;
                        }
                        // Deliberately NOT wrapped in the remaining-budget
                        // timeout: cancelling a mid-turn prompt races the
                        // remote completion and can mint a contradictory
                        // terminal. The delivery is bounded by the
                        // transport's own turn ceiling instead; the wall
                        // clock may overshoot the budget by at most one
                        // turn, and the deadline law above still ends the
                        // run typed at the next loop check.
                        match self.controller.prompt(&handle, correction).await {
                            Ok(_) => {}
                            Err(error) => {
                                refusal = Some(format!("correction delivery failed: {error}"));
                                saw_terminal = true;
                                break;
                            }
                        }
                    }
                    SessionEventKindV1::Terminal => {
                        saw_terminal = true;
                        // The harness's own terminal (completed/failed) or
                        // the cancel-confirmed terminal from our stop —
                        // already ingested above with its confirmation ref.
                        terminal_outcome = event.outcome.clone();
                        break;
                    }
                    _ => {}
                }
            }
            if saw_terminal {
                status = decided_status.take().unwrap_or(match &terminal_outcome {
                    Some(SessionTerminalOutcomeV1::Completed) => ExecutionRunStatusV1::Completed,
                    Some(SessionTerminalOutcomeV1::Failed) => ExecutionRunStatusV1::Failed,
                    Some(SessionTerminalOutcomeV1::Cancelled { .. }) => {
                        ExecutionRunStatusV1::StoppedGracefully
                    }
                    None => ExecutionRunStatusV1::Failed,
                });
                break;
            }
            if let Some(reason) = &refusal {
                status = ExecutionRunStatusV1::Failed;
                let _ = reason;
                break;
            }
            if deadline_hit {
                // One drain pass already ran past the deadline with no
                // authoritative terminal: lifecycle truth preserved, the
                // ceiling still ends the run typed.
                status = ExecutionRunStatusV1::TimedOut;
                break;
            }
        }

        // Nonterminal exits still kill the carrier — an immediate host
        // stop terminates and reaps the process group but mints NO event
        // into the fact stream (best-effort; the spine keeps its honest
        // state instead of a fabricated terminal). The gated client may
        // TYPE-REFUSE this stop when the profile did not declare cancel —
        // that refusal is the frozen capability law, not a fallback path:
        // such a session is reclaimed when the controller is released.
        if matches!(
            status,
            ExecutionRunStatusV1::TimedOut
                | ExecutionRunStatusV1::Aborted
                | ExecutionRunStatusV1::Failed
        ) {
            let _ = self.controller.stop(&handle, false).await;
        }

        // 6. CLEANUP + COLLECT for non-cancelled endings (the cancel chain
        // already reported its own terminal; cleanup is still ours to
        // record — the spine's cleanup receipt closes the attachment).
        if !matches!(status, ExecutionRunStatusV1::Refused) {
            meter.try_record_action();
            usage.actions += 1;
            let _ = self
                .sink
                .ingest_event(
                    &attachment,
                    &SessionEventFact {
                        event_id: SessionEventIdRef::from_opaque(format!("{run_ref}-cleanup")),
                        kind: SessionEventKindV1::Cleanup,
                        outcome: None,
                        source_revision: {
                            source_revision += 1;
                            source_revision
                        },
                        authority_confirmation_ref: None,
                        summary: None,
                        payload_digest: None,
                    },
                )
                .await;
            facts_reported += 1;
            meter.try_record_action();
            usage.actions += 1;
            collected = self.controller.collect(&handle).await.ok();
            if let Ok(state) = self.sink.get_state(&attachment).await {
                final_state = Some(state.canonical_state);
            }
        }

        usage.facts_reported = facts_reported;
        usage.elapsed_ms = started.elapsed().as_millis() as u64;
        ExecutionSessionReportV1 {
            run_ref,
            route: report_route,
            controller_ref: self.controller.binding_label().to_string(),
            status,
            remote_session_ref: Some(handle.remote_session.clone()),
            attachment_ref: Some(attachment.clone()),
            final_canonical_state: final_state,
            collected_summary: collected.as_ref().and_then(|view| view.summary.clone()),
            collected_digest: collected.as_ref().map(|view| view.digest.clone()),
            interventions,
            evidence_refs: collected.map(|view| view.evidence_refs).unwrap_or_default(),
            usage,
            refusal,
        }
    }

    /// The spine-legal stop: request_cancel (issued by this host through
    /// the spine) → controller.stop → record the accepted result with the
    /// harness confirmation ref → ingest the bound terminal cancelled
    /// fact. A refusal at ANY link is surfaced typed and no terminal fact
    /// is fabricated.
    #[allow(clippy::too_many_arguments)]
    async fn stop_via_cancel_chain(
        &self,
        handle: &super::controller::SessionHandle,
        attachment: &SessionAttachmentRef,
        run_ref: &str,
        source_revision: &mut u64,
        facts_reported: &mut u64,
        reason: &str,
    ) -> (
        ExecutionRunStatusV1,
        Option<String>,
        Vec<ExecutionInterventionRecordV1>,
    ) {
        let request_id = InterventionRequestIdRef::from_opaque(format!("{run_ref}-cancel"));
        // (a) the request receipt — zero-fabrication: if the spine
        // refuses the request, there is no chain and no terminal fact.
        // A typed unsupported-by-lifecycle-owner refusal surfaces as the
        // typed unsupported_operation status (never fake success).
        if let Err(error) = self
            .sink
            .request_intervention(
                attachment,
                &request_id,
                SessionInterventionKindV1::RequestCancel,
                reason,
            )
            .await
        {
            if error.to_string().contains("unsupported_by_lifecycle_owner") {
                return (
                    ExecutionRunStatusV1::UnsupportedOperation,
                    Some(format!(
                        "stop unsupported by the session's lifecycle owner (cancel); \
                         no terminal fact fabricated ({error})"
                    )),
                    Vec::new(),
                );
            }
            return (
                ExecutionRunStatusV1::Failed,
                Some(format!("cancel request refused by spine: {error}")),
                Vec::new(),
            );
        }
        *facts_reported += 1;
        // (b) the controller stop — the typed refusal path.
        let receipt = match self.controller.stop(handle, true).await {
            Ok(receipt) => receipt,
            Err(ControllerError::UnsupportedByLifecycleOwner { operation }) => {
                return (
                    ExecutionRunStatusV1::UnsupportedOperation,
                    Some(format!(
                        "stop unsupported by the session's lifecycle owner ({operation}); \
                         no terminal fact fabricated"
                    )),
                    Vec::new(),
                );
            }
            Err(error) => {
                return (
                    ExecutionRunStatusV1::Failed,
                    Some(format!("controller stop failed: {error}")),
                    Vec::new(),
                );
            }
        };
        // (c) record the host's authoritative result — accepted ONLY with
        // the harness confirmation reference.
        let confirmation = receipt.authority_confirmation_ref.clone();
        if let Err(error) = self
            .sink
            .record_intervention_result(
                attachment,
                &request_id,
                if receipt.confirmed && confirmation.is_some() {
                    SessionInterventionDispositionV1::Accepted
                } else {
                    SessionInterventionDispositionV1::Failed
                },
                confirmation.as_ref().map(|reference| reference.as_str()),
                None,
            )
            .await
        {
            return (
                ExecutionRunStatusV1::Failed,
                Some(format!("fact sink refused the cancel result: {error}")),
                Vec::new(),
            );
        }
        *facts_reported += 1;
        // (d) the bound terminal fact — only when the receipt chain is
        // complete; otherwise no terminal fact exists (honest absence).
        // A FAILED terminal-fact write can never surface as graceful: the
        // facts ARE the product, so the run ends failed (zero fabricated
        // completion at the parent boundary).
        if let (true, Some(confirmation)) = (receipt.confirmed, confirmation.clone()) {
            *source_revision += 1;
            let revision = *source_revision;
            // The spine REFUSES a cancelled terminal whose top-level
            // confirmation reference is absent: the binding is carried on
            // the fact itself, not only inside the typed outcome.
            let bound_confirmation = confirmation.as_str().to_string();
            if let Err(error) = self
                .sink
                .ingest_event(
                    attachment,
                    &SessionEventFact {
                        event_id: SessionEventIdRef::from_opaque(format!("{run_ref}-terminal")),
                        kind: SessionEventKindV1::Terminal,
                        outcome: Some(SessionTerminalOutcomeV1::Cancelled { confirmation }),
                        source_revision: revision,
                        authority_confirmation_ref: Some(bound_confirmation),
                        summary: None,
                        payload_digest: None,
                    },
                )
                .await
            {
                return (
                    ExecutionRunStatusV1::Failed,
                    Some(format!(
                        "fact sink refused the bound terminal fact (no graceful fabrication): {error}"
                    )),
                    Vec::new(),
                );
            }
            *facts_reported += 1;
        }
        let record = ExecutionInterventionRecordV1 {
            request_id: request_id.as_str().to_string(),
            kind: SessionInterventionKindV1::RequestCancel,
            disposition: if receipt.confirmed {
                SessionInterventionDispositionV1::Accepted
            } else {
                SessionInterventionDispositionV1::Failed
            },
        };
        let status = if receipt.confirmed {
            ExecutionRunStatusV1::StoppedGracefully
        } else {
            // Requested but unconfirmed: the run did NOT fake success.
            ExecutionRunStatusV1::UnsupportedOperation
        };
        let refusal = (!receipt.confirmed).then(|| {
            "stop requested but unconfirmed; no cancelled terminal fact was reported".to_string()
        });
        (status, refusal, vec![record])
    }

    fn effective_lineage(&self) -> LineageRef {
        self.lineage
            .clone()
            .unwrap_or_else(|| LineageRef::new_root(ParentRunRef::from_opaque("agent:parent")))
    }

    fn refused_report(
        &self,
        run_ref: &str,
        route: ExecutionRouteV1,
        usage: &mut ExecutionUsageV1,
        started: Instant,
        events_observed: u64,
        reason: String,
    ) -> ExecutionSessionReportV1 {
        usage.elapsed_ms = started.elapsed().as_millis() as u64;
        usage.events_observed = events_observed;
        ExecutionSessionReportV1 {
            run_ref: run_ref.to_string(),
            route,
            controller_ref: self.controller.binding_label().to_string(),
            status: ExecutionRunStatusV1::Refused,
            remote_session_ref: None,
            attachment_ref: None,
            final_canonical_state: None,
            collected_summary: None,
            collected_digest: None,
            interventions: Vec::new(),
            evidence_refs: Vec::new(),
            usage: *usage,
            refusal: Some(reason),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn typed_failure_report(
        &self,
        run_ref: &str,
        route: ExecutionRouteV1,
        usage: &mut ExecutionUsageV1,
        started: Instant,
        events_observed: u64,
        remote_session: Option<RemoteSessionRef>,
        status: ExecutionRunStatusV1,
        reason: String,
    ) -> ExecutionSessionReportV1 {
        usage.elapsed_ms = started.elapsed().as_millis() as u64;
        usage.events_observed = events_observed;
        ExecutionSessionReportV1 {
            run_ref: run_ref.to_string(),
            route,
            controller_ref: self.controller.binding_label().to_string(),
            status,
            remote_session_ref: remote_session,
            attachment_ref: None,
            final_canonical_state: None,
            collected_summary: None,
            collected_digest: None,
            interventions: Vec::new(),
            evidence_refs: Vec::new(),
            usage: *usage,
            refusal: Some(reason),
        }
    }

    /// Stop the session best-effort and report the typed refusal (used
    /// when the sink fails AFTER the session started — the run must never
    /// continue unobserved).
    async fn abandon(
        &self,
        handle: &super::controller::SessionHandle,
        attachment: &SessionAttachmentRef,
        run_ref: &str,
        usage: &mut ExecutionUsageV1,
        started: Instant,
        reason: String,
    ) -> ExecutionSessionReportV1 {
        let _ = self.controller.stop(handle, true).await;
        let _ = self
            .sink
            .mark_connection(attachment, SessionConnectionFactV1::Disconnected)
            .await;
        let _ = run_ref;
        usage.elapsed_ms = started.elapsed().as_millis() as u64;
        ExecutionSessionReportV1 {
            run_ref: run_ref.to_string(),
            route: ExecutionRouteV1::EphemeralExec,
            controller_ref: self.controller.binding_label().to_string(),
            status: ExecutionRunStatusV1::Refused,
            remote_session_ref: Some(handle.remote_session.clone()),
            attachment_ref: Some(attachment.clone()),
            final_canonical_state: None,
            collected_summary: None,
            collected_digest: None,
            interventions: Vec::new(),
            evidence_refs: Vec::new(),
            usage: *usage,
            refusal: Some(reason),
        }
    }
}

/// What one execution run holds — the negative-capability evidence type.
/// No shell, file_write, file_edit, git, worktree, CLI-flag, or
/// credential field exists; the serialized key set is pinned by test.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ExecutionSessionInventory {
    pub objective_bytes: usize,
    pub correction_authorized: bool,
    pub profile_id: String,
    pub profile_revision: u32,
    pub profile_digest: String,
    pub budget_max_actions: u32,
    pub declared_capabilities: Vec<String>,
    pub outbound_surfaces: Vec<&'static str>,
    pub lineage_depth: u32,
}

/// One run's parent-authorized inputs (the tool call arguments, typed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionRunRequest {
    /// Bounded objective; becomes the session's bounded prompt.
    pub objective: String,
    /// The parent-authorized correction prompt, used at most
    /// `max_corrections` times on `input_required`.
    pub correction_prompt: Option<String>,
}

impl zeroclaw_api::attribution::Attributable for ExecutionSubagentTool {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::Tool(zeroclaw_api::attribution::ToolKind::SpawnSubagent)
    }

    fn alias(&self) -> &str {
        Self::NAME
    }
}

#[async_trait]
impl Tool for ExecutionSubagentTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Run one bounded ExecutionSubAgent that supervises a short-lived \
         harness session (ephemeral execution). The subagent can only \
         start, watch, correct (bounded), stop, and collect the session; \
         the harness behind the session does the repository work. Facts \
         are reported as receipts to Tachi. Use for one short review/fix \
         whose failure is cheap to retry. Work needing restart recovery, \
         remote targets, multi-attempt claims, approvals, or evidence \
         must go through the durable Tachi bridge instead."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "objective": {
                    "type": "string",
                    "description": "The bounded, self-contained short execution task. The session sees only this prompt."
                },
                "correction_prompt": {
                    "type": "string",
                    "description": "Optional. One bounded correction delivered when the session reports it is waiting for input. Used at most twice."
                }
            },
            "required": ["objective"],
            "additionalProperties": false
        })
    }

    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({
            "type": "object",
            "description": "ExecutionSessionReportV1 — the ONLY child→parent result channel",
            "required": ["run_ref", "route", "status", "usage"],
            "properties": {
                "run_ref": {"type": "string"},
                "route": {"type": "string", "enum": ["Reason", "EphemeralExec", "DurableExec"]},
                "controller_ref": {"type": "string"},
                "status": {
                    "type": "string",
                    "enum": ["completed", "failed", "timed_out", "stopped_gracefully",
                             "aborted", "refused", "unsupported_operation"]
                },
                "remote_session_ref": {"type": ["string", "null"]},
                "attachment_ref": {"type": ["string", "null"]},
                "final_canonical_state": {"type": ["string", "null"]},
                "collected_summary": {"type": ["string", "null"]},
                "collected_digest": {"type": ["string", "null"]},
                "interventions": {"type": "array"},
                "evidence_refs": {"type": "array"},
                "usage": {"type": "object"},
                "refusal": {"type": ["string", "null"]}
            },
            "additionalProperties": false
        }))
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let objective = args
            .get("objective")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        if objective.trim().is_empty() {
            return Ok(ToolResult::ok(ToolOutput::text(
                "execution_subagent refused: objective is required and must be non-empty",
            )));
        }
        let request = ExecutionRunRequest {
            objective,
            correction_prompt: args
                .get("correction_prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        };
        let report = self.run(&request).await;
        let display = match report.status {
            ExecutionRunStatusV1::Completed | ExecutionRunStatusV1::StoppedGracefully => {
                format!(
                    "ephemeral execution {}: {:?} (session {:?}, spine state {:?})",
                    report.run_ref,
                    report.status,
                    report.remote_session_ref.as_ref().map(|r| r.as_str()),
                    report.final_canonical_state.map(|s| s.as_str())
                )
            }
            other => format!(
                "ephemeral execution {}: {:?}{}",
                report.run_ref,
                other,
                report
                    .refusal
                    .as_deref()
                    .map(|reason| format!(" — {reason}"))
                    .unwrap_or_default()
            ),
        };
        let data = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
        // Honesty at the tool-plumbing boundary: a run that did NOT end
        // completed/stopped is a FAILED tool result (success=false with an
        // explicit error) even though the structured report travels — the
        // model loop must never read a refused/unsupported/failed run as a
        // successful tool call.
        if matches!(
            report.status,
            ExecutionRunStatusV1::Completed | ExecutionRunStatusV1::StoppedGracefully
        ) {
            Ok(ToolResult::ok(ToolOutput::json_with_text(data, display)))
        } else {
            Ok(ToolResult {
                success: false,
                output: ToolOutput::json_with_text(data, display.clone()),
                error: Some(
                    report
                        .refusal
                        .clone()
                        .unwrap_or_else(|| format!("run ended {:?}", report.status)),
                ),
            })
        }
    }
}
