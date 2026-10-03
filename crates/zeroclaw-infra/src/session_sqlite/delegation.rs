//! Persistent request admission and canonical dispatch references.
//!
//! No TTL, receipt cap or session deletion touches these claims: forgetting an
//! unresolved external outcome could permit a duplicate worker submission.

use super::SqliteSessionBackend;
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use zeroclaw_api::delegation_request::{DelegationRequestBinding, DelegationRequestClaim};

impl SqliteSessionBackend {
    /// Release the caller's pending claim only after it proves no start was
    /// transmitted. Never use this for timeout, lost receipt, or recovery.
    pub fn release_unsent_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let deleted = conn
            .execute(
                "DELETE FROM session_delegations
                 WHERE agent_alias = ?1 AND request_id = ?2 AND request_digest = ?3
                   AND dispatch_id IS NULL",
                params![agent_alias, request_id, request_digest],
            )
            .context("releasing a proven unsent delegation request")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        Ok(deleted == 1)
    }

    /// Claim a request before its first external submission.
    ///
    /// The primary key and conflict-targeted insert arbitrate independent
    /// handles/processes. An existing claim with no dispatch is unresolved,
    /// including after a crash; it never grants another submission attempt.
    pub fn claim_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
    ) -> Result<DelegationRequestClaim> {
        let conn = self.conn.lock();
        let inserted = conn
            .execute(
                "INSERT INTO session_delegations
                    (agent_alias, request_id, request_digest, dispatch_id)
                 VALUES (?1, ?2, ?3, NULL)
                 ON CONFLICT (agent_alias, request_id) DO NOTHING",
                params![agent_alias, request_id, request_digest],
            )
            .context("claiming a delegation request")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        if inserted == 1 {
            return Ok(DelegationRequestClaim::Created);
        }

        let binding = conn
            .query_row(
                "SELECT request_digest, dispatch_id FROM session_delegations
                 WHERE agent_alias = ?1 AND request_id = ?2",
                params![agent_alias, request_id],
                |row| {
                    Ok(DelegationRequestBinding {
                        request_digest: row.get(0)?,
                        dispatch_id: row.get(1)?,
                    })
                },
            )
            .context("reading an existing delegation claim")?;
        if binding.request_digest != request_digest {
            return Ok(DelegationRequestClaim::Conflict);
        }
        Ok(DelegationRequestClaim::Existing {
            dispatch_id: binding.dispatch_id,
        })
    }

    /// Bind a claimed request to its canonical dispatch without overwriting a
    /// different known dispatch. Repeating the same binding is idempotent.
    pub fn bind_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
        request_digest: &str,
        dispatch_id: &str,
    ) -> Result<()> {
        if dispatch_id.is_empty() {
            bail!("delegation dispatch ID must not be empty");
        }
        let conn = self.conn.lock();
        let updated = conn
            .execute(
                "UPDATE session_delegations SET dispatch_id = ?4
                 WHERE agent_alias = ?1 AND request_id = ?2 AND request_digest = ?3
                   AND (dispatch_id IS NULL OR dispatch_id = ?4)",
                params![agent_alias, request_id, request_digest, dispatch_id],
            )
            .context("binding a delegation request to its canonical dispatch")?;
        if let Some(path) = conn.path() {
            crate::sqlite_perms::harden_sqlite_owner_only(std::path::Path::new(path));
        }
        if updated != 1 {
            bail!("delegation binding rejected: missing claim or conflicting digest/dispatch");
        }
        Ok(())
    }

    /// Read the local reference; execution status must be resolved from Tachi.
    pub fn read_delegation_request(
        &self,
        agent_alias: &str,
        request_id: &str,
    ) -> Result<Option<DelegationRequestBinding>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT request_digest, dispatch_id FROM session_delegations
             WHERE agent_alias = ?1 AND request_id = ?2",
            params![agent_alias, request_id],
            |row| {
                Ok(DelegationRequestBinding {
                    request_digest: row.get(0)?,
                    dispatch_id: row.get(1)?,
                })
            },
        )
        .optional()
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
        store.claim_delegation_request("a", "r", "digest").unwrap();
        assert!(
            !store
                .release_unsent_delegation_request("b", "r", "digest")
                .unwrap()
        );
        assert!(
            !store
                .release_unsent_delegation_request("a", "r", "other")
                .unwrap()
        );
        assert!(
            store
                .release_unsent_delegation_request("a", "r", "digest")
                .unwrap()
        );
        assert_eq!(store.read_delegation_request("a", "r").unwrap(), None);
        assert_eq!(
            store.claim_delegation_request("a", "r", "digest").unwrap(),
            DelegationRequestClaim::Created
        );
        store
            .bind_delegation_request("a", "r", "digest", "d")
            .unwrap();
        assert!(
            !store
                .release_unsent_delegation_request("a", "r", "digest")
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
    fn delegation_claim_distinguishes_duplicate_and_conflicting_digest() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(store.read_delegation_request("a", "r1").unwrap(), None);
        assert_eq!(
            store.claim_delegation_request("a", "r1", "digest").unwrap(),
            DelegationRequestClaim::Created
        );
        assert_eq!(
            store.claim_delegation_request("a", "r1", "digest").unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
        assert_eq!(
            store.claim_delegation_request("a", "r1", "other").unwrap(),
            DelegationRequestClaim::Conflict
        );
        assert_eq!(
            store.read_delegation_request("a", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest".into(),
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
                store.claim_delegation_request("a", "r1", "digest").unwrap(),
                DelegationRequestClaim::Created
            );
        }
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r1", "digest")
                .unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
    }

    #[test]
    fn delegation_known_dispatch_survives_reopen() {
        let tmp = TempDir::new().unwrap();
        {
            let store = SqliteSessionBackend::new(tmp.path()).unwrap();
            store.claim_delegation_request("a", "r1", "digest").unwrap();
            store
                .bind_delegation_request("a", "r1", "digest", "d1")
                .unwrap();
        }
        let reopened = SqliteSessionBackend::new(tmp.path()).unwrap();
        assert_eq!(
            reopened.read_delegation_request("a", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest".into(),
                dispatch_id: Some("d1".into()),
            })
        );
        assert_eq!(
            reopened
                .claim_delegation_request("a", "r1", "digest")
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
                store.claim_delegation_request(agent, "r1", digest).unwrap(),
                DelegationRequestClaim::Created
            );
        }
        store
            .bind_delegation_request("a", "r1", "digest-a", "d1")
            .unwrap();
        assert_eq!(
            store.read_delegation_request("b", "r1").unwrap(),
            Some(DelegationRequestBinding {
                request_digest: "digest-b".into(),
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
                    store.claim_delegation_request("a", "r1", "digest").unwrap()
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
                .bind_delegation_request("a", "r1", "digest", "d1")
                .is_err()
        );
        store.claim_delegation_request("a", "r1", "digest").unwrap();
        assert!(
            store
                .bind_delegation_request("a", "r1", "other", "d1")
                .is_err()
        );
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "")
                .is_err()
        );
        store
            .bind_delegation_request("a", "r1", "digest", "d1")
            .unwrap();
        store
            .bind_delegation_request("a", "r1", "digest", "d1")
            .unwrap();
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "d2")
                .is_err()
        );
        assert!(
            store
                .bind_delegation_request("a", "r1", "other", "d1")
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
        first.claim_delegation_request("a", "r1", "digest").unwrap();
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
                            .bind_delegation_request("a", "r1", "digest", id)
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
                .claim_delegation_request("a", request, "digest")
                .unwrap();
        }
        store
            .bind_delegation_request("a", "expired", "digest", "d1")
            .unwrap();
        assert!(store.delete_session("deleted").unwrap());
        store.conn.lock().execute(
            "UPDATE session_metadata SET last_activity = '2000-01-01T00:00:00Z' WHERE session_key = 'expired'", []
        ).unwrap();
        assert_eq!(store.cleanup_stale(1).unwrap(), 1);
        assert_eq!(
            store
                .claim_delegation_request("a", "deleted", "digest")
                .unwrap(),
            DelegationRequestClaim::Existing { dispatch_id: None }
        );
        assert_eq!(
            store
                .claim_delegation_request("a", "expired", "digest")
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
        assert!(store.claim_delegation_request("a", "r1", "digest").is_err());
        assert!(
            store
                .bind_delegation_request("a", "r1", "digest", "d1")
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
                .claim_delegation_request("a", "r1", "digest")
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
                    .bind_delegation_request("a", "r1", "digest", "d1")
                    .unwrap();
            } else {
                store.claim_delegation_request("a", "r1", "digest").unwrap();
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
