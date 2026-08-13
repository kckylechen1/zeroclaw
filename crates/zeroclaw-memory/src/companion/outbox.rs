//! Local outbox health and a read-only observe skeleton.
//!
//! Memcore's drain APIs (`claim_outbox_events` / `apply_outbox_outcome`) move
//! `pending` to `in_flight` and only then accept an ack. V1 has no Tachi
//! consumer, so this module never claims, never acknowledges, and never
//! compact-deletes terminal rows (memcore has no public compact seam). Health
//! is a SELECT; aging is a WARN.

use zeroclaw_api::companion::CompanionOutboxHealth;
use zeroclaw_config::schema::Config;

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

/// Status-command probe. Never uses the production factory.
///
/// Missing path → `not_configured` (no mkdir, no `create_fresh`). Existing
/// path → `open_existing_deny` only. Opening is not a strictly read-only
/// inspect: SQLite may take a WAL lock, and the probe still applies
/// owner-only `0600` to the database file and any existing `-wal`/`-shm`
/// sidecars.
#[must_use]
pub fn probe_companion_outbox_health(config: &Config) -> CompanionOutboxHealth {
    if !config.companion_memory.enable {
        return CompanionOutboxHealth::not_configured();
    }

    #[cfg(not(feature = "tachi"))]
    {
        CompanionOutboxHealth::not_configured()
    }

    #[cfg(feature = "tachi")]
    {
        let path = config.companion_memory.db_path(&config.data_dir);
        if !path.exists() {
            return CompanionOutboxHealth::not_configured();
        }
        match CompanionStore::open_for_status_probe(&path) {
            Ok(store) => store.outbox_health(),
            Err(err) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "error": err.to_string(),
                        })),
                    "companion outbox health could not be read for status"
                );
                CompanionOutboxHealth::pending(0, None)
            }
        }
    }
}

impl CompanionStore {
    /// Read-only snapshot of local outbox debt.
    ///
    /// An open store is always [`zeroclaw_api::companion::CompanionOutboxStatus::Pending`],
    /// including when `pending_count` is zero. This method does not claim or
    /// acknowledge events, and it does not emit the stale-pending WARN.
    #[must_use]
    pub fn outbox_health(&self) -> CompanionOutboxHealth {
        health_of(self)
    }

    /// Snapshot plus an aging WARN when the oldest pending event is stale.
    ///
    /// Still read-only: a consumer that dies holding `in_flight` is recovered
    /// by memcore reclaim, not by this host forging an ack. Reserved for the
    /// 5-minute daemon tick; `/api/health` must call [`Self::outbox_health`].
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
        Ok(raw) => CompanionOutboxHealth::pending(
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
                "companion outbox health query failed; reporting pending with no counts"
            );
            CompanionOutboxHealth::pending(0, None)
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

    #[test]
    fn probe_missing_path_is_not_configured_and_creates_nothing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Config::default()
        };
        config.companion_memory.enable = true;
        let expected = config.companion_memory.db_path(tmp.path());
        let health = probe_companion_outbox_health(&config);
        assert_eq!(health.status, CompanionOutboxStatus::NotConfigured);
        assert_eq!(health.pending_count, 0);
        assert!(!expected.exists(), "status probe must not create_fresh");
        assert!(
            !expected.parent().expect("dir").exists(),
            "status probe must not mkdir the companion store dir"
        );
    }

    #[test]
    fn probe_disabled_config_is_not_configured() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Config::default()
        };
        let health = probe_companion_outbox_health(&config);
        assert_eq!(health.status, CompanionOutboxStatus::NotConfigured);
        assert!(!config.companion_memory.db_path(tmp.path()).exists());
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
        fn open_empty_store_is_pending_not_synchronized() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let health = store.outbox_health();
            assert_eq!(health.status, CompanionOutboxStatus::Pending);
            assert_eq!(health.pending_count, 0);
            assert_eq!(health.oldest_pending_age_secs, None);
            let json = serde_json::to_string(&health).expect("ser");
            assert!(!json.contains("synchronized"), "{json}");
            assert!(!json.contains("accumulating"), "{json}");
        }

        #[test]
        fn pending_events_report_count_and_age() {
            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            let receipt = CompanionCapture::new(&store).capture(&context("turn-health-age"));
            assert!(receipt.event_id.is_some());

            let health = store.outbox_health();
            assert_eq!(health.status, CompanionOutboxStatus::Pending);
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
            let found = drain_aging_warn(&mut rx);
            zeroclaw_log::clear_broadcast_hook();
            assert!(found, "stale pending debt must WARN");
        }

        fn drain_aging_warn(rx: &mut tokio::sync::broadcast::Receiver<serde_json::Value>) -> bool {
            loop {
                match rx.try_recv() {
                    Ok(value) => {
                        if value.get("severity_text").and_then(|v| v.as_str()) != Some("WARN") {
                            continue;
                        }
                        let message = value.get("message").and_then(|v| v.as_str()).unwrap_or("");
                        if message.contains("aging with no drain configured") {
                            return true;
                        }
                    }
                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(
                        tokio::sync::broadcast::error::TryRecvError::Empty
                        | tokio::sync::broadcast::error::TryRecvError::Closed,
                    ) => return false,
                }
            }
        }

        fn backdate_pending(store: &crate::companion::CompanionStore, turn_id: &str) {
            let receipt = CompanionCapture::new(store).capture(&context(turn_id));
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
        }

        #[test]
        fn repeated_outbox_health_reads_do_not_warn_on_stale_pending() {
            let _writer_guard = zeroclaw_log::__private_test_writer_lock();
            let _hook_guard = zeroclaw_log::__private_test_hook_lock();
            zeroclaw_log::try_install_capture_subscriber();
            let mut rx = zeroclaw_log::subscribe_or_install();
            while rx.try_recv().is_ok() {}

            let tmp = TempDir::new().unwrap();
            let store = open_store(&tmp);
            backdate_pending(&store, "turn-health-no-warn");

            for _ in 0..3 {
                let health = store.outbox_health();
                assert_eq!(health.status, CompanionOutboxStatus::Pending);
                assert!(
                    health.oldest_pending_age_secs.expect("age") >= OUTBOX_PENDING_AGE_WARN_SECS
                );
            }
            assert!(
                !drain_aging_warn(&mut rx),
                "consecutive outbox_health reads must not emit stale WARN"
            );

            let observed = store.observe_local_outbox();
            assert_eq!(observed.status, CompanionOutboxStatus::Pending);
            let warned = drain_aging_warn(&mut rx);
            zeroclaw_log::clear_broadcast_hook();
            assert!(warned, "observe_local_outbox still owns the stale WARN");
        }

        #[test]
        fn probe_existing_store_reports_pending_counts() {
            let tmp = TempDir::new().unwrap();
            let config = enabled_config(tmp.path());
            let store = open_store(&tmp);
            let receipt = CompanionCapture::new(&store).capture(&context("turn-probe-existing"));
            assert!(receipt.event_id.is_some());
            drop(store);

            let health = probe_companion_outbox_health(&config);
            assert_eq!(health.status, CompanionOutboxStatus::Pending);
            assert_eq!(health.pending_count, 1);
            assert!(health.oldest_pending_age_secs.is_some());
        }

        #[test]
        fn probe_missing_path_never_calls_create_fresh() {
            let tmp = TempDir::new().unwrap();
            let config = enabled_config(tmp.path());
            let path = config.companion_memory.db_path(tmp.path());
            assert!(!path.exists());
            let health = probe_companion_outbox_health(&config);
            assert_eq!(health.status, CompanionOutboxStatus::NotConfigured);
            assert!(!path.exists());
            assert!(
                CompanionStore::open_for_status_probe(&path).is_err(),
                "probe open must refuse a missing file rather than create_fresh"
            );
            assert!(!path.exists());
            assert!(!path.parent().expect("dir").exists());
        }
    }
}
