//! In-process port implementations: the transport-binding test double
//! that mirrors the tachi host bridge law, and the always-unavailable
//! transport for TB-20 outage tests.
//!
//! [`InMemoryTachiTaskBridge`] is a TEST DOUBLE and binding-point
//! stand-in, never a production transport: its task/result state is
//! process-lifetime only (it dies with the process — deliberately NOT a
//! durable store; TB-22), and its submit path mirrors the tachi host's
//! ordering exactly, including the frozen order where a malformed or
//! forbidden payload is REJECTED before the TB-7 tuple is consulted (a
//! reused tuple carrying a different malformed digest answers
//! `Rejected`, matching the host bridge's admission-before-binding
//! order).
//!
//! [`InMemoryTachiTaskBridge`] is a faithful miniature of the tachi
//! `TaskIntentBridge` semantics (host vertical V2a) for the four in-scope ops —
//! same tuple law, same replay rule, same revision folding — carried in
//! process memory only. It owns NO DDL and writes NO durable state
//! (TB-1/TB-22). It is the Stage-B binding point's stand-in until the
//! tachi MCP facade lands; production transports replace it without
//! touching the client law.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};
use zeroclaw_api::taskintent::{
    AttemptRef, InterventionError, InterventionReceipt, InterventionStatic, InterventionV1,
    RequestId, RequesterRef, SCHEMA_TAG, StopMode, StopReceipt, StopStage, TaskIntentV1, TaskRef,
};

use super::client::{
    BridgeQueryError, ContractViolationView, ProjectedAdjudicationState, ProjectedDeliveryState,
    ProjectedExecutionState, ResultProjectionView, SubmitReceipt, SubmitTransportError,
    TachiTaskBridge, TaskEventPageView, TaskEventView, TaskSnapshotView, VerificationSummaryView,
};
use super::compose::scan_intent;

/// One ingested fact on a task's log.
#[derive(Debug, Clone)]
struct InMemoryFact {
    event_id: String,
    kind: String,
    payload_digest: String,
    detail: FactDetail,
}

/// The fact shapes the double models (mirroring the tachi event payload
/// vocabulary for the in-scope ops).
#[derive(Debug, Clone)]
enum FactDetail {
    /// The intent was admitted (carries the digest).
    TaskSubmitted { digest: String },
    /// An execution-dimension fact (carries the bridge wire label).
    Execution { label: String },
    /// An adjudication-dimension fact (carries the bridge wire label).
    Adjudication { label: String },
    /// A terminal outcome observation.
    OutcomeObserved {
        attempt: AttemptRef,
        reported_outcome: String,
        canonical_artifact_ref: Option<String>,
        evidence_refs: Vec<String>,
        verification_present: bool,
        diff_present: bool,
        provenance: String,
    },
    /// An owner capability declaration (TB-15: advertisement is a typed,
    /// revisioned fact — the harness/test declares what its owner leg
    /// supports beyond the lifecycle-mode baseline).
    OwnerCapabilities { operations: Vec<InterventionStatic> },
    /// A session intervention was forwarded (carries the intervention id
    /// and the operation).
    InterventionForwarded {
        operation: InterventionStatic,
        intervention_id: String,
    },
    /// A stop was requested/forwarded (TB-12 multi-stage fact; the double
    /// never confirms — `cancelled` requires authoritative owner
    /// confirmation, which only a real owner can produce). The reason
    /// rides the fact's payload digest only — the stop identity law
    /// excludes it, so no later stage needs it.
    StopRequested { mode: StopMode, stop_id: String },
}

/// SHA-256 lower-hex over the CANONICAL JSON of the typed payload
/// (keys sorted recursively — the shared canonical-JSON rule), matching
/// the `TaskEventView::payload_digest` contract: the digest covers the
/// payload CONTENT, not the event identity. Two ingests of identical
/// payload content therefore share a digest while remaining distinct
/// facts by event id.
fn fact_digest(payload: &serde_json::Value) -> String {
    let canonical = zeroclaw_api::taskintent::canonical_json(payload).to_string();
    let bytes = Sha256::digest(canonical.as_bytes());
    let mut out = String::with_capacity(2 * bytes.len());
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[derive(Debug, Default)]
struct HostState {
    /// TB-7 tuple binding: (requester, request_id) → (digest, TaskRef).
    bindings: BTreeMap<(String, String), (String, TaskRef)>,
    /// Per-task fact logs in append order (seq assigned on append).
    facts: BTreeMap<String, Vec<(u64, InMemoryFact)>>,
    /// Next task counter (server-side mint authority, TB-6).
    next_task: u64,
    /// Admitted expected artifacts per task (for TB-13 collect checks).
    expected: BTreeMap<String, Vec<(String, bool)>>,
    /// Submitting requester per task (TB-11 requester-owns-task law).
    owners: BTreeMap<String, String>,
    /// Materialized intervention receipts per TB-7 rule-6 tuple
    /// (same tuple + same digest ⇒ the SAME receipt on replay).
    intervention_receipts: BTreeMap<(String, String), InterventionReceipt>,
}

impl HostState {
    fn mint_task_ref(&mut self) -> TaskRef {
        self.next_task += 1;
        // Server-minted wire value; decode-only on the client side, so
        // build it through the same deserialization path a transport
        // would use (proves the value is wire-shaped, not constructed).
        serde_json::from_value(serde_json::Value::String(format!(
            "task:inmem-{:08x}",
            self.next_task
        )))
        .expect("minted task ref is wire-shaped")
    }

    fn append(&mut self, task_ref: &TaskRef, fact: InMemoryFact) -> u64 {
        let log = self
            .facts
            .entry(task_ref.as_wire().to_string())
            .or_default();
        let seq = log.len() as u64 + 1;
        log.push((seq, fact));
        seq
    }

    fn has_task_submitted(&self, task_ref: &TaskRef) -> bool {
        self.facts.get(task_ref.as_wire()).is_some_and(|log| {
            log.iter()
                .any(|(_, f)| matches!(f.detail, FactDetail::TaskSubmitted { .. }))
        })
    }

    /// The advertised intervention set for a task (TB-15): the
    /// `tachi_managed_batch` baseline (the two stop operations, exactly
    /// the real tachi `LifecycleMode` baseline) unioned with any owner
    /// capability declarations ingested onto the fact log.
    fn supported_interventions(&self, task_ref: &TaskRef) -> Vec<InterventionStatic> {
        let mut supported = vec![
            InterventionStatic::RequestGracefulStop,
            InterventionStatic::RequestHardCancel,
        ];
        if let Some(log) = self.facts.get(task_ref.as_wire()) {
            for (_, fact) in log {
                if let FactDetail::OwnerCapabilities { operations } = &fact.detail {
                    for op in operations {
                        if !supported.contains(op) {
                            supported.push(*op);
                        }
                    }
                }
            }
        }
        supported
    }
}

/// In-memory transport + host double mirroring the tachi bridge law.
#[derive(Debug, Default)]
pub struct InMemoryTachiTaskBridge {
    state: Mutex<HostState>,
}

impl InMemoryTachiTaskBridge {
    /// An empty bridge.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct TB-7 tuples are bound (test observability for
    /// "zero new execution" / "never invents a new request id").
    pub fn binding_count(&self) -> usize {
        self.state.lock().bindings.len()
    }

    /// How many distinct tasks were minted (test observability).
    pub fn task_count(&self) -> u64 {
        self.state.lock().next_task
    }

    /// Test/harness driver: ingest an execution-dimension fact (wire
    /// label must be in the execution mapping table). Every ingest is a
    /// DISTINCT canonical fact (mirroring how tachi ingests real
    /// transitions with their own source identities): a repeated label
    /// is a new transition, and event ids are occurrence-unique — the
    /// event id embeds the number of prior facts so `running → failed →
    /// running` folds correctly instead of collapsing. Refuses task
    /// refs this double never admitted (only `submit` mints tasks;
    /// pre-seeding an unadmitted ref's log is not ingestion).
    pub fn ingest_execution(&self, task_ref: &TaskRef, label: &str) {
        assert!(
            ProjectedExecutionState::project(label).is_some(),
            "unknown execution label {label}"
        );
        let mut state = self.state.lock();
        assert!(
            state.has_task_submitted(task_ref),
            "execution facts attach to admitted tasks only (host-double discipline)"
        );
        let occurrence = state.facts.get(task_ref.as_wire()).map_or(0, Vec::len);
        state.append(
            task_ref,
            InMemoryFact {
                event_id: format!("exec-{label}-{}-{occurrence}", task_ref.as_wire()),
                kind: "execution".to_string(),
                payload_digest: fact_digest(&serde_json::json!({
                    "kind": "execution",
                    "label": label,
                })),
                detail: FactDetail::Execution {
                    label: label.to_string(),
                },
            },
        );
    }

    /// Test/harness driver: ingest an adjudication-dimension fact. Same
    /// laws as [`Self::ingest_execution`]: admitted tasks only, each
    /// ingest is a distinct fact, occurrence-unique event id.
    pub fn ingest_adjudication(&self, task_ref: &TaskRef, label: &str) {
        assert!(
            ProjectedAdjudicationState::project(label).is_some(),
            "unknown adjudication label {label}"
        );
        let mut state = self.state.lock();
        assert!(
            state.has_task_submitted(task_ref),
            "adjudication facts attach to admitted tasks only (host-double discipline)"
        );
        let occurrence = state.facts.get(task_ref.as_wire()).map_or(0, Vec::len);
        state.append(
            task_ref,
            InMemoryFact {
                event_id: format!("adj-{label}-{}-{occurrence}", task_ref.as_wire()),
                kind: "adjudication".to_string(),
                payload_digest: fact_digest(&serde_json::json!({
                    "kind": "adjudication",
                    "label": label,
                })),
                detail: FactDetail::Adjudication {
                    label: label.to_string(),
                },
            },
        );
    }

    /// Test/harness driver: observe a terminal outcome. `reported_outcome`
    /// is the WORKER's claim; the collect fold independently checks the
    /// required-artifact contract (TB-13: prose is not the verdict).
    #[allow(clippy::too_many_arguments)]
    pub fn observe_outcome(
        &self,
        task_ref: &TaskRef,
        attempt: AttemptRef,
        reported_outcome: &str,
        canonical_artifact_ref: Option<String>,
        evidence_refs: Vec<String>,
        verification_present: bool,
        diff_present: bool,
        provenance: &str,
    ) {
        // Each observation is a DISTINCT canonical outcome row (a new
        // result revision, a revised result), so the event id is unique
        // per observation — exactly like every other ingest, where each
        // call is a distinct canonical fact. The id computation and
        // the append happen under ONE lock scope: computing the id, then
        // releasing the lock before appending would let two concurrent
        // observations pick the same id and silently suppress one
        // revision. Admitted tasks only — same host-double discipline
        // as the dimension ingest drivers (pre-seeding an unadmitted
        // ref's log is not observation).
        let mut state = self.state.lock();
        assert!(
            state.has_task_submitted(task_ref),
            "outcome facts attach to admitted tasks only (host-double discipline)"
        );
        let existing = state.facts.get(task_ref.as_wire()).map_or(0, Vec::len);
        let event_id = format!(
            "outcome-{}-{}-rev{}",
            attempt.as_wire(),
            task_ref.as_wire(),
            existing + 1
        );
        state.append(
            task_ref,
            InMemoryFact {
                event_id: event_id.clone(),
                kind: "outcome_observed".to_string(),
                payload_digest: fact_digest(&serde_json::json!({
                    "kind": "outcome_observed",
                    "attempt": attempt.as_wire(),
                    "reported_outcome": reported_outcome,
                    "canonical_artifact_ref": canonical_artifact_ref,
                    "evidence_refs": evidence_refs,
                    "verification_present": verification_present,
                    "diff_present": diff_present,
                    "provenance": provenance,
                })),
                detail: FactDetail::OutcomeObserved {
                    attempt,
                    reported_outcome: reported_outcome.to_string(),
                    canonical_artifact_ref,
                    evidence_refs,
                    verification_present,
                    diff_present,
                    provenance: provenance.to_string(),
                },
            },
        );
    }

    /// Test/harness driver: declare owner capabilities on a task's fact
    /// log (TB-15: advertisement is a typed, revisioned fact — the
    /// baseline for the managed lane is stops only, exactly like the
    /// real tachi `LifecycleMode` baseline; anything else the owner leg
    /// supports must be declared). Admitted tasks only.
    pub fn declare_owner_capabilities(
        &self,
        task_ref: &TaskRef,
        operations: &[InterventionStatic],
    ) {
        for op in operations {
            assert!(InterventionStatic::ALL.contains(op), "unknown op {op:?}");
        }
        let mut state = self.state.lock();
        assert!(
            state.has_task_submitted(task_ref),
            "capability declarations attach to admitted tasks only (host-double discipline)"
        );
        let occurrence = state.facts.get(task_ref.as_wire()).map_or(0, Vec::len);
        state.append(
            task_ref,
            InMemoryFact {
                event_id: format!("owner-caps-{}-{occurrence}", task_ref.as_wire()),
                kind: "owner_capabilities_declared".to_string(),
                payload_digest: fact_digest(&serde_json::json!({
                    "kind": "owner_capabilities_declared",
                    "operations": operations.len(),
                })),
                detail: FactDetail::OwnerCapabilities {
                    operations: operations.to_vec(),
                },
            },
        );
    }

    fn snapshot_from(state: &HostState, task_ref: &TaskRef) -> TaskSnapshotView {
        let log = state
            .facts
            .get(task_ref.as_wire())
            .cloned()
            .unwrap_or_default();
        let mut execution = ProjectedExecutionState::project("queued").expect("queued is mapped");
        let mut adjudication =
            ProjectedAdjudicationState::project("unreviewed").expect("unreviewed is mapped");
        let mut delivery =
            ProjectedDeliveryState::project("not_ready").expect("not_ready is mapped");
        let mut digest = String::new();
        for (_, fact) in &log {
            match &fact.detail {
                FactDetail::TaskSubmitted { digest: d } => digest = d.clone(),
                FactDetail::Execution { label } => {
                    execution =
                        ProjectedExecutionState::project(label).expect("host label is mapped");
                }
                FactDetail::StopRequested { .. } => {
                    // TB-12: a stop REQUEST is the multi-stage fact's
                    // first stages — the projection moves to
                    // `cancellation_requested` at most, NEVER `cancelled`
                    // (confirmation is the owner's alone).
                    execution = ProjectedExecutionState::project("cancellation_requested")
                        .expect("cancellation_requested is mapped");
                }
                FactDetail::Adjudication { label } => {
                    adjudication =
                        ProjectedAdjudicationState::project(label).expect("host label is mapped");
                }
                FactDetail::OutcomeObserved { .. } => {
                    delivery = ProjectedDeliveryState::project("ready").expect("ready is mapped");
                }
                FactDetail::OwnerCapabilities { .. } | FactDetail::InterventionForwarded { .. } => {
                }
            }
        }
        TaskSnapshotView {
            task_ref: task_ref.clone(),
            task_revision: log.len() as u64,
            execution,
            adjudication,
            delivery,
            lifecycle_mode: Some("tachi_managed_batch".to_string()),
            intent_digest: digest,
        }
    }

    /// Fold the whole fact log into the immutable per-revision projection
    /// history (single pass): every OutcomeObserved mints a revision; a
    /// later adjudication fact mints the next revision over the prior
    /// projection (mirroring the tachi collect fold).
    fn fold_revisions(state: &HostState, task_ref: &TaskRef) -> Vec<ResultProjectionView> {
        let Some(log) = state.facts.get(task_ref.as_wire()) else {
            return Vec::new();
        };
        let expected = state
            .expected
            .get(task_ref.as_wire())
            .cloned()
            .unwrap_or_default();
        let mut history: Vec<ResultProjectionView> = Vec::new();
        for (_, fact) in log {
            match &fact.detail {
                FactDetail::OutcomeObserved {
                    attempt,
                    reported_outcome,
                    canonical_artifact_ref,
                    evidence_refs,
                    verification_present,
                    diff_present,
                    provenance,
                } => {
                    // TB-13: the artifact contract is checked against the
                    // admitted expectations, NOT against the worker prose.
                    let violations = contract_violations(
                        &expected,
                        canonical_artifact_ref.is_some(),
                        *verification_present,
                        *diff_present,
                    );
                    history.push(ResultProjectionView {
                        task_ref: task_ref.clone(),
                        attempt_ref: Some(attempt.clone()),
                        terminal_classification: reported_outcome.clone(),
                        canonical_artifact_ref: canonical_artifact_ref.clone(),
                        artifact_evidence_refs: evidence_refs.clone(),
                        verification: VerificationSummaryView {
                            verification_present: *verification_present,
                            diff_present: *diff_present,
                            evidence_ref_count: evidence_refs.len(),
                        },
                        adjudication: ProjectedAdjudicationState::project("unreviewed")
                            .expect("unreviewed is mapped"),
                        contract_violations: violations,
                        provenance: provenance.clone(),
                        pending_user_action: None,
                        result_revision: history.len() as u64 + 1,
                    });
                }
                FactDetail::Adjudication { label } => {
                    if let Some(prior) = history.last().cloned() {
                        let mut next = prior;
                        next.adjudication = ProjectedAdjudicationState::project(label)
                            .expect("host label is mapped");
                        next.result_revision = history.len() as u64 + 1;
                        history.push(next);
                    }
                }
                _ => {}
            }
        }
        history
    }
}

/// TB-13 artifact contract: for each REQUIRED expectation, the matching
/// evidence must be present; otherwise the worker's claim does not
/// satisfy the contract.
fn contract_violations(
    expected: &[(String, bool)],
    artifact_ref_present: bool,
    verification_present: bool,
    diff_present: bool,
) -> Vec<ContractViolationView> {
    let mut violations = Vec::new();
    for (class, required) in expected {
        if !*required {
            continue;
        }
        let satisfied = match class.as_str() {
            "report" => artifact_ref_present,
            "verification_log" => verification_present,
            "diff" => diff_present,
            _ => false,
        };
        if !satisfied {
            violations.push(ContractViolationView {
                artifact_class: class.clone(),
                violation: format!("required artifact class `{class}` missing evidence"),
            });
        }
    }
    violations
}

#[async_trait]
impl TachiTaskBridge for InMemoryTachiTaskBridge {
    async fn submit(
        &self,
        intent: &TaskIntentV1,
        request_id: &RequestId,
    ) -> Result<SubmitReceipt, SubmitTransportError> {
        let mut state = self.state.lock();
        if intent.schema != SCHEMA_TAG {
            return Ok(SubmitReceipt::Rejected {
                reason: "schema_tag_mismatch".to_string(),
            });
        }
        // Host-side admission (authoritative; the compose-time scan is
        // pre-flight only). Same law, same categories.
        if let Err(rejection) = scan_intent(intent) {
            return Ok(SubmitReceipt::Rejected {
                reason: rejection.to_string(),
            });
        }
        let digest = intent.canonical_digest();
        let tuple = (intent.requester.to_string(), request_id.to_string());
        if let Some((bound_digest, task_ref)) = state.bindings.get(&tuple) {
            if *bound_digest != digest {
                return Ok(SubmitReceipt::RequestIdConflict {
                    bound_digest: bound_digest.clone(),
                    submitted_digest: digest,
                });
            }
            // TB-7 rule 2: same tuple + same digest ⇒ the SAME TaskRef,
            // never a second worker.
            let replayed = state.has_task_submitted(task_ref);
            return Ok(SubmitReceipt::Admitted {
                task_ref: task_ref.clone(),
                replayed,
            });
        }
        // TB-6: the host mints the TaskRef, after admission.
        let task_ref = state.mint_task_ref();
        state
            .bindings
            .insert(tuple, (digest.clone(), task_ref.clone()));
        state
            .owners
            .insert(task_ref.as_wire().to_string(), intent.requester.to_string());
        state.expected.insert(
            task_ref.as_wire().to_string(),
            intent
                .expected_artifacts
                .iter()
                .map(|a| {
                    (
                        serde_json::to_value(a.artifact_class)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_string))
                            .unwrap_or_default(),
                        a.required,
                    )
                })
                .collect(),
        );
        state.append(
            &task_ref,
            InMemoryFact {
                event_id: format!("submitted-{}", task_ref.as_wire()),
                kind: "task_submitted".to_string(),
                payload_digest: fact_digest(&serde_json::json!({
                    "kind": "task_submitted",
                    "intent_digest": digest,
                })),
                detail: FactDetail::TaskSubmitted { digest },
            },
        );
        Ok(SubmitReceipt::Admitted {
            task_ref,
            replayed: false,
        })
    }

    async fn get(&self, task_ref: &TaskRef) -> Result<TaskSnapshotView, BridgeQueryError> {
        let state = self.state.lock();
        if !state.has_task_submitted(task_ref) {
            return Err(BridgeQueryError::NotFound);
        }
        Ok(Self::snapshot_from(&state, task_ref))
    }

    async fn watch(
        &self,
        task_ref: &TaskRef,
        after_seq: u64,
        limit: usize,
    ) -> Result<TaskEventPageView, BridgeQueryError> {
        let state = self.state.lock();
        let Some(log) = state.facts.get(task_ref.as_wire()) else {
            return Err(BridgeQueryError::NotFound);
        };
        if !state.has_task_submitted(task_ref) {
            return Err(BridgeQueryError::NotFound);
        }
        let limit = limit.max(1);
        let missed: Vec<(u64, &InMemoryFact)> = log
            .iter()
            .filter(|(seq, _)| *seq > after_seq)
            .map(|(seq, fact)| (*seq, fact))
            .collect();
        let total = missed.len();
        let events = missed
            .into_iter()
            .take(limit)
            .map(|(seq, fact)| {
                // The payload-detail token rides the kind label so the
                // operation/mode of intervention and stop facts is
                // observable from the watch view (not just the id).
                let (kind, source_revision) = match &fact.detail {
                    FactDetail::InterventionForwarded {
                        operation,
                        intervention_id,
                    } => (
                        format!("intervention_forwarded_{}", op_token(operation)),
                        intervention_id.clone(),
                    ),
                    FactDetail::StopRequested { mode, stop_id, .. } => {
                        (format!("stop_requested_{}", mode.as_str()), stop_id.clone())
                    }
                    _ => (fact.kind.clone(), seq.to_string()),
                };
                TaskEventView {
                    seq,
                    event_id: fact.event_id.clone(),
                    source: "bridge".to_string(),
                    source_revision,
                    occurred_at: format!("t{seq}"),
                    recorded_at: format!("t{seq}"),
                    payload_digest: fact.payload_digest.clone(),
                    visibility: "internal".to_string(),
                    kind,
                }
            })
            .collect();
        Ok(TaskEventPageView {
            task_ref: task_ref.clone(),
            events,
            has_more: total > limit,
        })
    }

    async fn collect(
        &self,
        task_ref: &TaskRef,
        result_revision: Option<u64>,
    ) -> Result<ResultProjectionView, BridgeQueryError> {
        let state = self.state.lock();
        if !state.has_task_submitted(task_ref) {
            return Err(BridgeQueryError::NotFound);
        }
        // Fold the full history, then serve either the latest revision or
        // the exact pinned one (TB-13: newer wins; a pin is exact or
        // typed not-found).
        let mut revisions = Self::fold_revisions(&state, task_ref);
        match result_revision {
            None => revisions.pop().ok_or(BridgeQueryError::NotReady),
            Some(pinned) => revisions
                .into_iter()
                .find(|projection| projection.result_revision == pinned)
                .ok_or(BridgeQueryError::ResultRevisionNotFound),
        }
    }

    async fn intervene(
        &self,
        task_ref: &TaskRef,
        intervention: &InterventionV1,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<InterventionReceipt, InterventionError> {
        let op = intervention.discriminant();
        // TB-11: these are new task/adjudication lineage, NOT session
        // interventions — typed refusal, zero mutation, no fresh-task
        // fallback (mirrors the tachi host law exactly; this is the
        // SERVER-side half of "continuation ≠ independent review": the
        // session-intervention path can never mint review lineage).
        if matches!(
            op,
            InterventionStatic::RequestIndependentReview | InterventionStatic::Escalate
        ) {
            return Err(InterventionError::RequiresNewTaskLineage { operation: op });
        }
        // Single-pathed stop authority: the stop variants ARE
        // request_stop (TB-11/TB-12).
        if matches!(
            op,
            InterventionStatic::RequestGracefulStop | InterventionStatic::RequestHardCancel
        ) {
            let (mode, reason) = match intervention {
                InterventionV1::RequestGracefulStop { reason } => {
                    (StopMode::Graceful, reason.as_str().to_string())
                }
                InterventionV1::RequestHardCancel { reason } => {
                    (StopMode::Hard, reason.as_str().to_string())
                }
                _ => unreachable!("discriminant checked above"),
            };
            return self
                .stop_inner(
                    task_ref,
                    mode,
                    &reason,
                    requester,
                    request_id,
                    expected_task_revision,
                )
                .await
                .map(InterventionReceipt::Stop);
        }
        let mut state = self.state.lock();
        if !state.has_task_submitted(task_ref) {
            return Err(InterventionError::NotFound);
        }
        // TB-11 requester-owns-task: only the submitting requester may
        // intervene (collapsed to NotFound — existence is not leaked).
        if state
            .owners
            .get(task_ref.as_wire())
            .is_some_and(|owner| owner != &requester.to_string())
            || !state.owners.contains_key(task_ref.as_wire())
        {
            return Err(InterventionError::NotFound);
        }
        // Advertisement check: typed refusal, zero mutation (TB-11/TB-15).
        if !state.supported_interventions(task_ref).contains(&op) {
            return Err(InterventionError::UnsupportedByLifecycleOwner { operation: op });
        }
        // Revision-bound, never best-effort (TB-11).
        let revision = state.facts.get(task_ref.as_wire()).map_or(0, Vec::len) as u64;
        Self::revision_check(expected_task_revision, revision)?;
        // TB-7 rule 6 tuple law (mirrored from the host): bind first,
        // forward second; same tuple + same digest replays the same
        // receipt.
        let digest = {
            let payload = serde_json::to_value(intervention).unwrap_or_default();
            fact_digest(&serde_json::json!({
                "task": task_ref.as_wire(),
                "intervention": payload,
            }))
        };
        let tuple = (requester.to_string(), request_id.to_string());
        if let Some(bound) = state.bindings.get(&tuple) {
            if bound.0 != digest {
                return Err(InterventionError::RequestIdConflict {
                    bound_digest: bound.0.clone(),
                    submitted_digest: digest,
                });
            }
            return state
                .intervention_receipts
                .get(&tuple)
                .cloned()
                .ok_or(InterventionError::ReconciliationUnknown);
        }
        let intervention_id = format!(
            "iv:{}-{}",
            task_ref.as_wire(),
            uuid::Uuid::new_v4().simple()
        );
        let receipt = match op {
            InterventionStatic::ProvideAdditionalContext => InterventionReceipt::ContextProvided {
                intervention_id: intervention_id.clone(),
            },
            InterventionStatic::RequestCorrection => InterventionReceipt::CorrectionRequested {
                intervention_id: intervention_id.clone(),
            },
            InterventionStatic::RequestContinuation => InterventionReceipt::ContinuationRequested {
                intervention_id: intervention_id.clone(),
            },
            InterventionStatic::RequestUserInput => InterventionReceipt::UserInputRequested {
                intervention_id: intervention_id.clone(),
            },
            InterventionStatic::RequestPause => InterventionReceipt::Paused {
                intervention_id: intervention_id.clone(),
            },
            InterventionStatic::RequestResume => InterventionReceipt::Resumed {
                intervention_id: intervention_id.clone(),
            },
            // Stop/new-lineage ops never reach here (handled above).
            InterventionStatic::RequestGracefulStop
            | InterventionStatic::RequestHardCancel
            | InterventionStatic::RequestIndependentReview
            | InterventionStatic::Escalate => {
                return Err(InterventionError::UnsupportedByLifecycleOwner { operation: op });
            }
        };
        state
            .bindings
            .insert(tuple.clone(), (digest, task_ref.clone()));
        state.append(
            task_ref,
            InMemoryFact {
                event_id: format!("ivfwd-{intervention_id}"),
                kind: "intervention_forwarded".to_string(),
                payload_digest: fact_digest(&serde_json::json!({
                    "kind": "intervention_forwarded",
                    "operation": format!("{op:?}"),
                    "intervention_id": intervention_id,
                })),
                detail: FactDetail::InterventionForwarded {
                    operation: op,
                    intervention_id,
                },
            },
        );
        state.intervention_receipts.insert(tuple, receipt.clone());
        Ok(receipt)
    }

    async fn request_stop(
        &self,
        task_ref: &TaskRef,
        mode: StopMode,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<StopReceipt, InterventionError> {
        self.stop_inner(
            task_ref,
            mode,
            "",
            requester,
            request_id,
            expected_task_revision,
        )
        .await
    }
}

impl InMemoryTachiTaskBridge {
    /// The shared TB-11 revision-bound check (never best-effort).
    fn revision_check(
        expected_task_revision: Option<u64>,
        revision: u64,
    ) -> Result<(), InterventionError> {
        match expected_task_revision {
            Some(expected) if expected != revision => Err(InterventionError::RevisionConflict {
                expected,
                actual: revision,
            }),
            _ => Ok(()),
        }
    }

    /// The shared stop path (TB-11 single stop authority): bind the
    /// `(requester, request_id)` tuple against the stop digest
    /// `{task, mode}`, append the multi-stage stop fact, forward to the
    /// implicit owner, and return the receipt at stage `Forwarded` — the
    /// double NEVER produces `Confirmed` (only a real lifecycle owner
    /// can authoritatively confirm a cancellation).
    async fn stop_inner(
        &self,
        task_ref: &TaskRef,
        mode: StopMode,
        reason: &str,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<StopReceipt, InterventionError> {
        let mut state = self.state.lock();
        if !state.has_task_submitted(task_ref) {
            return Err(InterventionError::NotFound);
        }
        if state
            .owners
            .get(task_ref.as_wire())
            .is_some_and(|owner| owner != &requester.to_string())
        {
            return Err(InterventionError::NotFound);
        }
        if !state
            .supported_interventions(task_ref)
            .contains(&match mode {
                StopMode::Graceful => InterventionStatic::RequestGracefulStop,
                StopMode::Hard => InterventionStatic::RequestHardCancel,
            })
        {
            return Err(InterventionError::UnsupportedByLifecycleOwner {
                operation: match mode {
                    StopMode::Graceful => InterventionStatic::RequestGracefulStop,
                    StopMode::Hard => InterventionStatic::RequestHardCancel,
                },
            });
        }
        let revision = state.facts.get(task_ref.as_wire()).map_or(0, Vec::len) as u64;
        Self::revision_check(expected_task_revision, revision)?;
        let digest = fact_digest(&serde_json::json!({
            "task": task_ref.as_wire(),
            "mode": mode.as_str(),
        }));
        let tuple = (requester.to_string(), request_id.to_string());
        if let Some(bound) = state.bindings.get(&tuple) {
            if bound.0 != digest {
                return Err(InterventionError::RequestIdConflict {
                    bound_digest: bound.0.clone(),
                    submitted_digest: digest,
                });
            }
            // Same tuple + same stop digest: replay the same stop fact.
            if let Some(InterventionReceipt::Stop(stop)) = state.intervention_receipts.get(&tuple) {
                return Ok(stop.clone());
            }
            return Err(InterventionError::ReconciliationUnknown);
        }
        let stop_id = format!(
            "stop:{}-{}",
            task_ref.as_wire(),
            uuid::Uuid::new_v4().simple()
        );
        state
            .bindings
            .insert(tuple.clone(), (digest, task_ref.clone()));
        state.append(
            task_ref,
            InMemoryFact {
                event_id: format!("stopreq-{stop_id}"),
                kind: "stop_requested".to_string(),
                payload_digest: fact_digest(&serde_json::json!({
                    "kind": "stop_requested",
                    "mode": mode.as_str(),
                    "reason": reason,
                    "stop_id": stop_id,
                })),
                detail: FactDetail::StopRequested {
                    mode,
                    stop_id: stop_id.clone(),
                },
            },
        );
        let receipt = StopReceipt {
            task_ref: task_ref.clone(),
            stop_id,
            mode,
            stage: StopStage::Forwarded,
            request_id: request_id.to_string(),
        };
        state
            .intervention_receipts
            .insert(tuple, InterventionReceipt::Stop(receipt.clone()));
        Ok(receipt)
    }
}

/// A transport that is always down (TB-20 outage tests and fail-closed
/// proofs: every op returns typed `Unavailable`).
#[derive(Debug, Default)]
pub struct UnavailableTachiTaskBridge;

#[async_trait]
impl TachiTaskBridge for UnavailableTachiTaskBridge {
    async fn submit(
        &self,
        _intent: &TaskIntentV1,
        _request_id: &RequestId,
    ) -> Result<SubmitReceipt, SubmitTransportError> {
        Ok(SubmitReceipt::Unavailable)
    }

    async fn get(&self, _task_ref: &TaskRef) -> Result<TaskSnapshotView, BridgeQueryError> {
        Err(BridgeQueryError::Unavailable)
    }

    async fn watch(
        &self,
        _task_ref: &TaskRef,
        _after_seq: u64,
        _limit: usize,
    ) -> Result<TaskEventPageView, BridgeQueryError> {
        Err(BridgeQueryError::Unavailable)
    }

    async fn collect(
        &self,
        _task_ref: &TaskRef,
        _result_revision: Option<u64>,
    ) -> Result<ResultProjectionView, BridgeQueryError> {
        Err(BridgeQueryError::Unavailable)
    }

    async fn intervene(
        &self,
        _task_ref: &TaskRef,
        _intervention: &InterventionV1,
        _requester: &RequesterRef,
        _request_id: &RequestId,
        _expected_task_revision: Option<u64>,
    ) -> Result<InterventionReceipt, InterventionError> {
        Err(InterventionError::Unavailable)
    }

    async fn request_stop(
        &self,
        _task_ref: &TaskRef,
        _mode: StopMode,
        _requester: &RequesterRef,
        _request_id: &RequestId,
        _expected_task_revision: Option<u64>,
    ) -> Result<StopReceipt, InterventionError> {
        Err(InterventionError::Unavailable)
    }
}

/// A transport wrapper that DROPS the submit response exactly once after
/// the host has committed — the TB-7 rule-4 ambiguous-submit injector.
/// The first `submit` call returns [`SubmitTransportError`] (response
/// lost); every later call passes through to the inner transport. All
/// other ops pass through untouched.
pub struct AmbiguousSubmitOnce {
    inner: Arc<dyn TachiTaskBridge>,
    dropped: Mutex<bool>,
}

impl AmbiguousSubmitOnce {
    /// Wrap a transport; the first submit response is dropped.
    pub fn new(inner: Arc<dyn TachiTaskBridge>) -> Self {
        Self {
            inner,
            dropped: Mutex::new(false),
        }
    }
}

#[async_trait]
impl TachiTaskBridge for AmbiguousSubmitOnce {
    async fn submit(
        &self,
        intent: &TaskIntentV1,
        request_id: &RequestId,
    ) -> Result<SubmitReceipt, SubmitTransportError> {
        let should_drop = {
            let mut dropped = self.dropped.lock();
            if *dropped {
                false
            } else {
                *dropped = true;
                true
            }
        };
        if should_drop {
            // The host commit happens inside the inner call; the client
            // just never sees the receipt.
            let _ = self.inner.submit(intent, request_id).await;
            return Err(SubmitTransportError);
        }
        self.inner.submit(intent, request_id).await
    }

    async fn get(&self, task_ref: &TaskRef) -> Result<TaskSnapshotView, BridgeQueryError> {
        self.inner.get(task_ref).await
    }

    async fn watch(
        &self,
        task_ref: &TaskRef,
        after_seq: u64,
        limit: usize,
    ) -> Result<TaskEventPageView, BridgeQueryError> {
        self.inner.watch(task_ref, after_seq, limit).await
    }

    async fn collect(
        &self,
        task_ref: &TaskRef,
        result_revision: Option<u64>,
    ) -> Result<ResultProjectionView, BridgeQueryError> {
        self.inner.collect(task_ref, result_revision).await
    }

    async fn intervene(
        &self,
        task_ref: &TaskRef,
        intervention: &InterventionV1,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<InterventionReceipt, InterventionError> {
        self.inner
            .intervene(
                task_ref,
                intervention,
                requester,
                request_id,
                expected_task_revision,
            )
            .await
    }

    async fn request_stop(
        &self,
        task_ref: &TaskRef,
        mode: StopMode,
        requester: &RequesterRef,
        request_id: &RequestId,
        expected_task_revision: Option<u64>,
    ) -> Result<StopReceipt, InterventionError> {
        self.inner
            .request_stop(
                task_ref,
                mode,
                requester,
                request_id,
                expected_task_revision,
            )
            .await
    }
}

/// Snake-case token for an intervention discriminant (watch-kind labels).
fn op_token(op: &InterventionStatic) -> String {
    serde_json::to_value(op)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{op:?}"))
}
