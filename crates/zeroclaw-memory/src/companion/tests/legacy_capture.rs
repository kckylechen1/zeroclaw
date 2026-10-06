//! Historical capture rows for decoding and outbox tests only.
//! U4 retired capture writes, retries and turn-settlement identity minting.

use memcore::{MemoryEntry, OutboxEventMeta};
use zeroclaw_api::companion::{CaptureOutcome, CaptureReceipt};

use super::CompanionStore;

/// Seed the stored shape of an ordinary pre-U4 capture, using a real outbox commit.
/// This fixture does not implement the retired capture execution contract.
pub(super) fn seed_legacy_capture(store: &CompanionStore, turn_id: &str) -> CaptureReceipt {
    let persisted_at = chrono::Utc::now().to_rfc3339();
    let event_id = format!("capture:{turn_id}");
    let stored = serde_json::json!({
        "outcome": "not_evaluated",
        "event_id": event_id,
        "persisted_at": persisted_at,
        "agent_identity_id": "550e8400-e29b-41d4-a716-446655440000",
        "principal_id": "owner-principal",
        "session_id": "session-1",
        "turn_id": turn_id,
        "authority_class": "owner_authored",
        "origin": "channel",
        "partition": "user_model",
    });
    let entry = MemoryEntry {
        id: event_id.clone(),
        path: format!("/companion/user_model/capture_receipt/{turn_id}"),
        summary: "capture not_evaluated".into(),
        text: stored.to_string(),
        importance: 0.1,
        timestamp: persisted_at.clone(),
        valid_from: persisted_at.clone(),
        valid_until: None,
        category: "fact".into(),
        topic: "capture_receipt".into(),
        keywords: vec!["capture_receipt".into()],
        persons: Vec::new(),
        entities: Vec::new(),
        location: String::new(),
        source: "zeroclaw-companion-capture".into(),
        scope: "general".into(),
        archived: false,
        access_count: 0,
        scored_count: 0,
        last_use_at: None,
        last_access: None,
        revision: 1,
        vector: None,
        retention_policy: None,
        domain: Some("companion".into()),
        metadata: serde_json::json!({
            "object_class": "capture_receipt",
            "outcome": "not_evaluated",
            "session_id": "session-1",
            "turn_id": turn_id,
            "origin": "channel",
            "partition": "user_model",
        }),
        recall_count: 0,
        query_diversity: 0,
        tier: "raw".into(),
    };
    let meta = OutboxEventMeta {
        event_id,
        object_class: "capture_receipt".into(),
        authority_class: "owner_authored".into(),
        source_store: "companion".into(),
        source_partition: "user_model".into(),
    };
    let commit = store
        .with_store_mut(|mem| mem.commit_with_outbox_event(&entry, &meta))
        .expect("seed historical capture and pending outbox event");
    CaptureReceipt {
        outcome: CaptureOutcome::NotEvaluated,
        event_id: Some(commit.event.event_id),
        local_revision: Some(commit.object_revision),
        persisted_at,
    }
}

#[test]
fn historical_capture_decodes_with_attribution_and_pending_outbox_after_reopen() {
    use serde::Deserialize;
    use zeroclaw_api::companion::{AuthorityClass, CaptureOrigin, SourcePartition};

    // Historical storage shape, including attribution that is absent from the
    // public CaptureReceipt projection. Literal fixture tokens pin compatibility.
    #[derive(Deserialize)]
    struct StoredCaptureReceipt {
        outcome: CaptureOutcome,
        event_id: Option<String>,
        persisted_at: String,
        agent_identity_id: String,
        principal_id: String,
        session_id: String,
        turn_id: String,
        authority_class: AuthorityClass,
        origin: CaptureOrigin,
        partition: SourcePartition,
    }

    let tmp = tempfile::TempDir::new().unwrap();
    let path = tmp.path().join("companion-memory.db");
    let receipt = {
        let store = CompanionStore::open_runtime(&path).expect("open fixture store");
        seed_legacy_capture(&store, "turn-history")
    };
    let reopened = CompanionStore::open_runtime(&path).expect("reopen historical store");
    let entry = reopened.with_store(|mem| {
        mem.get("capture:turn-history")
            .expect("read historical row")
            .expect("historical row survives reopen")
    });
    let stored: StoredCaptureReceipt = serde_json::from_str(&entry.text).expect("decode history");
    assert_eq!(stored.outcome, CaptureOutcome::NotEvaluated);
    assert_eq!(stored.event_id, receipt.event_id);
    assert_eq!(stored.persisted_at, receipt.persisted_at);
    assert_eq!(Some(entry.revision), receipt.local_revision);
    assert!(receipt.is_durable());
    assert_eq!(stored.agent_identity_id, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(stored.principal_id, "owner-principal");
    assert_eq!(stored.session_id, "session-1");
    assert_eq!(stored.turn_id, "turn-history");
    assert_eq!(stored.authority_class, AuthorityClass::OwnerAuthored);
    assert_eq!(stored.origin, CaptureOrigin::Channel);
    assert_eq!(stored.partition, SourcePartition::UserModel);
    let pending = reopened.with_store(|mem| {
        mem.list_outbox_events(memcore::OutboxState::Pending, 100)
            .expect("read pending historical events")
    });
    assert_eq!(pending.len(), 1);
    assert_eq!(Some(&pending[0].event_id), receipt.event_id.as_ref());
}
