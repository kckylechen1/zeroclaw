//! Companion-memory PortableKernel store.
//!
//! Sibling of the generic [`crate::Memory`] backend. `TachiMemory` must never
//! open this file. Runtime opens always deny schema migration; upgrades go
//! through `zeroclaw companion migrate` (CLI body is a later slice).

use std::sync::Arc;

use zeroclaw_config::schema::Config;

#[cfg(feature = "tachi")]
mod store;
#[cfg(feature = "tachi")]
pub use store::CompanionStore;

#[cfg(not(feature = "tachi"))]
mod stub;
#[cfg(not(feature = "tachi"))]
pub use stub::CompanionStore;

mod capture;
pub use capture::{
    CompanionCapture, capture_channel_turn, capture_gateway_turn, capture_turn_if_present,
};

/// Construct the companion store from config.
///
/// Returns `Ok(None)` when `[companion_memory].enable` is false, or when this
/// build was compiled without the `tachi` feature (no memcore / no AGPL).
///
/// # Errors
/// Returns when enable is true, memcore is available, and the PortableKernel
/// file cannot be opened under the frozen deny-migration runtime posture.
pub fn create_companion_store(config: &Config) -> anyhow::Result<Option<Arc<CompanionStore>>> {
    if !config.companion_memory.enable {
        return Ok(None);
    }

    #[cfg(not(feature = "tachi"))]
    {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "companion_memory.enable is true but this build was compiled without \
             `memory-tachi`; companion store stays closed. Rebuild with \
             `--features memory-tachi` to open it."
        );
        Ok(None)
    }

    #[cfg(feature = "tachi")]
    {
        let path = config.companion_memory.db_path(&config.data_dir);
        let store = CompanionStore::open_runtime(&path)?;
        Ok(Some(Arc::new(store)))
    }
}

/// Drop the previous handle (closing rusqlite) and open again.
///
/// Reload never calls `create_fresh`: if the file exists, runtime open uses
/// `open_existing_deny`. Callers that still hold other `Arc` clones keep the
/// old connection alive until those clones drop.
///
/// # Errors
/// Same as [`create_companion_store`].
pub fn reload_companion_store(
    previous: Option<Arc<CompanionStore>>,
    config: &Config,
) -> anyhow::Result<Option<Arc<CompanionStore>>> {
    drop(previous);
    create_companion_store(config)
}

/// Clone one factory result for the two daemon consumers (gateway + channels).
///
/// Both handles are the same `Arc`, or both `None`. Callers must not open a
/// second store for the other consumer.
#[must_use]
pub fn clone_for_subsystems(
    store: &Option<Arc<CompanionStore>>,
) -> (Option<Arc<CompanionStore>>, Option<Arc<CompanionStore>>) {
    (store.clone(), store.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use zeroclaw_config::schema::Config;

    fn cfg_with(data_dir: &std::path::Path, enable: bool) -> Config {
        let mut config = Config {
            data_dir: data_dir.to_path_buf(),
            ..Config::default()
        };
        config.companion_memory.enable = enable;
        config
    }

    #[test]
    fn enable_false_returns_none_and_creates_no_files() {
        let tmp = TempDir::new().unwrap();
        let config = cfg_with(tmp.path(), false);
        let expected = config.companion_memory.db_path(tmp.path());
        let store = create_companion_store(&config).expect("disabled factory");
        assert!(store.is_none());
        assert!(!expected.exists());
        assert!(!expected.parent().expect("dir").exists());
    }

    #[test]
    fn reload_of_disabled_store_stays_none() {
        let tmp = TempDir::new().unwrap();
        let config = cfg_with(tmp.path(), false);
        let reloaded = reload_companion_store(None, &config).expect("reload disabled");
        assert!(reloaded.is_none());
        assert!(!config.companion_memory.db_path(tmp.path()).exists());
    }

    #[cfg(not(feature = "tachi"))]
    #[test]
    fn enable_true_without_tachi_feature_returns_none() {
        let tmp = TempDir::new().unwrap();
        let config = cfg_with(tmp.path(), true);
        let store = create_companion_store(&config).expect("feature-off factory");
        assert!(store.is_none());
        assert!(!config.companion_memory.db_path(tmp.path()).exists());
    }

    #[test]
    fn clone_for_subsystems_keeps_both_consumers_on_the_same_arc() {
        let tmp = TempDir::new().unwrap();
        let config = cfg_with(tmp.path(), true);
        let store = create_companion_store(&config).expect("factory once");
        let (gateway, channels) = clone_for_subsystems(&store);
        match (store.as_ref(), gateway.as_ref(), channels.as_ref()) {
            (None, None, None) => {}
            (Some(owner), Some(gw), Some(ch)) => {
                assert!(
                    Arc::ptr_eq(owner, gw) && Arc::ptr_eq(gw, ch),
                    "daemon consumers must share the factory Arc, not reopen"
                );
            }
            _ => panic!("clone_for_subsystems split None/Some across consumers"),
        }
    }

    #[cfg(feature = "tachi")]
    #[test]
    fn enabled_daemon_composition_injects_one_arc_without_second_create() {
        let tmp = TempDir::new().unwrap();
        let config = cfg_with(tmp.path(), true);
        let store = create_companion_store(&config)
            .expect("factory once")
            .expect("enabled");
        let (gateway, channels) = clone_for_subsystems(&Some(store.clone()));
        let gateway = gateway.expect("gateway clone");
        let channels = channels.expect("channels clone");
        assert!(Arc::ptr_eq(&store, &gateway));
        assert!(Arc::ptr_eq(&gateway, &channels));
        assert_eq!(Arc::strong_count(&store), 3);
    }
}
