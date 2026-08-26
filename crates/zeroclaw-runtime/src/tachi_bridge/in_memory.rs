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
use zeroclaw_api::taskintent::{AttemptRef, RequestId, SCHEMA_TAG, TaskIntentV1, TaskRef};

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
/// vocabulary for the four in-scope ops).
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
}

/// Real SHA-256 lower-hex digest over a canonical fact identity — the
/// `TaskEventView::payload_digest` contract is an actual hex digest, not
/// a label.
fn fact_digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\x1f");
    }
    let bytes = hasher.finalize();
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
                payload_digest: fact_digest(&[
                    "execution",
                    task_ref.as_wire(),
                    label,
                    &occurrence.to_string(),
                ]),
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
                payload_digest: fact_digest(&[
                    "adjudication",
                    task_ref.as_wire(),
                    label,
                    &occurrence.to_string(),
                ]),
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
                payload_digest: fact_digest(&[
                    "outcome",
                    task_ref.as_wire(),
                    event_id.as_str(),
                    reported_outcome,
                ]),
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
                FactDetail::Adjudication { label } => {
                    adjudication =
                        ProjectedAdjudicationState::project(label).expect("host label is mapped");
                }
                FactDetail::OutcomeObserved { .. } => {
                    delivery = ProjectedDeliveryState::project("ready").expect("ready is mapped");
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
                payload_digest: fact_digest(&["task_submitted", task_ref.as_wire(), &digest]),
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
            .map(|(seq, fact)| TaskEventView {
                seq,
                event_id: fact.event_id.clone(),
                source: "bridge".to_string(),
                source_revision: seq.to_string(),
                occurred_at: format!("t{seq}"),
                recorded_at: format!("t{seq}"),
                payload_digest: fact.payload_digest.clone(),
                visibility: "internal".to_string(),
                kind: fact.kind.clone(),
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
}
