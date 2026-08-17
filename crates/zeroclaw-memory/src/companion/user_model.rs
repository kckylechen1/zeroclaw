//! Local-first User Model authority (#51 slice 1): owner values, goals,
//! preferences as governed, append-only records — not an inferred profile
//! and not a generic memcore category.
//!
//! Authority rules (frozen in #51):
//! - An explicit owner-authored statement may become an active revision
//!   immediately.
//! - An observation is ALWAYS a candidate; no amount of repetition
//!   promotes it. Only an explicit review action (`accept`/`narrow`) can.
//! - `reject` records the decision without deleting evidence.
//! - `supersede` appends a new revision; history is never rewritten.
//! - Works fully offline; nothing here requires Tachi.

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::Connection;
use zeroclaw_infra::sqlite_perms::harden_sqlite_owner_only;

/// What kind of statement this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserModelKind {
    Value,
    Goal,
    Preference,
    Habit,
    Constraint,
}

impl UserModelKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Value => "value",
            Self::Goal => "goal",
            Self::Preference => "preference",
            Self::Habit => "habit",
            Self::Constraint => "constraint",
        }
    }
}

/// How a revision earned authority. Frequency and confidence never appear
/// here — evidence quality is not authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorityClass {
    OwnerAuthored,
    OwnerRatified,
}

impl AuthorityClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::OwnerAuthored => "owner_authored",
            Self::OwnerRatified => "owner_ratified",
        }
    }
}

/// The review actions an owner can take on a candidate. Each produces a
/// distinct append-only history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    Accept,
    Reject,
    Narrow,
    Supersede,
}

impl ReviewAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Reject => "reject",
            Self::Narrow => "narrow",
            Self::Supersede => "supersede",
        }
    }
}

/// An observation awaiting review. Never active on its own, regardless of
/// how many times it (or its siblings) was observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserModelCandidate {
    pub id: String,
    pub kind: UserModelKind,
    pub statement: String,
    pub semantic_key: String,
    pub scope: String,
    pub evidence: String,
    pub created_at_unix: u64,
}

/// An append-only revision. The active head for a semantic key is derived
/// at read time (latest applicable revision), so supersession never edits
/// history.
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// Receipt for an explicit review decision. Even a rejection keeps the
/// candidate and its evidence intact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserModelReviewReceipt {
    pub id: String,
    pub candidate_id: String,
    pub action: ReviewAction,
    pub reviewer: String,
    pub note: Option<String>,
    pub at_unix: u64,
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

    /// Apply an explicit review action to a candidate. Every action writes
    /// a receipt; `accept`/`narrow`/`supersede` additionally append a
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
        let conn = self.conn.lock();
        let candidate = conn.query_row(
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

        let receipt = UserModelReviewReceipt {
            id: uuid::Uuid::new_v4().to_string(),
            candidate_id: candidate_id.to_string(),
            action,
            reviewer: reviewer.to_string(),
            note: note.map(str::to_string),
            at_unix: now_unix,
        };
        conn.execute(
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
                let scope = match action {
                    ReviewAction::Narrow => narrowed_scope.unwrap_or("global"),
                    _ => "global",
                };
                conn.execute(
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
        Ok(receipt)
    }

    /// Active, applicable revisions as of `as_of_unix` (`None` = now).
    /// Per semantic key the latest revision valid at that instant wins;
    /// superseded and expired revisions are simply older history.
    pub fn active_heads(
        &self,
        as_of_unix: Option<u64>,
    ) -> Result<Vec<UserModelRevision>, rusqlite::Error> {
        // "Now" must fit sqlite's i64; u64::MAX would overflow the binding.
        let as_of = as_of_unix.unwrap_or(i64::MAX as u64);
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
                Some("session.trading"),
                3_000,
            )
            .unwrap();
        assert_eq!(narrowed.action, ReviewAction::Narrow);
        let heads = s.active_heads(Some(3_000)).unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads[0].scope, "session.trading");

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
            "task:one-off",
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
                "task:one-off",
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
}
