//! Snapshot mint (KP-11): bind an immutable, content-addressed
//! `ProcedureSnapshotV1` from a PUBLISHED captured revision.
//!
//! Publication rules encoded here:
//!
//! 1. **Draft-revision mint refused**: the capture's review state must
//!    be `Published` — a typed `DraftRevision` refusal otherwise.
//! 2. **Complete-definition atomicity**: the snapshot embeds the exact
//!    captured `SOP.toml`/`SOP.md` bytes; the parsed steps derive from
//!    those bytes only (single-read capture in
//!    [`super::definition::capture_definition`]; this module never
//!    re-reads the tree — no mixed/partial snapshot can exist).
//! 3. **Race-free mint**: the snapshot's canonical digest binds the
//!    whole payload; re-deriving the digest over the same snapshot
//!    yields the same value, and any later edit of the definitions tree
//!    changes nothing already minted (KP-12 — proven by tests).
//!
//! Admission scans over the snapshot content (fail closed):
//!
//! - credential-shaped content refused (a definition is not a secret
//!   store);
//! - Private-Dyad-markered content refused with an EXISTENCE-BLIND
//!   error (KP-20/RULING-207 §11: content, ids, and counts of Private
//!   Dyad must not enter Tachi — the refusal names the category, never
//!   the material);
//! - worktree/filesystem-path-shaped content refused (definitions are
//!   machine-independent; a path in a definition is placement detail);
//! - oversize content refused by the bounded-carrier law (typed
//!   `Oversize`, never silent truncation).

use zeroclaw_api::procedure_v1::{
    CompiledProcedureGuidanceV1, DefinitionReviewState, PROCEDURE_SNAPSHOT_MAX_BYTES,
    ProcedureSnapshotV1,
};
use zeroclaw_api::taskintent::{
    BoundedText, Capability, EvaluationRequirement, IndependenceClass, PrivacyClass,
    canonical_json_digest_hex,
};

use super::definition::{CapturedDefinition, project_definition};

/// Typed mint refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapshotMintError {
    /// KP-11 rule 1: only a PUBLISHED revision may be minted.
    #[error(
        "snapshot mint refused: definition revision is a draft (publication required before run creation)"
    )]
    DraftRevision,
    /// Forbidden content in the captured definition (category only —
    /// existence-blind for the PrivateDyad class).
    #[error("snapshot mint refused: forbidden content category `{category}` in the definition")]
    ForbiddenContent {
        /// Which category matched (never carries the offending bytes).
        category: SnapshotContentCategory,
    },
    /// Bounded-carrier law: oversize snapshots are a typed refusal.
    #[error("snapshot mint refused: serialized snapshot is {len} bytes, over the {max} byte bound")]
    Oversize {
        /// Actual serialized length.
        len: usize,
        /// The frozen maximum.
        max: usize,
    },
}

/// Content-scan categories admitted at mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotContentCategory {
    /// Credential-shaped material in the definition.
    Credential,
    /// Private-Dyad-markered material (existence-blind in errors).
    PrivateDyad,
    /// Worktree/filesystem-path-shaped material.
    WorktreePath,
}

impl std::fmt::Display for SnapshotContentCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Credential => write!(f, "credential"),
            Self::PrivateDyad => write!(f, "private_dyad"),
            Self::WorktreePath => write!(f, "worktree_path"),
        }
    }
}

const CREDENTIAL_MARKERS: &[&str] = &[
    "-----BEGIN OPENSSH PRIVATE KEY",
    "-----BEGIN RSA PRIVATE KEY",
    "-----BEGIN PRIVATE KEY",
    "-----BEGIN EC PRIVATE KEY",
    "sk-ant-",
    "sk-proj-",
    "ghp_",
    "github_pat_",
    "gho_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "api_key=",
    "apikey:",
    "password=",
    "bearer ",
];

const PRIVATE_DYAD_MARKERS: &[&str] = &["private dyad", "private_dyad", "private-dyad"];

const WORKTREE_PATH_MARKERS: &[&str] =
    &["/worktrees/", "/Users/", "/home/", "/tmp/", "/var/folders/"];

/// Scan captured definition content for mint-refusal categories. Shared
/// by the mint and by tests proving the privacy law.
pub fn snapshot_content_scan(toml_bytes: &str, md_bytes: &str) -> Result<(), SnapshotMintError> {
    for content in [toml_bytes, md_bytes] {
        let lower = content.to_ascii_lowercase();
        for marker in CREDENTIAL_MARKERS {
            if content.contains(marker) || lower.contains(&marker.to_ascii_lowercase()) {
                return Err(SnapshotMintError::ForbiddenContent {
                    category: SnapshotContentCategory::Credential,
                });
            }
        }
        for marker in PRIVATE_DYAD_MARKERS {
            if lower.contains(marker) {
                // Existence-blind: the category is named, the material
                // is never echoed (KP-20).
                return Err(SnapshotMintError::ForbiddenContent {
                    category: SnapshotContentCategory::PrivateDyad,
                });
            }
        }
        if lower.starts_with('/') || lower.starts_with('~') || lower.starts_with("./") {
            return Err(SnapshotMintError::ForbiddenContent {
                category: SnapshotContentCategory::WorktreePath,
            });
        }
        for marker in WORKTREE_PATH_MARKERS {
            // Markers compare against the LOWERCASED content — match the
            // marker's own lowercase form so mid-string `/Users/` paths
            // cannot evade by case.
            if lower.contains(&marker.to_ascii_lowercase()) {
                return Err(SnapshotMintError::ForbiddenContent {
                    category: SnapshotContentCategory::WorktreePath,
                });
            }
        }
    }
    Ok(())
}

/// Tool-class table for the narrow-only capability law (KP-17): the
/// capability a procedure REQUIRES given EVERY tool-bearing surface a
/// step declares — `suggested_tools`, the typed `scope.allow`, planned
/// `calls`, and capability-step ids. Unknown tool names conservatively
/// require the strongest capability — the failure direction of the
/// narrow-only law is refusal, never silent downgrade.
/// The capability a step list requires (crate-public: the submit-side
/// invariant verifier re-derives it from the pinned bytes).
pub(crate) fn required_capability_of(steps: &[crate::sop::types::SopStep]) -> Capability {
    required_capability_for_tools(steps)
}

fn required_capability_for_tools(steps: &[crate::sop::types::SopStep]) -> Capability {
    use crate::sop::types::SopStepKind;
    let class_of = |tool: &str| -> u8 {
        let lower = tool.to_ascii_lowercase();
        match lower.as_str() {
            // Read-only surface: the read-only investigation capability.
            "file_read" | "memory_recall" | "read_skill" => 2,
            // Side-effecting surface: the implementation capability.
            "shell" | "file_write" | "git_operations" | "http_request" | "pushover"
            | "memory_store" => 3,
            // Unknown: strongest requirement (fail closed).
            _ => 3,
        }
    };
    let mut max = 1u8;
    for step in steps {
        if matches!(step.kind, SopStepKind::Checkpoint) {
            // Gates are adjudication, not execution breadth.
            continue;
        }
        // The merged authoritative tool surface of the step.
        let mut tools: Vec<String> = Vec::new();
        if let Some(allow) = step.effective_tool_scope().and_then(|scope| scope.allow) {
            tools.extend(allow);
        }
        for call in &step.calls {
            tools.push(call.tool.clone());
        }
        if let Some(capability) = step.capability_id() {
            tools.push(capability.to_string());
        }
        for tool in &tools {
            max = max.max(class_of(tool));
        }
        // A capability step EXECUTES real capability code by definition.
        if step.kind == SopStepKind::Capability {
            max = max.max(3);
        }
    }
    match max {
        3 => Capability::RepositoryImplementation,
        2 => Capability::ReadOnlyInvestigation,
        _ => Capability::ReasoningReview,
    }
}

/// Mint the immutable snapshot from a captured revision (KP-11 full
/// field set). Pure over the capture: no I/O, no durable writes.
pub fn mint_snapshot(
    captured: &CapturedDefinition,
) -> Result<ProcedureSnapshotV1, SnapshotMintError> {
    // Publication truth is the RAW captured bytes, never the (mutable)
    // struct field — flipping `CapturedDefinition.review_state` after
    // capture cannot publish a draft.
    let raw_review_state = super::definition::review_state_of(&captured.toml_bytes)
        .map_err(|_| SnapshotMintError::DraftRevision)?;
    if raw_review_state != DefinitionReviewState::Published {
        return Err(SnapshotMintError::DraftRevision);
    }
    // Bounded-text law: an oversize step body is a typed refusal, never
    // a silently-emptied projection.
    for step in &captured.steps {
        if BoundedText::new(step.body.clone()).is_err() {
            return Err(SnapshotMintError::Oversize {
                len: step.body.len(),
                max: zeroclaw_api::taskintent::BOUNDED_TEXT_MAX,
            });
        }
    }
    snapshot_content_scan(&captured.toml_bytes, &captured.md_bytes)?;

    let definition = project_definition(captured);
    let guidance = CompiledProcedureGuidanceV1 {
        guidance_digest: String::new(), // bound below
        objective_constraints: definition.constraints.clone(),
        required_checks: vec![BoundedText::new(format!(
            "All {} steps of procedure {} complete per the pinned snapshot",
            definition.steps.len(),
            definition.procedure_id
        ))
        .expect("bounded check text")],
        artifact_expectations: definition.expected_artifacts.clone(),
        evaluation_requirement: EvaluationRequirement {
            independence: IndependenceClass::DeterministicCheck,
        },
        user_input_points: definition
            .approval_gates
            .iter()
            .map(|gate| gate.step)
            .collect(),
        privacy_constraints: BoundedText::new(format!(
            "privacy_class={:?}: snapshot and guidance carry only privacy-admitted projections; Private Dyad is structurally absent",
            definition.privacy_class
        ))
        .expect("bounded privacy text"),
        required_capability: required_capability_for_tools(&captured.steps),
    };

    let mut snapshot = ProcedureSnapshotV1 {
        schema: zeroclaw_api::procedure_v1::PROCEDURE_SNAPSHOT_SCHEMA.to_string(),
        procedure_id: definition.procedure_id.clone(),
        procedure_revision: definition.revision.clone(),
        procedure_digest: definition.digest.clone(),
        definition_toml: captured.toml_bytes.clone(),
        definition_md: captured.md_bytes.clone(),
        steps: definition.steps.clone(),
        compiled_guidance_digest: String::new(),
        source_skill_refs: Vec::new(),
        resolved_constraints: definition.constraints.clone(),
        artifact_evidence_expectations: definition.expected_artifacts.clone(),
        evaluation_contract: definition.evaluation_contract.clone(),
        privacy_class: PrivacyClass::Public,
        provenance: definition.provenance.clone(),
        approval_gates: definition.approval_gates.clone(),
        guidance,
    };
    // Digest-bound guidance (two-level binding — the snapshot digest
    // covers the guidance digest; KP-11 `compiled_guidance_digest`).
    // The digest covers the guidance payload with the digest field
    // itself normalized to empty, so mint and independent verifiers
    // re-derive the SAME value ([`guidance_payload_digest`]).
    snapshot.guidance.guidance_digest = guidance_payload_digest(&snapshot.guidance);
    snapshot.compiled_guidance_digest = snapshot.guidance.guidance_digest.clone();

    let serialized = snapshot.serialized_len();
    if serialized > PROCEDURE_SNAPSHOT_MAX_BYTES {
        return Err(SnapshotMintError::Oversize {
            len: serialized,
            max: PROCEDURE_SNAPSHOT_MAX_BYTES,
        });
    }
    Ok(snapshot)
}

/// The canonical digest of a guidance payload with its own
/// `guidance_digest` field normalized to empty — the single binding
/// rule shared by the mint and the submit-side invariant verifier.
pub(crate) fn guidance_payload_digest(guidance: &CompiledProcedureGuidanceV1) -> String {
    let mut normalized = guidance.clone();
    normalized.guidance_digest = String::new();
    let value = serde_json::to_value(&normalized).expect("guidance serializes");
    canonical_json_digest_hex(&value)
}
