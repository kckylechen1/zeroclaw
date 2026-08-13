//! Local outbox health and a read-only observe skeleton.
//!
//! Memcore's drain APIs (`claim_outbox_events` / `apply_outbox_outcome`) move
//! `pending` to `in_flight` and only then accept an ack. V1 has no Tachi
//! consumer, so this module never claims, never acknowledges, and never
//! compact-deletes terminal rows (memcore has no public compact seam). Health
//! is a SELECT; aging is a WARN.

use zeroclaw_api::companion::CompanionOutboxHealth;

use super::CompanionStore;

/// Pending events older than this emit a WARN. V1 has no drain, so this is
/// aging local debt, not a failed remote sync.
pub const OUTBOX_PENDING_AGE_WARN_SECS: u64 = 3600;

/// How often the daemon inspects outbox debt. Read-only; never claims.
pub const OUTBOX_OBSERVE_INTERVAL_SECS: u64 = 300;

/// Health for an optional store handle. `None` is [`CompanionOutboxHealth::not_configured`].
#[must_use]
pub fn companion_outbox_health(store: Option<&CompanionStore>) -> CompanionOutboxHealth {
    match store {
        None => CompanionOutboxHealth::not_configured(),
        Some(store) => store.outbox_health(),
    }
}

impl CompanionStore {
    /// Read-only snapshot of local outbox debt.
    ///
    /// An open store is always [`zeroclaw_api::companion::CompanionOutboxStatus::Accumulating`],
    /// including when `pending_count` is zero. This method does not claim or
    /// acknowledge events.
    #[must_use]
    pub fn outbox_health(&self) -> CompanionOutboxHealth {
        health_of(self)
    }

    /// Snapshot plus an aging WARN when the oldest pending event is stale.
    ///
    /// Still read-only: a consumer that dies holding `in_flight` is recovered
    /// by memcore reclaim, not by this host forging an ack.
    #[must_use]
    pub fn observe_local_outbox(&self) -> CompanionOutboxHealth {
        let health = self.outbox_health();
        warn_if_stale_pending(&health);
        health
    }
}

fn warn_if_stale_pending(health: &CompanionOutboxHealth) {
    let Some(age_secs) = health.oldest_pending_age_secs else {
        return;
    };
    if age_secs < OUTBOX_PENDING_AGE_WARN_SECS {
        return;
    }
    ::zeroclaw_log::record!(
        WARN,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
            .with_attrs(::serde_json::json!({
                "pending_count": health.pending_count,
                "oldest_pending_age_secs": age_secs,
                "warn_after_secs": OUTBOX_PENDING_AGE_WARN_SECS,
            })),
        "companion outbox pending events are aging with no drain configured"
    );
}

#[cfg(feature = "tachi")]
fn health_of(store: &CompanionStore) -> CompanionOutboxHealth {
    match store.with_store(|mem| mem.outbox_health()) {
        Ok(raw) => CompanionOutboxHealth::accumulating(
            raw.pending_count,
            raw.oldest_pending_at
                .as_deref()
                .and_then(age_secs_from_rfc3339),
        ),
        Err(err) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "error": err.to_string(),
                    })),
                "companion outbox health query failed; reporting accumulating with no counts"
            );
            CompanionOutboxHealth::accumulating(0, None)
        }
    }
}

#[cfg(not(feature = "tachi"))]
fn health_of(_store: &CompanionStore) -> CompanionOutboxHealth {
    CompanionOutboxHealth::not_configured()
}

#[cfg(feature = "tachi")]
fn age_secs_from_rfc3339(stamp: &str) -> Option<u64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(stamp).ok()?;
    let age = chrono::Utc::now().signed_duration_since(parsed.with_timezone(&chrono::Utc));
    Some(u64::try_from(age.num_seconds().max(0)).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_api::companion::CompanionOutboxStatus;

    #[test]
    fn no_store_is_not_configured() {
        let health = companion_outbox_health(None);
        assert_eq!(health.status, CompanionOutboxStatus::NotConfigured);
        assert_eq!(health.pending_count, 0);
        assert_eq!(health.oldest_pending_age_secs, None);
    }

    #[cfg(feature = "tachi")]
    mod with_store {
        use super::*;
        use crate::companion::{CompanionCapture, create_companion_store};
        use memcore::OutboxState;
        use tempfile::TempDir;
        use zeroclaw_api::companion::{
            AgentIdentityId, CaptureContext, CompanionOwnerGate, IngressIdentity,
        };
        use zeroclaw_api::principal::PrincipalId;
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

        fn pending_ids(store: &crate::companion::CompanionStore) -> Vec<String> {
            store
                .with_store(|mem| {
                    mem.list_outbox_events(OutboxState::Pending, 100)
                        .expect("list")
                })
                .into_iter()
                .map(|row| row.event_id)
                .collect()
        }

        #[test]
        fn open_empty_store_is_accumulating_not_synchronized() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let health = store.outbox_health();
            assert_eq!(health.status, CompanionOutboxStatus::Accumulating);
            assert_eq!(health.pending_count, 0);
            assert_eq!(health.oldest_pending_age_secs, None);
            let json = serde_json::to_string(&health).expect("ser");
            assert!(!json.contains("synchronized"), "{json}");
        }

        #[test]
        fn pending_events_report_count_and_age() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let receipt = CompanionCapture::new(&store).capture(&context("turn-health-age"));
            assert!(receipt.event_id.is_some());

            let health = store.outbox_health();
            assert_eq!(health.status, CompanionOutboxStatus::Accumulating);
            assert_eq!(health.pending_count, 1);
            let age = health
                .oldest_pending_age_secs
                .expect("fresh pending event has an age");
            assert!(age < 10, "fresh capture should be seconds old, not {age}s");

            let event_id = receipt.event_id.expect("outbox id");
            store
                .store_handle()
                .lock()
                .connection()
                .execute(
                    "UPDATE memory_outbox_events SET created_at = ?2 WHERE event_id = ?1",
                    rusqlite::params![event_id, "2020-01-01T00:00:00.000Z"],
                )
                .expect("backdate");

            let aged = store.outbox_health();
            assert_eq!(aged.pending_count, 1);
            let aged_secs = aged.oldest_pending_age_secs.expect("aged");
            assert!(
                aged_secs >= 60 * 60 * 24 * 365,
                "backdated 2020 stamp should be years old, got {aged_secs}s"
            );
        }

        #[test]
        fn health_query_does_not_consume_events() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let _receipt = CompanionCapture::new(&store).capture(&context("turn-health-readonly"));
            let before = pending_ids(&store);
            assert_eq!(before.len(), 1);

            let health = store.outbox_health();
            assert_eq!(health.pending_count, 1);
            let observed = store.observe_local_outbox();
            assert_eq!(observed.pending_count, 1);

            let after = pending_ids(&store);
            assert_eq!(after, before, "health/observe must not claim pending rows");
            let in_flight = store.with_store(|mem| {
                mem.list_outbox_events(OutboxState::InFlight, 100)
                    .expect("list")
            });
            assert!(
                in_flight.is_empty(),
                "a health read must not move events to in_flight"
            );
        }

        #[test]
        fn observe_warns_when_pending_events_are_stale() {
            let _writer_guard = zeroclaw_log::__private_test_writer_lock();
            let _hook_guard = zeroclaw_log::__private_test_hook_lock();
            zeroclaw_log::try_install_capture_subscriber();
            let mut rx = zeroclaw_log::subscribe_or_install();
            while rx.try_recv().is_ok() {}

            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let receipt = CompanionCapture::new(&store).capture(&context("turn-health-stale"));
            let event_id = receipt.event_id.expect("outbox id");
            store
                .store_handle()
                .lock()
                .connection()
                .execute(
                    "UPDATE memory_outbox_events SET created_at = ?2 WHERE event_id = ?1",
                    rusqlite::params![event_id, "2020-01-01T00:00:00.000Z"],
                )
                .expect("backdate");

            let health = store.observe_local_outbox();
            assert!(health.oldest_pending_age_secs.expect("age") >= OUTBOX_PENDING_AGE_WARN_SECS);

            let mut found = false;
            loop {
                match rx.try_recv() {
                    Ok(value) => {
                        if value.get("severity_text").and_then(|v| v.as_str()) != Some("WARN") {
                            continue;
                        }
                        let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        if message.contains("aging with no drain configured") {
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
            assert!(found, "stale pending debt must WARN");
        }
    }
}
