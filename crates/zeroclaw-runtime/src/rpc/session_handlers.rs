//! Session JSON-RPC method handlers extracted from dispatch.rs.

use super::dispatch::{
    RpcDispatcher, RpcResult, context_usage_max_tokens, notification, notification_for_turn_event,
    parse_params, persist_acp_turn, persist_plan_if_any, plan_replay_notification, rpc_err,
    session_should_initialize_mcp, to_result, validate_session_configure_overrides,
};
use super::turn::{TurnAttribution, TurnOutcome, execute_turn};
use super::types::*;
use crate::agent::agent::TurnEvent;
use serde_json::Value;
use std::sync::Arc;
use zeroclaw_api::jsonrpc::JsonRpcNotification;
use zeroclaw_api::jsonrpc::error_codes::*;
use zeroclaw_api::model_provider::ChatMessage;

impl RpcDispatcher {
    // ── Session handlers ─────────────────────────────────────────

    #[cfg(test)]
    pub async fn handle_session_new_for_test(&self, params: &Value) -> RpcResult {
        self.handle_session_new(params).await
    }

    #[cfg(test)]
    pub async fn handle_session_messages_for_test(&self, params: &Value) -> RpcResult {
        self.handle_session_messages(params).await
    }

    /// Drive a full JSON-RPC request line through the dispatcher from an
    /// external integration test, including notification emission on the
    /// outbound channel. Mirrors the transport `process_line` path.
    pub async fn process_line_for_test(&mut self, line: &str) {
        self.process_line(line).await;
    }

    pub(crate) async fn handle_session_new(&self, params: &Value) -> RpcResult {
        let req: SessionNewParams = parse_params(params)?;
        let resuming = req.session_id.is_some();
        let session_id = req
            .session_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let config = self.ctx.config.read().clone();
        let chat_mode = req
            .chat_mode
            .clone()
            .unwrap_or(crate::rpc::types::ChatMode::Chat);

        // Resuming an ACP session with no caller cwd: recover the original
        // working directory from the persisted store so the rehydrated session
        // keeps its own cwd instead of falling back to the agent workspace dir.
        // The loaded data is reused below so history is not fetched twice.
        let mut preloaded_acp: Option<zeroclaw_infra::acp_session_store::AcpSessionData> = None;
        if resuming
            && req.cwd.is_none()
            && matches!(chat_mode, crate::rpc::types::ChatMode::Acp)
            && let Some(ref store) = self.ctx.acp_session_store
        {
            let store_cloned = store.clone();
            let sid = session_id.clone();
            match tokio::task::spawn_blocking(move || store_cloned.load_session_for_restore(&sid))
                .await
            {
                Ok(Ok(zeroclaw_infra::acp_session_store::AcpSessionRestore::Restorable(data))) => {
                    if data.agent_alias != req.agent_alias {
                        return Err(rpc_err(
                            INVALID_PARAMS,
                            "ACP session belongs to a different agent",
                        ));
                    }
                    preloaded_acp = Some(data);
                }
                Ok(Ok(zeroclaw_infra::acp_session_store::AcpSessionRestore::Missing)) => {}
                Ok(Ok(zeroclaw_infra::acp_session_store::AcpSessionRestore::Killed)) => {
                    return Err(rpc_err(SESSION_NOT_FOUND, "Session not found"));
                }
                Ok(Err(e)) => {
                    return Err(rpc_err(
                        INTERNAL_ERROR,
                        format!("Failed to load ACP session: {e}"),
                    ));
                }
                Err(join) => {
                    return Err(rpc_err(
                        INTERNAL_ERROR,
                        format!("Failed to load ACP session: {join}"),
                    ));
                }
            }
        }

        // The session cwd: caller-supplied wins, then a resumed ACP session's
        // persisted cwd, then the agent's workspace dir.
        let cwd = req
            .cwd
            .clone()
            .or_else(|| preloaded_acp.as_ref().map(|d| d.workspace_dir.clone()))
            .unwrap_or_else(|| {
                config
                    .agent_workspace_dir(&req.agent_alias)
                    .to_string_lossy()
                    .to_string()
            });

        let cwd_path = Some(std::path::Path::new(&cwd));
        let tui_env = req
            .tui_id
            .as_deref()
            .and_then(|id| self.ctx.tui_registry.get_env(id));
        let chat_mode = req
            .chat_mode
            .clone()
            .unwrap_or(crate::rpc::types::ChatMode::Chat);
        let exclude_memory = matches!(chat_mode, crate::rpc::types::ChatMode::Acp)
            || req.exclude_memory == Some(true);
        // Chat sessions initialize MCP so the TUI sees the same MCP tools the
        // gateway exposes for this agent; ACP (Code) sessions skip it to keep
        // `session/new` prompt
        let initialize_mcp = session_should_initialize_mcp(&chat_mode);
        let mut agent = crate::agent::agent::Agent::from_live_config_with_tui_env(
            Arc::clone(&self.ctx.config),
            &req.agent_alias,
            cwd_path,
            initialize_mcp,
            exclude_memory,
            tui_env,
            self.ctx.sop_engine.clone(),
            self.ctx.sop_audit.clone(),
        )
        .await
        .map_err(|e| rpc_err(INTERNAL_ERROR, format!("Failed to create agent: {e}")))?;

        let approval_ch = Arc::new(crate::rpc::approval_channel::RpcApprovalChannel::new(
            "rpc",
            session_id.clone(),
            Arc::clone(&self.rpc),
            Arc::clone(&self.ctx.approval_pending),
            self.client_elicitation_caps,
        ));
        // Align agent.channel_name with the registered back-channel key so
        // ask_user/poll/escalate default to this conversation (not an arbitrary
        // external channel from the seeded channel map).
        agent.set_channel_name("rpc".to_string());
        agent.channel_handles().register_channel("rpc", approval_ch);

        self.ctx
            .sessions
            .insert(
                session_id.clone(),
                super::session::RpcSession::new(agent, &req.agent_alias, &cwd, chat_mode.clone())
                    .with_owner(self.tui_id.clone()),
            )
            .await
            .map_err(|_| rpc_err(SESSION_LIMIT_REACHED, "Session limit reached"))?;

        if let Some(ref tui_id) = self.tui_id {
            let evicted = self
                .ctx
                .sessions
                .evict_same_mode_sibling(tui_id, &chat_mode, &session_id)
                .await;
            if !evicted.is_empty() {
                if let Some(ref hooks) = self.ctx.hooks {
                    for (sid, _) in &evicted {
                        hooks.fire_session_end(sid, "rpc").await;
                    }
                }
                let span = ::zeroclaw_log::info_span!(
                    target: "zeroclaw_log_internal_scope",
                    "zeroclaw_scope",
                    session_key = %session_id,
                    agent_alias = %req.agent_alias,
                    channel = "rpc",
                );
                let _guard = span.enter();
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Success)
                        .with_attrs(::serde_json::json!({
                            "tui_id": tui_id,
                            "evicted": evicted.iter().map(|(id, _)| id).collect::<Vec<_>>(),
                        })),
                    "Evicted abandoned same-mode session(s) on session/new"
                );
                // Every evicted session was idle (no in-flight turn), so its
                // removal above dropped the last Agent strong ref and freed the
                // history. Trimming now actually returns those pages.
                crate::util::release_freed_heap();
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Success)
                        .with_attrs(::serde_json::json!({
                            "evicted_count": evicted.len(),
                        })),
                    "Trimmed glibc arenas after same-mode session eviction"
                );
            }
        }

        enum AcpSessionNewLoad {
            Restored(zeroclaw_infra::acp_session_store::AcpSessionData),
            Created,
            Killed,
        }

        let mut message_count = 0;
        match chat_mode {
            crate::rpc::types::ChatMode::Acp => {
                // Reuse the data already loaded for cwd recovery on resume so the
                // store isn't hit twice; otherwise fall through to the restore-
                // aware load-or-create path below.
                let loaded = if let Some(data) = preloaded_acp.take() {
                    Ok(Ok(AcpSessionNewLoad::Restored(data)))
                } else {
                    let Some(ref store) = self.ctx.acp_session_store else {
                        if let Some(ref hooks) = self.ctx.hooks {
                            hooks.fire_session_end(&session_id, "rpc").await;
                        }
                        self.ctx.sessions.remove(&session_id).await;
                        return Err(rpc_err(
                            INTERNAL_ERROR,
                            "ACP session store is not available",
                        ));
                    };

                    let store_cloned = store.clone();
                    let sid = session_id.clone();
                    let alias = req.agent_alias.clone();
                    let cwd_owned = cwd.clone();
                    tokio::task::spawn_blocking(move || -> anyhow::Result<AcpSessionNewLoad> {
                        match store_cloned.load_session_for_restore(&sid)? {
                            zeroclaw_infra::acp_session_store::AcpSessionRestore::Restorable(
                                data,
                            ) => Ok(AcpSessionNewLoad::Restored(data)),
                            zeroclaw_infra::acp_session_store::AcpSessionRestore::Missing => {
                                store_cloned.create_session(&sid, &alias, &cwd_owned)?;
                                Ok(AcpSessionNewLoad::Created)
                            }
                            zeroclaw_infra::acp_session_store::AcpSessionRestore::Killed => {
                                Ok(AcpSessionNewLoad::Killed)
                            }
                        }
                    })
                    .await
                };
                match loaded {
                    Ok(Ok(AcpSessionNewLoad::Restored(data))) => {
                        if data.agent_alias != req.agent_alias {
                            if let Some(ref hooks) = self.ctx.hooks {
                                hooks.fire_session_end(&session_id, "rpc").await;
                            }
                            self.ctx.sessions.remove(&session_id).await;
                            return Err(rpc_err(
                                INVALID_PARAMS,
                                "ACP session belongs to a different agent",
                            ));
                        }
                        message_count = data.messages.len();
                        let seed_event = self
                            .ctx
                            .sessions
                            .seed_conversation_history_with_event(&session_id, data.messages)
                            .await;
                        self.forward_seed_event(&session_id, seed_event).await;
                        // Restore the durable TodoWrite plan into the fresh
                        // in-memory session and re-emit it so the resuming /
                        // reconnecting client's tracker repopulates without a
                        // model round-trip. Robust against tmux detach, socket
                        // drop, suspend/resume, and daemon restart.
                        if let Some(ref store) = self.ctx.acp_session_store {
                            let store = store.clone();
                            let sid = session_id.clone();
                            let plan = tokio::task::spawn_blocking(move || {
                                store.get_plan(&sid).unwrap_or_default()
                            })
                            .await
                            .unwrap_or_default();
                            if !plan.is_empty() {
                                self.ctx.sessions.set_plan(&session_id, plan.clone()).await;
                                if let Some(n) = plan_replay_notification(&session_id, &plan) {
                                    let _ = self.rpc.send_raw(n).await;
                                }
                            }
                        }
                    }
                    Ok(Ok(AcpSessionNewLoad::Created)) => {}
                    Ok(Ok(AcpSessionNewLoad::Killed)) => {
                        if let Some(ref hooks) = self.ctx.hooks {
                            hooks.fire_session_end(&session_id, "rpc").await;
                        }
                        self.ctx.sessions.remove(&session_id).await;
                        return Err(rpc_err(SESSION_NOT_FOUND, "Session not found"));
                    }
                    Ok(Err(e)) => {
                        if let Some(ref hooks) = self.ctx.hooks {
                            hooks.fire_session_end(&session_id, "rpc").await;
                        }
                        self.ctx.sessions.remove(&session_id).await;
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(::serde_json::json!({"session_id": session_id, "error": e.to_string()})),
                            "Failed to load or create ACP session"
                        );
                        return Err(rpc_err(
                            INTERNAL_ERROR,
                            format!("Failed to load or create ACP session: {e}"),
                        ));
                    }
                    Err(join) => {
                        if let Some(ref hooks) = self.ctx.hooks {
                            hooks.fire_session_end(&session_id, "rpc").await;
                        }
                        self.ctx.sessions.remove(&session_id).await;
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                                .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                                .with_attrs(::serde_json::json!({"session_id": session_id, "error": join.to_string()})),
                            "ACP session load task failed"
                        );
                        return Err(rpc_err(
                            INTERNAL_ERROR,
                            format!("ACP session load task failed: {join}"),
                        ));
                    }
                }
            }
            crate::rpc::types::ChatMode::Chat => {
                if let Some(ref backend) = self.ctx.session_backend {
                    let session_key = format!("rpc_{session_id}");
                    let _ = backend.set_session_agent_alias(&session_key, &req.agent_alias);
                    let stored = backend.load(&session_key);
                    if !stored.is_empty() {
                        let seed_event = self
                            .ctx
                            .sessions
                            .seed_history_with_event(&session_id, &stored)
                            .await;
                        self.forward_seed_event(&session_id, seed_event).await;
                        message_count = stored.len();
                    }
                }
            }
        }

        if let Some(ref hooks) = self.ctx.hooks {
            hooks.fire_session_start(&session_id, "rpc").await;
        }

        to_result(SessionNewResult {
            session_id,
            agent_alias: req.agent_alias,
            message_count,
            workspace_dir: cwd,
        })
    }

    pub(crate) async fn handle_session_close(&self, params: &Value) -> RpcResult {
        let req: SessionIdParams = parse_params(params)?;
        if let Some(agent) = self.ctx.sessions.get_agent(&req.session_id).await {
            agent
                .lock()
                .await
                .channel_handles()
                .unregister_channel("rpc");
            let strong = std::sync::Arc::strong_count(&agent);
            let agent_alias = self
                .ctx
                .sessions
                .get_agent_alias(&req.session_id)
                .await
                .unwrap_or_default();
            let span = ::zeroclaw_log::info_span!(
                target: "zeroclaw_log_internal_scope",
                "zeroclaw_scope",
                session_key = %req.session_id,
                agent_alias = %agent_alias,
                channel = "rpc",
            );
            let _guard = span.enter();
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_attrs(::serde_json::json!({
                        "agent_arc_strong_count_before_remove": strong,
                    })),
                "session close: dropping local Agent handle before remove"
            );
            // Drop our clone explicitly so the session map holds the last
            // strong ref; `remove` then frees the Agent at removal time
            // rather than at end-of-scope, letting the allocator reclaim
            // promptly.
            drop(agent);
        }
        if !self.ctx.sessions.remove(&req.session_id).await {
            return Err(rpc_err(SESSION_NOT_FOUND, "Session not found"));
        }
        if let Some(ref hooks) = self.ctx.hooks {
            hooks.fire_session_end(&req.session_id, "rpc").await;
        }
        crate::util::release_freed_heap();
        {
            let span = ::zeroclaw_log::info_span!(
                target: "zeroclaw_log_internal_scope",
                "zeroclaw_scope",
                session_key = %req.session_id,
                channel = "rpc",
            );
            let _guard = span.enter();
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success),
                "Trimmed glibc arenas after session close"
            );
        }
        to_result(SessionCloseResult {
            session_id: req.session_id,
            closed: true,
        })
    }

    pub(crate) async fn handle_session_kill(&self, params: &Value) -> RpcResult {
        let req: SessionKillParams = parse_params(params)?;
        let sid = &req.session_id;

        let chat_mode = self
            .ctx
            .sessions
            .chat_mode(sid)
            .await
            .ok_or_else(|| rpc_err(SESSION_NOT_FOUND, "Session not found"))?;

        let agent_alias = self
            .ctx
            .sessions
            .get_agent_alias(sid)
            .await
            .unwrap_or_default();
        let span = ::zeroclaw_log::info_span!(
            target: "zeroclaw_log_internal_scope",
            "zeroclaw_scope",
            session_key = %sid,
            agent_alias = %agent_alias,
            channel = "rpc",
        );
        let _guard = span.enter();

        if matches!(chat_mode, ChatMode::Acp) {
            let store = self
                .ctx
                .acp_session_store
                .clone()
                .ok_or_else(|| rpc_err(INTERNAL_ERROR, "ACP session store is not available"))?;
            let sid_owned = sid.to_string();
            let marked =
                tokio::task::spawn_blocking(move || store.mark_session_killed(&sid_owned)).await;
            match marked {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                        "session/kill: live ACP session had no durable row to tombstone"
                    );
                }
                Ok(Err(e)) => {
                    return Err(rpc_err(
                        INTERNAL_ERROR,
                        format!("Failed to mark ACP session killed: {e}"),
                    ));
                }
                Err(e) => {
                    return Err(rpc_err(
                        INTERNAL_ERROR,
                        format!("Failed to mark ACP session killed: {e}"),
                    ));
                }
            }
        }

        let killed = self.ctx.sessions.kill_session(sid).await;
        if killed {
            if let Some(ref hooks) = self.ctx.hooks {
                hooks.fire_session_end(sid, "rpc").await;
            }
            crate::util::release_freed_heap();
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success),
                "session/kill: session terminated by admin"
            );
        } else {
            ::zeroclaw_log::record!(
                DEBUG,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Agent)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "session/kill: session vanished between existence check and kill (concurrent close?)"
            );
        }

        to_result(SessionKillResult {
            session_id: req.session_id,
            killed,
        })
    }

    /// Rebuild a reaped ACP session from a restorable durable row so a fresh
    /// prompt recovers to a working session instead of hanging. Returns the
    /// live agent on success; returns `None` for missing, killed, or unreadable
    /// durable state.
    pub(crate) async fn rehydrate_reaped_session(
        &self,
        sid: &str,
    ) -> Option<Arc<tokio::sync::Mutex<crate::agent::agent::Agent>>> {
        let store = self.ctx.acp_session_store.clone()?;
        let sid_owned = sid.to_string();
        let loaded =
            tokio::task::spawn_blocking(move || store.load_session_for_restore(&sid_owned)).await;
        let data = match loaded {
            Ok(Ok(zeroclaw_infra::acp_session_store::AcpSessionRestore::Restorable(data))) => data,
            Ok(Ok(zeroclaw_infra::acp_session_store::AcpSessionRestore::Killed)) => {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Success),
                    "session/prompt: refusing to rehydrate admin-killed ACP session"
                );
                return None;
            }
            Ok(Err(e)) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "session_id": sid,
                            "error": e.to_string(),
                        })),
                    "session/prompt: failed to query ACP killed marker before rehydrate"
                );
                return None;
            }
            Ok(Ok(zeroclaw_infra::acp_session_store::AcpSessionRestore::Missing)) => return None,
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "session_id": sid,
                            "error": e.to_string(),
                        })),
                    "session/prompt: ACP killed-marker query task failed before rehydrate"
                );
                return None;
            }
        };

        let cwd_path = Some(std::path::Path::new(&data.workspace_dir));
        let tui_env = self
            .tui_id
            .as_deref()
            .and_then(|id| self.ctx.tui_registry.get_env(id));
        let exclude_memory = true;
        // Reaped sessions always rehydrate as ACP, which skips eager MCP init to
        // stay prompt — matching `session_should_initialize_mcp(ChatMode::Acp)`.
        let mut agent = crate::agent::agent::Agent::from_live_config_with_tui_env(
            Arc::clone(&self.ctx.config),
            &data.agent_alias,
            cwd_path,
            false,
            exclude_memory,
            tui_env,
            self.ctx.sop_engine.clone(),
            self.ctx.sop_audit.clone(),
        )
        .await
        .ok()?;

        let approval_ch = Arc::new(crate::rpc::approval_channel::RpcApprovalChannel::new(
            "rpc",
            sid.to_string(),
            Arc::clone(&self.rpc),
            Arc::clone(&self.ctx.approval_pending),
            self.client_elicitation_caps,
        ));
        // See session/new: channel_name must match the registered back-channel
        // key so interactive tools default to this conversation.
        agent.set_channel_name("rpc".to_string());
        agent.channel_handles().register_channel("rpc", approval_ch);

        let message_count = data.messages.len();
        self.ctx
            .sessions
            .insert(
                sid.to_string(),
                super::session::RpcSession::new(
                    agent,
                    &data.agent_alias,
                    &data.workspace_dir,
                    crate::rpc::types::ChatMode::Acp,
                )
                .with_owner(self.tui_id.clone()),
            )
            .await
            .ok()?;
        let seed_event = self
            .ctx
            .sessions
            .seed_conversation_history_with_event(sid, data.messages)
            .await;
        self.forward_seed_event(sid, seed_event).await;
        self.ctx.sessions.touch(sid).await;

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_category(::zeroclaw_log::EventCategory::Agent)
                .with_outcome(::zeroclaw_log::EventOutcome::Success)
                .with_attrs(::serde_json::json!({
                    "session_id": sid,
                    "agent_alias": data.agent_alias,
                    "messages_restored": message_count,
                })),
            "rehydrated reaped session from durable store; turn continues on a working session"
        );

        self.ctx.sessions.get_agent(sid).await
    }

    pub(crate) async fn handle_session_prompt(&self, params: &Value) -> RpcResult {
        let req: SessionPromptParams = parse_params(params)?;
        let sid = &req.session_id;

        if req.prompt.trim().is_empty() && req.attachments.is_empty() {
            return Err(rpc_err(
                INVALID_PARAMS,
                "session/prompt requires a non-empty `prompt` or at least one attachment",
            ));
        }

        let agent = match self.ctx.sessions.get_agent(sid).await {
            Some(a) => a,
            None => match self.rehydrate_reaped_session(sid).await {
                Some(a) => a,
                None => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail,)
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({ "session_id": sid })),
                        "session/prompt on a session absent from memory and the durable store; emitting TurnComplete so the client exits the working state"
                    );
                    self.emit_turn_complete(
                        sid,
                        crate::rpc::types::TurnCompletionOutcome::Failed,
                        "turn cancelled by daemon: session_not_found".to_string(),
                    )
                    .await;
                    return Err(rpc_err(SESSION_NOT_FOUND, "Session not found"));
                }
            },
        };

        // Process inline attachments: upload each, append markers to prompt.
        let mut prompt = req.prompt.clone();
        if !req.attachments.is_empty() {
            use super::attachments::process_file_entry;

            let agent_alias = self
                .ctx
                .sessions
                .get_agent_alias(sid)
                .await
                .ok_or_else(|| rpc_err(SESSION_NOT_FOUND, "Session not found"))?;
            let upload_root = self
                .ctx
                .config
                .read()
                .agent_workspace_dir(&agent_alias)
                .to_string_lossy()
                .to_string();
            let is_wss = self.peer_label.starts_with("wss:");
            if !prompt.is_empty() {
                prompt.push('\n');
            }
            for (idx, entry) in req.attachments.iter().enumerate() {
                let result =
                    process_file_entry(entry, sid, &upload_root, is_wss, &self.ctx.sessions)
                        .await?;
                if idx > 0 {
                    prompt.push('\n');
                }
                prompt.push_str(&result.marker);
            }
        }

        let _guard = self
            .ctx
            .sessions
            .session_queue
            .acquire(sid)
            .await
            .map_err(|e| rpc_err(SESSION_BUSY, format!("Session busy: {e}")))?;

        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_generation = self.ctx.sessions.register_cancel_token(sid, cancel.clone());
        self.ctx.sessions.touch(sid).await;
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Invoke)
                .with_category(::zeroclaw_log::EventCategory::Agent)
                .with_attrs(::serde_json::json!({ "session_id": sid })),
            "turn dispatch: registered cancel token, starting turn"
        );

        let chat_mode = self
            .ctx
            .sessions
            .chat_mode(sid)
            .await
            .unwrap_or(crate::rpc::types::ChatMode::Chat);
        // Capture live attribution fields and max_context_tokens for the turn span.
        // Zerocode's context meter field is named `max_context_tokens` and must
        // reflect the runtime-profile budget (`[runtime_profiles.<name>]
        // max_context_tokens`), not the provider model-window helper (which
        // falls back to 32_000 when `context_window` is unset).
        let (agent_alias, model_provider, model, max_ctx) = {
            let alias = self
                .ctx
                .sessions
                .get_agent_alias(sid)
                .await
                .unwrap_or_default();
            let (mp, m) = if let Some(agent) = self.ctx.sessions.get_agent(sid).await {
                let (_, model_provider, model) = agent.lock().await.attribution_fields();
                (model_provider, model)
            } else {
                (String::new(), String::new())
            };
            let max_ctx = {
                let cfg = self.ctx.config.read();
                Some(context_usage_max_tokens(&cfg, &alias))
            };
            (alias, mp, m, max_ctx)
        };

        let rpc = self.rpc.clone();
        let sid_owned = sid.to_string();
        // Clone of the session store so the turn-event closure can persist
        // the latest TodoWrite plan (store-then-emit) before the plan
        // notification goes out. See `persist_plan_if_any`.
        let sessions_for_plan = self.ctx.sessions.clone();
        let acp_token_store = if matches!(chat_mode, crate::rpc::types::ChatMode::Acp) {
            self.ctx.acp_session_store.clone()
        } else {
            None
        };
        let attribution_agent_alias = agent_alias.clone();
        let attribution_model_provider = model_provider.clone();
        let attribution_model = model.clone();
        // Cost-tracking context for this turn. Built from the daemon-scoped
        // tracker + the live pricing map and stamped with the agent alias so
        // `execute_turn` can persist token usage and attribute spend. `None`
        // when cost tracking is disabled (no tracker wired).
        let cost_context = self.ctx.cost_tracker.as_ref().map(|tracker| {
            let cfg_guard = self.ctx.config.read();
            let pricing = crate::agent::cost::build_model_provider_pricing(&cfg_guard);
            crate::agent::cost::ToolLoopCostTrackingContext::new(
                tracker.clone(),
                std::sync::Arc::new(pricing),
            )
            .with_agent_alias(&attribution_agent_alias)
        });
        let outcome = execute_turn(
            agent,
            prompt.clone(),
            cancel,
            TurnAttribution {
                session_key: Some(sid.to_string()),
                agent_alias,
                model_provider,
                model,
                channel: "rpc",
            },
            cost_context,
            move |event| {
                let rpc = rpc.clone();
                let sid = sid_owned.clone();
                let acp_token_store = acp_token_store.clone();
                let sessions_for_plan = sessions_for_plan.clone();
                async move {
                    if let (
                        Some(store),
                        TurnEvent::Usage {
                            input_tokens: Some(it),
                            ..
                        },
                    ) = (acp_token_store.as_ref(), &event)
                    {
                        let store = store.clone();
                        let sid = sid.clone();
                        let it = *it;
                        let _ =
                            tokio::task::spawn_blocking(move || store.set_token_count(&sid, it))
                                .await;
                    }
                    persist_plan_if_any(&sessions_for_plan, acp_token_store.as_ref(), &sid, &event)
                        .await;
                    if let Some(n) = notification_for_turn_event(&sid, &event, max_ctx) {
                        let _ = rpc.send_raw(n).await;
                    }
                }
            },
        )
        .await;

        // Drain the cancel cause BEFORE removing the token (removal clears the
        // cause map). Every cancel firing site records its cause before firing;
        // a cancel with no recorded cause is a bug, not user attribution.
        let cancel_cause = self.ctx.sessions.take_cancel_cause(sid);
        self.ctx
            .sessions
            .remove_cancel_token(sid, cancel_generation);

        // ── Durable turn-verdict audit row ───────────────────────────────
        // Every turn termination writes one attributed row to the ACP session
        // store's event log so a cancel verdict is diagnosable after the trace
        // log rotates. Fire-and-forget on a blocking task.
        if matches!(chat_mode, crate::rpc::types::ChatMode::Acp)
            && let Some(store) = self.ctx.acp_session_store.clone()
        {
            let (action, event_outcome, payload) = match &outcome {
                Ok(crate::rpc::turn::TurnOutcome::Completed { .. }) => (
                    ::zeroclaw_log::Action::Complete,
                    ::zeroclaw_log::EventOutcome::Success,
                    None,
                ),
                Ok(crate::rpc::turn::TurnOutcome::Cancelled { .. }) => (
                    ::zeroclaw_log::Action::Cancel,
                    ::zeroclaw_log::EventOutcome::Unknown,
                    Some(
                        ::serde_json::json!({
                            "cancel_cause": cancel_cause.map(|c| c.as_str()),
                        })
                        .to_string(),
                    ),
                ),
                Err(e) => (
                    ::zeroclaw_log::Action::Fail,
                    ::zeroclaw_log::EventOutcome::Failure,
                    Some(::serde_json::json!({ "error": e.to_string() }).to_string()),
                ),
            };
            let sid_owned = sid.to_string();
            let span_session = sid.to_string();
            let span_alias = attribution_agent_alias.clone();
            let span_provider = attribution_model_provider.clone();
            let span_model = attribution_model.clone();
            zeroclaw_spawn::spawn!(async move {
                use ::zeroclaw_log::Instrument as _;
                let span = ::zeroclaw_log::info_span!(
                    target: "zeroclaw_log_internal_scope",
                    "zeroclaw_scope",
                    session_key = %span_session,
                    agent_alias = %span_alias,
                    model_provider = %span_provider,
                    model = %span_model,
                    channel = "rpc",
                );
                async move {
                    let persisted = tokio::task::spawn_blocking(move || {
                        store.append_event(&sid_owned, action, event_outcome, payload.as_deref())
                    })
                    .await;
                    let error = match persisted {
                        Ok(Ok(())) => return,
                        Ok(Err(e)) => e.to_string(),
                        Err(join) => join.to_string(),
                    };
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Write)
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({ "error": error })),
                        "Failed to persist ACP turn-verdict audit event"
                    );
                }
                .instrument(span)
                .await;
            });
        }

        match chat_mode {
            crate::rpc::types::ChatMode::Acp => {
                if let Some(ref store) = self.ctx.acp_session_store
                    && let Some(detail) = persist_acp_turn(store, sid, &outcome).await
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"session_id": sid, "error": detail})),
                        "Failed to persist ACP turn"
                    );
                }
            }
            crate::rpc::types::ChatMode::Chat => {
                if let Some(ref backend) = self.ctx.session_backend {
                    let key = format!("rpc_{sid}");
                    let _ = backend.append(&key, &ChatMessage::user(&prompt));
                    match &outcome {
                        Ok(TurnOutcome::Completed { text, .. }) => {
                            let _ = backend.append(&key, &ChatMessage::assistant(text));
                        }
                        Ok(TurnOutcome::Cancelled { partial_text, .. })
                            if !partial_text.is_empty() =>
                        {
                            let _ = backend.append(&key, &ChatMessage::assistant(partial_text));
                        }
                        _ => {}
                    }
                }
            }
        }

        match outcome {
            Ok(TurnOutcome::Completed { text, .. }) => {
                self.emit_turn_complete(
                    &req.session_id,
                    crate::rpc::types::TurnCompletionOutcome::Completed,
                    text.clone(),
                )
                .await;
                to_result(SessionPromptResult {
                    session_id: req.session_id,
                    stop_reason: "end_turn".to_string(),
                    content: text,
                })
            }
            Ok(TurnOutcome::Cancelled { partial_text, .. }) => {
                let cancel_message = match cancel_cause {
                    Some(cause) => {
                        format!(
                            "turn cancelled via {} in RPC_SESSION {}",
                            cause.as_str(),
                            req.session_id
                        )
                    }
                    None => {
                        format!(
                            "turn cancelled (cause unattributed) in RPC_SESSION {}",
                            req.session_id
                        )
                    }
                };
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Cancel)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({
                            "session_id": req.session_id,
                            "agent_alias": attribution_agent_alias,
                            "model_provider": attribution_model_provider,
                            "model": attribution_model,
                            "chat_mode": format!("{chat_mode:?}"),
                            "cancel_cause": cancel_cause.map(|c| c.as_str()),
                        })),
                    "turn cancelled; emitting attributed TurnComplete so the client exits the working state"
                );
                self.emit_turn_complete(
                    &req.session_id,
                    crate::rpc::types::TurnCompletionOutcome::Cancelled,
                    cancel_message,
                )
                .await;
                to_result(SessionPromptResult {
                    session_id: req.session_id,
                    stop_reason: "cancelled".to_string(),
                    content: partial_text,
                })
            }
            Err(e) => {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Agent)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "session_id": req.session_id,
                            "agent_alias": attribution_agent_alias,
                            "model_provider": attribution_model_provider,
                            "model": attribution_model,
                            "chat_mode": format!("{chat_mode:?}"),
                            "error": e.to_string(),
                        })),
                    "turn failed; emitting TurnComplete so the client exits the working state"
                );
                self.emit_turn_complete(
                    &req.session_id,
                    crate::rpc::types::TurnCompletionOutcome::Failed,
                    format!("turn failed: {e}"),
                )
                .await;
                Err(rpc_err(INTERNAL_ERROR, e.to_string()))
            }
        }
    }

    /// Emit the terminal `session/update` notification for a turn.
    /// The TUI uses this — not the JSON-RPC response — to flip
    /// `turn_in_flight` back to false.
    async fn emit_turn_complete(
        &self,
        session_id: &str,
        outcome: crate::rpc::types::TurnCompletionOutcome,
        content: String,
    ) {
        let update = SessionUpdateEvent::TurnComplete {
            session_id: session_id.to_string(),
            outcome,
            content,
        };
        if let Ok(params) = serde_json::to_value(update) {
            let n = JsonRpcNotification::new(notification::SESSION_UPDATE, params);
            if let Ok(s) = serde_json::to_string(&n) {
                let _ = self.rpc.send_raw(s).await;
            }
        }
    }

    pub(crate) async fn handle_session_configure(&self, params: &Value) -> RpcResult {
        let req: SessionConfigureParams = parse_params(params)?;
        validate_session_configure_overrides(&req.overrides)?;
        let _model_provider_update = self
            .ctx
            .sessions
            .lock_model_provider_update(&req.session_id)
            .await
            .ok_or_else(|| rpc_err(SESSION_NOT_FOUND, "Session not found"))?;

        let merged = self
            .ctx
            .sessions
            .preview_overrides(&req.session_id, &req.overrides)
            .await
            .ok_or_else(|| rpc_err(SESSION_NOT_FOUND, "Session not found"))?;

        // Model/model_provider overrides need a live provider-box rebuild,
        // which requires Config — held here, not in the session store. Resolve
        // the provider from the prospective merged override or configured
        // agent, build the box, and only then commit the override.
        let built_model_provider = if merged.model_provider.is_some() || merged.model.is_some() {
            let agent_alias = self
                .ctx
                .sessions
                .get_agent_alias(&req.session_id)
                .await
                .ok_or_else(|| rpc_err(SESSION_NOT_FOUND, "Session not found"))?;
            let built = {
                let config = self.ctx.config.read();
                let agent_cfg = config
                    .resolved_agent_config(&agent_alias)
                    .or_else(|| config.agent(&agent_alias).cloned())
                    .ok_or_else(|| {
                        rpc_err(
                            INVALID_PARAMS,
                            format!("Agent `{agent_alias}` is not configured"),
                        )
                    })?;
                let model_provider_ref = merged
                    .model_provider
                    .as_deref()
                    .unwrap_or_else(|| agent_cfg.model_provider.as_str());
                let (model_provider, model_provider_name, model_name) =
                    crate::agent::agent::build_session_model_provider(
                        &config,
                        model_provider_ref,
                        merged.model.as_deref(),
                    )
                    .map_err(|e| rpc_err(INVALID_PARAMS, e.to_string()))?;
                let tool_dispatcher = crate::agent::agent::tool_dispatcher_for_provider(
                    &agent_cfg,
                    model_provider.as_ref(),
                );
                (
                    model_provider,
                    model_provider_name,
                    model_name,
                    tool_dispatcher,
                )
            };
            Some(built)
        } else {
            None
        };

        let merged = self
            .ctx
            .sessions
            .set_overrides(&req.session_id, req.overrides)
            .await
            .ok_or_else(|| rpc_err(SESSION_NOT_FOUND, "Session not found"))?;

        if let Some((model_provider, model_provider_name, model_name, tool_dispatcher)) =
            built_model_provider
        {
            self.ctx
                .sessions
                .apply_model_provider(
                    &req.session_id,
                    model_provider,
                    model_provider_name,
                    model_name,
                    tool_dispatcher,
                )
                .await
                .then_some(())
                .ok_or_else(|| rpc_err(SESSION_NOT_FOUND, "Session not found"))?;
        }

        to_result(SessionConfigureResult {
            session_id: req.session_id,
            overrides: merged,
        })
    }

    pub(crate) async fn handle_session_cancel(&self, params: &Value) -> RpcResult {
        let req: SessionIdParams = parse_params(params)?;
        let owner = self
            .ctx
            .sessions
            .session_owner_tui_id(&req.session_id)
            .await;
        let allowed = match (
            owner.as_ref().and_then(|o| o.as_deref()),
            self.tui_id.as_deref(),
        ) {
            (Some(o), Some(c)) => o == c,
            _ => false,
        };
        if !allowed {
            let (agent_alias, model_provider, model) =
                match self.ctx.sessions.get_agent(&req.session_id).await {
                    Some(agent) => agent.lock().await.attribution_fields(),
                    None => (String::new(), String::new(), String::new()),
                };
            let span = ::zeroclaw_log::info_span!(
                target: "zeroclaw_log_internal_scope",
                "zeroclaw_scope",
                session_key = %req.session_id,
                agent_alias = %agent_alias,
                model_provider = %model_provider,
                model = %model,
                channel = "rpc",
            );
            let _guard = span.enter();
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_category(::zeroclaw_log::EventCategory::Channel)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "caller_tui_id": self.tui_id.as_deref().unwrap_or("<none>"),
                        "owner_tui_id": owner
                            .as_ref()
                            .and_then(|o| o.as_deref())
                            .unwrap_or("<none>"),
                        "peer_label": &self.peer_label,
                    })),
                "session/cancel refused: caller does not own the session"
            );
            return Err(rpc_err(
                SESSION_NOT_OWNED,
                "Caller does not own this session",
            ));
        }
        if self.ctx.sessions.cancel_session(&req.session_id) {
            to_result(SessionCancelResult {
                session_id: req.session_id,
                cancelled: true,
            })
        } else {
            Err(rpc_err(
                SESSION_NOT_FOUND,
                "No active turn for this session",
            ))
        }
    }

    pub(crate) async fn handle_session_git_branch(&self, params: &Value) -> RpcResult {
        let req: SessionIdParams = parse_params(params)?;
        let cwd = self
            .ctx
            .sessions
            .get_workspace_dir(&req.session_id)
            .await
            .ok_or_else(|| rpc_err(SESSION_NOT_FOUND, "session not found"))?;
        let info = crate::rpc::git::head_info(std::path::Path::new(&cwd)).unwrap_or_default();
        to_result(SessionGitBranchResult {
            session_id: req.session_id,
            branch: info.branch,
            hash: info.hash,
        })
    }

    pub(crate) async fn handle_session_list(&self, params: &Value) -> RpcResult {
        let backend = self
            .ctx
            .session_backend
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "Session persistence is disabled"))?;
        let req: SessionListParams = parse_params(params)?;
        let config = self.ctx.config.read().clone();

        // Use FTS when a query is provided, plain list otherwise.
        let all = if let Some(ref keyword) = req.query {
            if keyword.trim().is_empty() {
                backend.list_sessions_with_metadata()
            } else {
                use zeroclaw_infra::session_backend::SessionQuery;
                backend.search(&SessionQuery {
                    keyword: Some(keyword.clone()),
                    limit: req.limit,
                })
            }
        } else {
            backend.list_sessions_with_metadata()
        };

        let sessions: Vec<SessionEntry> = all
            .into_iter()
            .filter(|meta| meta.agent_alias.is_some() || meta.channel_id.is_some())
            .map(|meta| {
                let agent_alias = meta.agent_alias.clone().or_else(|| {
                    meta.channel_id
                        .as_deref()
                        .and_then(|c| config.agent_for_channel(c))
                        .map(str::to_string)
                });
                let session_id = meta
                    .key
                    .strip_prefix("rpc_")
                    .or_else(|| meta.key.strip_prefix("gw_"))
                    .map(str::to_string)
                    .unwrap_or_else(|| meta.key.clone());
                SessionEntry {
                    session_id,
                    session_key: meta.key,
                    created_at: meta.created_at.to_rfc3339(),
                    last_activity: meta.last_activity.to_rfc3339(),
                    message_count: meta.message_count,
                    agent_alias,
                    channel_id: meta.channel_id,
                    name: meta.name,
                }
            })
            .collect();
        to_result(SessionListResult { sessions })
    }

    /// List ACP sessions from the dedicated ACP session store. The Code (ACP)
    /// pane in the TUI calls this instead of `session/list` so its picker only
    /// shows sessions that came from `acp-sessions.db` — chat-pane sessions
    /// live in the unified `session_backend` and must not appear here.
    pub(crate) async fn handle_session_list_acp(&self, _params: &Value) -> RpcResult {
        let store = self
            .ctx
            .acp_session_store
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "ACP session store is not available"))?;

        let summaries = store
            .list_sessions()
            .map_err(|e| rpc_err(INTERNAL_ERROR, format!("acp session list failed: {e}")))?;

        let sessions: Vec<SessionEntry> = summaries
            .into_iter()
            .map(|s| SessionEntry {
                session_id: s.session_uuid.clone(),
                // ACP sessions are keyed by their UUID directly — no `rpc_`/`gw_`
                // prefix exists in this store, so session_id == session_key.
                session_key: s.session_uuid,
                created_at: s.created_at.to_rfc3339(),
                last_activity: s.last_activity.to_rfc3339(),
                message_count: s.message_count,
                agent_alias: Some(s.agent_alias),
                channel_id: None,
                // ACP sessions don't carry a user-set display name today; the
                // picker falls back to `session_id` when this is None.
                name: None,
            })
            .collect();

        to_result(SessionListResult { sessions })
    }

    pub(crate) async fn handle_session_messages(&self, params: &Value) -> RpcResult {
        let req: SessionMessagesParams = parse_params(params)?;
        let backend = self
            .ctx
            .session_backend
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "Session persistence is disabled"))?;

        // Try the raw id first (channel sessions store as-is), then
        // prefixed variants for RPC/gateway-originated sessions.
        let candidates = [
            req.session_id.clone(),
            format!("rpc_{}", req.session_id),
            format!("gw_{}", req.session_id),
        ];
        let mut raw: Vec<zeroclaw_api::model_provider::ChatMessage> = Vec::new();
        for key in &candidates {
            let loaded = backend.load(key);
            if !loaded.is_empty() {
                raw = loaded;
                break;
            }
        }

        if raw.is_empty()
            && let Some(store) = self.ctx.acp_session_store.as_ref()
        {
            match store.load_session(&req.session_id) {
                Ok(Some(data)) => {
                    raw = data
                        .messages
                        .into_iter()
                        .filter_map(|m| {
                            match m {
                            zeroclaw_api::model_provider::ConversationMessage::Chat(c) => Some(c),
                            zeroclaw_api::model_provider::ConversationMessage::AssistantToolCalls {
                                text: Some(t),
                                ..
                            } if !t.is_empty() => {
                                Some(zeroclaw_api::model_provider::ChatMessage::assistant(t))
                            }
                            zeroclaw_api::model_provider::ConversationMessage::AssistantToolCalls {
                                ..
                            }
                            | zeroclaw_api::model_provider::ConversationMessage::ToolResults(_) => {
                                None
                            }
                        }
                        })
                        .collect();
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(rpc_err(
                        INTERNAL_ERROR,
                        format!("Failed to load ACP session messages: {e}"),
                    ));
                }
            }
        }

        let total = raw.len();
        let limit = req.limit.unwrap_or(total);
        let end = req.before_index.map(|i| i.min(total)).unwrap_or(total);
        let start = end.saturating_sub(limit);
        let messages: Vec<MessageEntry> = raw[start..end]
            .iter()
            .map(|m| MessageEntry {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        to_result(SessionMessagesResult {
            session_id: req.session_id,
            messages,
            total,
            start,
        })
    }

    pub(crate) async fn handle_session_state(&self, params: &Value) -> RpcResult {
        let req: SessionIdParams = parse_params(params)?;
        let backend = self
            .ctx
            .session_backend
            .as_ref()
            .ok_or_else(|| rpc_err(INTERNAL_ERROR, "Session persistence is disabled"))?;
        let candidates = [
            req.session_id.clone(),
            format!("rpc_{}", req.session_id),
            format!("gw_{}", req.session_id),
        ];
        for key in &candidates {
            match backend.get_session_state(key) {
                Ok(Some(ss)) => {
                    return to_result(SessionStateResult {
                        session_id: req.session_id,
                        state: ss.state,
                        turn_id: ss.turn_id,
                        turn_started_at: ss.turn_started_at.map(|t| t.to_rfc3339()),
                    });
                }
                Ok(None) => continue,
                Err(e) => {
                    return Err(rpc_err(
                        INTERNAL_ERROR,
                        format!("Failed to get session state: {e}"),
                    ));
                }
            }
        }
        Err(rpc_err(SESSION_NOT_FOUND, "Session not found"))
    }

    pub(crate) async fn handle_session_delete(&self, params: &Value) -> RpcResult {
        let req: SessionIdParams = parse_params(params)?;
        if let Some(agent) = self.ctx.sessions.get_agent(&req.session_id).await {
            agent
                .lock()
                .await
                .channel_handles()
                .unregister_channel("rpc");
        }
        let existed = self.ctx.sessions.remove(&req.session_id).await;
        if existed && let Some(ref hooks) = self.ctx.hooks {
            hooks.fire_session_end(&req.session_id, "rpc").await;
        }
        // Remove from persistent backend — try raw id, then prefixed variants.
        if let Some(ref backend) = self.ctx.session_backend {
            for key in &[
                req.session_id.clone(),
                format!("rpc_{}", req.session_id),
                format!("gw_{}", req.session_id),
            ] {
                let _ = backend.delete_session(key);
            }
        }
        to_result(SessionDeleteResult {
            session_id: req.session_id,
            deleted: true,
        })
    }

    pub(crate) fn handle_session_approve(&self, params: &Value) -> RpcResult {
        let p: SessionApproveParams = parse_params(params)?;

        let response = match p.decision.as_str() {
            "allow_once" => zeroclaw_api::channel::ChannelApprovalResponse::Approve,
            "allow_always" => zeroclaw_api::channel::ChannelApprovalResponse::AlwaysApprove,
            "reject" | "reject_once" => zeroclaw_api::channel::ChannelApprovalResponse::Deny,
            "reject_with_edit" => {
                let replacement = p.replacement.unwrap_or_default();
                zeroclaw_api::channel::ChannelApprovalResponse::DenyWithEdit { replacement }
            }
            other => {
                return Err(rpc_err(
                    INVALID_PARAMS,
                    format!("unknown decision: {other}"),
                ));
            }
        };

        self.ctx.approval_pending.resolve(&p.request_id, response);

        to_result(SessionApproveResult {
            session_id: p.session_id,
            request_id: p.request_id,
            acknowledged: true,
        })
    }
}
