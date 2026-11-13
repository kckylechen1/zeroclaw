use super::*;
use crate::approval::ApprovalManager;
use crate::hooks::{HookHandler, HookResult, HookRunner};
use crate::observability::{Observer, ObserverEvent};
use crate::tools::{ActivatedToolSet, Tool, ToolResult};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use zeroclaw_api::agent::TurnEvent;
use zeroclaw_api::attribution::{Attributable, Role};
use zeroclaw_api::channel::{
    Channel, ChannelApprovalRequest, ChannelApprovalResponse, ChannelMessage, SendMessage,
};
use zeroclaw_config::schema::{PacingConfig, RiskProfileConfig};

struct NamedTool(&'static str);
impl Attributable for NamedTool {
    fn role(&self) -> Role {
        Role::System
    }
    fn alias(&self) -> &str {
        "preflight-fixture"
    }
}
#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "Preflight fixture"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _: Value) -> Result<ToolResult> {
        panic!("preparation must not execute a tool")
    }
}

#[derive(Default)]
struct DenyingChannel(AtomicUsize);
impl Attributable for DenyingChannel {
    fn role(&self) -> Role {
        Role::System
    }
    fn alias(&self) -> &str {
        "preflight-approval-fixture"
    }
}
#[async_trait]
impl Channel for DenyingChannel {
    fn name(&self) -> &str {
        "preflight-approval-fixture"
    }
    async fn send(&self, _: &SendMessage) -> Result<()> {
        Ok(())
    }
    async fn listen(&self, _: tokio::sync::mpsc::Sender<ChannelMessage>) -> Result<()> {
        Ok(())
    }
    async fn request_approval(
        &self,
        _: &str,
        _: &ChannelApprovalRequest,
    ) -> Result<Option<ChannelApprovalResponse>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Some(ChannelApprovalResponse::Deny))
    }
}

#[derive(Default)]
struct RecordingObserver(Mutex<Vec<String>>);
impl Observer for RecordingObserver {
    fn record_event(&self, event: &ObserverEvent) {
        let text = match event {
            ObserverEvent::ToolCallStart {
                tool,
                tool_call_id,
                arguments,
                ..
            } => format!("start:{tool}:{tool_call_id:?}:{arguments:?}"),
            ObserverEvent::ToolCall {
                tool,
                tool_call_id,
                arguments,
                result,
                ..
            } => format!("result:{tool}:{tool_call_id:?}:{arguments:?}:{result:?}"),
            _ => return,
        };
        self.0.lock().unwrap().push(text);
    }
    fn record_metric(&self, _: &zeroclaw_api::observability_traits::ObserverMetric) {}
    fn name(&self) -> &str {
        "preflight-observer"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

fn call(name: &str) -> ParsedToolCall {
    ParsedToolCall {
        name: name.into(),
        arguments: json!({"api_key": {"nested": "private-fixture-value"}}),
        tool_call_id: Some(format!("call-{name}")),
    }
}

struct Preparation {
    prepared: PreparedToolCalls,
    prompts: usize,
    observed: Vec<String>,
    events: Vec<TurnEvent>,
}

async fn prepare(
    calls: &[ParsedToolCall],
    dispatch: ToolDispatchContext<'_>,
    hooks: Option<&HookRunner>,
) -> Preparation {
    let observer = RecordingObserver::default();
    let channel = DenyingChannel::default();
    let approval = ApprovalManager::for_non_interactive(&RiskProfileConfig {
        auto_approve: vec!["tool_search".into()],
        always_ask: vec![],
        ..Default::default()
    });
    let pacing = PacingConfig::default();
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let ctx = TurnCtx {
        observer: &observer,
        provider_name: "fixture",
        model: "fixture",
        temperature: None,
        approval: Some(&approval),
        channel_name: "fixture",
        channel_reply_target: None,
        cancellation_token: None,
        on_delta: None,
        event_tx: Some(&tx),
        hooks,
        dedup_exempt_tools: &[],
        pacing: &pacing,
        strict_tool_parsing: false,
        channel: Some(&channel),
        turn_id: "preflight-turn",
        agent_alias: None,
        parent_agent_alias: None,
    };
    let prepared = prepare_tool_calls(
        &ctx,
        calls,
        dispatch,
        &mut HashSet::new(),
        &mut HashSet::new(),
        0,
        true,
    )
    .await
    .unwrap();
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let observed = observer.0.lock().unwrap().clone();
    Preparation {
        prepared,
        prompts: channel.0.load(Ordering::SeqCst),
        observed,
        events,
    }
}

fn outcome(run: &Preparation, index: usize) -> &ToolExecutionOutcome {
    &run.prepared.ordered_results[index].as_ref().unwrap().2
}

#[tokio::test]
async fn unavailable_calls_skip_approval_and_preserve_executor_evidence() {
    let registry: Vec<Box<dyn Tool>> = vec![Box::new(NamedTool("available"))];
    for (name, excluded, expected) in [
        ("absent", vec![], "Unknown tool: absent"),
        (
            "available",
            vec![" AVAILABLE ".into()],
            "Tool not available in this turn: available",
        ),
    ] {
        let call = call(name);
        let dispatch = ToolDispatchContext {
            tools_registry: &registry,
            activated_tools: None,
            excluded_tools: &excluded,
            model_switch_callback: None,
        };
        let run = prepare(std::slice::from_ref(&call), dispatch, None).await;
        assert_eq!(run.prompts, 0);
        assert!(run.prepared.executable_calls.is_empty());
        assert_eq!(outcome(&run, 0).output, expected);
        assert!(!outcome(&run, 0).success);
        assert!(outcome(&run, 0).receipt.is_none());
        let observer = RecordingObserver::default();
        let meta = crate::agent::turn::TurnMeta {
            agent_alias: None,
            parent_agent_alias: None,
            turn_id: "preflight-turn",
            channel_name: "fixture",
        };
        let actual = crate::agent::tool_execution::execute_one_tool(
            name,
            call.arguments.clone(),
            call.tool_call_id.as_deref(),
            dispatch,
            &meta,
            &observer,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(actual.output, outcome(&run, 0).output);
        assert_eq!(run.observed, *observer.0.lock().unwrap());
        assert!(!format!("{:?}", run.observed).contains("private-fixture-value"));
        match run.events.as_slice() {
            [
                TurnEvent::ToolCall {
                    id,
                    name: event_name,
                    args,
                },
                TurnEvent::ToolResult {
                    id: result_id,
                    name: result_name,
                    output,
                },
            ] => {
                assert_eq!(id, result_id);
                assert_eq!(event_name, result_name);
                assert_eq!(event_name, name);
                assert_eq!(output, expected);
                assert!(!args.to_string().contains("private-fixture-value"));
            }
            other => panic!("expected one correlated terminal pair: {other:?}"),
        }
    }
}

#[tokio::test]
async fn available_calls_and_recoverable_batch_aliases_still_need_approval() {
    let registry: Vec<Box<dyn Tool>> = vec![
        Box::new(NamedTool("available")),
        Box::new(NamedTool("tool_search")),
    ];
    let dispatch = ToolDispatchContext {
        tools_registry: &registry,
        activated_tools: None,
        excluded_tools: &[],
        model_switch_callback: None,
    };
    let run = prepare(&[call("available")], dispatch, None).await;
    assert_eq!(run.prompts, 1);
    assert_eq!(outcome(&run, 0).output, "Denied by user.");

    let run = prepare(
        &[call("alias"), call("tool_search"), call("alias")],
        dispatch,
        None,
    )
    .await;
    assert_eq!(outcome(&run, 0).output, "Unknown tool: alias");
    assert_eq!(run.prepared.executable_indices, vec![1]);
    assert_eq!(run.prompts, 1);
    assert_eq!(outcome(&run, 2).output, "Denied by user.");
}

struct RewriteHook;
#[async_trait]
impl HookHandler for RewriteHook {
    fn name(&self) -> &str {
        "preflight-rewrite"
    }
    async fn before_tool_call(&self, _: String, args: Value) -> HookResult<(String, Value)> {
        HookResult::Continue(("alias".into(), args))
    }
}

#[tokio::test]
async fn hook_rewrites_and_canonical_alias_exclusions_are_checked_before_approval() {
    let activated = Arc::new(Mutex::new(ActivatedToolSet::new()));
    activated.lock().unwrap().activate(
        "fixture__alias".into(),
        Arc::new(NamedTool("fixture__alias")),
    );
    let excluded = ["fixture__alias".into()];
    let dispatch = ToolDispatchContext {
        tools_registry: &[],
        activated_tools: Some(&activated),
        excluded_tools: &excluded,
        model_switch_callback: None,
    };
    let mut hooks = HookRunner::new();
    hooks.register(Box::new(RewriteHook));
    let mut input = call("original");
    input.tool_call_id = None;
    let run = prepare(&[input], dispatch, Some(&hooks)).await;
    assert_eq!(run.prompts, 0);
    assert_eq!(
        outcome(&run, 0).output,
        "Tool not available in this turn: alias"
    );
    match run.events.as_slice() {
        [
            TurnEvent::ToolCall { id, name, .. },
            TurnEvent::ToolResult {
                id: result_id,
                name: result_name,
                ..
            },
        ] => {
            assert_eq!(id, result_id);
            assert!(!id.is_empty());
            assert_eq!(name, "alias");
            assert_eq!(name, result_name);
        }
        other => panic!("expected one correlated pair: {other:?}"),
    }
}
