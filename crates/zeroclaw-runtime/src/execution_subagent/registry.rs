#![forbid(unsafe_code)]

//! Thread-safe discovery of host-registered session controllers.
//!
//! The registry owns registration metadata, not session lifecycle or admission.
//! Callers enforce concurrency limits and use `GatedSessionController` for
//! capability-gated operations. Removing a registration does not stop sessions
//! or revoke controller handles already returned to callers.

use std::{
    collections::{HashMap, hash_map::Entry},
    convert::Infallible,
    fmt,
    str::FromStr,
    sync::Arc,
};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use super::controller::{SessionCapabilities, SessionController};

/// An opaque, case-sensitive harness identifier.
///
/// Names are preserved verbatim; no normalization or fixed vocabulary applies.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HarnessId(String);

impl HarnessId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for HarnessId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for HarnessId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<HarnessId> for String {
    fn from(value: HarnessId) -> Self {
        value.0
    }
}

impl AsRef<str> for HarnessId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for HarnessId {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(value))
    }
}

impl fmt::Display for HarnessId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Host-declared discovery and admission metadata.
///
/// These capabilities do not override the capabilities on a session handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessCapabilities {
    /// Domain names match exactly and are case-sensitive.
    pub domains: Vec<String>,
    /// Admission ceiling enforced by the session owner, not this registry.
    /// Zero means no new sessions should be admitted.
    pub max_concurrent_sessions: usize,
    pub supported_session_capabilities: SessionCapabilities,
}

/// One host registration.
///
/// Cloning copies the metadata and shares the controller. Mutating a returned
/// clone does not change the registration stored in the registry.
#[derive(Clone)]
pub struct HarnessEntry {
    pub id: HarnessId,
    pub controller: Arc<dyn SessionController>,
    pub capabilities: HarnessCapabilities,
    /// Controls discovery through `get` and `find_by_domain`.
    pub enabled: bool,
}

impl fmt::Debug for HarnessEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HarnessEntry")
            .field("id", &self.id)
            .field("capabilities", &self.capabilities)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    AlreadyRegistered(HarnessId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRegistered(id) => {
                write!(formatter, "harness {id:?} is already registered")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

/// An in-memory registry with atomic registration and removal.
///
/// Share this registry using `Arc<HarnessRegistry>`. No controller method is
/// invoked while holding a registry lock. Returned entries are owned snapshots;
/// returned controllers remain valid independently of their registrations.
#[derive(Default)]
pub struct HarnessRegistry {
    entries: RwLock<HashMap<HarnessId, HarnessEntry>>,
}

impl HarnessRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers an entry without replacing an existing registration.
    ///
    /// Disabled registrations also reserve their identifiers.
    ///
    /// # Errors
    ///
    /// Returns `RegistryError::AlreadyRegistered` if the identifier exists.
    pub fn register(&self, entry: HarnessEntry) -> Result<(), RegistryError> {
        let mut entries = self.entries.write();

        match entries.entry(entry.id.clone()) {
            Entry::Vacant(slot) => {
                slot.insert(entry);
                Ok(())
            }
            Entry::Occupied(slot) => {
                let id = slot.key().clone();
                // A rejected controller may have a custom destructor. Release
                // the registry lock before dropping the rejected entry.
                drop(entries);
                Err(RegistryError::AlreadyRegistered(id))
            }
        }
    }

    /// Removes either an enabled or disabled registration.
    ///
    /// This does not stop sessions or invalidate previously cloned controllers.
    #[must_use]
    pub fn unregister(&self, id: &HarnessId) -> Option<HarnessEntry> {
        self.entries.write().remove(id)
    }

    /// Returns a controller only when its registration is enabled.
    #[must_use]
    pub fn get(&self, id: &HarnessId) -> Option<Arc<dyn SessionController>> {
        self.entries
            .read()
            .get(id)
            .filter(|entry| entry.enabled)
            .map(|entry| Arc::clone(&entry.controller))
    }

    /// Returns a management snapshot, including disabled registrations.
    ///
    /// The snapshot exposes the controller for lifecycle management; this
    /// method is not an admission or authorization boundary.
    #[must_use]
    pub fn get_entry(&self, id: &HarnessId) -> Option<HarnessEntry> {
        self.entries.read().get(id).cloned()
    }

    /// Lists all registered identifiers, including disabled registrations.
    ///
    /// Results are sorted lexicographically for deterministic presentation.
    #[must_use]
    pub fn list(&self) -> Vec<HarnessId> {
        let mut ids: Vec<_> = self.entries.read().keys().cloned().collect();
        ids.sort_unstable();
        ids
    }

    /// Finds enabled registrations advertising the exact domain name.
    ///
    /// Results are sorted and contain each identifier at most once. Discovery
    /// does not imply available session capacity.
    #[must_use]
    pub fn find_by_domain(&self, domain: &str) -> Vec<HarnessId> {
        let mut ids: Vec<_> = self
            .entries
            .read()
            .values()
            .filter(|entry| {
                entry.enabled
                    && entry
                        .capabilities
                        .domains
                        .iter()
                        .any(|candidate| candidate == domain)
            })
            .map(|entry| entry.id.clone())
            .collect();

        ids.sort_unstable();
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    use async_trait::async_trait;
    use zeroclaw_api::session_exec::{AdapterConnectionRef, RemoteSessionRef};

    use super::super::controller::{
        ControllerError, PromptReceipt, SessionCollectView, SessionEventPage, SessionHandle,
        SessionStartSpec, SessionStopReceipt,
    };

    struct UnavailableController;

    #[async_trait]
    impl SessionController for UnavailableController {
        async fn start(&self, _spec: &SessionStartSpec) -> Result<SessionHandle, ControllerError> {
            Err(ControllerError::Unavailable)
        }

        async fn watch(
            &self,
            _handle: &SessionHandle,
            _after_seq: u64,
            _limit: usize,
        ) -> Result<SessionEventPage, ControllerError> {
            Err(ControllerError::Unavailable)
        }

        async fn prompt(
            &self,
            _handle: &SessionHandle,
            _text: &str,
        ) -> Result<PromptReceipt, ControllerError> {
            Err(ControllerError::Unavailable)
        }

        async fn interrupt(&self, _handle: &SessionHandle) -> Result<(), ControllerError> {
            Err(ControllerError::Unavailable)
        }

        async fn stop(
            &self,
            _handle: &SessionHandle,
            _graceful: bool,
        ) -> Result<SessionStopReceipt, ControllerError> {
            Err(ControllerError::Unavailable)
        }

        async fn collect(
            &self,
            _handle: &SessionHandle,
        ) -> Result<SessionCollectView, ControllerError> {
            Err(ControllerError::Unavailable)
        }

        async fn reattach(
            &self,
            _adapter_connection: &AdapterConnectionRef,
            _remote_session: &RemoteSessionRef,
            _resume_from_revision: u64,
        ) -> Result<SessionHandle, ControllerError> {
            Err(ControllerError::Unavailable)
        }
    }

    fn entry(id: &str, domains: &[&str], enabled: bool) -> HarnessEntry {
        HarnessEntry {
            id: id.into(),
            controller: Arc::new(UnavailableController),
            capabilities: HarnessCapabilities {
                domains: domains.iter().map(|domain| (*domain).to_owned()).collect(),
                max_concurrent_sessions: 4,
                supported_session_capabilities: SessionCapabilities {
                    observe: true,
                    events: true,
                    ..SessionCapabilities::default()
                },
            },
            enabled,
        }
    }

    #[test]
    fn types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<HarnessId>();
        assert_send_sync::<HarnessCapabilities>();
        assert_send_sync::<HarnessEntry>();
        assert_send_sync::<HarnessRegistry>();
        assert_send_sync::<RegistryError>();
    }

    #[test]
    fn identifiers_support_lossless_string_and_json_conversion() {
        for name in [
            "dsh",
            "codex",
            "claude_code",
            "custom/vendor:v2",
            "研究",
            " Mixed Case ",
            "",
        ] {
            let id = HarnessId::new(name);

            assert_eq!(id.as_str(), name);
            assert_eq!(id.as_ref(), name);
            assert_eq!(id.to_string(), name);
            assert_eq!(String::from(id.clone()), name);
            assert_eq!(HarnessId::from(name.to_owned()), id);
            assert_eq!(name.parse::<HarnessId>().unwrap(), id);

            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(json, serde_json::to_string(name).unwrap());
            assert_eq!(serde_json::from_str::<HarnessId>(&json).unwrap(), id);
        }

        assert!(serde_json::from_str::<HarnessId>("42").is_err());
        assert!(serde_json::from_str::<HarnessId>("null").is_err());
    }

    #[test]
    fn identifiers_are_case_sensitive_hash_keys() {
        let mut map = HashMap::new();
        map.insert(HarnessId::from("codex"), 1);
        map.insert(HarnessId::from("Codex"), 2);

        assert_eq!(map.len(), 2);
        assert_eq!(map.get(&HarnessId::new("codex")), Some(&1));
        assert_eq!(map.get(&HarnessId::new("Codex")), Some(&2));
    }

    #[test]
    fn new_and_default_registries_are_empty() {
        for registry in [HarnessRegistry::new(), HarnessRegistry::default()] {
            let id = HarnessId::from("missing");

            assert!(registry.list().is_empty());
            assert!(registry.find_by_domain("code_edit").is_empty());
            assert!(registry.get(&id).is_none());
            assert!(registry.get_entry(&id).is_none());
            assert!(registry.unregister(&id).is_none());
        }
    }

    #[test]
    fn registration_preserves_controller_identity_and_metadata() {
        let registry = HarnessRegistry::new();
        let original = entry("codex", &["code_edit", "deep_analysis"], true);
        let id = original.id.clone();
        let controller = Arc::clone(&original.controller);
        let capabilities = original.capabilities.clone();

        registry.register(original).unwrap();

        assert!(Arc::ptr_eq(&registry.get(&id).unwrap(), &controller));

        let snapshot = registry.get_entry(&id).unwrap();
        assert_eq!(snapshot.id, id);
        assert_eq!(snapshot.capabilities, capabilities);
        assert!(snapshot.enabled);
        assert!(Arc::ptr_eq(&snapshot.controller, &controller));
    }

    #[test]
    fn duplicate_registration_does_not_replace_existing_entry() {
        let registry = HarnessRegistry::new();
        let original = entry("codex", &["code_edit"], true);
        let controller = Arc::clone(&original.controller);
        let id = original.id.clone();

        registry.register(original).unwrap();

        assert_eq!(
            registry.register(entry("codex", &["web_research"], false)),
            Err(RegistryError::AlreadyRegistered(id.clone()))
        );

        let stored = registry.get_entry(&id).unwrap();
        assert!(stored.enabled);
        assert_eq!(stored.capabilities.domains, vec!["code_edit".to_owned()]);
        assert!(Arc::ptr_eq(&stored.controller, &controller));
        assert_eq!(registry.list(), vec![id]);
    }

    #[test]
    fn disabled_entries_are_visible_to_management_but_not_discovery() {
        let registry = HarnessRegistry::new();
        let id = HarnessId::from("disabled");

        registry
            .register(entry("disabled", &["code_edit"], false))
            .unwrap();

        assert!(registry.get(&id).is_none());
        assert!(registry.find_by_domain("code_edit").is_empty());
        assert!(!registry.get_entry(&id).unwrap().enabled);
        assert_eq!(registry.list(), vec![id.clone()]);
        assert_eq!(
            registry.register(entry("disabled", &[], true)),
            Err(RegistryError::AlreadyRegistered(id.clone()))
        );
        assert!(!registry.unregister(&id).unwrap().enabled);
    }

    #[test]
    fn discovery_is_sorted_exact_and_deduplicated() {
        let registry = HarnessRegistry::new();

        for value in [
            entry("zeta", &["code_edit", "code_edit"], true),
            entry("alpha", &["code_edit", "web_research"], true),
            entry("beta", &["Code_Edit"], true),
            entry("disabled", &["code_edit"], false),
            entry("empty", &[], true),
        ] {
            registry.register(value).unwrap();
        }

        assert_eq!(
            registry.list(),
            ["alpha", "beta", "disabled", "empty", "zeta"]
                .map(HarnessId::from)
                .to_vec()
        );
        assert_eq!(
            registry.find_by_domain("code_edit"),
            vec![HarnessId::from("alpha"), HarnessId::from("zeta")]
        );
        assert_eq!(
            registry.find_by_domain("Code_Edit"),
            vec![HarnessId::from("beta")]
        );
        assert_eq!(
            registry.find_by_domain("web_research"),
            vec![HarnessId::from("alpha")]
        );
        assert!(registry.find_by_domain("code").is_empty());
        assert!(registry.find_by_domain(" code_edit").is_empty());
        assert!(registry.find_by_domain("").is_empty());
    }

    #[test]
    fn modifying_a_snapshot_does_not_mutate_the_registry() {
        let registry = HarnessRegistry::new();
        let id = HarnessId::from("codex");

        registry
            .register(entry("codex", &["code_edit"], true))
            .unwrap();

        let mut snapshot = registry.get_entry(&id).unwrap();
        snapshot.id = HarnessId::from("changed");
        snapshot.enabled = false;
        snapshot.capabilities.domains.clear();
        snapshot.capabilities.max_concurrent_sessions = 0;
        snapshot.capabilities.supported_session_capabilities = SessionCapabilities::default();

        let stored = registry.get_entry(&id).unwrap();
        assert_eq!(stored.id, id);
        assert!(stored.enabled);
        assert_eq!(stored.capabilities.domains, vec!["code_edit".to_owned()]);
        assert_eq!(stored.capabilities.max_concurrent_sessions, 4);
        assert!(stored.capabilities.supported_session_capabilities.observe);
        assert!(registry.get(&HarnessId::from("changed")).is_none());
    }

    #[test]
    fn removal_preserves_outstanding_handles_and_allows_reregistration() {
        let registry = HarnessRegistry::new();
        let id = HarnessId::from("codex");

        registry
            .register(entry("codex", &["code_edit"], true))
            .unwrap();
        let outstanding = registry.get(&id).unwrap();
        let removed = registry.unregister(&id).unwrap();

        assert!(Arc::ptr_eq(&outstanding, &removed.controller));
        assert!(registry.get(&id).is_none());
        assert!(registry.get_entry(&id).is_none());
        assert!(registry.unregister(&id).is_none());
        assert!(registry.list().is_empty());
        assert!(registry.find_by_domain("code_edit").is_empty());

        registry
            .register(entry("codex", &["web_research"], true))
            .unwrap();

        let replacement = registry.get(&id).unwrap();
        assert!(!Arc::ptr_eq(&outstanding, &replacement));
        drop(registry);
        assert!(Arc::ptr_eq(&outstanding, &removed.controller));
    }

    #[test]
    fn zero_capacity_is_metadata_not_a_discovery_filter() {
        let registry = HarnessRegistry::new();
        let mut value = entry("paused", &["code_edit"], true);
        value.capabilities.max_concurrent_sessions = 0;
        registry.register(value).unwrap();

        let id = HarnessId::from("paused");
        assert!(registry.get(&id).is_some());
        assert_eq!(registry.find_by_domain("code_edit"), vec![id]);
    }

    #[test]
    fn concurrent_duplicate_registration_has_exactly_one_winner() {
        const WORKERS: usize = 16;

        let registry = Arc::new(HarnessRegistry::new());
        let barrier = Arc::new(Barrier::new(WORKERS));

        let workers: Vec<_> = (0..WORKERS)
            .map(|_| {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let candidate = entry("shared", &["code_edit"], true);
                    let controller = Arc::clone(&candidate.controller);
                    barrier.wait();
                    (registry.register(candidate), controller)
                })
            })
            .collect();

        let mut winners = Vec::new();
        for worker in workers {
            let (result, controller) = worker.join().unwrap();
            match result {
                Ok(()) => winners.push(controller),
                Err(error) => assert_eq!(
                    error,
                    RegistryError::AlreadyRegistered(HarnessId::from("shared"))
                ),
            }
        }

        assert_eq!(winners.len(), 1);
        assert_eq!(registry.list(), vec![HarnessId::from("shared")]);
        assert!(Arc::ptr_eq(
            &registry.get(&HarnessId::from("shared")).unwrap(),
            &winners[0]
        ));
    }

    #[test]
    fn concurrent_distinct_registrations_and_removals_preserve_entries() {
        const WORKERS: usize = 16;

        let registry = Arc::new(HarnessRegistry::new());
        let barrier = Arc::new(Barrier::new(WORKERS));

        let workers: Vec<_> = (0..WORKERS)
            .map(|index| {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let name = format!("harness-{index}");
                    let id = HarnessId::from(name.as_str());

                    registry
                        .register(entry(&name, &["code_edit"], true))
                        .unwrap();

                    barrier.wait();
                    assert!(registry.get(&id).is_some());
                    assert!(registry.get_entry(&id).is_some());
                    assert!(registry.list().contains(&id));
                    assert!(registry.find_by_domain("code_edit").contains(&id));

                    let removed = registry.unregister(&id).unwrap();
                    assert_eq!(removed.id, id);
                    assert!(registry.get(&id).is_none());
                })
            })
            .collect();

        for worker in workers {
            worker.join().unwrap();
        }

        assert!(registry.list().is_empty());
        assert!(registry.find_by_domain("code_edit").is_empty());
    }

    #[test]
    fn registry_error_implements_standard_error() {
        let error = RegistryError::AlreadyRegistered(HarnessId::from("codex"));
        let error: &dyn std::error::Error = &error;

        assert!(error.to_string().contains("codex"));
        assert!(error.to_string().contains("already registered"));
        assert!(error.source().is_none());
    }
}
