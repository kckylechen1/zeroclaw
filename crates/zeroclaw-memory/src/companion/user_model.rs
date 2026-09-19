//! Local-first User Model authority: owner values, goals,
//! preferences as governed, append-only records — not an inferred profile
//! and not a generic memcore category.
//!
//! Authority rules (frozen in the User Model spec):
//! - An explicit owner-authored statement may become an active revision
//!   immediately.
//! - An observation is ALWAYS a candidate; no amount of repetition
//!   promotes it. Only an explicit review action (`accept`/`narrow`) can.
//! - `reject` records the decision without deleting evidence.
//! - `supersede` appends a new revision; history is never rewritten.
//! - Works fully offline; nothing here requires Tachi.

use std::fmt;
use std::fmt::Write as _;

use super::user_model_scope::{ApplicabilityContext, Scope};
use std::path::Path;

use parking_lot::Mutex;
use rusqlite::Connection;
pub use zeroclaw_api::user_model::{
    AuthorityClass, ReviewAction, UserModelCandidate, UserModelCandidateHistory, UserModelError,
    UserModelKind, UserModelQueryContext, UserModelReviewReceipt, UserModelRevision,
};
use zeroclaw_infra::sqlite_perms::harden_sqlite_owner_only;

/// A committed review already decides this candidate. The one supported
/// follow-up is narrowing a rejected candidate.
#[derive(Debug)]
struct CandidateAlreadyReviewed;

impl fmt::Display for CandidateAlreadyReviewed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("candidate already reviewed")
    }
}

impl std::error::Error for CandidateAlreadyReviewed {}

/// Classify the store's typed review conflict without matching error text.
pub fn is_candidate_already_reviewed(error: &rusqlite::Error) -> bool {
    matches!(error, rusqlite::Error::ToSqlConversionFailure(inner) if inner.is::<CandidateAlreadyReviewed>())
}

/// Append-only sqlite store for the User Model (`user_model.db` under the
/// companion data dir; owner-only, WAL).
pub struct UserModelStore {
    conn: Mutex<Connection>,
}

impl UserModelStore {
    /// Open (or create) the store. Blocking; call outside async contexts
    /// or via `spawn_blocking`.
    pub fn open(data_dir: &Path) -> Result<Self, rusqlite::Error> {
        std::fs::create_dir_all(data_dir)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let db_path = data_dir.join("user_model.db");
        create_owner_only_file(&db_path)?;
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS user_model_candidates (
                id TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                statement TEXT NOT NULL,
                semantic_key TEXT NOT NULL,
                scope TEXT NOT NULL DEFAULT 'global',
                evidence TEXT NOT NULL DEFAULT '[]',
                created_at_unix INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS user_model_revisions (
                id TEXT PRIMARY KEY,
                semantic_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                statement TEXT NOT NULL,
                scope TEXT NOT NULL DEFAULT 'global',
                authority TEXT NOT NULL,
                supersedes TEXT,
                valid_from_unix INTEGER NOT NULL,
                valid_until_unix INTEGER,
                source_candidate TEXT,
                created_at_unix INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS user_model_review_receipts (
                id TEXT PRIMARY KEY,
                candidate_id TEXT NOT NULL,
                action TEXT NOT NULL,
                reviewer TEXT NOT NULL,
                note TEXT,
                at_unix INTEGER NOT NULL
             );",
        )?;
        harden_sqlite_owner_only(&db_path);
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Record an explicit owner-authored statement and make it the active
    /// revision for its semantic key immediately (local-first; no review
    /// round-trip required when the owner speaks directly).
    ///
    /// Lookup and insert share ONE lock scope so two concurrent writers on
    /// the same key cannot both supersede the same prior revision.
    pub fn record_owner_statement(
        &self,
        kind: UserModelKind,
        statement: &str,
        semantic_key: &str,
        scope: &str,
        now_unix: u64,
    ) -> Result<UserModelRevision, rusqlite::Error> {
        if Scope::parse(scope).is_none() {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "invalid scope '{scope}': global | agent:<id> | channel:<id> | session:<id>"
            )));
        }
        let conn = self.conn.lock();
        let supersedes: Option<String> = conn
            .query_row(
                "SELECT id FROM user_model_revisions
                 WHERE semantic_key = ?1 AND valid_from_unix <= ?2
                 ORDER BY created_at_unix DESC, id DESC LIMIT 1",
                rusqlite::params![semantic_key, now_unix],
                |row| row.get(0),
            )
            .map(Some)
            .or_else(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let revision = UserModelRevision {
            id: uuid::Uuid::new_v4().to_string(),
            semantic_key: semantic_key.to_string(),
            kind,
            statement: statement.to_string(),
            scope: scope.to_string(),
            authority: AuthorityClass::OwnerAuthored,
            supersedes,
            valid_from_unix: now_unix,
            valid_until_unix: None,
            source_candidate: None,
            created_at_unix: now_unix,
        };
        conn.execute(
            "INSERT INTO user_model_revisions
                 (id, semantic_key, kind, statement, scope, authority, supersedes,
                  valid_from_unix, valid_until_unix, source_candidate, created_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                revision.id,
                revision.semantic_key,
                revision.kind.as_str(),
                revision.statement,
                revision.scope,
                revision.authority.as_str(),
                revision.supersedes,
                revision.valid_from_unix,
                revision.valid_until_unix,
                revision.source_candidate,
                revision.created_at_unix,
            ],
        )?;
        Ok(revision)
    }

    /// Record an observation as a candidate. Repeated observations only
    /// add evidence; there is deliberately no API that promotes a
    /// candidate by count, frequency, or confidence.
    pub fn record_observation(
        &self,
        kind: UserModelKind,
        statement: &str,
        semantic_key: &str,
        evidence: &str,
        now_unix: u64,
    ) -> Result<UserModelCandidate, rusqlite::Error> {
        let candidate = UserModelCandidate {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            statement: statement.to_string(),
            semantic_key: semantic_key.to_string(),
            scope: "global".to_string(),
            evidence: evidence.to_string(),
            created_at_unix: now_unix,
        };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO user_model_candidates
                 (id, kind, statement, semantic_key, scope, evidence, created_at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                candidate.id,
                candidate.kind.as_str(),
                candidate.statement,
                candidate.semantic_key,
                candidate.scope,
                candidate.evidence,
                candidate.created_at_unix,
            ],
        )?;
        Ok(candidate)
    }

    /// All candidates, newest first, with their evidence.
    pub fn list_candidates(&self) -> Result<Vec<UserModelCandidate>, rusqlite::Error> {
        self.list_candidates_by_review(false)
    }

    /// Candidates with no committed review receipt, in history order.
    pub fn list_pending_candidates(&self) -> Result<Vec<UserModelCandidate>, rusqlite::Error> {
        self.list_candidates_by_review(true)
    }

    fn list_candidates_by_review(
        &self,
        pending: bool,
    ) -> Result<Vec<UserModelCandidate>, rusqlite::Error> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, kind, statement, semantic_key, scope, evidence, created_at_unix
             FROM user_model_candidates c
             WHERE (?1 = 0 OR NOT EXISTS (
                 SELECT 1 FROM user_model_review_receipts r WHERE r.candidate_id = c.id
             ))
             ORDER BY created_at_unix DESC, id DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![pending], |row| {
            let kind_raw: String = row.get(1)?;
            Ok((UserModelCandidate {
                id: row.get(0)?,
                kind: kind_from_str(&kind_raw).ok_or(rusqlite::Error::QueryReturnedNoRows)?,
                statement: row.get(2)?,
                semantic_key: row.get(3)?,
                scope: row.get(4)?,
                evidence: row.get(5)?,
                created_at_unix: row.get::<_, i64>(6)?.max(0) as u64,
            },))
        })?;
        let mut candidates = Vec::new();
        for row in rows {
            candidates.push(row?.0);
        }
        Ok(candidates)
    }

    /// One candidate and its committed review receipts in insertion order.
    /// The read transaction keeps the candidate and receipts on one
    /// committed snapshot, including after a store reopen.
    pub fn candidate_history(
        &self,
        candidate_id: &str,
    ) -> Result<Option<(UserModelCandidate, Vec<UserModelReviewReceipt>)>, rusqlite::Error> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let candidate = match tx.query_row(
            "SELECT id, kind, statement, semantic_key, scope, evidence, created_at_unix
             FROM user_model_candidates WHERE id = ?1",
            rusqlite::params![candidate_id],
            |row| {
                let kind_raw: String = row.get(1)?;
                Ok(UserModelCandidate {
                    id: row.get(0)?,
                    kind: kind_from_str(&kind_raw).ok_or(rusqlite::Error::InvalidQuery)?,
                    statement: row.get(2)?,
                    semantic_key: row.get(3)?,
                    scope: row.get(4)?,
                    evidence: row.get(5)?,
                    created_at_unix: row.get::<_, i64>(6)?.max(0) as u64,
                })
            },
        ) {
            Ok(candidate) => candidate,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(error) => return Err(error),
        };
        let receipts = {
            let mut stmt = tx.prepare(
                "SELECT id, candidate_id, action, reviewer, note, at_unix
                 FROM user_model_review_receipts
                 WHERE candidate_id = ?1 ORDER BY rowid ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![candidate_id], |row| {
                let action_raw: String = row.get(2)?;
                let action = match action_raw.as_str() {
                    "accept" => ReviewAction::Accept,
                    "reject" => ReviewAction::Reject,
                    "narrow" => ReviewAction::Narrow,
                    "supersede" => ReviewAction::Supersede,
                    _ => return Err(rusqlite::Error::InvalidQuery),
                };
                Ok(UserModelReviewReceipt {
                    id: row.get(0)?,
                    candidate_id: row.get(1)?,
                    action,
                    reviewer: row.get(3)?,
                    note: row.get(4)?,
                    at_unix: row.get::<_, i64>(5)?.max(0) as u64,
                })
            })?;
            let mut receipts = Vec::new();
            for row in rows {
                receipts.push(row?);
            }
            receipts
        };
        tx.commit()?;
        Ok(Some((candidate, receipts)))
    }

    /// Apply an eligible review action to a candidate. A successful action
    /// writes a receipt; `accept`/`narrow`/`supersede` additionally append a
    /// revision. `reject` never deletes the candidate or its evidence.
    pub fn review_candidate(
        &self,
        candidate_id: &str,
        action: ReviewAction,
        reviewer: &str,
        note: Option<&str>,
        narrowed_scope: Option<&str>,
        now_unix: u64,
    ) -> Result<UserModelReviewReceipt, rusqlite::Error> {
        let mut conn = self.conn.lock();
        // Acquire the database write slot before reading the decision so a
        // second store connection cannot act on the same pending snapshot.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let candidate = tx.query_row(
            "SELECT kind, statement, semantic_key FROM user_model_candidates WHERE id = ?1",
            rusqlite::params![candidate_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        let kind =
            kind_from_str(&candidate.0).ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;

        let last_action = match tx.query_row(
            "SELECT action FROM user_model_review_receipts
             WHERE candidate_id = ?1 ORDER BY rowid DESC LIMIT 1",
            rusqlite::params![candidate_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(action) => Some(action),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(error) => return Err(error),
        };
        if let Some(last_action) = last_action {
            let prior = match last_action.as_str() {
                "accept" => ReviewAction::Accept,
                "reject" => ReviewAction::Reject,
                "narrow" => ReviewAction::Narrow,
                "supersede" => ReviewAction::Supersede,
                _ => return Err(rusqlite::Error::InvalidQuery),
            };
            if prior != ReviewAction::Reject || action != ReviewAction::Narrow {
                // Preserve callers' rusqlite::Error contract while carrying
                // an exact domain marker. This is not a SQL conversion error.
                return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                    CandidateAlreadyReviewed,
                )));
            }
        }
        // Validate only actions that are still eligible. Reject ignores
        // narrowed_scope; the other actions retain their existing scope rules.
        let scope = match action {
            ReviewAction::Narrow => narrowed_scope.unwrap_or("global"),
            _ => "global",
        };
        if action != ReviewAction::Reject && Scope::parse(scope).is_none() {
            return Err(rusqlite::Error::InvalidParameterName(format!(
                "invalid narrowed scope '{scope}'"
            )));
        }
        let receipt = UserModelReviewReceipt {
            id: uuid::Uuid::new_v4().to_string(),
            candidate_id: candidate_id.to_string(),
            action,
            reviewer: reviewer.to_string(),
            note: note.map(str::to_string),
            at_unix: now_unix,
        };
        tx.execute(
            "INSERT INTO user_model_review_receipts
                 (id, candidate_id, action, reviewer, note, at_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                receipt.id,
                receipt.candidate_id,
                receipt.action.as_str(),
                receipt.reviewer,
                receipt.note,
                receipt.at_unix,
            ],
        )?;

        match action {
            ReviewAction::Reject => {}
            ReviewAction::Accept | ReviewAction::Narrow | ReviewAction::Supersede => {
                tx.execute(
                    "INSERT INTO user_model_revisions
                         (id, semantic_key, kind, statement, scope, authority, supersedes,
                          valid_from_unix, valid_until_unix, source_candidate, created_at_unix)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                             (SELECT id FROM user_model_revisions
                              WHERE semantic_key = ?2
                                AND valid_from_unix <= ?7
                                AND (valid_until_unix IS NULL OR valid_until_unix > ?7)
                              ORDER BY created_at_unix DESC, id DESC LIMIT 1),
                             ?7, NULL, ?8, ?7)",
                    rusqlite::params![
                        uuid::Uuid::new_v4().to_string(),
                        candidate.2,
                        kind.as_str(),
                        candidate.1,
                        scope,
                        AuthorityClass::OwnerRatified.as_str(),
                        now_unix,
                        candidate_id,
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(receipt)
    }

    /// Active, applicable revisions as of `as_of_unix` (`None` = now).
    /// Per semantic key the latest revision valid at that instant wins;
    /// superseded and expired revisions are simply older history.
    pub fn active_heads(
        &self,
        as_of_unix: Option<u64>,
    ) -> Result<Vec<UserModelRevision>, rusqlite::Error> {
        let as_of = match as_of_unix {
            Some(as_of) => as_of,
            None => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?
                    .as_secs();
                // Validate SQLite's signed range without changing explicit as-of semantics.
                i64::try_from(now)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                now
            }
        };
        let conn = self.conn.lock();
        // Per key: take the newest revision that had already started at the
        // read instant, THEN gate it on its own validity window. A key whose
        // newest revision expired goes inactive — an older superseded
        // revision must never resurface through the gap.
        let mut stmt = conn.prepare(
            "SELECT id, semantic_key, kind, statement, scope, authority, supersedes,
                    valid_from_unix, valid_until_unix, source_candidate, created_at_unix
             FROM user_model_revisions r
             WHERE r.valid_from_unix <= ?1
               AND r.created_at_unix = (
                   SELECT MAX(r2.created_at_unix) FROM user_model_revisions r2
                   WHERE r2.semantic_key = r.semantic_key
                     AND r2.valid_from_unix <= ?1
               )
               AND (r.valid_until_unix IS NULL OR r.valid_until_unix > ?1)
             ORDER BY r.created_at_unix DESC, r.id DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![as_of], revision_from_row)?;
        let mut seen = std::collections::HashSet::new();
        let mut heads = Vec::new();
        for row in rows {
            let revision = row?;
            if seen.insert(revision.semantic_key.clone()) {
                heads.push(revision);
            }
        }
        heads.sort_by(|a, b| a.semantic_key.cmp(&b.semantic_key));
        Ok(heads)
    }
}

/// Canonical domain seam used by channels, gateway APIs, and prompt assembly.
/// Implementations own persistence access; consumers never open a store or
/// derive a database path.
pub trait UserModelService: Send + Sync {
    fn record_owner_statement(
        &self,
        kind: UserModelKind,
        statement: &str,
        semantic_key: &str,
        scope: &str,
        now_unix: u64,
    ) -> Result<UserModelRevision, UserModelError>;

    fn record_observation(
        &self,
        kind: UserModelKind,
        statement: &str,
        semantic_key: &str,
        evidence: &str,
        now_unix: u64,
    ) -> Result<UserModelCandidate, UserModelError>;

    fn review_candidate(
        &self,
        candidate_id: &str,
        action: ReviewAction,
        reviewer: &str,
        note: Option<&str>,
        narrowed_scope: Option<&str>,
        now_unix: u64,
    ) -> Result<UserModelReviewReceipt, UserModelError>;

    fn query_candidates(&self) -> Result<Vec<UserModelCandidate>, UserModelError>;

    fn query_pending_review(&self) -> Result<Vec<UserModelCandidate>, UserModelError>;

    fn query_candidate_history(
        &self,
        candidate_id: &str,
    ) -> Result<UserModelCandidateHistory, UserModelError>;

    fn query_active_heads(
        &self,
        as_of_unix: Option<u64>,
    ) -> Result<Vec<UserModelRevision>, UserModelError>;

    fn query_applicable_heads(
        &self,
        context: &UserModelQueryContext,
        as_of_unix: Option<u64>,
    ) -> Result<Vec<UserModelRevision>, UserModelError>;
}

/// Transitional adapter over the existing SQLite User Model store. It introduces no new
/// tables or lifecycle semantics and is the only production backend in this
/// delivery.
pub struct LegacySqliteBackend {
    store: UserModelStore,
}

impl LegacySqliteBackend {
    /// Open the compatibility store for one runtime generation.
    pub fn open(data_dir: &Path) -> Result<Self, UserModelError> {
        UserModelStore::open(data_dir)
            .map(|store| Self { store })
            .map_err(|error| UserModelError::Unavailable(error.to_string()))
    }
}

impl UserModelService for LegacySqliteBackend {
    fn record_owner_statement(
        &self,
        kind: UserModelKind,
        statement: &str,
        semantic_key: &str,
        scope: &str,
        now_unix: u64,
    ) -> Result<UserModelRevision, UserModelError> {
        self.store
            .record_owner_statement(kind, statement, semantic_key, scope, now_unix)
            .map_err(|error| UserModelError::Write(error.to_string()))
    }

    fn record_observation(
        &self,
        kind: UserModelKind,
        statement: &str,
        semantic_key: &str,
        evidence: &str,
        now_unix: u64,
    ) -> Result<UserModelCandidate, UserModelError> {
        self.store
            .record_observation(kind, statement, semantic_key, evidence, now_unix)
            .map_err(|error| UserModelError::Write(error.to_string()))
    }

    fn review_candidate(
        &self,
        candidate_id: &str,
        action: ReviewAction,
        reviewer: &str,
        note: Option<&str>,
        narrowed_scope: Option<&str>,
        now_unix: u64,
    ) -> Result<UserModelReviewReceipt, UserModelError> {
        self.store
            .review_candidate(
                candidate_id,
                action,
                reviewer,
                note,
                narrowed_scope,
                now_unix,
            )
            .map_err(|error| {
                if matches!(error, rusqlite::Error::QueryReturnedNoRows) {
                    UserModelError::DomainNotFound {
                        entity: "candidate id",
                        id: candidate_id.to_string(),
                    }
                } else if is_candidate_already_reviewed(&error) {
                    UserModelError::CandidateAlreadyReviewed
                } else {
                    UserModelError::Write(error.to_string())
                }
            })
    }

    fn query_candidates(&self) -> Result<Vec<UserModelCandidate>, UserModelError> {
        self.store
            .list_candidates()
            .map_err(|error| UserModelError::Read(error.to_string()))
    }

    fn query_pending_review(&self) -> Result<Vec<UserModelCandidate>, UserModelError> {
        self.store
            .list_pending_candidates()
            .map_err(|error| UserModelError::Read(error.to_string()))
    }

    fn query_candidate_history(
        &self,
        candidate_id: &str,
    ) -> Result<UserModelCandidateHistory, UserModelError> {
        self.store
            .candidate_history(candidate_id)
            .map_err(|error| UserModelError::Read(error.to_string()))?
            .map(|(candidate, review_receipts)| UserModelCandidateHistory {
                candidate,
                review_receipts,
            })
            .ok_or_else(|| UserModelError::DomainNotFound {
                entity: "candidate id",
                id: candidate_id.to_string(),
            })
    }

    fn query_active_heads(
        &self,
        as_of_unix: Option<u64>,
    ) -> Result<Vec<UserModelRevision>, UserModelError> {
        self.store
            .active_heads(as_of_unix)
            .map_err(|error| UserModelError::Read(error.to_string()))
    }

    fn query_applicable_heads(
        &self,
        context: &UserModelQueryContext,
        as_of_unix: Option<u64>,
    ) -> Result<Vec<UserModelRevision>, UserModelError> {
        let applicability =
            ApplicabilityContext::new(&context.agent_id, &context.channel_id, &context.session_id);
        self.query_active_heads(as_of_unix).map(|heads| {
            heads
                .into_iter()
                .filter(|revision| applicability.applies_str(&revision.scope))
                .collect()
        })
    }
}

/// Default character budget for the projected prompt section. The
/// projection is bounded so a large active set can never crowd out the
/// rest of the system prompt.
pub const USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS: usize = 1_200;

/// The rendered projection plus the revision ids behind it. The ids are
/// for internal correction paths (logging, review UI) — they are NOT
/// embedded in the prompt text the model sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserModelStateProjection {
    /// Ready-to-append prompt section; empty when nothing is active.
    pub prompt_section: String,
    /// Active revision ids backing the section, newest first.
    pub revision_ids: Vec<String>,
}

/// Render the active heads into a bounded prompt section. Newest heads get
/// the budget first; anything that no longer fits is elided with a count.
pub fn project_active_heads(
    heads: &[UserModelRevision],
    max_chars: usize,
) -> UserModelStateProjection {
    if heads.is_empty() {
        return UserModelStateProjection {
            prompt_section: String::new(),
            revision_ids: Vec::new(),
        };
    }
    let mut ordered: Vec<&UserModelRevision> = heads.iter().collect();
    ordered.sort_by(|a, b| {
        b.created_at_unix
            .cmp(&a.created_at_unix)
            .then(a.id.cmp(&b.id))
    });

    let mut section = String::from("## Owner profile (authoritative)\n");
    let mut included_ids = Vec::new();
    let mut elided = 0usize;
    for revision in ordered {
        let line = format!(
            "- {}: {} [scope: {}]\n",
            revision.kind.as_str(),
            revision.statement,
            revision.scope
        );
        if section.len() + line.len() > max_chars {
            elided += 1;
            continue;
        }
        section.push_str(&line);
        included_ids.push(revision.id.clone());
    }
    if elided > 0 {
        let _ = writeln!(section, "- (+{elided} elided; review to trim)");
    }
    UserModelStateProjection {
        prompt_section: section,
        revision_ids: included_ids,
    }
}

fn kind_from_str(raw: &str) -> Option<UserModelKind> {
    match raw {
        "value" => Some(UserModelKind::Value),
        "goal" => Some(UserModelKind::Goal),
        "preference" => Some(UserModelKind::Preference),
        "habit" => Some(UserModelKind::Habit),
        "constraint" => Some(UserModelKind::Constraint),
        _ => None,
    }
}

fn authority_from_str(raw: &str) -> Option<AuthorityClass> {
    match raw {
        "owner_authored" => Some(AuthorityClass::OwnerAuthored),
        "owner_ratified" => Some(AuthorityClass::OwnerRatified),
        _ => None,
    }
}

fn revision_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserModelRevision> {
    let kind_raw: String = row.get(2)?;
    let authority_raw: String = row.get(5)?;
    Ok(UserModelRevision {
        id: row.get(0)?,
        semantic_key: row.get(1)?,
        kind: kind_from_str(&kind_raw).ok_or(rusqlite::Error::QueryReturnedNoRows)?,
        statement: row.get(3)?,
        scope: row.get(4)?,
        authority: authority_from_str(&authority_raw)
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?,
        supersedes: row.get(6)?,
        valid_from_unix: row.get::<_, i64>(7)?.max(0) as u64,
        valid_until_unix: row.get::<_, Option<i64>>(8)?.map(|v| v.max(0) as u64),
        source_candidate: row.get(9)?,
        created_at_unix: row.get::<_, i64>(10)?.max(0) as u64,
    })
}

fn create_owner_only_file(path: &Path) -> Result<(), rusqlite::Error> {
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    harden_sqlite_owner_only(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, UserModelStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = UserModelStore::open(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn service_boundary_reports_typed_open_write_read_and_not_found_failures() {
        let blocked = tempfile::tempdir().unwrap();
        let file_path = blocked.path().join("not-a-directory");
        std::fs::write(&file_path, b"occupied").unwrap();
        assert!(matches!(
            LegacySqliteBackend::open(&file_path),
            Err(UserModelError::Unavailable(_))
        ));

        let dir = tempfile::tempdir().unwrap();
        let service = LegacySqliteBackend::open(dir.path()).unwrap();
        assert!(matches!(
            service.record_owner_statement(
                UserModelKind::Preference,
                "brief",
                "response.style",
                "unsupported:scope",
                100,
            ),
            Err(UserModelError::Write(_))
        ));
        assert_eq!(
            service.query_candidate_history("missing"),
            Err(UserModelError::DomainNotFound {
                entity: "candidate id",
                id: "missing".to_string(),
            })
        );

        let fixture = rusqlite::Connection::open(dir.path().join("user_model.db")).unwrap();
        fixture
            .execute_batch("DROP TABLE user_model_candidates;")
            .unwrap();
        assert!(matches!(
            service.query_candidates(),
            Err(UserModelError::Read(_))
        ));
    }

    /// Discrimination 1: an explicit owner statement becomes active
    /// immediately, with no Tachi and no review round-trip.
    #[test]
    fn owner_statement_becomes_active_immediately() {
        let (_dir, s) = store();
        let revision = s
            .record_owner_statement(
                UserModelKind::Preference,
                "Always give me the engineering conclusion first.",
                "communication.conclusion-first",
                "global",
                1_000,
            )
            .unwrap();
        assert_eq!(revision.authority, AuthorityClass::OwnerAuthored);
        let heads = s.active_heads(Some(1_000)).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].semantic_key, "communication.conclusion-first");
        assert_eq!(
            heads[0].statement,
            "Always give me the engineering conclusion first."
        );
    }

    /// Discrimination 2: observations remain candidates no matter how many
    /// times they repeat; nothing is ever active without an explicit
    /// owner action.
    #[test]
    fn repeated_observations_never_become_active() {
        let (_dir, s) = store();
        for i in 0..25 {
            s.record_observation(
                UserModelKind::Habit,
                "User keeps reformatting tables manually.",
                "formatting.tables",
                &format!("[\"turn-{i}\"]"),
                1_000 + i,
            )
            .unwrap();
        }
        assert!(
            s.active_heads(None).unwrap().is_empty(),
            "observations must never auto-promote to active heads"
        );
        // Even after accepting one sibling candidate, later repetitions
        // stay candidates; the active head stays exactly the ratified one.
        let accepted = s
            .record_observation(
                UserModelKind::Habit,
                "User keeps reformatting tables manually.",
                "formatting.tables",
                "[\"turn-99\"]",
                2_000,
            )
            .unwrap();
        s.review_candidate(
            &accepted.id,
            ReviewAction::Accept,
            "owner",
            None,
            None,
            2_100,
        )
        .unwrap();
        s.record_observation(
            UserModelKind::Habit,
            "User keeps reformatting tables manually.",
            "formatting.tables",
            "[\"turn-100\"]",
            3_000,
        )
        .unwrap();
        let heads = s.active_heads(None).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].authority, AuthorityClass::OwnerRatified);
    }

    /// Discrimination 4: accept / reject / narrow / supersede produce
    /// distinct, append-only histories.
    #[test]
    fn review_actions_produce_distinct_histories() {
        let (_dir, s) = store();
        let candidate = s
            .record_observation(
                UserModelKind::Preference,
                "Prefers concise summaries.",
                "communication.summary-length",
                "[]",
                1_000,
            )
            .unwrap();

        let rejected = s
            .review_candidate(
                &candidate.id,
                ReviewAction::Reject,
                "owner",
                Some("not a habit"),
                None,
                2_000,
            )
            .unwrap();
        assert_eq!(rejected.action, ReviewAction::Reject);
        assert!(
            s.active_heads(Some(2_000)).unwrap().is_empty(),
            "reject must not activate anything"
        );
        // The candidate and its evidence survive the rejection.
        let conn = s.conn.lock();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM user_model_candidates WHERE id = ?1",
                rusqlite::params![candidate.id],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(count, 1, "reject must not delete the candidate");

        let narrowed = s
            .review_candidate(
                &candidate.id,
                ReviewAction::Narrow,
                "owner",
                None,
                Some("session:trading"),
                3_000,
            )
            .unwrap();
        assert_eq!(narrowed.action, ReviewAction::Narrow);
        let heads = s.active_heads(Some(3_000)).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].scope, "session:trading");

        let superseded_by = s.record_owner_statement(
            UserModelKind::Preference,
            "Prefers detailed summaries in deep-dive sessions.",
            "communication.summary-length",
            "global",
            4_000,
        );
        let superseded_by = superseded_by.unwrap();
        assert!(
            superseded_by.supersedes.is_some(),
            "a new statement must supersede the prior active revision"
        );
        // Append-only: every receipt and revision still exists.
        let conn = s.conn.lock();
        let receipts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM user_model_review_receipts",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let revisions: i64 = conn
            .query_row("SELECT COUNT(*) FROM user_model_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        drop(conn);
        assert_eq!(receipts, 2);
        assert_eq!(revisions, 2);
    }

    #[test]
    fn candidate_history_uses_receipt_insertion_order_after_reopen() {
        let (dir, store) = store();
        let candidate = store
            .record_observation(UserModelKind::Habit, "habit", "habit.key", "[]", 100)
            .unwrap();
        store
            .conn
            .lock()
            .execute(
                "INSERT INTO user_model_review_receipts
                 (id, candidate_id, action, reviewer, note, at_unix)
                 VALUES ('zzzz-first', ?1, 'reject', 'owner', NULL, 200)",
                rusqlite::params![candidate.id],
            )
            .unwrap();
        let narrowed = store
            .review_candidate(
                &candidate.id,
                ReviewAction::Narrow,
                "owner",
                None,
                Some("session:A"),
                200,
            )
            .unwrap();
        assert!(narrowed.id.as_str() < "zzzz-first");
        store
            .conn
            .lock()
            .execute(
                "INSERT INTO user_model_review_receipts
                 (id, candidate_id, action, reviewer, note, at_unix)
                 VALUES ('aaaa-backdated', ?1, 'reject', 'owner', NULL, 199)",
                rusqlite::params![candidate.id],
            )
            .unwrap();
        drop(store);

        let reopened = UserModelStore::open(dir.path()).unwrap();
        let before = review_scope_snapshot(&reopened);
        let (found, receipts) = reopened.candidate_history(&candidate.id).unwrap().unwrap();
        assert_eq!(found, candidate);
        assert_eq!(
            receipts
                .iter()
                .map(|receipt| receipt.id.as_str())
                .collect::<Vec<_>>(),
            vec!["zzzz-first", narrowed.id.as_str(), "aaaa-backdated"]
        );
        assert_eq!(receipts.last().unwrap().action, ReviewAction::Reject);
        assert_eq!(review_scope_snapshot(&reopened), before);
    }

    /// Discrimination 5: as-of reads return the correct revision across a
    /// supersession boundary.
    #[test]
    fn as_of_reads_respect_supersession() {
        let (_dir, s) = store();
        let first = s
            .record_owner_statement(
                UserModelKind::Goal,
                "Ship the A-share harness first.",
                "goal.priority",
                "global",
                1_000,
            )
            .unwrap();
        s.record_owner_statement(
            UserModelKind::Goal,
            "Ship the companion memory first.",
            "goal.priority",
            "global",
            3_000,
        )
        .unwrap();

        let at_start = s.active_heads(Some(1_500)).unwrap();
        assert_eq!(at_start.len(), 1);
        assert_eq!(
            at_start[0].id, first.id,
            "before supersession the first revision is the head"
        );

        let now = s.active_heads(Some(3_500)).unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].statement, "Ship the companion memory first.");

        let before_anything = s.active_heads(Some(500)).unwrap();
        assert!(before_anything.is_empty());
    }

    /// Expiry: a revision with valid_until stops being projected after it
    /// expires (task-scoped requests ride the same mechanism).
    #[test]
    fn expired_revisions_leave_the_active_heads() {
        let (_dir, s) = store();
        s.record_owner_statement(
            UserModelKind::Preference,
            "For this task use Codex.",
            "task.model-choice",
            "session:one-off",
            1_000,
        )
        .unwrap();
        let heads = s.active_heads(Some(1_200)).unwrap();
        assert_eq!(heads.len(), 1);

        // Supersede it with a bounded (expiring) revision, then read past
        // its end: nothing may be active for that key anymore.
        let bounded = s
            .record_owner_statement(
                UserModelKind::Preference,
                "For this task use Codex.",
                "task.model-choice",
                "session:one-off",
                2_000,
            )
            .unwrap();
        let conn = s.conn.lock();
        conn.execute(
            "UPDATE user_model_revisions SET valid_until_unix = ?1 WHERE id = ?2",
            rusqlite::params![3_000, bounded.id],
        )
        .unwrap();
        drop(conn);
        assert!(s.active_heads(Some(3_500)).unwrap().is_empty());
        assert_eq!(s.active_heads(Some(2_500)).unwrap().len(), 1);
    }

    /// Same-timestamp revisions for one key must resolve to exactly one
    /// head, stably across reads (tie broken by id, deterministic).
    #[test]
    fn same_timestamp_tie_resolves_to_one_stable_head() {
        let (_dir, s) = store();
        s.record_owner_statement(
            UserModelKind::Preference,
            "First statement in the same second.",
            "communication.tie",
            "global",
            5_000,
        )
        .unwrap();
        s.record_owner_statement(
            UserModelKind::Preference,
            "Second statement in the same second.",
            "communication.tie",
            "global",
            5_000,
        )
        .unwrap();
        let first_read = s.active_heads(Some(5_000)).unwrap();
        assert_eq!(first_read.len(), 1, "a tie must yield exactly one head");
        let second_read = s.active_heads(Some(5_000)).unwrap();
        assert_eq!(
            first_read[0].id, second_read[0].id,
            "the tie winner must be stable across reads"
        );
    }

    /// Projection: empty heads render nothing; the section is bounded with
    /// newest-first priority and an elision count; ids ride alongside for
    /// internal correction without entering the prompt text.
    #[test]
    fn projection_is_bounded_and_reports_ids() {
        let (_dir, s) = store();
        for i in 0..40 {
            s.record_owner_statement(
                UserModelKind::Preference,
                &format!("Preference number {i} with some words to consume budget."),
                &format!("pref.{i}"),
                "global",
                1_000 + i,
            )
            .unwrap();
        }
        let heads = s.active_heads(None).unwrap();
        let projection = project_active_heads(&heads, 600);
        assert!(
            projection.prompt_section.len() <= 700,
            "section must stay near the budget"
        );
        assert!(
            projection.prompt_section.contains("elided"),
            "a 40-head set over a 600-char budget must elide"
        );
        assert!(
            !projection.prompt_section.contains(&heads[0].id),
            "revision ids must not enter the prompt text"
        );
        assert!(!projection.revision_ids.is_empty());

        let empty = project_active_heads(&[], 600);
        assert!(empty.prompt_section.is_empty());
        assert!(empty.revision_ids.is_empty());
    }

    #[test]
    fn projection_prefers_newest_heads() {
        let (_dir, s) = store();
        s.record_owner_statement(
            UserModelKind::Value,
            "Old value statement.",
            "value.only",
            "global",
            1_000,
        )
        .unwrap();
        s.record_owner_statement(
            UserModelKind::Goal,
            "Newest goal statement.",
            "goal.only",
            "global",
            2_000,
        )
        .unwrap();
        let heads = s.active_heads(None).unwrap();
        let projection = project_active_heads(&heads, 4_000);
        let goal_pos = projection
            .prompt_section
            .find("Newest goal statement.")
            .unwrap();
        let value_pos = projection
            .prompt_section
            .find("Old value statement.")
            .unwrap();
        assert!(
            goal_pos < value_pos,
            "newest revisions must be rendered first"
        );
        assert_eq!(projection.revision_ids.len(), 2);
    }

    /// Durable across reopen: local-first means no daemon lifetime magic.
    #[test]
    fn store_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = UserModelStore::open(dir.path()).unwrap();
            s.record_owner_statement(
                UserModelKind::Value,
                "Privacy over convenience.",
                "value.privacy",
                "global",
                1_000,
            )
            .unwrap();
        }
        let reopened = UserModelStore::open(dir.path()).unwrap();
        assert_eq!(reopened.active_heads(None).unwrap().len(), 1);
    }

    #[test]
    fn current_time_heads_include_live_windows_and_exclude_future_starts() {
        let (_dir, s) = store();
        assert!(s.active_heads(None).unwrap().is_empty());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let past = now.checked_sub(3_600).unwrap();
        let future = now.checked_add(3_600).unwrap();
        for (key, starts) in [
            ("active", past),
            ("future-start", future),
            ("future-expiry", past),
        ] {
            let revision = s
                .record_owner_statement(UserModelKind::Preference, key, key, "global", starts)
                .unwrap();
            if key == "future-expiry" {
                s.conn
                    .lock()
                    .execute(
                        "UPDATE user_model_revisions SET valid_until_unix = ?1 WHERE id = ?2",
                        rusqlite::params![future, revision.id],
                    )
                    .unwrap();
            }
        }
        let heads = s.active_heads(None).unwrap();
        let keys: Vec<_> = heads
            .iter()
            .map(|head| head.semantic_key.as_str())
            .collect();
        eprintln!(
            "USER_MODEL_CURRENT_TIME_OBSERVED active={} future_start={} future_expiry={}",
            keys.contains(&"active"),
            keys.contains(&"future-start"),
            keys.contains(&"future-expiry")
        );
        assert_eq!(keys, vec!["active", "future-expiry"]);
        assert_eq!(s.active_heads(Some(now)).unwrap(), heads);
    }
    fn review_scope_snapshot(store: &UserModelStore) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let conn = store.conn.lock();
        [
            "user_model_candidates",
            "user_model_review_receipts",
            "user_model_revisions",
        ]
        .iter()
        .map(|table| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY id"))
                .unwrap();
            let columns = stmt.column_count();
            stmt.query_map([], |row| (0..columns).map(|i| row.get(i)).collect())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .collect()
    }

    #[test]
    fn committed_review_repeats_preserve_all_rows_except_reject_then_narrow() {
        let (_dir, store) = store();
        for first in [
            ReviewAction::Accept,
            ReviewAction::Narrow,
            ReviewAction::Supersede,
        ] {
            let candidate = store
                .record_observation(
                    UserModelKind::Habit,
                    "habit",
                    &format!("key.{first:?}"),
                    "[]",
                    100,
                )
                .unwrap();
            store
                .review_candidate(&candidate.id, first, "owner", None, Some("session:A"), 200)
                .unwrap();
            let before = review_scope_snapshot(&store);
            for repeat in [
                ReviewAction::Accept,
                ReviewAction::Reject,
                ReviewAction::Narrow,
                ReviewAction::Supersede,
            ] {
                let error = store
                    .review_candidate(&candidate.id, repeat, "owner", None, Some("session:A"), 201)
                    .unwrap_err();
                assert!(
                    is_candidate_already_reviewed(&error),
                    "{first:?} then {repeat:?}: {error}"
                );
                assert_eq!(review_scope_snapshot(&store), before);
            }
        }

        let candidate = store
            .record_observation(UserModelKind::Habit, "rejected", "rejected.key", "[]", 100)
            .unwrap();
        store
            .review_candidate(
                &candidate.id,
                ReviewAction::Reject,
                "owner",
                None,
                None,
                200,
            )
            .unwrap();
        let rejected = review_scope_snapshot(&store);
        for repeat in [
            ReviewAction::Accept,
            ReviewAction::Reject,
            ReviewAction::Supersede,
        ] {
            let error = store
                .review_candidate(&candidate.id, repeat, "owner", None, None, 201)
                .unwrap_err();
            assert!(
                is_candidate_already_reviewed(&error),
                "Reject then {repeat:?}: {error}"
            );
            assert_eq!(review_scope_snapshot(&store), rejected);
        }
        let invalid = store
            .review_candidate(
                &candidate.id,
                ReviewAction::Narrow,
                "owner",
                None,
                Some("invalid"),
                201,
            )
            .unwrap_err();
        assert!(matches!(invalid, rusqlite::Error::InvalidParameterName(_)));
        assert_eq!(review_scope_snapshot(&store), rejected);
        store
            .review_candidate(
                &candidate.id,
                ReviewAction::Narrow,
                "owner",
                None,
                Some("session:A"),
                202,
            )
            .unwrap();
        let narrowed = review_scope_snapshot(&store);
        assert_eq!(narrowed[1].len(), rejected[1].len() + 1);
        assert_eq!(narrowed[2].len(), rejected[2].len() + 1);
        for repeat in [
            ReviewAction::Accept,
            ReviewAction::Reject,
            ReviewAction::Narrow,
            ReviewAction::Supersede,
        ] {
            let error = store
                .review_candidate(&candidate.id, repeat, "owner", None, Some("session:A"), 203)
                .unwrap_err();
            assert!(
                is_candidate_already_reviewed(&error),
                "Narrow then {repeat:?}: {error}"
            );
            assert_eq!(review_scope_snapshot(&store), narrowed);
        }
    }

    #[test]
    fn independent_connections_serialize_review_decisions() {
        let dir = tempfile::tempdir().unwrap();
        let store = UserModelStore::open(dir.path()).unwrap();
        let candidate = store
            .record_observation(UserModelKind::Habit, "habit", "concurrent.key", "[]", 100)
            .unwrap();
        let run_pair = |action: ReviewAction| {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
            let mut handles = Vec::new();
            for _ in 0..2 {
                let separate = UserModelStore::open(dir.path()).unwrap();
                let id = candidate.id.clone();
                let barrier = barrier.clone();
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    separate.review_candidate(&id, action, "owner", None, Some("session:A"), 200)
                }));
            }
            barrier.wait();
            let results: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
            assert_eq!(results.iter().filter(|result| matches!(result, Err(error) if is_candidate_already_reviewed(error))).count(), 1);
        };
        run_pair(ReviewAction::Accept);
        let after_accept = review_scope_snapshot(&store);
        assert_eq!(after_accept[1].len(), 1);
        assert_eq!(after_accept[2].len(), 1);

        let rejected = store
            .record_observation(
                UserModelKind::Habit,
                "other",
                "rejected.concurrent",
                "[]",
                100,
            )
            .unwrap();
        store
            .review_candidate(&rejected.id, ReviewAction::Reject, "owner", None, None, 200)
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let separate = UserModelStore::open(dir.path()).unwrap();
            let id = rejected.id.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                separate.review_candidate(
                    &id,
                    ReviewAction::Narrow,
                    "owner",
                    None,
                    Some("session:A"),
                    201,
                )
            }));
        }
        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(
                    |result| matches!(result, Err(error) if is_candidate_already_reviewed(error))
                )
                .count(),
            1
        );
        let after_narrow = review_scope_snapshot(&store);
        assert_eq!(after_narrow[1].len(), after_accept[1].len() + 2);
        assert_eq!(after_narrow[2].len(), after_accept[2].len() + 1);
    }

    #[test]
    fn unknown_stored_review_action_fails_without_writing() {
        let (_dir, store) = store();
        let candidate = store
            .record_observation(UserModelKind::Habit, "habit", "unknown.action", "[]", 100)
            .unwrap();
        store
            .conn
            .lock()
            .execute(
                "INSERT INTO user_model_review_receipts
             (id, candidate_id, action, reviewer, note, at_unix)
             VALUES ('unknown-action', ?1, 'unsupported', 'owner', NULL, 200)",
                rusqlite::params![candidate.id],
            )
            .unwrap();
        let before = review_scope_snapshot(&store);
        let error = store
            .review_candidate(
                &candidate.id,
                ReviewAction::Narrow,
                "owner",
                None,
                Some("session:A"),
                201,
            )
            .unwrap_err();
        assert!(matches!(error, rusqlite::Error::InvalidQuery));
        assert_eq!(review_scope_snapshot(&store), before);
    }

    #[test]
    fn invalid_review_scope_preserves_all_rows_before_receipt() {
        let (_dir, store) = store();
        let candidate = store
            .record_observation(
                UserModelKind::Habit,
                "private observation",
                "private.key",
                "[]",
                100,
            )
            .unwrap();
        let before = review_scope_snapshot(&store);
        for scope in ["", "task:unsupported", "unknown:scope"] {
            let error = store
                .review_candidate(
                    &candidate.id,
                    ReviewAction::Narrow,
                    "owner",
                    None,
                    Some(scope),
                    200,
                )
                .unwrap_err();
            assert!(
                matches!(error, rusqlite::Error::InvalidParameterName(ref message) if message == &format!("invalid narrowed scope '{scope}'"))
            );
            assert_eq!(review_scope_snapshot(&store), before);
        }
        assert!(matches!(
            store.review_candidate(
                "missing",
                ReviewAction::Narrow,
                "owner",
                None,
                Some("invalid"),
                200
            ),
            Err(rusqlite::Error::QueryReturnedNoRows)
        ));
        assert_eq!(review_scope_snapshot(&store), before);
        let receipt = store
            .review_candidate(
                &candidate.id,
                ReviewAction::Narrow,
                "owner",
                None,
                Some("session:A"),
                200,
            )
            .unwrap();
        assert_eq!(receipt.action, ReviewAction::Narrow);
        let after = review_scope_snapshot(&store);
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1].len(), 1);
        assert_eq!(after[2].len(), 1);
        let heads = store.active_heads(Some(200)).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].scope, "session:A");
        assert_eq!(heads[0].authority, AuthorityClass::OwnerRatified);
    }
    #[test]
    fn review_insert_failure_rolls_back_receipt_and_preserves_reject() {
        let (_dir, store) = store();
        let candidate = store
            .record_observation(UserModelKind::Habit, "private", "atomic.key", "[]", 100)
            .unwrap();
        store.conn.lock().execute_batch("CREATE TEMP TRIGGER review_insert_fault BEFORE INSERT ON main.user_model_revisions BEGIN SELECT RAISE(ABORT, 'private revision insert fault'); END;").unwrap();
        let before = review_scope_snapshot(&store);
        let error = store
            .review_candidate(
                &candidate.id,
                ReviewAction::Accept,
                "owner",
                None,
                None,
                200,
            )
            .unwrap_err();
        assert!(error.to_string().contains("private revision insert fault"));
        assert_eq!(review_scope_snapshot(&store), before);
        assert!(store.conn.lock().is_autocommit());
        let rejected = store
            .review_candidate(
                &candidate.id,
                ReviewAction::Reject,
                "owner",
                None,
                Some("ignored"),
                201,
            )
            .unwrap();
        assert_eq!(rejected.action, ReviewAction::Reject);
        let after_reject = review_scope_snapshot(&store);
        assert_eq!(after_reject[0], before[0]);
        assert_eq!(after_reject[1].len(), 1);
        assert_eq!(after_reject[2], before[2]);
        store
            .conn
            .lock()
            .execute_batch("DROP TRIGGER temp.review_insert_fault;")
            .unwrap();
        store
            .review_candidate(
                &candidate.id,
                ReviewAction::Narrow,
                "owner",
                None,
                Some("session:A"),
                202,
            )
            .unwrap();
        let after = review_scope_snapshot(&store);
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1].len(), 2);
        assert_eq!(after[2].len(), 1);
        assert_eq!(
            store.active_heads(Some(202)).unwrap()[0].authority,
            AuthorityClass::OwnerRatified
        );
    }

    #[test]
    fn review_commit_failure_rolls_back_canonical_and_auxiliary_rows() {
        let (_dir, store) = store();
        let candidate = store
            .record_observation(UserModelKind::Habit, "private", "atomic.key", "[]", 100)
            .unwrap();
        store
            .record_owner_statement(
                UserModelKind::Habit,
                "baseline",
                "baseline.key",
                "global",
                100,
            )
            .unwrap();
        {
            let conn = store.conn.lock();
            conn.execute_batch("PRAGMA foreign_keys = ON;
                CREATE TEMP TABLE review_fault_parent (id INTEGER PRIMARY KEY);
                CREATE TEMP TABLE review_fault_child (parent_id INTEGER REFERENCES review_fault_parent(id) DEFERRABLE INITIALLY DEFERRED);
                CREATE TEMP TRIGGER review_commit_fault AFTER INSERT ON main.user_model_revisions BEGIN INSERT INTO review_fault_child VALUES (1); END;").unwrap();
            assert_eq!(
                conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
        let before = review_scope_snapshot(&store);
        // Calibrate the exact trigger: revision INSERT succeeds, explicit
        // COMMIT fails. A statement-level abort is not this discriminator.
        {
            let mut conn = store.conn.lock();
            let tx = conn.transaction().unwrap();
            assert_eq!(tx.execute("INSERT INTO user_model_revisions
                (id, semantic_key, kind, statement, scope, authority, supersedes, valid_from_unix, valid_until_unix, source_candidate, created_at_unix)
                SELECT 'private-calibration', semantic_key, kind, statement, scope, authority, supersedes, valid_from_unix, valid_until_unix, source_candidate, created_at_unix FROM user_model_revisions LIMIT 1", []).unwrap(), 1);
            assert_eq!(
                tx.query_row("SELECT COUNT(*) FROM review_fault_child", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            let error = tx.commit().unwrap_err();
            assert!(
                matches!(error, rusqlite::Error::SqliteFailure(ref code, _) if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY)
            );
            assert!(conn.is_autocommit());
            assert_eq!(
                conn.query_row("SELECT COUNT(*) FROM review_fault_child", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        assert_eq!(review_scope_snapshot(&store), before);
        let error = store
            .review_candidate(
                &candidate.id,
                ReviewAction::Accept,
                "owner",
                None,
                None,
                200,
            )
            .unwrap_err();
        assert!(
            matches!(error, rusqlite::Error::SqliteFailure(ref code, _) if code.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY)
        );
        assert_eq!(review_scope_snapshot(&store), before);
        {
            let conn = store.conn.lock();
            assert!(conn.is_autocommit());
            for table in ["review_fault_child", "review_fault_parent"] {
                assert_eq!(
                    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row
                        .get::<_, i64>(0))
                        .unwrap(),
                    0
                );
            }
            conn.execute_batch("DROP TRIGGER temp.review_commit_fault; DROP TABLE temp.review_fault_child; DROP TABLE temp.review_fault_parent;").unwrap();
        }
        store
            .review_candidate(
                &candidate.id,
                ReviewAction::Accept,
                "owner",
                None,
                None,
                201,
            )
            .unwrap();
        let after = review_scope_snapshot(&store);
        assert_eq!(after[1].len(), 1);
        assert_eq!(after[2].len(), 2);
    }
    #[test]
    fn pending_candidates_derive_only_from_committed_receipts() {
        let (_dir, store) = store();
        assert!(store.list_pending_candidates().unwrap().is_empty());
        let c = store
            .record_observation(UserModelKind::Habit, "C", "c", "[]", 100)
            .unwrap();
        let d = store
            .record_observation(UserModelKind::Habit, "D", "d", "[]", 101)
            .unwrap();
        assert_eq!(
            store.list_pending_candidates().unwrap(),
            vec![d.clone(), c.clone()]
        );
        assert!(
            store
                .review_candidate(
                    &c.id,
                    ReviewAction::Narrow,
                    "owner",
                    None,
                    Some("invalid"),
                    200
                )
                .is_err()
        );
        assert_eq!(
            store.list_pending_candidates().unwrap(),
            vec![d.clone(), c.clone()]
        );
        store.conn.lock().execute_batch("CREATE TEMP TRIGGER pending_revision_fault BEFORE INSERT ON main.user_model_revisions BEGIN SELECT RAISE(ABORT, 'private pending fault'); END;").unwrap();
        assert!(
            store
                .review_candidate(&d.id, ReviewAction::Accept, "owner", None, None, 200)
                .is_err()
        );
        assert_eq!(
            store.list_pending_candidates().unwrap(),
            vec![d.clone(), c.clone()]
        );
        store
            .conn
            .lock()
            .execute_batch("DROP TRIGGER temp.pending_revision_fault;")
            .unwrap();
        store
            .review_candidate(&c.id, ReviewAction::Reject, "owner", None, None, 200)
            .unwrap();
        assert_eq!(store.list_pending_candidates().unwrap(), vec![d.clone()]);
        store
            .review_candidate(&d.id, ReviewAction::Accept, "owner", None, None, 201)
            .unwrap();
        assert!(store.list_pending_candidates().unwrap().is_empty());
        assert_eq!(store.list_candidates().unwrap(), vec![d, c]);
        let e = store
            .record_observation(UserModelKind::Habit, "E", "e", "[]", 202)
            .unwrap();
        let before = review_scope_snapshot(&store);
        assert_eq!(store.list_pending_candidates().unwrap(), vec![e]);
        assert_eq!(store.list_candidates().unwrap().len(), 3);
        assert_eq!(review_scope_snapshot(&store), before);
        assert_eq!(
            store.active_heads(Some(202)).unwrap()[0].authority,
            AuthorityClass::OwnerRatified
        );
    }
}
