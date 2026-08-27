//! The procedure-run client: compose + submit a `ProcedureSnapshotV1`
//! through the DECISION KP-16/E option-(b) carrier, then supervise the
//! Tachi-owned run through bridge refs only (get/watch/collect) and
//! derive candidate-only learning output.
//!
//! Zero durable state (KP-16): the client holds no run ledger, no gate
//! ledger, no cursor store — the TB-9 cursors are the bridge client's
//! process-lifetime ones, and restart rehydration is TB-7 same-tuple
//! replay (see [`derive_request_id`]).
//!
//! Narrow-only law (KP-17): the procedure's required capability (from
//! the snapshot's compiled guidance) must already be within the
//! requester's own admitted set — a procedure can never originate or
//! widen capability, and the refusal is typed and procedure-specific.

use std::sync::Arc;

use zeroclaw_api::procedure_v1::{
    CandidatePolicyDisposition, LearningCandidateV1, LearningTargetKind,
    PROCEDURE_SNAPSHOT_MAX_BYTES, PROCEDURE_SNAPSHOT_REF_PREFIX, ProcedureRunBinding,
    ProcedureSnapshotV1,
};
use zeroclaw_api::taskintent::{
    ArtifactExpectation, BoundedText, Capability, RequestId, RequesterRef, SourceKind, SourceRef,
    TaskRef,
};
use zeroclaw_log::{Action, Event, EventOutcome};

use crate::tachi_bridge::client::{
    BridgeQueryError, ResultProjectionView, SubmitReceipt, SubmitTransportError, TachiBridgeClient,
};
use crate::tachi_bridge::compose::{
    ComposeRejection, RequesterBridgePolicy, StructuralIntentContext, TaskIntentInputs,
    compose_intent,
};
use crate::tachi_bridge::procedure::ProcedureSubmitPort;

/// Typed procedure-run submission refusal.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ProcedureSubmitError {
    /// KP-17 narrow-only law: the procedure's required capability is
    /// outside the requester's own admitted set.
    #[error(
        "procedure run refused: requires capability {required:?} which the requester's admitted policy does not permit (guidance can only narrow)"
    )]
    CapabilityNotAdmitted {
        /// The capability the pinned snapshot requires.
        required: Capability,
    },
    /// The bounded-carrier law (typed refusal, never truncation).
    #[error(
        "procedure run refused: serialized snapshot is {len} bytes, over the {max} byte carrier bound"
    )]
    Oversize {
        /// Actual serialized length.
        len: usize,
        /// The frozen maximum.
        max: usize,
    },
    /// The snapshot's CAS ref is not content-addressed (a bare-path
    /// binding attempt — KP-11/DECISION E: refused).
    #[error(
        "procedure run refused: snapshot ref is not a content-addressed CAS ref (bare-path binding is refused)"
    )]
    BarePathBinding,
    /// Encode-side admission rejected the composed intent (TB-4).
    #[error("procedure run refused: {0}")]
    Compose(#[from] ComposeRejection),
    /// The transport-level submit failed before observation.
    #[error("procedure run submit transport failure")]
    Transport(#[from] SubmitTransportError),
}

/// A submitted procedure run: the Tachi task identity plus the KP-15
/// four-field binding.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcedureRunDriverOutput {
    /// The Tachi-minted task carrying the run.
    pub task_ref: TaskRef,
    /// The definition-side run binding.
    pub binding: ProcedureRunBinding,
    /// Whether the submission was an idempotent TB-7 replay.
    pub replayed: bool,
}

/// Derive the deterministic request id for one procedure run instance:
/// the TB-7 tuple `(requester, request_id)` replays to the SAME task,
/// so the id is a pure function of (procedure identity, pinned digest,
/// run-instance identity). The run-instance identity comes from the
/// trigger/event source (durable at its origin), never from local
/// state — a ZeroClaw restart re-derives the same tuple and rehydrates
/// through the bridge (KP-13; durable client-side cursors are the
/// TB-19 Batch-4 surface and are deliberately not built here).
#[must_use]
pub fn derive_request_id(procedure_id: &str, digest: &str, run_instance_id: &str) -> RequestId {
    let digest_of = zeroclaw_api::taskintent::canonical_json_digest_hex(&serde_json::json!({
        "procedure_id": procedure_id,
        "digest": digest,
        "run_instance_id": run_instance_id,
    }));
    RequestId::try_from(format!("procrun-{digest_of}")).expect("hex digest is bounded")
}

/// The procedure-run client: submits through the E(b) carrier and
/// supervises through the base bridge client. Holds no durable state.
#[derive(Clone)]
pub struct ProcedureRunClient {
    bridge: TachiBridgeClient,
    carrier: Arc<dyn ProcedureSubmitPort>,
}

impl ProcedureRunClient {
    /// Bind a procedure-run client to its carrier transport (which also
    /// implements the base bridge port — see the E2E transport).
    pub fn new(
        bridge_port: Arc<dyn crate::tachi_bridge::TachiTaskBridge>,
        carrier: Arc<dyn ProcedureSubmitPort>,
    ) -> Self {
        Self {
            bridge: TachiBridgeClient::new(bridge_port),
            carrier,
        }
    }

    /// The base bridge client (get/watch/collect over the same
    /// transport).
    #[must_use]
    pub fn bridge(&self) -> &TachiBridgeClient {
        &self.bridge
    }

    /// Submit one procedure run. Encode-side law, in order:
    ///
    /// 1. snapshot self-consistency + carrier size bound (typed
    ///    `Oversize`);
    /// 2. the snapshot ref must be the content-addressed CAS shape
    ///    (`proceduresnap:<digest>` — bare-path binding refused);
    /// 3. KP-17 narrow-only: the snapshot's required capability must be
    ///    within the requester's own admitted set;
    /// 4. compose the intent through the frozen five-value surface
    ///    (objective from the definition purpose, artifacts/evaluation
    ///    from the snapshot) and run the full TB-4 encode-side scan;
    /// 5. hand `(intent, request_id, snapshot)` to the carrier — the
    ///    transport verifies and retains the bytes BEFORE acknowledging
    ///    (DECISION KP-16/E option (b)).
    pub async fn submit_run(
        &self,
        snapshot: &ProcedureSnapshotV1,
        policy: &RequesterBridgePolicy,
        requester: &RequesterRef,
        request_id: &RequestId,
    ) -> Result<ProcedureRunDriverOutput, ProcedureSubmitError> {
        let serialized = snapshot.serialized_len();
        if serialized > PROCEDURE_SNAPSHOT_MAX_BYTES {
            return Err(ProcedureSubmitError::Oversize {
                len: serialized,
                max: PROCEDURE_SNAPSHOT_MAX_BYTES,
            });
        }
        let snapshot_ref = snapshot.snapshot_ref();
        if !snapshot_ref.starts_with(PROCEDURE_SNAPSHOT_REF_PREFIX)
            || snapshot_ref.contains('/')
            || snapshot_ref.contains('~')
            || snapshot_ref.contains("..")
        {
            return Err(ProcedureSubmitError::BarePathBinding);
        }
        let required = snapshot.guidance.required_capability;
        if !policy.admitted_capabilities.contains(&required) {
            return Err(ProcedureSubmitError::CapabilityNotAdmitted { required });
        }

        let objective = BoundedText::new(format!(
            "Execute procedure {} revision {}: {} steps per the pinned snapshot; produce the declared artifacts.",
            snapshot.procedure_id,
            snapshot.procedure_revision,
            snapshot.steps.len(),
        ))
        .map_err(|_| {
            ComposeRejection::ForbiddenContent {
                category: crate::tachi_bridge::compose::ForbiddenCategory::ExecutionDetail,
                field: "objective",
            }
        })?;
        // The authored purpose rides as the first constraint — scanned
        // by the same encode-side law (a purpose naming forbidden
        // content is a typed refusal, never silent truncation).
        let mut constraints = vec![TaskConstraintOf::from_text(format!(
            "procedure purpose: {}",
            purpose_text(snapshot)
        ))?];
        for constraint in &snapshot.guidance.objective_constraints {
            constraints.push(TaskConstraintOf::from_bounded(constraint.clone()));
        }

        let expected_artifacts = snapshot
            .guidance
            .artifact_expectations
            .iter()
            .map(|expectation| ArtifactExpectation {
                artifact_class: expectation.artifact_class,
                description: expectation.description.clone(),
                required: expectation.required,
            })
            .collect();

        let inputs = TaskIntentInputs {
            objective,
            capability_request: zeroclaw_api::taskintent::CapabilityRequest {
                capability: required,
            },
            constraints: constraints.into_iter().map(|c| c.0).collect(),
            expected_artifacts,
            evaluation_requirement: snapshot.guidance.evaluation_requirement.clone(),
        };
        let context = StructuralIntentContext {
            requester: requester.clone(),
            parent_ref: None,
            supervisor_ref: None,
            context_bundle_ref: BoundedText::new(snapshot_ref.clone())
                .expect("CAS ref is bounded hex"),
            source_refs: vec![SourceRef {
                kind: SourceKind::Document,
                locator: BoundedText::new(format!(
                    "procedure/{}@{}",
                    snapshot.procedure_id, snapshot.procedure_revision
                ))
                .expect("bounded locator"),
            }],
            expiry: None,
            retry_of: None,
        };
        let intent = compose_intent(&inputs, policy, &context)?;
        let receipt = self
            .carrier
            .submit_procedure_run(&intent, request_id, snapshot)
            .await?;
        match receipt {
            SubmitReceipt::Admitted { task_ref, replayed } => Ok(ProcedureRunDriverOutput {
                task_ref,
                binding: ProcedureRunBinding::from_snapshot(snapshot),
                replayed,
            }),
            SubmitReceipt::Rejected { reason } => {
                ::zeroclaw_log::record!(
                    WARN,
                    Event::new(module_path!(), Action::Reject)
                        .with_outcome(EventOutcome::Failure)
                        .with_attrs(serde_json::json!({ "reason": reason })),
                    "procedure_v1: host rejected the procedure run submission"
                );
                Err(ProcedureSubmitError::Transport(SubmitTransportError))
            }
            // Ambiguity surfaces typed; the caller replays the SAME
            // (requester, request_id) tuple — never invents a new id.
            SubmitReceipt::Unavailable
            | SubmitReceipt::ReconciliationUnknown { .. }
            | SubmitReceipt::RequestIdConflict { .. } => {
                Err(ProcedureSubmitError::Transport(SubmitTransportError))
            }
        }
    }

    /// `get(task_ref)` over the base bridge (TB-8).
    pub async fn get(
        &self,
        task_ref: &TaskRef,
    ) -> Result<crate::tachi_bridge::TaskSnapshotView, BridgeQueryError> {
        self.bridge.get(task_ref).await
    }

    /// `watch` new events over the base bridge (TB-9).
    pub async fn watch_new_events(
        &self,
        task_ref: &TaskRef,
        limit: usize,
    ) -> Result<crate::tachi_bridge::TaskEventPageView, BridgeQueryError> {
        self.bridge.watch_new_events(task_ref, limit).await
    }

    /// `collect` the latest result projection (TB-13).
    pub async fn collect_latest(
        &self,
        task_ref: &TaskRef,
    ) -> Result<ResultProjectionView, BridgeQueryError> {
        self.bridge.collect_latest(task_ref).await
    }

    /// The Tachi-retained snapshot for a CAS ref, if the transport
    /// still holds it (KP-13 projection: audit the admitted bytes while
    /// ZeroClaw was/is offline).
    pub async fn retained_snapshot(
        &self,
        snapshot_ref: &str,
    ) -> Result<Option<ProcedureSnapshotV1>, SubmitTransportError> {
        self.carrier.retained_snapshot(snapshot_ref).await
    }
}

/// Small helper carrying a `TaskConstraint` through the two text paths.
struct TaskConstraintOf(zeroclaw_api::taskintent::TaskConstraint);

impl TaskConstraintOf {
    fn from_text(text: String) -> Result<Self, ProcedureSubmitError> {
        Ok(Self(zeroclaw_api::taskintent::TaskConstraint {
            description: BoundedText::new(text).map_err(|_| {
                ComposeRejection::ForbiddenContent {
                    category: crate::tachi_bridge::compose::ForbiddenCategory::ExecutionDetail,
                    field: "constraints.description",
                }
            })?,
        }))
    }
    fn from_bounded(bounded: BoundedText) -> Self {
        Self(zeroclaw_api::taskintent::TaskConstraint {
            description: bounded,
        })
    }
}

fn purpose_text(snapshot: &ProcedureSnapshotV1) -> String {
    // The purpose lives in the captured TOML's description; recover it
    // from the embedded bytes so the projection stays derived from the
    // pinned content (never a live re-read).
    let manifest: Result<crate::sop::types::SopManifest, _> =
        toml::from_str(&snapshot.definition_toml);
    match manifest {
        Ok(manifest) => manifest.sop.description,
        Err(_) => String::new(),
    }
}

/// Derive a candidate-only learning output from a completed run's
/// result projection (KP-18/KP-19). Pure: no apply path exists for any
/// target kind — promotion is a separate, named, HUMAN/operator-gated
/// action on the reviewed-promotion surface
/// (`LearningCandidateV1::to_proposed_candidate` routes into it).
#[must_use]
pub fn derive_learning_candidate(
    snapshot: &ProcedureSnapshotV1,
    task_ref: &TaskRef,
    projection: &ResultProjectionView,
    sensitive_derivation: Option<&str>,
) -> LearningCandidateV1 {
    let digest_of = zeroclaw_api::taskintent::canonical_json_digest_hex(&serde_json::json!({
        "task": task_ref.as_wire(),
        "snapshot": snapshot.canonical_digest(),
        "artifacts": projection.artifact_evidence_refs,
    }));
    LearningCandidateV1 {
        candidate_id: format!("cand-{digest_of}"),
        target_kind: LearningTargetKind::ProcedureRevision,
        source_task_refs: vec![task_ref.as_wire().to_string()],
        evidence_refs: projection.artifact_evidence_refs.clone(),
        // KP-19: mandatory exactly when the derivation touched
        // sensitive source material.
        derivation_ref: sensitive_derivation.map(str::to_string),
        proposed_patch_digest: digest_of,
        confidence: 0.5,
        uncertainty: BoundedText::new(
            "single-run evidence; requires operator review before any change",
        )
        .expect("static bounded text"),
        policy_disposition: CandidatePolicyDisposition::ReviewQueued,
    }
}
