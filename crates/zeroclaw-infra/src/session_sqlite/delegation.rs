//! Persistent request admission and canonical dispatch references.
//!
//! No TTL, receipt cap or session deletion touches these claims: forgetting an
//! unresolved external outcome could permit a duplicate worker submission.

use super::SqliteSessionBackend;
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior, params};
use zeroclaw_api::delegation_request::{DelegationRequestBinding, DelegationRequestClaim};

// Existing scoped rows stay untouched. Schema discovery and migration share
// the write lock so independently opened handles cannot race the ALTER.
pub(super) fn migrate_route_provenance(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let has_route: bool = tx.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('session_delegations') WHERE name = 'route_digest'",
        [],
        |row| row.get(0),
    ).context("inspecting delegation route provenance schema")?;
    if !has_route {
        tx.execute(
            "ALTER TABLE session_delegations ADD COLUMN route_digest TEXT",
            [],
        )
        .context("adding delegation route provenance")?;
    }
    tx.execute(
        "CREATE INDEX IF NOT EXISTS idx_session_delegations_request ON session_delegations(request_id)",
        [],
    ).context("indexing delegation request lookup")?;
    tx.commit()
        .context("committing delegation route provenance schema")
}

fn legacy_route<'a>(scope: &'a str, agent_alias: &str) -> Option<&'a str> {
    let suffix = scope.strip_prefix(agent_alias)?.strip_prefix(':')?;
    (suffix.len() == 64
        && suffix
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
    .then_some(suffix)
}

// Query by request ID, then compare full keys. LIKE would confuse underscores
// in real aliases with wildcard matches and could seize another agent's claim.
// A legacy key carries the route fact already persisted by the old writer; no
// row is adopted, rewritten, merged or deleted during upgrade/read/replay.
fn find_binding(
    conn: &Connection,
    agent_alias: &str,
    request_id: &str,
) -> Result<Option<DelegationRequestBinding>> {
    let mut query = conn.prepare(
        "SELECT agent_alias, request_digest, dispatch_id, route_digest
         FROM session_delegations WHERE request_id = ?1",
    )?;
    let rows = query.query_map([request_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut binding = None;
    for row in rows {
        let (scope, request_digest, dispatch_id, route_digest) = row?;
        let route_digest = if scope == agent_alias {
            route_digest
        } else if route_digest.is_none() {
            let Some(route) = legacy_route(&scope, agent_alias) else {
                continue;
            };
            Some(route.to_string())
        } else {
            continue;
        };
        if binding.is_some() {
            bail!(
                "multiple delegation claims exist for this agent/request; explicit owner reconciliation is required"
            );
        }
        binding = Some(DelegationRequestBinding {
            request_digest,
            route_digest,
            dispatch_id,
        });
    }
    Ok(binding)
}

impl SqliteSessionBackend {
    /// Release only the newly created stable claim after proving no start was
    /// sent. Legacy claims are never released by a new admission attempt.
    pub fn release_unsent_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
        route_digest: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let deleted = conn
            .execute(
                "DELETE FROM session_delegations
             WHERE agent_alias = ?1 AND request_id = ?2 AND request_digest = ?3
               AND route_digest = ?4 AND dispatch_id IS NULL",
                params![agent_alias, request_id, request_digest, route_digest],
            )
            .context("releasing a proven unsent delegation request")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(deleted == 1)
    }

    /// Claim the true agent/request key before first external submission. The
    /// immutable route fact is created here, never used as part of that key.
    /// An IMMEDIATE transaction also arbitrates preserved legacy scoped rows
    /// before insertion, across independently opened handles/processes.
    pub fn claim_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
        route_digest: &str,
    ) -> Result<DelegationRequestClaim> {
        if route_digest.is_empty() {
            bail!("delegation route provenance must not be empty");
        }
        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claim = if let Some(binding) = find_binding(&tx, agent_alias, request_id)? {
            if binding.request_digest != request_digest
                || binding.route_digest.as_deref() != Some(route_digest)
            {
                DelegationRequestClaim::Conflict
            } else {
                DelegationRequestClaim::Existing {
                    dispatch_id: binding.dispatch_id,
                }
            }
        } else {
            tx.execute(
                "INSERT INTO session_delegations
                    (agent_alias, request_id, request_digest, route_digest, dispatch_id)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                params![agent_alias, request_id, request_digest, route_digest],
            )
            .context("claiming a delegation request")?;
            DelegationRequestClaim::Created
        };
        tx.commit()
            .context("committing delegation request admission")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(claim)
    }

    /// Bind only the matching stable claim. Neither route provenance nor an
    /// existing known dispatch can be overwritten by another response.
    pub fn bind_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
        route_digest: &str,
        dispatch_id: &str,
    ) -> Result<()> {
        if dispatch_id.is_empty() {
            bail!("delegation dispatch ID must not be empty");
        }
        let conn = self.conn.lock();
        let updated = conn
            .execute(
                "UPDATE session_delegations SET dispatch_id = ?5
             WHERE agent_alias = ?1 AND request_id = ?2 AND request_digest = ?3
               AND route_digest = ?4 AND (dispatch_id IS NULL OR dispatch_id = ?5)",
                params![
                    agent_alias,
                    request_id,
                    request_digest,
                    route_digest,
                    dispatch_id
                ],
            )
            .context("binding a delegation request to its canonical dispatch")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        if updated != 1 {
            bail!(
                "delegation binding rejected: missing claim or conflicting payload/route/dispatch"
            );
        }
        Ok(())
    }

    /// Resolve a stable or uniquely preserved legacy reference. The caller
    /// must compare immutable route provenance before using the dispatch ID.
    pub fn read_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
    ) -> Result<Option<DelegationRequestBinding>> {
        find_binding(&self.conn.lock(), agent_alias, request_id)
            .context("reading a delegation request binding")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_backend::SessionBackend;
    use std::sync::{Arc, Barrier};
    use tempfile::TempDir;
    use zeroclaw_api::model_provider::ChatMessage;

    #[test]
    fn unsent_release_matches_scope_digest_and_never_removes_known_dispatch() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        store
            .claim_delegation_request("a", "r", "digest", "route")
            .unwrap();
        assert!(
            !store
                .release_unsent_delegation_request("b", "r", "digest", "route")
                .unwrap()
        );
        assert!(
            !store
                .release_unsent_delegation_request("a", "r", "other", "route")
                .unwrap()
        );
        assert!(
            store
                .release_unsent_delegation_request("a", "r", "digest", "route")
                .unwrap()
        );
        assert_eq!(store.read_delegation_request("a", "r").unwrap(), None);
        assert_eq!(
            store
                .claim_delegation_request("a", "r", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Created
        );
        store
            .bind_delegation_request("a", "r", "digest", "route", "d")
            .unwrap();
        assert!(
            !store
                .release_unsent_delegation_request("a", "r", "digest", "route")
                .unwrap()
        );
        assert_eq!(
            store
                .read_delegation_request("a", "r")
                .unwrap()
                .unwrap()
                .dispatch_id
                .as_deref(),
            Some("d")
        );
    }

    #[test]
    fn delegation_route_is_immutable_across_claim_bind_release_and_reopen() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            store
                .claim_delegation_request("a", "r", "digest", "route-a")
                .unwrap(),
            DelegationRequestClaim::Created
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "r", "digest", "route-b")
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
        assert!(
            !store
                .release_unsent_delegation_request("a", "r", "digest", "route-b")
                .unwrap()
        );
        assert!(
            store
                .bind_delegation_request("a", "r", "digest", "route-b", "wrong")
                .is_err()
        );
        store
            .bind_delegation_request("a", "r", "digest", "route-a", "right")
            .unwrap();
        drop(store);
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        let binding = reopened.read_delegation_request("a", "r").unwrap().unwrap();
        assert_eq!(binding.route_digest.as_deref(), Some("route-a"));
        assert_eq!(binding.dispatch_id.as_deref(), Some("right"));
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r", "digest", "route-b")
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
    }

    #[test]
    fn legacy_route_parser_requires_the_complete_alias_and_exact_lowercase_hash() {
        let hash = "a".repeat(64);
        assert_eq!(
            legacy_route(&format!("a_b:{hash}"), "a_b"),
            Some(hash.as_str())
        );
        for scope in [
            format!("axb:{hash}"),
            format!("a_b_extra:{hash}"),
            format!("a_b:{}", "A".repeat(64)),
            format!("a_b:{hash}0"),
            format!("a_b:{hash}:other"),
        ] {
            assert_eq!(legacy_route(&scope, "a_b"), None, "{scope}");
        }
    }

    #[test]
    fn legacy_scoped_claims_survive_upgrade_without_adoption_or_wildcard_collision() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let route = "a".repeat(64);
        let other_route = "b".repeat(64);
        {
            let conn = store.conn.lock();
            conn.execute_batch(
                "DROP TABLE session_delegations;
                CREATE TABLE session_delegations (
                    agent_alias TEXT NOT NULL, request_id TEXT NOT NULL,
                    request_digest TEXT NOT NULL, dispatch_id TEXT,
                    PRIMARY KEY(agent_alias, request_id));",
            )
            .unwrap();
            for (alias, request, digest, dispatch) in [
                (format!("a_b:{route}"), "pending", "ours", None),
                (
                    format!("axb:{route}"),
                    "pending",
                    "foreign",
                    Some("foreign-dispatch"),
                ),
                (
                    format!("a_b:{route}"),
                    "bound",
                    "ours",
                    Some("old-dispatch"),
                ),
                (format!("a_b:{route}"), "multiple", "ours", None),
                (
                    format!("a_b:{other_route}"),
                    "multiple",
                    "other",
                    Some("other-dispatch"),
                ),
                ("unscoped".to_string(), "pending", "unknown-route", None),
            ] {
                conn.execute(
                    "INSERT INTO session_delegations VALUES(?1,?2,?3,?4)",
                    params![alias, request, digest, dispatch],
                )
                .unwrap();
            }
        }
        drop(store);
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        for (request, dispatch) in [("pending", None), ("bound", Some("old-dispatch"))] {
            assert_eq!(
                reopened
                    .claim_delegation_request("a_b", request, "ours", &route)
                    .unwrap(),
                DelegationRequestClaim::Existing {
                    dispatch_id: dispatch.map(str::to_string)
                }
            );
            assert_eq!(
                reopened
                    .claim_delegation_request("a_b", request, "ours", &other_route)
                    .unwrap(),
                DelegationRequestClaim::Conflict
            );
            let binding = reopened
                .read_delegation_request("a_b", request)
                .unwrap()
                .unwrap();
            assert_eq!(binding.route_digest.as_deref(), Some(route.as_str()));
            assert_eq!(binding.dispatch_id.as_deref(), dispatch);
            assert!(
                !reopened
                    .release_unsent_delegation_request("a_b", request, "ours", &route)
                    .unwrap()
            );
        }
        assert_eq!(
            reopened
                .read_delegation_request("axb", "pending")
                .unwrap()
                .unwrap()
                .dispatch_id
                .as_deref(),
            Some("foreign-dispatch")
        );
        assert!(
            reopened
                .claim_delegation_request("a_b", "multiple", "ours", &route)
                .is_err()
        );
        assert!(reopened.read_delegation_request("a_b", "multiple").is_err());
        assert_eq!(
            reopened
                .claim_delegation_request("unscoped", "pending", "unknown-route", &route)
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
        let conn = reopened.conn.lock();
        let rows: i64 = conn
            .query_row("SELECT count(*) FROM session_delegations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            rows, 6,
            "all foreign/ambiguous/known legacy claims remain untouched"
        );
        let new_rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM session_delegations WHERE route_digest IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            new_rows, 0,
            "upgrade/replay must not adopt or copy a legacy row"
        );
    }

    #[test]
    fn concurrent_route_changes_cannot_create_two_stable_claims() {
        let tmp = TempDir::new().unwrap();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [(first, "route-a"), (second, "route-b")]
            .into_iter()
            .map(|(store, route)| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .claim_delegation_request("a", "r", "digest", route)
                        .unwrap()
                })
            })
            .collect();
        let claims: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == DelegationRequestClaim::Created)
                .count(),
            1
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == DelegationRequestClaim::Conflict)
                .count(),
            1
        );
    }

    #[test]
    fn delegation_claim_distinguishes_duplicate_and_conflicting_digest() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(store.read_delegation_request("a", "r1").unwrap(), None);
        assert_eq!(
            store
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Created
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "r1", "other", "route")
                .unwrap(),
            DelegationRequestClaim::Conflict
        );
        assert_eq!(
            store.read_delegation_request("a", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest".into(),
                route_digest: Some("route".into()),
                dispatch_id: None,
            })
        );
    }

    #[test]
    fn delegation_unresolved_claim_survives_reopen_without_resubmission() {
        let tmp = TempDir::new().unwrap();
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            assert_eq!(
                store
                    .claim_delegation_request("a", "r1", "digest", "route")
                    .unwrap(),
                DelegationRequestClaim::Created
            );
        }
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
    }

    #[test]
    fn delegation_known_dispatch_survives_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            store
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap();
            store
                .bind_delegation_request("a", "r1", "digest", "route", "d1")
                .unwrap();
        }
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened.read_delegation_request("a", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest".into(),
                route_digest: Some("route".into()),
                dispatch_id: Some("d1".into()),
            })
        );
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing {
                dispatch_id: Some("d1".into())
            }
        );
    }

    #[test]
    fn delegation_request_ids_are_agent_scoped() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        for (agent, digest) in [("a", "digest-a"), ("b", "digest-b")] {
            assert_eq!(
                store
                    .claim_delegation_request(agent, "r1", digest, "route")
                    .unwrap(),
                DelegationRequestClaim::Created
            );
        }
        store
            .bind_delegation_request("a", "r1", "digest-a", "route", "d1")
            .unwrap();
        assert_eq!(
            store.read_delegation_request("b", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest-b".into(),
                route_digest: Some("route".into()),
                dispatch_id: None,
            })
        );
    }

    #[test]
    fn delegation_concurrent_handles_create_only_one_claim() {
        let tmp = TempDir::new().unwrap();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [first, second]
            .into_iter()
            .map(|store| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store
                        .claim_delegation_request("a", "r1", "digest", "route")
                        .unwrap()
                })
            })
            .collect();
        let claims: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == DelegationRequestClaim::Created)
                .count(),
            1
        );
        assert_eq!(
            claims
                .iter()
                .filter(|claim| **claim == DelegationRequestClaim::Existing { dispatch_id: None })
                .count(),
            1
        );
    }

    #[test]
    fn delegation_binding_is_digest_bound_and_rejects_conflicting_dispatch() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "route", "d1")
                .is_err()
        );
        store
            .claim_delegation_request("a", "r1", "digest", "route")
            .unwrap();
        assert!(
            store
                .bind_delegation_request("a", "r1", "other", "route", "d1")
                .is_err()
        );
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "route", "")
                .is_err()
        );
        store
            .bind_delegation_request("a", "r1", "digest", "route", "d1")
            .unwrap();
        store
            .bind_delegation_request("a", "r1", "digest", "route", "d1")
            .unwrap();
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "route", "d2")
                .is_err()
        );
        assert!(
            store
                .bind_delegation_request("a", "r1", "other", "route", "d1")
                .is_err()
        );
        assert_eq!(
            store
                .read_delegation_request("a", "r1")
                .unwrap()
                .unwrap()
                .dispatch_id,
            Some("d1".into())
        );
    }

    #[test]
    fn delegation_concurrent_bind_cannot_overwrite_a_known_dispatch() {
        let tmp = TempDir::new().unwrap();
        let first = SqliteSessionBackend::new(tmp.path()).unwrap();
        let second = SqliteSessionBackend::new(tmp.path()).unwrap();
        first
            .claim_delegation_request("a", "r1", "digest", "route")
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = [(first, "d1"), (second, "d2")]
            .into_iter()
            .map(|(store, id)| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    (
                        id,
                        store
                            .bind_delegation_request("a", "r1", "digest", "route", id)
                            .is_ok(),
                    )
                })
            })
            .collect();
        let outcomes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(outcomes.iter().filter(|(_, accepted)| *accepted).count(), 1);
        let winner = outcomes.iter().find(|(_, accepted)| *accepted).unwrap().0;
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened
                .read_delegation_request("a", "r1")
                .unwrap()
                .unwrap()
                .dispatch_id
                .as_deref(),
            Some(winner)
        );
    }

    #[test]
    fn delegation_claims_survive_session_deletion_and_ttl_cleanup() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        for request in ["deleted", "expired"] {
            store
                .append(request, &ChatMessage::user("message"))
                .unwrap();
            store
                .claim_delegation_request("a", request, "digest", "route")
                .unwrap();
        }
        store
            .bind_delegation_request("a", "expired", "digest", "route", "d1")
            .unwrap();
        assert!(store.delete_session("deleted").unwrap());
        store.conn.lock().execute(
            "UPDATE session_metadata SET last_activity = '2000-01-01T00:00:00Z' WHERE session_key = 'expired'", []
        ).unwrap();
        assert_eq!(store.cleanup_stale(1).unwrap(), 1);
        assert_eq!(
            store
                .claim_delegation_request("a", "deleted", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "expired", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Existing {
                dispatch_id: Some("d1".into())
            }
        );
    }

    #[test]
    fn delegation_sqlite_errors_are_not_missing_or_created_results() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        store
            .conn
            .lock()
            .execute("DROP TABLE session_delegations", [])
            .unwrap();
        assert!(store.read_delegation_request("a", "r1").is_err());
        assert!(
            store
                .claim_delegation_request("a", "r1", "digest", "route")
                .is_err()
        );
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "route", "d1")
                .is_err()
        );
    }

    #[test]
    fn delegation_schema_is_added_to_an_existing_sessions_database() {
        let tmp = TempDir::new().unwrap();
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            store
                .append("existing", &ChatMessage::user("retained message"))
                .unwrap();
            store
                .conn
                .lock()
                .execute("DROP TABLE session_delegations", [])
                .unwrap();
        }
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(reopened.load("existing")[0].content, "retained message");
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r1", "digest", "route")
                .unwrap(),
            DelegationRequestClaim::Created
        );
    }

    #[cfg(unix)]
    #[test]
    fn delegation_open_and_writes_harden_main_database_and_sidecars() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        let paths: Vec<_> = ["", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| {
                let mut path = tmp
                    .path()
                    .join("sessions/sessions.db")
                    .as_os_str()
                    .to_os_string();
                path.push(suffix);
                std::path::PathBuf::from(path)
            })
            .filter(|path| path.exists())
            .collect();
        assert!(
            paths.len() > 1,
            "keep a live WAL sidecar for this regression"
        );
        for path in &paths {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        for bind in [false, true] {
            for path in &paths {
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).unwrap();
            }
            if bind {
                store
                    .bind_delegation_request("a", "r1", "digest", "route", "d1")
                    .unwrap();
            } else {
                store
                    .claim_delegation_request("a", "r1", "digest", "route")
                    .unwrap();
            }
            for path in &paths {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600,
                    "{} must be owner-only after the write",
                    path.display()
                );
            }
        }
    }
}
