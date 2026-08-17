//! Inbound channel message processing. Extracted from orchestrator/mod.rs (god-file remainder C7).

use std::collections::HashSet;
use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use portable_atomic::Ordering;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::channel::{Channel, SendMessage};
use zeroclaw_providers::ChatMessage;
use zeroclaw_providers::ModelProvider;
use zeroclaw_providers::reliable::{scope_provider_fallback, take_last_provider_fallback};
use zeroclaw_runtime::agent::claim_announcements_for_scoped_turn;
use zeroclaw_runtime::agent::loop_::{
    LoopKnobs, ResolvedAgentExecution, ResolvedIo, ResolvedModelAccess, ResolvedRuntimeKnobs,
    ToolLoop, is_model_switch_requested, run_tool_call_loop, scope_session_key, scope_thread_id,
    scrub_credentials,
};
use zeroclaw_runtime::observability::Observer;
use zeroclaw_runtime::security::AutonomyLevel;
use zeroclaw_runtime::tools;
use zeroclaw_runtime::util::truncate_with_ellipsis;

use crate::link_enricher;
use crate::orchestrator::media_pipeline;

use super::{
    AUTOSAVE_MIN_MESSAGE_CHARS, ApprovalTypingChannel, AssistantChannelOutcome,
    CHANNEL_HOOK_MAX_OUTBOUND_CHARS, CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP, ChannelNotifyObserver,
    ChannelRouteSelection, ChannelRuntimeContext, LlmExecutionResult, ScopedTypingController,
    WHATSAPP_CURRENT_GROUP_MESSAGE_LABEL, acquire_persist_lock, append_sender_turn,
    build_channel_system_prompt_for_message_with_signal, build_channel_turn_context_preamble,
    channel_message_timeout_budget_secs_with_cap, channel_runtime_cli_string,
    channel_runtime_cli_string_with_args, classify_channel_reply_intent, clear_sender_history,
    collapse_inline_image_payloads, compact_sender_history,
    compose_outgoing_user_turn_with_context, conversation_history_key, conversation_memory_key,
    ensure_nonempty_channel_reply, extract_current_turn_tool_messages, find_channel_for_message,
    followup_thread_id, get_or_create_provider, get_route_selection,
    handle_runtime_command_if_needed, is_context_window_overflow_error, is_group_reply_target,
    maybe_apply_runtime_config_update, normalize_cached_channel_turns,
    outbound_content_format_for_channel, peer_prompt_channel_ref, provider_cache_key,
    reconcile_early_ack, record_passive_context, refreshed_new_session_system_prompt,
    resolve_channel_ack_reactions, resolve_channel_thinking, resolve_classifier_route,
    resolve_provider_ref_for_runtime_switch, rollback_orphan_user_turn,
    run_channel_turn_with_background_announcements, run_draft_updater, runtime_defaults_snapshot,
    sanitize_channel_response_for_format_with_leak_detection, send_message_to_peer_tool_available,
    sender_memory_session_ids, set_route_selection, should_bypass_reply_intent_precheck,
    should_rollback_failed_user_turn, stamp_session_routing_context,
    strip_inline_data_image_markers, strip_tool_result_content, strip_tool_summary_prefix,
    take_pending_new_session, timestamped_channel_user_history_content,
};

pub(super) async fn process_channel_message(
    ctx: Arc<ChannelRuntimeContext>,
    msg: zeroclaw_api::channel::ChannelMessage,
    cancellation_token: CancellationToken,
) {
    if cancellation_token.is_cancelled() {
        return;
    }

    let channel_composite = match &msg.channel_alias {
        Some(alias) => format!("{}.{}", msg.channel, alias),
        None => msg.channel.clone(),
    };
    let agent_alias = Arc::clone(&ctx.agent_alias);
    let sender = msg.sender.clone();
    let message_id = msg.id.clone();
    let composite_for_body = channel_composite.clone();
    zeroclaw_log::scope!(
        category: "channel",
        agent_alias: agent_alias.as_str(),
        channel: channel_composite.as_str(),
        sender: sender.as_str(),
        message_id: message_id.as_str(),
        => async move {
            process_channel_message_body(ctx, msg, cancellation_token, composite_for_body).await;
        }
    )
    .await;
}

async fn process_channel_message_body(
    ctx: Arc<ChannelRuntimeContext>,
    msg: zeroclaw_api::channel::ChannelMessage,
    cancellation_token: CancellationToken,
    channel_composite: String,
) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Inbound).with_attrs(
            ::serde_json::json!({
                "sender": msg.sender,
                "message_id": msg.id,
                "reply_target": msg.reply_target,
                "thread_ts": msg.thread_ts,
                "content": msg.content,
                "attachments_count": msg.attachments.len(),
                "passive_context": msg.passive_context,
            })
        ),
        "channel inbound message"
    );

    // ── Hook: on_message_received (modifying) ────────────
    let mut msg = if let Some(hooks) = &ctx.hooks {
        match hooks.run_on_message_received(msg).await {
            zeroclaw_runtime::hooks::HookResult::Cancel(reason) => {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"reason": reason.to_string()})),
                    "incoming message dropped by hook"
                );
                return;
            }
            zeroclaw_runtime::hooks::HookResult::Continue(modified) => modified,
        }
    } else {
        msg
    };

    let target_channel = find_channel_for_message(&ctx.channels_by_name, &msg).cloned();

    if let Some(channel) = target_channel.as_ref() {
        if channel.drop_self_messages(&msg) {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"sender": msg.sender})),
                "dropping self-authored inbound message (self-loop guard, sdk layer)"
            );
            return;
        }
        if zeroclaw_runtime::peers::should_drop_self_loop(
            &msg.sender,
            channel.self_handle().as_deref(),
        ) {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"sender": msg.sender})),
                "dropping self-authored inbound message (self-loop guard, agent-loop fallback)"
            );
            return;
        }
    }

    if let (Some(engine), Some(audit)) = (ctx.sop_engine.as_ref(), ctx.sop_audit.as_ref()) {
        let wants = engine
            .lock()
            .map(|eng| eng.wants_source(zeroclaw_runtime::sop::types::SopTriggerSource::Channel))
            .unwrap_or(false);
        if wants {
            let topic = match &msg.channel_alias {
                Some(alias) if !alias.is_empty() => format!("{}/{}", msg.channel, alias),
                _ => msg.channel.clone(),
            };
            zeroclaw_runtime::sop::dispatch::dispatch_untrusted_fan_in(
                engine,
                audit,
                zeroclaw_runtime::sop::types::SopTriggerSource::Channel,
                Some(&topic),
                Some(&msg.content),
                None,
            )
            .await;
        }
    }

    let history_key = conversation_history_key(&msg);
    stamp_session_routing_context(ctx.as_ref(), &msg, &history_key);
    if msg.passive_context {
        record_passive_context(ctx.as_ref(), &msg, &history_key);
        return;
    }

    // The early ack is spawned (fire-and-forget) so it lands before the
    // enrichment/model pipeline without blocking it. The join handle is kept so
    // any early-return reconciliation can await the add before removing the 👀,
    // making the swap deterministic instead of racing the spawned add.
    let early_ack_task: Option<tokio::task::JoinHandle<()>> =
        if resolve_channel_ack_reactions(&ctx, &msg)
            && let Some(channel) = target_channel.clone()
        {
            let reply_target = msg.reply_target.clone();
            let message_id = msg.id.clone();
            let message_id_label = message_id.clone();
            let agent_alias = Arc::clone(&ctx.agent_alias);
            let sender = msg.sender.clone();
            let channel_label = channel.name().to_string();
            let span = ::zeroclaw_log::attribution_span!(&*channel);
            Some(zeroclaw_spawn::spawn!(
            ::zeroclaw_log::scope!(
                category: "channel",
                agent_alias: agent_alias.as_str(),
                channel: channel_label.as_str(),
                sender: sender.as_str(),
                message_id: message_id_label.as_str(),
                => async move {
                    if let Err(e) = channel
                        .add_reaction(&reply_target, &message_id, "\u{1F440}")
                        .await
                    {
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "Failed to add ack reaction"
                        );
                    }
                }
            )
            .instrument(span)
        ))
        } else {
            None
        };

    let thinking_override = ctx
        .thinking_overrides
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&history_key)
        .copied();
    let thinking = resolve_channel_thinking(
        &msg.content,
        thinking_override,
        &ctx.agent_cfg.resolved.thinking,
        runtime_defaults_snapshot(ctx.as_ref()).defaults.temperature,
    );
    if thinking.effective_content != msg.content {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"thinking_level": thinking.level})),
            "Thinking directive parsed from channel message"
        );
        msg.content = thinking.effective_content.clone();
    }

    // ── Media pipeline: enrich inbound message with media annotations ──
    if ctx.media_pipeline.enabled && !msg.attachments.is_empty() {
        let vision =
            ctx.model_provider.supports_vision() || ctx.multimodal.vision_model_provider.is_some();
        // Build from legacy config; if that fails (e.g. no legacy api_key
        // but typed providers are configured), fall back to an empty shell
        // so with_typed_providers() can still populate the registry.
        let transcription_manager = {
            let base = crate::transcription::TranscriptionManager::new(&ctx.transcription_config)
                .unwrap_or_else(|_| crate::transcription::TranscriptionManager::empty());
            let m = base
                .with_typed_providers(&ctx.prompt_config.providers.transcription)
                .with_agent_transcription_provider(ctx.agent_transcription_provider.clone());
            if m.available_providers().is_empty() {
                None
            } else {
                Some(m)
            }
        };
        let pipeline = media_pipeline::MediaPipeline::new(
            &ctx.media_pipeline,
            transcription_manager.as_ref(),
            vision,
        );
        msg.content = Box::pin(pipeline.process(&msg.content, &msg.attachments)).await;
    }

    // ── Link enricher: prepend URL summaries before agent sees the message ──
    let le_config = &ctx.prompt_config.link_enricher;
    if le_config.enabled {
        let enricher_cfg = link_enricher::LinkEnricherConfig {
            enabled: le_config.enabled,
            max_links: le_config.max_links,
            timeout_secs: le_config.timeout_secs,
        };
        let enriched = link_enricher::enrich_message(&msg.content, &enricher_cfg).await;
        if enriched != msg.content {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"sender": msg.sender})),
                "Link enricher: prepended URL summaries to message"
            );
            msg.content = enriched;
        }
    }

    if let Err(err) = maybe_apply_runtime_config_update(ctx.as_ref()).await {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": format!("{}", err)})),
            "Failed to apply runtime config update"
        );
    }
    if handle_runtime_command_if_needed(ctx.as_ref(), &msg, target_channel.as_ref()).await {
        reconcile_early_ack(
            ctx.as_ref(),
            &msg,
            target_channel.as_ref(),
            early_ack_task,
            Some("\u{2705}"),
        )
        .await;
        return;
    }

    let runtime_defaults = runtime_defaults_snapshot(ctx.as_ref());
    let mut route = get_route_selection(ctx.as_ref(), &msg, &history_key, &runtime_defaults);

    if let Some(hint) =
        zeroclaw_runtime::agent::classifier::classify(&ctx.query_classification, &msg.content)
        && let Some(matched_route) = ctx
            .model_routes
            .iter()
            .find(|r| r.hint.eq_ignore_ascii_case(&hint))
    {
        ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"hint": hint.as_str(), "model_provider": matched_route.model_provider.as_str(), "model": matched_route.model.as_str()})), "Channel message classified — overriding route");
        route = ChannelRouteSelection {
            model_provider: matched_route.model_provider.clone(),
            model: matched_route.model.clone(),
            api_key: matched_route.api_key.clone(),
        };
    }

    let mut active_model_provider = match get_or_create_provider(
        ctx.as_ref(),
        &route.model_provider,
        route.api_key.as_deref(),
        &runtime_defaults,
    )
    .await
    {
        Ok(model_provider) => model_provider,
        Err(err) => {
            let safe_err = zeroclaw_providers::sanitize_api_error(&err.to_string());
            let message = channel_runtime_cli_string_with_args(
                "channel-runtime-provider-turn-init-failed",
                &[
                    ("provider", route.model_provider.as_str()),
                    ("error", safe_err.as_str()),
                ],
            );
            if let Some(channel) = target_channel.as_ref() {
                let _ = channel.send(&SendMessage::reply_to(&msg, message)).await;
            }
            reconcile_early_ack(
                ctx.as_ref(),
                &msg,
                target_channel.as_ref(),
                early_ack_task,
                Some("\u{26A0}\u{FE0F}"),
            )
            .await;
            return;
        }
    };
    let history_user_content = msg.content.clone();
    // Autosave must not persist heavy/private inline `data:` image bytes into
    // durable memory. Strip them here (path/markers are preserved) before the
    // store; the channel-history cache still keeps the re-loadable markers via
    // collapse_inline_image_payloads downstream.
    let autosave_content = strip_inline_data_image_markers(&history_user_content);
    if ctx.auto_save_memory
        && autosave_content.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS
        && !zeroclaw_memory::should_skip_autosave_content(&autosave_content)
    {
        let autosave_key = conversation_memory_key(&msg);
        let _ = ctx
            .memory
            .store(
                &autosave_key,
                &autosave_content,
                zeroclaw_memory::MemoryCategory::Conversation,
                Some(&history_key),
            )
            .await;
    }

    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(::serde_json::json!({"message_id": msg.id})),
        "processing inbound message"
    );
    let started_at = Instant::now();

    let force_fresh_session = take_pending_new_session(ctx.as_ref(), &history_key);
    if force_fresh_session {
        // `/new` should make the next user turn completely fresh even if
        // older cached turns reappear before this message starts.
        // Serialize per-sender persistence to prevent interleaving
        let persist_lock = acquire_persist_lock(ctx.as_ref(), &history_key);
        let _lock = persist_lock.lock().unwrap_or_else(|e| e.into_inner());
        clear_sender_history(ctx.as_ref(), &history_key);
    }

    let had_prior_history = if force_fresh_session {
        false
    } else {
        ctx.conversation_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .peek(&history_key)
            .is_some_and(|turns| !turns.is_empty())
    };

    // Preserve the dated user turn verbatim before the LLM call so interrupted
    // requests keep the same temporal context as CLI turns. History stores the
    // full content for every marker type so a later turn can re-load it.
    let timestamped_content =
        timestamped_channel_user_history_content(&msg, WHATSAPP_CURRENT_GROUP_MESSAGE_LABEL);
    append_sender_turn(
        ctx.as_ref(),
        &history_key,
        ChatMessage::user(&timestamped_content),
    );

    // Build history from per-sender conversation cache.
    let prior_turns_raw = if force_fresh_session {
        vec![ChatMessage::user(&timestamped_content)]
    } else {
        ctx.conversation_histories
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&history_key)
            .cloned()
            .unwrap_or_default()
    };
    let mut prior_turns = normalize_cached_channel_turns(prior_turns_raw);

    // Strip stale tool_result blocks from cached turns so the LLM never
    // sees a `<tool_result>` without a preceding `<tool_call>`, which
    // causes hallucinated output on subsequent heartbeat ticks or sessions.
    for turn in &mut prior_turns {
        if turn.content.contains("<tool_result") {
            turn.content = strip_tool_result_content(&turn.content);
        }
    }

    // Strip [Used tools: ...] prefixes from cached assistant turns so the
    // LLM never sees (and reproduces) this internal summary format.
    for turn in &mut prior_turns {
        if turn.role == "assistant" && turn.content.starts_with("[Used tools:") {
            turn.content = strip_tool_summary_prefix(&turn.content);
        }
    }

    // Collapse only heavy inline `data:` image payloads in older cached turns.
    // Re-loadable `[IMAGE:<path>]` references survive so a later turn can
    // re-inflate from disk inline base64 is dropped to keep history
    // within the context budget
    collapse_inline_image_payloads(&mut prior_turns);

    let is_group_chat = is_group_reply_target(&msg.reply_target);
    let mut memory_sessions: Vec<Option<String>> = sender_memory_session_ids(&msg, &history_key)
        .into_iter()
        .map(Some)
        .collect();
    if is_group_chat {
        memory_sessions.push(Some(history_key.clone()));
    }

    let base_system_prompt = if had_prior_history {
        ctx.system_prompt.as_str().to_string()
    } else {
        refreshed_new_session_system_prompt(ctx.as_ref())
    };
    let per_turn_excluded_tools: &[String] =
        if msg.channel == "cli" || ctx.autonomy_level == AutonomyLevel::Full {
            &[]
        } else {
            ctx.non_cli_excluded_tools.as_ref()
        };
    let per_turn_native_tool_specs_present =
        ::zeroclaw_runtime::agent::loop_::native_tool_specs_present_for_turn(
            active_model_provider.as_ref(),
            ctx.tools_registry.as_ref(),
            per_turn_excluded_tools,
            ctx.activated_tools.as_ref(),
        )
        .unwrap_or(false);
    let mut system_prompt = build_channel_system_prompt_for_message_with_signal(
        &base_system_prompt,
        &msg,
        target_channel.as_ref(),
        per_turn_native_tool_specs_present,
    );
    if let Some(user_model) = ctx.user_model().cloned() {
        let heads = tokio::task::spawn_blocking(move || user_model.active_heads(None))
            .await
            .ok()
            .and_then(Result::ok);
        match heads {
            Some(heads) => {
                let projection = zeroclaw_memory::companion::project_active_heads(
                    &heads,
                    zeroclaw_memory::companion::USER_MODEL_PROJECTION_DEFAULT_MAX_CHARS,
                );
                if !projection.prompt_section.is_empty() {
                    let _ = write!(system_prompt, "\n\n{}", projection.prompt_section);
                }
            }
            None => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                    "user model read failed; owner-profile projection skipped for this turn"
                );
            }
        }
    }
    if send_message_to_peer_tool_available(ctx.as_ref(), &msg)
        && let Some(current_channel_ref) = peer_prompt_channel_ref(ctx.as_ref(), &msg)
    {
        let peer_map =
            zeroclaw_runtime::tools::send_message_to_peer::render_sender_peer_map_for_channel(
                ctx.prompt_config.as_ref(),
                ctx.agent_alias.as_str(),
                &current_channel_ref,
            );
        if !peer_map.is_empty() {
            let _ = write!(system_prompt, "\n\n{peer_map}");
        }
    }
    // NOTE: memory_context is intentionally NOT appended to the system prompt
    // here — it carries per-turn data that would invalidate the provider-side
    // prompt cache The preamble below carries it into the outgoing
    // user turn instead, matching the CLI shape.
    if let Some(ref prefix) = thinking.params.system_prompt_prefix {
        system_prompt = format!("{prefix}\n\n{system_prompt}");
    }
    let mut history = vec![ChatMessage::system(system_prompt)];
    history.extend(prior_turns);

    let preamble = build_channel_turn_context_preamble(&msg, target_channel.as_ref());
    if let Some(last_turn) = history.last_mut()
        && last_turn.role == "user"
    {
        let raw_content = last_turn.content.clone();
        last_turn.content = compose_outgoing_user_turn_with_context(&preamble, &raw_content);
    }

    // ── Reply-intent precheck ────────────────────────────────────────
    let direct_message = target_channel
        .as_ref()
        .map(|c| c.is_direct_message(&msg))
        .unwrap_or(false);
    let precheck = &ctx.agent_cfg.precheck;
    let classifier_intent = ::zeroclaw_log::scope!(
        category: "channel",
        model_provider: route.model_provider.as_str(),
        model: route.model.as_str(),
        => async {
            if should_bypass_reply_intent_precheck(&msg, direct_message) {
                AssistantChannelOutcome::Reply(String::new())
            } else if !precheck.enabled {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Skip).with_attrs(
                        ::serde_json::json!({
                            "phase": "precheck",
                            "reason": "disabled",
                        })
                    ),
                    "reply-intent precheck skipped"
                );
                AssistantChannelOutcome::Reply(String::new())
            } else {
                let (classifier_provider_arc, classifier_model_owned, classifier_temperature): (
                    Arc<dyn ModelProvider>,
                    String,
                    Option<f64>,
                ) = resolve_classifier_route(
                    ctx.as_ref(),
                    &ctx.agent_cfg.classifier_provider,
                    &runtime_defaults,
                )
                .await
                .unwrap_or_else(|| {
                    (
                        Arc::clone(&active_model_provider),
                        route.model.clone(),
                        None,
                    )
                });

                let started = Instant::now();
                let precheck_future = classify_channel_reply_intent(
                    classifier_provider_arc.as_ref(),
                    history[0].content.as_str(),
                    &history,
                    classifier_model_owned.as_str(),
                    classifier_temperature.or(runtime_defaults.defaults.temperature),
                );
                match tokio::time::timeout(Duration::from_secs(precheck.timeout_secs), precheck_future)
                    .await
                {
                    Ok(Ok(outcome)) => {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_duration(
                                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                                )
                                .with_attrs(::serde_json::json!({
                                    "classifier_model": classifier_model_owned.as_str(),
                                    "phase": "precheck",
                                })),
                            "reply-intent precheck completed"
                        );
                        outcome
                    }
                    Ok(Err(e)) => {
                        let safe_err = zeroclaw_providers::sanitize_api_error(&e.to_string());
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_duration(
                                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                                )
                                .with_attrs(::serde_json::json!({
                                    "classifier_model": classifier_model_owned.as_str(),
                                    "error": safe_err,
                                    "phase": "precheck",
                                })),
                            "reply-intent precheck failed open"
                        );
                        AssistantChannelOutcome::Reply(String::new())
                    }
                    Err(_) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_duration(
                                    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                                )
                                .with_attrs(::serde_json::json!({
                                    "classifier_model": classifier_model_owned.as_str(),
                                    "phase": "precheck",
                                    "timeout_secs": precheck.timeout_secs,
                                })),
                            "reply-intent precheck timed out; failing open"
                        );
                        AssistantChannelOutcome::Reply(String::new())
                    }
                }
            }
        }
    )
    .await;

    let is_acp_channel = target_channel
        .as_ref()
        .map(|c| {
            matches!(
                ::zeroclaw_api::attribution::Attributable::role(c.as_ref()),
                ::zeroclaw_api::attribution::Role::Channel(
                    ::zeroclaw_api::attribution::ChannelKind::AcpChannel
                )
            )
        })
        .unwrap_or(false);
    let reply_intent = if is_acp_channel
        && let AssistantChannelOutcome::NoReply {
            ref kind,
            ref reason,
        } = classifier_intent
    {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "kind": format!("{kind:?}"),
                    "reason": reason.as_deref().unwrap_or(""),
                })
            ),
            "ACP channel: classifier voted no_reply, overriding to reply (ACP must always respond)"
        );
        AssistantChannelOutcome::Reply(String::new())
    } else {
        classifier_intent
    };

    if let AssistantChannelOutcome::NoReply { kind, reason } = reply_intent {
        let history_response = AssistantChannelOutcome::NoReply {
            kind,
            reason: reason.clone(),
        }
        .history_marker();
        append_sender_turn(
            ctx.as_ref(),
            &history_key,
            ChatMessage::assistant(&history_response),
        );
        reconcile_early_ack(
            ctx.as_ref(),
            &msg,
            target_channel.as_ref(),
            early_ack_task,
            None,
        )
        .await;
        if resolve_channel_ack_reactions(&ctx, &msg)
            && let Some(channel) = target_channel.as_ref()
        {
            let emoji = kind.emoji();
            if let Err(e) = channel
                .add_reaction(&msg.reply_target, &msg.id, emoji)
                .await
            {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    &format!(
                        "Failed to add {emoji} no-reply reaction on {}: {e}",
                        channel.name()
                    )
                );
            }
        }
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Skip)
                .with_duration(u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),)
                .with_attrs(::serde_json::json!({
                    "model_provider": route.model_provider,
                    "model": route.model,
                    "sender": msg.sender,
                    "phase": "precheck",
                    "kind": format!("{kind:?}"),
                    "reason": reason.as_deref().unwrap_or("no reason provided"),
                })),
            "channel_message_no_reply"
        );
        return;
    }

    let use_draft_streaming = target_channel
        .as_ref()
        .is_some_and(|ch| ch.supports_draft_updates());

    ::zeroclaw_log::record!(DEBUG, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"has_target_channel": target_channel.is_some(), "use_draft_streaming": use_draft_streaming})), "Streaming decision");

    // Partial mode: delta channel for draft updates (progress + text).
    let (delta_tx, delta_rx) = if use_draft_streaming {
        let (tx, rx) = tokio::sync::mpsc::channel::<zeroclaw_runtime::agent::loop_::DraftEvent>(64);
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // Partial mode: send an initial draft message for progressive editing.
    let draft_message_id = if use_draft_streaming {
        if let Some(channel) = target_channel.as_ref() {
            match channel
                .send_draft(
                    &SendMessage::new("...", &msg.reply_target).in_thread(msg.thread_ts.clone()),
                )
                .await
            {
                Ok(id) => id,
                Err(e) => {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        &format!("Failed to send draft on {}", channel.name())
                    );
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };

    // Spawn the appropriate handler for the delta channel.
    let draft_updater = if use_draft_streaming {
        // Partial: accumulate text and edit a single draft message.
        if let (Some(rx), Some(draft_id_ref), Some(channel_ref)) = (
            delta_rx,
            draft_message_id.as_deref(),
            target_channel.as_ref(),
        ) {
            let channel = Arc::clone(channel_ref);
            let reply_target = msg.reply_target.clone();
            let draft_id = draft_id_ref.to_string();
            // Same registry the final sanitizer reads, resolved once per turn
            // rather than per delta.
            let known_tool_names: HashSet<String> = ctx
                .tools_registry
                .iter()
                .map(|tool| tool.name().to_ascii_lowercase())
                .collect();
            Some(zeroclaw_spawn::spawn!(async move {
                run_draft_updater(channel, reply_target, draft_id, known_tool_names, rx).await;
            }))
        } else {
            None
        }
    } else {
        None
    };

    // Skip typing only for Partial mode — the draft message itself provides
    // visual feedback. MultiMessage and Off both keep typing active.
    let is_partial_draft = target_channel
        .as_ref()
        .is_some_and(|ch| ch.supports_draft_updates() && !ch.supports_multi_message_streaming());
    let typing_controller = if is_partial_draft {
        None
    } else {
        target_channel.as_ref().map(|channel| {
            Arc::new(ScopedTypingController::new(
                Arc::clone(channel),
                msg.reply_target.clone(),
            ))
        })
    };
    if let Some(typing) = typing_controller.as_ref() {
        typing.resume().await;
    }
    let approval_channel: Option<Arc<dyn Channel>> =
        match (target_channel.as_ref(), typing_controller.as_ref()) {
            (Some(channel), Some(typing)) => Some(Arc::new(ApprovalTypingChannel::new(
                Arc::clone(channel),
                Arc::clone(typing),
            ))),
            (Some(channel), None) => Some(Arc::clone(channel)),
            (None, _) => None,
        };

    // Wrap observer to forward tool events as live thread messages
    // Bounded so a slow downstream channel cannot grow this queue
    // without bound. See `ChannelNotifyObserver::record_event` for the
    // drop-on-full contract.
    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel::<String>(128);
    let notify_observer: Arc<ChannelNotifyObserver> = Arc::new(ChannelNotifyObserver {
        inner: Arc::clone(&ctx.observer),
        tx: notify_tx,
        tools_used: AtomicBool::new(false),
    });
    let notify_observer_flag = Arc::clone(&notify_observer);
    let notify_channel = target_channel.clone();
    let notify_reply_target = msg.reply_target.clone();
    let notify_thread_root = followup_thread_id(&msg);
    // Tool-call notifications go out as SEPARATE messages below, which is right
    // for chat channels (Discord/Telegram threads) but wrong for partial-draft
    // channels like the git forge, where every message is a PERMANENT comment on
    // a third-party issue/PR: each tool call became its own comment (issue spam),
    // duplicating the progress the draft stream already folds into the single
    // edited comment. Partial-draft channels drain-and-drop here; their draft
    // stream remains the (single-message) tool-activity surface.
    let notify_task = if msg.channel == "cli" || !ctx.show_tool_calls || is_partial_draft {
        Some(zeroclaw_spawn::spawn!(async move {
            while notify_rx.recv().await.is_some() {}
        }))
    } else {
        Some(zeroclaw_spawn::spawn!(async move {
            let thread_ts = notify_thread_root;
            while let Some(text) = notify_rx.recv().await {
                if let Some(ref ch) = notify_channel {
                    let _ = ch
                        .send(
                            &SendMessage::new(&text, &notify_reply_target)
                                .in_thread(thread_ts.clone()),
                        )
                        .await;
                }
            }
        }))
    };

    let scale_cap = ctx
        .pacing
        .message_timeout_scale_max
        .unwrap_or(CHANNEL_MESSAGE_TIMEOUT_SCALE_CAP);
    let timeout_budget_secs = channel_message_timeout_budget_secs_with_cap(
        ctx.message_timeout_secs,
        ctx.max_tool_iterations,
        scale_cap,
    );
    let cost_tracking_context = ctx.cost_tracking.clone().map(|state| {
        zeroclaw_runtime::agent::loop_::ToolLoopCostTrackingContext::new(
            state.tracker,
            state.model_provider_pricing,
        )
        .with_agent_alias(state.agent_alias.as_str())
    });
    let llm_call_start = Instant::now();
    #[allow(clippy::cast_possible_truncation)]
    let elapsed_before_llm_ms = started_at.elapsed().as_millis() as u64;
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(::serde_json::json!({"elapsed_before_llm_ms": elapsed_before_llm_ms})),
        "starting LLM call"
    );
    // Fresh per-turn routing handle, scoped into TURN_ROUTING for the duration of
    // the tool-call loop below. Allocating per turn (rather than clearing a shared
    // handle) keeps concurrent same-agent turns from reading each other's routes.
    let turn_routing: tools::TurnRoutingHandle =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

    let tool_receipts_collector: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let receipt_scope = ctx.receipt_generator.as_ref().map(|generator| {
        zeroclaw_runtime::agent::tool_receipts::ReceiptScope {
            generator: generator.clone(),
            collector: std::sync::Arc::clone(&tool_receipts_collector),
        }
    });
    let loop_knobs = LoopKnobs::default();
    let turn_id = uuid::Uuid::new_v4().to_string();
    // Bracket the channel turn so lifecycle events
    // reach observers (and, via the broadcast hook, /api/events and
    // /api/events/history) for channel-originated turns — mirroring the CLI
    // `run` and `Agent::turn_streamed` entry points. The drop-safe guard opens
    // exactly once before the model-switch retry loop and closes on every exit.
    // A successful switch updates the closing attribution without creating a
    // second lifecycle start for the same logical turn.
    let turn_observer = Arc::clone(&ctx.observer);
    let mut turn_guard = zeroclaw_runtime::observability::AgentTurnGuard::start(
        turn_observer.as_ref(),
        route.model_provider.clone(),
        route.model.clone(),
        Some(msg.channel.to_string()),
        Some(ctx.agent_alias.to_string()),
        Some(turn_id.clone()),
    );

    // Finished background children, claimed once for this turn and spliced
    // above the user message, so a Detached completion actually reaches the
    // person on Telegram/Discord/etc. instead of sitting delivered-to-nobody.
    //
    // **Claimed through the scoping entry point, not the ambient one.** This
    // turn owns `history_key`, but it only scopes it around the tool-loop
    // future below (`scope_session_key(Some(history_key.clone()), tool_loop)`),
    // which is built after `history` — so an ambient claim here would read no
    // key at all and be a silent no-op. The runtime scopes the key for us.
    //
    // **Once per turn, not once per model-switch retry.** A retry re-enters the
    // loop with this same `history`, so the block is still in front of the
    // model on the second attempt; claiming inside the loop would consume the
    // next batch of announcements for a turn that already has one.
    //
    // **Divergence from the CLI/Agent claim sites, deliberate:** the block goes
    // into this turn's local `history` only, never into the per-sender
    // conversation cache — `append_sender_turn` above already persisted the
    // plain user text, and rewriting that entry would re-show the same
    // completion at the top of every later turn. The consequence is that later
    // turns' history does not carry the block; that is accepted, because
    // delivered-exactly-once is the contract and the assistant's persisted
    // reply is the durable record of what it was told.
    //
    // **Above the turn-context preamble, not between it and the user's text —
    // and that is a divergence, not a mirror.** The CLI site composes
    // `{hw_context}{announcements}[{now}] {msg}` (`agent/loop_.rs`), putting the
    // news closest to the message it is news about. Here the preamble is already
    // composed onto the user turn by the time this claim runs, because the claim
    // is deliberately late: it sits below the reply-intent precheck, so a turn
    // that decides to stay silent never consumes a batch, and the window in
    // which the guard has to hand rows back is as narrow as this function
    // allows. The ordering is what that narrower window costs.
    //
    // Nothing fallible sits between here and the provider call that the guard
    // does not already cover: the splice is infallible, and every path from the
    // retry loop that fails before the provider leaves the guard armed.
    //
    // **Two limits of "one claimant per conversation" on this surface, named
    // rather than assumed away.** First, `history_key` is not the dispatch key:
    // Matrix folds thread roots into one history key while the interruption
    // scope keeps them apart, so two workers for the same conversation can
    // reach this line concurrently. SQLite's single claiming statement keeps
    // that safe — no row is read twice — but one batch can arrive split across
    // two turns. Second, settling below on a succeeded turn means the model
    // read the block, not that the user received anything: an outbound send can
    // still fail afterwards. That is deliberate. The assistant's reply is
    // persisted to this conversation's history either way, so the agent keeps
    // what it was told; handing the rows back on a send failure would
    // re-announce a completion it has already acted on.
    //
    // The claim, the splice and the settle live in
    // `run_channel_turn_with_background_announcements`; this turn's execution
    // body — the model-switch retry loop below, unchanged — is what gets handed
    // to it. That is the only seam through which those three can be asserted
    // without a live orchestrator context, and the disarm-on-failed-splice case
    // that used to be spelled here now lives there with its reasoning.
    let mut fallback_info = None;
    let llm_result = run_channel_turn_with_background_announcements(
        &history_key,
        &mut history,
        async |key| claim_announcements_for_scoped_turn(key).await,
        async |history| scope_provider_fallback(async {
            let llm_result = loop {
                let thread_scope_id = msg
                    .interruption_scope_id
                    .clone()
                    .or_else(|| msg.thread_ts.clone())
                    .or_else(|| Some(msg.id.clone()));
                let excluded_tools: &[String] =
                    if msg.channel == "cli" || ctx.autonomy_level == AutonomyLevel::Full {
                        &[]
                    } else {
                        ctx.non_cli_excluded_tools.as_ref()
                    };
                let tool_loop = run_tool_call_loop(ToolLoop {
                    exec: ResolvedAgentExecution::resolve(
                        ResolvedModelAccess {
                            model_provider: active_model_provider.as_ref(),
                            provider_name: route.model_provider.as_str(),
                            model: route.model.as_str(),
                            temperature: thinking.effective_temperature,
                        },
                        ResolvedIo {
                            tools_registry: ctx.tools_registry.as_ref(),
                            observer: notify_observer.as_ref() as &dyn Observer,
                            silent: true,
                            approval: Some(&*ctx.approval_manager),
                            multimodal_config: &ctx.multimodal,
                            // Full config for the vision route to resolve the
                            // configured `vision_model_provider`'s alias options - the
                            // same canonical `prompt_config` snapshot this path already
                            // uses for provider construction.
                            config: Some(ctx.prompt_config.as_ref()),
                            hooks: ctx.hooks.as_deref(),
                            activated_tools: ctx.activated_tools.as_ref(),
                            model_switch_callback: None,
                            receipt_generator: ctx.receipt_generator.as_ref(),
                        },
                        ResolvedRuntimeKnobs {
                            max_tool_iterations: ctx.max_tool_iterations,
                            excluded_tools,
                            dedup_exempt_tools: ctx.tool_call_dedup_exempt.as_ref(),
                            pacing: &ctx.pacing,
                            strict_tool_parsing: ctx.agent_cfg.resolved.strict_tool_parsing,
                            parallel_tools: ctx.agent_cfg.resolved.parallel_tools,
                            max_tool_result_chars: ctx.max_tool_result_chars,
                            context_token_budget: ctx.context_token_budget,
                            knobs: &loop_knobs,
                        },
                    ),
                    // Reborrow, not move: `history` is the bracket's `&mut` and the
                    // model-switch loop may take another lap with the same vector.
                    history: &mut *history,
                    channel_name: msg.channel.as_str(),
                    channel_reply_target: Some(msg.reply_target.as_str()),
                    cancellation_token: Some(cancellation_token.clone()),
                    on_delta: delta_tx.clone(),
                    shared_budget: None,
                    channel: approval_channel.as_deref(),
                    // Collector is meaningful only when the generator is active.
                    // Pass None when receipts are disabled so the call site
                    // reflects that coupling explicitly.
                    collected_receipts: ctx
                        .receipt_generator
                        .as_ref()
                        .map(|_| tool_receipts_collector.as_ref()),
                    event_tx: None,
                    steering: None,
                    new_messages_out: None,
                    image_cache: None,
                    // Channel-orchestrator dispatch; source/transport/trust stay
                    // placeholders, not yet stamped at the edge.
                    memory: Some(zeroclaw_runtime::agent::memory_inject::TurnMemory {
                        handle: ctx.memory.as_ref(),
                        query: msg.content.clone(),
                        sessions: memory_sessions.clone(),
                        suppress: false,
                        // The relevance floor stays the context's resolved copy;
                        // the rerank stage settings thread from the live config.
                        cfg: zeroclaw_runtime::agent::memory_inject::MemoryInjectConfig {
                            min_relevance_score: ctx.min_relevance_score,
                            ..zeroclaw_runtime::agent::memory_inject::MemoryInjectConfig::from_memory_config(
                                &ctx.prompt_config.memory,
                                zeroclaw_runtime::agent::memory_inject::DEFAULT_RECALL_LIMIT,
                            )
                        },
                    }),
                    ingress: zeroclaw_api::ingress::IngressContext::channel(),
                    agent_alias: Some(ctx.agent_alias.as_str()),
                    parent_agent_alias: None,
                    turn_id: &turn_id,
                    // Live channel-daemon SOP path: re-assemble a nested step's
                    // agent when it delegates to a different agent, so the step runs
                    // with that agent's own gated tools/policy/MCP scope rather than
                    // this turn's.
                    sop_reassembly: Some(zeroclaw_runtime::agent::loop_::SopStepReassembly {
                        config: ctx.prompt_config.as_ref(),
                    }),
                });
                // Scope this turn's routing handle so concurrent same-agent turns,
                // which share one SendViaTool, never read each other's routes.
                let tool_loop =
                    tools::TURN_ROUTING.scope(Some(std::sync::Arc::clone(&turn_routing)), tool_loop);
                let tool_loop = zeroclaw_api::NATIVE_THINKING_OVERRIDE
                    .scope(thinking.params.native_thinking, tool_loop);
                let tool_loop = zeroclaw_runtime::agent::tool_receipts::TOOL_LOOP_RECEIPT_CONTEXT
                    .scope(receipt_scope.clone(), tool_loop);
                let tool_loop = zeroclaw_runtime::agent::loop_::TOOL_LOOP_COST_TRACKING_CONTEXT
                    .scope(cost_tracking_context.clone(), tool_loop);
                let tool_loop = scope_session_key(Some(history_key.clone()), tool_loop);
                let tool_loop = scope_thread_id(thread_scope_id, tool_loop);
                let timed_tool_loop =
                    tokio::time::timeout(Duration::from_secs(timeout_budget_secs), tool_loop);

                let loop_result = tokio::select! {
                    () = cancellation_token.cancelled() => LlmExecutionResult::Cancelled,
                    result = timed_tool_loop => LlmExecutionResult::Completed(result),
                };

                if let LlmExecutionResult::Completed(Ok(Err(ref e))) = loop_result
                    && let Some((new_model_provider, new_model)) = is_model_switch_requested(e)
                {
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        &format!(
                            "Model switch requested, switching from {} {} to {} {}",
                            route.model_provider, route.model, new_model_provider, new_model
                        )
                    );

                    let resolved_model_provider = match resolve_provider_ref_for_runtime_switch(
                        runtime_defaults.config.as_ref(),
                        &new_model_provider,
                    ) {
                        Ok(provider_ref) => provider_ref,
                        Err(err) => {
                            ::zeroclaw_log::record!(
                                ERROR,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Fail
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(::serde_json::json!({"err": err.to_string()})),
                                "Failed to resolve model_provider after model switch"
                            );
                            break loop_result;
                        }
                    };

                    let resolved_api_key = ctx
                        .model_routes
                        .iter()
                        .find(|r| {
                            r.model_provider.eq_ignore_ascii_case(&new_model_provider)
                                && (r.model.eq_ignore_ascii_case(&new_model)
                                    || r.hint.eq_ignore_ascii_case(&new_model))
                        })
                        .and_then(|r| r.api_key.clone());

                    match get_or_create_provider(
                        ctx.as_ref(),
                        &resolved_model_provider,
                        resolved_api_key.as_deref(),
                        &runtime_defaults,
                    )
                    .await
                    {
                        Ok(new_prov) => {
                            // Commit state only after the provider was built
                            // successfully, so a failure leaves the turn on the
                            // original provider/model pair instead of a
                            // half-switched state.
                            active_model_provider = new_prov;
                            route.model_provider = resolved_model_provider;
                            route.model = new_model;
                            route.api_key = resolved_api_key;
                            // Persist the route override so subsequent messages
                            // from this sender continue using the switched model.
                            set_route_selection(
                                ctx.as_ref(),
                                &history_key,
                                ChannelRouteSelection {
                                    model_provider: route.model_provider.clone(),
                                    model: route.model.clone(),
                                    api_key: route.api_key.clone(),
                                },
                                &runtime_defaults,
                            );

                            continue;
                        }
                        Err(err) => {
                            ::zeroclaw_log::record!(
                                ERROR,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Fail
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(::serde_json::json!({"err": err.to_string()})),
                                "Failed to create model_provider after model switch"
                            );
                            // Fall through with the original error
                        }
                    }
                }

                break loop_result;
            };
            // Read inside the provider-fallback scope, where it is visible, and
            // handed out through the binding above rather than as part of the
            // body's outcome: the bracket settles against the turn's outcome, and a
            // fallback record is not part of that question.
            fallback_info = take_last_provider_fallback();
            llm_result
        })
        .await,
    )
    .await;

    // Attribute the closing event to the final route and attach aggregate
    // usage. Explicit completion records the normal duration; the guard's
    // `Drop` path supplies the same matched end on panic or early unwind.
    let turn_tokens_used = cost_tracking_context.as_ref().and_then(|ctx| {
        let usage = ctx.snapshot_turn_usage();
        (usage.input_tokens > 0 || usage.output_tokens > 0).then_some(
            zeroclaw_api::observability_traits::TurnTokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
            },
        )
    });
    turn_guard.set_model_route(route.model_provider.clone(), route.model.clone());
    turn_guard.set_usage(turn_tokens_used, None);
    turn_guard.finish();

    // Drop all senders so updater tasks can exit (rx.recv() returns None).
    ::zeroclaw_log::record!(
        DEBUG,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
        "Post-loop: dropping delta_tx and awaiting draft updater"
    );
    drop(delta_tx);
    if let Some(handle) = draft_updater {
        let _ = handle.await;
    }
    ::zeroclaw_log::record!(
        DEBUG,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
        "Post-loop: draft updater completed"
    );

    // Thread the final reply only if tools were used (multi-message response)
    if notify_observer_flag.tools_used.load(Ordering::Relaxed) && msg.channel != "cli" {
        msg.thread_ts = followup_thread_id(&msg);
    }
    // Drop the notify sender so the forwarder task finishes
    drop(notify_observer);
    drop(notify_observer_flag);
    if let Some(handle) = notify_task {
        let _ = handle.await;
    }

    #[allow(clippy::cast_possible_truncation)]
    let llm_call_ms = llm_call_start.elapsed().as_millis() as u64;
    #[allow(clippy::cast_possible_truncation)]
    let total_ms = started_at.elapsed().as_millis() as u64;
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(::serde_json::json!({"llm_call_ms": llm_call_ms, "total_ms": total_ms})),
        "LLM call completed"
    );

    if let Some(typing) = typing_controller.as_ref() {
        typing.pause().await;
    }

    let reaction_done_emoji = match &llm_result {
        LlmExecutionResult::Completed(Ok(Ok(_))) => "\u{2705}", // ✅
        _ => "\u{26A0}\u{FE0F}",                                // ⚠️
    };

    match llm_result {
        LlmExecutionResult::Cancelled => {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"sender": msg.sender})),
                "Cancelled in-flight channel request due to newer message"
            );
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Cancel)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_duration(
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    )
                    .with_attrs(::serde_json::json!({
                        "model_provider": route.model_provider,
                        "model": route.model,
                        "sender": msg.sender,
                        "reason": "cancelled due to newer inbound message",
                    })),
                "channel_message_cancelled"
            );
            if let (Some(channel), Some(draft_id)) =
                (target_channel.as_ref(), draft_message_id.as_deref())
                && let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await
            {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"error": format!("{}", err)})),
                    &format!("Failed to cancel draft on {}", channel.name())
                );
            }
        }
        LlmExecutionResult::Completed(Ok(Ok(response))) => {
            // ── Hook: on_message_sending (modifying) ─────────
            let mut outbound_response = response;
            if let Some(hooks) = &ctx.hooks {
                match hooks
                    .run_on_message_sending(
                        msg.channel.clone(),
                        msg.reply_target.clone(),
                        outbound_response.clone(),
                    )
                    .await
                {
                    zeroclaw_runtime::hooks::HookResult::Cancel(reason) => {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"reason": reason.to_string()})),
                            "outgoing message suppressed by hook"
                        );
                        if let (Some(channel), Some(draft_id)) =
                            (target_channel.as_ref(), draft_message_id.as_deref())
                        {
                            let _ = channel.cancel_draft(&msg.reply_target, draft_id).await;
                        }
                        return;
                    }
                    zeroclaw_runtime::hooks::HookResult::Continue((
                        hook_channel,
                        hook_recipient,
                        mut modified_content,
                    )) => {
                        if hook_channel != msg.channel || hook_recipient != msg.reply_target {
                            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"from_channel": channel_composite, "from_recipient": msg.reply_target, "to_channel": hook_channel, "to_recipient": hook_recipient})), "on_message_sending attempted to rewrite channel routing; only content mutation is applied");
                        }

                        let modified_len = modified_content.chars().count();
                        if modified_len > CHANNEL_HOOK_MAX_OUTBOUND_CHARS {
                            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"limit": CHANNEL_HOOK_MAX_OUTBOUND_CHARS, "attempted": modified_len})), "hook-modified outbound content exceeded limit; truncating");
                            modified_content = truncate_with_ellipsis(
                                &modified_content,
                                CHANNEL_HOOK_MAX_OUTBOUND_CHARS,
                            );
                        }

                        if modified_content != outbound_response {
                            ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"sender": msg.sender, "before_len": outbound_response.chars().count(), "after_len": modified_content.chars().count()})), "outgoing message content modified by hook");
                        }

                        outbound_response = modified_content;
                    }
                }
            }

            let sanitized_response = sanitize_channel_response_for_format_with_leak_detection(
                &outbound_response,
                ctx.tools_registry.as_ref(),
                &ctx.prompt_config.security.leak_detection,
                outbound_content_format_for_channel(&msg.channel),
            );
            let mut delivered_response =
                if sanitized_response.is_empty() && !outbound_response.trim().is_empty() {
                    channel_runtime_cli_string("channel-runtime-malformed-tool-output")
                } else {
                    sanitized_response
                };
            delivered_response = ensure_nonempty_channel_reply(
                delivered_response,
                &outbound_response,
                &msg.channel,
                &msg.reply_target,
            );

            // Append a footer when the response was served by a different model_provider family.
            // Intra-family fallbacks (e.g. minimax → minimax-cn) are suppressed.
            if let Some(fb) = fallback_info.as_ref() {
                let req_base = fb.requested_provider.split(':').next().unwrap_or("");
                let act_base = fb.actual_provider.split(':').next().unwrap_or("");
                let same_family = req_base == act_base
                    || req_base.starts_with(act_base)
                    || act_base.starts_with(req_base);
                if !same_family {
                    delivered_response.push_str("\n\n---\n");
                    delivered_response.push_str(&channel_runtime_cli_string_with_args(
                        "channel-runtime-fallback-footer",
                        &[
                            ("requested", fb.requested_provider.as_str()),
                            ("actual", fb.actual_provider.as_str()),
                            ("model", fb.actual_model.as_str()),
                        ],
                    ));
                }
            }

            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Outbound)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_duration(
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    )
                    .with_attrs(::serde_json::json!({
                        "model_provider": route.model_provider,
                        "model": route.model,
                        "sender": msg.sender,
                        "response": scrub_credentials(&delivered_response),
                    })),
                "channel_message_outbound"
            );

            // Persist intermediate tool-call/result messages from this turn
            // so the model retains concrete "I used tools" examples in
            // context, preventing drift toward tool-less responses.
            let keep_tool_turns = ctx.agent_cfg.resolved.keep_tool_context_turns;
            if keep_tool_turns > 0 {
                // Find tool messages for the current turn: everything after
                // the last user message up to (but not including) the final
                // assistant response that matches our delivered text.
                let tool_messages: Vec<ChatMessage> = extract_current_turn_tool_messages(&history);
                for tool_msg in tool_messages {
                    append_sender_turn(ctx.as_ref(), &history_key, tool_msg);
                }
            }

            let history_response = delivered_response.clone();
            append_sender_turn(
                ctx.as_ref(),
                &history_key,
                ChatMessage::assistant(&history_response),
            );

            ctx.persist_companion_capture(&msg, &history_key, &turn_id);

            // Fire-and-forget LLM-driven curated-memory consolidation.
            // Companion capture already ran at settlement, before send.
            // Passes the agent's resolved temperature through unchanged —
            // `None` means the provider sends no `temperature` field
            // (necessary for models that reject it, e.g. claude-opus-4-7).
            if ctx.auto_save_memory && msg.content.chars().count() >= AUTOSAVE_MIN_MESSAGE_CHARS {
                let memory_strategy = Arc::clone(&ctx.memory_strategy);
                let model_provider = Arc::clone(&ctx.model_provider);
                let model = ctx.model.to_string();
                let temperature = ctx.temperature;
                let user_msg = msg.content.clone();
                let assistant_resp = delivered_response.clone();
                zeroclaw_spawn::spawn!(async move {
                    if let Err(e) = memory_strategy
                        .consolidate_turn(
                            &user_msg,
                            &assistant_resp,
                            model_provider.as_ref(),
                            &model,
                            temperature,
                        )
                        .await
                    {
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "Memory consolidation skipped"
                        );
                    }
                });
            }

            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Outbound)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_duration(
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    )
                    .with_attrs(::serde_json::json!({
                        "sender": msg.sender,
                        "message_id": msg.id,
                        "reply_target": msg.reply_target,
                        "thread_ts": msg.thread_ts,
                        "content": delivered_response,
                    })),
                "reply delivered"
            );
            let receipts_block = if ctx.show_receipts_in_response {
                let receipts = tool_receipts_collector
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                zeroclaw_runtime::agent::tool_receipts::render_receipts_block(&receipts)
            } else {
                None
            };

            // Read the last routing instruction set by `send_via` this turn from
            // the per-turn handle scoped into TURN_ROUTING around the loop above.
            let turn_route = turn_routing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .cloned();

            // Resolve the delivery channel and modality from the routing entry.
            // `None` entry → default delivery (originating channel, no modality override).
            let (
                delivery_channel,
                delivery_recipient,
                suppress_voice_override,
                force_voice_override,
            ) = if let Some(ref route) = turn_route {
                let ch: Option<Arc<dyn Channel>> = match route.channel.as_deref() {
                    None | Some("") => target_channel.clone(),
                    Some(key) => ctx.channels_by_name.get(key).map(Arc::clone),
                };
                let recipient = route
                    .recipient
                    .clone()
                    .unwrap_or_else(|| msg.reply_target.clone());
                let suppress = match route.modality {
                    zeroclaw_config::multi_agent::OutputModality::Text => Some(true),
                    zeroclaw_config::multi_agent::OutputModality::Voice => Some(false),
                    zeroclaw_config::multi_agent::OutputModality::Mirror => None,
                };
                let force_voice = matches!(
                    route.modality,
                    zeroclaw_config::multi_agent::OutputModality::Voice
                );
                (ch, recipient, suppress, force_voice)
            } else {
                (
                    target_channel.clone(),
                    msg.reply_target.clone(),
                    None,
                    false,
                )
            };

            if let Some(channel) = delivery_channel.as_ref() {
                let is_redirect = turn_route
                    .as_ref()
                    .and_then(|r| r.channel.as_deref())
                    .is_some();
                // Whether the agent's reply reached a channel — gates the
                // `fire_message_sent` observer hook below.
                let reply_delivered = if is_redirect {
                    // Routing redirects to a different channel: cancel any in-progress
                    // draft on the originating channel before delivering elsewhere.
                    if let (Some(orig_ch), Some(draft_id)) =
                        (target_channel.as_ref(), draft_message_id.as_deref())
                    {
                        let _ = orig_ch.cancel_draft(&msg.reply_target, draft_id).await;
                    }
                    let suppress = suppress_voice_override.unwrap_or(false);
                    let mut send_msg = SendMessage::new(&delivered_response, &delivery_recipient)
                        .in_thread(msg.thread_ts.clone());
                    if suppress {
                        send_msg = send_msg.suppress_voice();
                    } else if force_voice_override {
                        send_msg = send_msg.force_voice();
                    }
                    channel.send(&send_msg).await.is_ok()
                } else if let Some(ref draft_id) = draft_message_id {
                    // Same channel with draft. For force-voice routing: cancel the
                    // draft placeholder and deliver via send() so force_voice
                    // reaches the channel's voice path (finalize_draft has no
                    // force_voice concept).
                    if force_voice_override {
                        let _ = channel.cancel_draft(&delivery_recipient, draft_id).await;
                        channel
                            .send(
                                &SendMessage::new(&delivered_response, &delivery_recipient)
                                    .force_voice()
                                    .in_thread(msg.thread_ts.clone()),
                            )
                            .await
                            .is_ok()
                    } else {
                        let suppress = suppress_voice_override.unwrap_or(false);
                        match channel
                            .finalize_draft(
                                &delivery_recipient,
                                draft_id,
                                &delivered_response,
                                suppress,
                            )
                            .await
                        {
                            Ok(()) => true,
                            Err(e) => {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                                    "Failed to finalize draft; sending as new message"
                                );
                                let mut fallback = SendMessage::reply_to(&msg, &delivered_response);
                                if suppress {
                                    fallback = fallback.suppress_voice();
                                }
                                channel.send(&fallback).await.is_ok()
                            }
                        }
                    }
                } else {
                    // No draft — plain send.
                    let suppress = suppress_voice_override.unwrap_or(false);
                    let mut send_msg = SendMessage::reply_to(&msg, &delivered_response)
                        .with_cancellation(cancellation_token.clone());
                    if suppress {
                        send_msg = send_msg.suppress_voice();
                    } else if force_voice_override {
                        send_msg = send_msg.force_voice();
                    }
                    match channel.send(&send_msg).await {
                        Ok(()) => true,
                        Err(e) => {
                            ::zeroclaw_log::record!(
                                ERROR,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Fail
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                                "failed to reply"
                            );
                            false
                        }
                    }
                };
                if reply_delivered && let Some(hooks) = ctx.hooks.as_ref() {
                    hooks
                        .fire_message_sent(&msg.channel, &msg.reply_target, &delivered_response)
                        .await;
                }
                // Send tool receipts as a separate message in the same thread.
                // The block is the operator-facing audit surface for the feature,
                // so a dropped send must leave a log signal rather than silently
                // disappear.
                if let Some(ref block) = receipts_block
                    && let Err(e) = channel
                        .send(
                            &SendMessage::new(block, &delivery_recipient)
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "failed to send tool receipts block"
                    );
                }
            }
        }
        LlmExecutionResult::Completed(Ok(Err(e))) => {
            if zeroclaw_runtime::agent::loop_::is_tool_loop_cancelled(&e)
                || cancellation_token.is_cancelled()
            {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({"sender": msg.sender})),
                    "Cancelled in-flight channel request due to newer message"
                );
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Cancel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_duration(
                            u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                        )
                        .with_attrs(::serde_json::json!({
                            "model_provider": route.model_provider,
                            "model": route.model,
                            "sender": msg.sender,
                            "reason": "cancelled during tool-call loop",
                        })),
                    "channel_message_cancelled"
                );
                if let (Some(channel), Some(draft_id)) =
                    (target_channel.as_ref(), draft_message_id.as_deref())
                    && let Err(err) = channel.cancel_draft(&msg.reply_target, draft_id).await
                {
                    ::zeroclaw_log::record!(
                        DEBUG,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_attrs(::serde_json::json!({"error": format!("{}", err)})),
                        &format!("Failed to cancel draft on {}", channel.name())
                    );
                }
            } else if is_context_window_overflow_error(&e) {
                let compacted = compact_sender_history(ctx.as_ref(), &history_key);
                let error_text = if compacted {
                    "⚠️ Context window exceeded for this conversation. I compacted recent history and kept the latest context. Please resend your last message."
                } else {
                    "⚠️ Context window exceeded for this conversation. Please resend your last message."
                };
                eprintln!(
                    "  ⚠️ Context window exceeded after {}ms; sender history compacted={}",
                    started_at.elapsed().as_millis(),
                    compacted
                );
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_duration(
                            u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                        )
                        .with_attrs(::serde_json::json!({
                            "model_provider": route.model_provider,
                            "model": route.model,
                            "sender": msg.sender,
                            "reason": "context window exceeded",
                            "history_compacted": compacted,
                        })),
                    "channel_message_error"
                );
                if let Some(channel) = target_channel.as_ref() {
                    if let Some(draft_id) = draft_message_id.as_deref() {
                        let _ = channel.cancel_draft(&msg.reply_target, draft_id).await;
                    }
                    let _ = channel
                        .send(
                            &SendMessage::new(error_text, &msg.reply_target)
                                .suppress_voice()
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                }
            } else {
                let safe_error = zeroclaw_providers::sanitize_api_error(&e.to_string());
                eprintln!(
                    "  ❌ LLM error after {}ms: {safe_error}",
                    started_at.elapsed().as_millis(),
                );

                // Evict cached model_provider on auth errors so the next request
                // re-creates it with fresh OAuth credentials.
                if zeroclaw_providers::reliable::is_auth_error(&e) {
                    let cache_key = provider_cache_key(
                        &route.model_provider,
                        route.api_key.as_deref(),
                        runtime_defaults.generation,
                    );
                    let mut cache = ctx.provider_cache.lock().unwrap_or_else(|p| p.into_inner());
                    if cache.remove(&cache_key).is_some() {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_attrs(
                                ::serde_json::json!({"model_provider": route.model_provider})
                            ),
                            "Evicted cached model_provider after auth error; next request will re-create with fresh credentials"
                        );
                    }
                }
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_duration(
                            u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                        )
                        .with_attrs(::serde_json::json!({
                            "model_provider": route.model_provider,
                            "model": route.model,
                            "sender": msg.sender,
                            "error": safe_error,
                        })),
                    "channel_message_error"
                );
                let should_rollback_user_turn = should_rollback_failed_user_turn(&e);
                let rolled_back = should_rollback_user_turn
                    && rollback_orphan_user_turn(ctx.as_ref(), &history_key, &timestamped_content);

                if !rolled_back {
                    // Close the orphan user turn so subsequent messages don't
                    // inherit this failed request as unfinished context.
                    append_sender_turn(
                        ctx.as_ref(),
                        &history_key,
                        ChatMessage::assistant("[Task failed — not continuing this request]"),
                    );
                }
                if let Some(channel) = target_channel.as_ref() {
                    let user_msg = zeroclaw_providers::reliable::transient_error_hint(&e)
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("⚠️ Error: {safe_error}"));
                    // Cancel any in-progress draft (don't finalize it with the
                    // error text, which would trigger TTS on the error message)
                    // then deliver the error as a plain suppressed send.
                    if let Some(ref draft_id) = draft_message_id {
                        let _ = channel.cancel_draft(&msg.reply_target, draft_id).await;
                    }
                    let _ = channel
                        .send(
                            &SendMessage::new(user_msg, &msg.reply_target)
                                .suppress_voice()
                                .in_thread(msg.thread_ts.clone()),
                        )
                        .await;
                }
            }
        }
        LlmExecutionResult::Completed(Err(_)) => {
            let timeout_msg = format!(
                "LLM response timed out after {}s (base={}s, max_tool_iterations={})",
                timeout_budget_secs, ctx.message_timeout_secs, ctx.max_tool_iterations
            );
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Timeout)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_duration(
                        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    )
                    .with_attrs(::serde_json::json!({
                        "model_provider": route.model_provider,
                        "model": route.model,
                        "sender": msg.sender,
                        "reason": timeout_msg,
                    })),
                "channel_message_timeout"
            );
            eprintln!(
                "  ❌ {} (elapsed: {}ms)",
                timeout_msg,
                started_at.elapsed().as_millis()
            );
            // Close the orphan user turn so subsequent messages don't
            // inherit this timed-out request as unfinished context.
            append_sender_turn(
                ctx.as_ref(),
                &history_key,
                ChatMessage::assistant("[Task timed out — not continuing this request]"),
            );
            if let Some(channel) = target_channel.as_ref() {
                // Localized error text (master) delivered with suppress_voice
                // (RFCerror-path fix): cancel the draft, then send as
                // text so a timeout notice is never read aloud on a voice peer.
                let error_text = zeroclaw_runtime::i18n::get_required_cli_string(
                    "channel-runtime-request-timeout",
                );
                if let Some(draft_id) = draft_message_id.as_deref() {
                    let _ = channel.cancel_draft(&msg.reply_target, draft_id).await;
                }
                let _ = channel
                    .send(
                        &SendMessage::new(error_text, &msg.reply_target)
                            .suppress_voice()
                            .in_thread(msg.thread_ts.clone()),
                    )
                    .await;
            }
        }
    }

    // Swap 👀 → ✅ (or ⚠️ on error) to signal processing is complete. Await the
    // spawned ack add first so the remove can never race ahead of it.
    if resolve_channel_ack_reactions(&ctx, &msg)
        && let Some(channel) = target_channel.as_ref()
    {
        if let Some(task) = early_ack_task {
            let _ = task.await;
        }
        let _ = channel
            .remove_reaction(&msg.reply_target, &msg.id, "\u{1F440}")
            .await;
        let _ = channel
            .add_reaction(&msg.reply_target, &msg.id, reaction_done_emoji)
            .await;
    }
}
