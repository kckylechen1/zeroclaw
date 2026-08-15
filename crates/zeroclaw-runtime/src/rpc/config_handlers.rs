//! Config JSON-RPC method handlers extracted from dispatch.rs.

use super::dispatch::{BoxRpcFuture, RpcDispatcher, RpcResult, parse_params, rpc_err, to_result};
use super::types::*;
use serde_json::Value;
use std::sync::Arc;
use zeroclaw_api::jsonrpc::JsonRpcError;
use zeroclaw_api::jsonrpc::error_codes::*;
use zeroclaw_config::schema::Config;

use super::context::{ConfigWriteGuard, RpcContext};
use super::types::SessionOverrides;

fn model_provider_ref_from_provider_profile_prop(prop: &str) -> Option<String> {
    let rest = prop.strip_prefix("providers.models.")?;
    let (provider_type, rest) = rest.split_once('.')?;
    let (provider_alias, field) = rest.split_once('.')?;
    if provider_type.is_empty() || provider_alias.is_empty() || field.is_empty() {
        None
    } else {
        Some(format!("{provider_type}.{provider_alias}"))
    }
}

/// Extract the agent alias from an `agents.<alias>.model_provider` prop path.
/// A live change to an agent's bound provider must rebuild that agent's live
/// session boxes the same way a `providers.models.*` edit does, so any
/// `config/set agents.<alias>.model_provider` caller (the config pane and other
/// RPC/config-set clients) gets a live refresh.
pub(crate) fn agent_alias_from_model_provider_prop(prop: &str) -> Option<String> {
    let rest = prop.strip_prefix("agents.")?;
    let (alias, field) = rest.split_once('.')?;
    if alias.is_empty() || field != "model_provider" {
        None
    } else {
        Some(alias.to_string())
    }
}

/// Session-selection predicate for an agent-scoped `model_provider` refresh
/// (`config/set agents.<alias>.model_provider`). Only sessions bound to the
/// edited agent are eligible, and a session that carries its own
/// `model_provider` override is excluded so unrelated agents and overridden
/// sessions are never rebuilt.
pub(crate) fn agent_scoped_refresh_selects(
    edited_agent: &str,
    session_agent: &str,
    overrides: &SessionOverrides,
) -> bool {
    session_agent == edited_agent && overrides.model_provider.is_none()
}

/// Session-selection predicate for a provider-scoped refresh
/// (`providers.models.*` edit). A session is eligible when its own
/// `model_provider` override matches the edited provider, or when it has no
/// override and thus inherits the agent's provider (final provider match is
/// resolved separately against config).
pub(crate) fn provider_scoped_refresh_selects(
    target_ref: &str,
    overrides: &SessionOverrides,
) -> bool {
    overrides
        .model_provider
        .as_deref()
        .map(|r| r == target_ref)
        .unwrap_or(true)
}

/// Whether memory embeddings resolve from the given `<type>.<alias>` provider
/// profile — either the base `[memory].embedding_provider` reference or any
/// `[[embedding_routes]]` entry. Gates the memory-embedder refresh on a
/// `config/set` provider-profile change
pub(crate) fn memory_embeddings_use_provider(
    config: &zeroclaw_config::schema::Config,
    model_provider_ref: &str,
) -> bool {
    config.memory.embedding_provider.trim() == model_provider_ref
        || config
            .embedding_routes
            .iter()
            .any(|route| route.model_provider.trim() == model_provider_ref)
}

fn rename_error_to_rpc(
    path: &str,
    from: &str,
    err: zeroclaw_config::alias_refs::RenameError,
) -> JsonRpcError {
    use zeroclaw_config::alias_refs::RenameError;
    let code = match err {
        RenameError::PostCondition(_) => INTERNAL_ERROR,
        _ => INVALID_PARAMS,
    };
    rpc_err(code, format!("{path}.{from}: {err}"))
}

async fn move_renamed_agent_workspace(
    old_workspace: &std::path::Path,
    new_workspace: &std::path::Path,
) -> Option<String> {
    if old_workspace == new_workspace || !old_workspace.exists() {
        return None;
    }
    if let Some(parent) = new_workspace.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    match tokio::fs::rename(old_workspace, new_workspace).await {
        Ok(()) => None,
        Err(err) => Some(format!(
            "workspace move {} -> {} failed: {err}",
            old_workspace.display(),
            new_workspace.display()
        )),
    }
}

impl RpcDispatcher {
    pub(crate) fn handle_config_get(&self, params: &Value) -> RpcResult {
        use zeroclaw_config::traits::MaskSecrets;
        let req: ConfigGetParams = parse_params(params)?;
        let config = self.ctx.config.read().clone();
        if let Some(prop) = req.prop {
            let val = config
                .get_prop(&prop)
                .map_err(|e| rpc_err(INVALID_PARAMS, format!("Unknown prop: {e}")))?;
            to_result(ConfigGetPropResult { prop, value: val })
        } else {
            // Return full config, masked.
            let mut masked = config;
            masked.mask_secrets();
            Ok(serde_json::to_value(&masked).unwrap_or(Value::Null))
        }
    }

    pub(crate) async fn handle_config_set(&self, params: &Value) -> RpcResult {
        let req: ConfigSetParams = parse_params(params)?;
        let refresh_model_provider_ref = model_provider_ref_from_provider_profile_prop(&req.prop);
        let config_write_guard = Arc::clone(&self.ctx.config_write_lock).lock_owned().await;
        {
            let mut config = self.ctx.config.write();
            if config.ensure_map_key_for_path(&req.prop) {
                // Refused to vivify the reserved `default` agent: return a
                // reserved error rather than a downstream "Unknown property".
                return Err(rpc_err(
                    INVALID_PARAMS,
                    "alias `default` is reserved and cannot be created",
                ));
            }
            let info = config
                .prop_fields()
                .into_iter()
                .find(|f| f.name == req.prop);
            // Polymorphic value: strings pass through, everything else coerced.
            let value_str = match &req.value {
                Value::String(s) => s.clone(),
                other => zeroclaw_config::typed_value::coerce_for_set_prop(
                    other,
                    info.as_ref().map(|i| i.kind),
                )
                .map_err(|e| rpc_err(INVALID_PARAMS, e.message))?,
            };
            // Reject the masked sentinel for secrets — surfaces echo the
            // masked display value back when no real edit happened, and
            // letting that through silently clobbers the live secret with
            // the literal masked string.
            let is_secret_prop = info
                .as_ref()
                .is_some_and(|i| i.is_secret || i.derived_from_secret)
                || zeroclaw_config::schema::Config::prop_is_secret(&req.prop);
            if is_secret_prop
                && (value_str == zeroclaw_config::traits::MASKED_SECRET
                    || value_str == "****"
                    || value_str.is_empty())
            {
                return Err(rpc_err(
                    INVALID_PARAMS,
                    format!(
                        "Refusing to overwrite secret `{}` with a masked or empty value",
                        req.prop
                    ),
                ));
            }
            config
                .set_prop_persistent(&req.prop, &value_str)
                .map_err(|e| rpc_err(INTERNAL_ERROR, format!("Config set failed: {e}")))?;
        }
        self.flush_config(&config_write_guard).await?;
        if let Some(model_provider_ref) = refresh_model_provider_ref {
            self.refresh_memory_embedder_for_model_provider(&model_provider_ref);
            self.schedule_live_sessions_refresh_for_model_provider(model_provider_ref);
        }
        if let Some(agent_alias) = agent_alias_from_model_provider_prop(&req.prop) {
            self.schedule_live_sessions_refresh_for_agent(agent_alias);
        }
        to_result(ConfigSetResult {
            prop: req.prop,
            set: true,
        })
    }

    pub(crate) fn refresh_memory_embedder_for_model_provider(&self, model_provider_ref: &str) {
        let resolved = {
            let config = self.ctx.config.read();
            if !memory_embeddings_use_provider(&config, model_provider_ref) {
                return;
            }
            // Match daemon-boot resolution (`create_memory_with_storage_and_routes`
            // is called with `api_key = None`): keys come from the per-route /
            // `[memory]` override or the referenced profile, never an inherited seed.
            zeroclaw_memory::resolve_embedding_settings(
                &config.memory,
                &config.embedding_routes,
                None,
                Some(&config.providers.models),
            )
        };
        // 1. Install-wide RPC memory handle.
        if let Some(memory) = self.ctx.memory.as_ref() {
            memory.refresh_embedder(
                &resolved.model_provider,
                resolved.api_key.as_deref(),
                &resolved.model,
                resolved.dimensions,
            );
        }
        self.schedule_live_agent_memory_refresh(resolved);
    }

    pub(crate) fn schedule_live_agent_memory_refresh(
        &self,
        resolved: zeroclaw_memory::EmbeddingSettings,
    ) {
        let ctx = Arc::clone(&self.ctx);
        zeroclaw_spawn::spawn!(async move {
            Self::refresh_live_agent_memory(ctx, resolved).await;
        });
    }

    pub(crate) async fn refresh_live_agent_memory(
        ctx: Arc<RpcContext>,
        resolved: zeroclaw_memory::EmbeddingSettings,
    ) {
        for session_id in ctx.sessions.list_ids().await {
            if let Some(agent) = ctx.sessions.get_agent(&session_id).await {
                agent.lock().await.refresh_memory_embedder(
                    &resolved.model_provider,
                    resolved.api_key.as_deref(),
                    &resolved.model,
                    resolved.dimensions,
                );
            }
        }
    }

    pub(crate) fn schedule_live_sessions_refresh_for_model_provider(
        &self,
        model_provider_ref: String,
    ) {
        let ctx = Arc::clone(&self.ctx);
        zeroclaw_spawn::spawn!(async move {
            Self::refresh_live_sessions_for_model_provider(ctx, &model_provider_ref).await;
        });
    }

    /// Rebuild the live agent box for every session bound to `agent_alias`,
    /// resolving the agent's currently-configured `model_provider` from config.
    /// Fired when `agents.<alias>.model_provider` changes via `config/set` so a
    /// provider switch takes effect on the running session without a restart —
    /// the same refresh a `providers.models.*` edit triggers. Only sessions
    /// bound to the edited agent are rebuilt; sessions belonging to other
    /// agents, and sessions that carry their own `model_provider` override, are
    /// left untouched even when they resolve to the same provider.
    pub(crate) fn schedule_live_sessions_refresh_for_agent(&self, agent_alias: String) {
        let ctx = Arc::clone(&self.ctx);
        zeroclaw_spawn::spawn!(async move {
            Self::refresh_live_sessions_for_agent(ctx, &agent_alias).await;
        });
    }

    pub(crate) async fn refresh_live_sessions_for_agent(ctx: Arc<RpcContext>, agent_alias: &str) {
        Self::refresh_live_sessions_matching(ctx, |config, session_agent, overrides| {
            if !agent_scoped_refresh_selects(agent_alias, session_agent, overrides) {
                return None;
            }
            config
                .agent(agent_alias)
                .map(|agent| agent.model_provider.to_string())
        })
        .await;
    }

    pub(crate) async fn refresh_live_sessions_for_model_provider(
        ctx: Arc<RpcContext>,
        model_provider_ref: &str,
    ) {
        let target_ref = model_provider_ref.to_string();
        Self::refresh_live_sessions_matching(ctx, move |config, session_agent, overrides| {
            if !provider_scoped_refresh_selects(&target_ref, overrides) {
                return None;
            }
            let effective_ref = overrides.model_provider.as_deref().or_else(|| {
                config
                    .agent(session_agent)
                    .map(|agent| agent.model_provider.as_str())
            });
            (effective_ref == Some(target_ref.as_str())).then(|| target_ref.clone())
        })
        .await;
    }

    pub(crate) async fn refresh_live_sessions_matching<F>(
        ctx: Arc<RpcContext>,
        resolve_provider_ref: F,
    ) where
        F: Fn(&Config, &str, &SessionOverrides) -> Option<String>,
    {
        let session_ids = ctx.sessions.list_ids().await;
        for session_id in session_ids {
            let Some(_model_provider_update) =
                ctx.sessions.lock_model_provider_update(&session_id).await
            else {
                continue;
            };
            let Some(agent_alias) = ctx.sessions.get_agent_alias(&session_id).await else {
                continue;
            };
            let Some(overrides) = ctx.sessions.get_overrides(&session_id).await else {
                continue;
            };
            let (model_provider, model_provider_name, model_name, tool_dispatcher, temperature) = {
                let config = ctx.config.read();
                let Some(model_provider_ref) =
                    resolve_provider_ref(&config, &agent_alias, &overrides)
                else {
                    continue;
                };
                let provider_temperature = model_provider_ref.split_once('.').and_then(
                    |(provider_type, provider_alias)| {
                        config
                            .providers
                            .models
                            .find(provider_type, provider_alias)
                            .and_then(|entry| entry.temperature)
                    },
                );
                let Some(agent_cfg) = config
                    .resolved_agent_config(&agent_alias)
                    .or_else(|| config.agent(&agent_alias).cloned())
                else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "session_id": session_id,
                                "agent_alias": agent_alias,
                                "model_provider": model_provider_ref,
                            })),
                        "config/set saved provider profile but live session refresh could not resolve agent config"
                    );
                    continue;
                };
                match crate::agent::agent::build_session_model_provider(
                    &config,
                    &model_provider_ref,
                    overrides.model.as_deref(),
                ) {
                    Ok((model_provider, model_provider_name, model_name)) => {
                        let tool_dispatcher = crate::agent::agent::tool_dispatcher_for_provider(
                            &agent_cfg,
                            model_provider.as_ref(),
                        );
                        (
                            model_provider,
                            model_provider_name,
                            model_name,
                            tool_dispatcher,
                            overrides.temperature.or(provider_temperature),
                        )
                    }
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "session_id": session_id,
                                "agent_alias": agent_alias,
                                "model_provider": model_provider_ref,
                                "error": e.to_string(),
                            })),
                            "config/set saved provider profile but live session refresh failed"
                        );
                        continue;
                    }
                }
            };
            if ctx
                .sessions
                .apply_model_provider(
                    &session_id,
                    model_provider,
                    model_provider_name,
                    model_name,
                    tool_dispatcher,
                )
                .await
                && let Some(agent) = ctx.sessions.get_agent(&session_id).await
            {
                let mut agent = agent.lock().await;
                agent.set_temperature(temperature);
            }
        }
    }

    pub(crate) fn handle_config_validate(&self) -> RpcResult {
        let config = self.ctx.config.read().clone();
        match config.validate() {
            Ok(()) => to_result(ConfigValidateResult {
                valid: true,
                error: None,
            }),
            Err(e) => to_result(ConfigValidateResult {
                valid: false,
                error: Some(e.to_string()),
            }),
        }
    }

    pub(crate) fn handle_config_reload(&self) -> RpcResult {
        if !self.schedule_daemon_reload("config") {
            return Err(rpc_err(INTERNAL_ERROR, "Reload not available"));
        }
        to_result(ConfigReloadResult { reloading: true })
    }
    pub(crate) fn handle_config_list(&self, params: &Value) -> RpcResult {
        use zeroclaw_config::field_visibility;
        use zeroclaw_config::traits::ConfigFieldEntry;
        let req: ConfigListParams = parse_params(params)?;
        let config = self.ctx.config.read().clone();
        let prefix = req.prefix.as_deref();
        let excluded = field_visibility::excluded_paths(&config, prefix.unwrap_or(""));
        let entries: Vec<ConfigFieldEntry> = config
            .prop_fields()
            .into_iter()
            .filter(|info| match prefix {
                Some(p) => field_visibility::path_matches_prefix(&info.name, p),
                None => true,
            })
            .filter(|info| !field_visibility::is_excluded(&info.name, &excluded))
            .map(|info| {
                let env = config.prop_is_env_overridden(&info.name);
                ConfigFieldEntry::from_prop_field(info, env)
            })
            .collect();
        to_result(ConfigListResult { entries })
    }

    pub(crate) async fn handle_config_delete(&self, params: &Value) -> RpcResult {
        let req: ConfigDeleteParams = parse_params(params)?;
        let refresh_model_provider_ref = model_provider_ref_from_provider_profile_prop(&req.prop);
        let config_write_guard = Arc::clone(&self.ctx.config_write_lock).lock_owned().await;
        {
            let mut config = self.ctx.config.write();
            config
                .set_prop_persistent(&req.prop, "")
                .map_err(|e| rpc_err(INTERNAL_ERROR, format!("Config delete failed: {e}")))?;
        }
        self.flush_config(&config_write_guard).await?;
        if let Some(model_provider_ref) = refresh_model_provider_ref {
            self.refresh_memory_embedder_for_model_provider(&model_provider_ref);
            self.schedule_live_sessions_refresh_for_model_provider(model_provider_ref);
        }
        to_result(ConfigDeleteResult {
            prop: req.prop,
            deleted: true,
        })
    }

    pub(crate) fn handle_config_resolve_alias_source(&self, params: &Value) -> RpcResult {
        let req: ConfigResolveAliasSourceParams = parse_params(params)?;
        let config = self.ctx.config.read().clone();
        let values = config.resolve_alias_source(req.source);
        to_result(ConfigResolveAliasSourceResult {
            source: req.source,
            values,
        })
    }

    pub(crate) fn handle_config_map_keys(&self, params: &Value) -> RpcResult {
        let req: ConfigMapKeysParams = parse_params(params)?;
        let config = self.ctx.config.read().clone();
        let keys = config.get_map_keys(&req.path).ok_or_else(|| {
            rpc_err(
                INVALID_PARAMS,
                format!("No map-keyed section at `{}`", req.path),
            )
        })?;
        to_result(ConfigMapKeysResult {
            path: req.path,
            keys,
        })
    }

    pub(crate) async fn handle_config_map_key_create(&self, params: &Value) -> RpcResult {
        let req: ConfigMapKeyCreateParams = parse_params(params)?;
        let config_write_guard = Arc::clone(&self.ctx.config_write_lock).lock_owned().await;
        let created = {
            let mut config = self.ctx.config.write();
            // Shared guarded boundary: enforces the reserved-agent rule (the
            // `default` runtime fallback) on this surface too, so the RPC create
            // path cannot author an `agents.default` the rename guard then traps.
            let created = zeroclaw_config::alias_refs::create_map_key_checked(
                &mut config,
                &req.path,
                &req.key,
            )
            .map_err(|e| rpc_err(INVALID_PARAMS, e.to_string()))?;
            if created {
                config.mark_dirty(&format!("{}.{}", req.path, req.key));
            }
            created
        };
        if created {
            self.flush_config(&config_write_guard).await?;
        }
        to_result(ConfigMapKeyCreateResult {
            path: req.path,
            key: req.key,
            created,
        })
    }

    pub(crate) async fn handle_config_map_key_delete(&self, params: &Value) -> RpcResult {
        let req: ConfigMapKeyDeleteParams = parse_params(params)?;
        let config_write_guard = Arc::clone(&self.ctx.config_write_lock).lock_owned().await;
        let deleted = {
            let mut config = self.ctx.config.write();
            let deleted = config
                .delete_map_key(&req.path, &req.key)
                .map_err(|e| rpc_err(INVALID_PARAMS, e))?;
            if deleted {
                config.mark_dirty(&format!("{}.{}", req.path, req.key));
            }
            deleted
        };
        if deleted {
            self.flush_config(&config_write_guard).await?;
        }
        to_result(ConfigMapKeyDeleteResult {
            path: req.path,
            key: req.key,
            deleted,
        })
    }

    pub(crate) fn handle_config_map_key_rename<'a>(
        &'a self,
        params: &'a Value,
    ) -> BoxRpcFuture<'a> {
        let req: ConfigMapKeyRenameParams = match parse_params(params) {
            Ok(req) => req,
            Err(err) => return Box::pin(std::future::ready(Err(err))),
        };

        Box::pin(async move {
            // Acquired once here, not inside `handle_config_alias_rename`:
            // the alias-kind branch below delegates into it, and the tokio
            // Mutex is not reentrant. The guard moves by value into the
            // alias-rename path so it can be released at that handler's
            // commit point, before its slow post-commit side effects.
            let config_write_guard = Arc::clone(&self.ctx.config_write_lock).lock_owned().await;
            if let Some(kind) = zeroclaw_config::alias_refs::alias_kind_for_map_path(&req.path) {
                return self
                    .handle_config_alias_rename(req, kind, config_write_guard)
                    .await;
            }

            let renamed = {
                let mut config = self.ctx.config.write();
                let renamed = config
                    .rename_map_key(&req.path, &req.from, &req.to)
                    .map_err(|e| rpc_err(INVALID_PARAMS, e))?;
                if renamed {
                    config.mark_dirty(&format!("{}.{}", req.path, req.from));
                    config.mark_dirty(&format!("{}.{}", req.path, req.to));
                }
                renamed
            };
            if renamed {
                self.flush_config(&config_write_guard).await?;
            }
            to_result(ConfigMapKeyRenameResult {
                path: req.path,
                from: req.from,
                to: req.to,
                renamed,
                warnings: Vec::new(),
            })
        })
    }

    pub(crate) fn handle_config_alias_rename<'a>(
        &'a self,
        req: ConfigMapKeyRenameParams,
        kind: zeroclaw_config::alias_refs::AliasKind,
        config_write_guard: ConfigWriteGuard,
    ) -> BoxRpcFuture<'a> {
        Box::pin(async move {
            let is_agent = matches!(kind, zeroclaw_config::alias_refs::AliasKind::Agent);
            if is_agent {
                // Live RPC sessions hold the selected agent alias in memory; refuse
                // rather than letting them recreate old-alias state after the rename.
                let active = self
                    .ctx
                    .sessions
                    .count_by_agent()
                    .await
                    .get(&req.from)
                    .copied()
                    .unwrap_or(0);
                if active > 0 {
                    return Err(rpc_err(
                        INVALID_PARAMS,
                        format!(
                            "{}.{}: cannot rename agent with {active} active RPC session(s); close those sessions first",
                            req.path, req.from
                        ),
                    ));
                }
            }

            let mut working = self.ctx.config.read().clone();
            let old_workspace = is_agent.then(|| working.agent_workspace_dir(&req.from));
            // If a prior call saved config as `to` but crashed before side effects,
            // re-running `from -> to` should converge lagging owned state instead
            // of failing because `from` is no longer a config key.
            let resume_committed_to = is_agent
                && working.agent(&req.from).is_none()
                && working.agent(&req.to).is_some()
                && self.agent_rename_residue_exists(&working, &req.from).await;

            if !resume_committed_to {
                let report = zeroclaw_config::alias_refs::rename_with_cascade(
                    &mut working,
                    &kind,
                    &req.from,
                    &req.to,
                )
                .map_err(|e| rename_error_to_rpc(&req.path, &req.from, e))?;
                for path in &report.dirty_paths {
                    working.mark_dirty(path);
                }
                self.save_and_swap_config(working.clone(), &config_write_guard)
                    .await?;
            }
            // Config is committed (saved + swapped, or already committed by a
            // prior crashed run). Release before the post-commit side effects
            // below: workspace moves and the memory/cron/ACP/session-backend
            // cascade can be slow or wedge, and holding the lock across them
            // would stall every config-mutating RPC daemon-wide.
            drop(config_write_guard);
            let new_workspace = is_agent.then(|| working.agent_workspace_dir(&req.to));

            let mut warnings = Vec::new();
            if let (Some(old_workspace), Some(new_workspace)) = (old_workspace, new_workspace) {
                warnings.extend(move_renamed_agent_workspace(&old_workspace, &new_workspace).await);
                warnings.extend(
                    self.rename_agent_owned_state(&working, &req.from, &req.to)
                        .await,
                );
            }

            to_result(ConfigMapKeyRenameResult {
                path: req.path,
                from: req.from,
                to: req.to,
                renamed: true,
                warnings,
            })
        })
    }

    pub(crate) async fn rename_agent_owned_state(
        &self,
        config: &zeroclaw_config::schema::Config,
        from: &str,
        to: &str,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut memory_rows = 0usize;
        let mut cron_jobs = 0usize;
        let mut acp_sessions = 0usize;
        let mut sessions_repointed = 0usize;

        if let Some(mem) = &self.ctx.memory {
            match mem.rename_agent(from, to).await {
                Ok(n) => memory_rows = n,
                Err(e) => warnings.push(format!("memory rename: {e}")),
            }
        }

        match crate::cron::rename_jobs_by_agent(config, from, to) {
            Ok(n) => cron_jobs = n,
            Err(e) => warnings.push(format!("cron rename: {e}")),
        }

        match &self.ctx.acp_session_store {
            Some(store) => match store.rename_sessions_by_agent(from, to) {
                Ok(n) => acp_sessions = n,
                Err(e) => warnings.push(format!("acp rename: {e}")),
            },
            None => warnings.push("acp store unavailable".to_string()),
        }

        if let Some(backend) = &self.ctx.session_backend {
            match backend.rename_agent_attribution(from, to) {
                Ok(n) => sessions_repointed = n,
                Err(e) => warnings.push(format!("session attribution rename: {e}")),
            }
        }

        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "from": from,
                    "to": to,
                    "memory": memory_rows,
                    "cron": cron_jobs,
                    "acp": acp_sessions,
                    "sessions": sessions_repointed,
                    "warnings": warnings.clone(),
                })
            ),
            "agent renamed with RPC owned-state cascade"
        );

        warnings
    }

    pub(crate) fn handle_config_templates(&self) -> RpcResult {
        use zeroclaw_config::schema::Config;
        let templates: Vec<ConfigTemplateEntry> = Config::map_key_sections()
            .into_iter()
            .map(Into::into)
            .collect();
        to_result(ConfigTemplatesResult { templates })
    }
    // ── Config introspection handlers ───────────────────────────

    pub(crate) fn handle_config_sections(&self) -> RpcResult {
        use zeroclaw_config::schema::Config;
        use zeroclaw_config::sections::{
            QUICKSTART_SECTIONS, Section, SectionShape, section_help, section_index_for_key,
        };

        let config = self.ctx.config.read().clone();

        // Schema-driven: walk Config::prop_fields() to discover ALL
        // top-level section roots, not just QUICKSTART_SECTIONS.
        let mut roots: std::collections::BTreeSet<String> = config
            .prop_fields()
            .iter()
            .filter_map(|f| f.name.split('.').next().map(str::to_string))
            .collect();

        // Hidden system fields the user never edits.
        const HIDDEN: &[&str] = &[
            "schema_version",
            "onboard_state",
            "onboard-state",
            "config_path",
            "workspace_dir",
            "env_overridden_paths",
            "pre_override_snapshots",
        ];
        for h in HIDDEN {
            roots.remove(*h);
        }

        // Map-keyed sections surface even when empty.
        let all_map_paths: Vec<&'static str> =
            Config::map_key_sections().iter().map(|s| s.path).collect();
        for &prefix in &all_map_paths
            .iter()
            .filter_map(|p| p.split('.').next())
            .collect::<std::collections::HashSet<_>>()
        {
            roots.insert(prefix.to_string());
        }

        // Inject synthetic onboarding sections (e.g. personality).
        for s in QUICKSTART_SECTIONS {
            roots.insert(s.as_str().to_string());
        }

        let direct_scalar_parents: std::collections::HashSet<String> = config
            .prop_fields()
            .iter()
            .filter_map(|f| {
                let mut segs = f.name.split('.');
                let root = segs.next()?;
                // exactly one more segment past root = direct child scalar
                segs.next()?;
                if segs.next().is_some() {
                    return None;
                }
                Some(root.to_string())
            })
            .collect();
        let parents_with_children: std::collections::HashSet<String> = roots
            .iter()
            .filter_map(|k| k.split_once('.').map(|(p, _)| p.to_string()))
            .collect();
        roots.retain(|k| {
            k.contains('.')
                || !parents_with_children.contains(k)
                || direct_scalar_parents.contains(k)
        });

        // Hide cost.rates subtree.
        roots.retain(|k| !k.starts_with("cost.rates"));

        // Sort: onboarding sections in canonical order first, rest alpha.
        let mut ordered: Vec<String> = roots.into_iter().collect();
        ordered.sort_by(
            |a, b| match (section_index_for_key(a), section_index_for_key(b)) {
                (Some(ai), Some(bi)) => ai.cmp(&bi),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.cmp(b),
            },
        );

        // Picker eligibility: map-keyed section or onboarding section
        // with a picker shape.
        let section_has_picker_for_key = |key: &str| -> bool {
            let key_dot = format!("{key}.");
            all_map_paths.iter().any(|p| {
                *p == key
                    || p.strip_prefix(&key_dot)
                        .is_some_and(|rest| !rest.contains('.'))
            })
        };

        let sections: Vec<ConfigSectionEntry> = ordered
            .into_iter()
            .map(|key| {
                let wizard = Section::from_key(&key);
                let has_picker = match wizard {
                    Some(w) => matches!(
                        w.shape(),
                        SectionShape::TypedFamilyMap | SectionShape::OneTierAliasMap
                    ),
                    None => section_has_picker_for_key(&key),
                };
                let completed = wizard
                    .map(|w| zeroclaw_config::sections::section_has_signal(&config, w))
                    .unwrap_or(false);
                let label = zeroclaw_config::sections::humanize_section_key(&key);
                ConfigSectionEntry {
                    help: section_help(&key).to_string(),
                    has_picker,
                    completed,
                    ready: false,
                    group: zeroclaw_config::sections::section_group_for_key(&key)
                        .label()
                        .to_string(),
                    is_quickstart: wizard.is_some(),
                    shape: wizard.map(Section::shape),
                    cost_category: zeroclaw_config::schema::cost_category_for_provider_section(
                        &key,
                    )
                    .unwrap_or_default()
                    .to_string(),
                    label,
                    key,
                }
            })
            .collect();
        to_result(ConfigSectionsResult { sections })
    }

    pub(crate) fn handle_config_status(&self) -> RpcResult {
        use zeroclaw_config::sections::QUICKSTART_SECTIONS;
        let config = self.ctx.config.read().clone();
        let missing: Vec<String> = QUICKSTART_SECTIONS
            .iter()
            .filter(|&&s| !zeroclaw_config::sections::section_has_signal(&config, s))
            .map(|s| s.as_str().to_string())
            .collect();
        let needs_quickstart = !missing.is_empty();
        let reason = if needs_quickstart {
            format!("{} section(s) incomplete", missing.len())
        } else {
            "all sections complete".to_string()
        };
        to_result(ConfigStatusResult {
            needs_quickstart,
            reason,
            has_partial_state: false,
            missing,
        })
    }

    pub(crate) fn handle_config_catalog(&self) -> RpcResult {
        let providers: Vec<CatalogModelProvider> = zeroclaw_providers::list_model_providers()
            .into_iter()
            .map(|p| CatalogModelProvider {
                name: p.name.to_string(),
                display_name: p.display_name.to_string(),
                local: p.local,
            })
            .collect();
        to_result(CatalogResponse {
            model_providers: providers,
        })
    }

    pub(crate) async fn handle_config_catalog_models(&self, params: &Value) -> RpcResult {
        let req: CatalogModelsParams = parse_params(params)?;
        let local = crate::quickstart::model_provider_is_local(&req.model_provider);
        // Snapshot config so the catalog can resolve the alias credential and
        // reach the native /models endpoint (surfacing new native-only models
        // that models.dev may not carry yet) rather than silently falling back.
        let config = self.ctx.config.read().clone();
        let (models, pricing, live) =
            crate::quickstart::model_catalog_with_config(Some(&config), &req.model_provider).await;
        to_result(CatalogModelsResult {
            model_provider: req.model_provider,
            models,
            pricing,
            local,
            live,
        })
    }
}
