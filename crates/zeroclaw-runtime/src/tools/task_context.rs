//! The task list belongs to the identity admitted with an execution context.
//! Aliases are mutable configuration keys, so looking them up again must never
//! transfer an old tool or prompt to a newly created agent with the same name.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use zeroclaw_api::companion::AgentIdentityId;
use zeroclaw_config::schema::Config;
use zeroclaw_policy::{SecurityPolicy, ToolOperation};

use super::task_list::{ReminderTargetResolver, TaskOwnerResolver};

type CapturedIdentities = BTreeMap<String, Result<AgentIdentityId, String>>;

/// Capture before queuing background work, and keep it through retries and
/// registry rebuilds. A failed capture stays unavailable; it never remints.
/// Peer turns share the complete snapshot so a later hop cannot bind a reused
/// alias while rebuilding its tool registry from the original configuration.
#[derive(Clone)]
pub struct TaskIdentity {
    alias: String,
    data_dir: PathBuf,
    store_dir: PathBuf,
    identities: Arc<Result<CapturedIdentities, String>>,
}

impl TaskIdentity {
    pub fn capture(config: &Config, alias: &str) -> Self {
        if !config.agent(alias).is_some_and(|agent| agent.enabled) {
            return Self::unavailable(config, alias, "task agent is not configured or is disabled");
        }
        Self::capture_all(config).for_alias(alias)
    }

    /// Pin the configured owners before starting a long-lived worker or its
    /// supervisor. Derive each turn's owner with `for_alias`; retries and polls
    /// must retain this snapshot rather than resolve aliases again.
    pub fn capture_all(config: &Config) -> Self {
        let unavailable = if !crate::platform::create_runtime(&config.runtime)
            .is_ok_and(|runtime| runtime.has_filesystem_access())
        {
            Some("the durable task list requires persistent filesystem access")
        } else if config.channels.session_backend != "sqlite" {
            Some("the durable task list requires the SQLite session backend")
        } else {
            None
        };
        if let Some(error) = unavailable {
            return Self::unavailable(config, "", error);
        }
        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        let identities = config
            .agents
            .iter()
            .filter(|(_, agent)| agent.enabled)
            .map(|(alias, _)| {
                let identity =
                    zeroclaw_memory::companion::resolve_or_mint_agent_identity(&store_dir, alias)
                        .map_err(|error| format!("agent identity unavailable: {error}"));
                (alias.clone(), identity)
            })
            .collect();
        Self {
            alias: String::new(),
            data_dir: config.data_dir.clone(),
            store_dir,
            identities: Arc::new(Ok(identities)),
        }
    }

    pub(super) fn unavailable(config: &Config, alias: &str, error: &str) -> Self {
        Self {
            alias: alias.to_string(),
            data_dir: config.data_dir.clone(),
            store_dir: config.companion_memory.resolved_store_dir(&config.data_dir),
            identities: Arc::new(Err(error.to_string())),
        }
    }

    /// Select an owner from the original snapshot, never from current aliases.
    pub fn for_alias(&self, alias: &str) -> Self {
        Self {
            alias: alias.to_string(),
            ..self.clone()
        }
    }

    fn resolve(&self, config: &Config, alias: &str) -> Result<(String, PathBuf), String> {
        let identities = self.identities.as_ref().as_ref().map_err(Clone::clone)?;
        let identity = identities
            .get(alias)
            .ok_or_else(|| "task agent was not captured with this execution context".to_string())?
            .as_ref()
            .map_err(Clone::clone)?;
        if alias != self.alias
            || config.data_dir != self.data_dir
            || config.companion_memory.resolved_store_dir(&config.data_dir) != self.store_dir
        {
            return Err(
                "task storage or owner changed; recreate the execution context".to_string(),
            );
        }
        if config.channels.session_backend != "sqlite" {
            return Err("the durable task list requires the SQLite session backend".to_string());
        }
        if !config.agent(alias).is_some_and(|agent| agent.enabled) {
            return Err("task agent is not configured or is disabled".to_string());
        }
        if zeroclaw_memory::companion::peek_agent_identity(&self.store_dir, alias).as_ref()
            != Some(identity)
        {
            return Err("task agent identity changed; recreate the execution context".to_string());
        }
        Ok((identity.as_str().to_string(), self.data_dir.clone()))
    }
}

/// One pinned owner is shared by the read-only prompt and the mutating tool.
/// Both honor live revocation; only the tool consumes the action budget.
pub(super) fn resolvers(
    config: Arc<Config>,
    alias: &str,
    security: Arc<SecurityPolicy>,
    persistent_writes: bool,
    live_config: Option<Arc<RwLock<Config>>>,
    identity: Option<TaskIdentity>,
) -> (TaskOwnerResolver, TaskOwnerResolver) {
    if !persistent_writes {
        let unavailable: TaskOwnerResolver = Arc::new(|| {
            Err("the durable task list requires persistent filesystem access".to_string())
        });
        return (unavailable.clone(), unavailable);
    }
    let identity = identity.unwrap_or_else(|| TaskIdentity::capture(&config, alias));
    let alias = alias.to_string();
    let resolve = Arc::new(move |write: bool| {
        let validate = |current: &Config| {
            if !crate::platform::create_runtime(&current.runtime)
                .is_ok_and(|runtime| runtime.has_filesystem_access())
            {
                return Err(
                    "the durable task list requires persistent filesystem access".to_string(),
                );
            }
            if !security.is_tool_allowed("task_list") {
                return Err("task list denied by execution policy".to_string());
            }
            let owner = identity.resolve(current, &alias)?;
            let mut policy =
                SecurityPolicy::for_agent(current, &alias).map_err(|error| error.to_string())?;
            if !policy.is_tool_allowed("task_list") {
                return Err("task list denied by current agent policy".to_string());
            }
            if write {
                if !security.can_act() {
                    return Err("task list writes denied in read-only mode".to_string());
                }
                // Preserve the execution's shared tracker and its narrower
                // budget when live configuration permits a larger one.
                policy.tracker = security.tracker.clone();
                policy.max_actions_per_hour = policy
                    .max_actions_per_hour
                    .min(security.max_actions_per_hour);
                policy.enforce_tool_operation(ToolOperation::Act, "task_list")?;
            }
            Ok(owner)
        };
        match &live_config {
            Some(live) => validate(&live.read()),
            None => validate(&config),
        }
    });
    let read = resolve.clone();
    (
        Arc::new(move || read(false)),
        Arc::new(move || resolve(true)),
    )
}

/// Resolve the owner-configured reminder destination (#568) against the
/// current live config (or the startup snapshot when no live handle exists).
pub(super) fn reminder_target_resolver(
    config: Arc<Config>,
    live_config: Option<Arc<RwLock<Config>>>,
) -> ReminderTargetResolver {
    Arc::new(move || {
        let resolve = |current: &Config| {
            let target = current.tasks.reminder_target.as_ref().ok_or_else(|| {
                "no [tasks.reminder_target] configured: reminders are unavailable \
                 until the owner sets one"
                    .to_string()
            })?;
            target
                .validate()
                .map_err(|error| format!("invalid [tasks.reminder_target]: {error}"))?;
            if !current.gateway.bridges.contains_key(&target.bridge) {
                return Err(format!(
                    "[tasks.reminder_target] names bridge {:?} which is not configured",
                    target.bridge
                ));
            }
            Ok((
                target.bridge.clone(),
                target.recipient.clone(),
                target.thread_id.clone(),
            ))
        };
        match &live_config {
            Some(live) => resolve(&live.read()),
            None => resolve(&config),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{AllToolsResult, TaskListSection, Tool};
    use async_trait::async_trait;
    use serde_json::json;
    use zeroclaw_config::schema::AliasedAgentConfig;
    use zeroclaw_infra::session_sqlite::{SqliteSessionBackend, agent_tasks::TaskOp};
    use zeroclaw_providers::{ChatRequest, ChatResponse, ModelProvider, ToolCall};

    fn agent() -> AliasedAgentConfig {
        AliasedAgentConfig {
            risk_profile: "tasks".into(),
            ..Default::default()
        }
    }

    fn config(tmp: &tempfile::TempDir) -> Config {
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        config.agents.clear();
        config.agents.insert("alpha".into(), agent());
        config
            .risk_profiles
            .insert("tasks".into(), Default::default());
        config
    }

    fn registry(config: &Config, live: Option<Arc<RwLock<Config>>>) -> AllToolsResult {
        let policy = Arc::new(SecurityPolicy::for_agent(config, "alpha").unwrap());
        let runtime = Arc::from(crate::platform::create_runtime(&config.runtime).unwrap());
        crate::tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &policy,
            config.risk_profile_for_agent("alpha").unwrap(),
            "alpha",
            runtime,
            Arc::new(zeroclaw_memory::NoneMemory::new("none")),
            &config.browser,
            &config.http_request,
            &config.web_fetch,
            &config.data_dir,
            &config.agents,
            None,
            config,
            false,
            None,
            live,
            None,
            None,
        )
    }

    fn task_tool(built: &AllToolsResult) -> &dyn Tool {
        built
            .tools
            .iter()
            .find(|tool| tool.name() == "task_list")
            .unwrap()
            .as_ref()
    }

    fn add(revision: i64, text: &str) -> serde_json::Value {
        json!({"expected_revision": revision, "ops": [{"op": "add", "description": text}]})
    }

    #[tokio::test]
    async fn registered_task_tool_obeys_readonly_and_live_revocation() {
        for change in [
            "readonly",
            "excluded",
            "disabled",
            "backend",
            "data_dir",
            "identity_dir",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let config = config(&tmp);
            let live = Arc::new(RwLock::new(config.clone()));
            let built = registry(&config, Some(live.clone()));
            assert!(
                task_tool(&built)
                    .execute(add(0, "committed"))
                    .await
                    .unwrap()
                    .success
            );
            let owner = (built.task_owner.as_ref().unwrap())().unwrap().0;
            {
                let mut current = live.write();
                let risk = current.agents["alpha"].risk_profile.clone();
                match change {
                    "readonly" => {
                        current.risk_profiles.get_mut(risk.as_str()).unwrap().level =
                            zeroclaw_policy::AutonomyLevel::ReadOnly
                    }
                    "excluded" => {
                        current
                            .risk_profiles
                            .get_mut(risk.as_str())
                            .unwrap()
                            .excluded_tools = vec!["task_list".into()]
                    }
                    "disabled" => current.agents.get_mut("alpha").unwrap().enabled = false,
                    "backend" => current.channels.session_backend = "jsonl".into(),
                    "data_dir" => current.data_dir = tmp.path().join("other-data"),
                    "identity_dir" => {
                        current.companion_memory.store_dir = Some(tmp.path().join("other-identity"))
                    }
                    _ => unreachable!(),
                }
            }
            let denied = task_tool(&built)
                .execute(add(1, "must not write"))
                .await
                .unwrap();
            assert!(!denied.success, "{change}: {denied:?}");
            let list = SqliteSessionBackend::new(&config.data_dir)
                .unwrap()
                .task_list(&owner)
                .unwrap();
            assert_eq!(list.revision, 1, "{change}");
            assert_eq!(list.items.len(), 1, "{change}");
            let section = TaskListSection {
                resolve: built.task_owner.unwrap(),
            }
            .refresh_into("");
            assert!(
                section.contains(if change == "readonly" {
                    "committed"
                } else {
                    "unavailable"
                }),
                "{change}: {section}"
            );
        }

        let tmp = tempfile::tempdir().unwrap();
        let mut config = config(&tmp);
        let risk = config.agents["alpha"].risk_profile.clone();
        config.risk_profiles.get_mut(risk.as_str()).unwrap().level =
            zeroclaw_policy::AutonomyLevel::ReadOnly;
        let built = registry(&config, None);
        assert!(
            !task_tool(&built)
                .execute(add(0, "denied from construction"))
                .await
                .unwrap()
                .success
        );
    }

    #[tokio::test]
    async fn alias_reuse_cannot_rebind_an_old_tool_or_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config(&tmp);
        let live = Arc::new(RwLock::new(config.clone()));
        let old = registry(&config, Some(live.clone()));
        assert!(
            task_tool(&old)
                .execute(add(0, "original owner task"))
                .await
                .unwrap()
                .success
        );
        let original = (old.task_owner.as_ref().unwrap())().unwrap().0;
        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        zeroclaw_memory::companion::rename_alias_identity(&store_dir, "alpha", "beta").unwrap();
        let renamed = config.agents.remove("alpha").unwrap();
        config.agents.insert("beta".into(), renamed);
        zeroclaw_memory::companion::invalidate_alias_identity(&store_dir, "alpha").unwrap();
        config.agents.insert("alpha".into(), agent());
        *live.write() = config.clone();
        let new = registry(&config, Some(live));
        assert!(
            task_tool(&new)
                .execute(add(0, "replacement owner task"))
                .await
                .unwrap()
                .success
        );
        assert!(
            !task_tool(&old)
                .execute(add(1, "old context must not enter replacement"))
                .await
                .unwrap()
                .success
        );
        let old_prompt = TaskListSection {
            resolve: old.task_owner.unwrap(),
        }
        .refresh_into("");
        assert!(old_prompt.contains("unavailable"));
        assert!(!old_prompt.contains("replacement owner task"));
        let db = SqliteSessionBackend::new(&config.data_dir).unwrap();
        assert_eq!(
            db.task_list(&original).unwrap().items[0].description,
            "original owner task"
        );
        let replacement = (new.task_owner.as_ref().unwrap())().unwrap().0;
        assert_ne!(replacement, original);
        assert_eq!(db.task_list(&replacement).unwrap().items.len(), 1);
    }

    #[tokio::test]
    async fn task_writes_share_the_action_budget_and_keep_the_narrower_limit() {
        for (initial_limit, live_limit) in [(1, 4), (4, 1)] {
            let tmp = tempfile::tempdir().unwrap();
            let mut config = config(&tmp);
            config.agents.get_mut("alpha").unwrap().runtime_profile = "tasks".into();
            config.runtime_profiles.insert(
                "tasks".into(),
                zeroclaw_config::schema::RuntimeProfileConfig {
                    max_actions_per_hour: initial_limit,
                    ..Default::default()
                },
            );
            let live = Arc::new(RwLock::new(config.clone()));
            let security = Arc::new(SecurityPolicy::for_agent(&config, "alpha").unwrap());
            let (read, write) = resolvers(
                Arc::new(config.clone()),
                "alpha",
                security.clone(),
                true,
                Some(live.clone()),
                None,
            );
            let target = reminder_target_resolver(Arc::new(config), Some(live.clone()));
            let tool = super::super::task_list::TaskListTool::new(write, target);
            // Prompt refresh is a read; repeated refreshes must not spend the
            // execution's shared action allowance.
            for _ in 0..4 {
                assert!(read().is_ok());
            }
            assert!(!security.is_rate_limited());
            // Simulate an action by another tool using the same policy.
            assert!(security.record_action());
            live.write()
                .runtime_profiles
                .get_mut("tasks")
                .unwrap()
                .max_actions_per_hour = live_limit;
            let denied = tool.execute(add(0, "over budget")).await.unwrap();
            assert!(
                !denied.success,
                "{initial_limit} -> {live_limit}: {denied:?}"
            );
            assert!(
                denied
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("action budget exhausted"))
            );
            assert!(
                read().is_ok(),
                "reads still work after the action budget is exhausted"
            );
        }
    }

    #[test]
    fn unavailable_identity_and_nonpersistent_runtime_never_remint() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(&tmp);
        let policy = Arc::new(SecurityPolicy::default());
        let (read, write) = resolvers(Arc::new(config.clone()), "alpha", policy, false, None, None);
        assert!(read().is_err() && write().is_err());
        assert!(!config.data_dir.exists());

        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        std::fs::create_dir_all(&store_dir).unwrap();
        let path = store_dir.join(zeroclaw_config::companion::COMPANION_AGENT_IDENTITY_FILE);
        std::fs::write(&path, "invalid json").unwrap();
        let pin = TaskIdentity::capture(&config, "alpha");
        std::fs::remove_file(path).unwrap();
        let repaired = TaskIdentity::capture(&config, "alpha");
        assert!(pin.resolve(&config, "alpha").is_err());
        assert!(repaired.resolve(&config, "alpha").is_ok());
    }

    #[tokio::test]
    async fn nonpersistent_registry_never_mints_owners_from_a_native_config_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = config(&tmp);
        config.agents.insert("beta".into(), agent());
        let policy = Arc::new(SecurityPolicy::default());
        // The admitted adapter is authoritative even when the root snapshot
        // still names native execution. Constructing peer tools must not mint
        // identities from that snapshot before the task resolver can reject it.
        let runtime = Arc::new(crate::platform::DockerRuntime::new(
            zeroclaw_config::schema::DockerRuntimeConfig {
                mount_workspace: false,
                ..Default::default()
            },
        ));
        let built = crate::tools::all_tools_with_runtime(
            Arc::new(config.clone()),
            &policy,
            config.risk_profile_for_agent("alpha").unwrap(),
            "alpha",
            runtime,
            Arc::new(zeroclaw_memory::NoneMemory::new("none")),
            &config.browser,
            &config.http_request,
            &config.web_fetch,
            &config.data_dir,
            &config.agents,
            None,
            &config,
            false,
            None,
            None,
            None,
            None,
        );
        assert!((built.task_owner.as_ref().unwrap())().is_err());
        assert!(
            !task_tool(&built)
                .execute(add(0, "must not persist"))
                .await
                .unwrap()
                .success
        );
        let store_dir = config.companion_memory.resolved_store_dir(&config.data_dir);
        assert!(
            !store_dir
                .join(zeroclaw_config::companion::COMPANION_AGENT_IDENTITY_FILE)
                .exists()
        );
    }

    struct TaskProvider {
        prompts: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ModelProvider for TaskProvider {
        async fn chat_with_system(
            &self,
            _: Option<&str>,
            _: &str,
            _: &str,
            _: Option<f64>,
        ) -> anyhow::Result<String> {
            unreachable!("native chat used")
        }
        fn supports_native_tools(&self) -> bool {
            true
        }
        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _: &str,
            _: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            let mut prompts = self.prompts.lock();
            prompts.push(
                request
                    .messages
                    .iter()
                    .find(|m| m.role == "system")
                    .unwrap()
                    .content
                    .clone(),
            );
            let first = prompts.len() == 1;
            Ok(ChatResponse {
                text: (!first).then(|| "done".into()),
                tool_calls: if first {
                    vec![ToolCall {
                        id: "task-write".into(),
                        name: "task_list".into(),
                        arguments: add(1, "created during this turn").to_string(),
                        extra_content: None,
                    }]
                } else {
                    vec![]
                },
                usage: None,
                reasoning_content: None,
            })
        }
    }
    impl zeroclaw_api::attribution::Attributable for TaskProvider {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            use zeroclaw_api::attribution::*;
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }
        fn alias(&self) -> &str {
            "task-provider"
        }
    }

    #[tokio::test]
    async fn production_agent_refreshes_tasks_on_every_model_request() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(&tmp);
        let built = registry(&config, None);
        let owner = (built.task_owner.as_ref().unwrap())().unwrap().0;
        let db = SqliteSessionBackend::new(&config.data_dir).unwrap();
        db.apply_task_ops(
            &owner,
            0,
            &[TaskOp::Add {
                description: "from another session".into(),
                status: "pending".into(),
                reminder: None,
            }],
        )
        .unwrap();
        let prompts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut agent = crate::agent::Agent::builder()
            .model_provider(Box::new(TaskProvider {
                prompts: prompts.clone(),
            }))
            .tools(
                built
                    .tools
                    .into_iter()
                    .filter(|tool| tool.name() == "task_list")
                    .collect(),
            )
            .task_owner(built.task_owner)
            .memory(Arc::new(zeroclaw_memory::NoneMemory::new("none")))
            .observer(Arc::new(crate::observability::NoopObserver))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .workspace_dir(tmp.path().to_path_buf())
            .agent_alias("alpha".into())
            .build()
            .unwrap();
        agent.turn("add a commitment").await.unwrap();
        let prompts = prompts.lock();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains("from another session"));
        assert!(!prompts[0].contains("created during this turn"));
        assert!(prompts[1].contains("created during this turn"));
        assert!(prompts[1].contains("expected_revision=2"));
        assert_eq!(
            prompts[1]
                .matches(super::super::task_section::TASKS_REGION_BEGIN)
                .count(),
            1
        );
        assert_eq!(db.task_list(&owner).unwrap().revision, 2);
    }

    #[tokio::test]
    async fn production_agent_projects_tasks_without_waiting_for_the_session_writer() {
        struct ProjectionProvider {
            prompts: Arc<parking_lot::Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl ModelProvider for ProjectionProvider {
            async fn chat_with_system(
                &self,
                _: Option<&str>,
                _: &str,
                _: &str,
                _: Option<f64>,
            ) -> anyhow::Result<String> {
                anyhow::bail!("fixture requires canonical chat dispatch")
            }
            async fn chat(
                &self,
                request: ChatRequest<'_>,
                _: &str,
                _: Option<f64>,
            ) -> anyhow::Result<ChatResponse> {
                self.prompts.lock().push(
                    request
                        .messages
                        .iter()
                        .find(|message| message.role == "system")
                        .unwrap()
                        .content
                        .clone(),
                );
                Ok(ChatResponse {
                    text: Some("done".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                })
            }
        }
        impl zeroclaw_api::attribution::Attributable for ProjectionProvider {
            fn role(&self) -> zeroclaw_api::attribution::Role {
                zeroclaw_api::attribution::Role::System
            }
            fn alias(&self) -> &str {
                "task-projection-fixture"
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let config = config(&tmp);
        // Build the real registry before taking the writer lock: session
        // startup, not per-request projection, owns schema initialization.
        let built = registry(&config, None);
        let owner = (built.task_owner.as_ref().unwrap())().unwrap().0;
        let db = SqliteSessionBackend::new(&config.data_dir).unwrap();
        db.apply_task_ops(
            &owner,
            0,
            &[TaskOp::Add {
                description: "committed before writer".into(),
                status: "pending".into(),
                reminder: None,
            }],
        )
        .unwrap();
        let prompts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut agent = crate::agent::Agent::builder()
            .model_provider(Box::new(ProjectionProvider {
                prompts: prompts.clone(),
            }))
            // AgentBuilder admits the Tasks section only when the retained
            // registry includes task_list. Keep the real tools and owner
            // together so this exercises production prompt assembly.
            .tools(built.tools)
            .task_owner(built.task_owner)
            .memory(Arc::new(zeroclaw_memory::NoneMemory::new("none")))
            .observer(Arc::new(crate::observability::NoopObserver))
            .tool_dispatcher(Box::new(crate::agent::dispatcher::NativeToolDispatcher))
            .workspace_dir(tmp.path().to_path_buf())
            .agent_alias("alpha".into())
            .build()
            .unwrap();
        let writer =
            rusqlite::Connection::open(config.data_dir.join("sessions/sessions.db")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        writer.execute("UPDATE agent_tasks SET description = 'committed on next request' WHERE agent_identity = ?1", [&owner]).unwrap();
        writer
            .execute(
                "UPDATE agent_task_revisions SET revision = 2 WHERE agent_identity = ?1",
                [&owner],
            )
            .unwrap();
        assert!(!writer.is_autocommit(), "the writer must remain open");

        agent
            .turn("read the committed task snapshot")
            .await
            .unwrap();
        assert!(
            !writer.is_autocommit(),
            "projection must complete while the writer still owns its transaction"
        );
        {
            let prompts = prompts.lock();
            assert_eq!(prompts.len(), 1);
            assert!(
                prompts[0].contains("committed before writer"),
                "expected the committed snapshot while the writer is open; synthetic prompt:\n{}",
                prompts[0]
            );
            assert!(prompts[0].contains("expected_revision=1"));
            assert!(!prompts[0].contains("committed on next request"));
            assert!(!prompts[0].contains("unavailable"));
        }
        writer.execute_batch("COMMIT").unwrap();
        agent
            .turn("read the latest committed task snapshot")
            .await
            .unwrap();
        let prompts = prompts.lock();
        assert_eq!(prompts.len(), 2);
        assert!(
            prompts[1].contains("committed on next request"),
            "the next request must project the newly committed snapshot; synthetic prompt:\n{}",
            prompts[1]
        );
        assert!(prompts[1].contains("expected_revision=2"));
        assert!(!prompts[1].contains("committed before writer"));
    }
}
