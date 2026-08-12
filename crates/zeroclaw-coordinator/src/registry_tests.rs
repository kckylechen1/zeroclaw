// Tests use bare `tokio::spawn` only if a future test needs an actor —
// this file currently stays synchronous. The allow matches the crate's
// other test modules so a later async case does not trip the lint.
#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]

use super::*;
use crate::driver::{
    AgentDriver, AgentRunHandle, AgentRunRequest, AgentRunSnapshot, DriverError, HarnessKind,
    ResumeRequest,
};

struct StubDriver {
    id: String,
    kind: HarnessKind,
}

#[async_trait::async_trait]
impl AgentDriver for StubDriver {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> HarnessKind {
        self.kind
    }

    async fn spawn(&self, _request: AgentRunRequest) -> Result<AgentRunHandle, DriverError> {
        Err(DriverError::Unsupported("spawn".into()))
    }

    async fn inspect(&self, _handle: &AgentRunHandle) -> Result<AgentRunSnapshot, DriverError> {
        Err(DriverError::Unsupported("inspect".into()))
    }

    async fn cancel(&self, _handle: &AgentRunHandle) -> Result<(), DriverError> {
        Err(DriverError::Unsupported("cancel".into()))
    }
}

fn stub(id: &str, kind: HarnessKind) -> Box<dyn AgentDriver> {
    Box::new(StubDriver {
        id: id.to_owned(),
        kind,
    })
}

#[test]
fn empty_registry_has_no_drivers() {
    let registry = DriverRegistry::new();
    assert!(registry.is_empty());
    assert_eq!(registry.len(), 0);
    assert!(registry.get("native").is_none());
}

#[test]
fn register_then_get_returns_the_same_driver() {
    let mut registry = DriverRegistry::new();
    registry
        .register(stub("claude-code", HarnessKind::Process))
        .unwrap();
    let driver = registry.get("claude-code").expect("just registered");
    assert_eq!(driver.id(), "claude-code");
    assert_eq!(driver.kind(), HarnessKind::Process);
}

#[test]
fn get_by_harness_id_matches_string_lookup() {
    let mut registry = DriverRegistry::new();
    registry
        .register(stub("codex", HarnessKind::Process))
        .unwrap();
    let id = HarnessId::from("codex");
    assert!(registry.get_id(&id).is_some());
    assert_eq!(registry.get("codex").map(AgentDriver::id), Some("codex"));
}

#[test]
fn duplicate_register_is_an_error_and_keeps_the_original() {
    let mut registry = DriverRegistry::new();
    registry
        .register(stub("native", HarnessKind::Native))
        .unwrap();
    let err = registry
        .register(stub("native", HarnessKind::Process))
        .expect_err("duplicate id must not replace");
    assert_eq!(err.id().as_str(), "native");
    assert!(err.to_string().contains("native"));
    // Original driver is still the Native one, not the Process replacement.
    assert_eq!(
        registry.get("native").map(AgentDriver::kind),
        Some(HarnessKind::Native)
    );
    assert_eq!(registry.len(), 1);
}

#[test]
fn two_process_drivers_register_under_distinct_ids() {
    let mut registry = DriverRegistry::new();
    registry
        .register(stub("claude-code", HarnessKind::Process))
        .unwrap();
    registry
        .register(stub("codex", HarnessKind::Process))
        .unwrap();
    assert_eq!(registry.len(), 2);
    assert_eq!(
        registry.get("claude-code").map(AgentDriver::kind),
        Some(HarnessKind::Process)
    );
    assert_eq!(
        registry.get("codex").map(AgentDriver::kind),
        Some(HarnessKind::Process)
    );
}

#[tokio::test]
async fn resume_on_unregistered_path_is_not_confused_with_missing_driver() {
    let mut registry = DriverRegistry::new();
    registry
        .register(stub("sdk-harness", HarnessKind::Sdk))
        .unwrap();
    let driver = registry.get("sdk-harness").unwrap();
    let err = driver
        .resume(ResumeRequest {
            run_id: "run-1".into(),
            prompt: "again".into(),
        })
        .await
        .expect_err("stub leaves resume unimplemented");
    assert!(matches!(err, DriverError::Unsupported(_)));
    assert!(registry.get("missing").is_none());
}
