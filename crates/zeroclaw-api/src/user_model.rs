//! Shared User Model domain types.
//!
//! Persistence adapters live in `zeroclaw-memory`. Consumers use these
//! shapes without depending on a database path, SQL connection, or schema.

use std::fmt;

/// What kind of statement this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserModelKind {
    Value,
    Goal,
    Preference,
    Habit,
    Constraint,
}

impl UserModelKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Goal => "goal",
            Self::Preference => "preference",
            Self::Habit => "habit",
            Self::Constraint => "constraint",
        }
    }
}

/// How a revision earned authority. Evidence frequency is never authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityClass {
    OwnerAuthored,
    OwnerRatified,
}

impl AuthorityClass {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OwnerAuthored => "owner_authored",
            Self::OwnerRatified => "owner_ratified",
        }
    }
}

/// An explicit owner decision on an observation candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAction {
    Accept,
    Reject,
    Narrow,
    Supersede,
}

impl ReviewAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Reject => "reject",
            Self::Narrow => "narrow",
            Self::Supersede => "supersede",
        }
    }
}

/// An observation awaiting review. It is never authoritative by itself.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserModelCandidate {
    pub id: String,
    pub kind: UserModelKind,
    pub statement: String,
    pub semantic_key: String,
    pub scope: String,
    pub evidence: String,
    pub created_at_unix: u64,
}

/// An append-only authoritative User Model revision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserModelRevision {
    pub id: String,
    pub semantic_key: String,
    pub kind: UserModelKind,
    pub statement: String,
    pub scope: String,
    pub authority: AuthorityClass,
    pub supersedes: Option<String>,
    pub valid_from_unix: u64,
    pub valid_until_unix: Option<u64>,
    pub source_candidate: Option<String>,
    pub created_at_unix: u64,
}

/// Multiple applicable graph heads for one semantic key.
///
/// Revision ids are evidence for owner review. Their order carries no
/// authority and must never be used to choose a winner.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserModelConflict {
    pub semantic_key: String,
    pub revision_ids: Vec<String>,
}

/// Conflict-aware result of a User Model read for one turn context.
///
/// `heads` contains only non-conflicted revisions applicable under the
/// supplied query context. Every semantic key in `conflicts` is withheld
/// until an owner-reviewed revision resolves the branch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserModelReadResult {
    pub heads: Vec<UserModelRevision>,
    pub conflicts: Vec<UserModelConflict>,
}

/// Receipt for an explicit review decision.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UserModelReviewReceipt {
    pub id: String,
    pub candidate_id: String,
    pub action: ReviewAction,
    pub reviewer: String,
    pub note: Option<String>,
    pub at_unix: u64,
}

/// Candidate plus its append-only review history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserModelCandidateHistory {
    pub candidate: UserModelCandidate,
    pub review_receipts: Vec<UserModelReviewReceipt>,
}

/// Current turn identity used by the existing scope applicability rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserModelQueryContext {
    pub agent_id: String,
    pub channel_id: String,
    pub session_id: String,
}

impl UserModelQueryContext {
    #[must_use]
    pub fn new(agent_id: &str, channel_id: &str, session_id: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            channel_id: channel_id.to_string(),
            session_id: session_id.to_string(),
        }
    }
}

/// Typed failures at the User Model domain-service boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserModelError {
    Unavailable(String),
    Read(String),
    Write(String),
    DomainNotFound { entity: &'static str, id: String },
    CandidateAlreadyReviewed,
    UnresolvedConflict(UserModelConflict),
}

impl fmt::Display for UserModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "user model unavailable: {message}"),
            Self::Read(message) => write!(formatter, "user model read failed: {message}"),
            Self::Write(message) => write!(formatter, "user model write failed: {message}"),
            Self::DomainNotFound { entity, id } => write!(formatter, "unknown {entity} '{id}'"),
            Self::CandidateAlreadyReviewed => formatter.write_str("candidate already reviewed"),
            Self::UnresolvedConflict(conflict) => write!(
                formatter,
                "unresolved user model conflict for '{}' ({} active heads)",
                conflict.semantic_key,
                conflict.revision_ids.len()
            ),
        }
    }
}

impl std::error::Error for UserModelError {}
