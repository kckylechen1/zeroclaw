//! Agent::turn / turn_streamed entry points extracted from `agent.rs`.

use super::*;
use crate::observability::ObserverEvent;
use anyhow::Result;
use zeroclaw_api::agent::TurnEvent;
use zeroclaw_providers::{ChatMessage, ConversationMessage};

impl Agent {
    pub async fn turn(&mut self, user_message: &str) -> Result<String> {
        if user_message.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "reason": "empty_user_message",
                        "entry_point": "Agent::turn",
                        "raw_len": user_message.len(),
                    })),
                "Refusing blank user turn (would emit timestamp-only message and risk prompt-template bleed-through)"
            );
            return Err(anyhow::Error::msg(
                "empty user message: refusing to dispatch a blank turn",
            ));
        }

        if self.history.is_empty() {
            let system_prompt = self.build_system_prompt()?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }

        let effective_model = self.classify_model(user_message);

        let turn_id = Self::new_turn_id();
        let turn_observer = Arc::clone(&self.observer);
        let mut guard = crate::observability::AgentTurnGuard::start(
            turn_observer.as_ref(),
            self.model_provider_name.clone(),
            effective_model.clone(),
            Some(self.channel_name.clone()),
            self.observer_agent_alias(),
            Some(turn_id.clone()),
        );

        // Memory context is injected once in the engine, keyed on the
        // ingress origin (agent::memory_inject).
        if self.auto_save {
            let store_start = std::time::Instant::now();
            let store_result = self
                .memory
                .store(
                    "user_msg",
                    user_message,
                    MemoryCategory::Conversation,
                    self.memory_session_id.as_deref(),
                )
                .await;
            self.observer.record_event(&ObserverEvent::MemoryStore {
                category: MemoryCategory::Conversation.to_string(),
                backend: self.memory.name().to_string(),
                duration: store_start.elapsed(),
                success: store_result.is_ok(),
                channel: Some(self.channel_name.clone()),
                agent_alias: self.observer_agent_alias(),
                turn_id: Some(turn_id.clone()),
            });
        }

        let now = self.current_turn_datetime();
        let (year, month, day) = (now.year(), now.month(), now.day());
        let (hour, minute, second) = (now.hour(), now.minute(), now.second());
        let tz = now.format("%Z");
        let date_str =
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {tz}");

        // Same claim-once-per-turn contract as
        // `append_streamed_user_message_to_history`; `turn` builds its user
        // message inline instead of going through that helper, so it holds its
        // own guard and settles it at this function's success exits.
        let (announcements, announcement_guard) =
            crate::agent::loop_::claim_announcements_for_turn(true).await;
        let enriched =
            format!("{announcements}[CURRENT DATE & TIME: {date_str}]\n\n{user_message}");

        self.history
            .push(ConversationMessage::Chat(ChatMessage::user(enriched)));

        let active_dispatcher = {
            let base_provider_messages = self.tool_dispatcher.to_provider_messages(&self.history);
            let (vision_provider_box, _degrade_strip_images) =
                match crate::agent::turn::resolve_vision_provider(
                    self.full_config(),
                    self.model_provider.as_ref(),
                    &base_provider_messages,
                    &self.multimodal_config,
                    &self.model_provider_name,
                    &effective_model,
                ) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let _ = self.trim_history(Some(&turn_id));
                        return Err(error);
                    }
                };
            let active_provider: &dyn ModelProvider = vision_provider_box
                .as_ref()
                .map(|resolved| resolved.provider.as_ref())
                .unwrap_or(self.model_provider.as_ref());
            tool_dispatcher_for_provider(&self.config, active_provider)
        };

        if let Err(error) = self.rebuild_system_prompt_for_dispatcher(active_dispatcher.as_ref()) {
            let _ = self.trim_history(Some(&turn_id));
            return Err(error);
        }

        let provider_messages = active_dispatcher.to_provider_messages(&self.history);
        let cache_key = self.response_cache_key_for_messages(&provider_messages, &effective_model);

        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key) {
            if let Ok(Some(cached)) = cache.get(key) {
                self.observer.record_event(&ObserverEvent::CacheHit {
                    cache_type: "response".into(),
                    tokens_saved: 0,
                });
                self.history
                    .push(ConversationMessage::Chat(ChatMessage::assistant(
                        cached.clone(),
                    )));
                let _ = self.trim_history(Some(&turn_id));
                // Cache hit: no provider call, but the block is durably in
                // `self.history` for the next turn. See the same exit in
                // `turn_streamed_with_steering_state`.
                return crate::agent::loop_::settle_announcement_guards(
                    announcement_guard,
                    Ok(cached),
                );
            }
            self.observer.record_event(&ObserverEvent::CacheMiss {
                cache_type: "response".into(),
            });
        }

        // Split provider_messages: loop_history gets past turns only,
        // loop_new_messages gets this turn's user message so the hook
        // can observe/modify it. Seed loop_history with the user message so
        // the provider sees it without clone-and-append.
        let split_idx = provider_messages
            .iter()
            .rposition(|m| m.role == "user")
            .unwrap_or(provider_messages.len());
        let mut loop_history = provider_messages[..split_idx].to_vec();
        let mut loop_new_messages: Vec<ChatMessage> = provider_messages[split_idx..].to_vec();

        let knobs = crate::agent::loop_::LoopKnobs {
            dedup_enabled: false,
            max_iteration_behavior: crate::agent::loop_::MaxIterationBehavior::ErrorAtCap,
            detect_protocol_without_tools: false,
        };
        // E3 never had pattern-based loop detection; default pacing turns it
        // on. Keep the embedder contract (an N-step identical-args tool chain
        // completes) until the Agent surface grows a pacing config of its own.
        let pacing = zeroclaw_config::schema::PacingConfig {
            loop_detection_enabled: false,
            ..zeroclaw_config::schema::PacingConfig::default()
        };

        // Keep the loop call as a plain `.await` on this task. Caller-scoped
        // task-locals (session key, cost tracking, tool choice / thinking
        // overrides) silently vanish across a spawn.
        let cost_context = self.tool_loop_cost_tracking_context();
        let receipt_scope = crate::agent::tool_receipts::ReceiptScope::from_config(
            &self.config.resolved.tool_receipts,
        );
        let agent_alias_for_loop = self.observer_agent_alias();
        let loop_result = crate::agent::loop_::TOOL_LOOP_COST_TRACKING_CONTEXT
            .scope(
                Some(cost_context.clone()),
                crate::agent::tool_receipts::scope_receipts(
                    receipt_scope.clone(),
                    crate::agent::loop_::run_tool_call_loop(crate::agent::loop_::ToolLoop {
                        exec: crate::agent::loop_::ResolvedAgentExecution::resolve(
                            crate::agent::loop_::ResolvedModelAccess {
                                model_provider: self.model_provider.as_ref(),
                                provider_name: &self.model_provider_name,
                                model: &effective_model,
                                temperature: self.temperature,
                            },
                            crate::agent::loop_::ResolvedIo {
                                tools_registry: &self.tools,
                                observer: self.observer.as_ref(),
                                silent: false,
                                approval: self.approval_manager.as_deref(),
                                multimodal_config: &self.multimodal_config,
                                // Inlined `full_config()` (per-field borrow) so it coexists with
                                // the `&mut self.image_cache` in this same ToolLoop expression.
                                config: self
                                    .provider_switch_config
                                    .as_ref()
                                    .and_then(|c| c.config.as_deref()),
                                hooks: self.hook_runner.as_deref(),
                                activated_tools: self.activated_tools.as_ref(),
                                model_switch_callback: None,
                                receipt_generator: receipt_scope
                                    .as_ref()
                                    .map(crate::agent::tool_receipts::ReceiptScope::generator),
                            },
                            crate::agent::loop_::ResolvedRuntimeKnobs {
                                max_tool_iterations: self.config.resolved.max_tool_iterations,
                                excluded_tools: &[],
                                dedup_exempt_tools: &self.config.resolved.tool_call_dedup_exempt,
                                pacing: &pacing,
                                strict_tool_parsing: self.config.resolved.strict_tool_parsing,
                                parallel_tools: self.config.resolved.parallel_tools,
                                max_tool_result_chars: self.config.resolved.max_tool_result_chars,
                                context_token_budget: self
                                    .config
                                    .resolved
                                    .effective_context_budget(),
                                knobs: &knobs,
                            },
                        ),
                        history: &mut loop_history,
                        channel_name: &self.channel_name,
                        channel_reply_target: None,
                        cancellation_token: None,
                        on_delta: None,
                        shared_budget: None,
                        channel: None,
                        collected_receipts: receipt_scope
                            .as_ref()
                            .map(crate::agent::tool_receipts::ReceiptScope::collector),
                        event_tx: None,
                        steering: None,
                        new_messages_out: Some(&mut loop_new_messages),
                        image_cache: Some(&mut self.image_cache),
                        // Direct embedded Agent::turn call; source/transport/
                        // trust stay placeholders, not yet stamped at the edge.
                        memory: Some(crate::agent::memory_inject::TurnMemory {
                            handle: self.memory.as_ref(),
                            query: user_message.to_string(),
                            sessions: vec![self.memory_session_id.clone()],
                            suppress: false,
                            cfg: self.memory_inject_cfg,
                        }),
                        ingress: zeroclaw_api::ingress::IngressContext::agent_direct(),
                        agent_alias: agent_alias_for_loop.as_deref(),
                        parent_agent_alias: None,
                        turn_id: &turn_id,
                    }),
                ),
            )
            .await;

        // Feed the accumulated per-call usage into the AgentEnd guard before
        // any return below drops it — including the error path, which must
        // still report usage from calls that succeeded earlier in the turn.
        let usage = cost_context.snapshot_turn_usage();
        if usage.input_tokens > 0 || usage.output_tokens > 0 {
            guard.set_usage(
                Some(zeroclaw_api::observability_traits::TurnTokenUsage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                }),
                None,
            );
        }
        // Pop the original user message (pushed before the loop) so the
        // replayed version — which includes the user message, possibly
        // modified by the hook.
        self.history.pop();
        for replayed in Self::replay_loop_messages(&loop_new_messages) {
            self.history.push(replayed);
        }
        let response = match loop_result {
            Ok(response) => response,
            Err(error) => {
                let _ = self.trim_history(Some(&turn_id));
                return Err(error);
            }
        };

        let response = self.append_receipts_block(response, receipt_scope.as_ref());

        // Store in the response cache only when the turn was a single
        // tool-free exchange (exactly one assistant message), mirroring the
        // old "no tool calls" put condition.
        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key)
            && loop_new_messages.len() == 2
            && loop_new_messages
                .last()
                .is_some_and(|m| m.role == "assistant")
        {
            #[allow(clippy::cast_possible_truncation)]
            let _ = cache.put(key, &effective_model, &response, usage.output_tokens as u32);
        }

        let _ = self.trim_history(Some(&turn_id));

        // Success point: the tool loop returned, so the provider was called
        // with the history containing the announcement block. Every other exit
        // above leaves the guard to drop armed.
        crate::agent::loop_::settle_announcement_guards(announcement_guard, Ok(response))
    }

    pub async fn turn_streamed(
        &mut self,
        user_message: &str,
        event_tx: tokio::sync::mpsc::Sender<TurnEvent>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<(String, Vec<ConversationMessage>)> {
        // See `Agent::turn` for the rationale. Same guard: blank input would
        // push a timestamp-only user message into history and the model would
        // narrate the trailing prompt-template sentinel instead of replying.
        if user_message.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "reason": "empty_user_message",
                        "entry_point": "Agent::turn_streamed",
                        "raw_len": user_message.len(),
                    })),
                "Refusing blank user turn (would emit timestamp-only message and risk prompt-template bleed-through)"
            );
            return Err(anyhow::Error::msg(
                "empty user message: refusing to dispatch a blank turn",
            ));
        }

        self.turn_streamed_with_steering_state(user_message, event_tx, cancel_token, None)
            .await
            .map(|outcome| (outcome.response, outcome.new_messages))
            .map_err(|err| err.error)
    }

    pub async fn turn_streamed_with_steering_state(
        &mut self,
        user_message: &str,
        event_tx: tokio::sync::mpsc::Sender<TurnEvent>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        mut steering_rx: Option<&mut tokio::sync::mpsc::Receiver<String>>,
    ) -> std::result::Result<StreamedTurnSuccess, StreamedTurnError> {
        // See `Agent::turn` for the rationale. Same guard: blank input would
        // push a timestamp-only user message into history and the model would
        // narrate the trailing prompt-template sentinel instead of replying.
        if user_message.trim().is_empty() {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "reason": "empty_user_message",
                        "entry_point": "Agent::turn_streamed_with_steering_state",
                        "raw_len": user_message.len(),
                    })),
                "Refusing blank user turn (would emit timestamp-only message and risk prompt-template bleed-through)"
            );
            return Err(StreamedTurnError {
                error: anyhow::Error::msg("empty user message: refusing to dispatch a blank turn"),
                committed_response: String::new(),
                new_messages: Vec::new(),
            });
        }

        // ── Preamble (identical to turn) ───────────────────────────────
        if self.history.is_empty() {
            let system_prompt = self
                .build_system_prompt()
                .map_err(|error| StreamedTurnError {
                    error,
                    committed_response: String::new(),
                    new_messages: Vec::new(),
                })?;
            self.history
                .push(ConversationMessage::Chat(ChatMessage::system(
                    system_prompt,
                )));
        }

        let mut new_msgs: Vec<ConversationMessage> = Vec::new();
        // `effective_model` is `mut` so a `model_switch` requested mid-turn
        // (handled in the round loop's `ModelSwitchRequested` arm via
        // `try_apply_model_switch`) can rebind it for later rounds
        let mut effective_model = self.classify_model(user_message);
        let turn_id = Self::new_turn_id();
        let mut committed_response = String::new();
        // Requested-vs-served divergence for THIS turn. Source of truth is the
        // task-local record inside `zeroclaw_providers::reliable`, consumed
        // once per round below; this is a per-turn transient resolved at
        // use-time, never stored on the agent.
        let mut turn_model_fallback: Option<zeroclaw_providers::reliable::ProviderFallbackInfo> =
            None;
        let turn_observer = Arc::clone(&self.observer);
        let mut guard = crate::observability::AgentTurnGuard::start(
            turn_observer.as_ref(),
            self.model_provider_name.clone(),
            effective_model.clone(),
            Some(self.channel_name.clone()),
            self.observer_agent_alias(),
            Some(turn_id.clone()),
        );
        // One guard per user message claimed into this turn: the opening one
        // here, plus one for each mid-turn steering message drained below.
        // They are settled together against this turn's one outcome at every
        // success exit; any other exit drops them armed and the announcements
        // go back to the store.
        let mut announcement_guards: Vec<crate::agent::loop_::UnclaimOnDrop> = Vec::new();
        announcement_guards.extend(
            self.append_streamed_user_message_to_history(user_message, &mut new_msgs, &turn_id)
                .await,
        );

        let active_dispatcher = {
            let base_provider_messages = self.tool_dispatcher.to_provider_messages(&self.history);
            let (vision_provider_box, _degrade_strip_images) =
                match crate::agent::turn::resolve_vision_provider(
                    self.full_config(),
                    self.model_provider.as_ref(),
                    &base_provider_messages,
                    &self.multimodal_config,
                    &self.model_provider_name,
                    &effective_model,
                ) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        let notice = self.trim_history(Some(&turn_id));
                        forward_history_trim_notice(&event_tx, notice).await;
                        return Err(StreamedTurnError {
                            error,
                            committed_response: String::new(),
                            new_messages: new_msgs,
                        });
                    }
                };
            let active_provider: &dyn ModelProvider = vision_provider_box
                .as_ref()
                .map(|resolved| resolved.provider.as_ref())
                .unwrap_or(self.model_provider.as_ref());
            tool_dispatcher_for_provider(&self.config, active_provider)
        };

        if let Err(error) = self.rebuild_system_prompt_for_dispatcher(active_dispatcher.as_ref()) {
            let notice = self.trim_history(Some(&turn_id));
            forward_history_trim_notice(&event_tx, notice).await;
            return Err(StreamedTurnError {
                error,
                committed_response: String::new(),
                new_messages: new_msgs,
            });
        }

        let provider_messages = active_dispatcher.to_provider_messages(&self.history);
        let cache_key = self.response_cache_key_for_messages(&provider_messages, &effective_model);

        if let (Some(cache), Some(key)) = (&self.response_cache, &cache_key) {
            if let Ok(Some(cached)) = cache.get(key) {
                self.observer.record_event(&ObserverEvent::CacheHit {
                    cache_type: "response".into(),
                    tokens_saved: 0,
                });
                let cached_msg = ConversationMessage::Chat(ChatMessage::assistant(cached.clone()));
                new_msgs.push(cached_msg.clone());
                self.history.push(cached_msg);
                let notice = self.trim_history(Some(&turn_id));
                forward_history_trim_notice(&event_tx, notice).await;
                self.observer.record_event(&ObserverEvent::TurnComplete);
                committed_response.push_str(&cached);
                // A cache hit is a completed turn: no provider call happened,
                // but the announcement block is in `self.history`, which this
                // pipeline carries into the next turn — so the news is not
                // lost, and returning it to the store would show it twice.
                return crate::agent::loop_::settle_announcement_guards(
                    announcement_guards,
                    Ok(StreamedTurnSuccess {
                        response: committed_response,
                        new_messages: new_msgs,
                    }),
                );
            }
            self.observer.record_event(&ObserverEvent::CacheMiss {
                cache_type: "response".into(),
            });
        }

        // Split provider_messages: loop_history gets past turns, user_msg_for_loop
        // seeds round 0's round_added so the hook can observe/modify the user message.
        let split_idx = provider_messages
            .iter()
            .rposition(|m| m.role == "user")
            .unwrap_or(provider_messages.len());
        let mut loop_history = provider_messages[..split_idx].to_vec();
        let user_msg_for_loop: Vec<ChatMessage> = provider_messages[split_idx..].to_vec();

        let approval_bridge: Option<Box<dyn zeroclaw_api::channel::Channel>> =
            self.channel_handles.ask_user.as_ref().map(|handles| {
                Box::new(crate::agent::approval_bridge::AskUserApprovalBridge::new(
                    Arc::clone(handles),
                    self.approval_route.clone(),
                )) as Box<dyn zeroclaw_api::channel::Channel>
            });

        let knobs = crate::agent::loop_::LoopKnobs {
            dedup_enabled: false,
            max_iteration_behavior: crate::agent::loop_::MaxIterationBehavior::GracefulSummary,
            detect_protocol_without_tools: false,
        };
        // The streaming engine never had pattern-based loop detection; default
        // pacing turns it on. Keep the embedder contract until this surface
        // grows a pacing config of its own (matches `Agent::turn`).
        let pacing = zeroclaw_config::schema::PacingConfig {
            loop_detection_enabled: false,
            ..zeroclaw_config::schema::PacingConfig::default()
        };

        let cost_context = self.tool_loop_cost_tracking_context();
        let agent_alias_for_loop = self.observer_agent_alias();

        // Built once per turn so the HMAC key is stable across steering rounds
        // and the same collector accumulates every round's receipts. `None`
        // when receipts are disabled, gated by the one shared seam.
        let receipt_scope = crate::agent::tool_receipts::ReceiptScope::from_config(
            &self.config.resolved.tool_receipts,
        );

        // ── Round loop: one tool-call-loop run per steering round ──────────
        for round in 0..self.config.resolved.max_tool_iterations {
            // Early exit if the caller cancelled this turn (e.g. user abort)
            if cancel_token
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
            {
                let marker = crate::i18n::get_required_cli_string("turn-interrupted-by-user");
                let interruption =
                    ConversationMessage::Chat(ChatMessage::assistant(marker.clone()));
                new_msgs.push(interruption.clone());
                self.history.push(interruption);
                committed_response.push_str(&marker);
                let notice = self.trim_history(Some(&turn_id));
                forward_history_trim_notice(&event_tx, notice).await;
                return Err(StreamedTurnError {
                    error: crate::agent::loop_::ToolLoopCancelled.into(),
                    committed_response,
                    new_messages: new_msgs,
                });
            }

            let mut round_added: Vec<ChatMessage> = if round == 0 {
                user_msg_for_loop.clone()
            } else {
                Vec::new()
            };

            // Steering drain: each accepted mid-turn message becomes its own
            // enriched user turn in both transcripts before the next round.
            //
            // This claims again inside a turn that already claimed once, and
            // that is deliberate, not an oversight: a steering message is a
            // fresh user turn in everything but name, it gets its own round
            // with the provider, and any child that finished since the turn
            // started is news the model can still act on. Delivering it with
            // the message that triggered it is better than holding it back
            // until the whole turn ends.
            for steering_message in crate::agent::loop_::drain_steering_messages(&mut steering_rx) {
                // Mirror the enrichment logic from append_streamed_user_message_to_history
                // but route through round_added instead of self.history/new_msgs, so
                // the before-llm-call hook sees steering messages as round-local context
                // without polluting the durable history (PR-A behavioral equivalence).
                if self.auto_save {
                    let store_start = std::time::Instant::now();
                    let store_result = self
                        .memory
                        .store(
                            "user_msg",
                            &steering_message,
                            MemoryCategory::Conversation,
                            self.memory_session_id.as_deref(),
                        )
                        .await;
                    self.observer.record_event(&ObserverEvent::MemoryStore {
                        category: MemoryCategory::Conversation.to_string(),
                        backend: self.memory.name().to_string(),
                        duration: store_start.elapsed(),
                        success: store_result.is_ok(),
                        channel: Some(self.channel_name.clone()),
                        agent_alias: self.observer_agent_alias(),
                        turn_id: Some(turn_id.clone()),
                    });
                }
                // Claim finished background children for this steering turn —
                // same as append_streamed_user_message_to_history, but routed to
                // round_added instead of self.history.
                let (announcements, guard) =
                    crate::agent::loop_::claim_announcements_for_turn(true).await;
                if let Some(g) = guard {
                    announcement_guards.push(g);
                }
                let now = self.current_turn_datetime().format("%Y-%m-%d %H:%M:%S %Z");
                let enriched = format!("{announcements}[{now}] {steering_message}");
                round_added.push(ChatMessage::user(enriched));
            }
            let round_loop = crate::agent::loop_::TOOL_LOOP_COST_TRACKING_CONTEXT.scope(
                Some(cost_context.clone()),
                crate::agent::tool_receipts::scope_receipts(
                    receipt_scope.clone(),
                    crate::agent::loop_::run_tool_call_loop(crate::agent::loop_::ToolLoop {
                        exec: crate::agent::loop_::ResolvedAgentExecution::resolve(
                            crate::agent::loop_::ResolvedModelAccess {
                                model_provider: self.model_provider.as_ref(),
                                provider_name: &self.model_provider_name,
                                model: &effective_model,
                                temperature: self.temperature,
                            },
                            crate::agent::loop_::ResolvedIo {
                                tools_registry: &self.tools,
                                observer: self.observer.as_ref(),
                                silent: true,
                                approval: self.approval_manager.as_deref(),
                                multimodal_config: &self.multimodal_config,
                                // Inlined `full_config()` (per-field borrow) so it coexists with
                                // the `&mut self.image_cache` in this same ToolLoop expression.
                                config: self
                                    .provider_switch_config
                                    .as_ref()
                                    .and_then(|c| c.config.as_deref()),
                                hooks: self.hook_runner.as_deref(),
                                activated_tools: self.activated_tools.as_ref(),
                                // `None` here (rather than a shared global) is
                                // deliberate: `run_tool_call_loop` mints a fresh,
                                // task-local switch state for this round when it
                                // sees `None`, so a `model_switch` requested this
                                // round can never leak into a sibling round or a
                                // concurrently running turn/agent.
                                model_switch_callback: None,
                                receipt_generator: receipt_scope
                                    .as_ref()
                                    .map(crate::agent::tool_receipts::ReceiptScope::generator),
                            },
                            crate::agent::loop_::ResolvedRuntimeKnobs {
                                max_tool_iterations: self.config.resolved.max_tool_iterations,
                                excluded_tools: &[],
                                dedup_exempt_tools: &self.config.resolved.tool_call_dedup_exempt,
                                pacing: &pacing,
                                strict_tool_parsing: self.config.resolved.strict_tool_parsing,
                                parallel_tools: self.config.resolved.parallel_tools,
                                max_tool_result_chars: self.config.resolved.max_tool_result_chars,
                                context_token_budget: self
                                    .config
                                    .resolved
                                    .effective_context_budget(),
                                knobs: &knobs,
                            },
                        ),
                        history: &mut loop_history,
                        channel_name: &self.channel_name,
                        channel_reply_target: None,
                        cancellation_token: cancel_token.clone(),
                        on_delta: None,
                        shared_budget: None,
                        channel: approval_bridge.as_deref(),
                        collected_receipts: receipt_scope
                            .as_ref()
                            .map(crate::agent::tool_receipts::ReceiptScope::collector),
                        event_tx: Some(event_tx.clone()),
                        steering: None,
                        new_messages_out: Some(&mut round_added),
                        image_cache: Some(&mut self.image_cache),
                        // Direct embedded Agent::turn call; source/transport/
                        // trust stay placeholders, not yet stamped at the edge.
                        memory: Some(crate::agent::memory_inject::TurnMemory {
                            handle: self.memory.as_ref(),
                            query: user_message.to_string(),
                            sessions: vec![self.memory_session_id.clone()],
                            suppress: false,
                            cfg: self.memory_inject_cfg,
                        }),
                        ingress: zeroclaw_api::ingress::IngressContext::agent_direct(),
                        agent_alias: agent_alias_for_loop.as_deref(),
                        parent_agent_alias: None,
                        turn_id: &turn_id,
                    }),
                ),
            );
            // Scope the provider-fallback task-local around the round so the
            // resilient wrapper's requested-vs-served record is visible here,
            // then read it immediately (same pattern as the channels
            // orchestrator's `scope_provider_fallback` wrapping). Box::pin
            // moves the round future to the heap: nesting it inside another
            // async block otherwise grows the turn future past the tokio
            // worker stack in debug builds (observed live as a worker-thread
            // stack overflow aborting the gateway).
            let (loop_result, round_fallback) =
                zeroclaw_providers::reliable::scope_provider_fallback(async {
                    let result = Box::pin(round_loop).await;
                    (
                        result,
                        zeroclaw_providers::reliable::take_last_provider_fallback(),
                    )
                })
                .await;
            if round_fallback.is_some() {
                turn_model_fallback = round_fallback;
            }

            // Feed cumulative usage into the AgentEnd guard before any return
            // below drops it — the error paths must still report usage from
            // calls that succeeded earlier in the turn.
            let usage = cost_context.snapshot_turn_usage();
            if usage.input_tokens > 0 || usage.output_tokens > 0 {
                guard.set_usage(
                    Some(zeroclaw_api::observability_traits::TurnTokenUsage {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                    }),
                    None,
                );
            }

            // round_added now contains the user message for round 0;
            // a single tool-free exchange is [user, assistant].
            let single_text_exchange = round == 0
                && round_added.len() == 2
                && round_added.first().is_some_and(|m| m.role == "user")
                && round_added.last().is_some_and(|m| m.role == "assistant");

            if round == 0 {
                self.history.pop();
                new_msgs.pop();
            }
            for replayed in Self::replay_loop_messages(&round_added) {
                new_msgs.push(replayed.clone());
                self.history.push(replayed);
            }

            match loop_result {
                Ok(response) => {
                    // Commit-before-drain: this round's assistant output is in
                    // history/new_msgs (replay above) and committed_response
                    // before any steering continuation is folded in.
                    committed_response.push_str(&response);
                    let notice = self.trim_history(Some(&turn_id));
                    forward_history_trim_notice(&event_tx, notice).await;

                    let has_more_steering =
                        steering_rx.as_deref_mut().is_some_and(|rx| !rx.is_empty());
                    if has_more_steering {
                        continue;
                    }

                    // Cache put only when the turn was a single tool-free
                    // exchange, mirroring the old "no tool calls" condition.
                    if single_text_exchange
                        && let (Some(cache), Some(key)) = (&self.response_cache, &cache_key)
                    {
                        #[allow(clippy::cast_possible_truncation)]
                        let _ =
                            cache.put(key, &effective_model, &response, usage.output_tokens as u32);
                    }

                    self.observer.record_event(&ObserverEvent::TurnComplete);
                    let committed_response =
                        self.append_receipts_block(committed_response, receipt_scope.as_ref());
                    let committed_response = Self::append_model_fallback_notice(
                        committed_response,
                        turn_model_fallback.as_ref(),
                        &event_tx,
                    )
                    .await;
                    // Success point: the round loop returned, which means the
                    // provider was called and the model read every user
                    // message this turn appended — the opening one and each
                    // steering message. A steering continuation `continue`s
                    // above without settling, so this runs once per turn.
                    return crate::agent::loop_::settle_announcement_guards(
                        announcement_guards,
                        Ok(StreamedTurnSuccess {
                            response: committed_response,
                            new_messages: new_msgs,
                        }),
                    );
                }
                Err(error) => {
                    // Model switch requested mid-turn: the unified loop
                    // signals a pending `model_switch` by returning
                    // `ModelSwitchRequested`. The
                    // round's tool call + result are already replayed into
                    // history/new_msgs above; rebuild the provider from the
                    // captured `ProviderSwitchConfig` and continue the round
                    // loop so the next provider call uses the switched
                    // provider/model. A failed rebuild (no switch config / build
                    // error) falls through to the normal error handling below.
                    if let Some((new_model_provider, new_model)) =
                        crate::agent::loop_::is_model_switch_requested(&error)
                        && let Some(new_effective_model) = self.try_apply_model_switch(
                            &effective_model,
                            new_model_provider,
                            new_model,
                        )
                    {
                        let notice = self.trim_history(Some(&turn_id));
                        forward_history_trim_notice(&event_tx, notice).await;
                        effective_model = new_effective_model;
                        continue;
                    }
                    // Rebuild the committed text from the failed round's plain
                    // assistant output (e.g. a persisted stream partial) when
                    // no prior round committed anything.
                    if committed_response.is_empty() {
                        for replayed in Self::replay_loop_messages(&round_added) {
                            if let ConversationMessage::Chat(message) = &replayed
                                && message.role == "assistant"
                            {
                                committed_response.push_str(&message.content);
                            }
                        }
                    }
                    let error = if crate::agent::loop_::is_tool_loop_cancelled(&error) {
                        // When the cancel arrived after event-visible
                        // streamed text, the error itself carries the
                        // partial the loop persisted (replayed into
                        // history/new_msgs above, and into
                        // committed_response by the empty-committed
                        // rebuild). Provenance, not content sniffing:
                        // model-authored text can end with the marker
                        // literal, so suffix-matching round_added would
                        // misfire. Synthesize the bare marker only when no
                        // interruption text was persisted this round.
                        let marker =
                            crate::i18n::get_required_cli_string("turn-interrupted-by-user");
                        let persisted_interruption = error
                            .downcast_ref::<crate::agent::loop_::StreamCancelledAfterOutput>()
                            .map(|cancelled| format!("{}\n\n{marker}", cancelled.partial_text));
                        match persisted_interruption {
                            Some(text) => {
                                if !committed_response.ends_with(&marker) {
                                    if !committed_response.is_empty() {
                                        committed_response.push_str("\n\n");
                                    }
                                    committed_response.push_str(&text);
                                }
                            }
                            None => {
                                committed_response.push_str(&marker);
                                let interruption = ConversationMessage::Chat(
                                    ChatMessage::assistant(marker.clone()),
                                );
                                new_msgs.push(interruption.clone());
                                self.history.push(interruption);
                            }
                        }
                        crate::agent::loop_::ToolLoopCancelled.into()
                    } else {
                        // Mark the interruption only when nothing was committed —
                        // prior-round text must round-trip unmodified.
                        if committed_response.is_empty() {
                            committed_response.push_str(&crate::i18n::get_required_cli_string(
                                "turn-stream-interrupted",
                            ));
                        }
                        error
                    };
                    let notice = self.trim_history(Some(&turn_id));
                    forward_history_trim_notice(&event_tx, notice).await;
                    return Err(StreamedTurnError {
                        error,
                        committed_response,
                        new_messages: new_msgs,
                    });
                }
            }
        }

        let notice = self.trim_history(Some(&turn_id));
        forward_history_trim_notice(&event_tx, notice).await;
        Err(StreamedTurnError {
            error: anyhow::Error::msg(format!(
                "Agent exceeded maximum tool iterations ({})",
                self.config.resolved.max_tool_iterations
            )),
            committed_response,
            new_messages: new_msgs,
        })
    }
}
