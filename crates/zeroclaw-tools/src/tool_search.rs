//! Built-in `tool_search` tool for on-demand MCP tool schema loading.

use std::fmt::Write;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::mcp_deferred::{ActivatedToolSet, ActivationHook, DeferredMcpToolSet};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::autonomy::McpDiscoveredToolPolicy;

/// Default maximum number of search results.
const DEFAULT_MAX_RESULTS: usize = 5;

#[derive(Clone, Default)]
pub struct ToolAccessPolicy {
    pub allowed: Option<Vec<String>>,
    pub caller_allowed: Option<Vec<String>>,
    pub denied: Option<Vec<String>>,
    /// Whether a non-empty `allowed` list auto-admits runtime-discovered MCP
    /// tools. Defaults to `ExplicitOnly`, so a call site that forgets to pass
    /// it fails closed rather than open.
    pub mcp_discovered: McpDiscoveredToolPolicy,
}

impl ToolAccessPolicy {
    pub fn from_security(
        allowed_tools: Option<&[String]>,
        excluded_tools: Option<&[String]>,
        caller_allowed: Option<&[String]>,
        mcp_discovered: McpDiscoveredToolPolicy,
    ) -> Option<Self> {
        let mut policy = Self {
            mcp_discovered,
            ..Self::default()
        };
        if let Some(list) = allowed_tools {
            policy.allowed = Some(list.to_vec());
        }
        if let Some(caller) = caller_allowed {
            policy.caller_allowed = Some(caller.to_vec());
        }
        if let Some(list) = excluded_tools {
            policy.denied = Some(list.to_vec());
        }
        if policy.allowed.is_some() || policy.caller_allowed.is_some() || policy.denied.is_some() {
            Some(policy)
        } else {
            None
        }
    }

    pub fn is_tool_allowed(&self, name: &str) -> bool {
        // Deny-list always wins.
        let in_deny = self
            .denied
            .as_ref()
            .is_some_and(|list| list.iter().any(|t| t == name));
        if in_deny {
            return false;
        }

        // Risk-profile gate. An explicit empty list (`Some(vec![])`) means
        // "deny everything". Whether a non-empty list also admits unlisted
        // MCP `<server>__<tool>` names is the profile's call — under the
        // default `ExplicitOnly` it does not.
        let risk_ok = match self.allowed.as_ref() {
            None => true,
            Some(list) if list.is_empty() => false,
            Some(list) => {
                list.iter().any(|t| t == name) || self.mcp_discovered.admits_unlisted(name)
            }
        };
        if !risk_ok {
            return false;
        }

        match self.caller_allowed.as_ref() {
            None => true,
            Some(list) => list.iter().any(|t| t == name),
        }
    }
}

/// Built-in tool that fetches full schemas from the conversation's MCP owner.
pub struct ToolSearchTool {
    activated: Arc<Mutex<ActivatedToolSet>>,
}

impl ToolSearchTool {
    pub fn new(deferred: DeferredMcpToolSet, activated: Arc<Mutex<ActivatedToolSet>>) -> Self {
        ActivatedToolSet::lock(&activated).deferred = Some(deferred);
        Self { activated }
    }

    pub fn with_access_policy(self, policy: ToolAccessPolicy) -> Self {
        ActivatedToolSet::lock(&self.activated).set_access_policy(policy);
        self
    }

    pub fn with_activation_hook(self, hook: ActivationHook) -> Self {
        ActivatedToolSet::lock(&self.activated).activation_hook = Some(hook);
        self
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }

    fn description(&self) -> &str {
        "Fetch full schema definitions for deferred MCP tools so they can be called. \
         Use \"select:name1,name2\" for exact match or keywords to search."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "description": "Query to find deferred tools. Use \"select:<tool_name>\" for direct selection, or keywords to search.",
                    "type": "string"
                },
                "max_results": {
                    "description": "Maximum number of results to return (default: 5)",
                    "type": "number",
                    "default": DEFAULT_MAX_RESULTS
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();

        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .map(|v| usize::try_from(v).unwrap_or(DEFAULT_MAX_RESULTS))
            .unwrap_or(DEFAULT_MAX_RESULTS);

        if query.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(crate::i18n::get_required_tool_string(
                    "tool-mcp-search-query-required",
                )),
            });
        }

        // Parse query mode
        if let Some(names_str) = query.strip_prefix("select:") {
            // Exact selection mode
            let names: Vec<&str> = names_str.split(',').map(str::trim).collect();
            return self.select_tools(&names);
        }

        // Filter before applying the result cap. Search and exact-name
        // recovery read the same catalog and admission policy.
        let (matched, names) = {
            let guard = ActivatedToolSet::lock(&self.activated);
            let results = guard
                .deferred
                .as_ref()
                .map(|deferred| deferred.search(query, usize::MAX))
                .unwrap_or_default();
            let names: Vec<String> = results
                .iter()
                .filter(|stub| guard.is_allowed(&stub.prefixed_name))
                .take(max_results)
                .map(|stub| stub.prefixed_name.clone())
                .collect();
            (results.len(), names)
        };
        if matched == 0 {
            return Ok(ToolResult::ok(crate::i18n::get_required_tool_string(
                "tool-mcp-search-no-matches",
            )));
        }
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.select_tools(&names)
    }
}

impl ToolSearchTool {
    fn select_tools(&self, names: &[&str]) -> anyhow::Result<ToolResult> {
        let mut output = String::from("<functions>\n");
        let mut not_found = Vec::new();
        for name in names.iter().copied().filter(|name| !name.is_empty()) {
            match ActivatedToolSet::activate_exact(&self.activated, name, &[]) {
                Some(tool) => {
                    let spec = tool.spec();
                    let _ = writeln!(
                        output,
                        "<function>{{\"name\": {}, \"description\": {}, \"parameters\": {}}}</function>",
                        serde_json::to_string(&spec.name)?,
                        serde_json::to_string(&spec.description)?,
                        spec.parameters,
                    );
                }
                None => not_found.push(name),
            }
        }
        output.push_str("</functions>\n");
        if !not_found.is_empty() {
            let _ = write!(
                output,
                "\n{}",
                crate::i18n::get_required_tool_string_with_args(
                    "tool-mcp-search-not-found",
                    &[("names", &not_found.join(", "))],
                )
            );
        }
        Ok(ToolResult::ok(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_client::McpRegistry;
    use crate::mcp_deferred::{
        DeferredMcpToolStub, build_deferred_tools_section, build_deferred_tools_section_filtered,
    };
    use crate::mcp_protocol::McpToolDef;

    async fn make_deferred_set(stubs: Vec<DeferredMcpToolStub>) -> DeferredMcpToolSet {
        let registry = Arc::new(McpRegistry::connect_all(&[]).await.unwrap());
        DeferredMcpToolSet { stubs, registry }
    }

    fn make_stub(name: &str, desc: &str) -> DeferredMcpToolStub {
        let def = McpToolDef {
            name: name.to_string(),
            description: Some(desc.to_string()),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        };
        DeferredMcpToolStub::new(name.to_string(), def)
    }

    fn assert_poisoned_activated_contains(
        activated: &Arc<Mutex<ActivatedToolSet>>,
        tool_name: &str,
    ) {
        let guard = activated
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(guard.is_activated(tool_name));
    }

    #[tokio::test]
    async fn tool_metadata() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        assert_eq!(tool.name(), "tool_search");
        assert!(!tool.description().is_empty());
        assert!(tool.parameters_schema()["properties"]["query"].is_object());
    }

    #[tokio::test]
    async fn empty_query_returns_error() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": ""}))
            .await
            .unwrap();
        assert!(!result.success);
    }

    #[tokio::test]
    async fn select_nonexistent_tool_reports_not_found() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": "select:nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Not found"));
    }

    #[tokio::test]
    async fn keyword_search_no_matches() {
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read file")]).await,
            Arc::new(Mutex::new(ActivatedToolSet::new())),
        );
        let result = tool
            .execute(serde_json::json!({"query": "zzzzz_nonexistent"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("No matching"));
    }

    #[tokio::test]
    async fn keyword_search_finds_match() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read a file from disk")]).await,
            Arc::clone(&activated),
        );
        let result = tool
            .execute(serde_json::json!({"query": "read file"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("fs__read"));
        // Tool should now be activated
        assert!(activated.lock().unwrap().is_activated("fs__read"));
    }

    #[tokio::test]
    async fn keyword_search_recovers_poisoned_activated_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("poison activated-tools lock");
        })
        .join();
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("fs__read", "Read a file from disk")]).await,
            Arc::clone(&activated),
        );

        let result = tool
            .execute(serde_json::json!({"query": "read file"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("fs__read"));
        assert_poisoned_activated_contains(&activated, "fs__read");
    }

    #[tokio::test]
    async fn multiple_servers_stubs_all_searchable() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("server_a__list_files", "List files on server A"),
            make_stub("server_a__read_file", "Read file on server A"),
            make_stub("server_b__query_db", "Query database on server B"),
            make_stub("server_b__insert_row", "Insert row on server B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        // Search should find tools across both servers
        let result = tool
            .execute(serde_json::json!({"query": "file"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("server_a__list_files"));
        assert!(result.output.contains("server_a__read_file"));

        // Server B tools should also be searchable
        let result = tool
            .execute(serde_json::json!({"query": "database query"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("server_b__query_db"));
    }

    #[tokio::test]
    async fn select_activates_and_persists_across_calls() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__tool_a", "Tool A"),
            make_stub("srv__tool_b", "Tool B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        // Activate tool_a
        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_a"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(activated.lock().unwrap().is_activated("srv__tool_a"));
        assert!(!activated.lock().unwrap().is_activated("srv__tool_b"));

        // Activate tool_b in a separate call
        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_b"}))
            .await
            .unwrap();
        assert!(result.success);

        // Both should remain activated
        let guard = activated.lock().unwrap();
        assert!(guard.is_activated("srv__tool_a"));
        assert!(guard.is_activated("srv__tool_b"));
        assert_eq!(guard.tool_specs().len(), 2);
    }

    #[tokio::test]
    async fn select_recovers_poisoned_activated_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("poison activated-tools lock");
        })
        .join();
        let stubs = vec![
            make_stub("srv__tool_a", "Tool A"),
            make_stub("srv__tool_b", "Tool B"),
        ];
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated));

        let result = tool
            .execute(serde_json::json!({"query": "select:srv__tool_a"}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("<function>"));
        assert!(result.output.contains("srv__tool_a"));
        assert_poisoned_activated_contains(&activated, "srv__tool_a");
    }

    #[tokio::test]
    async fn reactivation_is_idempotent() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        );

        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();
        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();

        assert_eq!(activated.lock().unwrap().tool_specs().len(), 1);
    }

    #[tokio::test]
    async fn activation_hook_receives_newly_activated_tools_once() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_hook = Arc::clone(&seen);
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        )
        .with_activation_hook(Arc::new(move |tool| {
            seen_hook.lock().unwrap().push(tool.name().to_string());
        }));

        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();
        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();

        assert_eq!(seen.lock().unwrap().as_slice(), ["srv__tool"]);
    }

    #[tokio::test]
    async fn exact_activation_and_search_share_one_catalog_and_hook() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let seen = Arc::new(AtomicUsize::new(0));
        let hook_seen = Arc::clone(&seen);
        let hook_activated = Arc::downgrade(&activated);
        let tool = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        )
        .with_activation_hook(Arc::new(move |tool| {
            // Re-entry proves the callback does not hold the activation lock.
            assert!(
                hook_activated
                    .upgrade()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .is_activated(tool.name())
            );
            hook_seen.fetch_add(1, Ordering::SeqCst);
        }));
        for name in ["tool", "SRV__TOOL", "srv__missing"] {
            assert!(ActivatedToolSet::activate_exact(&activated, name, &[]).is_none());
        }
        assert!(activated.lock().unwrap().tool_specs().is_empty());
        assert!(ActivatedToolSet::activate_exact(&activated, "srv__tool", &[]).is_some());
        tool.execute(serde_json::json!({"query": "select:srv__tool"}))
            .await
            .unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(activated.lock().unwrap().tool_specs().len(), 1);
    }

    #[tokio::test]
    async fn exact_activation_rechecks_policy_exclusions_and_catalog_membership() {
        let policies = [
            ToolAccessPolicy {
                denied: Some(vec!["srv__tool".into()]),
                ..ToolAccessPolicy::default()
            },
            ToolAccessPolicy {
                allowed: Some(vec![]),
                ..ToolAccessPolicy::default()
            },
            ToolAccessPolicy {
                caller_allowed: Some(vec![]),
                ..ToolAccessPolicy::default()
            },
            ToolAccessPolicy {
                allowed: Some(vec!["other".into()]),
                ..ToolAccessPolicy::default()
            },
            ToolAccessPolicy {
                allowed: Some(vec!["srv__tool".into()]),
                denied: Some(vec!["srv__tool".into()]),
                ..ToolAccessPolicy::default()
            },
        ];
        for policy in policies {
            let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
            let search = ToolSearchTool::new(
                make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
                Arc::clone(&activated),
            )
            .with_access_policy(policy);
            assert!(ActivatedToolSet::activate_exact(&activated, "srv__tool", &[]).is_none());
            let result = search
                .execute(serde_json::json!({"query": "select:srv__tool"}))
                .await
                .unwrap();
            assert!(!result.output.contains("<function>"));
            let guard = activated.lock().unwrap();
            assert!(guard.tool_specs().is_empty());
            assert_eq!(guard.deferred_tool_names().count(), 0);
        }
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let _search = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        )
        .with_access_policy(ToolAccessPolicy {
            allowed: Some(vec!["srv__tool".into(), "srv__unavailable".into()]),
            ..ToolAccessPolicy::default()
        });
        assert!(
            ActivatedToolSet::activate_exact(&activated, "srv__tool", &[" SRV__TOOL ".into()])
                .is_none()
        );
        assert!(ActivatedToolSet::activate_exact(&activated, "srv__unavailable", &[]).is_none());
        assert!(activated.lock().unwrap().tool_specs().is_empty());
    }

    #[tokio::test]
    async fn parallel_exact_activation_inserts_one_schema_and_notifies_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let seen = Arc::new(AtomicUsize::new(0));
        let hook_seen = Arc::clone(&seen);
        let _search = ToolSearchTool::new(
            make_deferred_set(vec![make_stub("srv__tool", "A tool")]).await,
            Arc::clone(&activated),
        )
        .with_activation_hook(Arc::new(move |_| {
            hook_seen.fetch_add(1, Ordering::SeqCst);
        }));
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let activated = Arc::clone(&activated);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    ActivatedToolSet::activate_exact(&activated, "srv__tool", &[]).unwrap()
                })
            })
            .collect();
        let tools: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(tools.iter().all(|tool| Arc::ptr_eq(tool, &tools[0])));
        assert_eq!(seen.load(Ordering::SeqCst), 1);
        assert_eq!(activated.lock().unwrap().tool_specs().len(), 1);
    }

    #[test]
    fn policy_none_is_unrestricted() {
        let p = ToolAccessPolicy::default();
        assert!(p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("anything"));
    }

    #[test]
    fn policy_allowlist_admits_only_listed() {
        let p = ToolAccessPolicy {
            allowed: Some(vec!["shell".into(), "file_read".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(p.is_tool_allowed("shell"));
        assert!(!p.is_tool_allowed("file_write"));
    }

    #[test]
    fn policy_denylist_rejects_listed() {
        let p = ToolAccessPolicy {
            denied: Some(vec!["shell".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(!p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("file_read"));
    }

    #[test]
    fn policy_deny_overrides_allow() {
        let p = ToolAccessPolicy {
            allowed: Some(vec!["shell".into(), "file_read".into()]),
            denied: Some(vec!["shell".into()]),
            ..ToolAccessPolicy::default()
        };
        assert!(!p.is_tool_allowed("shell"));
        assert!(p.is_tool_allowed("file_read"));
    }

    #[tokio::test]
    async fn policy_filters_keyword_search_results() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__allowed_tool", "An allowed tool"),
            make_stub("srv__blocked_tool", "A blocked tool"),
        ];
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__blocked_tool".into()]),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__allowed_tool"));
        assert!(!result.output.contains("srv__blocked_tool"));
        assert!(!activated.lock().unwrap().is_activated("srv__blocked_tool"));
    }

    #[tokio::test]
    async fn policy_denied_tool_does_not_consume_max_results_slot() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        // "denied_tool" ranks higher (more keyword matches) but is blocked.
        // "allowed_tool" ranks lower but should still be returned with max_results=1.
        let stubs = vec![
            make_stub("srv__denied_tool", "tool for searching files"),
            make_stub("srv__allowed_tool", "tool for files"),
        ];
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__denied_tool".into()]),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "searching files", "max_results": 1}))
            .await
            .unwrap();
        assert!(result.success);
        // The allowed tool should be returned even though max_results=1
        // and the denied tool ranked higher.
        assert!(result.output.contains("srv__allowed_tool"));
        assert!(!result.output.contains("srv__denied_tool"));
        assert!(activated.lock().unwrap().is_activated("srv__allowed_tool"));
    }

    #[tokio::test]
    async fn policy_filters_select_results() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let stubs = vec![
            make_stub("srv__ok", "OK tool"),
            make_stub("srv__nope", "Blocked tool"),
        ];
        // Runtime-discovered MCP tools (names containing "__") are auto-admitted
        // when an allow-list is present, so the operator-visible way to block a
        // specific MCP tool is the deny-list (the `excluded_tools` equivalent).
        // See `ToolAccessPolicy::is_tool_allowed` and
        let policy = ToolAccessPolicy {
            allowed: Some(vec!["srv__ok".into()]),
            denied: Some(vec!["srv__nope".into()]),
            ..ToolAccessPolicy::default()
        };
        let tool = ToolSearchTool::new(make_deferred_set(stubs).await, Arc::clone(&activated))
            .with_access_policy(policy);

        let result = tool
            .execute(serde_json::json!({"query": "select:srv__ok,srv__nope"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__ok"));
        assert!(!result.output.contains("\"name\": \"srv__nope\""));
        assert!(result.output.contains("Not found"));
        assert!(!activated.lock().unwrap().is_activated("srv__nope"));
    }

    #[tokio::test]
    async fn prompt_section_and_tool_search_see_same_filtered_set() {
        // The prompt-side
        // `build_deferred_tools_section_filtered` and the runtime
        // `ToolSearchTool` constructor (built from the same
        // `filtered_deferred` value, the single source of
        // truth) must agree on which tools are visible. If the
        // prompt and the search tool each applied the access policy
        // independently, a denied tool could leak through one of them.
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let set = make_deferred_set(vec![
            make_stub("srv__visible", "Visible tool"),
            make_stub("srv__hidden", "Hidden tool"),
        ])
        .await;
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__hidden".into()]),
            ..ToolAccessPolicy::default()
        };

        // Single source of truth: build the filtered set once and
        // feed it to both consumers.
        let filtered = set.filter_by_policy(Some(&policy));

        // Prompt-side: the section must not mention the denied tool.
        let section = build_deferred_tools_section_filtered(&filtered, Some(&policy));
        assert!(section.contains("srv__visible"));
        assert!(!section.contains("srv__hidden"));

        // Runtime-side: the search tool must not return the denied
        // tool's schema on a keyword match.
        let tool = ToolSearchTool::new(filtered, Arc::clone(&activated)).with_access_policy(policy);
        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__visible"));
        assert!(!result.output.contains("srv__hidden"));
        assert!(!activated.lock().unwrap().is_activated("srv__hidden"));
    }

    #[tokio::test]
    async fn omission_safety_pre_filtered_set_suffices_without_reapplied_policy() {
        // The `filter_by_policy` helper alone (the single source of
        // truth) is sufficient to keep a denied tool schema out of
        // both consumers, even when the policy is NOT reapplied at the
        // consumer level. A missed `with_access_policy` builder step
        // on `ToolSearchTool` is therefore not load-bearing.
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let set = make_deferred_set(vec![
            make_stub("srv__visible", "Visible tool"),
            make_stub("srv__hidden", "Hidden tool"),
        ])
        .await;
        let policy = ToolAccessPolicy {
            denied: Some(vec!["srv__hidden".into()]),
            ..ToolAccessPolicy::default()
        };

        // Single source of truth: build the filtered set once.
        let filtered = set.filter_by_policy(Some(&policy));

        // Prompt-side: use the UNFILTERED prompt builder — the pre-filtered
        // set alone must keep the denied tool out. No policy re-applied.
        let section = build_deferred_tools_section(&filtered);
        assert!(section.contains("srv__visible"));
        assert!(!section.contains("srv__hidden"));

        // Runtime-side: construct ToolSearchTool WITHOUT with_access_policy.
        // The pre-filtered stub set alone must keep the denied schema out.
        let tool = ToolSearchTool::new(filtered, Arc::clone(&activated));
        let result = tool
            .execute(serde_json::json!({"query": "tool"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("srv__visible"));
        assert!(!result.output.contains("srv__hidden"));
        assert!(!activated.lock().unwrap().is_activated("srv__hidden"));
    }

    #[test]
    fn caller_allowed_per_run_gate_does_not_auto_admit_mcp_names() {
        // The risk-profile gate is wide (unrestricted), so the MCP
        // auto-admit would happily pass any `__` name. The caller-supplied
        // per-run list narrows down to a single non-MCP tool (`cron_add`).
        let policy = ToolAccessPolicy::from_security(
            None,
            None,
            Some(&["cron_add".to_string()]),
            McpDiscoveredToolPolicy::AutoAdmit,
        )
        .expect("caller-supplied list should produce a policy");

        assert!(
            policy.is_tool_allowed("cron_add"),
            "cron_add must pass — it is in the caller list"
        );
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "MCP wrapper outside the caller list must be rejected, but \
             was admitted — the per-run gate is leaking the risk-profile \
             MCP auto-admit exception (PR #7547 review regression)"
        );
        assert!(
            !policy.is_tool_allowed("github__search"),
            "second MCP wrapper outside the caller list must also be \
             rejected (PR #7547 review regression)"
        );
    }

    #[test]
    fn caller_allowed_per_run_gate_narrows_after_risk_profile_auto_admit() {
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            None,
            Some(&["shell".to_string(), "github__search".to_string()]),
            McpDiscoveredToolPolicy::AutoAdmit,
        )
        .expect("risk + caller lists should produce a policy");

        // `shell`: in risk allow + in caller list → admitted.
        assert!(policy.is_tool_allowed("shell"));
        // `github__search`: auto-admitted by risk MCP exception + in caller
        // list → admitted.
        assert!(policy.is_tool_allowed("github__search"));
        // `filesystem__write_file`: auto-admitted by risk MCP exception
        // (would pass the risk gate) but NOT in caller list → rejected.
        // This is the per-run narrowing the bug used to break.
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "MCP wrapper not in caller list must be rejected even when \
             the risk-profile auto-admit would let it through"
        );
        // Non-MCP outside both lists: rejected.
        assert!(!policy.is_tool_allowed("memory_recall"));
    }

    #[test]
    fn caller_allowed_per_run_gate_still_honors_denylist() {
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            Some(&["filesystem__write_file".to_string()]),
            Some(&["shell".to_string(), "filesystem__write_file".to_string()]),
            McpDiscoveredToolPolicy::AutoAdmit,
        )
        .expect("policy with all three fields should be constructed");

        assert!(policy.is_tool_allowed("shell"));
        assert!(
            !policy.is_tool_allowed("filesystem__write_file"),
            "denylist subtracts even when both gates would admit"
        );
    }

    // ── explicit_only: an MCP server cannot grant itself reach ──

    /// The default posture. A non-empty allow-list means what it says: an
    /// MCP tool nobody listed is not reachable, even though its name has
    /// the `<server>__<tool>` shape that used to be a free pass.
    #[test]
    fn explicit_only_rejects_unlisted_mcp_names() {
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string(), "hapi-edge__snapshot".to_string()]),
            None,
            None,
            McpDiscoveredToolPolicy::ExplicitOnly,
        )
        .expect("risk list should produce a policy");

        assert!(policy.is_tool_allowed("shell"));
        assert!(
            policy.is_tool_allowed("hapi-edge__snapshot"),
            "an MCP tool named in the allow-list must still be reachable"
        );
        assert!(
            !policy.is_tool_allowed("hapi-edge__portfolio_buy"),
            "an unlisted MCP tool must be rejected — this is the whole point \
             of explicit_only: a server that grows a trading tool gains no \
             reach until someone names it"
        );
    }

    /// Same inputs, opposite policy — proves the gate is the flag and not
    /// some other filter incidentally rejecting the name.
    #[test]
    fn auto_admit_still_admits_unlisted_mcp_names() {
        let policy = ToolAccessPolicy::from_security(
            Some(&["shell".to_string()]),
            None,
            None,
            McpDiscoveredToolPolicy::AutoAdmit,
        )
        .expect("risk list should produce a policy");

        assert!(policy.is_tool_allowed("hapi-edge__portfolio_buy"));
    }

    /// Explicit-only does not weaken the empty-list rule: `Some(vec![])`
    /// still denies everything, MCP-shaped or not.
    #[test]
    fn explicit_only_keeps_empty_list_as_deny_all() {
        let policy = ToolAccessPolicy::from_security(
            Some(&[]),
            None,
            None,
            McpDiscoveredToolPolicy::ExplicitOnly,
        )
        .expect("empty risk list should still produce a policy");

        assert!(!policy.is_tool_allowed("shell"));
        assert!(!policy.is_tool_allowed("hapi-edge__snapshot"));
    }
}
