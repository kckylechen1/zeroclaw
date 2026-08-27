//! Procedure vertical V4 wire/domain types (frozen contract #207 rev 3,
//! KP-10/KP-11/KP-15/KP-17/KP-18/KP-19; wire dependency #205).
//!
//! The split this module carries:
//!
//! - **ProcedureDefinitionV1 is ZeroClaw-owned authored state** (KP-10,
//!   RULING-207 §3): authoring, revision creation, applicability, and
//!   review state stay in the ZeroClaw definitions tree. There is no
//!   second mutable definition catalogue here — Tachi-side retention is
//!   of the immutable SNAPSHOT only.
//! - **ProcedureSnapshotV1 is immutable run input** (KP-11): minted at run
//!   creation from a PUBLISHED revision, content-addressed by canonical
//!   digest, carrying the full KP-11 field set. It is provenance/input —
//!   never a second canonical definition.
//! - **ProcedureRunBinding (KP-15)** is the four-field run binding
//!   `(procedure_id, revision, digest, snapshot_ref)`. Deliberately NOT
//!   named `ProcedureRunRef`: the KP-15 tuple is a definition-side
//!   identity projection carried on the ZeroClaw side; it is never used
//!   as a Tachi task ref and never reuses a run-state CAS or
//!   gate-presentation revision counter as a definition revision.
//! - **CompiledProcedureGuidanceV1 (KP-17)** is data, not authority: it
//!   can only NARROW — the required capability it names must already be
//!   within the requester's own admitted set, and oversize input is a
//!   typed refusal, never silent truncation.
//! - **LearningCandidateV1 (KP-18/KP-19)** is candidate-only output: zero
//!   apply paths exist for any target kind; sensitive sources carry a
//!   mandatory `derivation_ref`.
//!
//! Privacy law (KP-20/RULING-207 §11): the snapshot and guidance admit
//! only privacy-admitted projections — Private Dyad content/ids/counts
//! are structurally unrepresentable here (no field carries them; the
//! mint refuses Private-Dyad-markered definition content with an
//! existence-blind error).

use serde::{Deserialize, Serialize};

use crate::subagent_v1::{CandidateProvenance, ProposedCandidate, ProposedCandidateKind};
use crate::taskintent::{
    ArtifactClass, BoundedText, Capability, EvaluationRequirement, PrivacyClass,
    canonical_json_digest_hex,
};

/// Schema tag of the immutable procedure snapshot.
pub const PROCEDURE_SNAPSHOT_SCHEMA: &str = "procedure-snapshot.v1";

/// Maximum serialized size of one procedure snapshot (KP-11 additional
/// constraints / KP-17 bounded-carrier law: oversize is a typed refusal,
/// never silent truncation). 256 KiB covers SOP packages (manifest +
/// steps markdown) plus the compiled guidance projection with ample
/// headroom while keeping the DECISION KP-16/E option-(b) embedded
/// submit envelope bounded.
pub const PROCEDURE_SNAPSHOT_MAX_BYTES: usize = 256 * 1024;

/// Wire prefix of a Tachi-retained content-addressed snapshot ref
/// (DECISION KP-16/E common denominator: the retained ref is a CAS ref
/// held Tachi-side and NEVER resolves through the mutable definitions
/// directory).
pub const PROCEDURE_SNAPSHOT_REF_PREFIX: &str = "proceduresnap:";

/// Review state of a procedure definition revision (KP-11 publication
/// rule 1: a snapshot mint from a DRAFT revision is refused).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionReviewState {
    /// Authored but not yet published — minting a run snapshot from this
    /// state is a typed refusal.
    Draft,
    /// Published by the authoring side; the only state a snapshot may be
    /// minted from.
    Published,
}

/// One procedure step projected into the definition/snapshot (the
/// runtime-side `SopStep` mapped onto the wire shape; machine-independent
/// — no paths, no commands-as-authority, prose plus typed tool hints).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcedureStepV1 {
    /// Ordinal position within the procedure (1-based, dense).
    pub number: u32,
    /// Short human label.
    pub title: String,
    /// The step's instruction body (bounded prose; guidance, not
    /// authority — KP-17).
    pub body: BoundedText,
    /// Advisory tool names the step names for its execution (closed
    /// per-step hints; never a grant).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_tools: Vec<String>,
    /// Whether the step pauses for human confirmation before running.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub requires_confirmation: bool,
    /// Step kind label (`execute` | `checkpoint` | `capability`).
    pub kind: String,
}

/// A gate a procedure step declares (drives the Tachi-side approval
/// authority and the intent's `approval_requirement` consistency).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcedureGateV1 {
    /// The step that pauses.
    pub step: u32,
    /// Named approval policy the gate references, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
}

/// The ZeroClaw-owned authored definition projection (KP-10 field freeze:
/// procedure_id, revision, digest, name, purpose, applicability, steps,
/// dependencies, approvals, constraints, expected_artifacts,
/// evidence_contract, evaluation_contract, privacy_class, provenance,
/// review_state — dependencies/evidence_contract fold into the snapshot
/// expectations where the SOP format carries them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcedureDefinitionV1 {
    /// Stable procedure identity (the SOP package name).
    pub procedure_id: String,
    /// Definition revision identity — the manifest `version` string plus
    /// the content digest binding below. This is a DEFINITION revision:
    /// never a run-state CAS counter, never a gate-presentation revision
    /// (KP-15).
    pub revision: String,
    /// Canonical digest over the captured definition bytes (sha256 hex).
    pub digest: String,
    /// Human name.
    pub name: String,
    /// Purpose/description prose.
    pub purpose: BoundedText,
    /// Applicability summary (trigger surfaces the procedure binds).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applicability: Vec<String>,
    /// Ordered steps.
    pub steps: Vec<ProcedureStepV1>,
    /// Declared approval gates.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approval_gates: Vec<ProcedureGateV1>,
    /// Resolved constraints/checks the procedure asserts (KP-11).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<BoundedText>,
    /// Expected artifacts/evidence (KP-11 artifact/evidence expectations).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_artifacts: Vec<ArtifactExpectationV1>,
    /// Evaluation contract revision identity + digest.
    pub evaluation_contract: EvaluationContractRef,
    /// Privacy class of the definition content.
    pub privacy_class: PrivacyClass,
    /// Provenance of the definition (authoring surface + capture time).
    pub provenance: DefinitionProvenance,
    /// Review state of this revision (KP-11 publication rule 1).
    pub review_state: DefinitionReviewState,
}

/// One expected artifact/evidence entry (drives TB-13-style contract
/// satisfaction checks on the Tachi side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactExpectationV1 {
    /// Closed artifact class (report/diff/verification_log).
    pub artifact_class: ArtifactClass,
    /// What satisfies this expectation.
    pub description: BoundedText,
    /// Whether absence fails the evaluation contract.
    pub required: bool,
}

/// Evaluation contract identity (KP-11 `evaluation contract
/// revision/digest`; KP-21: Tachi owns adjudication of results against
/// this contract — ZeroClaw selects it, never grades against it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationContractRef {
    /// Contract revision label (e.g. `procedure-eval.v1`).
    pub revision: String,
    /// Canonical digest of the contract content.
    pub digest: String,
}

/// Provenance of a captured definition revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DefinitionProvenance {
    /// Which surface authored/captured the revision (e.g.
    /// `zeroclaw-sops-dir`).
    pub authored_via: String,
    /// When the bytes were captured (RFC3339).
    pub captured_at: String,
}

/// A source SkillDefinition reference folded into the snapshot (KP-11:
/// `source SkillDefinition refs + revisions + digests`). Absent when the
/// procedure does not bind skills.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSkillRefV1 {
    /// Skill identity.
    pub skill_id: String,
    /// Skill revision label.
    pub revision: String,
    /// Canonical digest of the referenced skill revision content.
    pub digest: String,
}

/// Compiled task guidance (KP-17): the bounded, digest-bound projection
/// of a procedure (plus applicable skills) into the task intent. Data,
/// not authority — it can only NARROW the work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledProcedureGuidanceV1 {
    /// Canonical digest binding this guidance (KP-11
    /// `compiled_guidance_digest`).
    pub guidance_digest: String,
    /// Objective constraints the procedure asserts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objective_constraints: Vec<BoundedText>,
    /// Required checks (mechanical checks the run must satisfy).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_checks: Vec<BoundedText>,
    /// Artifact expectations projected into the task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_expectations: Vec<ArtifactExpectationV1>,
    /// Evaluation requirement the task carries.
    pub evaluation_requirement: EvaluationRequirement,
    /// Points where user input is expected (gate steps).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub user_input_points: Vec<u32>,
    /// Privacy constraints asserted over task content.
    pub privacy_constraints: BoundedText,
    /// The capability this procedure requires (narrow-only law: must be
    /// within the requester's own admitted set — a procedure can never
    /// ORIGINATE or WIDEN capability).
    pub required_capability: Capability,
}

/// The immutable procedure snapshot (KP-11 full field set). Constructed
/// only by the runtime mint; carries the COMPLETE captured definition
/// bytes (DECISION KP-16/E option (b): the bytes ride the submit
/// envelope; the retained Tachi-side ref is content-addressed and never
/// resolves through the mutable definitions directory).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcedureSnapshotV1 {
    /// Schema tag (`procedure-snapshot.v1`).
    pub schema: String,
    /// Procedure identity.
    pub procedure_id: String,
    /// The PUBLISHED definition revision the snapshot binds.
    pub procedure_revision: String,
    /// Canonical digest of the captured definition content.
    pub procedure_digest: String,
    /// The immutable definition content itself — the exact captured
    /// `SOP.toml` and `SOP.md` bytes (single-read capture; the snapshot
    /// digest binds these bytes).
    pub definition_toml: String,
    pub definition_md: String,
    /// The parsed step projection (derived from the captured bytes only).
    pub steps: Vec<ProcedureStepV1>,
    /// Digest of the compiled guidance embedded below.
    pub compiled_guidance_digest: String,
    /// Source SkillDefinition refs + revisions + digests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_skill_refs: Vec<SourceSkillRefV1>,
    /// Resolved constraints/checks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved_constraints: Vec<BoundedText>,
    /// Artifact/evidence expectations.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_evidence_expectations: Vec<ArtifactExpectationV1>,
    /// Evaluation contract revision/digest.
    pub evaluation_contract: EvaluationContractRef,
    /// Privacy class of the snapshot content.
    pub privacy_class: PrivacyClass,
    /// Provenance of the capture.
    pub provenance: DefinitionProvenance,
    /// Declared approval gates (host derives approval facts from the
    /// snapshot, never from the requester's projection).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approval_gates: Vec<ProcedureGateV1>,
    /// The compiled guidance (KP-17), digest-bound above.
    pub guidance: CompiledProcedureGuidanceV1,
}

impl ProcedureSnapshotV1 {
    /// Canonical content digest of the snapshot (sha256 hex over
    /// canonical JSON of the whole payload; same rule as the taskintent
    /// canonical digest).
    #[must_use]
    pub fn canonical_digest(&self) -> String {
        let value = serde_json::to_value(self).expect("snapshot serializes");
        canonical_json_digest_hex(&value)
    }

    /// The content-addressed snapshot ref (the DECISION KP-16/E retained
    /// ref shape: a CAS ref, never a filesystem path).
    #[must_use]
    pub fn snapshot_ref(&self) -> String {
        format!("{PROCEDURE_SNAPSHOT_REF_PREFIX}{}", self.canonical_digest())
    }

    /// Serialized size of the snapshot (for the bounded-carrier law).
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        serde_json::to_vec(self)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX)
    }
}

/// The KP-15 four-field run binding. A definition-side identity
/// projection: `(procedure_id, revision, digest, snapshot_ref)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcedureRunBinding {
    /// Procedure identity.
    pub procedure_id: String,
    /// The definition revision the run binds.
    pub revision: String,
    /// The definition content digest the run binds.
    pub digest: String,
    /// The Tachi-retained content-addressed snapshot ref.
    pub snapshot_ref: String,
}

impl ProcedureRunBinding {
    /// Bind a run to a snapshot (the only construction path).
    #[must_use]
    pub fn from_snapshot(snapshot: &ProcedureSnapshotV1) -> Self {
        Self {
            procedure_id: snapshot.procedure_id.clone(),
            revision: snapshot.procedure_revision.clone(),
            digest: snapshot.procedure_digest.clone(),
            snapshot_ref: snapshot.snapshot_ref(),
        }
    }
}

/// Target kind of a learning candidate (KP-18 field set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningTargetKind {
    /// A SkillDefinition revision.
    SkillRevision,
    /// A ProcedureDefinition revision.
    ProcedureRevision,
    /// A supervisor policy change.
    SupervisorPolicyCandidate,
    /// An AgentSoul craft change.
    AgentSoulCraftCandidate,
    /// A User Model change.
    UserModelCandidate,
}

/// Policy disposition of a candidate (how the review queue should treat
/// it before any human decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidatePolicyDisposition {
    /// Default: queue for human review.
    ReviewQueued,
    /// Quarantined (derivation or confidence warrants isolation).
    Quarantined,
}

/// A run/evidence-derived improvement candidate (KP-18/KP-19).
/// Candidate-only: NO apply path exists for any target kind — promotion
/// is a separate, named, HUMAN/operator-gated action. `derivation_ref`
/// is MANDATORY when the candidate's source material is sensitive
/// (personal-memory-adjacent inputs, Private-Dyad-derived analysis,
/// personal data) — keyed on derivation provenance, not target kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LearningCandidateV1 {
    /// Candidate identity.
    pub candidate_id: String,
    /// What the candidate targets.
    pub target_kind: LearningTargetKind,
    /// Task refs whose execution produced the candidate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_task_refs: Vec<String>,
    /// Evidence refs backing the candidate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
    /// Mandatory for sensitive derivations (KP-19).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derivation_ref: Option<String>,
    /// Digest of the proposed patch — the patch body itself is never
    /// carried as authority-bearing text on this type; the review path
    /// resolves it from the evidence store.
    pub proposed_patch_digest: String,
    /// Calibrated confidence in [0, 1].
    pub confidence: f64,
    /// Stated uncertainty (bounded prose).
    pub uncertainty: BoundedText,
    /// Policy disposition before review.
    pub policy_disposition: CandidatePolicyDisposition,
}

impl LearningCandidateV1 {
    /// Project into the V3 `ProposedCandidate` reviewed-promotion
    /// routing (single review surface — no second queue): the candidate
    /// lands in the SAME reviewed promotion path supervisor-session
    /// candidates use, and no apply action exists on either shape.
    #[must_use]
    pub fn to_proposed_candidate(&self) -> ProposedCandidate {
        let kind = match self.target_kind {
            LearningTargetKind::SkillRevision => ProposedCandidateKind::Skill,
            LearningTargetKind::ProcedureRevision => ProposedCandidateKind::Procedure,
            LearningTargetKind::UserModelCandidate => ProposedCandidateKind::UserModel,
            LearningTargetKind::AgentSoulCraftCandidate => ProposedCandidateKind::AgentSoul,
            // No supervisor-policy kind exists on the V3 surface yet; the
            // closest reviewed-promotion target is AgentSoul-class
            // authority review. Mapped explicitly and fail-closed to the
            // reviewed path (never to an apply path).
            LearningTargetKind::SupervisorPolicyCandidate => ProposedCandidateKind::AgentSoul,
        };
        ProposedCandidate {
            candidate_id: self.candidate_id.clone(),
            kind,
            content_digest: self.proposed_patch_digest.clone(),
            // Where the review path resolves the payload: the sensitive
            // derivation ref when present, else the first evidence/task
            // pointer the candidate carries. Never empty for
            // active-authority kinds (P2 caveat law).
            payload_ref: self
                .derivation_ref
                .clone()
                .or_else(|| self.evidence_refs.first().cloned())
                .or_else(|| self.source_task_refs.first().cloned()),
            provenance: Some(CandidateProvenance {
                source_task_refs: self.source_task_refs.clone(),
                evidence_refs: self.evidence_refs.clone(),
                derivation: format!(
                    "learning-candidate-v1 target={:?} confidence={:.3} uncertainty={}",
                    self.target_kind,
                    self.confidence,
                    self.uncertainty.as_str()
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_fixture() -> ProcedureSnapshotV1 {
        use crate::taskintent::{IndependenceClass, PrivacyClass};
        ProcedureSnapshotV1 {
            schema: PROCEDURE_SNAPSHOT_SCHEMA.to_string(),
            procedure_id: "stagex-update".to_string(),
            procedure_revision: "1.0.0".to_string(),
            procedure_digest: "a".repeat(64),
            definition_toml: "[sop]\nname = \"stagex-update\"\n".to_string(),
            definition_md: "## Steps\n".to_string(),
            steps: vec![ProcedureStepV1 {
                number: 1,
                title: "Resolve".to_string(),
                body: BoundedText::new("Map the upstream project.").unwrap(),
                suggested_tools: vec!["shell".to_string(), "file_read".to_string()],
                requires_confirmation: false,
                kind: "execute".to_string(),
            }],
            compiled_guidance_digest: "b".repeat(64),
            source_skill_refs: vec![],
            resolved_constraints: vec![],
            artifact_evidence_expectations: vec![],
            evaluation_contract: EvaluationContractRef {
                revision: "procedure-eval.v1".to_string(),
                digest: "c".repeat(64),
            },
            privacy_class: PrivacyClass::Public,
            provenance: DefinitionProvenance {
                authored_via: "zeroclaw-sops-dir".to_string(),
                captured_at: "2026-08-26T00:00:00Z".to_string(),
            },
            approval_gates: vec![],
            guidance: CompiledProcedureGuidanceV1 {
                guidance_digest: "b".repeat(64),
                objective_constraints: vec![],
                required_checks: vec![],
                artifact_expectations: vec![],
                evaluation_requirement: EvaluationRequirement {
                    independence: IndependenceClass::DeterministicCheck,
                },
                user_input_points: vec![],
                privacy_constraints: BoundedText::new("public-safe").unwrap(),
                required_capability: Capability::RepositoryImplementation,
            },
        }
    }

    #[test]
    fn snapshot_digest_is_content_addressed_and_stable() {
        let snapshot = snapshot_fixture();
        let first = snapshot.canonical_digest();
        // Digest stability: same content, same digest — deterministically
        // re-derivable (restart/re-admission yields the same content).
        assert_eq!(snapshot.canonical_digest(), first);
        let mut mutated = snapshot.clone();
        mutated.definition_md = "## Steps\n1. changed".to_string();
        assert_ne!(mutated.canonical_digest(), first);
        // The snapshot ref is a CAS ref, never a filesystem path.
        let reference = snapshot.snapshot_ref();
        assert!(reference.starts_with(PROCEDURE_SNAPSHOT_REF_PREFIX));
        assert!(!reference.contains('/') && !reference.starts_with('/'));
    }

    #[test]
    fn run_binding_carries_the_four_field_tuple() {
        let snapshot = snapshot_fixture();
        let binding = ProcedureRunBinding::from_snapshot(&snapshot);
        assert_eq!(binding.procedure_id, "stagex-update");
        assert_eq!(binding.revision, "1.0.0");
        assert_eq!(binding.digest, snapshot.procedure_digest);
        assert_eq!(binding.snapshot_ref, snapshot.snapshot_ref());
        // KP-15: the definition revision is the manifest version, never a
        // run-state CAS counter — structurally, the binding only carries
        // what the snapshot minted.
    }

    #[test]
    fn learning_candidate_maps_into_the_reviewed_promotion_path() {
        let candidate = LearningCandidateV1 {
            candidate_id: "cand-1".to_string(),
            target_kind: LearningTargetKind::ProcedureRevision,
            source_task_refs: vec!["task:e2e".to_string()],
            evidence_refs: vec!["evidence://run-1".to_string()],
            derivation_ref: Some("evidence://derivation".to_string()),
            proposed_patch_digest: "d".repeat(64),
            confidence: 0.7,
            uncertainty: BoundedText::new("single-run evidence").unwrap(),
            policy_disposition: CandidatePolicyDisposition::ReviewQueued,
        };
        let proposed = candidate.to_proposed_candidate();
        assert!(proposed.kind.requires_reviewed_promotion());
        assert!(proposed.is_substantiated());
    }

    #[test]
    fn wire_shapes_are_frozen_deny_unknown_fields() {
        // A smuggled extra field fails decode on every frozen shape.
        let snapshot = snapshot_fixture();
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["smuggled"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ProcedureSnapshotV1>(value).is_err());
    }
}
