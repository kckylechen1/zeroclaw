//! Run only against the checked-out, verified legacy pin; never the new kernel.
#![allow(dead_code)]

mod data;
#[path = "../../../src/portable_migration/format.rs"]
mod format;

use anyhow::{ensure, Context, Result};
use data::Fixture;
use format::{Manifest, LEGACY_REVISION};
use memcore::types::MemorySource;
use memcore::{
    DbOpenContext, MemoryEntry, MemoryStore, OutboxConflictResolution, OutboxEventMeta,
    OutboxState, PortableImportEntry, StoreProfile,
};
use rusqlite::{params, Connection};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const STAMP: &str = "2026-08-01T10:00:00.000Z";

fn encoded(value: &str) -> String {
    let clean: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let digest = format!("{:x}", Sha256::digest(value.as_bytes()));
    format!(
        "{}__{}",
        if clean.is_empty() { "x" } else { &clean },
        &digest[..16]
    )
}

fn memory(id: &str, agent: &str, namespace: &str) -> MemoryEntry {
    MemoryEntry {
        id: id.into(),
        path: format!(
            "/agents/{}/{}/core/{}",
            encoded(agent),
            encoded(namespace),
            encoded(id)
        ),
        summary: "synthetic legacy kernel fixture".into(),
        text: format!("legacy migration searchable fixture {id}"),
        importance: 0.7,
        timestamp: STAMP.into(),
        valid_from: STAMP.into(),
        valid_until: None,
        category: "fact".into(),
        topic: "fixture".into(),
        keywords: vec!["legacy".into()],
        persons: vec![],
        entities: vec!["fixture".into()],
        location: String::new(),
        source: MemorySource::parse_or_external("zeroclaw"),
        scope: if namespace == "default" {
            "general"
        } else if namespace == "user" || namespace.starts_with("user:") {
            "user"
        } else {
            "project"
        }
        .into(),
        archived: false,
        access_count: 7,
        scored_count: 9,
        last_access: Some(STAMP.into()),
        last_use_at: Some("2026-07-31T10:00:00+00:00".into()),
        revision: 3,
        vector: None,
        retention_policy: Some("durable".into()),
        domain: Some("zeroclaw".into()),
        metadata: json!({"zeroclaw_key":id,"zeroclaw_category":"core","zeroclaw_agent":agent,"zeroclaw_namespace":namespace,"session_id":"fixture-session"}),
        recall_count: 5,
        query_diversity: 2,
        tier: "raw".into(),
    }
}

fn snapshot(entry: MemoryEntry) -> PortableImportEntry {
    PortableImportEntry {
        entry,
        created_at: STAMP.into(),
        updated_at: STAMP.into(),
        superseded_by: None,
    }
}

fn seed_memory(store: &mut MemoryStore) -> Result<()> {
    // Exercise the historical adapter's ordinary write path, including source normalization.
    let mut active = memory("legacy-active", "default", "default");
    active.source = "zeroclaw".into();
    active.vector = Some((0..1024).map(|index| index as f32 / 1024.0).collect());
    store.upsert(&active)?;

    let mut archived = snapshot(memory("legacy-archived", "default", "default"));
    archived.entry.archived = true;
    archived.entry.revision = 0;
    archived.entry.valid_from.clear();
    archived.entry.valid_until = Some(String::new());
    archived.created_at.clear();
    archived.updated_at.clear();
    archived.superseded_by = Some("legacy-active".into());
    let private = snapshot(memory("legacy-private", "researcher", "user"));
    let mut superseded = snapshot(memory("legacy-superseded", "default", "default"));
    superseded.superseded_by = Some("legacy-active".into());
    store.import_snapshot_batch(&[archived, private, superseded])?;
    Ok(())
}

fn seed_companion(store: &mut MemoryStore) -> Result<()> {
    for (turn, state) in [
        ("pending", OutboxState::Pending),
        ("claimed", OutboxState::InFlight),
        ("acked", OutboxState::Acknowledged),
        ("local-wins", OutboxState::Conflicted),
    ] {
        let id = format!("capture:fixture-{turn}");
        let mut entry = memory(&id, "default", "default");
        entry.path = format!("/companion/user_model/capture_receipt/fixture-{turn}");
        entry.text = json!({
            "outcome":"not_evaluated", "event_id":id, "persisted_at":STAMP,
            "agent_identity_id":"550e8400-e29b-41d4-a716-446655440000",
            "principal_id":"fixture-principal", "session_id":"fixture-session",
            "turn_id":format!("fixture-{turn}"), "authority_class":"owner_authored",
            "origin":"channel", "partition":"user_model"
        })
        .to_string();
        entry.topic = "capture_receipt".into();
        entry.source = "zeroclaw-companion-capture".into();
        entry.domain = Some("companion".into());
        entry.metadata = json!({"object_class":"capture_receipt","outcome":"not_evaluated","session_id":"fixture-session","turn_id":format!("fixture-{turn}"),"origin":"channel","partition":"user_model"});
        let meta = OutboxEventMeta {
            event_id: id.clone(),
            object_class: "capture_receipt".into(),
            authority_class: "owner_authored".into(),
            source_store: "companion".into(),
            source_partition: "user_model".into(),
        };
        store.commit_with_outbox_event(&entry, &meta)?;
        if state != OutboxState::Pending {
            store.transition_outbox_event(&id, OutboxState::InFlight, None)?;
        }
        if state == OutboxState::Acknowledged {
            store.transition_outbox_event(&id, state, None)?;
            // A completed event remains valid historical evidence when its
            // memory later advances. Never rewrite the old digest/revision.
            entry.summary = "synthetic capture updated after acknowledgement".into();
            store.upsert(&entry)?;
        }
        if state == OutboxState::Conflicted {
            store.transition_outbox_event(&id, state, Some("divergent_revision"))?;
            entry.summary = "synthetic capture updated before local-wins resolution".into();
            store.upsert(&entry)?;
            // Exercise the real legacy public protocol: quarantined original
            // plus a pending ::local-wins successor, with unchanged body ID.
            store.resolve_outbox_conflict(&id, &OutboxConflictResolution::LocalWins)?;
        }
    }
    Ok(())
}

fn seed_auxiliary(conn: &Connection, companion: bool) -> Result<()> {
    memcore::db::ensure_reserved_reference_write_guard(conn)?;
    conn.execute("INSERT INTO hard_state(namespace,key,value_json,version,created_at,updated_at) VALUES ('zeroclaw-fixture','ordinary',?1,7,'','legacy-time')", ["{ \"retained\": true, \"counter\": 7 }"])?;
    conn.execute("INSERT INTO access_history(rowid,memory_id,accessed_at,query_hash,event_kind) VALUES (70,?1,'2026-07-31T10:00:00+00:00','fixture-query','use')", [if companion { "capture:fixture-pending" } else { "legacy-active" }])?;
    conn.execute("INSERT INTO access_history(rowid,memory_id,accessed_at,query_hash,event_kind) VALUES (91,'historical-orphan','','','display')", [])?;
    conn.execute(
        "INSERT INTO recall_cache(cache_id,query,rows_json) VALUES ('stale-fixture','legacy','[]')",
        [],
    )?;
    conn.execute("INSERT INTO memory_edges(source_id,target_id,relation,weight,metadata,created_at) VALUES ('legacy-active','legacy-private','related',0.75,'{ \"raw\": true }','')", [])?;
    conn.execute("INSERT INTO edge_observations(observation_id,source_id,target_id,relation,observed_at,evidence_hash) VALUES ('fixture-observation','legacy-active','legacy-private','related','',?1)", params![Vec::<u8>::new()])?;
    conn.execute("INSERT INTO processed_events(event_hash,event_id,worker,created_at) VALUES ('fixture-hash','fixture-event','fixture-worker','')", [])?;
    conn.execute("INSERT INTO tachi_events(id,event_type,created_at,payload_json) VALUES ('fixture-event','zeroclaw.fixture','',?1)", ["{ \"retained\": true }"])?;
    conn.execute("INSERT INTO derived_items(id,text,path,source,metadata) VALUES ('fixture-derived','synthetic derived evidence','/agents/fixture','external:zeroclaw','{}')", [])?;
    // An empty BLOB in the nullable ordinary evidence cell above is distinct
    // from NULL. SQLite permits it despite TEXT affinity; the converter must
    // carry the actual storage class, not coerce it through a typed reader.
    conn.execute("INSERT INTO recall_impression_groups(group_id,created_at,weights_profile,semantic_weight,fts_weight,symbolic_weight,decay_weight,use_rrf,rrf_k,top_k,candidate_count,displayed_count,scored_returned_count) VALUES ('fixture-group','','fixture',0.7,0.2,0.1,0.0,1,60.0,5,1,1,1)", [])?;
    conn.execute("INSERT INTO recall_impressions(group_id,memory_id,vector_score,fts_score,symbolic_score,decay_score,merge_adjustment,pre_boost_score,pre_boost_rank,tie_break_epoch_millis,final_score,final_rank,scored,scored_returned,access_count_at_recall) VALUES ('fixture-group','legacy-active',0.7,0.2,0.1,0.0,'none',0.8,1,0,0.8,1,1,1,7)", [])?;
    Ok(())
}

fn manifest(conn: &Connection, profile: StoreProfile) -> Result<Manifest> {
    let user_version = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        user_version == 38,
        "generator must run the authentic schema38 kernel"
    );
    let mut statement =
        conn.prepare("SELECT key FROM hard_state WHERE namespace='migrations' ORDER BY key")?;
    let migration_keys = statement
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Manifest {
        source_revision: LEGACY_REVISION.into(),
        user_version,
        profile: profile.as_str().into(),
        objects: format::objects(conn)?,
        tables: format::table_shapes(conn)?,
        migration_keys,
    })
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)?;
    serde_json::to_writer_pretty(file, value)?;
    Ok(())
}

fn generate(root: &Path, name: &str, profile: StoreProfile) -> Result<()> {
    let path = root.join(format!("{name}-v38.db"));
    ensure!(!path.exists(), "fixture destination already exists");
    let mut store = MemoryStore::open_with_context(
        path.to_str().context("UTF-8 fixture path")?,
        &DbOpenContext::create_fresh().with_profile(profile),
    )?;
    if profile == StoreProfile::TachiFull {
        seed_memory(&mut store)?;
    } else {
        seed_companion(&mut store)?;
    }
    drop(store);
    let conn = Connection::open(&path)?;
    seed_auxiliary(&conn, profile == StoreProfile::PortableKernel)?;
    let schema = manifest(&conn, profile)?;
    write_json(&root.join(format!("{name}-schema-v38.json")), &schema)?;
    let mut tables = BTreeMap::new();
    for (table, shape) in &schema.tables {
        if format::physical_projection(table) {
            continue;
        }
        tables.insert(table.clone(), data::read_table(&conn, table, shape, true)?);
    }
    write_json(
        &root.join(format!("{name}-v38.json")),
        &Fixture {
            source_revision: LEGACY_REVISION.into(),
            user_version: 38,
            tables,
        },
    )?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    conn.close().map_err(|(_, error)| error)?;
    Ok(())
}

fn main() -> Result<()> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .context("usage: generate-legacy-kernel-fixture OUTPUT_DIRECTORY")?,
    );
    std::fs::create_dir(&root).context("create a new output directory")?;
    generate(&root, "memory", StoreProfile::TachiFull)?;
    generate(&root, "companion", StoreProfile::PortableKernel)?;
    Ok(())
}
