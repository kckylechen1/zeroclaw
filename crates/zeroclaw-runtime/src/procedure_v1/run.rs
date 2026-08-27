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
    compose_intent, scan_client_authored_refs,
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
    /// The snapshot failed invariant re-derivation: its authority-bearing
    /// claims (required capability, approval gates, review state, step
    /// projection) are not what its pinned bytes actually say. Kills the
    /// forged/mutated-snapshot class: submit trusts only the bytes.
    #[error(
        "procedure run refused: snapshot invariant mismatch (`{field}` does not match the pinned definition bytes)"
    )]
    SnapshotInvariant {
        /// Which re-derived invariant failed.
        field: &'static str,
    },
    /// The transport acknowledged the run but cannot produce the
    /// retained snapshot bytes (verify-before-ack violated — refused).
    #[error(
        "procedure run refused: transport did not retain the snapshot bytes before acknowledging"
    )]
    NotRetainedBeforeAck,
    /// The same `(requester, request_id)` tuple is already bound to a
    /// different intent digest (TB-7 rule 3) — typically a policy change
    /// across a restart. Zero new execution; the caller must use a new
    /// run-instance identity, never replay this tuple with new policy.
    #[error(
        "procedure run refused: request id already bound to a different intent (policy or content changed; use a new run instance)"
    )]
    RequestIdConflict,
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
    pub fn new<T>(transport: Arc<T>) -> Self
    where
        T: crate::tachi_bridge::TachiTaskBridge + ProcedureSubmitPort + 'static,
    {
        Self {
            bridge: TachiBridgeClient::new(transport.clone()),
            carrier: transport,
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
        // Invariant re-derivation from the PINNED BYTES: a snapshot is
        // a public wire type, so submit trusts nothing but its bytes —
        // review state, capability requirement, gates, and the step
        // projection must all be what the embedded definition says.
        verify_snapshot_invariants(snapshot)?;
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
        // The base client's ref-wire hardening applies here too: the
        // requester claim and lineage refs are content-scanned before
        // any transport sees the intent (same law as base submit).
        scan_client_authored_refs(&intent)?;
        let receipt = self
            .carrier
            .submit_procedure_run(&intent, request_id, snapshot)
            .await?;
        match receipt {
            SubmitReceipt::Admitted { task_ref, replayed } => {
                // Encoded verify-before-ack: a transport that
                // acknowledged without retaining cannot produce the
                // bytes for the ref it just bound — refuse loudly
                // instead of proceeding on an unbacked acknowledgment.
                let retained = self.carrier.retained_snapshot(&snapshot_ref).await;
                let verified = matches!(
                    &retained,
                    Ok(Some(retained)) if retained.canonical_digest() == snapshot.canonical_digest()
                );
                if !verified {
                    return Err(ProcedureSubmitError::NotRetainedBeforeAck);
                }
                Ok(ProcedureRunDriverOutput {
                    task_ref,
                    binding: ProcedureRunBinding::from_snapshot(snapshot),
                    replayed,
                })
            }
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
            // A TB-7 rule-3 conflict is TYPED truth (typically a
            // policy change across a restart), not a transport
            // failure.
            SubmitReceipt::RequestIdConflict { .. } => Err(ProcedureSubmitError::RequestIdConflict),
            // Ambiguity surfaces typed; the caller replays the SAME
            // (requester, request_id) tuple — never invents a new id.
            SubmitReceipt::Unavailable | SubmitReceipt::ReconciliationUnknown { .. } => {
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

/// Re-derive every authority-bearing snapshot claim from the PINNED
/// definition bytes (KP-11/KP-17): a forged or mutated snapshot — lowered
/// `required_capability`, removed `approval_gates`, draft review state,
/// steps that are not what the embedded markdown parses to — is refused
/// here, at submit, regardless of how the snapshot object was
/// constructed.
///
/// Crate-public: the reference carrier re-runs the same law at the port
/// (defense-in-depth — a direct port caller cannot skip it).
pub(crate) fn verify_snapshot_invariants(
    snapshot: &ProcedureSnapshotV1,
) -> Result<(), ProcedureSubmitError> {
    use crate::sop::types::{SopManifest, SopStepKind};
    let mismatch = |field: &'static str| ProcedureSubmitError::SnapshotInvariant { field };

    // 1. The embedded definition bytes re-derive the pinned digest.
    let derived = zeroclaw_api::taskintent::canonical_json_digest_hex(&serde_json::json!({
        "sop_toml": snapshot.definition_toml,
        "sop_md": snapshot.definition_md,
    }));
    if derived != snapshot.procedure_digest {
        return Err(mismatch("procedure_digest"));
    }
    // 2. The embedded manifest is the published revision the snapshot
    //    claims (review truth from the RAW bytes).
    let manifest: SopManifest =
        toml::from_str(&snapshot.definition_toml).map_err(|_| mismatch("definition_toml"))?;
    if manifest.sop.version != snapshot.procedure_revision {
        return Err(mismatch("procedure_revision"));
    }
    // The snapshot is a TOTAL function of the captured bytes for the
    // SOP package format: identity, provenance, privacy class, and the
    // format-empty collections (constraints, skill refs) are fixed
    // projections — a forge cannot originate values there either.
    if manifest.sop.name != snapshot.procedure_id {
        return Err(mismatch("procedure_id"));
    }
    if snapshot.provenance.authored_via != "zeroclaw-sops-dir" {
        return Err(mismatch("provenance"));
    }
    if snapshot.privacy_class != zeroclaw_api::taskintent::PrivacyClass::Public {
        return Err(mismatch("privacy_class"));
    }
    if !snapshot.source_skill_refs.is_empty()
        || !snapshot.resolved_constraints.is_empty()
        || !snapshot.guidance.objective_constraints.is_empty()
    {
        return Err(mismatch("projection_purity"));
    }
    if super::definition::review_state_of(&snapshot.definition_toml)
        .map_err(|_| mismatch("review_state"))?
        != zeroclaw_api::procedure_v1::DefinitionReviewState::Published
    {
        return Err(mismatch("review_state"));
    }
    // 3. The step projection IS what the embedded definition parses
    //    to. TOML-only manifest steps are a legal source (markdown
    //    absent ⇒ manifest steps) — the fallback below handles it.
    let parsed = crate::sop::parse_steps(&snapshot.definition_md);
    // TOML-only fallback: the manifest steps carry authored numbers
    // that may legitimately default to zero — apply the SAME dense
    // 1..=N renumbering the capture applies before comparing.
    let renumbered_manifest: Vec<crate::sop::types::SopStep> = {
        let mut steps = manifest.steps.clone();
        for (index, step) in steps.iter_mut().enumerate() {
            step.number = (index as u32) + 1;
        }
        steps
    };
    let steps_source: &Vec<crate::sop::types::SopStep> = if parsed.is_empty() {
        &renumbered_manifest
    } else {
        &parsed
    };
    if steps_source.len() != snapshot.steps.len() {
        return Err(mismatch("steps"));
    }
    for (step, projected) in steps_source.iter().zip(&snapshot.steps) {
        let body_ok = zeroclaw_api::taskintent::BoundedText::new(step.body.clone())
            .map(|body| body.as_str() == projected.body.as_str())
            .unwrap_or(false);
        if step.number != projected.number
            || step.title != projected.title
            || !body_ok
            || step.suggested_tools != projected.suggested_tools
            || step.requires_confirmation != projected.requires_confirmation
        {
            return Err(mismatch("steps"));
        }
        let kind_label = match step.kind {
            SopStepKind::Execute => "execute",
            SopStepKind::Checkpoint => "checkpoint",
            SopStepKind::Capability => "capability",
        };
        if projected.kind != kind_label {
            return Err(mismatch("steps"));
        }
    }
    // 4. The gates are exactly the parsed confirmation/checkpoint
    //    steps, WITH their authored approval policies.
    let mut gates: Vec<(u32, Option<String>)> = steps_source
        .iter()
        .filter(|step| step.requires_confirmation || step.kind == SopStepKind::Checkpoint)
        .map(|step| (step.number, step.policy.clone()))
        .collect();
    gates.sort();
    let mut claimed: Vec<(u32, Option<String>)> = snapshot
        .approval_gates
        .iter()
        .map(|gate| (gate.step, gate.policy.clone()))
        .collect();
    claimed.sort();
    if gates != claimed {
        return Err(mismatch("approval_gates"));
    }
    // 5. The guidance's artifact expectations are the DEFINITION-derived
    //    projections (Report always; VerificationLog iff any step
    //    declares a contract) and all required — a forge cannot strip
    //    required evidence. The evaluation requirement is the frozen
    //    deterministic-check class — a forge cannot weaken evaluation.
    let schema_any = steps_source.iter().any(|step| step.schema.is_some());
    let expected_classes: std::collections::BTreeSet<&str> = if schema_any {
        ["report", "verification_log"].into_iter().collect()
    } else {
        ["report"].into_iter().collect()
    };
    let claimed_classes: std::collections::BTreeSet<&str> = snapshot
        .guidance
        .artifact_expectations
        .iter()
        .map(|expectation| match expectation.artifact_class {
            zeroclaw_api::taskintent::ArtifactClass::Report => "report",
            zeroclaw_api::taskintent::ArtifactClass::Diff => "diff",
            zeroclaw_api::taskintent::ArtifactClass::VerificationLog => "verification_log",
        })
        .collect();
    if expected_classes != claimed_classes
        || snapshot
            .guidance
            .artifact_expectations
            .iter()
            .any(|expectation| !expectation.required)
    {
        return Err(mismatch("artifact_expectations"));
    }
    if snapshot.guidance.evaluation_requirement.independence
        != zeroclaw_api::taskintent::IndependenceClass::DeterministicCheck
    {
        return Err(mismatch("evaluation_requirement"));
    }
    // 6. Forbidden content categories apply at submit exactly as at
    //    mint (a minted-then-tampered body carrying credentials or
    //    Private-Dyad material dies here, existence-blind).
    super::snapshot::snapshot_content_scan(&snapshot.definition_toml, &snapshot.definition_md)
        .map_err(|_| mismatch("content_scan"))?;
    // 7. The required capability is what the pinned steps' FULL tool
    //    surface implies (narrow-only: guidance can never lower it
    //    below what the procedure actually does).
    if super::snapshot::required_capability_of(steps_source)
        != snapshot.guidance.required_capability
    {
        return Err(mismatch("required_capability"));
    }
    // 8. The guidance digest binds the embedded guidance (shared
    //    binding rule: digest over the payload with the field itself
    //    normalized to empty).
    if super::snapshot::guidance_payload_digest(&snapshot.guidance)
        != snapshot.compiled_guidance_digest
        || snapshot.guidance.guidance_digest != snapshot.compiled_guidance_digest
    {
        return Err(mismatch("compiled_guidance_digest"));
    }
    // 9. TOTALITY SEAL: the snapshot must be exactly the PURE MINT of
    //    its own embedded bytes — recapture the bytes and re-mint, then
    //    require the canonical digests to match. This subsumes every
    //    per-field check above AND the fields no enumeration names
    //    (artifact/evidence descriptions, required-checks text,
    //    user_input_points, privacy text, the evaluation-contract
    //    digest, the schema tag): there is no field a forge can
    //    originate that the remint does not re-derive from the bytes.
    //    A mutation that recomputes every digest remains refusable
    //    here, because the remint of the SAME bytes is unique.
    let recaptured = super::definition::recapture_from_bytes(
        &snapshot.procedure_id,
        snapshot.definition_toml.clone(),
        snapshot.definition_md.clone(),
    )
    .map_err(|_| mismatch("definition_bytes"))?;
    let reminted =
        super::snapshot::mint_snapshot(&recaptured).map_err(|_| mismatch("definition_bytes"))?;
    if reminted.canonical_digest() != snapshot.canonical_digest() {
        return Err(mismatch("snapshot_totality"));
    }
    Ok(())
}
