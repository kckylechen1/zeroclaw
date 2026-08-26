//! `TaskIntentV1` — the ZeroClaw ENCODER half of the frozen host semantic
//! wire (vertical V2b; contract law TB-3; the host decoder half is the
//! tachi TaskIntent bridge, vertical V2a).
//!
//! Both halves are pinned to ONE golden vector,
//! [`GOLDEN_TASK_INTENT_V1`] (`golden/task-intent.v1.json`, copied
//! byte-identical from tachi `crates/tachi-params/src/taskintent/golden/`
//! at tachi `origin/main` = `b073882929f`): a nested-type or
//! serde-rename change on either side fails its golden test (TB-3
//! cross-repo golden pair).
//!
//! Field freeze (exactly these seventeen top-level fields, TB-3):
//!
//! ```text
//! objective, capability_request, requester, parent_ref, supervisor_ref,
//! context_bundle_ref, source_refs, constraints, expected_artifacts,
//! evaluation_requirement, workspace_source, routing_preference,
//! approval_requirement, privacy_class, expiry, retry_of
//! ```
//! (plus `schema` — the version tag, see [`SCHEMA_TAG`]).
//!
//! Watershed discrimination, enforced structurally on this side:
//!
//! - **No execution detail is representable as a FIELD** (TB-1/TB-4).
//!   There is no `command`/`env`/`cwd`/`path`/`model`/`backend`-shaped
//!   field at any depth, and every struct on the wire is
//!   `deny_unknown_fields`, so a smuggled field fails deserialization
//!   instead of being silently carried. Every free-text value is a
//!   bounded [`BoundedText`]; every selector is a closed enum.
//! - **`Capability` is a closed enum** (DECISION TB-5/A, carrier picked
//!   as option (a) by the tachi host bridge; this leaf admits
//!   [`Capability::RepositoryImplementation`] into the catalog per
//!   the watershed ticket). No variant can name a vendor, model, CLI, or tool:
//!   `glm`, `codex`, CLI flags, and friends are not representable in
//!   `capability_request` — they fail enum deserialization.
//! - **Content-level rejection is per the five frozen TB-4 categories**
//!   (credential-shaped, command-lead-token, worktree-path,
//!   private-dyad, caller-minted-ref), mirrored marker-for-marker from
//!   the tachi host admission law so the two sides cannot drift. This
//!   scan covers every `BoundedText` field of the intent; the typed refs
//!   are namespace- and length-enforced instead (they carry no free
//!   text). The scan is category-law, not a prose ban: text that merely
//!   MENTIONS a vendor inside an otherwise-clean objective passes THIS
//!   mirrored scan (and the host-side law it mirrors) — the structural
//!   bans above are what make placement impossible. The V2b CLIENT adds
//!   a deliberate strict superset on top
//!   (`zeroclaw-runtime`'s `tachi_bridge::compose` watershed layer):
//!   vendor/model/worktree/cwd/tmux-SSH/sandbox/CLI-flag PROSE is
//!   rejected at compose time before anything is sent. The client may
//!   reject more than the mirrored law; it may never reject less.
//! - **`TaskRef`/`AttemptRef` are decode-only here** (TB-6): Tachi mints
//!   them; the ZeroClaw side has NO public constructor, no `mint`, and
//!   no `From<String>` — there is no constructor API, so no code path
//!   MINTS a ref value; the only way one enters this process is by
//!   DESERIALIZING a wire value (that is how transport receipts
//!   arrive). A value hand-written into raw JSON therefore still
//!   enters as a decoded value — it is not authority: the host
//!   validates retry lineage at admission and rejects foreign ids, and
//!   ZeroClaw treats refs it did not receive from a receipt as
//!   untrusted input.
//! - **`ParentRunRef`/`SubAgentRunRef` constructors force their own
//!   namespace** (`parent:`/`subrun:`) and length-cap the body exactly
//!   like the decode path: ZeroClaw names its own run lineage but can
//!   never fabricate a `task:`/`attempt:` value through them.
//!
//! Digest: [`TaskIntentV1::canonical_digest`] implements the identical
//! rule to tachi's `memcore::canonical_digest::canonical_json_digest_hex`
//! (recursively key-sorted canonical JSON, SHA-256, lower hex, no prefix)
//! over `{"schema": tag, "intent": payload}`. The golden pins the sample
//! digest `84ab2316…ce3a31`.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

/// Version tag carried on every `TaskIntentV1` wire payload (frozen by the
/// `task-intent.v1` golden).
pub const SCHEMA_TAG: &str = "task-intent.v1";

/// Hard cap for any single text-bearing wire value (TB-4: an unbounded
/// transcript cannot be represented on this wire at all).
pub const BOUNDED_TEXT_MAX: usize = 4_096;

/// The digest pinned by the checked-in golden vector (`84ab2316…ce3a31`).
/// The encoder golden test asserts [`TaskIntentV1::canonical_digest`]
/// equals exactly this constant — drift anywhere in the rule, the field
/// set, or a serde name breaks the pin.
pub const GOLDEN_DIGEST_SHA256: &str =
    "84ab23166aba2fd25c4c98d9f23bb52a81ffbdd82d99653bb0758ae840ce3a31";

/// The checked-in golden vector pinning the `task-intent.v1` wire (TB-3
/// cross-repo golden pair; byte-identical to the tachi-side copy).
pub const GOLDEN_TASK_INTENT_V1: &str = include_str!("taskintent/golden/task-intent.v1.json");

/// Byte cap for the BODY of a ref wire value (the value minus its
/// namespace prefix). Refs are opaque bounded identifiers, never content
/// carriers (TB-4 forbids oversized payloads); the full wire value may
/// exceed this constant by the prefix length.
pub const REF_VALUE_MAX: usize = 256;

// ─────────────────────────────────────────────────────────────────────────
// Bounded scalar wire values
// ─────────────────────────────────────────────────────────────────────────

/// A bounded text value. Construction validates length; the encode-side
/// admission scan (see `zeroclaw-runtime::tachi_bridge`) additionally
/// rejects forbidden content per TB-4 category.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BoundedText(String);

impl BoundedText {
    /// Construct a bounded text, rejecting oversize values.
    pub fn new(value: impl Into<String>) -> Result<Self, WireError> {
        let value = value.into();
        if value.len() > BOUNDED_TEXT_MAX {
            return Err(WireError::TextTooLong { len: value.len() });
        }
        Ok(Self(value))
    }

    /// The text content.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<BoundedText> for String {
    fn from(value: BoundedText) -> Self {
        value.0
    }
}

/// RFC3339 timestamp on the wire (e.g. task expiry). Serialization is the
/// chrono `to_rfc3339` form (`2026-12-01T00:00:00+00:00`) — byte-identical
/// to the tachi decoder side, which the golden pins.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    /// Parse an RFC3339 timestamp.
    pub fn parse(value: &str) -> Result<Self, WireError> {
        Ok(Self(
            DateTime::parse_from_rfc3339(value)
                .map_err(WireError::BadTimestamp)?
                .with_timezone(&Utc),
        ))
    }
}

impl From<Timestamp> for String {
    fn from(value: Timestamp) -> Self {
        value.0.to_rfc3339()
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Typed identity vocabulary (TB-14 / TB-6 client half)
// ─────────────────────────────────────────────────────────────────────────

/// Generates one namespaced, **decode-only** ref newtype for refs ZeroClaw
/// receives but never mints: wire decode validates the namespace prefix,
/// and there is deliberately NO public constructor, no `mint`, and no
/// `From<String>` (TB-6: `TaskRef` is minted by Tachi only; a
/// caller-minted value is unrepresentable on this side).
macro_rules! decode_only_ref {
    ($(#[$meta:meta])* $name:ident, $prefix:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        pub struct $name(String);

        impl $name {
            /// Wire namespace prefix (frozen by the `task-intent.v1`
            /// golden).
            pub const WIRE_PREFIX: &'static str = $prefix;

            /// The namespaced wire form, e.g. `"task:…"`.
            pub fn as_wire(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                let prefix = Self::WIRE_PREFIX;
                let body = raw.strip_prefix(prefix).ok_or_else(|| {
                    D::Error::custom(concat!(
                        stringify!($name),
                        " wire value must use its own namespace prefix",
                    ))
                })?;
                if body.is_empty() || body.len() > REF_VALUE_MAX {
                    return Err(D::Error::custom(concat!(
                        stringify!($name),
                        " wire body must be 1..=256 bytes",
                    )));
                }
                Ok(Self(raw))
            }
        }
    };
}

decode_only_ref!(
    /// A Tachi-minted durable task identity (TB-6: minted by Tachi only,
    /// after admission; caller-provided task ids are not authority). The
    /// ZeroClaw side is decode-only: no constructor API exists, so a
    /// value enters this process only by deserializing a Tachi-sent
    /// wire value.
    TaskRef,
    "task:"
);
decode_only_ref!(
    /// One execution try of a task (TB-18: Task ≠ Attempt). Tachi-minted;
    /// decode-only on this side.
    AttemptRef,
    "attempt:"
);
decode_only_ref!(
    /// A host/harness-owned session attached through the tachi attached-session
    /// attachment spine (never an ACP `session_uuid`). Tachi-owned;
    /// decode-only on this side.
    HarnessSessionRef,
    "harness:"
);
decode_only_ref!(
    /// A procedure run identity (TB-14). Tachi-owned; decode-only here.
    ProcedureRunRef,
    "proc:"
);
decode_only_ref!(
    /// A durable delivery intent identity (tachi durable-delivery surface; V2 is
    /// pull-only so this ref is only projected). Decode-only here.
    DeliveryIntentRef,
    "deliver:"
);
decode_only_ref!(
    /// A durable conversational session identity (TB-14). Not a Tachi
    /// `acp_sessions.session_uuid`, not a `TaskRef`. Decode-only here.
    ConversationSessionRef,
    "conv:"
);

/// Generates one namespaced ref newtype whose namespace ZeroClaw OWNS
/// (requester-side run lineage): the constructor FORCES the namespace
/// prefix onto an opaque id, so these types can name their own lineage
/// but can never fabricate a `task:`/`attempt:` value (TB-6).
macro_rules! own_namespace_ref {
    ($(#[$meta:meta])* $name:ident, $prefix:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Wire namespace prefix (frozen by the `task-intent.v1`
            /// golden).
            pub const WIRE_PREFIX: &'static str = $prefix;

            /// Name this side's own run lineage. The namespace prefix is
            /// forced: the value always serializes inside this ref's own
            /// namespace, whatever the opaque id contains. The body is
            /// length-capped exactly like the decode path, so a
            /// constructed value can never be oversize on the wire.
            pub fn own(opaque_id: impl fmt::Display) -> Result<Self, RefError> {
                let body = opaque_id.to_string();
                if body.is_empty() || body.len() > REF_VALUE_MAX {
                    return Err(RefError::InvalidLength);
                }
                Ok(Self(format!("{}{}", Self::WIRE_PREFIX, body)))
            }

            /// The namespaced wire form, e.g. `"parent:…"`.
            pub fn as_wire(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = RefError;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                let body = raw
                    .strip_prefix(Self::WIRE_PREFIX)
                    .ok_or(RefError::WrongNamespace)?;
                if body.is_empty() || body.len() > REF_VALUE_MAX {
                    return Err(RefError::InvalidLength);
                }
                Ok(Self(raw))
            }
        }
    };
}

own_namespace_ref!(
    /// A parent run the submitting requester belongs to (TB-14). This is
    /// the REQUESTER's own run lineage — ZeroClaw names it, but only ever
    /// inside the `parent:` namespace.
    ParentRunRef,
    "parent:"
);
own_namespace_ref!(
    /// A supervising sub-agent run (TB-14, SubAgent spine).
    /// Requester-owned lineage, `subrun:` namespace only.
    SubAgentRunRef,
    "subrun:"
);

/// The identity of the requester submitting an intent (TB-3 wire field;
/// not one of the TB-14 eight, and never interchangeable with them).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequesterRef(String);

impl RequesterRef {
    /// Minimum/maximum length for a requester identity value.
    pub const LEN_RANGE: std::ops::RangeInclusive<usize> = 1..=REF_VALUE_MAX;

    /// Admitted requester identity. This is caller-stated identity that
    /// Tachi admission must verify against its own authority source —
    /// construction is a *claim*, never authority.
    pub fn claim(value: impl Into<String>) -> Result<Self, RefError> {
        let value = value.into();
        if !Self::LEN_RANGE.contains(&value.len()) {
            return Err(RefError::InvalidLength);
        }
        Ok(Self(value))
    }
}

impl fmt::Display for RequesterRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<RequesterRef> for String {
    fn from(value: RequesterRef) -> Self {
        value.0
    }
}

/// A caller RequestId for TB-7 idempotency: the `(requester, request_id)`
/// tuple is the idempotency scope for submit (and, later, intervene/stop).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(String);

impl RequestId {
    /// Minimum/maximum length for a request id value.
    pub const LEN_RANGE: std::ops::RangeInclusive<usize> = 1..=128;

    /// Caller-chosen request id. RULING-205 §2: a caller that loses a
    /// submit response must REPLAY the same request id, never invent a
    /// new one.
    pub fn new(value: impl Into<String>) -> Result<Self, RefError> {
        let value = value.into();
        if !Self::LEN_RANGE.contains(&value.len()) {
            return Err(RefError::InvalidLength);
        }
        Ok(Self(value))
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<RequestId> for String {
    fn from(value: RequestId) -> Self {
        value.0
    }
}

impl TryFrom<String> for RequesterRef {
    type Error = RefError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::claim(value)
    }
}

impl TryFrom<String> for RequestId {
    type Error = RefError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Typed ref construction failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RefError {
    /// Value length outside the admitted bounds for the ref type.
    #[error("ref value length outside admitted bounds")]
    InvalidLength,
    /// A wire value did not carry this ref type's own namespace prefix.
    #[error("ref wire value must use its own namespace prefix")]
    WrongNamespace,
}

// ─────────────────────────────────────────────────────────────────────────
// Closed vocabularies (TB-5)
// ─────────────────────────────────────────────────────────────────────────

/// DECISION TB-5/A (carrier: closed Rust enum, option (a) — the pick
/// recorded by the tachi host bridge; least authority). No variant can name a
/// vendor, model, CLI, or tool, so a `capability_request` carrying `glm`,
/// `codex`, or CLI flags fails enum deserialization — the discrimination
/// is structural, not a content filter.
///
/// The watershed leaf (V2b) admits [`Capability::RepositoryImplementation`]
/// into the catalog as its required capability. The tachi-side enum
/// extension (decoder-side admission of the `repository_implementation`
/// wire value) is tachi-repo work and is tracked as the Stage-B gap in
/// the leaf ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Bounded reasoning / review producing a structured report.
    ReasoningReview,
    /// Read-only investigation over admitted repos, docs, and issues.
    ReadOnlyInvestigation,
    /// Implementation work against a repository, producing artifacts
    /// (diff/verification) — this leaf's acceptance capability.
    RepositoryImplementation,
}

/// The capability an intent requests (TB-5). One capability per intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityRequest {
    /// The requested capability (closed enum, TB-5/A option (a)).
    pub capability: Capability,
}

/// Where a task's source material lives (TB-3 `source_refs`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    /// Kind of source (closed set).
    pub kind: SourceKind,
    /// Locator text (e.g. an `owner/repo` issue locator). Bounded; never a filesystem
    /// path or a command (content-scanned at encode time).
    pub locator: BoundedText,
}

/// Closed source-kind vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A tracked issue.
    Issue,
    /// A pull request.
    PullRequest,
    /// A repository at large.
    Repository,
    /// An admitted document.
    Document,
}

/// A semantic constraint on the work (TB-3 `constraints`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskConstraint {
    /// Human-readable constraint statement. Content-scanned (TB-4).
    pub description: BoundedText,
}

/// What artifact the requester expects (TB-3 `expected_artifacts`; drives
/// the TB-13 "success without required artifact is not contract success"
/// check).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactExpectation {
    /// Closed artifact class, e.g. `report`, `diff`, `verification_log`.
    /// Deliberately not a path: naming an output path would be execution
    /// detail (TB-1).
    pub artifact_class: ArtifactClass,
    /// Bounded description of what satisfies this expectation.
    pub description: BoundedText,
    /// Whether absence of this artifact fails the evaluation contract.
    pub required: bool,
}

/// Closed artifact-class vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactClass {
    /// A written report.
    Report,
    /// A source diff.
    Diff,
    /// Evidence that verification ran (tests/checks).
    VerificationLog,
}

/// Evaluation independence requirement (TB-3 `evaluation_requirement`;
/// classes frozen by TB-17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRequirement {
    /// Required independence class for the evaluation of this task's
    /// result.
    pub independence: IndependenceClass,
}

/// TB-17 independence classes. `SameSessionContinuation` can never satisfy
/// an independent-review requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndependenceClass {
    /// Deterministic mechanical check.
    DeterministicCheck,
    /// Continuation inside the same session — never independent review.
    SameSessionContinuation,
    /// Fresh context, same harness.
    FreshContextSameHarness,
    /// Fresh context, different model, same vendor.
    FreshContextCrossModelSameVendor,
    /// Fresh context, different vendor.
    FreshContextCrossVendor,
    /// Human review.
    HumanReview,
}

impl IndependenceClass {
    /// Whether this class marks an INDEPENDENT-review requirement
    /// (vertical V3's discriminating half of TB-17): the fresh-context
    /// classes and human review. `SameSessionContinuation` and
    /// `DeterministicCheck` are deliberately NOT independence-marked —
    /// a requirement carried by either of those classes is not asking
    /// for independence at all.
    #[must_use]
    pub fn is_independence_marked(self) -> bool {
        matches!(
            self,
            Self::FreshContextSameHarness
                | Self::FreshContextCrossModelSameVendor
                | Self::FreshContextCrossVendor
                | Self::HumanReview
        )
    }

    /// The frozen satisfaction law (TB-17 + vertical V3):
    ///
    /// - `SameSessionContinuation` satisfies NOTHING independence-marked,
    ///   ever — continuation is not independent review. It satisfies only
    ///   an explicit `same_session_continuation` requirement.
    /// - `DeterministicCheck` likewise satisfies only its own class: a
    ///   mechanical check is not independent review either.
    /// - Among the independence-marked classes, a STRICTER actual class
    ///   satisfies a weaker requirement (`fresh_context_same_harness` <
    ///   `fresh_context_cross_model_same_vendor` <
    ///   `fresh_context_cross_vendor`), and `human_review` satisfies every
    ///   independence-marked requirement (a human is strictly more
    ///   independent than any model context).
    /// - A non-marked requirement is satisfied only by its exact class.
    #[must_use]
    pub fn satisfies_requirement(self, required: Self) -> bool {
        fn rank(class: IndependenceClass) -> Option<u8> {
            match class {
                IndependenceClass::FreshContextSameHarness => Some(1),
                IndependenceClass::FreshContextCrossModelSameVendor => Some(2),
                IndependenceClass::FreshContextCrossVendor => Some(3),
                IndependenceClass::HumanReview => Some(4),
                IndependenceClass::DeterministicCheck
                | IndependenceClass::SameSessionContinuation => None,
            }
        }
        match (rank(self), rank(required)) {
            (Some(actual), Some(needed)) => actual >= needed,
            // Same class, exact match — including the two non-marked
            // classes, which satisfy nothing but themselves.
            _ => self == required && !required.is_independence_marked(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// TB-11/TB-12 intervention wire (vertical V3): the ZeroClaw mirror of the
// tachi host's `intervention.rs` vocabulary, receipts, and typed failure
// surface. Shapes are frozen by the tachi-side implementation
// (externally-tagged serde, `deny_unknown_fields`); drift breaks the
// stage-B live pair, not a golden file.
// ─────────────────────────────────────────────────────────────────────────

/// Payload-free discriminant of the frozen 10-op `InterventionV1`
/// vocabulary (TB-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionStatic {
    /// ProvideAdditionalContext
    ProvideAdditionalContext,
    /// RequestCorrection
    RequestCorrection,
    /// RequestContinuation
    RequestContinuation,
    /// RequestIndependentReview
    RequestIndependentReview,
    /// RequestUserInput
    RequestUserInput,
    /// RequestPause
    RequestPause,
    /// RequestResume
    RequestResume,
    /// RequestGracefulStop (alias of `request_stop(graceful)`)
    RequestGracefulStop,
    /// RequestHardCancel (alias of `request_stop(hard)`)
    RequestHardCancel,
    /// Escalate
    Escalate,
}

impl InterventionStatic {
    /// All ten frozen operations (machine-checkable vocabulary freeze).
    pub const ALL: [Self; 10] = [
        Self::ProvideAdditionalContext,
        Self::RequestCorrection,
        Self::RequestContinuation,
        Self::RequestIndependentReview,
        Self::RequestUserInput,
        Self::RequestPause,
        Self::RequestResume,
        Self::RequestGracefulStop,
        Self::RequestHardCancel,
        Self::Escalate,
    ];

    /// The SA-29 Supervisor authority this operation corresponds to, if
    /// any. `RequestPause`/`RequestResume`/`Escalate` are OUTSIDE the
    /// Supervisor grant set (vertical V3 DoD row 1) and map to `None` —
    /// the mapping is the refusal law made structural: no supervisor
    /// surface can be built for an operation with no corresponding
    /// authority.
    #[must_use]
    pub fn supervisor_authority(self) -> Option<crate::subagent_v1::SupervisorAuthority> {
        use crate::subagent_v1::SupervisorAuthority;
        match self {
            Self::ProvideAdditionalContext => Some(SupervisorAuthority::ProvideContext),
            Self::RequestCorrection => Some(SupervisorAuthority::RequestCorrection),
            Self::RequestContinuation => Some(SupervisorAuthority::RequestContinuation),
            Self::RequestIndependentReview => Some(SupervisorAuthority::RequestIndependentReview),
            Self::RequestUserInput => Some(SupervisorAuthority::RequestUserInput),
            Self::RequestGracefulStop => Some(SupervisorAuthority::RequestGracefulStop),
            Self::RequestHardCancel => Some(SupervisorAuthority::RequestCancel),
            Self::RequestPause | Self::RequestResume | Self::Escalate => None,
        }
    }
}

/// The frozen 10-op intervention vocabulary (TB-11), ZeroClaw mirror.
/// `RequestIndependentReview` and `Escalate` are NOT session
/// interventions — the host refuses them with
/// [`InterventionError::RequiresNewTaskLineage`] and the client maps
/// independent review to a NEW task submission (vertical V3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum InterventionV1 {
    /// Provide additional context to the running work.
    ProvideAdditionalContext {
        /// The context note (content, not authority — TB-4 seam law).
        note: BoundedText,
    },
    /// Request a correction.
    RequestCorrection {
        /// What to correct.
        note: BoundedText,
    },
    /// Request continuation (NEVER independent review — TB-17).
    RequestContinuation {
        /// Continuation note.
        note: BoundedText,
    },
    /// Request independent review (new task/adjudication lineage —
    /// refused by the session-intervention path on both sides).
    RequestIndependentReview {
        /// Required independence class for the review.
        independence_class: IndependenceClass,
    },
    /// Request user input.
    RequestUserInput {
        /// The question to surface.
        prompt: BoundedText,
    },
    /// Request a pause.
    RequestPause,
    /// Request a resume.
    RequestResume,
    /// Request a graceful stop — alias of `request_stop(graceful)`.
    RequestGracefulStop {
        /// Why the stop is requested.
        reason: BoundedText,
    },
    /// Request a hard cancel — alias of `request_stop(hard)`.
    RequestHardCancel {
        /// Why the cancel is requested.
        reason: BoundedText,
    },
    /// Escalate (new task/adjudication lineage — refused by the
    /// session-intervention path).
    Escalate {
        /// Why the escalation is requested.
        reason: BoundedText,
    },
}

impl InterventionV1 {
    /// The payload-free discriminant of this operation.
    #[must_use]
    pub fn discriminant(&self) -> InterventionStatic {
        match self {
            Self::ProvideAdditionalContext { .. } => InterventionStatic::ProvideAdditionalContext,
            Self::RequestCorrection { .. } => InterventionStatic::RequestCorrection,
            Self::RequestContinuation { .. } => InterventionStatic::RequestContinuation,
            Self::RequestIndependentReview { .. } => InterventionStatic::RequestIndependentReview,
            Self::RequestUserInput { .. } => InterventionStatic::RequestUserInput,
            Self::RequestPause => InterventionStatic::RequestPause,
            Self::RequestResume => InterventionStatic::RequestResume,
            Self::RequestGracefulStop { .. } => InterventionStatic::RequestGracefulStop,
            Self::RequestHardCancel { .. } => InterventionStatic::RequestHardCancel,
            Self::Escalate { .. } => InterventionStatic::Escalate,
        }
    }
}

/// Stop mode for `request_stop` and the stop-alias interventions (TB-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopMode {
    /// Graceful stop: ask the work to wind down.
    Graceful,
    /// Hard cancel: terminate.
    Hard,
}

impl StopMode {
    /// Wire token for the mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::Hard => "hard",
        }
    }
}

/// The multi-stage stop fact (TB-12): requested → forwarded → confirmed →
/// terminal. `Confirmed` is the only stage a `cancelled` projection may
/// derive from; the client never mints one locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopStage {
    /// Stop requested and bound (not yet forwarded).
    Requested,
    /// Forwarded to the lifecycle owner (still not cancelled).
    Forwarded,
    /// The lifecycle owner authoritatively confirmed the terminal
    /// cancellation.
    Confirmed,
    /// The owner disappeared after possible side effects — outcome unknown.
    OutcomeUnknown,
}

/// One `request_stop` receipt (TB-12). Exactly one stop receipt type; the
/// stop variants of `intervene` carry this payload (TB-11 connection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopReceipt {
    /// The task stopped.
    pub task_ref: TaskRef,
    /// Host-minted stop operation id.
    pub stop_id: String,
    /// `graceful` or `hard`.
    pub mode: StopMode,
    /// Current stage of the multi-stage stop fact.
    pub stage: StopStage,
    /// The RequestId this stop was idempotency-bound to.
    pub request_id: String,
}

/// Typed intervention receipt envelope (TB-11). The stop variants carry
/// exactly the [`StopReceipt`] of TB-12. Externally tagged, mirroring
/// the tachi host's serde shape (`{"context_provided": {...}}`,
/// `{"stop": {...}}`) — the stage-B live pair depends on the shapes
/// matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterventionReceipt {
    /// Additional context was forwarded.
    ContextProvided {
        /// Host-minted intervention id.
        intervention_id: String,
    },
    /// Correction was requested.
    CorrectionRequested {
        /// Host-minted intervention id.
        intervention_id: String,
    },
    /// Continuation was requested (recorded as continuation — NEVER
    /// independent review; TB-17).
    ContinuationRequested {
        /// Host-minted intervention id.
        intervention_id: String,
    },
    /// User input was requested.
    UserInputRequested {
        /// Host-minted intervention id.
        intervention_id: String,
    },
    /// Pause was forwarded.
    Paused {
        /// Host-minted intervention id.
        intervention_id: String,
    },
    /// Resume was forwarded.
    Resumed {
        /// Host-minted intervention id.
        intervention_id: String,
    },
    /// Stop-alias interventions resolve to the single stop authority
    /// (TB-11/TB-12).
    Stop(StopReceipt),
}

impl InterventionReceipt {
    /// The intervention id this receipt is bound to, when it is a
    /// non-stop receipt. Stop receipts are bound to their stop id.
    #[must_use]
    pub fn intervention_id(&self) -> Option<&str> {
        match self {
            Self::ContextProvided { intervention_id }
            | Self::CorrectionRequested { intervention_id }
            | Self::ContinuationRequested { intervention_id }
            | Self::UserInputRequested { intervention_id }
            | Self::Paused { intervention_id }
            | Self::Resumed { intervention_id } => Some(intervention_id),
            Self::Stop(_) => None,
        }
    }

    /// Whether this receipt is a CONTINUATION fact. Load-bearing for
    /// TB-17: a continuation receipt can never be used as independent
    /// review evidence — this predicate is the client-side hook the
    /// supervisor gate consults (a continuation answers `false` to
    /// "is this an independent review?" by construction).
    #[must_use]
    pub fn is_continuation(&self) -> bool {
        matches!(self, Self::ContinuationRequested { .. })
    }
}

/// Typed intervention/stop failure surface (TB-11/TB-12), ZeroClaw mirror
/// of the tachi host's `InterventionError`. `category` in
/// `ForbiddenContent` is a label string because the forbidden-category
/// enum lives in `zeroclaw-runtime`'s compose layer (client superset);
/// the host law stays authoritative host-side.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InterventionError {
    /// The lifecycle owner does not support the requested operation —
    /// typed refusal, ZERO state mutation, NO fresh-task fallback (TB-11).
    #[error("unsupported_by_lifecycle_owner")]
    UnsupportedByLifecycleOwner {
        /// The refused operation.
        operation: InterventionStatic,
    },
    /// The operation maps to NEW task/adjudication lineage and is refused
    /// by the session-intervention path on both sides (TB-11). The
    /// client's independent-review surface performs this mapping by
    /// submitting a NEW review task.
    #[error("operation requires new task/adjudication lineage (not a session intervention)")]
    RequiresNewTaskLineage {
        /// The refused operation.
        operation: InterventionStatic,
    },
    /// `expected_task_revision` mismatch — typed conflict, never a
    /// best-effort apply (TB-11).
    #[error("expected_task_revision mismatch: expected {expected}, snapshot is {actual}")]
    RevisionConflict {
        /// The revision the caller expected.
        expected: u64,
        /// The snapshot's actual revision.
        actual: u64,
    },
    /// Same `(requester, request_id)` bound to a different digest
    /// (TB-7 rule 3, applied to interventions per rule 6).
    #[error("request id conflict: bound to {bound_digest}, submitted {submitted_digest}")]
    RequestIdConflict {
        /// Digest the tuple is already bound to.
        bound_digest: String,
        /// Digest of the incoming request.
        submitted_digest: String,
    },
    /// The task does not exist, or the requester does not own it.
    #[error("task not found")]
    NotFound,
    /// The requester is not admitted by the authority source.
    #[error("requester not admitted")]
    RequesterNotAdmitted,
    /// An intervention text matched a forbidden-content category (TB-4).
    #[error("intervention rejected: {category} in field `{field}`")]
    ForbiddenContent {
        /// The matched category label.
        category: String,
        /// The offending field.
        field: String,
    },
    /// The request tuple is bound but its receipt has not materialized
    /// (ambiguous in-flight window).
    #[error("intervention pending reconciliation")]
    ReconciliationUnknown,
    /// The lifecycle owner disappeared while the request was in flight;
    /// nothing was mutated.
    #[error("lifecycle owner disappeared")]
    OwnerDisappeared,
    /// The bridge transport/truth source is unavailable (TB-20).
    #[error("bridge unavailable")]
    Unavailable,
}

/// Typed workspace selector (TB-3 `workspace_source`): a repo and revision
/// the requester points at. This is a **selector over Tachi-admitted
/// workspace truth** (the Tachi ExecEnv/Workspace plane owns placement);
/// a caller-selected worktree path as execution authority is forbidden
/// wire content (TB-4) and is not representable here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSourceRef {
    /// Repository identity (e.g. `owner/name`). Bounded; content-scanned.
    pub repo: BoundedText,
    /// Optional git revision selector (branch, tag, or commit).
    pub git_ref: Option<BoundedText>,
}

/// Typed routing preference (TB-5: preference only — never grants
/// placement, credentials, data egress, safety exceptions, or lifecycle
/// authority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingPreference {
    /// No preference; Tachi staffing chooses freely (default posture).
    NoPreference,
    /// Prefer the Tachi-managed batch lane.
    PreferTachiManaged,
    /// Prefer a harness-native attached session.
    PreferHarnessNative,
}

/// Typed approval requirement asserted by the requester (TB-3). This is an
/// assertion of what approval the requester believes applies — actual
/// approval authority is resolved by Tachi admission against policy,
/// never granted by this field (TB-4 seam law: intent fields are not
/// authority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRequirement {
    /// Requester asserts no explicit approval gate is required for this
    /// intent. Admission may still impose one from policy.
    NotRequired,
    /// Requester asserts explicit human approval is required before launch.
    RequireExplicitApproval,
}

/// Privacy class of the intent content (TB-3). Private-Dyad-labeled
/// content is forbidden on this wire entirely (TB-4); `Confidential` is
/// the most sensitive class that may be represented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    /// Public-safe content.
    Public,
    /// Internal content.
    Internal,
    /// Confidential content; strictest visibility/redaction handling short
    /// of the forbidden Private-Dyad class.
    Confidential,
}

// ─────────────────────────────────────────────────────────────────────────
// The wire payload
// ─────────────────────────────────────────────────────────────────────────

/// The frozen host semantic wire (TB-3). Exactly the fields below; the
/// golden test pins the wire shape and the canonical digest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskIntentV1 {
    /// Schema version tag (`task-intent.v1`).
    pub schema: String,
    /// What the requester wants accomplished. Bounded; content-scanned.
    pub objective: BoundedText,
    /// The capability being requested (closed enum; TB-5).
    pub capability_request: CapabilityRequest,
    /// Admitted requester identity (verified Tachi-side at admission).
    pub requester: RequesterRef,
    /// Optional parent run lineage.
    pub parent_ref: Option<ParentRunRef>,
    /// Optional supervising sub-agent run.
    pub supervisor_ref: Option<SubAgentRunRef>,
    /// Opaque reference to the admitted context bundle (content, never
    /// authority — TB-4 seam law).
    pub context_bundle_ref: BoundedText,
    /// Source material references.
    pub source_refs: Vec<SourceRef>,
    /// Semantic constraints.
    pub constraints: Vec<TaskConstraint>,
    /// Expected artifacts (drives TB-13 contract satisfaction).
    pub expected_artifacts: Vec<ArtifactExpectation>,
    /// Evaluation independence requirement.
    pub evaluation_requirement: EvaluationRequirement,
    /// Optional typed workspace selector.
    pub workspace_source: Option<WorkspaceSourceRef>,
    /// Optional typed routing preference.
    pub routing_preference: Option<RoutingPreference>,
    /// Requester-asserted approval requirement.
    pub approval_requirement: ApprovalRequirement,
    /// Privacy class of the intent content.
    pub privacy_class: PrivacyClass,
    /// Optional expiry timestamp.
    pub expiry: Option<Timestamp>,
    /// Explicit lineage for a deliberate retry of a prior task (TB-18):
    /// absent for a first submission; a retry is a NEW submission with
    /// this field set, never a rewrite of the prior attempt's facts.
    pub retry_of: Option<TaskRef>,
}

impl TaskIntentV1 {
    /// The canonical request digest used by TB-7 idempotency: SHA-256
    /// (lower hex) over the canonical JSON of `{"schema": tag, "intent":
    /// payload}` using the same rule as tachi's
    /// `memcore::canonical_digest::canonical_json_digest_hex` (keys
    /// sorted recursively, so serde field order cannot fork digests).
    /// The golden test pins the sample digest `84ab2316…ce3a31`.
    pub fn canonical_digest(&self) -> String {
        let value = serde_json::to_value(self).expect("TaskIntentV1 serializes");
        let composite = serde_json::json!({
            "schema": SCHEMA_TAG,
            "intent": value,
        });
        canonical_json_digest_hex(&composite)
    }
}

/// Recursively sort every object key, leaving arrays in caller order
/// (array order is content, not layout) — the first half of the shared
/// canonical-JSON digest rule.
pub fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted = map
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect::<Map<_, _>>())
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        scalar => scalar.clone(),
    }
}

/// SHA-256 lower hex (no prefix) of the canonical serialization of
/// `value` — the second half of the shared digest rule, byte-identical to
/// tachi's `memcore::canonical_digest::canonical_json_digest_hex`.
pub fn canonical_json_digest_hex(value: &Value) -> String {
    let canonical = canonical_json(value).to_string();
    let bytes = Sha256::digest(canonical.as_bytes());
    let mut out = String::with_capacity(2 * bytes.len());
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

impl TryFrom<String> for BoundedText {
    type Error = WireError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for Timestamp {
    type Error = WireError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

/// Wire-level construction/validation failure.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    /// A text-bearing value exceeded the bounded-text cap.
    #[error("text value exceeds the {BOUNDED_TEXT_MAX}-byte wire cap (len {len})")]
    TextTooLong {
        /// The offending length.
        len: usize,
    },
    /// A timestamp was not RFC3339.
    #[error("timestamp is not valid RFC3339")]
    BadTimestamp(#[source] chrono::ParseError),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The golden document: pinned digest + the sample intent payload.
    fn golden() -> (String, TaskIntentV1) {
        let document: Value = serde_json::from_str(GOLDEN_TASK_INTENT_V1)
            .expect("golden JSON parses on the encoder side");
        let digest = document["digest_sha256"]
            .as_str()
            .expect("golden digest field")
            .to_string();
        let intent: TaskIntentV1 = serde_json::from_value(document["intent"].clone())
            .expect("golden intent decodes on the encoder side");
        (digest, intent)
    }

    #[test]
    fn golden_digest_pin_matches_encoder_digest() {
        // TB-3 cross-repo golden pair (encoder half): the digest computed
        // by THIS implementation over the golden payload equals both the
        // digest recorded in the golden document AND the frozen constant.
        let (document_digest, intent) = golden();
        assert_eq!(document_digest, GOLDEN_DIGEST_SHA256);
        assert_eq!(intent.canonical_digest(), GOLDEN_DIGEST_SHA256);
        assert_eq!(intent.schema, SCHEMA_TAG);
    }

    #[test]
    fn golden_payload_round_trips_value_stably() {
        // A nested-type or serde-rename change on this side breaks here.
        // The comparison is at the canonical-JSON Value level (semantic
        // equality, formatting-insensitive): byte-level formatting is
        // pinned separately — the RFC3339 `Z`→`+00:00` normalization is
        // asserted in `timestamps_round_trip_the_golden_rfc3339_form`,
        // and key order is normalized by `canonical_json` (digest-pinned
        // in `golden_digest_pin_matches_encoder_digest`).
        let (_, intent) = golden();
        let encoded = serde_json::to_value(&intent).expect("intent serializes");
        let document: Value = serde_json::from_str(GOLDEN_TASK_INTENT_V1).expect("golden parses");
        assert_eq!(encoded, document["intent"]);
        let decoded: TaskIntentV1 = serde_json::from_value(encoded).expect("round-trip decodes");
        assert_eq!(decoded, intent);
    }

    #[test]
    fn digest_is_stable_and_content_sensitive() {
        let (_, mut intent) = golden();
        let first = intent.canonical_digest();
        // Same semantic content ⇒ same digest.
        let same = intent.clone();
        assert_eq!(same.canonical_digest(), first);
        // Different content ⇒ different digest.
        intent.objective = BoundedText::new("different objective").expect("bounded");
        assert_ne!(intent.canonical_digest(), first);
    }

    #[test]
    fn bounded_text_caps_unbounded_transcripts() {
        assert!(BoundedText::new("x".repeat(BOUNDED_TEXT_MAX)).is_ok());
        assert!(BoundedText::new("x".repeat(BOUNDED_TEXT_MAX + 1)).is_err());
    }

    #[test]
    fn timestamps_round_trip_the_golden_rfc3339_form() {
        let ts = Timestamp::parse("2026-12-01T00:00:00Z").expect("rfc3339");
        // chrono `to_rfc3339` normalizes to the +00:00 form the golden
        // pins — byte-identical to the tachi decoder side.
        assert_eq!(String::from(ts), "2026-12-01T00:00:00+00:00");
        assert!(Timestamp::parse("not-a-timestamp").is_err());
    }

    // ─────────────────────────────────────────────────────────────────────
    // Watershed discrimination (vertical V2b): serialization-level proofs
    // that every banned dimension is UNREPRESENTABLE on this wire.
    // ─────────────────────────────────────────────────────────────────────

    /// A pristine golden payload as a JSON object, for mutation tests.
    fn golden_payload() -> Value {
        let document: Value = serde_json::from_str(GOLDEN_TASK_INTENT_V1).expect("golden parses");
        document["intent"].clone()
    }

    fn decode_fails(payload: &Value) -> String {
        serde_json::from_value::<TaskIntentV1>(payload.clone())
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| panic!("payload must fail decode: {payload}"))
    }

    #[test]
    fn vendor_and_model_names_are_unrepresentable_in_capability_request() {
        // TB-5: glm/codex/claude/any vendor, model, or CLI token cannot
        // round-trip — the closed enum rejects them at decode.
        for banned in [
            "glm",
            "glm-5",
            "codex",
            "claude",
            "claude-code",
            "gpt-5",
            "deepseek",
            "codex_cli",
            "opencode",
        ] {
            let mut payload = golden_payload();
            payload["capability_request"]["capability"] = Value::String(banned.to_string());
            let error = decode_fails(&payload);
            assert!(
                error.contains("unknown variant"),
                "banned capability {banned:?} must fail as unknown variant, got: {error}"
            );
        }
    }

    #[test]
    fn execution_detail_fields_are_unrepresentable_at_any_nesting() {
        // TB-1/TB-4: no command/env/cwd/path/model/backend/sandbox/
        // credential-shaped field exists at any depth, including renamed
        // or nested variants — deny_unknown_fields rejects the smuggle.
        let smuggles: &[(&str, Value)] = &[
            ("command", Value::String("cargo test".into())),
            ("cli_flags", Value::String("--model glm".into())),
            ("args", serde_json::json!(["--dangerously-skip"])),
            ("env", serde_json::json!({"OPENAI_API_KEY": "x"})),
            ("cwd", Value::String("/tmp/worktree-1".into())),
            ("path", Value::String("/Users/x/repo".into())),
            ("model", Value::String("glm-5".into())),
            ("backend", Value::String("codex".into())),
            ("worktree", Value::String("/repo/worktrees/wt-1".into())),
            (
                "worktree_path",
                Value::String("/repo/worktrees/wt-1".into()),
            ),
            ("tmux", Value::String("tmux new-session".into())),
            ("ssh", serde_json::json!({"host": "box"})),
            ("sandbox", Value::String("danger-full-access".into())),
            ("sandbox_flags", serde_json::json!(["--no-sandbox"])),
            ("credentials", serde_json::json!({"token": "ghp_x"})),
            ("api_key", Value::String("sk-ant-x".into())),
        ];
        for (field, value) in smuggles {
            // Top level.
            let mut payload = golden_payload();
            payload[*field] = value.clone();
            assert!(
                decode_fails(&payload).contains("unknown field"),
                "top-level smuggle of {field:?} must fail deny_unknown_fields"
            );
            // Nested: capability_request.
            let mut payload = golden_payload();
            payload["capability_request"][*field] = value.clone();
            assert!(
                decode_fails(&payload).contains("unknown field"),
                "capability_request smuggle of {field:?} must fail"
            );
            // Nested: workspace_source.
            let mut payload = golden_payload();
            payload["workspace_source"][*field] = value.clone();
            assert!(
                decode_fails(&payload).contains("unknown field"),
                "workspace_source smuggle of {field:?} must fail"
            );
            // Nested: a constraint entry.
            let mut payload = golden_payload();
            payload["constraints"][0][*field] = value.clone();
            assert!(
                decode_fails(&payload).contains("unknown field"),
                "constraint smuggle of {field:?} must fail"
            );
            // Nested: an expected_artifacts entry.
            let mut payload = golden_payload();
            payload["expected_artifacts"][0][*field] = value.clone();
            assert!(
                decode_fails(&payload).contains("unknown field"),
                "expected_artifacts smuggle of {field:?} must fail"
            );
            // Nested: a source_refs entry.
            let mut payload = golden_payload();
            payload["source_refs"][0][*field] = value.clone();
            assert!(
                decode_fails(&payload).contains("unknown field"),
                "source_refs smuggle of {field:?} must fail"
            );
            // Nested: evaluation_requirement.
            let mut payload = golden_payload();
            payload["evaluation_requirement"][*field] = value.clone();
            assert!(
                decode_fails(&payload).contains("unknown field"),
                "evaluation_requirement smuggle of {field:?} must fail"
            );
        }
    }

    #[test]
    fn caller_minted_task_and_attempt_ids_are_unrepresentable_as_ref_values() {
        // TB-6/TB-14: the typed refs are non-interchangeable — each
        // deserializes only inside its own wire namespace.
        let task: TaskRef =
            serde_json::from_value(Value::String("task:abc123".into())).expect("task: decodes");
        assert_eq!(task.as_wire(), "task:abc123");
        // A TaskRef carrying another namespace or a bare id fails.
        for wrong in ["attempt:abc123", "abc123", ""] {
            assert!(
                serde_json::from_value::<TaskRef>(Value::String(wrong.into())).is_err(),
                "TaskRef must reject wire value {wrong:?}"
            );
        }
        // Same body, different ref type: not interchangeable.
        assert!(
            serde_json::from_value::<AttemptRef>(Value::String("task:abc123".into())).is_err(),
            "AttemptRef must reject a task: wire value"
        );
        // retry_of with a caller-minted id shape still only accepts the
        // task: namespace — and there is no constructor to build one.
        let mut payload = golden_payload();
        payload["retry_of"] = Value::String("my-own-task-id".into());
        assert!(
            serde_json::from_value::<TaskIntentV1>(payload).is_err(),
            "retry_of must reject a non-namespaced caller-minted id"
        );
    }

    #[test]
    fn own_namespace_refs_cannot_fabricate_task_or_attempt_values() {
        // The requester-owned lineage constructors FORCE their own
        // namespace: whatever opaque id they wrap, the wire value stays
        // inside parent:/subrun: and can never become a task:/attempt:
        // value (TB-6).
        let parent = ParentRunRef::own("task:abc123").expect("bounded");
        assert_eq!(parent.as_wire(), "parent:task:abc123");
        assert!(parent.as_wire().starts_with(ParentRunRef::WIRE_PREFIX));
        assert!(!parent.as_wire().starts_with(TaskRef::WIRE_PREFIX));
        let subrun = SubAgentRunRef::own("attempt:xyz").expect("bounded");
        assert_eq!(subrun.as_wire(), "subrun:attempt:xyz");
        // Wire round-trip requires the own namespace too.
        assert!(serde_json::from_value::<ParentRunRef>(Value::String("task:abc".into())).is_err());
        let round: ParentRunRef =
            serde_json::from_value(Value::String("parent:run-1".into())).expect("own namespace");
        assert_eq!(round.as_wire(), "parent:run-1");
    }

    #[test]
    fn eight_wire_namespaces_are_distinct() {
        // TB-14: distinct serialization namespaces, no collisions.
        let prefixes = [
            ConversationSessionRef::WIRE_PREFIX,
            ParentRunRef::WIRE_PREFIX,
            SubAgentRunRef::WIRE_PREFIX,
            TaskRef::WIRE_PREFIX,
            AttemptRef::WIRE_PREFIX,
            HarnessSessionRef::WIRE_PREFIX,
            ProcedureRunRef::WIRE_PREFIX,
            DeliveryIntentRef::WIRE_PREFIX,
        ];
        let mut sorted = prefixes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            prefixes.len(),
            "wire namespaces must be distinct"
        );
    }

    #[test]
    fn own_namespace_ref_constructors_enforce_the_wire_length_cap() {
        // The construction path must enforce the
        // same body cap as the decode path, so a constructed ref can
        // never be oversize (or empty) on the wire.
        assert!(ParentRunRef::own("run-1").is_ok());
        assert!(ParentRunRef::own("").is_err());
        assert!(ParentRunRef::own("x".repeat(REF_VALUE_MAX + 1)).is_err());
        assert!(ParentRunRef::own("x".repeat(REF_VALUE_MAX)).is_ok());
        assert!(SubAgentRunRef::own("x".repeat(REF_VALUE_MAX + 1)).is_err());
    }

    #[test]
    fn repository_implementation_is_representable_on_this_wire() {
        // Vertical V2b: the leaf's required capability is admitted into
        // the closed catalog on the encoder side. (The tachi-side enum
        // extension is the recorded Stage-B gap; see the module docs.)
        let mut payload = golden_payload();
        payload["capability_request"]["capability"] =
            Value::String("repository_implementation".into());
        let intent: TaskIntentV1 = serde_json::from_value(payload).expect("decodes");
        assert_eq!(
            intent.capability_request.capability,
            Capability::RepositoryImplementation
        );
        let round = serde_json::to_value(&intent).expect("serializes");
        assert_eq!(
            round["capability_request"]["capability"],
            Value::String("repository_implementation".into())
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Vertical V3: TB-17 independence law + TB-11/TB-12 intervention wire
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn same_session_continuation_never_satisfies_independence_marked_requirements() {
        use IndependenceClass as IC;
        // Discrimination 1 (continuation ≠ independent review): for EVERY
        // independence-marked requirement, a SameSessionContinuation
        // actual class fails — and so does a DeterministicCheck.
        for required in [
            IC::FreshContextSameHarness,
            IC::FreshContextCrossModelSameVendor,
            IC::FreshContextCrossVendor,
            IC::HumanReview,
        ] {
            assert!(
                required.is_independence_marked(),
                "{required:?} must be independence-marked"
            );
            assert!(
                !IC::SameSessionContinuation.satisfies_requirement(required),
                "SameSessionContinuation must never satisfy {required:?}"
            );
            assert!(
                !IC::DeterministicCheck.satisfies_requirement(required),
                "DeterministicCheck must never satisfy {required:?}"
            );
        }
        // The two non-marked classes are not independence-marked.
        assert!(!IC::SameSessionContinuation.is_independence_marked());
        assert!(!IC::DeterministicCheck.is_independence_marked());
        // A non-marked requirement is satisfied only by its exact class.
        assert!(IC::SameSessionContinuation.satisfies_requirement(IC::SameSessionContinuation));
        assert!(!IC::DeterministicCheck.satisfies_requirement(IC::SameSessionContinuation));
        assert!(IC::DeterministicCheck.satisfies_requirement(IC::DeterministicCheck));
    }

    #[test]
    fn fresh_context_same_harness_is_a_valid_distinct_independence_class() {
        use IndependenceClass as IC;
        // Discrimination 2 (same harness SESSION ≠ independent review, but
        // FRESH CONTEXT SAME HARNESS is a valid distinct frozen class):
        // the class exists, is independence-marked, and satisfies exactly
        // the marked requirements at or below its rank.
        assert!(IC::FreshContextSameHarness.is_independence_marked());
        assert!(IC::FreshContextSameHarness.satisfies_requirement(IC::FreshContextSameHarness));
        // It does NOT substitute for a stricter requirement.
        assert!(!IC::FreshContextSameHarness.satisfies_requirement(IC::FreshContextCrossVendor));
        // A stricter actual class satisfies the weaker fresh requirement…
        assert!(IC::FreshContextCrossVendor.satisfies_requirement(IC::FreshContextSameHarness));
        assert!(
            IC::FreshContextCrossModelSameVendor.satisfies_requirement(IC::FreshContextSameHarness)
        );
        // …and human review satisfies every marked requirement.
        for required in [
            IC::FreshContextSameHarness,
            IC::FreshContextCrossModelSameVendor,
            IC::FreshContextCrossVendor,
            IC::HumanReview,
        ] {
            assert!(IC::HumanReview.satisfies_requirement(required));
        }
    }

    #[test]
    fn supervisor_authority_mapping_refuses_pause_resume_escalate() {
        use crate::subagent_v1::SupervisorAuthority;
        // Vertical V3 DoD row 1: TB-11 operations outside the Supervisor
        // grant set have NO corresponding supervisor authority — the
        // mapping is None, so no supervisor surface can be built for them.
        for op in [
            InterventionStatic::RequestPause,
            InterventionStatic::RequestResume,
            InterventionStatic::Escalate,
        ] {
            assert_eq!(
                op.supervisor_authority(),
                None,
                "{op:?} is outside the Supervisor grant set"
            );
        }
        // The seven session-side authorities map one-to-one.
        assert_eq!(
            InterventionStatic::ProvideAdditionalContext.supervisor_authority(),
            Some(SupervisorAuthority::ProvideContext)
        );
        assert_eq!(
            InterventionStatic::RequestHardCancel.supervisor_authority(),
            Some(SupervisorAuthority::RequestCancel)
        );
        assert_eq!(
            InterventionStatic::RequestIndependentReview.supervisor_authority(),
            Some(SupervisorAuthority::RequestIndependentReview)
        );
        // The vocabulary is exactly the frozen ten.
        assert_eq!(InterventionStatic::ALL.len(), 10);
    }

    #[test]
    fn intervention_wire_round_trips_the_frozen_shapes() {
        // Mirror fidelity: the externally-tagged wire form matches the
        // tachi host's serde shape (`{"variant": {...}}`).
        let intervention = InterventionV1::RequestCorrection {
            note: BoundedText::new("tighten the boundary check").expect("bounded"),
        };
        let wire = serde_json::to_value(&intervention).expect("serializes");
        assert_eq!(
            wire,
            serde_json::json!({"request_correction": {"note": "tighten the boundary check"}})
        );
        let back: InterventionV1 = serde_json::from_value(wire).expect("decodes");
        assert_eq!(back, intervention);
        assert_eq!(back.discriminant(), InterventionStatic::RequestCorrection);
        // deny_unknown_fields: a smuggled field fails decode.
        let mut smuggled = serde_json::to_value(&intervention).expect("serializes");
        smuggled["request_correction"]["command"] = Value::String("rm -rf".into());
        assert!(serde_json::from_value::<InterventionV1>(smuggled).is_err());
        // Stop receipts round-trip and the continuation predicate holds.
        let stop = InterventionReceipt::Stop(StopReceipt {
            task_ref: serde_json::from_value(Value::String("task:abc".into())).expect("ref"),
            stop_id: "stop-1".into(),
            mode: StopMode::Hard,
            stage: StopStage::Forwarded,
            request_id: "rid-1".into(),
        });
        let round: InterventionReceipt =
            serde_json::from_value(serde_json::to_value(&stop).expect("serializes"))
                .expect("decodes");
        assert_eq!(round, stop);
        assert!(!round.is_continuation());
        assert!(round.intervention_id().is_none());
        let cont = InterventionReceipt::ContinuationRequested {
            intervention_id: "iv-1".into(),
        };
        assert!(cont.is_continuation());
        assert_eq!(cont.intervention_id(), Some("iv-1"));
    }
}
