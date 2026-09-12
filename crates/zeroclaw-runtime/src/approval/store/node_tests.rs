use super::*;
use serde_json::json;

const LEGACY_SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA busy_timeout = 5000;
CREATE TABLE IF NOT EXISTS approval_grants (
    approval_id TEXT PRIMARY KEY,
    boot_id     TEXT NOT NULL,
    run_id      TEXT NOT NULL,
    tool_name   TEXT NOT NULL,
    args_hash   TEXT NOT NULL,
    granted_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    consumed_at TEXT,
    approver    TEXT NOT NULL,
    channel     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_grants_lookup
    ON approval_grants(boot_id, run_id, tool_name, args_hash);
CREATE TABLE IF NOT EXISTS approval_audit (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           TEXT NOT NULL,
    boot_id      TEXT NOT NULL,
    run_id       TEXT,
    agent        TEXT,
    tool_name    TEXT NOT NULL,
    args_hash    TEXT NOT NULL,
    args_summary TEXT NOT NULL,
    decision     TEXT NOT NULL,
    approver     TEXT,
    channel      TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON approval_audit(ts);
CREATE INDEX IF NOT EXISTS idx_audit_run ON approval_audit(run_id);
";

use zeroclaw_api::device_identity::{DeviceIdentityV1, DeviceKeyAlgorithm, DeviceRole};

fn identity() -> DeviceIdentityV1 {
    DeviceIdentityV1 {
        device_id: "private-device".into(),
        public_key: String::new(),
        key_fingerprint: String::new(),
        algorithm: DeviceKeyAlgorithm::Ed25519,
        role: DeviceRole::Node,
        identity_epoch: 1,
        admitted_at: Utc::now().to_rfc3339(),
        revoked_at: None,
        capability_ceiling: vec!["test.cap".into()],
    }
}
fn projection(id: &str) -> NodeGrantProjection {
    NodeGrantProjection {
        grant_id: id.into(),
        kind: NodeGrantKind::TachiProjected,
        device_id: identity().device_id,
        identity_epoch: 1,
        capability: "test.cap".into(),
        args_hash: args_hash(&json!({"b": 2, "a": 1})),
        nonce: "private-nonce".into(),
        granted_at: Utc::now() - Duration::seconds(10),
        expires_at: None,
        revoked_at: None,
    }
}
fn request<'a>(
    id: &'a str,
    device: &'a DeviceIdentityV1,
    args: &'a serde_json::Value,
) -> NodeGrantClaim<'a> {
    NodeGrantClaim {
        grant_id: id,
        kind: NodeGrantKind::TachiProjected,
        identity: device,
        capability: "test.cap",
        args,
        nonce: "private-nonce",
        connection_id: "private-connection",
        cap_revision: 0,
        call_id: "private-call",
    }
}
fn rows(conn: &Connection, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let count = stmt.column_count();
    stmt.query_map([], |row| (0..count).map(|i| row.get(i)).collect())
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}
fn claim_tuple(
    conn: &Connection,
    id: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    conn.query_row("SELECT consumed_at, claim_connection_id, claim_cap_revision, claim_call_id FROM approval_grants WHERE approval_id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap()
}

#[test]
fn node_migration_preserves_legacy_rows_audit_indexes_and_local_redeem() {
    let dir = tempfile::tempdir().unwrap();
    let conn = Connection::open(dir.path().join("approvals.db")).unwrap();
    conn.execute_batch(LEGACY_SCHEMA).unwrap();
    let now = Utc::now();
    for (id, consumed) in [("first", None), ("second", Some(now.to_rfc3339()))] {
        conn.execute("INSERT INTO approval_grants VALUES (?1,'old-boot','run','tool',?2,?3,?4,?5,'owner','test')",
            params![id, args_hash(&json!({})), now.to_rfc3339(), (now + Duration::minutes(5)).to_rfc3339(), consumed]).unwrap();
    }
    conn.execute_batch("INSERT INTO approval_audit VALUES (7,'time','old-boot','run',NULL,'tool','hash','summary','granted',NULL,NULL);
        INSERT INTO approval_audit VALUES (9,'time2','old-boot','run',NULL,'tool','hash','summary','denied',NULL,NULL);").unwrap();
    let legacy_select = "SELECT approval_id,boot_id,run_id,tool_name,args_hash,granted_at,expires_at,consumed_at,approver,channel FROM approval_grants ORDER BY approval_id";
    let before = rows(&conn, legacy_select);
    let audit = rows(&conn, "SELECT * FROM approval_audit ORDER BY seq");
    let sequence = rows(&conn, "SELECT * FROM sqlite_sequence");
    let indexes = rows(
        &conn,
        "SELECT name,sql FROM sqlite_master WHERE type='index' ORDER BY name",
    );
    drop(conn);
    let store = ApprovalStore::open(dir.path(), "old-boot").unwrap();
    {
        let conn = store.lock();
        assert_eq!(rows(&conn, legacy_select), before);
        assert_eq!(
            rows(&conn, "SELECT * FROM approval_audit ORDER BY seq"),
            audit
        );
        assert_eq!(rows(&conn, "SELECT * FROM sqlite_sequence"), sequence);
        assert_eq!(
            rows(
                &conn,
                "SELECT name,sql FROM sqlite_master WHERE type='index' ORDER BY name"
            ),
            indexes
        );
        let count: i64 = conn.query_row("SELECT count(*) FROM approval_grants WHERE grant_kind='local_tool' AND device_id IS NULL AND identity_epoch IS NULL AND capability IS NULL AND nonce IS NULL AND revoked_at IS NULL AND claim_connection_id IS NULL AND claim_cap_revision IS NULL AND claim_call_id IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "wal"
        );
        assert_eq!(
            conn.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    assert_eq!(
        store.redeem("run", "tool", &json!({})).unwrap(),
        Ok("first".into())
    );
    drop(store);
    let restarted = ApprovalStore::open(dir.path(), "new-boot").unwrap();
    assert_eq!(
        restarted.redeem("run", "tool", &json!({})).unwrap(),
        Err(RedeemFailure::NoGrant)
    );
    assert_eq!(restarted.audit_for_run("run").unwrap().len(), 2);
}

#[test]
fn node_migration_refuses_unknown_or_conflicting_shape_without_reset() {
    for unknown in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("approvals.db")).unwrap();
        conn.execute_batch(
            &LEGACY_SCHEMA
                .replace("PRAGMA journal_mode = WAL;", "")
                .replace("PRAGMA synchronous = NORMAL;", ""),
        )
        .unwrap();
        conn.execute_batch("INSERT INTO approval_grants VALUES ('retained','boot','run','tool','hash','start','end',NULL,'owner','test');
            INSERT INTO approval_audit VALUES (11,'time','boot','run',NULL,'tool','hash','summary','denied',NULL,NULL);").unwrap();
        if unknown {
            conn.pragma_update(None, "user_version", 99).unwrap();
        } else {
            conn.execute_batch("CREATE TABLE approval_grants_v1 (foreign_value TEXT); INSERT INTO approval_grants_v1 VALUES ('retained');").unwrap();
        }
        let schema = rows(
            &conn,
            "SELECT type,name,sql FROM sqlite_master ORDER BY name",
        );
        let grants = rows(&conn, "SELECT * FROM approval_grants");
        let audit = rows(&conn, "SELECT * FROM approval_audit");
        let journal = rows(&conn, "PRAGMA journal_mode");
        assert!(ApprovalStore::open(dir.path(), "boot").is_err());
        assert_eq!(
            rows(
                &conn,
                "SELECT type,name,sql FROM sqlite_master ORDER BY name"
            ),
            schema
        );
        assert_eq!(rows(&conn, "SELECT * FROM approval_grants"), grants);
        assert_eq!(rows(&conn, "SELECT * FROM approval_audit"), audit);
        assert_eq!(rows(&conn, "PRAGMA journal_mode"), journal);
        if unknown {
            conn.pragma_update(None, "user_version", 0).unwrap();
        } else {
            conn.execute_batch("DROP TABLE approval_grants_v1").unwrap();
        }
        drop(conn);
        assert!(ApprovalStore::open(dir.path(), "boot").is_ok());
    }
}

#[test]
fn node_restart_single_claim_full_revision_and_identity_ranges() {
    for revision in [0, u64::MAX] {
        let dir = tempfile::tempdir().unwrap();
        let store = ApprovalStore::open(dir.path(), "boot-a").unwrap();
        let mut grant = projection("stable-upstream-id");
        grant.identity_epoch = if revision == 0 { 0 } else { i64::MAX as u64 };
        grant.kind = if revision == 0 {
            NodeGrantKind::NodeCapability
        } else {
            NodeGrantKind::TachiProjected
        };
        store.insert_trusted_node_projection(&grant).unwrap();
        assert!(store.insert_trusted_node_projection(&grant).is_err());
        store
            .grant(
                "run",
                "tool",
                &json!({}),
                "owner",
                "test",
                Duration::minutes(5),
            )
            .unwrap();
        drop(store);
        let store = ApprovalStore::open(dir.path(), "boot-b").unwrap();
        let mut device = identity();
        device.identity_epoch = grant.identity_epoch;
        let args = json!({"a":1,"b":2});
        let mut claim = request(&grant.grant_id, &device, &args);
        claim.cap_revision = revision;
        claim.kind = grant.kind;
        let evidence = store.claim_node_grant_by_id(&claim).unwrap().unwrap();
        assert_eq!(evidence.cap_revision, revision);
        assert_eq!(evidence.grant_id, grant.grant_id);
        assert_eq!(evidence.identity_epoch, grant.identity_epoch);
        assert_eq!(
            evidence.expires_at - evidence.granted_at,
            Duration::seconds(300)
        );
        assert_eq!(
            claim_tuple(&store.lock(), &grant.grant_id).2,
            Some(revision.to_string())
        );
        assert_eq!(
            store.claim_node_grant_by_id(&claim).unwrap(),
            Err(NodeClaimFailure::AlreadyClaimed)
        );
        assert_eq!(
            store.redeem("run", "tool", &json!({})).unwrap(),
            Err(RedeemFailure::NoGrant)
        );
        drop(store);
        let store = ApprovalStore::open(dir.path(), "boot-c").unwrap();
        assert_eq!(
            store.claim_node_grant_by_id(&claim).unwrap(),
            Err(NodeClaimFailure::AlreadyClaimed)
        );
    }
}

#[test]
fn node_immutable_mismatches_do_not_consume_the_valid_grant() {
    for mutation in 0..12 {
        let dir = tempfile::tempdir().unwrap();
        let store = ApprovalStore::open(dir.path(), "boot").unwrap();
        store
            .insert_trusted_node_projection(&projection("grant"))
            .unwrap();
        let mut device = identity();
        match mutation {
            0 => device.device_id = "wrong".into(),
            1 => device.identity_epoch = 2,
            2 => device.identity_epoch = u64::MAX,
            3 => device.revoked_at = Some(Utc::now().to_rfc3339()),
            4 => device.role = DeviceRole::Client,
            _ => {}
        }
        let args = if mutation == 5 {
            json!({"a":9})
        } else {
            json!({"a":1,"b":2})
        };
        let mut claim = request("grant", &device, &args);
        match mutation {
            6 => claim.kind = NodeGrantKind::NodeCapability,
            7 => claim.capability = "wrong",
            8 => claim.nonce = "wrong",
            9 => claim.connection_id = "",
            10 => claim.call_id = "",
            11 => claim.grant_id = "",
            _ => {}
        }
        assert_eq!(
            store.claim_node_grant_by_id(&claim).unwrap(),
            Err(NodeClaimFailure::NotClaimable)
        );
        assert_eq!(
            claim_tuple(&store.lock(), "grant"),
            (None, None, None, None)
        );
        let device = identity();
        let args = json!({"b":2,"a":1});
        assert!(
            store
                .claim_node_grant_by_id(&request("grant", &device, &args))
                .unwrap()
                .is_ok()
        );
    }
}

#[test]
fn node_expiry_revocation_and_malformed_timestamps_are_not_claimable() {
    for mutation in 0..4 {
        let dir = tempfile::tempdir().unwrap();
        let store = ApprovalStore::open(dir.path(), "boot").unwrap();
        let mut grant = projection("grant");
        if mutation == 0 {
            grant.granted_at = Utc::now() - Duration::hours(2);
        }
        if mutation == 1 {
            grant.revoked_at = Some(Utc::now());
        }
        store.insert_trusted_node_projection(&grant).unwrap();
        if mutation == 2 {
            store
                .lock()
                .execute("UPDATE approval_grants SET expires_at='malformed'", [])
                .unwrap();
        }
        if mutation == 3 {
            store
                .lock()
                .execute("UPDATE approval_grants SET granted_at='malformed'", [])
                .unwrap();
        }
        let device = identity();
        let args = json!({"a":1,"b":2});
        assert_eq!(
            store
                .claim_node_grant_by_id(&request("grant", &device, &args))
                .unwrap(),
            Err(NodeClaimFailure::NotClaimable)
        );
        assert_eq!(
            claim_tuple(&store.lock(), "grant"),
            (None, None, None, None)
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let store = ApprovalStore::open(dir.path(), "boot").unwrap();
    for mutation in 0..4 {
        let mut grant = projection("invalid");
        match mutation {
            0 => grant.identity_epoch = u64::MAX,
            1 => grant.expires_at = Some(grant.granted_at + Duration::minutes(16)),
            2 => grant.args_hash = "not-a-hash".into(),
            _ => grant.nonce.clear(),
        }
        assert!(store.insert_trusted_node_projection(&grant).is_err());
    }
}

#[test]
fn node_two_connection_claim_and_revocation_races_are_atomic() {
    for revoke in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let first = ApprovalStore::open(dir.path(), "a").unwrap();
        first
            .insert_trusted_node_projection(&projection("grant"))
            .unwrap();
        let second = ApprovalStore::open(dir.path(), "b").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker = std::thread::spawn(move || {
            let device = identity();
            let args = json!({"a":1,"b":2});
            let claim = request("grant", &device, &args);
            worker_barrier.wait();
            first.claim_node_grant_by_id(&claim).unwrap()
        });
        let device = identity();
        let args = json!({"a":1,"b":2});
        let mut claim = request("grant", &device, &args);
        claim.connection_id = "competing-connection";
        claim.call_id = "competing-call";
        claim.cap_revision = u64::MAX;
        barrier.wait();
        if revoke {
            assert!(second.record_node_revocation("grant", Utc::now()).unwrap());
            let result = worker.join().unwrap();
            let tuple = claim_tuple(&second.lock(), "grant");
            match result {
                Ok(winner) => assert_eq!(
                    tuple,
                    (
                        Some(winner.claimed_at.to_rfc3339()),
                        Some(winner.connection_id),
                        Some(winner.cap_revision.to_string()),
                        Some(winner.call_id)
                    )
                ),
                Err(reason) => {
                    assert_eq!(reason, NodeClaimFailure::NotClaimable);
                    assert_eq!(tuple, (None, None, None, None));
                }
            }
            assert_eq!(
                second.claim_node_grant_by_id(&claim).unwrap(),
                Err(NodeClaimFailure::NotClaimable)
            );
            assert!(
                !second
                    .record_node_revocation("grant", Utc::now() - Duration::hours(1))
                    .unwrap()
            );
        } else {
            let result = second.claim_node_grant_by_id(&claim).unwrap();
            let other = worker.join().unwrap();
            let winner = match (result, other) {
                (Ok(winner), Err(NodeClaimFailure::AlreadyClaimed))
                | (Err(NodeClaimFailure::AlreadyClaimed), Ok(winner)) => winner,
                pair => panic!("invalid claim race: {pair:?}"),
            };
            assert_eq!(
                claim_tuple(&second.lock(), "grant"),
                (
                    Some(winner.claimed_at.to_rfc3339()),
                    Some(winner.connection_id),
                    Some(winner.cap_revision.to_string()),
                    Some(winner.call_id)
                )
            );
        }
    }
}

#[test]
fn node_update_and_deferred_commit_failure_roll_back_complete_claim() {
    for deferred in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = ApprovalStore::open(dir.path(), "boot").unwrap();
        store
            .insert_trusted_node_projection(&projection("grant"))
            .unwrap();
        if deferred {
            store.lock().execute_batch("PRAGMA foreign_keys=ON;
                CREATE TABLE fixture_parent(id TEXT PRIMARY KEY);
                CREATE TABLE fixture_child(parent TEXT REFERENCES fixture_parent(id) DEFERRABLE INITIALLY DEFERRED);
                CREATE TRIGGER fixture_failure AFTER UPDATE OF consumed_at ON approval_grants WHEN NEW.approval_id='grant'
                BEGIN INSERT INTO fixture_child VALUES ('missing'); END;").unwrap();
        } else {
            store.lock().execute_batch("CREATE TRIGGER fixture_failure BEFORE UPDATE OF consumed_at ON approval_grants WHEN NEW.approval_id='grant'
                BEGIN SELECT RAISE(ABORT,'private update fault'); END;").unwrap();
        }
        if deferred {
            // Establish that this private fault permits UPDATE and rejects COMMIT.
            let mut conn = store.lock();
            let tx = conn.transaction().unwrap();
            assert_eq!(tx.execute("UPDATE approval_grants SET consumed_at='2026-01-01T00:00:00Z', claim_connection_id='fixture', claim_cap_revision='0', claim_call_id='fixture' WHERE approval_id='grant'", []).unwrap(), 1);
            assert!(tx.commit().is_err());
            assert!(conn.is_autocommit());
            assert_eq!(claim_tuple(&conn, "grant"), (None, None, None, None));
        }
        let device = identity();
        let args = json!({"a":1,"b":2});
        let result = store.claim_node_grant_by_id(&request("grant", &device, &args));
        assert!(
            result.is_err(),
            "write/commit failure must not return claim evidence"
        );
        let conn = Connection::open(dir.path().join("approvals.db")).unwrap();
        assert_eq!(claim_tuple(&conn, "grant"), (None, None, None, None));
        if deferred {
            assert_eq!(
                conn.query_row("SELECT count(*) FROM fixture_child", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        eprintln!(
            "NODE_CLAIM_FAULT deferred_commit={deferred} no_claim_evidence=true complete_tuple_rolled_back=true"
        );
        conn.execute_batch("DROP TRIGGER fixture_failure").unwrap();
        if deferred {
            conn.execute_batch("DROP TABLE fixture_child; DROP TABLE fixture_parent;")
                .unwrap();
        }
        drop(conn);
        assert!(
            store
                .claim_node_grant_by_id(&request("grant", &device, &args))
                .unwrap()
                .is_ok()
        );
    }
}

#[test]
fn node_claim_revision_schema_rejects_noncanonical_and_partial_tuples() {
    let dir = tempfile::tempdir().unwrap();
    let store = ApprovalStore::open(dir.path(), "boot").unwrap();
    store
        .insert_trusted_node_projection(&projection("grant"))
        .unwrap();
    for column in [
        "approval_id",
        "grant_kind",
        "device_id",
        "identity_epoch",
        "capability",
        "nonce",
    ] {
        assert!(
            store
                .lock()
                .execute(
                    &format!("UPDATE approval_grants SET {column}=NULL WHERE approval_id='grant'"),
                    []
                )
                .is_err()
        );
        assert_eq!(
            claim_tuple(&store.lock(), "grant"),
            (None, None, None, None)
        );
    }
    for value in [
        "",
        "+1",
        "-1",
        "01",
        " 1",
        "1 ",
        "1.0",
        "18446744073709551616",
        "x",
    ] {
        assert!(store.lock().execute("UPDATE approval_grants SET consumed_at='2026-01-01T00:00:00Z', claim_connection_id='fixture', claim_cap_revision=?1, claim_call_id='fixture' WHERE approval_id='grant'", [value]).is_err());
        assert_eq!(
            claim_tuple(&store.lock(), "grant"),
            (None, None, None, None)
        );
    }
    assert!(store.lock().execute("UPDATE approval_grants SET consumed_at='2026-01-01T00:00:00Z' WHERE approval_id='grant'", []).is_err());
    assert_eq!(
        claim_tuple(&store.lock(), "grant"),
        (None, None, None, None)
    );
    let device = identity();
    let args = json!({"a":1,"b":2});
    assert!(
        store
            .claim_node_grant_by_id(&request("grant", &device, &args))
            .unwrap()
            .is_ok()
    );
}

#[test]
fn node_migration_copy_failure_rolls_back_ddl_and_preserves_legacy_database() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = Connection::open(dir.path().join("approvals.db")).unwrap();
    conn.execute_batch(LEGACY_SCHEMA).unwrap();
    conn.execute_batch("INSERT INTO approval_grants VALUES ('retained','boot','run','tool','hash','start','end',NULL,'owner','test');
        INSERT INTO approval_audit VALUES (17,'time','boot','run',NULL,'tool','hash','summary','denied',NULL,NULL);").unwrap();
    let schema = rows(
        &conn,
        "SELECT type,name,sql FROM main.sqlite_master ORDER BY name",
    );
    let grants = rows(&conn, "SELECT * FROM approval_grants");
    let audit = rows(&conn, "SELECT * FROM approval_audit");
    let sequence = rows(&conn, "SELECT * FROM sqlite_sequence");
    // Private connection-local shadowing reaches INSERT after main-table CREATE.
    // Its named CHECK distinguishes transactional copy failure from eligibility refusal.
    conn.execute_batch(
        "CREATE TEMP TABLE approval_grants_v1 (
        approval_id TEXT, grant_kind TEXT, boot_id TEXT, run_id TEXT, tool_name TEXT,
        args_hash TEXT, granted_at TEXT, expires_at TEXT, consumed_at TEXT, approver TEXT,
        channel TEXT, CONSTRAINT migration_copy_fault CHECK (0));",
    )
    .unwrap();
    let error = schema::initialize(&mut conn, true).unwrap_err();
    assert!(
        error.to_string().contains("migration_copy_fault"),
        "must reach copy: {error}"
    );
    assert!(conn.is_autocommit());
    assert_eq!(
        rows(
            &conn,
            "SELECT type,name,sql FROM main.sqlite_master ORDER BY name"
        ),
        schema
    );
    assert_eq!(rows(&conn, "SELECT * FROM approval_grants"), grants);
    assert_eq!(rows(&conn, "SELECT * FROM approval_audit"), audit);
    assert_eq!(rows(&conn, "SELECT * FROM sqlite_sequence"), sequence);
    assert_eq!(
        conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    eprintln!(
        "NODE_MIGRATION_FAULT copy_constraint_observed=true ddl_rows_audit_indexes_rolled_back=true"
    );
    conn.execute_batch("DROP TABLE temp.approval_grants_v1")
        .unwrap();
    drop(conn);
    assert!(ApprovalStore::open(dir.path(), "boot").is_ok());
}

#[test]
fn node_claim_uses_expiry_without_inventing_a_not_before_policy() {
    let dir = tempfile::tempdir().unwrap();
    let store = ApprovalStore::open(dir.path(), "boot").unwrap();
    let mut grant = projection("grant");
    grant.granted_at = Utc::now() + Duration::minutes(1);
    store.insert_trusted_node_projection(&grant).unwrap();
    let device = identity();
    let args = json!({"a":1,"b":2});
    assert!(
        store
            .claim_node_grant_by_id(&request("grant", &device, &args))
            .unwrap()
            .is_ok()
    );
}
