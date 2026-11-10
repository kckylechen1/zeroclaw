//! Lock-free read-only side-query (`/btw`) handle for an [`super::Agent`].
//!
//! A [`SideQueryHandle`] holds an [`Arc<parking_lot::RwLock<SideQuerySnapshot>>`]
//! updated by the owning [`super::Agent`] at construction, configuration/history
//! mutations, turn start, and each completed tool round. Callers (such as the
//! WebSocket gateway) can clone the handle and answer a one-shot `/btw` question
//! concurrently while a main turn holds the `Agent`'s `&mut self` lock, without
//! mutating conversation history, session storage, steering queues, or turn state.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use parking_lot::RwLock;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use zeroclaw_config::schema::PacingConfig;
use zeroclaw_providers::{ChatMessage, ChatRequest, ModelProvider};

/// Maximum number of concurrent in-flight `/btw` side queries per conversation.
pub const MAX_CONCURRENT_SIDE_QUERIES: usize = 4;

/// Maximum UTF-8 byte length of a `/btw` side-query question.
pub const MAX_SIDE_QUERY_QUESTION_BYTES: usize = 16 * 1024;

/// Default step timeout (in seconds) applied to a `/btw` side query when the
/// agent's [`PacingConfig::step_timeout_secs`] is `None`.
pub const DEFAULT_SIDE_QUERY_TIMEOUT_SECS: u64 = 60;

/// Framing prepended to the ephemeral user message in a `/btw` side query.
pub const BTW_SIDE_QUERY_INSTRUCTION: &str = "\
[SIDE QUERY (/btw) — READ-ONLY, NO TOOLS]\n\
The user is asking a one-shot side question alongside the ongoing session. \
Answer directly and concisely using only the conversation and tool context already present above. \
No tools are available in this side query, and this exchange will not be added to the main conversation history; \
do not emit tool-call markup or claim to have executed new actions.";

/// Immutable point-in-time view of an [`super::Agent`]'s provider and transcript state.
#[derive(Clone)]
pub struct SideQuerySnapshot {
    pub model_provider: Arc<dyn ModelProvider>,
    pub provider_name: String,
    pub model: String,
    pub temperature: Option<f64>,
    pub pacing: PacingConfig,
    pub messages: Vec<ChatMessage>,
    pub known_tool_names: Arc<HashSet<String>>,
    pub active_turn_id: Option<String>,
    pub revision: u64,
}

/// Outcome of a read-only `/btw` side query.
#[derive(Debug, Clone)]
pub struct SideQueryResponse {
    pub question: String,
    pub answer: String,
    pub model: String,
    pub provider_name: String,
    pub revision: u64,
    pub active_turn_id: Option<String>,
    pub known_tool_names: Arc<HashSet<String>>,
}

/// Cloneable, lock-free handle for executing read-only `/btw` side queries
/// against the latest [`SideQuerySnapshot`] of an [`super::Agent`].
#[derive(Clone)]
pub struct SideQueryHandle {
    inner: Arc<RwLock<SideQuerySnapshot>>,
    semaphore: Arc<Semaphore>,
}

/// RAII guard that ensures `active_turn_id` is cleared from the [`SideQuerySnapshot`]
/// if an in-flight turn future is dropped before normal completion.
pub(crate) struct ActiveTurnSideQueryGuard {
    handle: SideQueryHandle,
    turn_id: String,
}

impl Drop for ActiveTurnSideQueryGuard {
    fn drop(&mut self) {
        self.handle.clear_active_turn(&self.turn_id);
    }
}

impl SideQueryHandle {
    pub(crate) fn new(initial: SideQuerySnapshot) -> Self {
        Self {
            inner: Arc::new(RwLock::new(initial)),
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_SIDE_QUERIES)),
        }
    }

    /// Create an RAII guard that clears `active_turn_id` if the turn is dropped early.
    pub(crate) fn enter_turn(&self, turn_id: &str) -> ActiveTurnSideQueryGuard {
        ActiveTurnSideQueryGuard {
            handle: self.clone(),
            turn_id: turn_id.to_owned(),
        }
    }

    pub(crate) fn clear_active_turn(&self, expected_turn_id: &str) {
        let mut guard = self.inner.write();
        if guard.active_turn_id.as_deref() == Some(expected_turn_id) {
            guard.active_turn_id = None;
            crate::agent::history_pruner::strip_orphaned_tool_calls_from_assistants(
                &mut guard.messages,
            );
            let _ =
                crate::agent::history_pruner::remove_orphaned_tool_messages(&mut guard.messages);
            guard.revision = guard.revision.wrapping_add(1);
        }
    }

    /// Try to acquire one of the bounded concurrent side-query execution slots.
    pub fn try_acquire_slot(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.semaphore).try_acquire_owned().ok()
    }

    /// Return a clone of the current snapshot without holding the lock.
    pub fn snapshot(&self) -> SideQuerySnapshot {
        self.inner.read().clone()
    }

    /// Return the active turn ID recorded in the snapshot, if a turn is currently running.
    pub fn active_turn_id(&self) -> Option<String> {
        self.inner.read().active_turn_id.clone()
    }

    /// Update only the model/provider/temperature configuration on the snapshot,
    /// preserving any in-flight `messages` and `active_turn_id`.
    pub(crate) fn update_model_config(
        &self,
        model_provider: Arc<dyn ModelProvider>,
        provider_name: String,
        model: String,
        temperature: Option<f64>,
    ) {
        let mut guard = self.inner.write();
        guard.model_provider = model_provider;
        guard.provider_name = provider_name;
        guard.model = model;
        guard.temperature = temperature;
        guard.revision = guard.revision.wrapping_add(1);
    }

    /// Replace the snapshot from the owning `Agent`'s committed state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn replace(
        &self,
        model_provider: Arc<dyn ModelProvider>,
        provider_name: String,
        model: String,
        temperature: Option<f64>,
        pacing: PacingConfig,
        messages: Vec<ChatMessage>,
        known_tool_names: Arc<HashSet<String>>,
        active_turn_id: Option<String>,
    ) {
        let mut guard = self.inner.write();
        let next_revision = guard.revision.wrapping_add(1);
        *guard = SideQuerySnapshot {
            model_provider,
            provider_name,
            model,
            temperature,
            pacing,
            messages,
            known_tool_names,
            active_turn_id,
            revision: next_revision,
        };
    }

    /// Publish the effective provider, model, transcript, and turn ID together.
    /// A vision override owns a different provider instance from the Agent's
    /// default, so its handle must travel with its name and model. Cloned
    /// snapshots keep that instance alive without borrowing the running Agent.
    pub(crate) fn update_live_messages(
        &self,
        model_provider: Arc<dyn ModelProvider>,
        provider_name: &str,
        model: &str,
        messages: &[ChatMessage],
        active_turn_id: Option<&str>,
    ) {
        let mut guard = self.inner.write();
        guard.model_provider = model_provider;
        guard.provider_name = provider_name.to_string();
        guard.model = model.to_string();
        guard.messages = messages.to_vec();
        guard.active_turn_id = active_turn_id.map(str::to_owned);
        guard.revision = guard.revision.wrapping_add(1);
    }

    /// Execute a one-shot, tools-free `/btw` side query against a cloned snapshot.
    ///
    /// Never holds the snapshot lock across `.await` and never mutates the snapshot
    /// or the owning `Agent`.
    pub async fn ask(
        &self,
        question: &str,
        cancel_token: Option<&CancellationToken>,
    ) -> Result<SideQueryResponse> {
        let trimmed_question = question.trim();
        if trimmed_question.is_empty() {
            anyhow::bail!("empty /btw question: refusing to dispatch a blank side query");
        }
        if trimmed_question.len() > MAX_SIDE_QUERY_QUESTION_BYTES {
            anyhow::bail!(
                "/btw question exceeds maximum length ({MAX_SIDE_QUERY_QUESTION_BYTES} bytes)"
            );
        }

        let snapshot = self.snapshot();
        let mut messages = snapshot.messages;

        // Sanitize any orphaned tool calls or tool results so strict providers
        // accept the tools-free side-query transcript even mid-turn.
        crate::agent::history_pruner::strip_orphaned_tool_calls_from_assistants(&mut messages);
        let _ = crate::agent::history_pruner::remove_orphaned_tool_messages(&mut messages);
        crate::agent::turn::refresh_prompt_anchor(&mut messages, false);

        messages.push(ChatMessage::user(format!(
            "{BTW_SIDE_QUERY_INSTRUCTION}\n\nQuestion: {trimmed_question}"
        )));

        let access = crate::agent::turn::execution::ResolvedModelAccess {
            model_provider: snapshot.model_provider.as_ref(),
            provider_name: &snapshot.provider_name,
            model: &snapshot.model,
            temperature: snapshot.temperature,
        };
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };

        let step_secs = snapshot
            .pacing
            .step_timeout_secs
            .filter(|&secs| secs > 0)
            .unwrap_or(DEFAULT_SIDE_QUERY_TIMEOUT_SECS);
        let query_fut = async {
            match tokio::time::timeout(
                Duration::from_secs(step_secs),
                access.run_model_query(request),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => anyhow::bail!("LLM side query timed out after {step_secs}s"),
            }
        };

        let response = if let Some(token) = cancel_token {
            tokio::select! {
                biased;
                () = token.cancelled() => {
                    return Err(crate::agent::loop_::ToolLoopCancelled.into());
                }
                res = query_fut => res?,
            }
        } else {
            query_fut.await?
        };

        let raw_text = response.text.unwrap_or_default();
        let without_think = zeroclaw_tool_call_parser::strip_think_tags(&raw_text);
        let without_tool_results =
            zeroclaw_tool_call_parser::strip_tool_result_blocks(&without_think);
        let (parsed_text, parsed_calls) =
            zeroclaw_tool_call_parser::parse_tool_calls(&without_tool_results);
        let cleaned = if !parsed_calls.is_empty() && !parsed_text.trim().is_empty() {
            parsed_text.trim().to_string()
        } else if !parsed_calls.is_empty() {
            String::new()
        } else {
            without_tool_results.trim().to_string()
        };
        if cleaned.is_empty() {
            anyhow::bail!("Side query produced an empty response");
        }

        Ok(SideQueryResponse {
            question: trimmed_question.to_string(),
            answer: cleaned,
            model: snapshot.model,
            provider_name: snapshot.provider_name,
            revision: snapshot.revision,
            active_turn_id: snapshot.active_turn_id,
            known_tool_names: snapshot.known_tool_names,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::agent::dispatcher::NativeToolDispatcher;
    use async_trait::async_trait;
    use parking_lot::Mutex;
    use tokio::sync::{Notify, Semaphore};
    use zeroclaw_api::attribution::{Attributable, ModelProviderKind, ProviderKind, Role};
    use zeroclaw_api::tool::{Tool, ToolResult};
    use zeroclaw_memory::NoneMemory;
    use zeroclaw_providers::{ChatResponse, ConversationMessage, ToolCall};

    zeroclaw_api::mock_tool_attribution!(EchoTool);

    struct BlockingScriptedProvider {
        seen_requests: Arc<Mutex<Vec<Vec<ChatMessage>>>>,
        main_entered: Arc<Notify>,
        main_gate: Arc<Semaphore>,
        use_tool_on_first_call: bool,
        calls: Arc<Mutex<usize>>,
    }

    impl Attributable for BlockingScriptedProvider {
        fn role(&self) -> Role {
            Role::Provider(ProviderKind::Model(ModelProviderKind::Custom))
        }

        fn alias(&self) -> &str {
            "blocking-scripted"
        }
    }

    #[async_trait]
    impl ModelProvider for BlockingScriptedProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<String> {
            Ok("ok".into())
        }

        async fn chat(
            &self,
            request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> Result<ChatResponse> {
            let messages = request.messages.to_vec();
            self.seen_requests.lock().push(messages.clone());

            let is_side_query = messages.last().is_some_and(|m| {
                m.role == "user" && m.content.starts_with(BTW_SIDE_QUERY_INSTRUCTION)
            });
            if is_side_query {
                assert!(
                    request.tools.is_none(),
                    "/btw side query must never expose tools to the provider"
                );
                return Ok(ChatResponse {
                    text: Some(
                        "<think>internal thought</think>Side answer based on live context<tool_call>{\"name\":\"echo_tool\",\"arguments\":{}}</tool_call>"
                            .into(),
                    ),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                });
            }

            let call_index = {
                let mut calls = self.calls.lock();
                let idx = *calls;
                *calls += 1;
                idx
            };

            if self.use_tool_on_first_call && call_index == 0 {
                return Ok(ChatResponse {
                    text: Some("Running echo_tool first.".into()),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "echo_tool".into(),
                        arguments: r#"{"value":"alpha-42"}"#.into(),
                        extra_content: None,
                    }],
                    usage: None,
                    reasoning_content: None,
                });
            }

            self.main_entered.notify_waiters();
            self.main_gate.acquire().await?.forget();
            Ok(ChatResponse {
                text: Some("Main turn final response.".into()),
                tool_calls: vec![],
                usage: None,
                reasoning_content: None,
            })
        }
    }

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo_tool"
        }

        fn description(&self) -> &str {
            "Echoes a value for testing"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "value": { "type": "string" }
                }
            })
        }

        async fn execute(&self, args: serde_json::Value) -> Result<ToolResult> {
            let val = args["value"].as_str().unwrap_or("none");
            Ok(ToolResult {
                success: true,
                output: format!("tool-output:{val}").into(),
                error: None,
            })
        }
    }

    fn build_agent(provider: BlockingScriptedProvider, tools: Vec<Box<dyn Tool>>) -> Agent {
        Agent::builder()
            .model_provider(Box::new(provider))
            .tools(tools)
            .memory(Arc::new(NoneMemory::new("none")))
            .observer(Arc::new(crate::observability::NoopObserver))
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .model_name("test-model".into())
            .model_provider_name("blocking-scripted".into())
            .build()
            .expect("build test agent")
    }

    #[tokio::test]
    async fn btw_side_query_on_idle_agent_reads_seeded_history_without_mutating_state() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = BlockingScriptedProvider {
            seen_requests: Arc::clone(&seen),
            main_entered: Arc::new(Notify::new()),
            main_gate: Arc::new(Semaphore::new(1)),
            use_tool_on_first_call: false,
            calls: Arc::new(Mutex::new(0)),
        };
        let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);
        agent.seed_history(&[
            ChatMessage::user("We are deploying service-x"),
            ChatMessage::assistant("Understood, targeting service-x."),
        ]);
        let history_before = agent.history().len();

        let resp = agent
            .btw("  what service are we deploying?  ")
            .await
            .expect("btw query should succeed");
        assert_eq!(resp.question, "what service are we deploying?");
        assert_eq!(resp.answer, "Side answer based on live context");
        assert_eq!(resp.model, "test-model");
        assert_eq!(resp.provider_name, "blocking-scripted");
        assert!(resp.active_turn_id.is_none());
        assert!(resp.known_tool_names.contains("echo_tool"));
        assert_eq!(
            agent.history().len(),
            history_before,
            "/btw must not mutate Agent::history"
        );

        let recorded = seen.lock();
        assert_eq!(recorded.len(), 1);
        assert!(
            recorded[0]
                .iter()
                .any(|m| m.role == "user" && m.content.contains("deploying service-x"))
        );
        assert!(recorded[0].last().is_some_and(|m| {
            m.content
                .contains("Question: what service are we deploying?")
        }));
    }

    #[tokio::test]
    async fn btw_side_query_mid_turn_sees_completed_tool_round_while_main_turn_is_blocked() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let main_entered = Arc::new(Notify::new());
        let main_gate = Arc::new(Semaphore::new(0));
        let provider = BlockingScriptedProvider {
            seen_requests: Arc::clone(&seen),
            main_entered: Arc::clone(&main_entered),
            main_gate: Arc::clone(&main_gate),
            use_tool_on_first_call: true,
            calls: Arc::new(Mutex::new(0)),
        };
        let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);
        let side_query = agent.side_query_handle();

        let entered_waiter = main_entered.notified();
        let main_turn = ::zeroclaw_spawn::spawn!(async move {
            let reply = agent.turn("run the tool and summarize").await?;
            Ok::<(String, Vec<ConversationMessage>), anyhow::Error>((
                reply,
                agent.history().to_vec(),
            ))
        });

        // Wait until round 0 finished executing `echo_tool` and round 1 is blocked in `provider.chat`.
        entered_waiter.await;

        let side_resp = side_query
            .ask("what did echo_tool output?", None)
            .await
            .expect("mid-turn /btw query should succeed without waiting on main_gate");
        assert_eq!(side_resp.answer, "Side answer based on live context");
        assert!(
            side_resp.active_turn_id.is_some(),
            "mid-turn snapshot must carry the active turn ID"
        );

        // Verify the side query's snapshot included both the in-flight user message and the tool result.
        {
            let recorded = seen.lock();
            let btw_messages = recorded
                .last()
                .expect("side query request must be recorded");
            assert!(
                btw_messages
                    .iter()
                    .any(|m| m.role == "user" && m.content.contains("run the tool and summarize")),
                "side query must see the in-flight user prompt"
            );
            assert!(
                btw_messages
                    .iter()
                    .any(|m| m.content.contains("tool-output:alpha-42")),
                "side query must see completed mid-turn tool output"
            );
        }

        // Release the main turn and verify its committed history is untouched by /btw.
        main_gate.add_permits(1);
        let (main_reply, final_history) = main_turn.await.unwrap().unwrap();
        assert_eq!(main_reply, "Main turn final response.");
        assert!(
            side_query.active_turn_id().is_none(),
            "active_turn_id must be cleared once the turn completes"
        );
        for msg in &final_history {
            if let ConversationMessage::Chat(chat) = msg {
                assert!(
                    !chat.content.contains("what did echo_tool output?")
                        && !chat.content.contains("Side answer based on live context"),
                    "/btw exchange must never leak into committed Agent::history"
                );
            }
        }
    }

    #[tokio::test]
    async fn btw_uses_the_live_vision_endpoint_and_restores_the_default_binding() {
        use axum::{Json, Router, extract::State, routing::post};
        use serde_json::{Value, json};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::mpsc;

        #[derive(Clone)]
        struct Endpoint {
            name: &'static str,
            requests: mpsc::UnboundedSender<(&'static str, Value)>,
            main_calls: Arc<AtomicUsize>,
            main_gate: Arc<Semaphore>,
        }

        async fn capture(State(endpoint): State<Endpoint>, Json(body): Json<Value>) -> Json<Value> {
            let side_query = body["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .is_some_and(|message| match &message["content"] {
                    Value::String(content) => content.contains(BTW_SIDE_QUERY_INSTRUCTION),
                    Value::Array(parts) => parts.iter().any(|part| {
                        part["text"]
                            .as_str()
                            .is_some_and(|text| text.contains(BTW_SIDE_QUERY_INSTRUCTION))
                    }),
                    _ => false,
                });
            endpoint.requests.send((endpoint.name, body)).unwrap();
            if endpoint.name == "vision" && !side_query {
                if endpoint.main_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Json(json!({
                        "choices": [{"message": {
                            "role": "assistant",
                            "tool_calls": [{
                                "id": "vision-echo",
                                "type": "function",
                                "function": {
                                    "name": "echo_tool",
                                    "arguments": "{\"value\":\"vision-context\"}"
                                }
                            }]
                        }}]
                    }));
                }
                endpoint.main_gate.acquire().await.unwrap().forget();
            }
            Json(json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "content": format!("answer from {}", endpoint.name)
                }}]
            }))
        }

        async fn serve(endpoint: Endpoint) -> (String, tokio::task::JoinHandle<()>) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = Router::new()
                .route("/v1/chat/completions", post(capture))
                .with_state(endpoint);
            let server = ::zeroclaw_spawn::spawn!(async move {
                axum::serve(listener, app).await.unwrap();
            });
            (format!("http://{address}/v1"), server)
        }

        let (requests, mut received) = mpsc::unbounded_channel();
        let main_gate = Arc::new(Semaphore::new(0));
        let (text_url, text_server) = serve(Endpoint {
            name: "text",
            requests: requests.clone(),
            main_calls: Arc::new(AtomicUsize::new(0)),
            main_gate: Arc::clone(&main_gate),
        })
        .await;
        let (vision_url, vision_server) = serve(Endpoint {
            name: "vision",
            requests,
            main_calls: Arc::new(AtomicUsize::new(0)),
            main_gate: Arc::clone(&main_gate),
        })
        .await;
        let config: zeroclaw_config::schema::Config = toml::from_str(&format!(
            r#"
schema_version = 3
[providers.models.custom.text]
uri = "{text_url}"
model = "text-model"
vision = false
native_tools = true
[providers.models.custom.vision]
uri = "{vision_url}"
model = "vision-model"
vision = true
native_tools = true
[multimodal]
vision_model_provider = "custom.vision"
"#
        ))
        .unwrap();
        let primary = zeroclaw_providers::create_routed_model_provider_with_options(
            &config,
            "custom.text",
            None,
            None,
            &config.reliability,
            &[],
            "text-model",
            &zeroclaw_providers::provider_runtime_options_for_alias(&config, "custom", "text"),
        )
        .unwrap();
        let mut agent = Agent::builder()
            .model_provider(primary)
            .model_provider_name("custom.text".into())
            .model_name("text-model".into())
            .multimodal_config(config.multimodal.clone())
            .provider_switch_config(crate::agent::agent::ProviderSwitchConfig {
                config: Some(Arc::new(config)),
            })
            .tools(vec![Box::new(EchoTool)])
            .memory(Arc::new(NoneMemory::new("none")))
            .observer(Arc::new(crate::observability::NoopObserver))
            .tool_dispatcher(Box::new(NativeToolDispatcher))
            .build()
            .unwrap();
        let side_query = agent.side_query_handle();
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("fixture.png");
        image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]))
            .save(&image_path)
            .unwrap();
        let prompt = format!("inspect [IMAGE:{}]", image_path.display());
        let main_turn = ::zeroclaw_spawn::spawn!(async move {
            let answer = agent.turn(&prompt).await?;
            Ok::<_, anyhow::Error>((agent, answer))
        });

        // The first vision call executes echo_tool; the second holds the
        // main Agent borrow while /btw reads the completed tool context.
        for _ in 0..2 {
            let (endpoint, body) = tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(endpoint, "vision");
            assert_eq!(body["model"], "vision-model");
        }
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            side_query.ask("what did the tool find?", None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(response.answer, "answer from vision");
        assert_eq!(response.provider_name, "custom.vision");
        assert_eq!(response.model, "vision-model");
        assert!(response.active_turn_id.is_some());
        let (endpoint, body) = received.try_recv().unwrap();
        assert_eq!(
            endpoint, "vision",
            "the actual HTTP destination must match the snapshot"
        );
        assert_eq!(body["model"], "vision-model");
        assert!(body.get("tools").is_none_or(Value::is_null));
        assert!(
            body["messages"]
                .to_string()
                .contains("tool-output:vision-context")
        );

        main_gate.add_permits(1);
        let (mut agent, answer) = tokio::time::timeout(Duration::from_secs(5), main_turn)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(answer, "answer from vision");
        agent.clear_history();
        let response = side_query.ask("a new text question", None).await.unwrap();
        assert_eq!(response.answer, "answer from text");
        assert_eq!(response.provider_name, "custom.text");
        assert_eq!(response.model, "text-model");
        let (endpoint, body) = received.try_recv().unwrap();
        assert_eq!(endpoint, "text");
        assert_eq!(body["model"], "text-model");
        assert!(
            agent.history().is_empty(),
            "side queries must not persist their exchange"
        );
        text_server.abort();
        vision_server.abort();
    }

    #[tokio::test]
    async fn btw_side_query_rejects_blank_or_oversized_questions() {
        let provider = BlockingScriptedProvider {
            seen_requests: Arc::new(Mutex::new(Vec::new())),
            main_entered: Arc::new(Notify::new()),
            main_gate: Arc::new(Semaphore::new(1)),
            use_tool_on_first_call: false,
            calls: Arc::new(Mutex::new(0)),
        };
        let agent = build_agent(provider, vec![]);
        assert!(agent.btw("   \n\t  ").await.is_err());
        let oversized = "q".repeat(MAX_SIDE_QUERY_QUESTION_BYTES + 1);
        assert!(agent.btw(&oversized).await.is_err());
    }
}
