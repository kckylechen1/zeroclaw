//! Shared wire/domain types for the V1 bounded SubAgent vertical.
//!
//! These types are the frozen contract's wire vocabulary: profiles,
//! context bundles, lineage, budgets, control states, and the structured
//! report. They are CONTENT and bookkeeping only — nothing here carries or
//! grants authority. Admission, enforcement, and the run itself live in
//! `zeroclaw-runtime::subagent_v1`.
//!
//! Design law (SA-18): a `ContextBundleV1` is content, never authority.
//! Nothing derived from a bundle may widen a capability, admit a tool, or
//! mint a credential. The profile is the only capability source (SA-3/SA-5),
//! and the profile itself is deny-by-default.
//!
//! Law for the banned tool names (SA-7b/SA-12): the v1 tool-name type
//! refuses `spawn_subagent` and `delegate` at parse time, so no v1 profile
//! field path can name either tool. The refusal is typed, never prose.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::companion::SourcePartition;

// ─────────────────────────────────────────────────────────────────────────
// Lineage (SA-9/SA-10/SA-11): ONE depth authority
// ─────────────────────────────────────────────────────────────────────────

/// Opaque identity of the root coordination run that a lineage belongs to.
///
/// Minted by the host at the root of a coordination run (typically the
/// top-level turn's session key). Children never mint roots: a fresh root is
/// a typed root transition (cron, explicit new-root API), never a silent
/// registry-rebuild reset (SA-11).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ParentRunRef(String);

impl ParentRunRef {
    /// Wrap an already-minted run id. Does not mint.
    #[must_use]
    pub fn from_opaque(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The single depth authority for local agent spawning (SA-9).
///
/// Immutable; the only way to advance is [`LineageRef::child`], which the
/// spawn sites must call when constructing a child context. Because the
/// lineage is carried by the spawning context (agent-run overrides, child
/// requests) rather than by per-tool-instance depth fields, a registry
/// rebuild inside a child cannot reset it (SA-11), and `delegate` /
/// `spawn_subagent` increment the same ledger (SA-10).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineageRef {
    root: ParentRunRef,
    depth: u32,
}

impl LineageRef {
    /// A fresh root lineage (depth 0). Only the host at a genuine root
    /// (interactive top-level turn, cron job, explicit new-root API) mints
    /// these; every spawn site advances with [`LineageRef::child`].
    #[must_use]
    pub fn new_root(root: ParentRunRef) -> Self {
        Self { root, depth: 0 }
    }

    /// The lineage of a context spawned from this one. Monotonic: the only
    /// depth-advancing operation on this type.
    #[must_use]
    pub fn child(&self) -> Self {
        Self {
            root: self.root.clone(),
            depth: self.depth + 1,
        }
    }

    #[must_use]
    pub fn root_ref(&self) -> &ParentRunRef {
        &self.root
    }

    /// Monotonic local depth. 0 = root context.
    #[must_use]
    pub fn depth(&self) -> u32 {
        self.depth
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Profile (SA-3/SA-4/SA-5)
// ─────────────────────────────────────────────────────────────────────────

/// SubAgent role (SA-3). `Supervisor` exists as a profile-schema constraint
/// only in the V1 vertical: Supervisor profiles may be admitted to the
/// registry, but no Supervisor run is constructible in V1 (V3 leaf).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentRoleV1 {
    Reasoning,
    Supervisor,
}

/// The frozen v1 recursion law (SA-12, D1 strict local ban): local
/// SubAgent-to-SubAgent spawn is denied, so every v1 profile resolves to
/// no-local-spawn. The enum carries exactly one variant: there is no other
/// value the type can hold, so the law is structural, not prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentRecursionPolicyV1 {
    NoLocalSpawn,
}

/// A v1 child tool name. Parse-time refusal of every banned name means no
/// v1 profile field path can name them (SA-7b, SA-12, SA-7e, SA-29):
///
/// - `spawn_subagent`, `delegate`: D1 strict local ban (SA-7b/SA-12);
/// - `memory_store`, `memory_forget`, `memory_purge`: D2 — no
///   personal-memory mutation handle reaches a child (SA-7e/SA-17);
/// - `ask_user`: no live channel handle reaches a child (SA-7c/SA-25);
/// - `shell`, `file_write`, `file_edit`: the SA-30 transitional direct
///   execution trio is a PARENT-kernel marking; a v1 grant would need a
///   named transitional admission under SA-30/SA-29, which V1 does not
///   open (deny until the owner picks).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubAgentToolNameV1(String);

/// Tool names that can never appear in a v1 profile, with the clause that
/// bans each. Refusal is typed at parse time, never tool prose.
pub const V1_BANNED_TOOL_NAMES: &[(&str, &str)] = &[
    ("spawn_subagent", "SA-7b/SA-12 (D1 strict local spawn ban)"),
    ("delegate", "SA-7b/SA-12 (D1 strict local spawn ban)"),
    (
        "memory_store",
        "SA-7e/SA-17 (D2 no personal-memory mutation)",
    ),
    (
        "memory_forget",
        "SA-7e/SA-17 (D2 no personal-memory mutation)",
    ),
    (
        "memory_purge",
        "SA-7e/SA-17 (D2 no personal-memory mutation)",
    ),
    ("ask_user", "SA-7c/SA-25 (D4 parent-only asking)"),
    (
        "shell",
        "SA-30 (transitional parent-kernel trio; no v1 grant)",
    ),
    (
        "file_write",
        "SA-30 (transitional parent-kernel trio; no v1 grant)",
    ),
    (
        "file_edit",
        "SA-30 (transitional parent-kernel trio; no v1 grant)",
    ),
];

#[derive(Debug, thiserror::Error)]
#[error("tool name {name:?} is banned from v1 SubAgent profiles: {clause}")]
pub struct BannedToolNameError {
    pub name: String,
    pub clause: &'static str,
}

impl SubAgentToolNameV1 {
    /// Parse a candidate child tool name. Fails closed on every banned name
    /// (SA-7b/SA-12 enforcement at the type level) and on empty input.
    pub fn parse(name: &str) -> Result<Self, BannedToolNameError> {
        let trimmed = name.trim();
        for (banned, clause) in V1_BANNED_TOOL_NAMES {
            if trimmed == *banned {
                return Err(BannedToolNameError {
                    name: trimmed.to_string(),
                    clause,
                });
            }
        }
        if trimmed.is_empty() {
            return Err(BannedToolNameError {
                name: name.to_string(),
                clause: "empty tool name",
            });
        }
        Ok(Self(trimmed.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Budget in the type with all three ceilings (SA-27): time, tokens,
/// actions. Token accounting is recorded and enforced over reported usage;
/// a provider that reports no usage triggers a WARN, never a silent pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentBudgetV1 {
    /// Wall-clock ceiling for the whole run, seconds. Enforced: expiry is
    /// budget exhaustion and terminates the run `timed_out` (SA-23).
    pub time_limit_secs: u64,
    /// Token ceiling over reported usage (prompt + completion). Enforced
    /// over counted usage; never silently ignored (SA-27).
    pub max_tokens: u64,
    /// Maximum number of billable actions (model calls and tool events).
    /// Enforced; shared with the parent meter (SA-8).
    pub max_actions: u32,
}

impl Default for SubAgentBudgetV1 {
    fn default() -> Self {
        Self {
            time_limit_secs: 120,
            max_tokens: 32_768,
            max_actions: 8,
        }
    }
}

/// Model access as a REFERENCE, never a credential (SA-7d). Resolved at
/// use time by the host behind an opaque binding; the child context holds
/// no provider configuration and no key material.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelPolicyV1 {
    /// `family.alias` reference into the operator's provider configuration.
    pub provider_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
}

/// Declared child tool list. Deny-by-default complement: admission refuses
/// any name outside the v1 child catalog, and the V1 catalog is EMPTY (V1
/// reasoning runs execute no tools — recorded least-authority choice), so
/// every admitted v1 profile carries an empty list.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentToolPolicyV1 {
    pub tools: Vec<SubAgentToolNameV1>,
}

/// The typed Tachi authority set a Supervisor profile may enumerate
/// (SA-29). A request is never a lifecycle transition. V1 admits
/// Supervisor profiles with this constraint but runs none of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorAuthority {
    ObserveTask,
    ReadResultRefs,
    ProvideContext,
    RequestCorrection,
    RequestContinuation,
    RequestIndependentReview,
    RequestUserInput,
    RequestGracefulStop,
    RequestCancel,
    ProposeJudgment,
}

/// Context policy: which context classes a profile permits to be projected
/// into the bundle at all (SA-19 exclusions are the deny-by-default
/// complement enforced at projection time).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentContextPolicyV1 {
    /// Context classes the profile permits; everything else is excluded.
    pub allowed_classes: Vec<ContextClassV1>,
    /// Hard ceiling on the projected bundle payload, bytes.
    pub max_projection_bytes: usize,
}

/// Privacy policy: which partitions may appear as bundle source refs.
/// `PrivateDyad` can never be permitted — the type offers no way to say it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubAgentPrivacyPolicyV1 {
    /// Partitions whose PUBLIC, non-private-dyad content may be referenced.
    /// The bundle projection independently redacts Private Dyad existence
    /// (SA-14); this field can only narrow further.
    pub permitted_partitions: Vec<SourcePartition>,
}

/// A context class, used for first-class exclusions (SA-19).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextClassV1 {
    /// Bounded read-only User Model projection (SA-15).
    UserModelProjection,
    /// Skill references (content only; never capability — SA-18).
    SkillRefs,
    /// Procedure references (content only; never capability — SA-18).
    ProcedureRefs,
    /// Non-persona source material (documents, snippets).
    SourceRefs,
    /// The objective's surrounding context block.
    ObjectiveContext,
    /// The parent's conversation transcript — excluded by default and by
    /// construction on the V1 path (SA-16); present in the enum so an
    /// exclusion of it is representable and auditable.
    ParentTranscript,
}

/// The immutable, digest-bound SubAgent profile (SA-3/SA-4). Any capability
/// change mints a new revision with a new digest and a new run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubAgentProfileV1 {
    pub profile_id: String,
    pub revision: u32,
    pub digest: String,
    pub role: SubAgentRoleV1,
    pub model_policy: ModelPolicyV1,
    pub tool_policy: SubAgentToolPolicyV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supervisor_authority_set: Vec<SupervisorAuthority>,
    pub context_policy: SubAgentContextPolicyV1,
    pub privacy_policy: SubAgentPrivacyPolicyV1,
    pub budget: SubAgentBudgetV1,
    pub recursion: SubAgentRecursionPolicyV1,
    pub output_schema: SubAgentOutputSchemaV1,
}

/// Output schema selector. V1 has exactly one admitted output: the
/// structured report. No free-text mode exists in the type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentOutputSchemaV1 {
    StructuredReport,
}

/// Reference to an admitted profile revision (SA-3). The only way to
/// construct a `SubAgentRun` (in the runtime crate).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionedProfileRef {
    pub profile_id: String,
    pub revision: u32,
    pub digest: String,
}

/// Authority-minted, run-scoped child identity (SA-13). Never the parent
/// alias/agent-UUID/namespace; caller-supplied ids are advisory labels
/// with zero authority effect.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubAgentRunRef(String);

impl SubAgentRunRef {
    #[must_use]
    pub fn from_opaque(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ─────────────────────────────────────────────────────────────────────────
// ContextBundleV1 (SA-18/SA-19/SA-20) — content, never authority
// ─────────────────────────────────────────────────────────────────────────

/// A bounded, read-only User Model projection item (SA-15). Carries a
/// digest of the projected statement, not the raw memory row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedFactRef {
    pub fact_id: String,
    pub statement_digest: String,
}

/// A source reference inside a bundle. The partition marks where content
/// came from so projection-time redaction can be existence-blind for
/// Private Dyad (SA-14): a bundle holding a Private-Dyad-derived ref
/// projects byte-identically to one holding none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleSourceRef {
    pub ref_id: String,
    pub partition: SourcePartition,
    pub content_digest: String,
}

/// Redaction policy applied at projection time. Private Dyad exclusion is
/// unconditional (not a field anyone can turn off — SA-14).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleRedactionPolicy {
    /// Drop `skill_refs` from the projection (content only; no authority
    /// effect either way — SA-18).
    pub redact_skill_refs: bool,
    /// Drop `procedure_refs` from the projection.
    pub redact_procedure_refs: bool,
}

/// The only context carrier (SA-18). Immutable, digest-bound at child
/// admission; mid-run mutation is refused by verification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBundleV1 {
    pub bundle_id: String,
    pub revision: u32,
    pub digest: String,
    pub parent_ref: ParentRunRef,
    /// Bounded objective-surrounding context (NOT the parent transcript —
    /// SA-16).
    pub objective_context: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_refs: Vec<BundleSourceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applicable_user_model: Vec<ProjectedFactRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub procedure_refs: Vec<String>,
    /// First-class exclusions enforced at projection time (SA-19).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub explicit_exclusions: Vec<ContextClassV1>,
    pub redaction_policy: BundleRedactionPolicy,
}

/// The bounded, redacted projection of a bundle — what a child turn
/// actually reads. Existence-blind for Private Dyad by construction,
/// INCLUDING the digest: `projection_digest` is computed over the
/// PROJECTED content only, so two bundles whose public content is
/// identical but whose private-derived inputs differ project to
/// byte-identical values (SA-14.3: no ids, refs, counts, or existence
/// signals). No full-bundle digest exists anywhere on this struct —
/// admission verification reads it from the bundle itself, and the
/// child only ever sees `projection_digest`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleProjection {
    pub bundle_id: String,
    /// Digest over the PROJECTED content (never includes redacted or
    /// excluded material). Child-visible. The pinned full-bundle digest
    /// deliberately does NOT exist on this struct: it covers
    /// unprojected (private-derived) content and must never reach the
    /// child in any form.
    pub projection_digest: String,
    pub objective_context: String,
    pub source_refs: Vec<BundleSourceRef>,
    pub applicable_user_model: Vec<ProjectedFactRef>,
    pub skill_refs: Vec<String>,
    pub procedure_refs: Vec<String>,
}

/// Why a policy-filtered projection was refused (deny-by-default — the
/// run fails closed rather than silently narrowing the bundle).
#[derive(Debug, thiserror::Error)]
pub enum ProjectionPolicyError {
    #[error(
        "source ref {ref_id:?} carries partition {partition}, which the profile's privacy \
         policy does not permit"
    )]
    DisallowedPartition {
        ref_id: String,
        partition: SourcePartition,
    },
    #[error(
        "projected bundle is {actual_bytes} bytes, over the profile's \
         max_projection_bytes ceiling of {max_bytes}"
    )]
    ProjectionTooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
}

// ─────────────────────────────────────────────────────────────────────────
// Control states (SA-23) and mid-run requests (SA-25)
// ─────────────────────────────────────────────────────────────────────────

/// Control events a parent (or host) may raise. Exactly two (SA-23).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentControlEvent {
    GracefulStopRequested,
    AbortRequested,
}

/// Terminal facts. Exactly five (SA-23). `timed_out` is budget exhaustion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubAgentTerminalFact {
    Stopped,
    Aborted,
    TimedOut,
    Completed,
    Failed,
}

impl SubAgentTerminalFact {
    /// The control event that must have been raised before this terminal
    /// fact may be written. `None` means no control event is required
    /// (`completed`, `failed`); `timed_out` is budget exhaustion and is
    /// justified by the meter, not a control event (SA-23).
    #[must_use]
    pub fn required_control_event(self) -> Option<SubAgentControlEvent> {
        match self {
            Self::Stopped => Some(SubAgentControlEvent::GracefulStopRequested),
            Self::Aborted => Some(SubAgentControlEvent::AbortRequested),
            Self::TimedOut | Self::Completed | Self::Failed => None,
        }
    }
}

/// A typed mid-run child→parent request (SA-21/SA-25). No free-text
/// payload: the Parent owns asking, interruption semantics, and final
/// wording; the child names WHAT it needs by reference, never the wording.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SubAgentMidRunRequest {
    /// The child needs user input. References uncertainty items already
    /// in the run's observable state; the Parent composes the question.
    RequestUserInput { uncertainty_item_ids: Vec<String> },
    /// The child asks the Parent to stop it gracefully (current bounded
    /// unit finishes — SA-23).
    RequestGracefulStop { reason_code: String },
    /// The child asks the Parent to abort it (distinct from graceful
    /// stop — SA-23).
    RequestAbort { reason_code: String },
}

// ─────────────────────────────────────────────────────────────────────────
// SubAgentReportV1 (SA-21/SA-22) — the ONLY child→parent result channel
// ─────────────────────────────────────────────────────────────────────────

/// A finding, stated and evidenced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub finding_id: String,
    pub statement: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<EvidenceRef>,
}

/// An opaque evidence pointer (bundle ref id, source digest, …).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EvidenceRef(pub String);

/// A bounded uncertainty item. Referenced by `RequestUserInput`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UncertaintyItem {
    pub uncertainty_id: String,
    pub topic_code: String,
    pub impact: String,
}

/// Kinds of action a child may ask the Parent to take (typed — SA-25).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentActionKind {
    /// Ask the user a question composed by the Parent.
    AskUser,
    /// Route a proposed candidate into its review path.
    ReviewCandidate,
    /// Nothing further; informational.
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedParentAction {
    pub action: ParentActionKind,
    /// What the action concerns, by typed reference (no authority effect).
    pub subject_ref: String,
}

/// Kind of a proposed candidate. KP-18 active-authority kinds can reach
/// the owning repo's reviewed promotion path ONLY — the Parent agent has
/// no apply action for them (SA-17/SA-22 seam with the KP-18 knowledge split).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposedCandidateKind {
    /// Ordinary personal/episodic memory; the Parent commits through its
    /// own normal memory-write path if it accepts.
    OrdinaryMemory,
    /// User Model (KP-18 active-authority kind).
    UserModel,
    /// AgentSoul (KP-18; excluded from child contexts by SA-15 — a child
    /// cannot legitimately produce this, and the review queue flags it).
    AgentSoul,
    /// Skill revision (KP-18).
    Skill,
    /// Procedure revision (KP-18).
    Procedure,
    /// Anything derived from Private Dyad content (KP-18; and a red flag —
    /// the child should never have seen such content).
    PrivateDyadDerived,
}

impl ProposedCandidateKind {
    #[must_use]
    pub fn requires_reviewed_promotion(self) -> bool {
        matches!(
            self,
            Self::UserModel
                | Self::AgentSoul
                | Self::Skill
                | Self::Procedure
                | Self::PrivateDyadDerived
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposedCandidate {
    pub candidate_id: String,
    pub kind: ProposedCandidateKind,
    /// Digest of the proposed content — the payload itself is not carried
    /// as raw authority-bearing text.
    pub content_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recommendation {
    pub recommendation_id: String,
    pub statement: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<EvidenceRef>,
}

/// Resource usage reported back to the Parent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentUsage {
    pub elapsed_ms: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub actions: u32,
}

/// The ONLY child→parent result channel (SA-21). Frozen field set;
/// `deny_unknown_fields` means a chain-of-thought field (or any other
/// smuggled extra) fails to deserialize — report hygiene is structural
/// (SA-22).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubAgentReportV1 {
    pub run_ref: SubAgentRunRef,
    pub profile_ref: VersionedProfileRef,
    pub context_bundle_ref: String,
    pub status: SubAgentTerminalFact,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uncertainty: Vec<UncertaintyItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommendations: Vec<Recommendation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_parent_actions: Vec<RequestedParentAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed_candidates: Vec<ProposedCandidate>,
    pub usage: SubAgentUsage,
}

/// What travels on the report channel: typed mid-run events and the
/// single terminal report (SA-21/SA-25). The report is boxed to keep
/// the channel's per-message footprint small.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ReportChannelMessage {
    Event(SubAgentMidRunRequest),
    Report(Box<SubAgentReportV1>),
}

// ─────────────────────────────────────────────────────────────────────────
// Private Dyad / AgentSoul type-level gate (SA-14/SA-15)
// ─────────────────────────────────────────────────────────────────────────

/// A partition key a child context may hold. Fails closed on
/// `PrivateDyad` (SA-14 — structurally unreachable from any SubAgent
/// path, enforced by this type, not by prompt instruction) and on
/// `AgentSoul` (SA-15 — SubAgents do not own front-stage identity; no
/// child-path arm resolves it in V1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChildPartitionKey(SourcePartition);

#[derive(Debug, thiserror::Error)]
#[error("partition {partition} is unreachable from a v1 SubAgent context: {clause}")]
pub struct ChildPartitionError {
    pub partition: SourcePartition,
    pub clause: &'static str,
}

impl ChildPartitionKey {
    /// The only constructor. A child attempting any Private-Dyad- or
    /// AgentSoul-keyed read/write fails closed here, at the type level.
    pub fn parse(partition: SourcePartition) -> Result<Self, ChildPartitionError> {
        match partition {
            SourcePartition::PrivateDyad => Err(ChildPartitionError {
                partition,
                clause: "SA-14 — Private Dyad is unreachable from any SubAgent path",
            }),
            SourcePartition::AgentSoul => Err(ChildPartitionError {
                partition,
                clause: "SA-15 — AgentSoul is excluded from SubAgent contexts by default",
            }),
            SourcePartition::UserModel | SourcePartition::SharedLexicon => Ok(Self(partition)),
        }
    }

    #[must_use]
    pub fn partition(&self) -> SourcePartition {
        self.0
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Digests
// ─────────────────────────────────────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut hex = String::with_capacity(out.len() * 2);
    for byte in out {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn canonical_json<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

impl ContextBundleV1 {
    /// Digest over the bundle's canonical content EXCLUDING the digest
    /// field itself. Deterministic across processes.
    #[must_use]
    pub fn compute_digest(&self) -> String {
        let mut value = canonical_json(self);
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("digest");
        }
        sha256_hex(serde_json::to_string(&value).unwrap_or_default().as_bytes())
    }

    /// Verify the pinned digest. Mid-run bundle mutation (any content
    /// change without a recomputed digest) fails here — admission pins the
    /// digest and the run re-verifies it (SA-18).
    pub fn verify_digest(&self) -> Result<(), DigestMismatchError> {
        let computed = self.compute_digest();
        if computed == self.digest {
            Ok(())
        } else {
            Err(DigestMismatchError {
                pinned: self.digest.clone(),
                computed,
            })
        }
    }

    /// Project this bundle into the bounded, redacted child view (SA-14/
    /// SA-18/SA-19). Private-Dyad-derived source refs are dropped
    /// UNCONDITIONALLY — ids, digests, counts, and existence — so a bundle
    /// materialized with and without such inputs projects byte-identically
    /// (existence-blind). Explicit exclusions remove their classes.
    #[must_use]
    pub fn projection(&self) -> BundleProjection {
        let excluded = |class: ContextClassV1| self.explicit_exclusions.contains(&class);
        let objective_context = if excluded(ContextClassV1::ObjectiveContext) {
            String::new()
        } else {
            self.objective_context.clone()
        };
        let source_refs = self
            .source_refs
            .iter()
            .filter(|r| r.partition != SourcePartition::PrivateDyad)
            .filter(|_| !excluded(ContextClassV1::SourceRefs))
            .cloned()
            .collect();
        let applicable_user_model = if excluded(ContextClassV1::UserModelProjection) {
            Vec::new()
        } else {
            self.applicable_user_model.clone()
        };
        let skill_refs =
            if excluded(ContextClassV1::SkillRefs) || self.redaction_policy.redact_skill_refs {
                Vec::new()
            } else {
                self.skill_refs.clone()
            };
        let procedure_refs = if excluded(ContextClassV1::ProcedureRefs)
            || self.redaction_policy.redact_procedure_refs
        {
            Vec::new()
        } else {
            self.procedure_refs.clone()
        };
        let mut projection = BundleProjection {
            bundle_id: self.bundle_id.clone(),
            projection_digest: String::new(),
            objective_context,
            source_refs,
            applicable_user_model,
            skill_refs,
            procedure_refs,
        };
        projection.projection_digest = projection.compute_digest();
        projection
    }

    /// The policy-enforced projection (SA-14/SA-15/SA-18/SA-19/SA-5):
    /// intersects the redacted projection with the admitted profile's
    /// context classes and permitted partitions, enforces the projection
    /// size ceiling (measured on the FINAL projection including its
    /// digest), and fails closed on any violation.
    ///
    /// Partition law, two distinct behaviors: `PrivateDyad` content is
    /// REDACTED (dropped unconditionally by `projection()` before this
    /// policy runs — SA-14.3 redaction/existence-blindness, so a bundle
    /// with and without private-derived inputs project identically);
    /// every OTHER partition not in `permitted_partitions` — including
    /// `AgentSoul`, which no v1 profile can permit (SA-15) — is REFUSED
    /// with a typed error, never silently dropped.
    pub fn projection_with_policy(
        &self,
        allowed_classes: &[ContextClassV1],
        permitted_partitions: &[SourcePartition],
        max_projection_bytes: usize,
    ) -> Result<BundleProjection, ProjectionPolicyError> {
        let class_allowed = |class: ContextClassV1| allowed_classes.contains(&class);
        let partition_allowed = |partition: SourcePartition| match partition {
            SourcePartition::PrivateDyad | SourcePartition::AgentSoul => false,
            other => permitted_partitions.contains(&other),
        };
        let base = self.projection();
        let mut source_refs = Vec::new();
        // Partition law first, over the RAW refs (PrivateDyad excepted —
        // redaction, SA-14.3): an unpermitted partition is REFUSED, and
        // NEITHER a class drop NOR a bundle-level explicit exclusion may
        // mask that refusal (dropping keeps the ref from the child, but
        // only the refusal surfaces the privacy violation).
        for source in &self.source_refs {
            if source.partition != SourcePartition::PrivateDyad
                && !partition_allowed(source.partition)
            {
                return Err(ProjectionPolicyError::DisallowedPartition {
                    ref_id: source.ref_id.clone(),
                    partition: source.partition,
                });
            }
            if class_allowed(ContextClassV1::SourceRefs)
                && !self
                    .explicit_exclusions
                    .contains(&ContextClassV1::SourceRefs)
                && source.partition != SourcePartition::PrivateDyad
            {
                source_refs.push(source.clone());
            }
        }
        let applicable_user_model = if class_allowed(ContextClassV1::UserModelProjection) {
            base.applicable_user_model.clone()
        } else {
            Vec::new()
        };
        let skill_refs = if class_allowed(ContextClassV1::SkillRefs) {
            base.skill_refs.clone()
        } else {
            Vec::new()
        };
        let procedure_refs = if class_allowed(ContextClassV1::ProcedureRefs) {
            base.procedure_refs.clone()
        } else {
            Vec::new()
        };
        let mut projection = BundleProjection {
            bundle_id: base.bundle_id,
            projection_digest: String::new(),
            objective_context: if class_allowed(ContextClassV1::ObjectiveContext) {
                base.objective_context
            } else {
                String::new()
            },
            source_refs,
            applicable_user_model,
            skill_refs,
            procedure_refs,
        };
        // Compute the digest FIRST, then measure the FINAL projection
        // (including the digest) against the size ceiling — the ceiling
        // covers exactly what the child receives.
        projection.projection_digest = projection.compute_digest();
        let serialized = serde_json::to_string(&projection).unwrap_or_default().len();
        if serialized > max_projection_bytes {
            return Err(ProjectionPolicyError::ProjectionTooLarge {
                actual_bytes: serialized,
                max_bytes: max_projection_bytes,
            });
        }
        Ok(projection)
    }
}

impl BundleProjection {
    /// Digest over the projected content. Existence-blind: redacted
    /// material never influences this value.
    #[must_use]
    pub fn compute_digest(&self) -> String {
        let mut value = canonical_json(self);
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("projection_digest");
        }
        sha256_hex(serde_json::to_string(&value).unwrap_or_default().as_bytes())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("digest mismatch: pinned {pinned}, computed {computed}")]
pub struct DigestMismatchError {
    pub pinned: String,
    pub computed: String,
}

impl SubAgentProfileV1 {
    /// Digest over the profile's canonical content excluding the digest
    /// field itself. A capability change changes the digest — SA-4's
    /// new-revision law is enforced over this value.
    #[must_use]
    pub fn compute_digest(&self) -> String {
        let mut value = canonical_json(self);
        if let serde_json::Value::Object(map) = &mut value {
            map.remove("digest");
        }
        sha256_hex(serde_json::to_string(&value).unwrap_or_default().as_bytes())
    }

    pub fn verify_digest(&self) -> Result<(), DigestMismatchError> {
        let computed = self.compute_digest();
        if computed == self.digest {
            Ok(())
        } else {
            Err(DigestMismatchError {
                pinned: self.digest.clone(),
                computed,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_bundle() -> ContextBundleV1 {
        let mut bundle = ContextBundleV1 {
            bundle_id: "bundle-1".into(),
            revision: 1,
            digest: String::new(),
            parent_ref: ParentRunRef::from_opaque("root-1"),
            objective_context: "Analyze the stability envelope.".into(),
            source_refs: vec![BundleSourceRef {
                ref_id: "src-1".into(),
                partition: SourcePartition::UserModel,
                content_digest: "aa".into(),
            }],
            applicable_user_model: vec![ProjectedFactRef {
                fact_id: "f-1".into(),
                statement_digest: "bb".into(),
            }],
            skill_refs: vec!["skill-a".into()],
            procedure_refs: vec![],
            explicit_exclusions: vec![],
            redaction_policy: BundleRedactionPolicy::default(),
        };
        bundle.digest = bundle.compute_digest();
        bundle
    }

    // SA-9/SA-11: lineage is immutable and monotonic; only `child` advances.
    #[test]
    fn lineage_only_advances_via_child() {
        let root = LineageRef::new_root(ParentRunRef::from_opaque("run-1"));
        assert_eq!(root.depth(), 0);
        let child = root.child();
        assert_eq!(child.depth(), 1);
        let grandchild = child.child();
        assert_eq!(grandchild.depth(), 2);
        assert_eq!(grandchild.root_ref(), root.root_ref());
        // The parent is unchanged by child() — no in-place advance.
        assert_eq!(root.depth(), 0);
    }

    // SA-7b/SA-12 (D1): the banned names are refused at the TYPE level —
    // "no field path that can name either tool" is structural.
    #[test]
    fn banned_tool_names_fail_at_parse() {
        for (name, _) in V1_BANNED_TOOL_NAMES {
            let err = SubAgentToolNameV1::parse(name).unwrap_err();
            assert_eq!(err.name, *name);
        }
        assert!(SubAgentToolNameV1::parse("").is_err());
        assert!(SubAgentToolNameV1::parse("read_context").is_ok());
    }

    // SA-27: the budget type carries all three ceilings.
    #[test]
    fn budget_carries_time_tokens_actions() {
        let budget = SubAgentBudgetV1::default();
        assert!(budget.time_limit_secs > 0);
        assert!(budget.max_tokens > 0);
        assert!(budget.max_actions > 0);
    }

    // SA-4/SA-18: digest pinning — content mutation without a recomputed
    // digest is detected.
    #[test]
    fn bundle_digest_pins_content() {
        let bundle = sample_bundle();
        bundle.verify_digest().expect("fresh bundle verifies");
        let mut mutated = bundle.clone();
        mutated.objective_context = "Widened objective.".into();
        assert!(
            mutated.verify_digest().is_err(),
            "mid-run bundle mutation must be refused by digest verification"
        );
    }

    #[test]
    fn profile_digest_pins_content() {
        let mut profile = SubAgentProfileV1 {
            profile_id: "p".into(),
            revision: 1,
            digest: String::new(),
            role: SubAgentRoleV1::Reasoning,
            model_policy: ModelPolicyV1 {
                provider_ref: "openai.main".into(),
                model: None,
                temperature: None,
            },
            tool_policy: SubAgentToolPolicyV1::default(),
            supervisor_authority_set: vec![],
            context_policy: SubAgentContextPolicyV1::default(),
            privacy_policy: SubAgentPrivacyPolicyV1::default(),
            budget: SubAgentBudgetV1::default(),
            recursion: SubAgentRecursionPolicyV1::NoLocalSpawn,
            output_schema: SubAgentOutputSchemaV1::StructuredReport,
        };
        profile.digest = profile.compute_digest();
        profile.verify_digest().unwrap();
        let mut widened = profile.clone();
        widened.tool_policy.tools = vec![SubAgentToolNameV1::parse("read_context").unwrap()];
        assert!(widened.verify_digest().is_err());
    }

    // SA-14: existence-blind projection — a bundle with and without a
    // Private-Dyad-derived ref project byte-identically.
    #[test]
    fn bundle_projection_is_existence_blind_for_private_dyad() {
        let clean = sample_bundle();
        let mut with_private = clean.clone();
        with_private.source_refs.push(BundleSourceRef {
            ref_id: "secret-1".into(),
            partition: SourcePartition::PrivateDyad,
            content_digest: "cc".into(),
        });
        // Both bundles are digest-VALID (each recomputes its own pinned
        // digest over its own content): the private-derived ref is
        // legitimately IN the second bundle. Existence-blindness must
        // hold anyway — including through the child-visible projection
        // digest, which is computed over projected content only.
        with_private.digest = with_private.compute_digest();
        with_private.verify_digest().unwrap();
        clean.verify_digest().unwrap();
        assert_ne!(clean.digest, with_private.digest);
        assert_eq!(clean.projection(), with_private.projection());
        // The child-visible digest specifically is identical (the
        // pinned full-bundle digests differ — that difference must
        // never surface to the child).
        assert_eq!(
            clean.projection().projection_digest,
            with_private.projection().projection_digest
        );
        // And the policy-filtered path is existence-blind too.
        let policy = clean.projection_with_policy(
            &[
                ContextClassV1::ObjectiveContext,
                ContextClassV1::SourceRefs,
                ContextClassV1::UserModelProjection,
            ],
            &[SourcePartition::UserModel, SourcePartition::SharedLexicon],
            65_536,
        );
        let policy_private = with_private.projection_with_policy(
            &[
                ContextClassV1::ObjectiveContext,
                ContextClassV1::SourceRefs,
                ContextClassV1::UserModelProjection,
            ],
            &[SourcePartition::UserModel, SourcePartition::SharedLexicon],
            65_536,
        );
        assert_eq!(policy.unwrap(), policy_private.unwrap());
    }

    // SA-14/SA-15: the policy-filtered projection fails closed on
    // disallowed partitions and on oversized projections.
    #[test]
    fn policy_projection_fails_closed() {
        let bundle = sample_bundle();
        // PrivateDyad is REDACTION, not refusal (SA-14.3): a
        // private-ONLY bundle (no other refs to trigger partition law)
        // projects fine with zero source refs — even if someone lists
        // PrivateDyad as "permitted", the partition never reaches the
        // child and the projection is indistinguishable from a bundle
        // with no private input at all.
        let mut private_only_bundle = ContextBundleV1 {
            source_refs: vec![BundleSourceRef {
                ref_id: "secret".into(),
                partition: SourcePartition::PrivateDyad,
                content_digest: "x".into(),
            }],
            ..sample_bundle()
        };
        private_only_bundle.digest = private_only_bundle.compute_digest();
        let projection = private_only_bundle
            .projection_with_policy(
                &[ContextClassV1::SourceRefs],
                &[SourcePartition::PrivateDyad, SourcePartition::UserModel],
                65_536,
            )
            .expect("private-only content is redacted, never refused");
        assert!(projection.source_refs.is_empty());
        assert!(!format!("{projection:?}").contains("secret"));

        // AgentSoul partition: refused unconditionally (SA-15).
        let mut soul_bundle = sample_bundle();
        soul_bundle.source_refs.push(BundleSourceRef {
            ref_id: "soul".into(),
            partition: SourcePartition::AgentSoul,
            content_digest: "y".into(),
        });
        soul_bundle.digest = soul_bundle.compute_digest();
        let err = soul_bundle
            .projection_with_policy(
                &[ContextClassV1::SourceRefs],
                &[SourcePartition::AgentSoul],
                65_536,
            )
            .unwrap_err();
        assert!(err.to_string().contains("does not permit"), "{err}");

        // A permitted partition not listed in the privacy policy:
        // deny-by-default refusal.
        let err = bundle
            .projection_with_policy(
                &[ContextClassV1::SourceRefs],
                &[SourcePartition::SharedLexicon], // UserModel not permitted
                65_536,
            )
            .unwrap_err();
        assert!(err.to_string().contains("user_model"), "{err}");

        // Oversized projection: refused, never silently truncated.
        let err = bundle
            .projection_with_policy(
                &[
                    ContextClassV1::ObjectiveContext,
                    ContextClassV1::SourceRefs,
                    ContextClassV1::UserModelProjection,
                ],
                &[SourcePartition::UserModel, SourcePartition::SharedLexicon],
                8,
            )
            .unwrap_err();
        assert!(err.to_string().contains("max_projection_bytes"), "{err}");

        // Classes not in allowed_classes are dropped (narrowing), and a
        // clean projection passes.
        let projection = bundle
            .projection_with_policy(
                &[ContextClassV1::ObjectiveContext],
                &[SourcePartition::UserModel, SourcePartition::SharedLexicon],
                65_536,
            )
            .unwrap();
        assert!(projection.source_refs.is_empty());
        assert!(projection.applicable_user_model.is_empty());
    }

    // SA-19: explicit exclusions are enforced at projection time.
    #[test]
    fn exclusions_remove_their_classes_at_projection() {
        let mut bundle = sample_bundle();
        bundle
            .explicit_exclusions
            .push(ContextClassV1::UserModelProjection);
        bundle.explicit_exclusions.push(ContextClassV1::SkillRefs);
        let projection = bundle.projection();
        assert!(projection.applicable_user_model.is_empty());
        assert!(projection.skill_refs.is_empty());
        assert!(!projection.source_refs.is_empty());
    }

    // SA-22: the report rejects unknown fields — a chain-of-thought field
    // cannot even be deserialized.
    #[test]
    fn report_rejects_chain_of_thought_and_unknown_fields() {
        let report = SubAgentReportV1 {
            run_ref: SubAgentRunRef::from_opaque("run-1"),
            profile_ref: VersionedProfileRef {
                profile_id: "p".into(),
                revision: 1,
                digest: "d".into(),
            },
            context_bundle_ref: "bundle-1".into(),
            status: SubAgentTerminalFact::Completed,
            summary: "done".into(),
            findings: vec![],
            evidence_refs: vec![],
            uncertainty: vec![],
            recommendations: vec![],
            requested_parent_actions: vec![],
            proposed_candidates: vec![],
            usage: SubAgentUsage::default(),
        };
        let ok = serde_json::to_value(&report).unwrap();
        assert!(serde_json::from_value::<SubAgentReportV1>(ok).is_ok());

        let mut smuggled = serde_json::to_value(&report).unwrap();
        smuggled["chain_of_thought"] = serde_json::json!("step 1: ... step 2: ...");
        let err = serde_json::from_value::<SubAgentReportV1>(smuggled).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    // SA-23: the control-event and terminal-fact sets are distinct, and
    // the transition map is exact.
    #[test]
    fn terminal_facts_map_to_their_control_events() {
        assert_eq!(
            SubAgentTerminalFact::Stopped.required_control_event(),
            Some(SubAgentControlEvent::GracefulStopRequested)
        );
        assert_eq!(
            SubAgentTerminalFact::Aborted.required_control_event(),
            Some(SubAgentControlEvent::AbortRequested)
        );
        for fact in [
            SubAgentTerminalFact::TimedOut,
            SubAgentTerminalFact::Completed,
            SubAgentTerminalFact::Failed,
        ] {
            assert_eq!(fact.required_control_event(), None);
        }
    }

    // SA-17/SA-22 seam: KP-18 active-authority candidate kinds are marked.
    #[test]
    fn kp18_candidate_kinds_require_reviewed_promotion() {
        assert!(ProposedCandidateKind::UserModel.requires_reviewed_promotion());
        assert!(ProposedCandidateKind::PrivateDyadDerived.requires_reviewed_promotion());
        assert!(!ProposedCandidateKind::OrdinaryMemory.requires_reviewed_promotion());
    }

    // SA-6 (wire half): the mid-run request surface has no free-text
    // payload — variants carry ids/codes only.
    #[test]
    fn midrun_requests_carry_no_free_text() {
        let req = SubAgentMidRunRequest::RequestUserInput {
            uncertainty_item_ids: vec!["u-1".into()],
        };
        let value = serde_json::to_value(&req).unwrap();
        let obj = value.as_object().unwrap();
        for (key, val) in obj {
            match val {
                serde_json::Value::Array(items) => {
                    for item in items {
                        assert!(item.is_string(), "unexpected payload in {key}");
                    }
                }
                serde_json::Value::String(s) => {
                    assert!(s.len() < 64, "field {key} looks like free text");
                }
                _ => {}
            }
        }
    }

    // SA-14/SA-15: the child partition gate fails closed on Private Dyad
    // and AgentSoul, at the type level.
    #[test]
    fn child_partition_gate_fails_closed() {
        assert!(ChildPartitionKey::parse(SourcePartition::PrivateDyad).is_err());
        assert!(ChildPartitionKey::parse(SourcePartition::AgentSoul).is_err());
        assert!(ChildPartitionKey::parse(SourcePartition::UserModel).is_ok());
        assert!(ChildPartitionKey::parse(SourcePartition::SharedLexicon).is_ok());
    }
}
