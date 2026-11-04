use super::format::Cell;
use super::*;
use crate::agent_scoped::AgentScopedMemory;
use crate::tachi::TachiMemory;
use crate::traits::{Memory, MemoryCategory};
use rusqlite::params_from_iter;
use serde_json::json;
use std::sync::Arc;

mod admission;
#[path = "../../tests/fixtures/legacy-kernel/data.rs"]
mod data;
mod files;

fn fixture(kind: KernelStoreKind) -> data::Fixture {
    let bytes = match kind {
        KernelStoreKind::Memory => {
            include_str!("../../tests/fixtures/legacy-kernel/memory-v38.json")
        }
        KernelStoreKind::Companion => {
            include_str!("../../tests/fixtures/legacy-kernel/companion-v38.json")
        }
    };
    let fixture: data::Fixture =
        serde_json::from_str(bytes).expect("authentic legacy fixture JSON");
    assert_eq!(fixture.source_revision, format::LEGACY_REVISION);
    assert_eq!(fixture.user_version, 38);
    fixture
}

fn insert_rows(conn: &Connection, name: &str, data: &data::TableData) {
    if data.rows.is_empty() {
        return;
    }
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        format::identifier(name),
        data.columns
            .iter()
            .map(|column| format::identifier(column))
            .collect::<Vec<_>>()
            .join(","),
        vec!["?"; data.columns.len()].join(",")
    );
    let mut statement = conn.prepare(&sql).expect("prepare authentic fixture rows");
    for row in &data.rows {
        statement
            .execute(params_from_iter(row))
            .expect("restore authentic fixture row");
    }
}

/// Reconstitute only trusted, committed fixture DDL. Unique indexes precede
/// DML because old foreign keys depend on them; protective triggers come last.
fn restore_fixture(path: &Path, kind: KernelStoreKind) {
    memcore::db::enable_simple_auto_extension().expect("simple tokenizer");
    memcore::db::register_sqlite_vec();
    let schema = schema::manifest(kind).expect("authentic schema manifest");
    let fixture = fixture(kind);
    create_private_file(path).expect("new fixture file");
    let mut conn = Connection::open(path).expect("open fixture");
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for object in &schema.objects {
        if object.object_type == "table"
            && object.name != "sqlite_sequence"
            && !format::physical_projection(&object.name)
        {
            conn.execute_batch(object.sql.as_deref().expect("table DDL"))
                .expect("restore authentic table");
        }
    }
    for object in &schema.objects {
        if object.object_type == "index"
            && !format::physical_projection(&object.table)
            && let Some(sql) = &object.sql
        {
            conn.execute_batch(sql)
                .expect("restore authentic index before DML");
        }
    }
    let tx = conn.transaction().unwrap();
    tx.execute_batch("PRAGMA defer_foreign_keys=ON").unwrap();
    for (name, table) in &fixture.tables {
        if name != "sqlite_sequence" {
            insert_rows(&tx, name, table);
        }
    }
    if let Some(sequence) = fixture.tables.get("sqlite_sequence") {
        tx.execute("DELETE FROM sqlite_sequence", []).unwrap();
        insert_rows(&tx, "sqlite_sequence", sequence);
    }
    tx.commit()
        .expect("authentic rows satisfy their foreign keys");
    for object in &schema.objects {
        if object.object_type == "trigger" || object.object_type == "view" {
            conn.execute_batch(object.sql.as_deref().expect("fixture object DDL"))
                .expect("restore authentic trigger or view");
        }
    }
    conn.pragma_update(None, "user_version", fixture.user_version)
        .unwrap();
    close_connection(conn).unwrap();
    checkpoint(path).unwrap();
    // Match the legacy generator's fresh-reader census. On the connection
    // that creates vec0, SQLite reports some backing tables as `shadow`;
    // their durable DDL and column shapes are unchanged after reopening.
    let conn = open_read_only_snapshot(path).unwrap();
    assert_eq!(format::objects(&conn).unwrap(), schema.objects);
    let shapes = format::table_shapes(&conn).unwrap();
    assert_eq!(
        shapes.keys().collect::<Vec<_>>(),
        schema.tables.keys().collect::<Vec<_>>(),
        "restored fixture table names"
    );
    for (name, expected) in &schema.tables {
        assert_eq!(
            shapes.get(name),
            Some(expected),
            "restored fixture table shape: {name}"
        );
    }
    close_connection(conn).unwrap();
}

fn mutate(path: &Path, mutation: impl FnOnce(&Connection)) {
    let conn = snapshot::open_auxiliary_target(path).unwrap();
    mutation(&conn);
    close_connection(conn).unwrap();
    checkpoint(path).unwrap();
}

fn table(path: &Path, name: &str, shape: &format::TableShape) -> data::TableData {
    let conn = open_read_only_snapshot(path).unwrap();
    let result = data::read_table(&conn, name, shape, true).unwrap();
    conn.execute_batch("ROLLBACK").unwrap();
    close_connection(conn).unwrap();
    result
}

fn assert_no_target(path: &Path) {
    for suffix in TARGET_SUFFIXES {
        assert!(
            !source::suffixed(path, suffix).exists(),
            "unexpected target artifact {suffix}"
        );
    }
    let parent = path.parent().unwrap();
    assert!(std::fs::read_dir(parent).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".zeroclaw-kernel-migration-")
    }));
}

fn assert_refused(kind: KernelStoreKind, mutation: impl FnOnce(&Connection), message: &str) {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, kind);
    mutate(&source_path, mutation);
    let before = source::census(&source_path).unwrap();
    let error =
        migrate_kernel_store(&source_path, &destination, kind, "fixture-operator").unwrap_err();
    assert!(
        format!("{error:#}").contains(message),
        "expected {message}, got {error:#}"
    );
    assert_eq!(source::census(&source_path).unwrap(), before);
    assert_no_target(&destination);
}

fn assert_receipt(path: &Path, kind: KernelStoreKind, receipt: &MigrationReceipt) {
    assert_eq!(receipt.source_schema, 38);
    assert_eq!(receipt.target_schema, 41);
    assert_eq!(receipt.target_profile, "portable_kernel");
    assert_eq!(receipt.role, kind.role());
    assert!(receipt.requires_offline_quiescent_source);
    assert_eq!(
        receipt.verification.source_tables,
        receipt.verification.target_tables
    );
    assert!(
        receipt
            .verification
            .source_tables
            .iter()
            .all(|table| table.sha256.len() == 64)
    );
    let store = open_store(path, kind, DbOpenContext::open_existing_deny()).unwrap();
    let stored = store
        .get_state_kv(RECEIPT_NAMESPACE, &receipt.receipt_key)
        .unwrap()
        .unwrap();
    assert_eq!(stored.0, serde_json::to_string(receipt).unwrap());
    assert_eq!(stored.1, 1);
}

#[test]
fn bundled_fixtures_pin_the_authentic_generated_schema_and_data_bytes() {
    let artifacts: &[(&[u8], &str)] = &[
        (
            include_bytes!("memory-schema-v38.json"),
            "be0ab6750fdbc6806b90b08ec363abd5ab75b062c93cc55cee3f634c6003f32a",
        ),
        (
            include_bytes!("companion-schema-v38.json"),
            "21b7b8f272646df8a617bfeeda0de8d920217586056007ba32a1b4a1d89b1dc8",
        ),
        (
            include_bytes!("../../tests/fixtures/legacy-kernel/memory-v38.json"),
            "7d86044e44956b1ed1740a7ca86ed614fdbe86162cf26ce3fbad2600438f20ac",
        ),
        (
            include_bytes!("../../tests/fixtures/legacy-kernel/companion-v38.json"),
            "0b3d63422d082a499f37a4527491d5a7ed1267859344a857ef4321908b7551a6",
        ),
    ];
    for (bytes, expected) in artifacts {
        assert_eq!(snapshot::bytes_digest(bytes), *expected);
    }
    for kind in [KernelStoreKind::Memory, KernelStoreKind::Companion] {
        fixture(kind);
    }
}

#[tokio::test]
async fn authentic_memory_preserves_lifecycle_vectors_auxiliary_and_agent_boundaries_after_restart()
{
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(workspace.join("memory")).unwrap();
    let destination = workspace.join("memory/tachi.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    let source_files = source::census(&source_path).unwrap();
    let legacy = schema::manifest(KernelStoreKind::Memory).unwrap();
    let receipt = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert_eq!(receipt.verification.source_files, source_files);
    assert_eq!(source::census(&source_path).unwrap(), source_files);
    assert_eq!(receipt.recall_cache_rows_invalidated, 1);
    assert_receipt(&destination, KernelStoreKind::Memory, &receipt);
    for name in [
        "memories_vec",
        "access_history",
        "edge_observations",
        "memory_edges",
        "recall_impressions",
        "sqlite_sequence",
    ] {
        assert_eq!(
            table(&source_path, name, &legacy.tables[name]),
            table(&destination, name, &legacy.tables[name]),
            "raw projection {name}"
        );
    }
    let conn = open_read_only_snapshot(&destination).unwrap();
    let lifecycle: (i64,String,Option<String>,String,String,Option<String>,i64) = conn.query_row(
        "SELECT revision,valid_from,valid_until,created_at,updated_at,superseded_by,archived FROM memories WHERE id='legacy-archived'", [],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?))).unwrap();
    assert_eq!(
        lifecycle,
        (
            0,
            String::new(),
            Some(String::new()),
            String::new(),
            String::new(),
            Some("legacy-active".into()),
            1
        )
    );
    let ordinary: (String,i64,String,String) = conn.query_row("SELECT value_json,version,created_at,updated_at FROM hard_state WHERE namespace='zeroclaw-fixture' AND key='ordinary'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    assert_eq!(
        ordinary,
        (
            "{ \"retained\": true, \"counter\": 7 }".into(),
            7,
            String::new(),
            "legacy-time".into()
        )
    );
    assert_eq!(conn.query_row("SELECT typeof(evidence_hash)||':'||length(evidence_hash) FROM edge_observations WHERE observation_id='fixture-observation'", [], |row| row.get::<_,String>(0)).unwrap(), "blob:0");
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM access_history WHERE memory_id='historical-orphan' AND accessed_at='' AND event_kind='display'", [], |row| row.get::<_,i64>(0)).unwrap(),1);
    conn.execute_batch("ROLLBACK").unwrap();
    close_connection(conn).unwrap();

    for phase in ["after migration", "after full restart"] {
        let backend: Arc<dyn Memory> = Arc::new(TachiMemory::new("default", &workspace).unwrap());
        let default = AgentScopedMemory::new(backend.clone(), "default", Vec::<String>::new());
        let researcher =
            AgentScopedMemory::new(backend.clone(), "researcher", Vec::<String>::new());
        let own = default
            .recall("legacy migration searchable fixture", 30, None, None, None)
            .await
            .unwrap();
        assert!(
            own.iter().any(|entry| entry.key == "legacy-active"),
            "default owns its active row {phase}"
        );
        assert!(
            own.iter().all(|entry| !matches!(
                entry.key.as_str(),
                "legacy-private" | "legacy-archived" | "legacy-superseded"
            )),
            "default boundary/lifecycle {phase}"
        );
        let private = researcher
            .recall("legacy migration searchable fixture", 30, None, None, None)
            .await
            .unwrap();
        assert!(
            private.iter().any(|entry| entry.key == "legacy-private"),
            "researcher owns its row {phase}"
        );
        assert!(
            private.iter().all(|entry| entry.key != "legacy-active"),
            "researcher cannot recall default {phase}"
        );
        assert!(
            default
                .recall_for_agents(
                    &["researcher"],
                    "legacy migration searchable fixture",
                    30,
                    None,
                    None,
                    None
                )
                .await
                .unwrap()
                .is_empty(),
            "explicit foreign allowlist cannot expand bound access {phase}"
        );
        drop(default);
        drop(researcher);
        drop(backend);
    }
}

#[test]
fn authentic_companion_preserves_outbox_leases_acks_local_wins_and_stale_revision_history() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("companion.db");
    restore_fixture(&source_path, KernelStoreKind::Companion);
    let before = source::census(&source_path).unwrap();
    let legacy = schema::manifest(KernelStoreKind::Companion).unwrap();
    let receipt = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Companion,
        "fixture-operator",
    )
    .unwrap();
    assert_eq!(source::census(&source_path).unwrap(), before);
    assert_eq!(
        table(
            &source_path,
            "memory_outbox_events",
            &legacy.tables["memory_outbox_events"]
        ),
        table(
            &destination,
            "memory_outbox_events",
            &legacy.tables["memory_outbox_events"]
        )
    );
    assert_eq!(receipt.source_row_counts["memories"], 4);
    assert_eq!(receipt.source_row_counts["memory_outbox_events"], 5);
    for _ in 0..2 {
        assert_receipt(&destination, KernelStoreKind::Companion, &receipt);
        let conn = open_read_only_snapshot(&destination).unwrap();
        let states: Vec<(String, String)> = conn
            .prepare("SELECT event_id,state FROM memory_outbox_events ORDER BY event_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(states.contains(&("capture:fixture-pending".into(), "pending".into())));
        assert!(states.contains(&("capture:fixture-claimed".into(), "in_flight".into())));
        assert!(states.contains(&("capture:fixture-acked".into(), "acknowledged".into())));
        assert!(states.contains(&("capture:fixture-local-wins".into(), "quarantined".into())));
        assert!(states.contains(&(
            "capture:fixture-local-wins::local-wins".into(),
            "pending".into()
        )));
        let stale: i64 = conn.query_row("SELECT COUNT(*) FROM memory_outbox_events e JOIN memories m ON m.id=e.object_id WHERE e.source_revision<m.revision", [], |row| row.get(0)).unwrap();
        assert!(
            stale >= 2,
            "old ack and resolved conflict retain historical revision/digest"
        );
        let body: String = conn
            .query_row(
                "SELECT text FROM memories WHERE id='capture:fixture-local-wins'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(payload["event_id"], "capture:fixture-local-wins");
        assert_eq!(payload["partition"], "user_model");
        assert_eq!(payload["principal_id"], "fixture-principal");
        assert_eq!(payload["session_id"], "fixture-session");
        assert_eq!(
            serde_json::from_value::<zeroclaw_api::companion::CaptureOutcome>(
                payload["outcome"].clone()
            )
            .unwrap(),
            zeroclaw_api::companion::CaptureOutcome::NotEvaluated
        );
        conn.execute_batch("ROLLBACK").unwrap();
        close_connection(conn).unwrap();
    }
}

#[test]
fn migration_retains_duplicate_bodies_distinct_ids_and_same_key_across_agents() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    mutate(&source_path, |conn| {
        let path = TachiMemory::storage_path(
            Some("researcher"),
            Some("user"),
            &MemoryCategory::Core,
            "legacy-active",
        );
        conn.execute("UPDATE memories SET text=(SELECT text FROM memories WHERE id='legacy-active'),summary=(SELECT summary FROM memories WHERE id='legacy-active'),path=?1,metadata=json_set(metadata,'$.zeroclaw_key','legacy-active') WHERE id='legacy-private'", [path]).unwrap();
    });
    let receipt = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert_eq!(receipt.source_row_counts["memories"], 4);
    let conn = open_read_only_snapshot(&destination).unwrap();
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM memories WHERE json_extract(metadata,'$.zeroclaw_key')='legacy-active'", [], |row| row.get::<_,i64>(0)).unwrap(),2);
    assert_eq!(conn.query_row("SELECT COUNT(DISTINCT id) FROM memories WHERE text=(SELECT text FROM memories WHERE id='legacy-active')", [], |row| row.get::<_,i64>(0)).unwrap(),2);
    conn.execute_batch("ROLLBACK").unwrap();
    close_connection(conn).unwrap();
}

#[test]
fn custom_core_category_and_sequence_high_watermarks_are_preserved() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    mutate(&source_path, |conn| {
        let path = TachiMemory::storage_path(
            Some("default"),
            Some("default"),
            &MemoryCategory::Custom("core".into()),
            "legacy-active",
        );
        conn.execute(
            "UPDATE memories SET category='other',path=?1 WHERE id='legacy-active'",
            [path],
        )
        .unwrap();
        conn.execute("UPDATE sqlite_sequence SET seq=seq+100", [])
            .unwrap();
    });
    let receipt = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert_receipt(&destination, KernelStoreKind::Memory, &receipt);
    let legacy = schema::manifest(KernelStoreKind::Memory).unwrap();
    assert_eq!(
        table(
            &source_path,
            "sqlite_sequence",
            &legacy.tables["sqlite_sequence"]
        ),
        table(
            &destination,
            "sqlite_sequence",
            &legacy.tables["sqlite_sequence"]
        )
    );
}

#[test]
fn absent_optional_vec_projection_is_recognized_without_losing_other_rows() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    mutate(&source_path, |conn| {
        conn.execute_batch("DROP TABLE memories_vec").unwrap();
    });
    let receipt = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert!(
        !receipt
            .verification
            .source_tables
            .iter()
            .any(|table| table.table == "memories_vec")
    );
    assert_eq!(receipt.source_row_counts["memories"], 4);
}

#[test]
fn previous_host_receipts_remain_in_projection_and_kernel_guards_stay_denied() {
    let root = tempfile::TempDir::new().unwrap();
    let source_path = root.path().join("legacy.db");
    let destination = root.path().join("new.db");
    restore_fixture(&source_path, KernelStoreKind::Memory);
    mutate(&source_path, |conn| {
        conn.execute("INSERT INTO hard_state(namespace,key,value_json,version,created_at,updated_at) VALUES (?1,'prior-receipt','{ \"old\": true }',4,'','')", [RECEIPT_NAMESPACE]).unwrap();
    });
    let receipt = migrate_kernel_store(
        &source_path,
        &destination,
        KernelStoreKind::Memory,
        "fixture-operator",
    )
    .unwrap();
    assert_receipt(&destination, KernelStoreKind::Memory, &receipt);
    let conn = snapshot::open_auxiliary_target(&destination).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT value_json FROM hard_state WHERE namespace=?1 AND key='prior-receipt'",
            [RECEIPT_NAMESPACE],
            |row| row.get::<_, String>(0)
        )
        .unwrap(),
        "{ \"old\": true }"
    );
    assert_eq!(
        conn.query_row(
            "SELECT tachi_reserved_reference_write_enabled()",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(conn.execute("INSERT INTO hard_state(namespace,key,value_json,version,created_at,updated_at) VALUES ('typed_record_kinds','forbidden','{}',1,'','')",[]).is_err());
    close_connection(conn).unwrap();
}
