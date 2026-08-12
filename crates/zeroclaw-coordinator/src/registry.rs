//! In-process [`AgentDriver`] registry.
//!
//! Keyed by [`HarnessId`], not [`HarnessKind`]: two process harnesses (claude-code
//! and codex) share a kind and must still occupy distinct slots. The coordinator
//! crate owns this map so a later PR can look up a driver without taking a
//! dependency on `zeroclaw-runtime`'s `NativeChildRunner`.
//!
//! Duplicate registration is an **error**. Replacing a live driver would drop
//! the in-flight runs that driver still owns; V1 does not offer unregister —
//! tear the registry down instead.
//!
//! This registry is not yet the live spawn path. Existing `ChannelBackend::spawn`
//! / `spawn_subagent` callers are unchanged; P3 switches them over.

use std::collections::HashMap;
use std::fmt;

use crate::backend::ChannelBackend;
use crate::driver::{AgentDriver, HarnessId};
use crate::native::NativeAgentDriver;

/// Map of harness id → driver.
#[derive(Default)]
pub struct DriverRegistry {
    drivers: HashMap<HarnessId, Box<dyn AgentDriver>>,
}

impl DriverRegistry {
    /// Empty registry. Prefer [`Self::with_native`] for a host that should
    /// already know how to run ZeroClaw-native children.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registry with [`NativeAgentDriver`] already registered under
    /// [`crate::native::NATIVE_HARNESS_ID`].
    #[must_use]
    pub fn with_native(backend: ChannelBackend) -> Self {
        let mut drivers = HashMap::new();
        drivers.insert(
            HarnessId::from(crate::native::NATIVE_HARNESS_ID),
            Box::new(NativeAgentDriver::new(backend)) as Box<dyn AgentDriver>,
        );
        Self { drivers }
    }

    /// Insert `driver` under [`AgentDriver::id`].
    ///
    /// # Errors
    ///
    /// [`RegistryError::Duplicate`] when that id is already taken. The
    /// previously registered driver is left in place.
    pub fn register(&mut self, driver: Box<dyn AgentDriver>) -> Result<(), RegistryError> {
        let id = HarnessId::from(driver.id());
        if self.drivers.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.drivers.insert(id, driver);
        Ok(())
    }

    /// Look up a driver by the string [`AgentDriver::id`] returns.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&dyn AgentDriver> {
        self.drivers.get(&HarnessId::from(id)).map(AsRef::as_ref)
    }

    /// Look up a driver by the typed registry key.
    #[must_use]
    pub fn get_id(&self, id: &HarnessId) -> Option<&dyn AgentDriver> {
        self.drivers.get(id).map(AsRef::as_ref)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.drivers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.drivers.is_empty()
    }
}

/// Why a [`DriverRegistry::register`] call was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// `id` is already occupied. The existing driver was not replaced.
    Duplicate(HarnessId),
}

impl RegistryError {
    #[must_use]
    pub fn id(&self) -> &HarnessId {
        match self {
            Self::Duplicate(id) => id,
        }
    }
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(id) => {
                write!(f, "driver already registered for harness id '{id}'")
            }
        }
    }
}

impl std::error::Error for RegistryError {}

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
