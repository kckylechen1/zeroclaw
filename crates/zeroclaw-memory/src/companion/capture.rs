//! Companion-capture seam: turn context in, durable receipt out.
//!
//! Candidate evaluation is a later slice. This module always records a typed
//! outcome — including negative ones — so an empty store cannot mean
//! "capture never ran."

use zeroclaw_api::companion::{CaptureContext, CaptureOutcome, CaptureReceipt};

use super::CompanionStore;

/// Writes one capture receipt for a turn. Does not own store open/close.
pub struct CompanionCapture<'a> {
    store: &'a CompanionStore,
}

impl<'a> CompanionCapture<'a> {
    #[must_use]
    pub fn new(store: &'a CompanionStore) -> Self {
        Self { store }
    }

    /// Placeholder judgment: persist [`CaptureOutcome::NotEvaluated`].
    ///
    /// User Model / Soul / LLM extraction are later slices. This call is the
    /// synchronous turn-path contract: the receipt lands before any detached
    /// work.
    #[must_use]
    pub fn capture(&self, context: &CaptureContext) -> CaptureReceipt {
        self.persist(context, CaptureOutcome::NotEvaluated, persist_write_mode())
    }

    /// Read a previously persisted receipt for `turn_id`, if the row exists.
    #[must_use]
    pub fn read_receipt(&self, turn_id: &str) -> Option<CaptureReceipt> {
        #[cfg(feature = "tachi")]
        {
            persist::read_receipt(self.store, turn_id)
        }
        #[cfg(not(feature = "tachi"))]
        {
            let _ = (self.store, turn_id);
            None
        }
    }

    fn persist(
        &self,
        context: &CaptureContext,
        outcome: CaptureOutcome,
        mode: WriteMode,
    ) -> CaptureReceipt {
        #[cfg(feature = "tachi")]
        {
            persist::persist_receipt(self.store, context, outcome, mode)
        }
        #[cfg(not(feature = "tachi"))]
        {
            let _ = (self.store, context, outcome, mode);
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "companion capture ran against a feature-off store; receipt is not durable"
            );
            ephemeral_receipt(CaptureOutcome::LocalWriteFailed)
        }
    }
}

/// No-op when the companion store is closed (`None`).
#[must_use]
pub fn capture_turn_if_present(
    store: Option<&CompanionStore>,
    context: &CaptureContext,
) -> Option<CaptureReceipt> {
    store.map(|store| CompanionCapture::new(store).capture(context))
}

/// Channel close-out helper. `agent_alias` is stamped as the opaque agent
/// identity until per-alias UUID minting exists; it is not an identity source
/// of record.
#[must_use]
pub fn capture_channel_turn(
    store: Option<&CompanionStore>,
    agent_alias: &str,
    session_id: &str,
    turn_id: &str,
    channel: &str,
    sender: &str,
    owner: &zeroclaw_api::companion::CompanionOwnerGate,
) -> Option<CaptureReceipt> {
    let context = CaptureContext::from_channel_identity(
        zeroclaw_api::companion::AgentIdentityId::from_opaque(agent_alias),
        session_id,
        turn_id,
        zeroclaw_api::companion::IngressIdentity::new(format!("{channel}:{sender}")),
        owner,
    );
    capture_turn_if_present(store, &context)
}

/// Gateway WebSocket close-out helper. Same agent-identity stand-in as
/// [`capture_channel_turn`].
#[must_use]
pub fn capture_gateway_turn(
    store: Option<&CompanionStore>,
    agent_alias: &str,
    session_id: &str,
    turn_id: &str,
    identity: &str,
    owner: &zeroclaw_api::companion::CompanionOwnerGate,
) -> Option<CaptureReceipt> {
    let context = CaptureContext::from_gateway_identity(
        zeroclaw_api::companion::AgentIdentityId::from_opaque(agent_alias),
        session_id,
        turn_id,
        zeroclaw_api::companion::IngressIdentity::new(identity),
        owner,
    );
    capture_turn_if_present(store, &context)
}

#[derive(Clone, Copy)]
enum WriteMode {
    Normal,
    #[cfg(all(test, feature = "tachi"))]
    FailPrimary,
    #[cfg(all(test, feature = "tachi"))]
    FailAll,
}

const fn persist_write_mode() -> WriteMode {
    WriteMode::Normal
}

fn ephemeral_receipt(outcome: CaptureOutcome) -> CaptureReceipt {
    CaptureReceipt {
        outcome,
        event_id: None,
        local_revision: None,
        persisted_at: now_rfc3339(),
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(feature = "tachi")]
mod persist {
    use memcore::{MemoryEntry, MemoryError, OutboxEventMeta};
    use serde::{Deserialize, Serialize};
    use zeroclaw_api::companion::{
        AuthorityClass, CaptureContext, CaptureOrigin, CaptureOutcome, CaptureReceipt,
        SourcePartition,
    };

    use super::{WriteMode, ephemeral_receipt, now_rfc3339};
    use crate::companion::CompanionStore;

    fn receipt_object_id(turn_id: &str) -> String {
        format!("capture:{turn_id}")
    }

    const OBJECT_CLASS: &str = "capture_receipt";
    const SOURCE_STORE: &str = "companion";
    const ENTRY_SOURCE: &str = "zeroclaw-companion-capture";

    #[derive(Debug, Serialize, Deserialize)]
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

    pub(super) fn persist_receipt(
        store: &CompanionStore,
        context: &CaptureContext,
        outcome: CaptureOutcome,
        mode: WriteMode,
    ) -> CaptureReceipt {
        if let Some(existing) = read_receipt(store, context.turn_id()) {
            return existing;
        }
        #[cfg(test)]
        if matches!(mode, WriteMode::FailAll) {
            warn_write_failed(
                context,
                &MemoryError::Internal("forced undurable write".to_string()),
            );
            return ephemeral_receipt(CaptureOutcome::LocalWriteFailed);
        }

        #[cfg(test)]
        let primary = if matches!(mode, WriteMode::FailPrimary) {
            write_sabotaged(store, context, outcome)
        } else {
            write_outcome(store, context, outcome)
        };
        #[cfg(not(test))]
        let primary = {
            let _ = mode;
            write_outcome(store, context, outcome)
        };

        match primary {
            Ok(receipt) => receipt,
            Err(err) => {
                if outcome == CaptureOutcome::LocalWriteFailed {
                    warn_write_failed(context, &err);
                    return ephemeral_receipt(CaptureOutcome::LocalWriteFailed);
                }
                match write_outcome(store, context, CaptureOutcome::LocalWriteFailed) {
                    Ok(receipt) => receipt,
                    Err(fallback_err) => {
                        warn_write_failed(context, &fallback_err);
                        ephemeral_receipt(CaptureOutcome::LocalWriteFailed)
                    }
                }
            }
        }
    }

    pub(super) fn read_receipt(store: &CompanionStore, turn_id: &str) -> Option<CaptureReceipt> {
        let id = receipt_object_id(turn_id);
        let entry = store.with_store(|mem| mem.get(&id).ok().flatten())?;
        receipt_from_entry(&entry)
    }

    fn write_outcome(
        store: &CompanionStore,
        context: &CaptureContext,
        outcome: CaptureOutcome,
    ) -> Result<CaptureReceipt, MemoryError> {
        let persisted_at = now_rfc3339();
        let event_id = outbox_event_id(context, outcome);
        let stored = StoredCaptureReceipt {
            outcome,
            event_id: event_id.clone(),
            persisted_at: persisted_at.clone(),
            agent_identity_id: context.agent_identity_id().as_str().to_string(),
            principal_id: context.principal().id.as_str().to_string(),
            session_id: context.session_id().to_string(),
            turn_id: context.turn_id().to_string(),
            authority_class: context.authority_class(),
            origin: context.origin(),
            partition: context.partition(),
        };
        let entry = memory_entry(context, &stored)?;
        match (event_id, context.partition().outbox_token()) {
            (Some(event_id), Some(partition)) => {
                let meta = OutboxEventMeta {
                    event_id: event_id.clone(),
                    object_class: OBJECT_CLASS.to_string(),
                    authority_class: context.authority_class().as_str().to_string(),
                    source_store: SOURCE_STORE.to_string(),
                    source_partition: partition.to_string(),
                };
                let commit =
                    store.with_store_mut(|mem| mem.commit_with_outbox_event(&entry, &meta))?;
                Ok(CaptureReceipt {
                    outcome,
                    event_id: Some(commit.event.event_id),
                    local_revision: Some(commit.object_revision),
                    persisted_at,
                })
            }
            _ => {
                store.with_store_mut(|mem| mem.upsert(&entry))?;
                let revision = store
                    .with_store(|mem| mem.get(&entry.id))?
                    .ok_or_else(|| {
                        MemoryError::NotFound(format!(
                            "capture receipt '{}' missing after upsert",
                            entry.id
                        ))
                    })?
                    .revision;
                Ok(CaptureReceipt {
                    outcome,
                    event_id: None,
                    local_revision: Some(revision),
                    persisted_at,
                })
            }
        }
    }

    #[cfg(test)]
    fn write_sabotaged(
        store: &CompanionStore,
        context: &CaptureContext,
        outcome: CaptureOutcome,
    ) -> Result<CaptureReceipt, MemoryError> {
        let persisted_at = now_rfc3339();
        let stored = StoredCaptureReceipt {
            outcome,
            event_id: None,
            persisted_at,
            agent_identity_id: context.agent_identity_id().as_str().to_string(),
            principal_id: context.principal().id.as_str().to_string(),
            session_id: context.session_id().to_string(),
            turn_id: context.turn_id().to_string(),
            authority_class: context.authority_class(),
            origin: context.origin(),
            partition: context.partition(),
        };
        let mut entry = memory_entry(context, &stored)?;
        entry.id = format!("wiki-rem:{}", entry.id);
        store.with_store_mut(|mem| mem.upsert(&entry))?;
        Err(MemoryError::Internal(
            "sabotaged primary write unexpectedly succeeded".to_string(),
        ))
    }

    fn memory_entry(
        context: &CaptureContext,
        stored: &StoredCaptureReceipt,
    ) -> Result<MemoryEntry, MemoryError> {
        let text = serde_json::to_string(stored)?;
        let now = stored.persisted_at.clone();
        Ok(MemoryEntry {
            id: receipt_object_id(context.turn_id()),
            path: format!(
                "/companion/{}/capture_receipt/{}",
                context.partition().as_str(),
                context.turn_id()
            ),
            summary: format!("capture {}", stored.outcome.as_str()),
            text,
            importance: 0.1,
            timestamp: now.clone(),
            valid_from: now,
            valid_until: None,
            category: "fact".into(),
            topic: "capture_receipt".into(),
            keywords: vec!["capture_receipt".into()],
            persons: Vec::new(),
            entities: Vec::new(),
            location: String::new(),
            source: ENTRY_SOURCE.into(),
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
                "object_class": OBJECT_CLASS,
                "outcome": stored.outcome.as_str(),
                "session_id": stored.session_id,
                "turn_id": stored.turn_id,
                "origin": stored.origin,
                "partition": stored.partition.as_str(),
            }),
            recall_count: 0,
            query_diversity: 0,
            tier: "raw".into(),
        })
    }

    fn outbox_event_id(context: &CaptureContext, outcome: CaptureOutcome) -> Option<String> {
        context.partition().outbox_token()?;
        Some(match outcome {
            CaptureOutcome::LocalWriteFailed => {
                format!("capture:{}:write_failed", context.turn_id())
            }
            _ => format!("capture:{}", context.turn_id()),
        })
    }

    fn receipt_from_entry(entry: &MemoryEntry) -> Option<CaptureReceipt> {
        let stored: StoredCaptureReceipt = serde_json::from_str(&entry.text).ok()?;
        Some(CaptureReceipt {
            outcome: stored.outcome,
            event_id: stored.event_id,
            local_revision: Some(entry.revision),
            persisted_at: stored.persisted_at,
        })
    }

    fn warn_write_failed(context: &CaptureContext, err: &MemoryError) {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "turn_id": context.turn_id(),
                    "session_id": context.session_id(),
                    "error": err.to_string(),
                })),
            "companion capture could not persist a receipt; degrading without a durable row"
        );
    }
}

#[cfg(all(test, feature = "tachi"))]
impl CompanionCapture<'_> {
    fn capture_outcome(&self, context: &CaptureContext, outcome: CaptureOutcome) -> CaptureReceipt {
        self.persist(context, outcome, WriteMode::Normal)
    }

    fn capture_forcing_primary_write_failure(&self, context: &CaptureContext) -> CaptureReceipt {
        self.persist(
            context,
            CaptureOutcome::NotEvaluated,
            WriteMode::FailPrimary,
        )
    }

    fn capture_forcing_all_writes_to_fail(&self, context: &CaptureContext) -> CaptureReceipt {
        self.persist(context, CaptureOutcome::NotEvaluated, WriteMode::FailAll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::companion::{
        AgentIdentityId, CaptureContext, CompanionOwnerGate, IngressIdentity,
    };
    use zeroclaw_api::principal::PrincipalId;

    fn owner_gate() -> CompanionOwnerGate {
        CompanionOwnerGate {
            principal_id: PrincipalId::from("owner-principal"),
            identities: vec![IngressIdentity::new("wechat:alice")],
            trust_local: true,
        }
    }

    fn context(turn_id: &str) -> CaptureContext {
        CaptureContext::from_channel_identity(
            AgentIdentityId::from_opaque("agent-alias"),
            "session-1",
            turn_id,
            IngressIdentity::new("wechat:alice"),
            &owner_gate(),
        )
    }

    #[test]
    fn store_none_is_a_zero_cost_no_op() {
        let ctx = context("turn-none");
        assert!(capture_turn_if_present(None, &ctx).is_none());
        let owner = owner_gate();
        assert!(
            capture_channel_turn(None, "agent", "sess", "turn", "wechat", "alice", &owner)
                .is_none()
        );
        assert!(capture_gateway_turn(None, "agent", "sess", "turn", "wss", &owner).is_none());
    }

    #[cfg(feature = "tachi")]
    mod persist_tests {
        use super::*;
        use crate::companion::{CompanionCapture, create_companion_store};
        use memcore::OutboxState;
        use tempfile::TempDir;
        use zeroclaw_api::companion::{CaptureOutcome, SourcePartition};
        use zeroclaw_config::schema::Config;

        fn enabled_config(data_dir: &std::path::Path) -> Config {
            let mut config = Config {
                data_dir: data_dir.to_path_buf(),
                ..Config::default()
            };
            config.companion_memory.enable = true;
            config
        }

        fn open_store(tmp: &TempDir) -> std::sync::Arc<crate::companion::CompanionStore> {
            create_companion_store(&enabled_config(tmp.path()))
                .expect("factory")
                .expect("enabled")
        }

        fn pending_outbox_count(store: &crate::companion::CompanionStore) -> usize {
            store
                .with_store(|mem| {
                    mem.list_outbox_events(OutboxState::Pending, 100)
                        .expect("list")
                })
                .len()
        }

        #[test]
        fn capture_persists_not_evaluated_and_survives_restart() {
            let tmp = TempDir::new().unwrap();
            let path;
            {
                let store = open_store(&tmp);
                path = store.path().to_path_buf();
                let ctx = context("turn-restart");
                let receipt = CompanionCapture::new(&store).capture(&ctx);
                assert_eq!(receipt.outcome, CaptureOutcome::NotEvaluated);
                assert!(receipt.is_durable());
                assert!(receipt.event_id.is_some());
                assert!(receipt.local_revision.is_some());
            }
            let reopened = crate::companion::CompanionStore::open_runtime(&path).expect("reopen");
            let read = CompanionCapture::new(&reopened)
                .read_receipt("turn-restart")
                .expect("row after restart");
            assert_eq!(read.outcome, CaptureOutcome::NotEvaluated);
            assert!(read.is_durable());
        }

        #[test]
        fn negative_outcomes_each_leave_a_row() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let capture = CompanionCapture::new(&store);
            for (turn_id, outcome) in [
                ("turn-no-candidate", CaptureOutcome::NoCandidate),
                ("turn-rejected", CaptureOutcome::CandidateRejectedByPolicy),
                ("turn-write-failed", CaptureOutcome::LocalWriteFailed),
            ] {
                let receipt = capture.capture_outcome(&context(turn_id), outcome);
                assert_eq!(receipt.outcome, outcome);
                assert!(receipt.is_durable(), "{outcome}");
                let read = capture.read_receipt(turn_id).expect("row");
                assert_eq!(read.outcome, outcome);
            }
        }

        #[test]
        fn private_dyad_never_enqueues_an_outbox_event() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let ctx = context("turn-dyad").with_partition(SourcePartition::PrivateDyad);
            let receipt = CompanionCapture::new(&store).capture(&ctx);
            assert_eq!(receipt.outcome, CaptureOutcome::NotEvaluated);
            assert!(receipt.is_durable());
            assert!(
                receipt.event_id.is_none(),
                "private dyad must not mint an outbox event id"
            );
            assert_eq!(pending_outbox_count(&store), 0);
            assert!(
                CompanionCapture::new(&store)
                    .read_receipt("turn-dyad")
                    .is_some(),
                "the memories row must still land"
            );
        }

        #[test]
        fn ordinary_partition_enqueues_one_pending_outbox_event() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let _ = CompanionCapture::new(&store).capture(&context("turn-outbox"));
            assert_eq!(pending_outbox_count(&store), 1);
        }

        #[test]
        fn primary_write_failure_persists_local_write_failed() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let receipt = CompanionCapture::new(&store)
                .capture_forcing_primary_write_failure(&context("turn-fallback"));
            assert_eq!(receipt.outcome, CaptureOutcome::LocalWriteFailed);
            assert!(receipt.is_durable());
            let read = CompanionCapture::new(&store)
                .read_receipt("turn-fallback")
                .expect("fallback row");
            assert_eq!(read.outcome, CaptureOutcome::LocalWriteFailed);
        }

        #[test]
        fn total_write_failure_warns_and_returns_ephemeral_receipt() {
            let _writer_guard = zeroclaw_log::__private_test_writer_lock();
            let _hook_guard = zeroclaw_log::__private_test_hook_lock();
            zeroclaw_log::try_install_capture_subscriber();
            let mut rx = zeroclaw_log::subscribe_or_install();
            while rx.try_recv().is_ok() {}

            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let receipt = CompanionCapture::new(&store)
                .capture_forcing_all_writes_to_fail(&context("turn-ephemeral"));
            assert_eq!(receipt.outcome, CaptureOutcome::LocalWriteFailed);
            assert!(!receipt.is_durable());
            assert!(
                CompanionCapture::new(&store)
                    .read_receipt("turn-ephemeral")
                    .is_none()
            );

            let mut found = false;
            loop {
                match rx.try_recv() {
                    Ok(value) => {
                        if value.get("severity_text").and_then(|v| v.as_str()) != Some("WARN") {
                            continue;
                        }
                        let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        if message.contains("degrading without a durable row") {
                            found = true;
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(
                        tokio::sync::broadcast::error::TryRecvError::Empty
                        | tokio::sync::broadcast::error::TryRecvError::Closed,
                    ) => break,
                }
            }
            zeroclaw_log::clear_broadcast_hook();
            assert!(found, "total write failure must WARN");
        }

        #[test]
        fn second_capture_of_the_same_turn_is_idempotent() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let capture = CompanionCapture::new(&store);
            let first = capture.capture(&context("turn-idem"));
            let second = capture.capture(&context("turn-idem"));
            assert_eq!(first.outcome, second.outcome);
            assert_eq!(first.event_id, second.event_id);
            assert_eq!(pending_outbox_count(&store), 1);
        }
    }
}
