//! The Mission Control Dispatcher for external agent harnesses.
//!
//! Orchestrates execution requests across multiple registered agent harnesses:
//! - Direct child process sessions (OpenAI Codex app-server, ACP harnesses like DeepSeek Harness)
//! - Durable delegated tasks via the Tachi task bridge (`TaskIntentV1`)
//! - Local reasoning / analysis (in-process planning)

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use zeroclaw_api::session_exec::{
    ExecutionRequestV1, ExecutionRouteV1, ExecutionSessionReportV1, HostIdentityRef,
};
use zeroclaw_api::taskintent::{
    ApprovalRequirement, BoundedText, Capability, CapabilityRequest, EvaluationRequirement,
    IndependenceClass, PrivacyClass, RequestId, RequesterRef, RoutingPreference,
};
use zeroclaw_config::harness::{
    HarnessDefinition, HarnessKind, HarnessPermissionPolicy, HarnessResumeMethod,
};

use super::acpx::{AcpPermissionPolicy, AcpResumeMethod, AcpxController, AcpxControllerConfig};
use super::codex::{CodexController, CodexControllerConfig};
use super::controller::{GatedSessionController, SessionController};
use super::facts::SessionFactSink;
use super::registry::{
    HarnessCapabilities, HarnessEntry, HarnessId, HarnessRegistry, RegistryError,
};
use super::router::{DispatchError, DispatchPlan, plan_dispatch};
use super::tool::ExecutionSubagentTool;
use crate::subagent_v1::ObjectiveV1;
use crate::tachi_bridge::SubmitReceipt;
use crate::tachi_bridge::client::TachiBridgeClient;
use crate::tachi_bridge::compose::{
    RequesterBridgePolicy, StructuralIntentContext, TaskIntentInputs, compose_intent,
};

/// The outcome of an execution dispatched through Mission Control.
#[derive(Clone, Debug)]
pub enum DispatchExecutionOutcome {
    /// Pure reason / analysis — no external harness session was started.
    Reason,
    /// Ephemeral run completed through an external session controller (Codex, DSH, etc.).
    Ephemeral(Box<ExecutionSessionReportV1>),
    /// Submitted to durable Tachi bridge — returns TaskRef and SubmitReceipt.
    Durable(SubmitReceipt),
}

/// Errors originating from dispatcher configuration or execution.
#[derive(Debug, thiserror::Error)]
pub enum DispatcherError {
    #[error("harness registry error: {0}")]
    Registry(#[from] RegistryError),
    #[error("controller construction failed for harness '{0}': {1}")]
    ControllerConstruction(String, String),
    #[error("dispatch error: {0:?}")]
    Dispatch(#[from] DispatchError),
}

/// The top-level Mission Control orchestration dispatcher.
pub struct MissionControlDispatcher {
    registry: Arc<HarnessRegistry>,
    tachi_client: Option<TachiBridgeClient>,
    default_ephemeral_harness: Option<HarnessId>,
    default_durable_harness: Option<HarnessId>,
    sink: Arc<dyn SessionFactSink>,
    host_identity: HostIdentityRef,
}

impl MissionControlDispatcher {
    /// Create a new Mission Control dispatcher directly with its components.
    #[must_use]
    pub fn new(
        registry: Arc<HarnessRegistry>,
        tachi_client: Option<TachiBridgeClient>,
        sink: Arc<dyn SessionFactSink>,
        host_identity: HostIdentityRef,
    ) -> Self {
        Self {
            registry,
            tachi_client,
            default_ephemeral_harness: None,
            default_durable_harness: Some(HarnessId::from("tachi")),
            sink,
            host_identity,
        }
    }

    /// Get the default ephemeral harness identifier, if configured.
    #[must_use]
    pub fn default_ephemeral_harness(&self) -> Option<&HarnessId> {
        self.default_ephemeral_harness.as_ref()
    }

    /// Set the default ephemeral harness to use when none is explicitly specified.
    pub fn set_default_ephemeral_harness(&mut self, id: HarnessId) {
        self.default_ephemeral_harness = Some(id);
    }

    /// Get the default durable harness identifier, if configured.
    #[must_use]
    pub fn default_durable_harness(&self) -> Option<&HarnessId> {
        self.default_durable_harness.as_ref()
    }

    /// Set the default durable harness identifier.
    pub fn set_default_durable_harness(&mut self, id: HarnessId) {
        self.default_durable_harness = Some(id);
    }

    /// Access the underlying harness registry.
    #[must_use]
    pub fn registry(&self) -> &Arc<HarnessRegistry> {
        &self.registry
    }

    /// Check whether a durable task bridge (e.g. Tachi) is configured and available.
    #[must_use]
    pub fn has_durable_bridge(&self) -> bool {
        self.tachi_client.is_some()
    }

    /// Build a single `HarnessEntry` from a `HarnessDefinition`.
    pub fn build_entry(
        id: impl Into<String>,
        def: &HarnessDefinition,
        default_workspace: &Path,
    ) -> Result<Option<HarnessEntry>, DispatcherError> {
        let id_str = id.into();
        let harness_id = HarnessId::from(id_str.as_str());

        if !def.enabled {
            return Ok(None);
        }

        let workspace = def
            .workspace_root
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| default_workspace.to_path_buf());

        let (controller, supported_caps): (Arc<dyn SessionController>, _) = match def.kind {
            HarnessKind::Codex => {
                let command = def
                    .command
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("codex"));

                let args = if def.args.is_empty() {
                    vec!["app-server".to_string(), "--stdio".to_string()]
                } else {
                    def.args.clone()
                };

                let (sandbox, approval_policy) = match def.permission_policy {
                    HarnessPermissionPolicy::DenyAll => (
                        Some("read-only".to_string()),
                        Some("on-request".to_string()),
                    ),
                    HarnessPermissionPolicy::AllowAll => (
                        Some("workspace-write".to_string()),
                        Some("never".to_string()),
                    ),
                };

                let config = CodexControllerConfig {
                    command,
                    args,
                    env: def.env.clone(),
                    workspace_root: workspace,
                    model: None,
                    sandbox,
                    approval_policy,
                    startup_timeout: Duration::from_secs(def.startup_timeout_secs),
                    turn_timeout: Duration::from_secs(def.turn_timeout_secs),
                    max_line_bytes: 512 * 1024,
                    declared_capabilities: vec![
                        "observe", "wait", "prompt", "cancel", "resume", "events",
                    ],
                };

                let controller = CodexController::new(config).map_err(|e| {
                    DispatcherError::ControllerConstruction(id_str.clone(), format!("{e:?}"))
                })?;
                (
                    Arc::new(controller),
                    CodexControllerConfig::supported_capabilities(),
                )
            }
            HarnessKind::Acp => {
                let command = def
                    .command
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("dsh"));

                let resume_method = match def.resume_method {
                    HarnessResumeMethod::Load => AcpResumeMethod::Load,
                    HarnessResumeMethod::Resume => AcpResumeMethod::Resume,
                };

                let permission_policy = match def.permission_policy {
                    HarnessPermissionPolicy::DenyAll => AcpPermissionPolicy::DenyAll,
                    HarnessPermissionPolicy::AllowAll => AcpPermissionPolicy::AllowAll,
                };

                let config = AcpxControllerConfig {
                    command,
                    args: def.args.clone(),
                    env: def.env.clone(),
                    workspace_root: workspace,
                    session_mode: def.session_mode.clone(),
                    supports_set_mode: def.supports_set_mode,
                    resume_method,
                    permission_policy,
                    startup_timeout: Duration::from_secs(def.startup_timeout_secs),
                    turn_timeout: Duration::from_secs(def.turn_timeout_secs),
                    max_line_bytes: 512 * 1024,
                    declared_capabilities: vec![
                        "observe", "wait", "prompt", "cancel", "resume", "events",
                    ],
                };

                let controller = AcpxController::new(config).map_err(|e| {
                    DispatcherError::ControllerConstruction(id_str.clone(), format!("{e:?}"))
                })?;
                (
                    Arc::new(controller),
                    AcpxControllerConfig::supported_capabilities(),
                )
            }
            HarnessKind::Tachi | HarnessKind::ClaudeCode | HarnessKind::Custom => {
                return Ok(None);
            }
        };

        Ok(Some(HarnessEntry {
            id: harness_id,
            controller,
            capabilities: HarnessCapabilities {
                domains: def.domains.clone(),
                max_concurrent_sessions: def.max_concurrent_sessions,
                supported_session_capabilities: supported_caps,
            },
            enabled: true,
        }))
    }

    /// Initialize a `MissionControlDispatcher` from configured harness definitions.
    pub fn from_definitions(
        definitions: &HashMap<String, HarnessDefinition>,
        default_workspace: &Path,
        tachi_client: Option<TachiBridgeClient>,
        sink: Arc<dyn SessionFactSink>,
        host_identity: HostIdentityRef,
    ) -> Result<Self, DispatcherError> {
        let registry = Arc::new(HarnessRegistry::new());
        let mut first_ephemeral = None;

        for (id, def) in definitions {
            if let Some(entry) = Self::build_entry(id, def, default_workspace)? {
                if first_ephemeral.is_none() {
                    first_ephemeral = Some(entry.id.clone());
                }
                registry.register(entry)?;
            }
        }

        let mut dispatcher = Self::new(registry, tachi_client, sink, host_identity);
        if let Some(first) = first_ephemeral {
            dispatcher.set_default_ephemeral_harness(first);
        }
        Ok(dispatcher)
    }

    /// Select an ephemeral controller for a given request or target hint.
    fn resolve_ephemeral_controller(
        &self,
        target_harness: Option<&str>,
    ) -> Result<Arc<dyn SessionController>, DispatchError> {
        if let Some(target) = target_harness {
            let id = HarnessId::from(target);
            self.registry
                .get(&id)
                .ok_or(DispatchError::EphemeralRequiresController)
        } else if let Some(ref default_id) = self.default_ephemeral_harness {
            self.registry
                .get(default_id)
                .ok_or(DispatchError::EphemeralRequiresController)
        } else {
            let all = self.registry.list();
            let first_id = all
                .first()
                .ok_or(DispatchError::EphemeralRequiresController)?;
            self.registry
                .get(first_id)
                .ok_or(DispatchError::EphemeralRequiresController)
        }
    }

    /// Plan and execute dispatch for an execution request.
    ///
    /// Respects the three-path model:
    /// - `Reason` -> in-process planning (returns `DispatchExecutionOutcome::Reason`)
    /// - `EphemeralExec` -> runs through selected external harness (`Codex`, `DSH`, etc.)
    /// - `DurableExec` -> delegates to durable Tachi bridge (`TaskIntentV1`)
    pub async fn dispatch(
        &self,
        request: &ExecutionRequestV1,
        target_harness: Option<&str>,
        request_id: &RequestId,
    ) -> Result<DispatchExecutionOutcome, DispatchError> {
        let is_explicit_durable = target_harness.is_some_and(|t| t == "tachi");

        let route = if is_explicit_durable {
            ExecutionRouteV1::DurableExec
        } else {
            ExecutionRouteV1::route(request)
        };

        let has_bridge = self.has_durable_bridge();
        let has_ephemeral = !self.registry.list().is_empty();

        match route {
            ExecutionRouteV1::Reason => Ok(DispatchExecutionOutcome::Reason),
            ExecutionRouteV1::DurableExec => {
                ObjectiveV1::new(request.objective.clone())
                    .map_err(|_| DispatchError::ObjectiveTooLarge)?;

                let tachi = self
                    .tachi_client
                    .as_ref()
                    .ok_or(DispatchError::DurableRequiresBridge)?;

                let bounded_obj = BoundedText::new(&request.objective)
                    .map_err(|_| DispatchError::ObjectiveTooLarge)?;
                let inputs = TaskIntentInputs {
                    objective: bounded_obj,
                    capability_request: CapabilityRequest {
                        capability: Capability::RepositoryImplementation,
                    },
                    constraints: Vec::new(),
                    expected_artifacts: Vec::new(),
                    evaluation_requirement: EvaluationRequirement {
                        independence: IndependenceClass::DeterministicCheck,
                    },
                };
                let policy = RequesterBridgePolicy {
                    admitted_capabilities: BTreeSet::from([Capability::RepositoryImplementation]),
                    workspace_source: None,
                    routing_preference: Some(RoutingPreference::PreferTachiManaged),
                    approval_requirement: ApprovalRequirement::NotRequired,
                    privacy_class: PrivacyClass::Internal,
                };
                let structural = StructuralIntentContext {
                    requester: RequesterRef::claim("agent:mission-control")
                        .map_err(|_| DispatchError::DurableRequiresBridge)?,
                    parent_ref: None,
                    supervisor_ref: None,
                    context_bundle_ref: BoundedText::new("bundle-default")
                        .map_err(|_| DispatchError::DurableRequiresBridge)?,
                    source_refs: Vec::new(),
                    expiry: None,
                    retry_of: None,
                };
                let intent = compose_intent(&inputs, &policy, &structural)
                    .map_err(|_| DispatchError::DurableRequiresBridge)?;

                let receipt = tachi
                    .submit(&intent, request_id)
                    .await
                    .map_err(|_| DispatchError::DurableRequiresBridge)?;

                Ok(DispatchExecutionOutcome::Durable(receipt))
            }
            ExecutionRouteV1::EphemeralExec => {
                let plan = plan_dispatch(request, has_bridge, has_ephemeral)?;
                let DispatchPlan::Ephemeral { run } = plan else {
                    return Ok(DispatchExecutionOutcome::Reason);
                };

                let controller = self.resolve_ephemeral_controller(target_harness)?;
                let gated = Arc::new(GatedSessionController::new(controller));
                let tool = ExecutionSubagentTool::new(
                    gated,
                    self.sink.clone(),
                    self.host_identity.clone(),
                );

                let report = tool.run(&run).await;
                Ok(DispatchExecutionOutcome::Ephemeral(Box::new(report)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_subagent::fixtures::InMemoryFactSink;

    #[test]
    fn test_dispatcher_from_empty_definitions() {
        let defs = HashMap::new();
        let sink = Arc::new(InMemoryFactSink::default());
        let host = HostIdentityRef::from_opaque("test-host");
        let dispatcher =
            MissionControlDispatcher::from_definitions(&defs, Path::new("/tmp"), None, sink, host)
                .expect("empty dispatcher constructs");

        assert!(dispatcher.registry().list().is_empty());
        assert!(!dispatcher.has_durable_bridge());
    }

    #[test]
    fn test_dispatcher_routing_reason_path() {
        let defs = HashMap::new();
        let sink = Arc::new(InMemoryFactSink::default());
        let host = HostIdentityRef::from_opaque("test-host");
        let dispatcher =
            MissionControlDispatcher::from_definitions(&defs, Path::new("/tmp"), None, sink, host)
                .unwrap();

        let req = ExecutionRequestV1 {
            objective: "Analyze performance bottlenecks".to_string(),
            needs_restart_recovery: false,
            needs_remote: false,
            needs_multi_attempt: false,
            needs_approvals: false,
            needs_evidence: false,
            analysis_only: true,
        };

        let req_id = RequestId::new("req-1".to_string()).expect("valid req id");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let outcome = rt
            .block_on(dispatcher.dispatch(&req, None, &req_id))
            .unwrap();

        assert!(matches!(outcome, DispatchExecutionOutcome::Reason));
    }

    #[test]
    fn test_dispatcher_routing_durable_fails_closed_without_bridge() {
        let defs = HashMap::new();
        let sink = Arc::new(InMemoryFactSink::default());
        let host = HostIdentityRef::from_opaque("test-host");
        let dispatcher =
            MissionControlDispatcher::from_definitions(&defs, Path::new("/tmp"), None, sink, host)
                .unwrap();

        let req = ExecutionRequestV1 {
            objective: "Long running overhaul".to_string(),
            needs_restart_recovery: true,
            needs_remote: false,
            needs_multi_attempt: false,
            needs_approvals: false,
            needs_evidence: false,
            analysis_only: false,
        };

        let req_id = RequestId::new("req-2".to_string()).expect("valid req id");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(dispatcher.dispatch(&req, None, &req_id))
            .unwrap_err();

        assert_eq!(err, DispatchError::DurableRequiresBridge);
    }

    #[test]
    fn test_dispatcher_from_definitions_with_codex_and_acp() {
        let exe = std::env::current_exe().expect("current test binary exists");
        let ws = std::env::temp_dir();
        let mut defs = HashMap::new();
        let codex_def = HarnessDefinition {
            kind: HarnessKind::Codex,
            command: Some(exe.to_string_lossy().to_string()),
            args: vec!["app-server".to_string()],
            env: HashMap::new(),
            workspace_root: Some(ws.to_string_lossy().to_string()),
            permission_policy: HarnessPermissionPolicy::DenyAll,
            session_mode: None,
            supports_set_mode: false,
            resume_method: HarnessResumeMethod::Load,
            startup_timeout_secs: 15,
            turn_timeout_secs: 120,
            max_concurrent_sessions: 2,
            domains: vec!["rust".to_string(), "backend".to_string()],
            route_preference: Default::default(),
            enabled: true,
        };
        let dsh_def = HarnessDefinition {
            kind: HarnessKind::Acp,
            command: Some(exe.to_string_lossy().to_string()),
            args: vec!["--acp".to_string()],
            env: HashMap::new(),
            workspace_root: None,
            permission_policy: HarnessPermissionPolicy::AllowAll,
            session_mode: Some("code".to_string()),
            supports_set_mode: true,
            resume_method: HarnessResumeMethod::Resume,
            startup_timeout_secs: 20,
            turn_timeout_secs: 300,
            max_concurrent_sessions: 1,
            domains: vec!["python".to_string()],
            route_preference: Default::default(),
            enabled: true,
        };
        defs.insert("codex".to_string(), codex_def);
        defs.insert("dsh".to_string(), dsh_def);

        let sink = Arc::new(InMemoryFactSink::default());
        let host = HostIdentityRef::from_opaque("test-host");
        let dispatcher =
            MissionControlDispatcher::from_definitions(&defs, &ws, None, sink, host).unwrap();

        assert!(!dispatcher.has_durable_bridge());
        assert_eq!(
            dispatcher.default_durable_harness(),
            Some(&HarnessId::from("tachi"))
        );
        assert!(dispatcher.default_ephemeral_harness().is_some());

        let registry = dispatcher.registry();
        assert!(registry.get(&HarnessId::from("codex")).is_some());
        assert!(registry.get(&HarnessId::from("dsh")).is_some());
    }

    #[test]
    fn test_dispatcher_routing_ephemeral_fails_closed_without_harness() {
        let defs = HashMap::new();
        let sink = Arc::new(InMemoryFactSink::default());
        let host = HostIdentityRef::from_opaque("test-host");
        let dispatcher =
            MissionControlDispatcher::from_definitions(&defs, Path::new("/tmp"), None, sink, host)
                .unwrap();

        let req = ExecutionRequestV1 {
            objective: "Quick fix".to_string(),
            needs_restart_recovery: false,
            needs_remote: false,
            needs_multi_attempt: false,
            needs_approvals: false,
            needs_evidence: false,
            analysis_only: false,
        };

        let req_id = RequestId::new("req-3".to_string()).expect("valid req id");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(dispatcher.dispatch(&req, None, &req_id))
            .unwrap_err();

        assert_eq!(err, DispatchError::EphemeralRequiresController);
    }
}
