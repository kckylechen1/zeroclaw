//! Canonical loop -> tool dispatch/preparation -> real log subscriber ->
//! broadcast and JSONL. Synthetic provider/tools exercise the full rendering
//! path without external services or credentials.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use zeroclaw_api::attribution::{Attributable, Role};
use zeroclaw_api::channel::{
    Channel, ChannelApprovalRequest, ChannelApprovalResponse, ChannelMessage, SendMessage,
};
use zeroclaw_api::hook::{HookHandler, HookResult};
use zeroclaw_api::ingress::IngressContext;
use zeroclaw_api::model_provider::{ChatRequest, ProviderCapabilities};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::{MultimodalConfig, PacingConfig, RiskProfileConfig};
use zeroclaw_log::LogConfig;
use zeroclaw_providers::{ChatMessage, ChatResponse, ModelProvider, ToolCall};
use zeroclaw_runtime::agent::loop_::{
    LoopKnobs, ResolvedAgentExecution, ResolvedModelAccess, ToolLoop, run_tool_call_loop,
};
use zeroclaw_runtime::agent::tool_receipts::ReceiptGenerator;
use zeroclaw_runtime::approval::ApprovalManager;
use zeroclaw_runtime::hooks::HookRunner;
use zeroclaw_runtime::observability::NoopObserver;

const INPUT_SECRET: &str = "fixture-input-private-value";
const OUTPUT_SECRET: &str = "fixture-output-private-value";
const ERROR_SECRET: &str = "fixture-error-private-value";
const BODY_MARKER: &str = "fixture-body-is-not-metadata";
const TOOL_NAMES: &[&str] = &["audit_success", "audit_failure", "audit_error"];
const BODY_FIELDS: &[&str] = &[
    "input",
    "arguments",
    "output",
    "result",
    "error",
    "error_reason",
    "reason",
];

fn raw_arguments() -> Value {
    json!({
        "api_key": {"nested": [INPUT_SECRET]},
        "query": BODY_MARKER.repeat(8),
    })
}

fn raw_output() -> ToolOutput {
    ToolOutput::json(json!({
        "api_key": [OUTPUT_SECRET],
        "body": BODY_MARKER.repeat(8),
        "opaque": bare_secret(),
    }))
}

fn bare_secret() -> String {
    format!("ghp_{}", "a".repeat(36))
}

type ReceivedArguments = Arc<Mutex<Vec<(String, Value)>>>;

struct FixtureTool {
    name: &'static str,
    received: ReceivedArguments,
}

zeroclaw_api::mock_tool_attribution!(FixtureTool);

#[async_trait]
impl Tool for FixtureTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Synthetic log-policy fixture"
    }

    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        self.received.lock().unwrap().push((self.name.into(), args));
        if self.name == "audit_error" {
            anyhow::bail!("fixture failure token={ERROR_SECRET}");
        }
        Ok(ToolResult {
            success: self.name == "audit_success",
            output: raw_output(),
            error: (self.name == "audit_failure")
                .then(|| format!("fixture failure password={ERROR_SECRET}")),
        })
    }
}

struct FixtureProvider {
    calls: Mutex<Option<Vec<ToolCall>>>,
    seen: Mutex<Vec<Vec<ChatMessage>>>,
    native: bool,
}

impl Attributable for FixtureProvider {
    fn role(&self) -> Role {
        Role::System
    }

    fn alias(&self) -> &str {
        "log-policy-fixture"
    }
}

#[async_trait]
impl ModelProvider for FixtureProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: self.native,
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
        anyhow::bail!("fixture requires native chat dispatch")
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        _: &str,
        _: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        self.seen.lock().unwrap().push(request.messages.to_vec());
        let mut calls = self.calls.lock().unwrap().take();
        if !self.native
            && let Some(calls) = calls.take()
        {
            let text = calls
                .into_iter()
                .map(|call| {
                    format!(
                        "<tool_call>{}</tool_call>",
                        json!({
                            "name": call.name,
                            "arguments": serde_json::from_str::<Value>(&call.arguments).unwrap(),
                        })
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(ChatResponse {
                text: Some(text),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            });
        }
        Ok(ChatResponse {
            text: calls.is_none().then(|| "done".into()),
            tool_calls: calls.unwrap_or_default(),
            usage: None,
            reasoning_content: None,
        })
    }
}

struct CancelHook;

#[async_trait]
impl HookHandler for CancelHook {
    fn name(&self) -> &str {
        "log-policy-cancel"
    }

    async fn before_tool_call(&self, _: String, _: Value) -> HookResult<(String, Value)> {
        HookResult::Cancel(format!("{BODY_MARKER} password={ERROR_SECRET}"))
    }
}

struct ReplacementChannel;

impl Attributable for ReplacementChannel {
    fn role(&self) -> Role {
        Role::System
    }

    fn alias(&self) -> &str {
        "log-policy-approval"
    }
}

#[async_trait]
impl Channel for ReplacementChannel {
    fn name(&self) -> &str {
        "test"
    }

    async fn send(&self, _: &SendMessage) -> anyhow::Result<()> {
        Ok(())
    }

    async fn listen(&self, _: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        Ok(())
    }

    async fn request_approval(
        &self,
        _: &str,
        _: &ChannelApprovalRequest,
    ) -> anyhow::Result<Option<ChannelApprovalResponse>> {
        Ok(Some(ChannelApprovalResponse::DenyWithEdit {
            replacement: format!("{BODY_MARKER} token={OUTPUT_SECRET}"),
        }))
    }
}

#[derive(Clone, Copy, Debug)]
enum Scenario {
    Execute,
    HookCancel,
    ApprovalDeny,
    ApprovalReplace,
    Duplicate,
}

async fn exercise_loop(scenario: Scenario, native: bool) {
    let received = ReceivedArguments::default();
    let tools: Vec<Box<dyn Tool>> = TOOL_NAMES
        .iter()
        .map(|&name| {
            Box::new(FixtureTool {
                name,
                received: Arc::clone(&received),
            }) as Box<dyn Tool>
        })
        .collect();
    let names = if matches!(scenario, Scenario::Duplicate) {
        vec!["audit_success", "audit_success"]
    } else {
        TOOL_NAMES.to_vec()
    };
    let provider = FixtureProvider {
        calls: Mutex::new(Some(
            names
                .iter()
                .enumerate()
                .map(|(index, name)| ToolCall {
                    id: format!("audit-call-{index}"),
                    name: (*name).into(),
                    arguments: raw_arguments().to_string(),
                    extra_content: None,
                })
                .collect(),
        )),
        seen: Mutex::new(Vec::new()),
        native,
    };
    let mut hooks = HookRunner::new();
    hooks.register(Box::new(CancelHook));
    let approval = ApprovalManager::for_non_interactive(&RiskProfileConfig {
        always_ask: TOOL_NAMES.iter().map(|name| (*name).into()).collect(),
        ..Default::default()
    });
    let receipt_generator = ReceiptGenerator::new();
    let receipts = Mutex::new(Vec::new());
    let mut history = vec![
        ChatMessage::system("Fixture"),
        ChatMessage::user("Run fixture"),
    ];
    let result = run_tool_call_loop(ToolLoop {
        exec: ResolvedAgentExecution {
            task_owner: None,
            model_access: ResolvedModelAccess {
                model_provider: &provider,
                provider_name: "fixture",
                model: "fixture",
                temperature: None,
            },
            tools_registry: &tools,
            observer: &NoopObserver,
            silent: true,
            approval: matches!(scenario, Scenario::ApprovalDeny | Scenario::ApprovalReplace)
                .then_some(&approval),
            multimodal_config: &MultimodalConfig::default(),
            config: None,
            max_tool_iterations: 3,
            hooks: matches!(scenario, Scenario::HookCancel).then_some(&hooks),
            excluded_tools: &[],
            dedup_exempt_tools: &[],
            activated_tools: None,
            model_switch_callback: None,
            pacing: &PacingConfig::default(),
            strict_tool_parsing: false,
            parallel_tools: false,
            max_tool_result_chars: 0,
            keep_tool_context_turns: 2,
            context_token_budget: 0,
            receipt_generator: Some(&receipt_generator),
            knobs: &LoopKnobs::default(),
        },
        history: &mut history,
        channel_name: "test",
        channel_reply_target: Some("fixture"),
        cancellation_token: None,
        on_delta: None,
        shared_budget: None,
        channel: matches!(scenario, Scenario::ApprovalReplace)
            .then_some(&ReplacementChannel as &dyn Channel),
        collected_receipts: Some(&receipts),
        event_tx: None,
        steering: None,
        new_messages_out: None,
        image_cache: None,
        ingress: IngressContext::sub_turn(),
        memory: None,
        agent_alias: None,
        parent_agent_alias: None,
        turn_id: "log-policy-fixture",
    })
    .await
    .unwrap();
    assert_eq!(result, "done");
    let seen = provider.seen.lock().unwrap();
    assert_eq!(
        seen.len(),
        2,
        "fixture must complete a provider/tool/provider round trip"
    );
    let model_results = seen[1]
        .iter()
        .filter(|message| matches!(message.role.as_str(), "tool" | "user"))
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let received = received.lock().unwrap();
    if matches!(scenario, Scenario::Execute | Scenario::Duplicate) {
        let expected = if matches!(scenario, Scenario::Duplicate) {
            1
        } else {
            3
        };
        assert_eq!(received.len(), expected);
        assert!(
            received
                .iter()
                .all(|(_, arguments)| arguments == &raw_arguments())
        );
        assert!(
            model_results.contains(OUTPUT_SECRET),
            "model output remains raw"
        );
        if matches!(scenario, Scenario::Execute) {
            assert!(
                model_results.contains(ERROR_SECRET),
                "model failure remains raw"
            );
        }
        let receipts = receipts.lock().unwrap();
        assert_eq!(receipts.len(), 1);
        let receipt = receipts[0].strip_prefix("audit_success: ").unwrap();
        assert!(receipt_generator.verify(
            receipt,
            "audit_success",
            &raw_arguments(),
            raw_output().as_str()
        ));
    } else {
        assert!(received.is_empty(), "denied/replaced calls never execute");
        assert!(receipts.lock().unwrap().is_empty());
        if matches!(scenario, Scenario::HookCancel) {
            assert!(model_results.contains(ERROR_SECRET));
        }
        if matches!(scenario, Scenario::ApprovalReplace) {
            assert!(model_results.contains(OUTPUT_SECRET));
        }
    }
}

fn tool_records(records: impl IntoIterator<Item = Value>) -> Vec<Value> {
    records
        .into_iter()
        .filter(|record| {
            record["attributes"]["tool"]
                .as_str()
                .is_some_and(|name| TOOL_NAMES.contains(&name))
        })
        .collect()
}

fn assert_capture(records: &[Value], capture: bool, truncated: bool, cap: usize) {
    assert!(!records.is_empty(), "tool metadata must still be recorded");
    let encoded = serde_json::to_string(records).unwrap();
    for secret in [INPUT_SECRET, OUTPUT_SECRET, ERROR_SECRET] {
        assert!(!encoded.contains(secret), "log must not contain {secret}");
    }
    assert!(
        !encoded.contains(&bare_secret()),
        "bare credentials are leak-scanned"
    );
    if !capture {
        assert!(!encoded.contains(BODY_MARKER));
        for record in records {
            for &field in BODY_FIELDS {
                assert!(record["attributes"].get(field).is_none(), "{record}");
            }
        }
        return;
    }
    assert!(
        records
            .iter()
            .any(|record| record["attributes"].get("arguments").is_some())
    );
    if truncated {
        assert!(
            records
                .iter()
                .any(|record| record["attributes"]["tool_input_truncated"] == true)
        );
        assert!(
            records
                .iter()
                .any(|record| record["attributes"]["tool_output_truncated"] == true)
        );
        for record in records {
            for fields in [&BODY_FIELDS[..2], &BODY_FIELDS[2..]] {
                let bytes: usize = fields
                    .iter()
                    .filter_map(|field| record["attributes"].get(*field))
                    .filter(|value| !value.is_null())
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::len)
                            .unwrap_or_else(|| value.to_string().len())
                    })
                    .sum();
                assert!(bytes <= cap, "captured body exceeds cap: {record}");
            }
        }
    } else {
        assert!(
            encoded.contains(BODY_MARKER),
            "full capture preserves non-secret body"
        );
        assert!(encoded.contains("[REDACTED]"));
    }
}

#[tokio::test]
async fn canonical_loop_enforces_tool_log_policy_on_broadcast_and_disk() {
    // This integration binary owns its subscriber, so it can test the same
    // INFO recording floor used by the daemon without other tests changing it.
    let debug_child = std::env::var("ZEROCLAW_AUDIT_LOG_DEBUG").as_deref() == Ok("1");
    let filter = if debug_child { "debug" } else { "info" };
    zeroclaw_log::install_global_subscriber(Some(filter), filter, false);
    let mut rx = zeroclaw_log::subscribe_or_install();
    let tmp = tempfile::tempdir().unwrap();

    // Before initialization, no policy means no body capture.
    exercise_loop(Scenario::Execute, true).await;
    let records = tool_records(std::iter::from_fn(|| rx.try_recv().ok()));
    assert_capture(&records, false, false, 0);

    for (index, (mode, deny, cap)) in [
        ("off", false, 80),
        ("redacted", false, 80),
        ("redacted", false, 0),
        ("full", false, 8),
        ("full", true, 80),
    ]
    .into_iter()
    .enumerate()
    {
        let path = tmp.path().join(format!("case-{index}.jsonl"));
        zeroclaw_log::init_from_config(
            &LogConfig {
                log_persistence: "full".into(),
                log_persistence_path: path.to_string_lossy().into_owned(),
                log_tool_io: mode.into(),
                log_tool_io_truncate_bytes: cap,
                log_tool_io_denylist: if deny {
                    TOOL_NAMES.iter().map(|name| (*name).into()).collect()
                } else {
                    Vec::new()
                },
                ..LogConfig::default()
            },
            tmp.path(),
        );
        while rx.try_recv().is_ok() {}
        for native in [true, false] {
            for scenario in [
                Scenario::Execute,
                Scenario::HookCancel,
                Scenario::ApprovalDeny,
                Scenario::ApprovalReplace,
                Scenario::Duplicate,
            ] {
                exercise_loop(scenario, native).await;
            }
        }
        zeroclaw_log::flush_for_test().unwrap();
        let broadcast = tool_records(std::iter::from_fn(|| rx.try_recv().ok()));
        let log_text = std::fs::read_to_string(&path).unwrap();
        for secret in [INPUT_SECRET, OUTPUT_SECRET, ERROR_SECRET] {
            assert!(
                !log_text.contains(secret),
                "no log path may disclose {secret}"
            );
        }
        assert!(!log_text.contains(&bare_secret()));
        if mode == "off" || deny {
            assert!(
                !log_text.contains(BODY_MARKER),
                "text protocol must not bypass the tool policy"
            );
        }
        let persisted = tool_records(
            log_text
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap()),
        );
        assert_eq!(
            broadcast, persisted,
            "broadcast and JSONL see the same policy"
        );
        assert!(
            persisted
                .iter()
                .any(|record| record["severity_text"] == "WARN")
        );
        assert!(
            persisted
                .iter()
                .any(|record| record["severity_text"] == "ERROR")
        );
        assert_eq!(
            persisted
                .iter()
                .any(|record| record["severity_text"] == "DEBUG"),
            debug_child,
        );
        assert_capture(&persisted, mode != "off" && !deny, mode == "redacted", cap);
    }

    // Close the final file before the temporary directory is removed (Windows too).
    zeroclaw_log::init_from_config(
        &LogConfig {
            log_persistence: "none".into(),
            ..LogConfig::default()
        },
        tmp.path(),
    );
    zeroclaw_log::clear_broadcast_hook();

    if !debug_child {
        // A fresh test process can install a DEBUG subscriber without changing
        // the INFO-floor assertion above or introducing a mutable global filter.
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("canonical_loop_enforces_tool_log_policy_on_broadcast_and_disk")
            .env("ZEROCLAW_AUDIT_LOG_DEBUG", "1")
            .kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(60), command.output())
            .await
            .expect("DEBUG fixture must finish within its deadline")
            .unwrap();
        assert!(
            output.status.success(),
            "DEBUG fixture failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
