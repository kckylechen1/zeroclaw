//! Exercise native/text admission, preparation and execution against a real
//! in-process MCP HTTP peer and scripted model stream. No external process,
//! provider service or credentials.

use super::{ToolDispatchContext, ToolExecutionOutcome, execute_one_tool};
use crate::agent::turn::call_prep::prepare_tool_calls;
use crate::agent::turn::context::TurnCtx;
use crate::agent::turn::parse_response::interpret_chat_response;
use crate::agent::turn::tool_specs::build_iteration_tool_specs;
use crate::approval::ApprovalManager;
use crate::hooks::{HookHandler, HookResult, HookRunner};
use crate::observability::NoopObserver;
use crate::tools::{ActivatedToolSet, DeferredMcpToolSet, McpRegistry, Tool, ToolSearchTool};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};
use zeroclaw_config::schema::{McpServerConfig, McpTransport, PacingConfig, RiskProfileConfig};
use zeroclaw_providers::{ChatResponse, ModelProvider, ToolCall};
use zeroclaw_tools::tool_search::ToolAccessPolicy;

struct Fixture {
    server: MockServer,
    tools: Vec<Box<dyn Tool>>,
    activated: Arc<Mutex<ActivatedToolSet>>,
}

impl Fixture {
    async fn new(policy: ToolAccessPolicy) -> Self {
        let server = MockServer::start().await;
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
                        {"name": "read", "description": "Read fixture data", "inputSchema": {
                            "type": "object", "properties": {
                                "query": {"type": "string"}, "limit": {"type": "integer"}
                            }, "required": ["query"]
                        }},
                        {"name": "reject", "description": "Reject fixture arguments", "inputSchema": {
                            "type": "object", "properties": {"query": {"type": "string", "enum": ["allowed"]}},
                            "required": ["query"]
                        }},
                        {"name": "fail", "description": "Fail fixture execution", "inputSchema": {"type": "object"}}
                    ]}),
                    "tools/call" if request["params"]["name"] == "reject" => {
                        return ResponseTemplate::new(200).set_body_json(json!({
                            "jsonrpc": "2.0", "id": request["id"], "error": {
                                "code": -32602, "message": "rejected private-server-echo", "data": request["params"]["arguments"]
                            }
                        }));
                    }
                    "tools/call" if request["params"]["name"] == "fail" => json!({
                        "isError": true, "content": [{"type": "text", "text": "fixture execution failed"}]
                    }),
                    "tools/call" => {
                        assert!(request["params"]["arguments"].get("approved").is_none());
                        json!({"content": [{"type": "text", "text": "fixture result"}]})
                    }
                    other => panic!("unexpected MCP method: {other}"),
                };
                ResponseTemplate::new(200).set_body_json(json!({
                    "jsonrpc": "2.0", "id": request["id"], "result": result
                }))
            })
            .mount(&server)
            .await;
        let registry = Arc::new(
            McpRegistry::connect_all(&[McpServerConfig {
                name: "fixture".into(),
                transport: McpTransport::Http,
                url: Some(server.uri()),
                ..McpServerConfig::default()
            }])
            .await
            .unwrap(),
        );
        assert_eq!(registry.tool_count(), 3);
        let deferred = DeferredMcpToolSet::from_registry(registry).await;
        let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
        let search =
            ToolSearchTool::new(deferred, Arc::clone(&activated)).with_access_policy(policy);
        Self {
            server,
            tools: vec![Box::new(search)],
            activated,
        }
    }

    async fn rpc_calls(&self) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| {
                serde_json::from_slice::<Value>(&request.body).unwrap()["method"] == "tools/call"
            })
            .count()
    }

    async fn invoke(
        &self,
        native: bool,
        name: &str,
        args: Value,
        excluded: &[String],
        approval: Option<&ApprovalManager>,
        hooks: Option<&HookRunner>,
    ) -> Option<ToolExecutionOutcome> {
        let specs =
            build_iteration_tool_specs(&Provider, &self.tools, excluded, Some(&self.activated))
                .unwrap();
        let response = if native {
            ChatResponse {
                text: None,
                tool_calls: vec![ToolCall {
                    id: "call-mcp".into(),
                    name: name.into(),
                    arguments: args.to_string(),
                    extra_content: None,
                }],
                usage: None,
                reasoning_content: None,
            }
        } else {
            ChatResponse {
                text: Some(format!(
                    "<tool_call>{}</tool_call>",
                    json!({"name": name, "arguments": args})
                )),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            }
        };
        let pacing = PacingConfig::default();
        let ctx = TurnCtx {
            observer: &NoopObserver,
            provider_name: "fixture",
            model: "fixture",
            temperature: None,
            approval,
            channel_name: "test",
            channel_reply_target: None,
            cancellation_token: None,
            on_delta: None,
            event_tx: None,
            hooks,
            dedup_exempt_tools: &[],
            pacing: &pacing,
            strict_tool_parsing: false,
            channel: None,
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: "mcp-recovery",
        };
        let parsed =
            interpret_chat_response(&ctx, response, &specs, false, Instant::now(), 0, false).await;
        let prepared = prepare_tool_calls(
            &ctx,
            &parsed.tool_calls,
            ToolDispatchContext {
                tools_registry: &self.tools,
                activated_tools: Some(&self.activated),
                excluded_tools: excluded,
                model_switch_callback: None,
            },
            &mut HashSet::new(),
            &mut HashSet::new(),
            0,
            false,
        )
        .await
        .unwrap();
        if let Some(call) = prepared.executable_calls.first() {
            Some(
                execute_one_tool(
                    &call.name,
                    call.arguments.clone(),
                    call.tool_call_id.as_deref(),
                    ToolDispatchContext {
                        tools_registry: &self.tools,
                        activated_tools: Some(&self.activated),
                        excluded_tools: excluded,
                        model_switch_callback: None,
                    },
                    &ctx.meta(),
                    &NoopObserver,
                    None,
                    None,
                    None,
                )
                .await
                .unwrap(),
            )
        } else {
            prepared
                .ordered_results
                .into_iter()
                .flatten()
                .map(|(_, _, outcome)| outcome)
                .next()
        }
    }
}

struct Provider;

impl zeroclaw_api::attribution::Attributable for Provider {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::System
    }

    fn alias(&self) -> &str {
        "mcp-recovery-provider"
    }
}

#[async_trait]
impl ModelProvider for Provider {
    async fn chat_with_system(
        &self,
        _: Option<&str>,
        _: &str,
        _: &str,
        _: Option<f64>,
    ) -> anyhow::Result<String> {
        unreachable!("no model request is needed to test the dispatch boundary")
    }
}

struct DeferredStreamProvider {
    responses: Vec<String>,
    stream_calls: Arc<AtomicUsize>,
}

impl zeroclaw_api::attribution::Attributable for DeferredStreamProvider {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::System
    }

    fn alias(&self) -> &str {
        "mcp-recovery-stream-provider"
    }
}

#[async_trait]
impl ModelProvider for DeferredStreamProvider {
    fn capabilities(&self) -> zeroclaw_api::model_provider::ProviderCapabilities {
        zeroclaw_api::model_provider::ProviderCapabilities {
            native_tool_calling: true,
            ..Default::default()
        }
    }

    async fn chat_with_system(
        &self,
        _: Option<&str>,
        _: &str,
        _: &str,
        _: Option<f64>,
    ) -> anyhow::Result<String> {
        unreachable!("the streaming regression must consume the scripted stream")
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn supports_streaming_tool_events(&self) -> bool {
        true
    }

    fn stream_chat(
        &self,
        request: zeroclaw_providers::ChatRequest<'_>,
        _: &str,
        _: Option<f64>,
        _: zeroclaw_providers::traits::StreamOptions,
    ) -> futures_util::stream::BoxStream<
        'static,
        zeroclaw_providers::traits::StreamResult<zeroclaw_api::model_provider::StreamEvent>,
    > {
        use zeroclaw_api::model_provider::StreamEvent;
        use zeroclaw_providers::traits::StreamChunk;

        let turn = self.stream_calls.fetch_add(1, Ordering::SeqCst);
        if turn == 0 {
            assert!(
                !request
                    .tools
                    .unwrap_or_default()
                    .iter()
                    .any(|spec| spec.name == "fixture__read"),
                "the first protocol response must precede deferred schema activation"
            );
        }
        if turn > 0 && self.responses[turn - 1].starts_with("```json") {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.content.starts_with("[Tool call parse error]")),
                "a fenced envelope must request repair before dispatch"
            );
            assert!(
                !request
                    .tools
                    .unwrap_or_default()
                    .iter()
                    .any(|spec| spec.name == "fixture__read"),
                "the unparsed envelope must not activate the tool"
            );
        }
        let text = self
            .responses
            .get(turn)
            .expect("unexpected provider request after the scripted final reply");
        // These fixtures are ASCII; small chunks split both names and JSON keys.
        let mut events: Vec<_> = text
            .as_bytes()
            .chunks(7)
            .map(|chunk| {
                Ok(StreamEvent::TextDelta(StreamChunk::delta(
                    std::str::from_utf8(chunk).unwrap(),
                )))
            })
            .collect();
        events.push(Ok(StreamEvent::Final));
        Box::pin(futures_util::stream::iter(events))
    }
}

async fn assert_deferred_stream_is_hidden(responses: Vec<String>, expose_search_helper: bool) {
    use crate::agent::Agent;
    use crate::agent::dispatcher::NativeToolDispatcher;
    use zeroclaw_api::agent::TurnEvent;

    let protocol = responses[0].clone();
    let expected_requests = responses.len();
    let mut fixture = Fixture::new(ToolAccessPolicy {
        allowed: Some(vec!["fixture__read".into()]),
        ..ToolAccessPolicy::default()
    })
    .await;
    if !expose_search_helper {
        fixture.tools.clear();
    }
    assert!(fixture.activated.lock().unwrap().tool_specs().is_empty());
    let workspace = tempfile::tempdir().unwrap();
    let memory = zeroclaw_memory::create_memory(
        &zeroclaw_config::schema::MemoryConfig {
            backend: "none".into(),
            ..Default::default()
        },
        workspace.path(),
        None,
    )
    .unwrap();
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent::builder()
        .model_provider(Box::new(DeferredStreamProvider {
            responses,
            stream_calls: Arc::clone(&stream_calls),
        }))
        .tools(std::mem::take(&mut fixture.tools))
        .activated_tools(Some(Arc::clone(&fixture.activated)))
        .memory(Arc::from(memory))
        .observer(Arc::new(NoopObserver))
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(workspace.path().to_path_buf())
        .auto_save(false)
        .build()
        .unwrap();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(256);
    let (response, _) = agent
        .turn_streamed("read the fixture", event_tx, None)
        .await
        .unwrap();
    let mut visible = String::new();
    let mut dispatched = 0;
    while let Some(event) = event_rx.recv().await {
        match event {
            TurnEvent::Chunk { delta } => visible.push_str(&delta),
            TurnEvent::ToolCall { name, .. } if name == "fixture__read" => {
                dispatched += 1;
            }
            _ => {}
        }
    }
    assert_eq!(visible, "done", "protocol leaked into text: {protocol}");
    assert_eq!(response, "done");
    assert_eq!(dispatched, 1);
    assert_eq!(fixture.rpc_calls().await, 1);
    assert_eq!(stream_calls.load(Ordering::SeqCst), expected_requests);
    assert_eq!(fixture.activated.lock().unwrap().tool_specs().len(), 1);
}

#[tokio::test]
async fn streamed_deferred_json_executes_without_leaking_protocol_or_arguments_into_chunks() {
    let tool_call = json!({
        "name": "fixture__read",
        "arguments": {"query": "private-argument-never-a-text-chunk"}
    });
    for protocol in [
        tool_call.to_string(),
        json!({"tool_calls": [tool_call]}).to_string(),
    ] {
        for expose_search_helper in [true, false] {
            assert_deferred_stream_is_hidden(
                vec![protocol.clone(), "done".into()],
                expose_search_helper,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn streamed_deferred_fenced_envelope_requires_repair_without_leaking_text() {
    let tool_call = json!({
        "name": "fixture__read",
        "arguments": {"query": "private-argument-never-a-text-chunk"}
    });
    // A bare fenced {name, arguments} object is an ordinary code example in
    // the existing parser, not an executable format. A fenced tool_calls
    // envelope is withheld as protocol and requests a corrected response;
    // only the following unfenced JSON may execute. Do not widen parser syntax.
    let envelope = json!({"tool_calls": [tool_call.clone()]});
    for expose_search_helper in [true, false] {
        assert_deferred_stream_is_hidden(
            vec![
                format!("```json\n{envelope}\n```"),
                tool_call.to_string(),
                "done".into(),
            ],
            expose_search_helper,
        )
        .await;
    }
}

#[tokio::test]
async fn native_and_text_calls_activate_exact_deferred_names_without_search() {
    for native in [true, false] {
        let mut fixture = Fixture::new(ToolAccessPolicy {
            allowed: Some(vec!["fixture__read".into()]),
            ..ToolAccessPolicy::default()
        })
        .await;
        // The caller can admit the MCP tool without admitting tool_search.
        if !native {
            fixture.tools.clear();
        }
        let specs =
            build_iteration_tool_specs(&Provider, &fixture.tools, &[], Some(&fixture.activated))
                .unwrap();
        assert!(specs.known_tool_names.contains("fixture__read"));
        assert!(
            !specs
                .tool_specs
                .iter()
                .any(|spec| spec.name == "fixture__read")
        );
        assert!(fixture.activated.lock().unwrap().tool_specs().is_empty());
        let result = fixture
            .invoke(
                native,
                "fixture__read",
                json!({"query": "fixture", "approved": true}),
                &[],
                None,
                None,
            )
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert_eq!(fixture.rpc_calls().await, 1);
        let specs =
            build_iteration_tool_specs(&Provider, &fixture.tools, &[], Some(&fixture.activated))
                .unwrap();
        assert_eq!(
            specs
                .tool_specs
                .iter()
                .filter(|spec| spec.name == "fixture__read")
                .count(),
            1
        );
        let excluded = [" FIXTURE__READ ".into()];
        let specs = build_iteration_tool_specs(
            &Provider,
            &fixture.tools,
            &excluded,
            Some(&fixture.activated),
        )
        .unwrap();
        assert!(
            !specs
                .tool_specs
                .iter()
                .any(|spec| spec.name == "fixture__read")
        );
        assert!(!specs.known_tool_names.contains("fixture__read"));
    }
}

#[tokio::test]
async fn preapproval_preflight_reads_deferred_admission_without_activation() {
    for (policy, expected_unknown) in [
        (ToolAccessPolicy::default(), false),
        (
            ToolAccessPolicy {
                denied: Some(vec!["fixture__read".into()]),
                ..Default::default()
            },
            true,
        ),
        (
            ToolAccessPolicy {
                allowed: Some(vec![]),
                ..Default::default()
            },
            true,
        ),
        (
            ToolAccessPolicy {
                caller_allowed: Some(vec![]),
                ..Default::default()
            },
            true,
        ),
    ] {
        let fixture = Fixture::new(policy).await;
        let meta = crate::agent::turn::TurnMeta {
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: "preflight-deferred",
            channel_name: "fixture",
        };
        let outcome = super::undispatchable_tool_outcome(
            "fixture__read",
            &json!({"query": "fixture"}),
            Some("call-fixture"),
            ToolDispatchContext {
                tools_registry: &fixture.tools,
                activated_tools: Some(&fixture.activated),
                excluded_tools: &[],
                model_switch_callback: None,
            },
            false,
            &meta,
            &NoopObserver,
        );
        assert_eq!(outcome.is_some(), expected_unknown);
        if let Some(outcome) = outcome {
            assert_eq!(outcome.output, "Unknown tool: fixture__read");
        }
        assert!(fixture.activated.lock().unwrap().tool_specs().is_empty());
        assert_eq!(fixture.rpc_calls().await, 0);
    }
}

#[tokio::test]
async fn denied_empty_allow_excluded_and_unknown_calls_never_activate_or_dispatch() {
    let cases = [
        (
            ToolAccessPolicy {
                denied: Some(vec!["fixture__read".into()]),
                ..ToolAccessPolicy::default()
            },
            vec![],
            "fixture__read",
        ),
        (
            ToolAccessPolicy {
                allowed: Some(vec![]),
                ..ToolAccessPolicy::default()
            },
            vec![],
            "fixture__read",
        ),
        (
            ToolAccessPolicy {
                caller_allowed: Some(vec![]),
                ..ToolAccessPolicy::default()
            },
            vec![],
            "fixture__read",
        ),
        (
            ToolAccessPolicy::default(),
            vec![" FIXTURE__READ ".into()],
            "fixture__read",
        ),
        (ToolAccessPolicy::default(), vec![], "read"),
        (ToolAccessPolicy::default(), vec![], "FIXTURE__READ"),
        (ToolAccessPolicy::default(), vec![], "fixture__absent"),
    ];
    for (policy, excluded, name) in cases {
        let fixture = Fixture::new(policy).await;
        for native in [true, false] {
            let result = fixture
                .invoke(
                    native,
                    name,
                    json!({"query": "fixture"}),
                    &excluded,
                    None,
                    None,
                )
                .await;
            assert!(result.is_none_or(|outcome| !outcome.success), "{name}");
            assert!(
                fixture.activated.lock().unwrap().tool_specs().is_empty(),
                "{name}"
            );
        }
        assert_eq!(fixture.rpc_calls().await, 0, "{name}");
    }
}

struct CancelHook;
#[async_trait]
impl HookHandler for CancelHook {
    fn name(&self) -> &str {
        "cancel-mcp"
    }
    async fn before_tool_call(&self, _: String, _: Value) -> HookResult<(String, Value)> {
        HookResult::Cancel("fixture hook cancelled".into())
    }
}

struct RenameHook;
#[async_trait]
impl HookHandler for RenameHook {
    fn name(&self) -> &str {
        "rename-mcp"
    }
    async fn before_tool_call(&self, _: String, args: Value) -> HookResult<(String, Value)> {
        HookResult::Continue(("fixture__reject".into(), args))
    }
}

struct AliasHook;
#[async_trait]
impl HookHandler for AliasHook {
    fn name(&self) -> &str {
        "alias-mcp"
    }
    async fn before_tool_call(&self, _: String, args: Value) -> HookResult<(String, Value)> {
        HookResult::Continue(("read".into(), args))
    }
}

#[tokio::test]
async fn activated_suffix_and_hook_aliases_obey_canonical_approval_rules() {
    let fixture = Fixture::new(ToolAccessPolicy {
        allowed: Some(vec!["fixture__read".into()]),
        ..ToolAccessPolicy::default()
    })
    .await;
    assert!(ActivatedToolSet::activate_exact(&fixture.activated, "fixture__read", &[]).is_some());
    assert_eq!(fixture.rpc_calls().await, 0);
    let approval = ApprovalManager::for_non_interactive(&RiskProfileConfig {
        level: crate::security::AutonomyLevel::Full,
        always_ask: vec!["fixture__read".into()],
        auto_approve: vec!["*".into()],
        ..RiskProfileConfig::default()
    });
    assert!(approval.needs_approval("fixture__read"));
    assert!(!approval.needs_approval("read"));
    let mut hooks = HookRunner::new();
    hooks.register(Box::new(AliasHook));

    // Text fallback admits only exact known names. Its before-tool hook can
    // still rewrite that name to an existing suffix, so check that route too.
    for (native, name, rewrite) in [
        (true, "read", false),
        (true, "fixture__read", true),
        (false, "fixture__read", true),
    ] {
        let outcome = fixture
            .invoke(
                native,
                name,
                json!({"query": "fixture", "approved": true}),
                &[],
                Some(&approval),
                rewrite.then_some(&hooks),
            )
            .await
            .unwrap();
        assert!(!outcome.success);
        assert!(outcome.output.contains("requires approval"));
        assert!(outcome.output.contains("fixture__read"));
        assert_eq!(fixture.rpc_calls().await, 0);
        assert_eq!(fixture.activated.lock().unwrap().tool_specs().len(), 1);
    }
    let decisions = approval.audit_log();
    assert_eq!(decisions.len(), 3);
    assert!(decisions.iter().all(|decision| {
        decision.tool_name == "fixture__read"
            && decision.decision == crate::approval::ApprovalResponse::No
    }));

    // Alias support remains usable when the canonical policy permits execution.
    let unrestricted = ApprovalManager::for_non_interactive(&RiskProfileConfig {
        level: crate::security::AutonomyLevel::Full,
        always_ask: Vec::new(),
        ..RiskProfileConfig::default()
    });
    let outcome = fixture
        .invoke(
            true,
            "read",
            json!({"query": "fixture"}),
            &[],
            Some(&unrestricted),
            None,
        )
        .await
        .unwrap();
    assert!(outcome.success);
    assert_eq!(fixture.rpc_calls().await, 1);
}

#[tokio::test]
async fn hook_rewrites_cannot_activate_a_denied_tool() {
    let fixture = Fixture::new(ToolAccessPolicy {
        allowed: Some(vec!["fixture__read".into()]),
        ..ToolAccessPolicy::default()
    })
    .await;
    let mut hooks = HookRunner::new();
    hooks.register(Box::new(RenameHook));
    for native in [true, false] {
        let result = fixture
            .invoke(
                native,
                "fixture__read",
                json!({"query": "fixture"}),
                &[],
                None,
                Some(&hooks),
            )
            .await
            .unwrap();
        assert!(!result.success);
        assert!(fixture.activated.lock().unwrap().tool_specs().is_empty());
    }
    assert_eq!(fixture.rpc_calls().await, 0);
}

#[tokio::test]
async fn approval_or_hook_denial_leaves_no_deferred_activation() {
    let fixture = Fixture::new(ToolAccessPolicy::default()).await;
    let approval = ApprovalManager::for_non_interactive(&RiskProfileConfig {
        always_ask: vec!["fixture__read".into()],
        ..RiskProfileConfig::default()
    });
    let mut hooks = HookRunner::new();
    hooks.register(Box::new(CancelHook));
    for native in [true, false] {
        let denied = fixture
            .invoke(
                native,
                "fixture__read",
                json!({"query": "fixture", "approved": true}),
                &[],
                Some(&approval),
                None,
            )
            .await
            .unwrap();
        assert!(!denied.success);
        assert!(denied.output.contains("requires approval"));
        let cancelled = fixture
            .invoke(
                native,
                "fixture__read",
                json!({"query": "fixture"}),
                &[],
                None,
                Some(&hooks),
            )
            .await
            .unwrap();
        assert!(!cancelled.success);
        assert!(cancelled.output.contains("fixture hook cancelled"));
        assert!(fixture.activated.lock().unwrap().tool_specs().is_empty());
    }
    assert_eq!(fixture.rpc_calls().await, 0);
}

#[tokio::test]
async fn shape_error_can_be_corrected_without_exposing_values_or_replaying() {
    let fixture = Fixture::new(ToolAccessPolicy::default()).await;
    let invalid = fixture
        .invoke(
            true,
            "fixture__read",
            json!({"query": {"private-nested-key": "private-value"}, "limit": 1}),
            &[],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(!invalid.success);
    assert!(invalid.output.contains("query"));
    assert!(invalid.output.contains("string"));
    assert!(!invalid.output.contains("private-"));
    assert_eq!(fixture.rpc_calls().await, 0);
    let corrected = fixture
        .invoke(
            false,
            "fixture__read",
            json!({"query": "fixture", "limit": 1}),
            &[],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(corrected.success);
    assert_eq!(fixture.rpc_calls().await, 1);
}

#[tokio::test]
async fn remote_invalid_params_gets_a_hint_but_execution_errors_do_not() {
    let fixture = Fixture::new(ToolAccessPolicy::default()).await;
    let invalid = fixture
        .invoke(
            true,
            "fixture__reject",
            json!({"query": "private-input"}),
            &[],
            None,
            None,
        )
        .await
        .unwrap();
    assert!(!invalid.success);
    assert!(invalid.output.contains("tool_search"));
    assert!(!invalid.output.contains("private-input"));
    assert!(!invalid.output.contains("private-server-echo"));
    assert_eq!(
        fixture.rpc_calls().await,
        1,
        "an argument error must not be replayed"
    );
    let failed = fixture
        .invoke(false, "fixture__fail", json!({}), &[], None, None)
        .await
        .unwrap();
    assert!(!failed.success);
    assert!(failed.output.contains("fixture execution failed"));
    assert!(!failed.output.contains("Expected fields/types"));
    assert_eq!(fixture.rpc_calls().await, 2);
}
