//! Per-iteration tool-spec assembly for the turn engine.

use crate::tools::{ActivatedToolSet, Tool, ToolSpec};
use anyhow::Result;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use zeroclaw_providers::ModelProvider;

/// Tool specs assembled for one loop iteration.
pub(crate) struct IterationToolSpecs {
    pub(crate) tool_specs: Vec<ToolSpec>,
    pub(crate) known_tool_names: HashSet<String>,
    pub(crate) use_native_tools: bool,
}

impl IterationToolSpecs {
    pub(crate) fn refresh_native_tool_mode(&mut self, model_provider: &dyn ModelProvider) {
        self.use_native_tools =
            model_provider.supports_native_tools() && !self.tool_specs.is_empty();
    }
}

pub(crate) fn build_iteration_tool_specs(
    model_provider: &dyn ModelProvider,
    tools_registry: &[Box<dyn Tool>],
    excluded_tools: &[String],
    activated_tools: Option<&Arc<Mutex<ActivatedToolSet>>>,
) -> Result<IterationToolSpecs> {
    // Match the executor's exclusion semantics before exposing any schema/name.
    let is_excluded = |name: &str| {
        excluded_tools
            .iter()
            .any(|excluded| excluded.trim().eq_ignore_ascii_case(name.trim()))
    };
    // Rebuild tool_specs each iteration so newly activated deferred tools appear.
    let mut tool_specs: Vec<crate::tools::ToolSpec> = tools_registry
        .iter()
        .filter(|tool| !is_excluded(tool.name()))
        .map(|tool| tool.spec())
        .collect();
    let mut known_tool_names: HashSet<String> = tool_specs
        .iter()
        .map(|tool| tool.name.to_ascii_lowercase())
        .collect();
    if let Some(at) = activated_tools {
        let activated_tools = match at.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Tool)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "activated-tool lock poisoned while assembling iteration tool specs; recovering guard for read"
                );
                poisoned.into_inner()
            }
        };
        for spec in activated_tools.tool_specs() {
            if !is_excluded(&spec.name) {
                known_tool_names.insert(spec.name.to_ascii_lowercase());
                tool_specs.push(spec);
            }
        }
        // Text fallback must recognize authorized deferred names before they
        // have schemas. This is discovery only: the executor performs the same
        // exact catalog lookup after hooks/approval, and rechecks exclusions.
        known_tool_names.extend(
            activated_tools
                .deferred_tool_names()
                .filter(|name| !is_excluded(name))
                .map(str::to_ascii_lowercase),
        );
    }
    let use_native_tools = model_provider.supports_native_tools() && !tool_specs.is_empty();

    Ok(IterationToolSpecs {
        tool_specs,
        known_tool_names,
        use_native_tools,
    })
}

#[cfg(test)]
mod tests {
    use super::build_iteration_tool_specs;
    use crate::tools::ActivatedToolSet;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use zeroclaw_api::attribution::Role;
    use zeroclaw_api::model_provider::{ModelProvider, ProviderCapabilities};
    use zeroclaw_api::tool::Tool;

    struct NativeToolsProvider;

    #[async_trait]
    impl ModelProvider for NativeToolsProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: true,
                ..ProviderCapabilities::default()
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            unreachable!("test provider should not execute chat")
        }
    }

    impl zeroclaw_api::attribution::Attributable for NativeToolsProvider {
        fn role(&self) -> Role {
            Role::System
        }

        fn alias(&self) -> &str {
            "test-native-tools-provider"
        }
    }

    struct PromptToolsProvider;

    #[async_trait]
    impl ModelProvider for PromptToolsProvider {
        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                native_tool_calling: false,
                ..ProviderCapabilities::default()
            }
        }

        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            unreachable!("test provider should not execute chat")
        }
    }

    impl zeroclaw_api::attribution::Attributable for PromptToolsProvider {
        fn role(&self) -> Role {
            Role::System
        }

        fn alias(&self) -> &str {
            "test-prompt-tools-provider"
        }
    }

    struct CountingTool {
        name: String,
        invocations: Arc<AtomicUsize>,
    }

    impl CountingTool {
        fn new(name: &str, invocations: Arc<AtomicUsize>) -> Self {
            Self {
                name: name.to_string(),
                invocations,
            }
        }
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            &self.name
        }

        fn description(&self) -> &str {
            "Counts executions for poisoned-lock tests"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            })
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> anyhow::Result<crate::tools::ToolResult> {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            Ok(crate::tools::ToolResult {
                success: true,
                output: "counted".into(),
                error: None,
            })
        }
    }

    impl zeroclaw_api::attribution::Attributable for CountingTool {
        fn role(&self) -> Role {
            Role::Tool(zeroclaw_api::attribution::ToolKind::Plugin)
        }

        fn alias(&self) -> &str {
            self.name()
        }
    }

    #[test]
    fn build_iteration_tool_specs_recovers_poisoned_activated_tool_lock() {
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let invocations = Arc::new(AtomicUsize::new(0));
        let activated_tool: Arc<dyn Tool> = Arc::new(CountingTool::new(
            "docker-mcp__extract_text",
            Arc::clone(&invocations),
        ));
        activated
            .lock()
            .unwrap()
            .activate("docker-mcp__extract_text".into(), activated_tool);
        let poisoned = Arc::clone(&activated);
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.lock().expect("test mutex should lock");
            panic!("poison activated-tools lock");
        })
        .join();

        let specs = build_iteration_tool_specs(&NativeToolsProvider, &[], &[], Some(&activated))
            .expect("poisoned activated-tools lock should recover for read");
        assert!(
            specs
                .tool_specs
                .iter()
                .any(|spec| spec.name == "docker-mcp__extract_text"),
            "recovered poisoned lock should still expose activated tool specs"
        );
    }

    #[test]
    fn iteration_tool_specs_recomputes_native_mode_for_active_provider() {
        let invocations = Arc::new(AtomicUsize::new(0));
        let tool = Box::new(CountingTool::new("read_file", invocations));
        let mut specs = build_iteration_tool_specs(&NativeToolsProvider, &[tool], &[], None)
            .expect("native provider with tools should build specs");
        assert!(specs.use_native_tools);

        specs.refresh_native_tool_mode(&PromptToolsProvider);

        assert!(
            !specs.use_native_tools,
            "active provider must decide whether this turn uses native tool transport"
        );
    }

    #[tokio::test]
    async fn mcp_discovery_and_activation_order_keep_provider_payload_bytes_stable() {
        use crate::tools::{DeferredMcpToolSet, McpRegistry, ToolSearchTool};
        use serde_json::{Value, json};
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use zeroclaw_config::schema::{McpServerConfig, McpTransport};
        use zeroclaw_providers::compatible::{AuthStyle, OpenAiCompatibleModelProvider};
        use zeroclaw_providers::{ChatMessage, ChatRequest, ProviderDispatch};
        use zeroclaw_tools::mcp_deferred::build_deferred_tools_section_filtered;
        use zeroclaw_tools::tool_search::ToolAccessPolicy;

        let mcp = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &wiremock::Request| {
                let request: Value = serde_json::from_slice(&request.body).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "server/discover" => {
                        return ResponseTemplate::new(200).set_body_json(json!({
                            "jsonrpc": "2.0", "id": request["id"],
                            "error": {"code": -32601, "message": "Method not found"}
                        }));
                    }
                    "notifications/initialized" => return ResponseTemplate::new(202),
                    "initialize" => json!({
                        "protocolVersion": "2024-11-05", "capabilities": {"tools": {}}
                    }),
                    "tools/list" => json!({"tools": [
                        {"name": "write", "description": "Write fixture data", "inputSchema": {"type": "object"}},
                        {"name": "read", "description": "Read fixture data", "inputSchema": {"type": "object"}}
                    ]}),
                    other => panic!("discovery must not execute a tool: {other}"),
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0", "id": request["id"], "result": result
                }))
            })
            .mount(&mcp)
            .await;

        let endpoint = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"role": "assistant", "content": "done"}}]
            })))
            .expect(2)
            .mount(&endpoint)
            .await;
        let provider = OpenAiCompatibleModelProvider::builder("ordering-fixture")
            .display_name("Ordering fixture")
            .base_url(&format!("{}/v1", endpoint.uri()))
            .credential(Some("fixture-key"))
            .auth_style(AuthStyle::Bearer)
            .build();
        let policy = ToolAccessPolicy {
            denied: Some(vec!["zeta__write".into()]),
            ..ToolAccessPolicy::default()
        };
        let names = ["zeta__read", "alpha__write", "alpha__read", "zeta__write"];
        for reverse in [false, true] {
            let mut servers = vec!["zeta", "alpha"];
            let mut activation_order = names.to_vec();
            if reverse {
                servers.reverse();
                activation_order.reverse();
            }
            let configs: Vec<_> = servers
                .into_iter()
                .map(|name| McpServerConfig {
                    name: name.into(),
                    transport: McpTransport::Http,
                    url: Some(mcp.uri()),
                    ..McpServerConfig::default()
                })
                .collect();
            let registry = Arc::new(McpRegistry::connect_all(&configs).await.unwrap());
            assert_eq!(registry.tool_count(), names.len());
            let deferred = DeferredMcpToolSet::from_registry(registry).await;
            let system = build_deferred_tools_section_filtered(&deferred, Some(&policy));
            assert!(!system.contains("zeta__write"));
            let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
            let search = ToolSearchTool::new(deferred, Arc::clone(&activated))
                .with_access_policy(policy.clone());
            for name in activation_order {
                let tool = ActivatedToolSet::activate_exact(&activated, name, &[]);
                assert_eq!(tool.is_some(), name != "zeta__write");
            }
            {
                let set = activated.lock().unwrap();
                assert_eq!(
                    set.tool_names(),
                    vec!["alpha__read", "alpha__write", "zeta__read"]
                );
                assert!(set.get_resolved("read").is_none());
                assert_eq!(set.get_resolved("write").unwrap().name(), "alpha__write");
            }
            let tools: Vec<Box<dyn Tool>> = vec![Box::new(search)];
            let specs =
                build_iteration_tool_specs(&provider, &tools, &[], Some(&activated)).unwrap();
            let messages = vec![ChatMessage::system(system), ChatMessage::user("fixture")];
            ProviderDispatch::from_ref(&provider)
                .chat(
                    ChatRequest {
                        messages: &messages,
                        tools: Some(&specs.tool_specs),
                        thinking: None,
                    },
                    "fixture-model",
                    None,
                )
                .await
                .unwrap();
        }
        let requests = endpoint.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body, requests[1].body);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let sent_names: Vec<_> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            sent_names,
            vec!["tool_search", "alpha__read", "alpha__write", "zeta__read"]
        );
    }
}
