//! AgentSoul candidate intake — evidence is not authority (AgentSoul tracker contract).
//!
//! This leaf builds on the soul seam ([`crate::soul`]): it records
//! *candidate* Soul dispositions plus evidence/counterevidence
//! references for exactly one admitted
//! [`AgentIdentityId`](zeroclaw_api::companion::AgentIdentityId), while
//! making implicit promotion structurally unrepresentable:
//!
//! - only the frozen authority list may create/update a candidate —
//!   one incident, repeated behavior, benchmark result, worker
//!   receipt, Tachi verified evidence ref, model summary, owner
//!   correction — encoded as the closed [`CandidateOrigin`] enum, and
//!   NONE of them can create an active Soul disposition;
//! - [`CandidateStatus`] has exactly two variants, `Candidate` and
//!   `Retracted`. There is no `Active` variant anywhere in this
//!   module, so no method — however named — can return or store an
//!   active disposition. Promotion belongs to the explicit authorized
//!   later review leaf alone;
//! - counts, frequency, recurrence, and confidence are evidence
//!   QUALITY metadata ([`RecurrenceMetadata`]), recomputed from the
//!   deduped evidence refs on every write. They never feed any
//!   authority decision because no authority decision exists here;
//! - evidence refs are opaque ids into their owning source systems
//!   (incident store, benchmark run, Tachi envelope, worker receipt).
//!   Referenced content is NEVER copied into a second truth store;
//!   each ref keeps its source revision and derivation ref so a later
//!   invalidation leaf (a future AgentSoul leaf) can find dependents;
//! - user-preference-shaped evidence is rejected with a typed error
//!   naming the User Model ([`CandidateError::UserModelDomain`]) —
//!   never silently stored as Soul. Every intake must carry an
//!   explicit [`DomainClassification`];
//! - identity resolution reuses the same fail-closed
//!   [`IdentityRegistry`]: unadmitted, ambiguous, revoked, or
//!   malformed identities fail closed before any storage access;
//! - storage is one row per candidate under the soul namespace
//!   (`soul::<identity>::candidate::<id>`) through the existing
//!   [`Memory`] trait; carrier attributes never enter the key;
//! - Tachi verified evidence can support a candidate but never
//!   activates Soul (structurally: nothing can). When Tachi is
//!   unavailable, local observations may still create local
//!   candidates while the external ref is marked
//!   [`EvidenceVerification::Unavailable`] — never fabricated as
//!   verified;
//! - no persona projection, no promotion path, no suspend/resume/
//!   reset/erase lifecycle exists here (later leaves under).
//!   Retraction ([`SoulCandidateService::retract`]) is the one
//!   lifecycle-shaped operation: owner-correction-only, and it keeps
//!   the row as history instead of erasing it.
//!
//! Deliberate scope boundary (repair-round adjudication for this leaf): this
//! module performs NO owner principal/grant authentication — any caller
//! claiming the [`CandidateOrigin::OwnerCorrection`] origin is taken at
//! its word. Authenticating who may speak for an owner belongs to the
//! explicit review/promotion leaf  and its authorization
//! substrate , not to intake.

use crate::soul::{
    CarrierContext, IdentityRegistry, NAMESPACE_DELIMITER, SoulError, SoulService,
    classify_backend_error, validate_identity_token,
};
use crate::traits::{Memory, MemoryCategory};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use zeroclaw_api::companion::AgentIdentityId;

/// The reserved key prefix separating candidate rows from other Soul
/// rows under `soul::<identity>::`. Single source of truth:
/// [`crate::soul::RESERVED_CANDIDATE_PREFIX`], which the raw Soul seam
/// refuses for non-candidate writers, so the two can never drift.
use crate::soul::RESERVED_CANDIDATE_PREFIX;

/// Typed fail-closed errors for Soul candidate intake. Identity
/// failures reuse [`SoulError`] through the [`CandidateError::Soul`]
/// variant; every candidate-domain failure has its own explicit
/// variant. No variant ever stores, guesses, or promotes anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateError {
    /// Identity resolution failed (unavailable / ambiguous / revoked /
    /// malformed / cross-identity). Carries the soul seam's typed cause.
    Soul(SoulError),
    /// A candidate value exceeds a fixed processing/storage bound.
    LimitExceeded {
        field: &'static str,
        limit: usize,
        actual: usize,
    },
    /// The evidence is user-preference-shaped and belongs to the User
    /// Model domain, not Soul. Rejected at intake — never stored.
    UserModelDomain(String),
    /// The candidate id is malformed for namespacing (empty, or
    /// containing the `::` delimiter).
    InvalidCandidateId(String),
    /// The disposition facet or proposed rule is empty.
    InvalidDisposition(String),
    /// The evidence reference id is empty.
    InvalidEvidenceRef(String),
    /// The same evidence ref id was already recorded with the opposite
    /// stance. Stance flips are explicit rejections, never silent
    /// rewrites.
    ConflictingEvidenceStance(String),
    /// Only an owner correction may retract a candidate.
    RetractionRequiresOwnerCorrection,
    /// The candidate is retracted; it stays readable as history but
    /// accepts no further evidence through this leaf.
    CandidateRetracted(String),
    /// No candidate with this id exists for the resolved identity.
    NotFound(String),
    /// The backend cannot round-trip raw-key JSON rows (e.g. the
    /// markdown backend synthesizes keys and wraps rows), so Soul
    /// candidate rows would corrupt on read. Refused at construction.
    UnsupportedBackend(String),
    /// A stored candidate row failed to deserialize. Fails loud: the
    /// row is never overwritten or guessed around.
    Corrupt(String),
}

impl CandidateError {
    fn message(&self) -> &str {
        match self {
            Self::LimitExceeded { .. } => "soul candidate size limit exceeded",
            Self::Soul(_) => "soul identity resolution failed",
            Self::UserModelDomain(_) => {
                "user-preference evidence rejected: route to UserModel, not Soul"
            }
            Self::InvalidCandidateId(_) => "invalid candidate id for Soul namespacing",
            Self::InvalidDisposition(_) => "invalid candidate disposition or proposed rule",
            Self::InvalidEvidenceRef(_) => "invalid evidence reference",
            Self::ConflictingEvidenceStance(_) => {
                "evidence ref already recorded with the opposite stance"
            }
            Self::RetractionRequiresOwnerCorrection => {
                "candidate retraction requires an owner correction origin"
            }
            Self::CandidateRetracted(_) => "candidate is retracted: kept as history, intake denied",
            Self::NotFound(_) => "soul candidate not found for the resolved identity",
            Self::UnsupportedBackend(_) => "backend cannot round-trip raw-key soul candidate rows",
            Self::Corrupt(_) => "stored soul candidate row is corrupt",
        }
    }
}

impl fmt::Display for CandidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                field,
                limit,
                actual,
            } => write!(
                f,
                "{}: {field} limit={limit} actual={actual}",
                self.message()
            ),
            Self::Soul(inner) => write!(f, "{}: {inner}", self.message()),
            Self::UserModelDomain(detail) => write!(f, "{}: {detail}", self.message()),
            Self::InvalidCandidateId(id) => write!(f, "{}: {id}", self.message()),
            Self::InvalidDisposition(detail) => write!(f, "{}: {detail}", self.message()),
            Self::InvalidEvidenceRef(id) => write!(f, "{}: {id}", self.message()),
            Self::ConflictingEvidenceStance(id) => write!(f, "{}: {id}", self.message()),
            Self::CandidateRetracted(id) => write!(f, "{}: {id}", self.message()),
            Self::NotFound(id) => write!(f, "{}: {id}", self.message()),
            Self::UnsupportedBackend(name) => write!(f, "{}: {name}", self.message()),
            Self::Corrupt(detail) => write!(f, "{}: {detail}", self.message()),
            Self::RetractionRequiresOwnerCorrection => f.write_str(self.message()),
        }
    }
}

impl std::error::Error for CandidateError {}

impl From<SoulError> for CandidateError {
    fn from(inner: SoulError) -> Self {
        Self::Soul(inner)
    }
}

/// The complete lifecycle vocabulary of a Soul candidate in this leaf.
/// Deliberately two variants: `Candidate` and `Retracted`. There is NO
/// `Active` variant — an active Soul disposition is created only by the
/// explicit authorized review leaf , through its own types, never
/// through candidate intake. The exhaustive-match test in this module
/// fails to compile if a variant is added here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// Recorded proposal with evidence. Absorbs any amount of
    /// repetition, frequency, or confidence without ever becoming more
    /// than a candidate.
    Candidate,
    /// Retracted by owner correction. The row remains as history;
    /// staleness-through-source-invalidation belongs to a later leaf.
    Retracted,
}

/// The frozen authority list : the ONLY origins that may create
/// or update a candidate. Closed by construction — there is no
/// `Other`/`Custom` variant, so no caller can smuggle an origin past
/// this enum. None of these origins can activate Soul; they only ever
/// feed candidate/evidence state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateOrigin {
    /// A single observed incident.
    SingleIncident,
    /// A repeatedly observed behavior pattern.
    RepeatedBehavior,
    /// A benchmark/evaluation result.
    BenchmarkResult,
    /// A receipt from an external worker/harness.
    WorkerReceipt,
    /// A verified Tachi evidence reference (the pending Tachi envelope
    /// contract). Supports candidates; never activates Soul.
    TachiVerifiedEvidence,
    /// A model-generated summary of observations.
    ModelSummary,
    /// A correction issued by the owner. The only origin that may
    /// retract a candidate or amend its disposition/rule text.
    OwnerCorrection,
}

/// Explicit domain classification required at intake (per the AgentSoul domain
/// boundary: Soul is not a user-preference store). Evidence classified
/// as [`DomainClassification::UserPreference`] is typed-rejected with
/// [`CandidateError::UserModelDomain`] and must be routed to the User
/// Model store instead — this is the "explicit domain classification
/// path" the discrimination demands; there is no unclassified
/// intake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainClassification {
    /// A proposed disposition of the AGENT's own operating posture
    /// (temperament/craft/judgment/relationship protocol shapes).
    SoulDisposition,
    /// A preference about or directed at the USER. Belongs to the User
    /// Model domain; rejected here.
    UserPreference,
}

/// Whether one evidence reference supports or counters the candidate.
/// Counterevidence is a first-class citizen: it is recorded alongside
/// supporting evidence and never deletes or demotes the candidate —
/// weighing is review's job .
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStance {
    /// Supports the proposed disposition.
    Supporting,
    /// Counts against the proposed disposition.
    Countering,
}

/// Verification state of an evidence reference. Tachi being unavailable
/// is represented as [`EvidenceVerification::Unavailable`] — external
/// evidence is marked, never fabricated as verified, so local
/// observations can still create local candidates (per the leaf's failure model).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceVerification {
    /// Verified by the source system (e.g. a Tachi verified envelope).
    /// Still only evidence: verification never activates Soul.
    Verified,
    /// Recorded but not verified by any external system.
    Unverified,
    /// The backing source was unreachable/unavailable at record time.
    Unavailable,
}

/// Privacy/sensitivity classification of the candidate. Required on
/// every intake so no Soul row exists unclassified; consumed by the
/// future privacy-erase leaf (a future privacy-erase leaf).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    /// No privacy concern.
    Public,
    /// Internal agent-operation data.
    Internal,
    /// Privacy-sensitive; restricts downstream handling.
    Sensitive,
}

/// A reference to one piece of evidence. Refs are opaque ids into
/// their OWNING source system — incident store, benchmark run ledger,
/// Tachi envelope, worker receipt log. Referenced content is never
/// copied here: this module holds no second copy of any source truth
/// (project engineering precedent stays in project/precedent memory,
/// per the AgentSoul domain boundary).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    /// Opaque reference id in the owning source system. Non-empty;
    /// deduped by value on the candidate.
    pub id: String,
    /// Whether this observation supports or counters the candidate.
    pub stance: EvidenceStance,
    /// Verification state of the reference at record time.
    pub verification: EvidenceVerification,
    /// Revision/version of the referenced source, when the source
    /// system exposes one. Retained so a corrected/retracted source can
    /// later find its dependents (invalidation leaf).
    pub source_revision: Option<String>,
    /// Reference this evidence was derived from (computation,
    /// aggregation, summary), so invalidation can walk the chain.
    pub derived_from: Option<String>,
}

impl EvidenceRef {
    fn validate(&self, origin: CandidateOrigin) -> Result<(), CandidateError> {
        if self.id.trim().is_empty() {
            return Err(CandidateError::InvalidEvidenceRef("(empty)".to_string()));
        }
        // Consistency rule (repair-round adjudication for this leaf): the
        // TachiVerifiedEvidence origin MUST carry a Verified ref — an
        // unverified or unavailable reference can never claim it.
        // Verified refs from OTHER origins stay allowed (worker
        // receipts etc. can be verified by other means).
        if origin == CandidateOrigin::TachiVerifiedEvidence
            && self.verification != EvidenceVerification::Verified
        {
            return Err(CandidateError::InvalidEvidenceRef(format!(
                "{}: tachi-verified origin requires a verified ref",
                self.id
            )));
        }
        Ok(())
    }

    /// Fold a refresh of this ref (same id, same stance) into it:
    /// incoming `Some(...)` revision/derivation values REPLACE the old
    /// ones, but an incoming `None` KEEPS the existing value — refreshes
    /// merge, they never lose retained provenance. Verification state
    /// is non-optional and always takes the incoming value.
    fn merge_refresh(&mut self, incoming: EvidenceRef) {
        if incoming.source_revision.is_some() {
            self.source_revision = incoming.source_revision;
        }
        if incoming.derived_from.is_some() {
            self.derived_from = incoming.derived_from;
        }
        self.verification = incoming.verification;
    }
}

/// Repetition/frequency/confidence metadata — evidence QUALITY signals
/// only (per the leaf's frozen rule). Recomputed from the deduped evidence refs
/// on every write, so these numbers can never drift from the evidence
/// they describe, and nothing in this module reads them to make any
/// authority decision (there is no authority decision to make).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecurrenceMetadata {
    /// Count of DISTINCT supporting evidence refs (deduped by ref id).
    pub supporting_observations: usize,
    /// Count of DISTINCT countering evidence refs.
    pub countering_observations: usize,
    /// Latest observer-supplied confidence label (e.g. `"high"`).
    /// Quality signal only — never authority.
    pub confidence: Option<String>,
    /// Outcome/adjudication references recorded with the evidence.
    /// Quality signal only — never authority.
    pub outcome_refs: Vec<String>,
}

impl RecurrenceMetadata {
    /// Recompute observation counts from the canonical evidence list.
    fn recount(&mut self, evidence: &[EvidenceRef]) {
        self.supporting_observations = evidence
            .iter()
            .filter(|r| r.stance == EvidenceStance::Supporting)
            .count();
        self.countering_observations = evidence
            .iter()
            .filter(|r| r.stance == EvidenceStance::Countering)
            .count();
    }
}

/// One stored Soul candidate: a proposed behavioral disposition bound
/// to exactly one admitted AgentIdentity (the binding lives in the
/// agent-scoped row — key prefix plus row attribution — which is why
/// the record itself carries no identity field to drift).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoulCandidate {
    /// Candidate id, unique within one identity's namespace.
    pub candidate_id: String,
    /// Disposition facet this candidate speaks to (e.g.
    /// `"explanation density"`). Free-form: the AgentSoul boundary lists
    /// example shapes, not a closed registry.
    pub disposition: String,
    /// The proposed behavioral rule as observed/proposed. A proposal —
    /// never an operating rule until review promotes it .
    pub proposed_rule: String,
    /// Lifecycle status. `Candidate` or `Retracted` — never active.
    pub status: CandidateStatus,
    /// Deduped evidence and counterevidence references.
    pub evidence: Vec<EvidenceRef>,
    /// Recurrence/confidence/outcome metadata. Evidence quality only.
    pub recurrence: RecurrenceMetadata,
    /// Context/task-shape tags accumulated across intakes, where the
    /// observer already had them (diversity is evidence quality).
    pub context_shapes: Vec<String>,
    /// Privacy/sensitivity classification. Required at intake.
    pub sensitivity: Sensitivity,
    /// Origin that last created/updated this candidate (audit trail of
    /// the frozen authority list).
    pub last_origin: CandidateOrigin,
}

impl SoulCandidate {
    fn from_intake(intake: &CandidateIntake) -> Self {
        let evidence = vec![intake.evidence.clone()];
        let mut recurrence = RecurrenceMetadata {
            confidence: intake.confidence.clone(),
            outcome_refs: intake.outcome_refs.clone(),
            ..RecurrenceMetadata::default()
        };
        recurrence.recount(&evidence);
        Self {
            candidate_id: intake.candidate_id.clone(),
            disposition: intake.disposition.clone(),
            proposed_rule: intake.proposed_rule.clone(),
            status: CandidateStatus::Candidate,
            evidence,
            recurrence,
            context_shapes: intake.context_shapes.clone(),
            sensitivity: intake.sensitivity,
            last_origin: intake.origin,
        }
    }

    /// Fold one intake into the candidate: dedupe/refresh evidence by
    /// ref id, merge context/outcome tags, adopt the latest confidence
    /// label, retain the more restrictive sensitivity, and let an owner
    /// correction amend the disposition/rule text. Never touches status
    /// and never promotes anything.
    fn merge_intake(&mut self, intake: CandidateIntake) -> Result<(), CandidateError> {
        match self
            .evidence
            .iter_mut()
            .find(|existing| existing.id == intake.evidence.id)
        {
            Some(existing) if existing.stance != intake.evidence.stance => {
                return Err(CandidateError::ConflictingEvidenceStance(
                    intake.evidence.id,
                ));
            }
            // Same id, same stance: merge the refresh into the existing
            // ref (explicit dedupe/reference behavior — no duplicate
            // entry, no authority change, and retained revision/
            // derivation refs are never dropped by a None).
            Some(existing) => existing.merge_refresh(intake.evidence),
            None => self.evidence.push(intake.evidence),
        }
        merge_dedup(&mut self.context_shapes, intake.context_shapes);
        merge_dedup(&mut self.recurrence.outcome_refs, intake.outcome_refs);
        if intake.confidence.is_some() {
            self.recurrence.confidence = intake.confidence;
        }
        if intake.origin == CandidateOrigin::OwnerCorrection {
            self.disposition = intake.disposition;
            self.proposed_rule = intake.proposed_rule;
        }
        self.sensitivity = match (self.sensitivity, intake.sensitivity) {
            (Sensitivity::Sensitive, _) | (_, Sensitivity::Sensitive) => Sensitivity::Sensitive,
            (Sensitivity::Internal, _) | (_, Sensitivity::Internal) => Sensitivity::Internal,
            (Sensitivity::Public, Sensitivity::Public) => Sensitivity::Public,
        };
        self.recurrence.recount(&self.evidence);
        self.last_origin = intake.origin;
        Ok(())
    }
}

/// Append `additions` to `target`, skipping values already present.
/// Order-stable so merged metadata stays deterministic.
fn merge_dedup(target: &mut Vec<String>, additions: Vec<String>) {
    for item in additions {
        if !target.contains(&item) {
            target.push(item);
        }
    }
}

/// One intake event: what a frozen-list origin submits for one
/// candidate. Every field that classifies the submission
/// (`domain`, `sensitivity`, `origin`) is required — there is no
/// unclassified intake path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateIntake {
    /// Candidate id, caller-chosen, stable across intakes so repeated
    /// evidence accumulates on one row. Non-empty, no `::`.
    pub candidate_id: String,
    /// Explicit domain classification. `UserPreference` is
    /// typed-rejected here — this is the routing decision point.
    pub domain: DomainClassification,
    /// Disposition facet (set at creation; amendable by owner
    /// correction). Non-empty.
    pub disposition: String,
    /// Proposed behavioral rule. Non-empty.
    pub proposed_rule: String,
    /// Which frozen authority creates/updates the candidate.
    pub origin: CandidateOrigin,
    /// The evidence reference for this intake. Ids only — never
    /// content.
    pub evidence: EvidenceRef,
    /// Context/task-shape tags already available to the observer.
    pub context_shapes: Vec<String>,
    /// Outcome/adjudication refs for this intake.
    pub outcome_refs: Vec<String>,
    /// Observer confidence label. Quality signal only.
    pub confidence: Option<String>,
    /// Privacy/sensitivity classification. Required.
    pub sensitivity: Sensitivity,
}

const MAX_SCALAR_BYTES: usize = 4096;
const MAX_RULE_BYTES: usize = 16384;
const MAX_TAGS: usize = 64;
const MAX_EVIDENCE: usize = 256;
const MAX_ENCODED_BYTES: usize = 262144;

fn bound(field: &'static str, actual: usize, limit: usize) -> Result<(), CandidateError> {
    if actual > limit {
        return Err(CandidateError::LimitExceeded {
            field,
            limit,
            actual,
        });
    }
    Ok(())
}

fn scalar(field: &'static str, value: &str) -> Result<(), CandidateError> {
    bound(field, value.len(), MAX_SCALAR_BYTES)
}

fn evidence_bounds(evidence: &EvidenceRef) -> Result<(), CandidateError> {
    scalar("evidence.id", &evidence.id)?;
    if let Some(value) = &evidence.source_revision {
        scalar("source_revision", value)?;
    }
    if let Some(value) = &evidence.derived_from {
        scalar("derived_from", value)?;
    }
    Ok(())
}

fn metadata_bounds(
    contexts: &[String],
    outcomes: &[String],
    confidence: Option<&str>,
) -> Result<(), CandidateError> {
    bound("context_shapes", contexts.len(), MAX_TAGS)?;
    bound("outcome_refs", outcomes.len(), MAX_TAGS)?;
    for value in contexts {
        scalar("context_shape", value)?;
    }
    for value in outcomes {
        scalar("outcome_ref", value)?;
    }
    if let Some(value) = confidence {
        scalar("confidence", value)?;
    }
    Ok(())
}

fn intake_bounds(intake: &CandidateIntake) -> Result<(), CandidateError> {
    scalar("candidate_id", &intake.candidate_id)?;
    scalar("disposition", &intake.disposition)?;
    bound("proposed_rule", intake.proposed_rule.len(), MAX_RULE_BYTES)?;
    evidence_bounds(&intake.evidence)?;
    metadata_bounds(
        &intake.context_shapes,
        &intake.outcome_refs,
        intake.confidence.as_deref(),
    )
}

fn candidate_bounds(candidate: &SoulCandidate) -> Result<(), CandidateError> {
    scalar("candidate_id", &candidate.candidate_id)?;
    scalar("disposition", &candidate.disposition)?;
    bound(
        "proposed_rule",
        candidate.proposed_rule.len(),
        MAX_RULE_BYTES,
    )?;
    bound("evidence", candidate.evidence.len(), MAX_EVIDENCE)?;
    for evidence in &candidate.evidence {
        evidence_bounds(evidence)?;
    }
    metadata_bounds(
        &candidate.context_shapes,
        &candidate.recurrence.outcome_refs,
        candidate.recurrence.confidence.as_deref(),
    )
}

// The backend has already allocated the String. This bounds decoding and
// subsequent processing, not the backend's initial read allocation.
fn decode_candidate(content: &str) -> Result<SoulCandidate, CandidateError> {
    bound("encoded_candidate", content.len(), MAX_ENCODED_BYTES)?;
    let candidate: SoulCandidate =
        serde_json::from_str(content).map_err(|e| CandidateError::Corrupt(e.to_string()))?;
    candidate_bounds(&candidate)?;
    Ok(candidate)
}

fn validate_candidate_id(candidate_id: &str) -> Result<(), CandidateError> {
    if candidate_id.trim().is_empty() {
        Err(CandidateError::InvalidCandidateId("(empty)".to_string()))
    } else if candidate_id.contains(NAMESPACE_DELIMITER) {
        Err(CandidateError::InvalidCandidateId(candidate_id.to_string()))
    } else {
        Ok(())
    }
}

fn validate_disposition(disposition: &str, proposed_rule: &str) -> Result<(), CandidateError> {
    if disposition.trim().is_empty() || proposed_rule.trim().is_empty() {
        Err(CandidateError::InvalidDisposition(format!(
            "disposition={disposition:?} rule-empty={}",
            proposed_rule.trim().is_empty()
        )))
    } else {
        Ok(())
    }
}

/// Process-wide serialization for candidate read-modify-write paths
/// (submit/retract). A STATIC, not a per-instance field: two
/// `SoulCandidateService` instances over one backend would otherwise
/// interleave their read-modify-write sequences and silently lose each
/// other's evidence. Candidate write volume is low (intake events, not
/// chat traffic) — cross-instance correctness beats per-instance
/// latency. Point reads (`get`) stay lock-free; `candidates` takes this
/// lock while walking pages to keep its view stable against candidate
/// updates in this process.
static CANDIDATE_WRITE_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Each storage read stays bounded while `candidates` walks the exact prefix
/// to completion. The write lock keeps that multi-page view stable against
/// candidate updates in this process; cross-process consistency requires a
/// separately chosen and mechanically enforced supported-writer model.
const CANDIDATE_PAGE_SIZE: usize = 256;

/// Candidate intake/query service behind the Soul domain seam.
/// Every method resolves the caller's identity through the same
/// fail-closed [`IdentityRegistry`] as [`SoulService`], stores one row
/// per candidate under `soul::<identity>::candidate::<id>` through the
/// existing [`Memory`] trait, and exposes NO activation, promotion, or
/// reset path — candidate status is the only lifecycle this type can
/// write, and it has no active variant.
pub struct SoulCandidateService {
    registry: Arc<IdentityRegistry>,
    backend: Arc<dyn Memory>,
}

impl SoulCandidateService {
    /// Construct the candidate service over one memory backend.
    /// Refuses backends that cannot round-trip raw-key JSON rows: the
    /// markdown backend synthesizes keys and wraps rows in scaffolding,
    /// so candidate JSON written through it corrupts on read. Other
    /// backends may be constructed, but complete listing additionally
    /// requires exact-prefix pagination. Tachi currently uses the
    /// trait's default refusal for that operation, so `candidates`
    /// fails closed instead of returning an incomplete result.
    pub fn new(
        registry: Arc<IdentityRegistry>,
        backend: Arc<dyn Memory>,
    ) -> Result<Self, CandidateError> {
        if backend.name().contains("markdown") {
            return Err(CandidateError::UnsupportedBackend(
                backend.name().to_string(),
            ));
        }
        Ok(Self { registry, backend })
    }

    /// Full namespaced key for one candidate row. Carrier attributes
    /// are structurally irrelevant (same law as the soul seam); the `candidate`
    /// segment is reserved inside the Soul namespace.
    #[must_use]
    pub fn candidate_key(identity: &AgentIdentityId, candidate_id: &str) -> String {
        SoulService::namespace_key(
            identity,
            &format!("{RESERVED_CANDIDATE_PREFIX}{candidate_id}"),
            &CarrierContext::default(),
        )
    }

    fn resolve(&self, identity: &AgentIdentityId) -> Result<AgentIdentityId, CandidateError> {
        let resolved = self.registry.resolve(Some(identity))?;
        validate_identity_token(&resolved)?;
        Ok(resolved)
    }

    async fn read_raw(
        &self,
        resolved: &AgentIdentityId,
        candidate_id: &str,
    ) -> Result<Option<SoulCandidate>, CandidateError> {
        scalar("candidate_id", candidate_id)?;
        let key = Self::candidate_key(resolved, candidate_id);
        match self.backend.get_for_agent(&key, resolved.as_str()).await {
            Ok(Some(entry)) => decode_candidate(&entry.content).map(Some).inspect_err(|e| {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "error_key": "memory.soul_candidate_corrupt",
                            "identity": resolved.as_str(),
                            "candidate": candidate_id,
                            "err": e.to_string(),
                        })),
                    "stored soul candidate row failed to deserialize"
                );
            }),
            Ok(None) => Ok(None),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "error_key": "memory.soul_candidate_read_failed",
                            "identity": resolved.as_str(),
                            "candidate": candidate_id,
                            "err": e.to_string(),
                        })),
                    "soul candidate read failed through the memory backend"
                );
                Err(CandidateError::Soul(SoulError::Backend(e.to_string())))
            }
        }
    }

    async fn write_raw(
        &self,
        resolved: &AgentIdentityId,
        candidate: &SoulCandidate,
    ) -> Result<(), CandidateError> {
        candidate_bounds(candidate)?;
        let key = Self::candidate_key(resolved, &candidate.candidate_id);
        let content =
            serde_json::to_string(candidate).map_err(|e| CandidateError::Corrupt(e.to_string()))?;
        bound("encoded_candidate", content.len(), MAX_ENCODED_BYTES)?;
        if let Err(e) = self
            .backend
            .store_with_agent(
                &key,
                &content,
                // Custom("soul"), never plain Core: Soul rows must be
                // excludable from ambient recall by category/namespace,
                // not by key convention (see memory_inject).
                MemoryCategory::Custom("soul".to_string()),
                None,
                Some("soul"),
                None,
                Some(resolved.as_str()),
            )
            .await
        {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "error_key": "memory.soul_candidate_store_failed",
                        "identity": resolved.as_str(),
                        "candidate": candidate.candidate_id,
                        "err": e.to_string(),
                    })),
                "soul candidate store failed through the memory backend"
            );
            return Err(CandidateError::Soul(classify_backend_error(&e)));
        }
        Ok(())
    }

    /// Submit one intake event for one candidate of the resolved
    /// identity. Creates the candidate row if absent, otherwise folds
    /// the evidence into it under the bounded dedupe policy (one entry
    /// per distinct ref id; same id refreshes revision/verification in
    /// place; same id with opposite stance is a typed error). The
    /// returned/stored status is always `Candidate` — no amount of
    /// repetition, confidence, or verified evidence can make it
    /// anything else, because no other status exists here.
    pub async fn submit(
        &self,
        identity: &AgentIdentityId,
        intake: CandidateIntake,
    ) -> Result<SoulCandidate, CandidateError> {
        let resolved = self.resolve(identity)?;
        intake_bounds(&intake)?;
        validate_candidate_id(&intake.candidate_id)?;
        validate_disposition(&intake.disposition, &intake.proposed_rule)?;
        intake.evidence.validate(intake.origin)?;
        // Domain routing happens BEFORE any storage access:
        // user-preference-shaped evidence is rejected, not stored.
        if intake.domain == DomainClassification::UserPreference {
            return Err(CandidateError::UserModelDomain(intake.disposition.clone()));
        }

        let _guard = CANDIDATE_WRITE_LOCK.lock().await;
        let mut candidate = match self.read_raw(&resolved, &intake.candidate_id).await? {
            Some(existing) => existing,
            None => SoulCandidate::from_intake(&intake),
        };
        if candidate.status == CandidateStatus::Retracted {
            return Err(CandidateError::CandidateRetracted(intake.candidate_id));
        }
        candidate.merge_intake(intake)?;
        self.write_raw(&resolved, &candidate).await?;
        Ok(candidate)
    }

    /// Retract a candidate. Owner-correction-only (typed rejection for
    /// any other origin); the row REMAINS as history — this is not a
    /// reset or erase. Idempotent for an already-retracted candidate.
    pub async fn retract(
        &self,
        identity: &AgentIdentityId,
        candidate_id: &str,
        origin: CandidateOrigin,
    ) -> Result<SoulCandidate, CandidateError> {
        let resolved = self.resolve(identity)?;
        scalar("candidate_id", candidate_id)?;
        validate_candidate_id(candidate_id)?;
        if origin != CandidateOrigin::OwnerCorrection {
            return Err(CandidateError::RetractionRequiresOwnerCorrection);
        }
        let _guard = CANDIDATE_WRITE_LOCK.lock().await;
        let mut candidate = self
            .read_raw(&resolved, candidate_id)
            .await?
            .ok_or_else(|| CandidateError::NotFound(candidate_id.to_string()))?;
        if candidate.status == CandidateStatus::Retracted {
            return Ok(candidate);
        }
        candidate.status = CandidateStatus::Retracted;
        candidate.last_origin = CandidateOrigin::OwnerCorrection;
        self.write_raw(&resolved, &candidate).await?;
        Ok(candidate)
    }

    /// Read one candidate for the resolved identity. A candidate stored
    /// under a different identity is invisible (`None`) — the binding
    /// is enforced by the agent-scoped row, and a missing candidate is
    /// not an error.
    pub async fn get(
        &self,
        identity: &AgentIdentityId,
        candidate_id: &str,
    ) -> Result<Option<SoulCandidate>, CandidateError> {
        let resolved = self.resolve(identity)?;
        scalar("candidate_id", candidate_id)?;
        validate_candidate_id(candidate_id)?;
        self.read_raw(&resolved, candidate_id).await
    }

    /// List all candidates for the resolved identity, ordered by candidate id.
    /// This is an on-demand, complete walk over bounded backend pages. The
    /// namespace, agent id, and candidate key prefix are applied by the backend
    /// before every page boundary; no parallel index row is maintained.
    pub async fn candidates(
        &self,
        identity: &AgentIdentityId,
    ) -> Result<Vec<SoulCandidate>, CandidateError> {
        let resolved = self.resolve(identity)?;
        let prefix = Self::candidate_key(&resolved, "");
        let _guard = CANDIDATE_WRITE_LOCK.lock().await;
        let mut cursor: Option<String> = None;
        let mut candidates: Vec<SoulCandidate> = Vec::new();

        loop {
            let page = match self
                .backend
                .list_prefix_page(
                    crate::soul::SOUL_NAMESPACE,
                    resolved.as_str(),
                    &prefix,
                    cursor.as_deref(),
                    CANDIDATE_PAGE_SIZE,
                )
                .await
            {
                Ok(page) => page,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "error_key": "memory.soul_candidate_list_failed",
                                "identity": resolved.as_str(),
                                "err": e.to_string(),
                            })),
                        "soul candidate list failed through the memory backend"
                    );
                    return Err(CandidateError::Soul(SoulError::Backend(e.to_string())));
                }
            };

            if page.entries.len() > CANDIDATE_PAGE_SIZE {
                return Err(CandidateError::Soul(SoulError::Backend(
                    "memory backend exceeded the requested candidate page size".to_string(),
                )));
            }
            for row in page.entries {
                // Fail closed if a backend violates the dedicated query
                // contract. Post-filtering here would recreate the defect by
                // making an incomplete page look complete.
                if row.namespace != crate::soul::SOUL_NAMESPACE
                    || row.agent_id.as_deref() != Some(resolved.as_str())
                    || !row.key.starts_with(&prefix)
                {
                    return Err(CandidateError::Soul(SoulError::Backend(
                        "memory backend returned a row outside the candidate query scope"
                            .to_string(),
                    )));
                }
                candidates.push(decode_candidate(&row.content)?);
            }

            match page.next_cursor {
                Some(next) => {
                    if !next.starts_with(&prefix)
                        || cursor
                            .as_deref()
                            .is_some_and(|previous| next.as_str() <= previous)
                    {
                        return Err(CandidateError::Soul(SoulError::Backend(
                            "memory backend returned a non-advancing candidate cursor".to_string(),
                        )));
                    }
                    cursor = Some(next);
                }
                None => break,
            }
        }
        candidates.sort_by(|a, b| a.candidate_id.cmp(&b.candidate_id));
        Ok(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteMemory;

    fn fresh_backend() -> (tempfile::TempDir, Arc<SqliteMemory>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let mem = SqliteMemory::new("soul-candidate-test", tmp.path()).unwrap();
        (tmp, Arc::new(mem))
    }

    fn services(
        backend: &Arc<SqliteMemory>,
    ) -> (Arc<IdentityRegistry>, SoulService, SoulCandidateService) {
        let registry = Arc::new(IdentityRegistry::new());
        let soul = SoulService::new(
            Arc::clone(&registry) as Arc<IdentityRegistry>,
            Arc::clone(backend) as Arc<dyn Memory>,
        )
        .expect("sqlite backend must be accepted");
        let candidates = SoulCandidateService::new(
            Arc::clone(&registry) as Arc<IdentityRegistry>,
            Arc::clone(backend) as Arc<dyn Memory>,
        )
        .unwrap();
        (registry, soul, candidates)
    }

    /// Admit-through-backend identity (SqliteMemory rejects rows for
    /// unregistered agent ids), mirroring the soul seam test setup.
    async fn identity_for(backend: &Arc<SqliteMemory>, alias: &str) -> AgentIdentityId {
        AgentIdentityId::from_opaque(backend.ensure_agent_uuid(alias).await.unwrap())
    }

    fn intake(candidate_id: &str, evidence_id: &str) -> CandidateIntake {
        CandidateIntake {
            candidate_id: candidate_id.to_string(),
            domain: DomainClassification::SoulDisposition,
            disposition: "explanation density".to_string(),
            proposed_rule: "leads with the conclusion, then one example".to_string(),
            origin: CandidateOrigin::RepeatedBehavior,
            evidence: EvidenceRef {
                id: evidence_id.to_string(),
                stance: EvidenceStance::Supporting,
                verification: EvidenceVerification::Unverified,
                source_revision: Some("r1".to_string()),
                derived_from: None,
            },
            context_shapes: vec!["code-review".to_string()],
            outcome_refs: Vec::new(),
            confidence: Some("high".to_string()),
            sensitivity: Sensitivity::Internal,
        }
    }

    #[test]
    fn candidate_status_vocabulary_has_no_active_variant() {
        // Structural guard for the leaf's frozen rule: this exhaustive
        // match over every constructible CandidateStatus value stops
        // COMPILING the moment anyone adds a variant (Active,
        // Suspended, ...) — an implicit-promotion state would be
        // unrepresentable at this line first. With no active value in
        // the vocabulary, no method on SoulCandidateService can store,
        // return, or project one, whatever it is named.
        let mut seen = Vec::new();
        for status in [CandidateStatus::Candidate, CandidateStatus::Retracted] {
            let confined = match status {
                CandidateStatus::Candidate | CandidateStatus::Retracted => status,
            };
            seen.push(confined);
        }
        assert_eq!(seen.len(), 2);
    }

    #[tokio::test]
    async fn repeated_high_confidence_evidence_never_activates_soul() {
        // REQUIRED discrimination : submit the same
        // high-confidence candidate evidence 25 times for one identity.
        // Expected: candidate/evidence state grows only under the
        // bounded dedupe policy, active Soul disposition count is 0
        // (proven by scanning EVERY backend row), and unrelated Soul
        // state (the disposition row feeding persona projection)
        // is byte-identical afterwards.
        let (tmp, backend) = fresh_backend();
        let (registry, soul, candidates) = services(&backend);
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        // Pre-existing Soul state from the soul seam: must survive the
        // intake storm untouched (persona projection input unchanged).
        soul.store(
            &id,
            "disposition",
            "direct and warm",
            &CarrierContext::default(),
        )
        .await
        .unwrap();
        let disposition_key =
            SoulService::namespace_key(&id, "disposition", &CarrierContext::default());
        // Full serialized bytes of the disposition row BEFORE the intake
        // storm (same serde shape used for storage round-trips), so the
        // after-comparison below proves byte-identity, not just content
        // equality.
        let disposition_before = serde_json::to_string(
            &backend
                .get_for_agent(&disposition_key, id.as_str())
                .await
                .unwrap()
                .expect("disposition row must exist before the storm"),
        )
        .unwrap();

        for round in 0..25 {
            let mut event = intake("density", "bench-run-77");
            event.origin = CandidateOrigin::RepeatedBehavior;
            event.confidence = Some("high".to_string());
            let record = candidates.submit(&id, event).await.unwrap();
            assert_eq!(
                record.status,
                CandidateStatus::Candidate,
                "round {round}: high confidence and repetition must never leave candidate state"
            );
        }

        let stored = candidates
            .get(&id, "density")
            .await
            .unwrap()
            .expect("candidate must exist after 25 submissions");
        // Bounded dedupe: the SAME evidence ref id 25 times is ONE
        // entry and ONE observation — recurrence metadata only.
        assert_eq!(stored.evidence.len(), 1);
        assert_eq!(stored.recurrence.supporting_observations, 1);
        assert_eq!(stored.recurrence.confidence.as_deref(), Some("high"));

        // Active Soul disposition count -> 0: scan the reserved Soul
        // namespace through the namespaced opt-in channel (ambient list
        // structurally excludes it, and this module cannot write outside
        // it — the storage reservation refuses non-soul shapes).
        let rows = backend
            .recall_namespaced(crate::soul::SOUL_NAMESPACE, "*", 1000, None, None, None)
            .await
            .unwrap();
        let candidate_prefix = SoulCandidateService::candidate_key(&id, "");
        let mut candidate_rows = 0;
        let mut disposition_byte_compared = false;
        for row in &rows {
            assert!(
                row.agent_id.as_deref() == Some(id.as_str()),
                "no rows for foreign agents expected in this test backend"
            );
            if row.key.starts_with(&candidate_prefix) {
                candidate_rows += 1;
            } else {
                assert_eq!(
                    row.key, disposition_key,
                    "intake must not write any Soul state other than candidate rows"
                );
                // FULL serialized bytes before vs after: any field drift
                // (content, timestamps, attribution, metadata) breaks
                // the persona-projection input, not just content.
                let disposition_after = serde_json::to_string(
                    &backend
                        .get_for_agent(&disposition_key, id.as_str())
                        .await
                        .unwrap()
                        .expect("disposition row must survive"),
                )
                .unwrap();
                assert_eq!(
                    disposition_before, disposition_after,
                    "pre-existing Soul state must be byte-identical after 25 intakes"
                );
                disposition_byte_compared = true;
            }
        }
        assert!(
            disposition_byte_compared,
            "the disposition row must be present in the backend listing so the \
             full-byte comparison actually ran — its absence is a failure, not a pass"
        );
        assert_eq!(candidate_rows, 1, "dedupe keeps one candidate row");
        drop(tmp);
    }

    #[tokio::test]
    async fn intake_creates_candidate_bound_to_exactly_one_identity() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let a = identity_for(&backend, "identity-a").await;
        let b = identity_for(&backend, "identity-b").await;
        registry.admit(&a, "local bootstrap").unwrap();
        registry.admit(&b, "local bootstrap").unwrap();

        let record = candidates
            .submit(&a, intake("directness", "incident-1"))
            .await
            .unwrap();
        assert_eq!(record.status, CandidateStatus::Candidate);
        assert_eq!(record.sensitivity, Sensitivity::Internal);
        assert_eq!(record.disposition, "explanation density");
        assert_eq!(record.evidence.len(), 1);

        // Exactly one identity: B sees nothing of A's candidate...
        assert!(
            candidates.get(&b, "directness").await.unwrap().is_none(),
            "candidate binding must not leak across identities"
        );
        assert!(
            candidates.candidates(&b).await.unwrap().is_empty(),
            "identity B's listing must exclude A's candidate"
        );
        // ...and B submitting the same candidate id creates B's OWN
        // row, leaving A's untouched.
        let mut own = intake("directness", "incident-b");
        own.disposition = "directness".to_string();
        candidates.submit(&b, own).await.unwrap();
        let a_row = candidates.get(&a, "directness").await.unwrap().unwrap();
        assert_eq!(
            a_row.evidence.len(),
            1,
            "A's evidence must not gain B's ref"
        );
        assert_eq!(
            a_row.recurrence.supporting_observations, 1,
            "A's counts must not move from B's intake"
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn candidate_listing_is_complete_across_pages_and_large_distractor_sets() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let owner = identity_for(&backend, "candidate-owner").await;
        let sibling = identity_for(&backend, "candidate-sibling").await;
        registry.admit(&owner, "local bootstrap").unwrap();
        registry.admit(&sibling, "local bootstrap").unwrap();
        let prefix = SoulCandidateService::candidate_key(&owner, "");
        let candidate_time = "2026-09-19T00:00:00+00:00";
        let distractor_time = "2026-09-20T00:00:00+00:00";

        {
            let conn = backend.connection().lock();
            let tx = conn.unchecked_transaction().unwrap();
            let mut insert = tx
                .prepare(
                    "INSERT INTO memories \
                     (id, key, content, category, created_at, updated_at, namespace, agent_id) \
                     VALUES (?1, ?2, ?3, 'soul', ?4, ?4, ?5, ?6)",
                )
                .unwrap();

            // These rows are valid JSON so an accidentally admitted row is
            // observable as a wrong candidate, not masked by a decode error.
            for index in 0..1001 {
                let candidate = SoulCandidate::from_intake(&intake(
                    &format!("distractor-{index:04}"),
                    &format!("distractor-evidence-{index:04}"),
                ));
                let encoded = serde_json::to_string(&candidate).unwrap();
                insert
                    .execute(rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        format!("soul::{}::ordinary::{index:04}", owner.as_str()),
                        encoded,
                        distractor_time,
                        crate::soul::SOUL_NAMESPACE,
                        owner.as_str(),
                    ])
                    .unwrap();
                insert
                    .execute(rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        format!("{prefix}sibling-{index:04}"),
                        serde_json::to_string(&candidate).unwrap(),
                        distractor_time,
                        crate::soul::SOUL_NAMESPACE,
                        sibling.as_str(),
                    ])
                    .unwrap();
                insert
                    .execute(rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        format!("{prefix}ambient-{index:04}"),
                        serde_json::to_string(&candidate).unwrap(),
                        distractor_time,
                        "default",
                        owner.as_str(),
                    ])
                    .unwrap();
            }

            // 513 forces three backend pages at the production page size.
            for index in 0..513 {
                let candidate_id = format!("real-{index:04}");
                let candidate = SoulCandidate::from_intake(&intake(
                    &candidate_id,
                    &format!("real-evidence-{index:04}"),
                ));
                insert
                    .execute(rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        SoulCandidateService::candidate_key(&owner, &candidate_id),
                        serde_json::to_string(&candidate).unwrap(),
                        candidate_time,
                        crate::soul::SOUL_NAMESPACE,
                        owner.as_str(),
                    ])
                    .unwrap();
            }
            drop(insert);
            tx.commit().unwrap();
        }

        let listed = candidates.candidates(&owner).await.unwrap();
        assert_eq!(listed.len(), 513);
        assert_eq!(listed.first().unwrap().candidate_id, "real-0000");
        assert_eq!(listed.last().unwrap().candidate_id, "real-0512");
        assert!(
            listed
                .windows(2)
                .all(|pair| { pair[0].candidate_id < pair[1].candidate_id })
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn counterevidence_is_representable_without_deletion() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        candidates
            .submit(&id, intake("density", "bench-1"))
            .await
            .unwrap();
        let mut counter = intake("density", "incident-counter-9");
        counter.evidence.stance = EvidenceStance::Countering;
        let record = candidates.submit(&id, counter).await.unwrap();

        // Both stances coexist on one candidate; counterevidence
        // neither deletes the candidate nor promotes/demotes anything.
        assert_eq!(record.evidence.len(), 2);
        assert_eq!(record.recurrence.supporting_observations, 1);
        assert_eq!(record.recurrence.countering_observations, 1);
        assert_eq!(record.status, CandidateStatus::Candidate);
        let stored = candidates.get(&id, "density").await.unwrap().unwrap();
        assert_eq!(stored.evidence.len(), 2);

        // The same ref id with the OPPOSITE stance is a typed conflict,
        // never a silent stance flip.
        let mut flip = intake("density", "incident-counter-9");
        flip.evidence.stance = EvidenceStance::Supporting;
        assert_eq!(
            candidates.submit(&id, flip).await.unwrap_err(),
            CandidateError::ConflictingEvidenceStance("incident-counter-9".to_string())
        );
        drop(tmp);
    }

    /// Backend wrapper counting EVERY Memory-trait call (repairs-round
    /// item: the user-preference rejection must provably make ZERO
    /// backend calls — rejection is routing, not storage).
    struct CountingBackend {
        inner: Arc<SqliteMemory>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingBackend {
        fn hit(&self) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl ::zeroclaw_api::attribution::Attributable for CountingBackend {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            self.inner.role()
        }
        fn alias(&self) -> &str {
            self.inner.alias()
        }
    }

    #[async_trait::async_trait]
    impl Memory for CountingBackend {
        fn name(&self) -> &str {
            self.inner.name()
        }
        async fn store(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.hit();
            self.inner.store(key, content, category, session_id).await
        }
        async fn recall(
            &self,
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<zeroclaw_api::memory_traits::MemoryEntry>> {
            self.hit();
            self.inner
                .recall(query, limit, session_id, since, until)
                .await
        }
        async fn get(
            &self,
            key: &str,
        ) -> anyhow::Result<Option<zeroclaw_api::memory_traits::MemoryEntry>> {
            self.hit();
            self.inner.get(key).await
        }
        async fn get_for_agent(
            &self,
            key: &str,
            agent_id: &str,
        ) -> anyhow::Result<Option<zeroclaw_api::memory_traits::MemoryEntry>> {
            self.hit();
            self.inner.get_for_agent(key, agent_id).await
        }
        async fn list(
            &self,
            category: Option<&MemoryCategory>,
            session_id: Option<&str>,
        ) -> anyhow::Result<Vec<zeroclaw_api::memory_traits::MemoryEntry>> {
            self.hit();
            self.inner.list(category, session_id).await
        }
        async fn forget(&self, key: &str) -> anyhow::Result<bool> {
            self.hit();
            self.inner.forget(key).await
        }
        async fn forget_for_agent(&self, key: &str, agent_id: &str) -> anyhow::Result<bool> {
            self.hit();
            self.inner.forget_for_agent(key, agent_id).await
        }
        async fn count(&self) -> anyhow::Result<usize> {
            self.hit();
            self.inner.count().await
        }
        async fn health_check(&self) -> bool {
            self.hit();
            self.inner.health_check().await
        }
        async fn store_with_agent(
            &self,
            key: &str,
            content: &str,
            category: MemoryCategory,
            session_id: Option<&str>,
            namespace: Option<&str>,
            importance: Option<f64>,
            agent_id: Option<&str>,
        ) -> anyhow::Result<()> {
            self.hit();
            self.inner
                .store_with_agent(
                    key, content, category, session_id, namespace, importance, agent_id,
                )
                .await
        }
        async fn recall_for_agents(
            &self,
            allowed_agent_ids: &[&str],
            query: &str,
            limit: usize,
            session_id: Option<&str>,
            since: Option<&str>,
            until: Option<&str>,
        ) -> anyhow::Result<Vec<zeroclaw_api::memory_traits::MemoryEntry>> {
            self.hit();
            self.inner
                .recall_for_agents(allowed_agent_ids, query, limit, session_id, since, until)
                .await
        }
    }

    #[tokio::test]
    async fn user_preference_evidence_is_typed_rejected_routed_to_user_model() {
        let (tmp, backend) = fresh_backend();
        let registry = Arc::new(IdentityRegistry::new());
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counting = Arc::new(CountingBackend {
            inner: Arc::clone(&backend),
            calls: Arc::clone(&calls),
        });
        let candidates =
            SoulCandidateService::new(Arc::clone(&registry), counting as Arc<dyn Memory>).unwrap();
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        let mut preference = intake("vibe", "session-42");
        preference.domain = DomainClassification::UserPreference;
        preference.disposition = "user prefers concise replies".to_string();
        let before = calls.load(std::sync::atomic::Ordering::SeqCst);
        let err = candidates.submit(&id, preference).await.unwrap_err();
        assert!(matches!(err, CandidateError::UserModelDomain(_)));
        assert!(
            err.to_string().contains("UserModel"),
            "the rejection must name the UserModel domain for routing: {err}"
        );

        // ZERO backend calls across the rejection: domain routing
        // happens entirely before storage access — not even a read.
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            before,
            "user-preference rejection must make zero backend calls"
        );

        // Rejected means NOT stored: no row exists for that candidate.
        assert!(
            candidates.get(&id, "vibe").await.unwrap().is_none(),
            "user-preference evidence must not be silently stored as Soul"
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn unadmitted_or_revoked_identity_fails_closed_on_intake() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);

        // Never-admitted identity: typed failure, no fallback alias.
        let ghost = AgentIdentityId::from_opaque("never-admitted");
        assert_eq!(
            candidates
                .submit(&ghost, intake("density", "bench-1"))
                .await
                .unwrap_err(),
            CandidateError::Soul(SoulError::IdentityUnavailable),
            "unadmitted identity must fail closed on intake"
        );
        assert_eq!(
            candidates.get(&ghost, "density").await.unwrap_err(),
            CandidateError::Soul(SoulError::IdentityUnavailable),
            "unadmitted identity must fail closed on query"
        );

        // Admitted-then-revoked identity: keeps its typed revoked error.
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();
        registry.revoke(&id).unwrap();
        assert_eq!(
            candidates
                .submit(&id, intake("density", "bench-1"))
                .await
                .unwrap_err(),
            CandidateError::Soul(SoulError::IdentityRevoked(id.as_str().to_string())),
            "revoked identity must fail closed on intake"
        );
        assert_eq!(
            candidates
                .retract(&id, "density", CandidateOrigin::OwnerCorrection)
                .await
                .unwrap_err(),
            CandidateError::Soul(SoulError::IdentityRevoked(id.as_str().to_string())),
            "revoked identity must fail closed on retract"
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn malformed_ids_and_fields_are_rejected_not_namespaced() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        // A delimiter inside a candidate id would let (id="a::b")
        // collide with foreign key space — rejected before storage.
        let mut delimited = intake("density", "bench-1");
        delimited.candidate_id = "density::escape".to_string();
        assert_eq!(
            candidates.submit(&id, delimited).await.unwrap_err(),
            CandidateError::InvalidCandidateId("density::escape".to_string())
        );
        let mut empty = intake("density", "bench-1");
        empty.candidate_id = "   ".to_string();
        assert_eq!(
            candidates.submit(&id, empty).await.unwrap_err(),
            CandidateError::InvalidCandidateId("(empty)".to_string())
        );
        let mut no_rule = intake("density", "bench-1");
        no_rule.proposed_rule = "  ".to_string();
        assert!(matches!(
            candidates.submit(&id, no_rule).await.unwrap_err(),
            CandidateError::InvalidDisposition(_)
        ));
        let mut no_ref = intake("density", "bench-1");
        no_ref.evidence.id = String::new();
        assert_eq!(
            candidates.submit(&id, no_ref).await.unwrap_err(),
            CandidateError::InvalidEvidenceRef("(empty)".to_string())
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn duplicate_evidence_dedupes_by_reference_id() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        candidates
            .submit(&id, intake("density", "bench-1"))
            .await
            .unwrap();
        // Same ref id resubmitted with a NEWER source revision: refresh
        // in place, no duplicate entry, no count inflation.
        let mut refresh = intake("density", "bench-1");
        refresh.evidence.source_revision = Some("r2".to_string());
        refresh.evidence.derived_from = Some("agg-77".to_string());
        refresh.context_shapes = vec!["incident-triage".to_string()];
        let record = candidates.submit(&id, refresh).await.unwrap();
        assert_eq!(record.evidence.len(), 1);
        assert_eq!(record.evidence[0].source_revision.as_deref(), Some("r2"));
        assert_eq!(record.evidence[0].derived_from.as_deref(), Some("agg-77"));
        assert_eq!(record.recurrence.supporting_observations, 1);
        assert_eq!(
            record.context_shapes,
            vec!["code-review".to_string(), "incident-triage".to_string()],
            "context diversity accumulates deduped"
        );

        // A refresh supplying None for revision/derivation KEEPS the
        // retained values (merge semantics — refreshes never lose
        // provenance, repairs-round item 6).
        let mut thin_refresh = intake("density", "bench-1");
        thin_refresh.evidence.source_revision = None;
        thin_refresh.evidence.derived_from = None;
        let record = candidates.submit(&id, thin_refresh).await.unwrap();
        assert_eq!(record.evidence.len(), 1);
        assert_eq!(
            record.evidence[0].source_revision.as_deref(),
            Some("r2"),
            "a None revision on refresh must keep the retained revision"
        );
        assert_eq!(
            record.evidence[0].derived_from.as_deref(),
            Some("agg-77"),
            "a None derivation on refresh must keep the retained derivation"
        );

        // A DISTINCT ref id is a real new observation.
        let record = candidates
            .submit(&id, intake("density", "bench-2"))
            .await
            .unwrap();
        assert_eq!(record.evidence.len(), 2);
        assert_eq!(record.recurrence.supporting_observations, 2);
        drop(tmp);
    }

    #[tokio::test]
    async fn tachi_verified_evidence_supports_but_never_activates() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        let mut verified = intake("warmth", "tachi-ev-1");
        verified.origin = CandidateOrigin::TachiVerifiedEvidence;
        verified.evidence.verification = EvidenceVerification::Verified;
        let record = candidates.submit(&id, verified).await.unwrap();
        assert_eq!(record.status, CandidateStatus::Candidate);

        // Consistency rule (repairs-round item 3): the TachiVerified
        // origin MUST carry a Verified ref; unverified/unavailable refs
        // claiming it are typed-rejected. Verified refs from OTHER
        // origins (worker receipts etc.) stay allowed — proven by the
        // Unverified fixture this test's baseline intake already used.
        let mut unverified_claim = intake("warmth", "tachi-ev-0");
        unverified_claim.origin = CandidateOrigin::TachiVerifiedEvidence;
        unverified_claim.evidence.verification = EvidenceVerification::Unverified;
        assert!(matches!(
            candidates.submit(&id, unverified_claim).await.unwrap_err(),
            CandidateError::InvalidEvidenceRef(_)
        ));

        // Even verified evidence, repeated, stays candidate-only.
        for _ in 0..5 {
            let mut again = intake("warmth", "tachi-ev-1");
            again.origin = CandidateOrigin::TachiVerifiedEvidence;
            again.evidence.verification = EvidenceVerification::Verified;
            again.confidence = Some("high".to_string());
            assert_eq!(
                candidates.submit(&id, again).await.unwrap().status,
                CandidateStatus::Candidate
            );
        }
        let stored = candidates.get(&id, "warmth").await.unwrap().unwrap();
        assert_eq!(stored.status, CandidateStatus::Candidate);
        assert_eq!(stored.evidence.len(), 1);

        // Tachi-unavailable external refs are marked unavailable, never
        // fabricated as verified (per the leaf's failure model).
        let mut unreachable = intake("warmth", "tachi-ev-2");
        unreachable.evidence.verification = EvidenceVerification::Unavailable;
        let record = candidates.submit(&id, unreachable).await.unwrap();
        assert_eq!(
            record
                .evidence
                .iter()
                .find(|r| r.id == "tachi-ev-2")
                .unwrap()
                .verification,
            EvidenceVerification::Unavailable
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn retraction_is_owner_correction_only_and_keeps_history() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        candidates
            .submit(&id, intake("density", "bench-1"))
            .await
            .unwrap();

        // Non-owner origins cannot retract.
        assert_eq!(
            candidates
                .retract(&id, "density", CandidateOrigin::ModelSummary)
                .await
                .unwrap_err(),
            CandidateError::RetractionRequiresOwnerCorrection
        );
        // Unknown candidate id retracts nothing.
        assert_eq!(
            candidates
                .retract(&id, "ghost", CandidateOrigin::OwnerCorrection)
                .await
                .unwrap_err(),
            CandidateError::NotFound("ghost".to_string())
        );

        let retracted = candidates
            .retract(&id, "density", CandidateOrigin::OwnerCorrection)
            .await
            .unwrap();
        assert_eq!(retracted.status, CandidateStatus::Retracted);
        assert_eq!(retracted.last_origin, CandidateOrigin::OwnerCorrection);

        // History is kept: the row remains readable...
        let stored = candidates.get(&id, "density").await.unwrap().unwrap();
        assert_eq!(stored.status, CandidateStatus::Retracted);
        assert_eq!(stored.evidence.len(), 1);
        // ...re-retraction is idempotent...
        assert_eq!(
            candidates
                .retract(&id, "density", CandidateOrigin::OwnerCorrection)
                .await
                .unwrap()
                .status,
            CandidateStatus::Retracted
        );
        // ...and a retracted candidate takes no further evidence.
        assert_eq!(
            candidates
                .submit(&id, intake("density", "bench-2"))
                .await
                .unwrap_err(),
            CandidateError::CandidateRetracted("density".to_string())
        );
        drop(tmp);
    }

    #[tokio::test]
    async fn owner_correction_amends_rule_text_other_origins_do_not() {
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        candidates
            .submit(&id, intake("density", "bench-1"))
            .await
            .unwrap();
        // A non-owner origin proposing a new rule text changes only
        // evidence, never the recorded proposal.
        let mut rewrite = intake("density", "bench-2");
        rewrite.proposed_rule = "rewritten by an incident".to_string();
        let record = candidates.submit(&id, rewrite).await.unwrap();
        assert_eq!(
            record.proposed_rule, "leads with the conclusion, then one example",
            "only an owner correction may amend the proposed rule"
        );

        let mut correction = intake("density", "bench-3");
        correction.origin = CandidateOrigin::OwnerCorrection;
        correction.proposed_rule =
            "corrected: conclusion first, example only on request".to_string();
        let record = candidates.submit(&id, correction).await.unwrap();
        assert_eq!(
            record.proposed_rule,
            "corrected: conclusion first, example only on request"
        );
        assert_eq!(record.last_origin, CandidateOrigin::OwnerCorrection);
        drop(tmp);
    }

    #[tokio::test]
    async fn concurrent_intakes_from_two_service_instances_lose_no_evidence() {
        // Repairs-round item 4: the write lock is PROCESS-WIDE, not
        // per-instance. Two SoulCandidateService instances over one
        // backend submitting different evidence refs for the same
        // candidate concurrently must both survive — a per-instance
        // lock would let their read-modify-write sequences interleave
        // and silently drop one ref.
        let (tmp, backend) = fresh_backend();
        let registry = Arc::new(IdentityRegistry::new());
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        let service_a = SoulCandidateService::new(
            Arc::clone(&registry),
            Arc::clone(&backend) as Arc<dyn Memory>,
        )
        .unwrap();
        let service_b = SoulCandidateService::new(
            Arc::clone(&registry),
            Arc::clone(&backend) as Arc<dyn Memory>,
        )
        .unwrap();

        let a_id = id.clone();
        let b_id = id.clone();
        let (a_res, b_res) = tokio::join!(
            service_a.submit(&a_id, intake("density", "bench-1")),
            service_b.submit(&b_id, intake("density", "bench-2")),
        );
        a_res.unwrap();
        b_res.unwrap();

        let stored = service_a.candidates(&id).await.unwrap();
        assert_eq!(stored.len(), 1);
        let record = &stored[0];
        let mut ids: Vec<&str> = record.evidence.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec!["bench-1", "bench-2"],
            "both concurrent intakes must be reflected — no evidence loss"
        );
        assert_eq!(record.recurrence.supporting_observations, 2);
        drop(tmp);
    }

    #[tokio::test]
    async fn verified_evidence_from_non_tachi_origins_is_accepted() {
        // Consistency rule is one-directional: Tachi origin REQUIRES
        // Verified, but Verified is legitimately attestable by other
        // origins (e.g. a worker receipt from an observed run).
        let (tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "identity-a").await;
        registry.admit(&id, "local bootstrap").unwrap();

        let mut verified = intake("warmth", "worker-ev-1");
        verified.origin = CandidateOrigin::WorkerReceipt;
        verified.evidence.verification = EvidenceVerification::Verified;
        let record = candidates.submit(&id, verified).await.unwrap();
        assert_eq!(
            record.evidence[0].verification,
            EvidenceVerification::Verified,
            "non-Tachi verified evidence must be accepted as-is"
        );
        assert_eq!(record.status, CandidateStatus::Candidate);
        drop(tmp);
    }

    #[tokio::test]
    async fn markdown_backend_is_refused_at_construction() {
        // Repairs-round item 5: the markdown backend synthesizes keys
        // and wraps rows in scaffolding, so raw-key candidate JSON
        // corrupts on read. Refused with a typed error, never used.
        let tmp = tempfile::TempDir::new().unwrap();
        let markdown = Arc::new(crate::markdown::MarkdownMemory::new(
            "soul-candidate-test",
            tmp.path(),
        ));
        let registry = Arc::new(IdentityRegistry::new());
        let err =
            match SoulCandidateService::new(Arc::clone(&registry), markdown as Arc<dyn Memory>) {
                Err(err) => err,
                Ok(_) => panic!("markdown backend must be refused at construction"),
            };
        assert_eq!(
            err,
            CandidateError::UnsupportedBackend("markdown".to_string())
        );
        assert!(
            err.to_string().contains("round-trip"),
            "the refusal must say why: {err}"
        );
    }

    #[test]
    fn candidate_key_is_identity_bound_and_carrier_free() {
        let a = AgentIdentityId::from_opaque("identity-a");
        let b = AgentIdentityId::from_opaque("identity-b");
        assert_eq!(
            SoulCandidateService::candidate_key(&a, "density"),
            "soul::identity-a::candidate::density"
        );
        assert_ne!(
            SoulCandidateService::candidate_key(&a, "density"),
            SoulCandidateService::candidate_key(&b, "density")
        );
    }

    #[test]
    fn candidate_record_round_trips_through_json() {
        // The stored row shape must survive serialization: sensitivity
        // classification, stance, verification, and derivation refs are
        // all retained for later invalidation handling.
        let candidate = SoulCandidate::from_intake(&intake("density", "bench-1"));
        let json = serde_json::to_string(&candidate).unwrap();
        let back: SoulCandidate = serde_json::from_str(&json).unwrap();
        assert_eq!(back, candidate);
        assert!(json.contains("\"sensitivity\":\"internal\""));
        assert!(json.contains("\"stance\":\"supporting\""));
    }

    #[tokio::test]
    async fn accepted_intakes_persist_monotone_sensitivity() {
        let (_tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "sensitivity-owner").await;
        registry.admit(&id, "local bootstrap").unwrap();
        use Sensitivity::{Internal, Public, Sensitive};
        let cases = [
            (Public, Public, Public),
            (Public, Internal, Internal),
            (Public, Sensitive, Sensitive),
            (Internal, Public, Internal),
            (Internal, Internal, Internal),
            (Internal, Sensitive, Sensitive),
            (Sensitive, Public, Sensitive),
            (Sensitive, Internal, Sensitive),
            (Sensitive, Sensitive, Sensitive),
        ];
        for (index, (initial, incoming, expected)) in cases.into_iter().enumerate() {
            for refresh in [false, true] {
                for origin in [
                    CandidateOrigin::RepeatedBehavior,
                    CandidateOrigin::OwnerCorrection,
                ] {
                    let candidate_id = format!("sensitivity-{index}-{refresh}-{origin:?}");
                    let mut first = intake(&candidate_id, "original-evidence");
                    first.sensitivity = initial;
                    let created = candidates.submit(&id, first).await.unwrap();
                    assert_eq!(created.sensitivity, initial);
                    let evidence_id = if refresh {
                        "original-evidence"
                    } else {
                        "new-evidence"
                    };
                    let mut next = intake(&candidate_id, evidence_id);
                    next.sensitivity = incoming;
                    next.origin = origin;
                    let submitted = candidates.submit(&id, next).await.unwrap();
                    let persisted = candidates.get(&id, &candidate_id).await.unwrap().unwrap();
                    eprintln!(
                        "CANDIDATE_SENSITIVITY initial={initial:?} incoming={incoming:?} refresh={refresh} origin={origin:?} persisted={:?}",
                        persisted.sensitivity
                    );
                    assert_eq!(persisted, submitted);
                    assert_eq!(persisted.sensitivity, expected);
                    assert_eq!(persisted.status, CandidateStatus::Candidate);
                    assert_eq!(persisted.evidence.len(), if refresh { 1 } else { 2 });
                }
            }
        }
    }

    #[tokio::test]
    async fn rejected_sensitive_intake_preserves_stored_candidate_bytes() {
        let (_tmp, backend) = fresh_backend();
        let (registry, _soul, candidates) = services(&backend);
        let id = identity_for(&backend, "rejected-sensitivity-owner").await;
        registry.admit(&id, "local bootstrap").unwrap();
        let mut first = intake("density", "same-evidence");
        first.sensitivity = Sensitivity::Public;
        candidates.submit(&id, first).await.unwrap();
        let key = SoulCandidateService::candidate_key(&id, "density");
        let before = serde_json::to_string(
            &backend
                .get_for_agent(&key, id.as_str())
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let mut rejected = intake("density", "same-evidence");
        rejected.sensitivity = Sensitivity::Sensitive;
        rejected.evidence.stance = EvidenceStance::Countering;
        assert_eq!(
            candidates.submit(&id, rejected).await.unwrap_err(),
            CandidateError::ConflictingEvidenceStance("same-evidence".to_string())
        );
        let after = serde_json::to_string(
            &backend
                .get_for_agent(&key, id.as_str())
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(after, before);
    }
    async fn seed_bounded_fixture(
        backend: &SqliteMemory,
        id: &AgentIdentityId,
        candidate: &SoulCandidate,
    ) {
        backend
            .store_with_agent(
                &SoulCandidateService::candidate_key(id, &candidate.candidate_id),
                &serde_json::to_string(candidate).unwrap(),
                MemoryCategory::Custom("soul".into()),
                None,
                Some("soul"),
                None,
                Some(id.as_str()),
            )
            .await
            .unwrap();
    }

    async fn bounded_fixture_bytes(
        backend: &SqliteMemory,
        id: &AgentIdentityId,
        key: &str,
    ) -> String {
        backend
            .get_for_agent(&SoulCandidateService::candidate_key(id, key), id.as_str())
            .await
            .unwrap()
            .unwrap()
            .content
    }

    #[tokio::test]
    async fn candidate_bounds_scalar_rule_and_raw_vectors() {
        let (_tmp, backend) = fresh_backend();
        let (registry, _, service) = services(&backend);
        let id = identity_for(&backend, "bounds").await;
        registry.admit(&id, "fixture").unwrap();
        for (name, value) in [
            ("ascii", "x".repeat(MAX_SCALAR_BYTES)),
            ("utf8", "é".repeat(MAX_SCALAR_BYTES / 2)),
        ] {
            let mut input = intake(name, "e");
            input.disposition = value;
            input.proposed_rule = "r".repeat(MAX_RULE_BYTES);
            input.context_shapes = vec!["same".into(); MAX_TAGS];
            input.outcome_refs = vec!["same".into(); MAX_TAGS];
            service.submit(&id, input.clone()).await.unwrap();
            let before = bounded_fixture_bytes(&backend, &id, name).await;
            let mut over = input.clone();
            over.disposition.push('x');
            assert!(matches!(
                service.submit(&id, over).await,
                Err(CandidateError::LimitExceeded {
                    field: "disposition",
                    ..
                })
            ));
            let mut over = input.clone();
            over.proposed_rule.push('r');
            assert!(matches!(
                service.submit(&id, over).await,
                Err(CandidateError::LimitExceeded {
                    field: "proposed_rule",
                    ..
                })
            ));
            let mut over = input.clone();
            over.context_shapes.push("same".into());
            assert!(matches!(
                service.submit(&id, over).await,
                Err(CandidateError::LimitExceeded {
                    field: "context_shapes",
                    ..
                })
            ));
            let mut over = input;
            over.outcome_refs.push("same".into());
            assert!(matches!(
                service.submit(&id, over).await,
                Err(CandidateError::LimitExceeded {
                    field: "outcome_refs",
                    ..
                })
            ));
            assert_eq!(bounded_fixture_bytes(&backend, &id, name).await, before);
        }
    }

    #[tokio::test]
    async fn candidate_bounds_aggregate_vectors_and_evidence_preserve_raw_row() {
        let (_tmp, backend) = fresh_backend();
        let (registry, _, service) = services(&backend);
        let id = identity_for(&backend, "bounds").await;
        registry.admit(&id, "fixture").unwrap();
        let mut seed = SoulCandidate::from_intake(&intake("aggregate", "e0"));
        seed.context_shapes = (0..MAX_TAGS - 1).map(|i| format!("context-{i}")).collect();
        seed.recurrence.outcome_refs = (0..MAX_TAGS - 1).map(|i| format!("outcome-{i}")).collect();
        seed.evidence = (0..MAX_EVIDENCE)
            .map(|i| intake("aggregate", &format!("e{i}")).evidence)
            .collect();
        seed.recurrence.recount(&seed.evidence);
        seed_bounded_fixture(&backend, &id, &seed).await;
        let mut refresh = intake("aggregate", "e0");
        refresh.context_shapes = vec![format!("context-{}", MAX_TAGS - 1)];
        refresh.outcome_refs = vec![format!("outcome-{}", MAX_TAGS - 1)];
        let record = service.submit(&id, refresh.clone()).await.unwrap();
        assert_eq!(record.evidence.len(), MAX_EVIDENCE);
        assert_eq!(record.context_shapes.len(), MAX_TAGS);
        assert_eq!(record.recurrence.outcome_refs.len(), MAX_TAGS);
        let before = bounded_fixture_bytes(&backend, &id, "aggregate").await;
        for stance in [EvidenceStance::Supporting, EvidenceStance::Countering] {
            let mut over = refresh.clone();
            over.evidence.id = "new".into();
            over.evidence.stance = stance;
            over.confidence = Some("changed".into());
            assert!(matches!(
                service.submit(&id, over).await,
                Err(CandidateError::LimitExceeded {
                    field: "evidence",
                    ..
                })
            ));
            assert_eq!(
                bounded_fixture_bytes(&backend, &id, "aggregate").await,
                before
            );
        }
        for context in [true, false] {
            let mut over = refresh.clone();
            if context {
                over.context_shapes.push("new-context".into());
            } else {
                over.outcome_refs.push("new-outcome".into());
            }
            assert!(matches!(
                service.submit(&id, over).await,
                Err(CandidateError::LimitExceeded { .. })
            ));
            assert_eq!(
                bounded_fixture_bytes(&backend, &id, "aggregate").await,
                before
            );
        }
        assert_eq!(
            service.get(&id, "aggregate").await.unwrap().unwrap(),
            record
        );
    }

    #[tokio::test]
    async fn candidate_bounds_encoded_escaping_and_legacy_read_refusal() {
        let (_tmp, backend) = fresh_backend();
        let (registry, _, service) = services(&backend);
        let id = identity_for(&backend, "bounds").await;
        registry.admit(&id, "fixture").unwrap();
        // Each string is valid; escape expansion alone exceeds the row cap.
        let mut input = intake("encoded", "e");
        input.context_shapes = (0..16)
            .map(|i| format!("{i}{}", "\u{0001}".repeat(3000)))
            .collect();
        let mut control = input.clone();
        control.context_shapes = (0..16)
            .map(|i| format!("{i}{}", "x".repeat(3000)))
            .collect();
        service.submit(&id, control).await.unwrap();
        let before = bounded_fixture_bytes(&backend, &id, "encoded").await;
        assert!(matches!(
            service.submit(&id, input.clone()).await,
            Err(CandidateError::LimitExceeded {
                field: "encoded_candidate",
                ..
            })
        ));
        assert_eq!(
            bounded_fixture_bytes(&backend, &id, "encoded").await,
            before
        );
        let legacy = SoulCandidate::from_intake(&input);
        seed_bounded_fixture(&backend, &id, &legacy).await;
        let oversized = bounded_fixture_bytes(&backend, &id, "encoded").await;
        assert!(oversized.len() > MAX_ENCODED_BYTES);
        assert!(matches!(
            service.get(&id, "encoded").await,
            Err(CandidateError::LimitExceeded {
                field: "encoded_candidate",
                ..
            })
        ));
        assert!(matches!(
            service.candidates(&id).await,
            Err(CandidateError::LimitExceeded {
                field: "encoded_candidate",
                ..
            })
        ));
        assert!(matches!(
            service.submit(&id, intake("encoded", "new")).await,
            Err(CandidateError::LimitExceeded {
                field: "encoded_candidate",
                ..
            })
        ));
        assert_eq!(
            bounded_fixture_bytes(&backend, &id, "encoded").await,
            oversized
        );
        // A small encoded row can still exceed the evidence-count bound.
        let mut legacy = SoulCandidate::from_intake(&intake("encoded", "e"));
        legacy.evidence = vec![legacy.evidence[0].clone(); MAX_EVIDENCE + 1];
        seed_bounded_fixture(&backend, &id, &legacy).await;
        assert!(matches!(
            service.get(&id, "encoded").await,
            Err(CandidateError::LimitExceeded {
                field: "evidence",
                ..
            })
        ));
    }
}
