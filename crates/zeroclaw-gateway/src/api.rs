//! REST API handlers for the web dashboard.
//! All `/api/*` routes require bearer token authentication (PairingGuard).

use super::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json},
};
use serde::{Deserialize, Serialize};
use zeroclaw_config::schema::{ChannelAliasInfo, Config};
use zeroclaw_memory::MemoryEntry;

const MEMORY_API_CONTENT_MAX_CHARS: usize = 4096;

fn integration_entry_json(
    entry: &zeroclaw_runtime::integrations::IntegrationEntry,
) -> serde_json::Value {
    serde_json::json!({
        "name": &entry.name,
        "description": &entry.description,
        "category": entry.category,
        "category_label": entry.category.label(),
        "status": entry.status,
    })
}

// ── Bearer token auth extractor ─────────────────────────────────

/// Extract and validate bearer token from Authorization header.
pub(crate) fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|auth| auth.strip_prefix("Bearer "))
}

/// Verify bearer token against PairingGuard. Returns error response if unauthorized.
pub(crate) fn require_auth(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if !state.pairing.require_pairing() {
        return Ok(());
    }

    let token = extract_bearer_token(headers).unwrap_or("");
    // Defense-in-depth: reject empty tokens explicitly so a future
    // refactor of is_authenticated cannot accidentally treat "" as valid.
    if token.is_empty() {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "Unauthorized — pair first via POST /pair, then send Authorization: Bearer <token>"
            })),
        ));
    }
    if state.pairing.is_authenticated(token) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "Unauthorized — pair first via POST /pair, then send Authorization: Bearer <token>"
            })),
        ))
    }
}

// ── Query parameters ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MemoryQuery {
    pub query: Option<String>,
    pub category: Option<String>,
    /// Filter memories created at or after (RFC 3339 / ISO 8601)
    pub since: Option<String>,
    /// Filter memories created at or before (RFC 3339 / ISO 8601)
    pub until: Option<String>,
    /// When set to a configured agent alias, the request goes through
    /// that agent's per-alias memory backend (so SQL backends filter by
    /// the agent's UUID, Markdown reads only that agent's directory,
    /// etc.). Omit for the install-wide view.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryStoreBody {
    pub key: String,
    pub content: String,
    pub category: Option<String>,
    /// Configured agent alias to write under. When omitted the store goes
    /// to the install-wide memory backend (no per-agent attribution).
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Deserialize)]
pub struct MemoryDeleteQuery {
    /// Configured agent alias to delete from. Omit for the install-wide
    /// backend.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Deserialize)]
pub struct CronRunsQuery {
    pub limit: Option<u32>,
}

#[derive(Deserialize)]
pub struct CronAddBody {
    /// Configured agent alias the cron job will run as. Required —
    /// there is no default agent.
    pub agent: String,
    pub name: Option<String>,
    pub schedule: String,
    pub tz: Option<String>,
    pub command: Option<String>,
    pub job_type: Option<String>,
    pub prompt: Option<String>,
    pub delivery: Option<zeroclaw_runtime::cron::DeliveryConfig>,
    pub session_target: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub delete_after_run: Option<bool>,
    /// If false, disable memory recall for this agent cron job (default: true).
    pub uses_memory: Option<bool>,
}

#[derive(Deserialize)]
pub struct CronPatchBody {
    /// Configured agent alias whose risk profile gates the new shell
    /// command. Only consulted when `command` is being patched; optional
    /// otherwise (e.g. a pure schedule/name change or an enable/disable
    /// toggle), so non-command patches need not supply it.
    #[serde(default)]
    pub agent: String,
    pub name: Option<String>,
    pub schedule: Option<String>,
    pub tz: Option<String>,
    pub clear_tz: Option<bool>,
    pub command: Option<String>,
    pub prompt: Option<String>,
    /// Toggle the job on/off without deleting it (pause/resume). `None` leaves
    /// the current state unchanged.
    pub enabled: Option<bool>,
    /// If false, disable memory recall for this agent cron job (default: true).
    pub uses_memory: Option<bool>,
}

enum CronTimezonePatch {
    Preserve,
    Set(String),
    Clear,
}

fn bad_request(message: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message.into() })),
    )
}

fn normalize_optional_timezone(
    tz: Option<String>,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    match tz {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                Err(bad_request(
                    "tz must be a non-empty IANA timezone; use clear_tz=true to clear it",
                ))
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        None => Ok(None),
    }
}

fn parse_timezone_patch(
    tz: Option<String>,
    clear_tz: Option<bool>,
) -> Result<CronTimezonePatch, (StatusCode, Json<serde_json::Value>)> {
    let tz = normalize_optional_timezone(tz)?;
    let clear_tz = clear_tz.unwrap_or(false);

    if clear_tz && tz.is_some() {
        return Err(bad_request("Provide either tz or clear_tz=true, not both"));
    }

    if clear_tz {
        Ok(CronTimezonePatch::Clear)
    } else if let Some(tz) = tz {
        Ok(CronTimezonePatch::Set(tz))
    } else {
        Ok(CronTimezonePatch::Preserve)
    }
}

fn cron_schedule_from_api(
    expr: String,
    tz: Option<String>,
) -> Result<zeroclaw_runtime::cron::Schedule, (StatusCode, Json<serde_json::Value>)> {
    let schedule = zeroclaw_runtime::cron::Schedule::Cron { expr, tz };
    zeroclaw_runtime::cron::validate_schedule(&schedule, chrono::Utc::now())
        .map_err(|e| bad_request(format!("Invalid cron schedule: {e}")))?;
    Ok(schedule)
}

#[derive(Deserialize)]
pub struct SessionMessagePostBody {
    pub content: String,
}

// ── Handlers ────────────────────────────────────────────────────

/// Query parameters for `GET /api/status`. Pass `?agent=<alias>` to
/// have `model_provider`, `model`, `temperature`, and `memory_backend`
/// reflect that specific agent's resolved config; omit it for the
/// install-wide summary.
#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    #[serde(default)]
    pub agent: Option<String>,
}

/// GET /api/status — system status overview
pub async fn handle_api_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<StatusQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    let health = zeroclaw_runtime::health::snapshot();

    // Per-alias map keyed by composite `<type>.<alias>`. Every
    // populated `[channels.<type>.<alias>]` is a separate dashboard row.
    let mut channels = serde_json::Map::new();
    for info in config.channels_by_alias() {
        let composite = format!("{}.{}", info.channel_type, info.alias);
        channels.insert(composite, serde_json::Value::Bool(true));
    }

    let locale = config
        .locale
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(zeroclaw_runtime::i18n::detect_locale);

    // Per-agent resolution when `?agent=<alias>` is supplied. Falls back
    // to the install-wide first-of-each view when the alias is unknown
    // (so the dashboard's old shape still renders during onboarding,
    // before any agent exists).
    let agent_alias = query.agent.as_deref().filter(|s| !s.trim().is_empty());
    let (model_provider, model, temperature, memory_backend) =
        match agent_alias.and_then(|alias| config.agent(alias).map(|a| (alias, a))) {
            Some((alias, agent)) => {
                let provider_ref = if agent.model_provider.is_empty() {
                    None
                } else {
                    Some(agent.model_provider.as_str().to_string())
                };
                let resolved = config.resolved_model_provider_for_agent(alias);
                let model = resolved
                    .as_ref()
                    .and_then(|(_, _, cfg)| cfg.model.clone())
                    .unwrap_or_default();
                let temperature: Option<f64> =
                    resolved.as_ref().and_then(|(_, _, cfg)| cfg.temperature);
                let backend_kind = agent.memory.backend;
                let backend = serde_json::to_value(backend_kind)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| format!("{backend_kind:?}").to_lowercase());
                (provider_ref, model, temperature, backend)
            }
            None => (
                config
                    .providers
                    .models
                    .iter_entries()
                    .next()
                    .map(|(ty, alias, _)| format!("{ty}.{alias}")),
                state.model.clone(),
                state.temperature,
                state.mem.name().to_string(),
            ),
        };

    let process = zeroclaw_runtime::process_stats::sample();

    // Upgrade affordance: whether the dashboard should poll for updates / offer
    // the upgrade button, and which restart command to show afterwards.
    let restart = crate::version::detect_restart();

    // Node discovery surface is feature-gated; keep the status payload shape
    // stable by emitting empty collections when the `nodes` feature is off.
    #[cfg(feature = "nodes")]
    let node_status = serde_json::json!({
        "connected": state.node_registry.node_ids(),
        "mdns_peers": state.mdns_peer_registry.snapshots(),
    });
    #[cfg(not(feature = "nodes"))]
    let node_status = serde_json::json!({
        "connected": Vec::<String>::new(),
        "mdns_peers": Vec::<serde_json::Value>::new(),
    });

    let body = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "model_provider": model_provider,
        "model": model,
        "temperature": temperature,
        "uptime_seconds": health.uptime_seconds,
        "daemon_started_at": zeroclaw_runtime::health::daemon_started_at(),
        "gateway_port": config.gateway.port,
        "locale": locale,
        "memory_backend": memory_backend,
        "paired": state.pairing.is_paired(),
        "channels": channels,
        "nodes": node_status,
        "health": health,
        "agent_alias": agent_alias,
        "process": process,
        "check_updates": config.gateway.check_updates,
        "allow_self_upgrade": config.gateway.allow_self_upgrade,
        "restart_mode": restart.mode.as_str(),
        "restart_hint": restart.hint,
    });

    Json(body).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ToolsQuery {
    #[serde(default)]
    pub agent: Option<String>,
}

/// GET /api/tools - list registered tool specs, optionally scoped to `?agent=`
pub async fn handle_api_tools(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ToolsQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let registry = query
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
        .and_then(|alias| state.tools_registry_by_agent.get(alias).cloned())
        .unwrap_or_else(|| std::sync::Arc::clone(&state.tools_registry));

    let tools: Vec<serde_json::Value> = registry
        .iter()
        .map(|spec| {
            let mut tool = serde_json::json!({
                "name": spec.name,
                "description": spec.description,
                "parameters": spec.parameters,
            });
            if let Some(output) = &spec.output {
                tool["output"] = output.clone();
            }
            if !spec.param_domains.is_empty() {
                tool["param_domains"] = serde_json::json!(spec.param_domains);
            }
            tool
        })
        .collect();

    Json(serde_json::json!({"tools": tools})).into_response()
}

/// GET /api/cron — list cron jobs
pub async fn handle_api_cron_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    match zeroclaw_runtime::cron::list_jobs(&config) {
        Ok(jobs) => Json(serde_json::json!({"jobs": jobs})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to list cron jobs: {e}")})),
        )
            .into_response(),
    }
}

/// POST /api/cron — add a new cron job
pub async fn handle_api_cron_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CronAddBody>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let CronAddBody {
        agent: agent_alias,
        name,
        schedule,
        tz,
        command,
        job_type,
        prompt,
        delivery,
        session_target,
        model,
        allowed_tools,
        delete_after_run,
        uses_memory,
    } = body;

    let config = state.config.read().clone();
    if config.agent(&agent_alias).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!(
                "Unknown agent {agent_alias:?} (no [agents.{agent_alias}] entry configured)"
            )})),
        )
            .into_response();
    }
    let tz = match normalize_optional_timezone(tz) {
        Ok(tz) => tz,
        Err(e) => return e.into_response(),
    };
    let schedule = match cron_schedule_from_api(schedule, tz) {
        Ok(schedule) => schedule,
        Err(e) => return e.into_response(),
    };
    if let Err(e) = zeroclaw_runtime::cron::validate_delivery_config(delivery.as_ref()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("Failed to add cron job: {e}")})),
        )
            .into_response();
    }

    // Determine job type: explicit field, or infer "agent" when prompt is provided.
    let is_agent =
        matches!(job_type.as_deref(), Some("agent")) || (job_type.is_none() && prompt.is_some());

    let result = if is_agent {
        let prompt = match prompt.as_deref() {
            Some(p) if !p.trim().is_empty() => p,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "Missing 'prompt' for agent job"})),
                )
                    .into_response();
            }
        };

        let session_target = session_target
            .as_deref()
            .map(zeroclaw_runtime::cron::SessionTarget::parse)
            .unwrap_or_default();

        let default_delete = matches!(schedule, zeroclaw_runtime::cron::Schedule::At { .. });
        let delete_after_run = delete_after_run.unwrap_or(default_delete);

        zeroclaw_runtime::cron::add_agent_job(
            &config,
            &agent_alias,
            name,
            schedule,
            prompt,
            session_target,
            model,
            delivery,
            delete_after_run,
            allowed_tools,
            uses_memory.unwrap_or(true),
        )
    } else {
        let command = match command.as_deref() {
            Some(c) if !c.trim().is_empty() => c,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "Missing 'command' for shell job"})),
                )
                    .into_response();
            }
        };

        zeroclaw_runtime::cron::add_shell_job_with_approval(
            &config,
            &agent_alias,
            name,
            schedule,
            command,
            delivery,
            false,
        )
    };

    match result {
        Ok(job) => Json(serde_json::json!({"status": "ok", "job": job})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to add cron job: {e}")})),
        )
            .into_response(),
    }
}

/// GET /api/cron/:id/runs — list recent runs for a cron job
pub async fn handle_api_cron_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<CronRunsQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let limit = params.limit.unwrap_or(20).clamp(1, 100) as usize;
    let config = state.config.read().clone();

    // Verify the job exists before listing runs.
    if let Err(e) = zeroclaw_runtime::cron::get_job(&config, &id) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Cron job not found: {e}")})),
        )
            .into_response();
    }

    match zeroclaw_runtime::cron::list_runs(&config, &id, limit) {
        Ok(runs) => {
            let runs_json: Vec<serde_json::Value> = runs
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id,
                        "job_id": r.job_id,
                        "started_at": r.started_at.to_rfc3339(),
                        "finished_at": r.finished_at.to_rfc3339(),
                        "status": r.status,
                        "output": r.output,
                        "duration_ms": r.duration_ms,
                    })
                })
                .collect();
            Json(serde_json::json!({"runs": runs_json})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to list cron runs: {e}")})),
        )
            .into_response(),
    }
}

/// POST /api/cron/:id/run — trigger a cron job manually
pub async fn handle_api_cron_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();

    let job = match zeroclaw_runtime::cron::get_job(&config, &id) {
        Ok(job) => job,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Cron job not found: {e}")})),
            )
                .into_response();
        }
    };

    let event_tx = Some(state.event_tx.clone());
    let result = zeroclaw_runtime::cron::scheduler::run_manual_job(
        &config,
        &job,
        zeroclaw_runtime::cron::scheduler::CronDeliveryContext::GatewayManual,
        &event_tx,
    )
    .await;

    Json(serde_json::json!({
        "status": result.status,
        "job_id": result.job_id,
        "success": result.success,
        "output": result.output,
        "duration_ms": result.duration_ms,
        "started_at": result.started_at.to_rfc3339(),
        "finished_at": result.finished_at.to_rfc3339(),
    }))
    .into_response()
}

/// PATCH /api/cron/:id — update an existing cron job
pub async fn handle_api_cron_patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CronPatchBody>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    let agent_alias = body.agent.clone();
    let CronPatchBody {
        agent: _,
        name,
        schedule: schedule_expr,
        tz,
        clear_tz,
        command,
        prompt,
        enabled,
        uses_memory,
    } = body;
    let timezone_patch = match parse_timezone_patch(tz, clear_tz) {
        Ok(patch) => patch,
        Err(e) => return e.into_response(),
    };

    let existing = match zeroclaw_runtime::cron::get_job(&config, &id) {
        Ok(j) => j,
        Err(e) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Cron job not found: {e}")})),
            )
                .into_response();
        }
    };
    let is_agent = matches!(existing.job_type, zeroclaw_runtime::cron::JobType::Agent);
    let setting_shell_command = !is_agent && (command.is_some() || prompt.is_some());
    if setting_shell_command && config.agent(&agent_alias).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!(
                "Unknown agent {a:?} (no [agents.{a}] entry configured)",
                a = agent_alias
            )})),
        )
            .into_response();
    }
    let new_expr = schedule_expr
        .as_deref()
        .map(str::trim)
        .filter(|expr| !expr.is_empty())
        .map(str::to_string);
    let timezone_changed = !matches!(timezone_patch, CronTimezonePatch::Preserve);
    let schedule = if new_expr.is_some() || timezone_changed {
        let (expr, existing_tz) = match (&existing.schedule, new_expr) {
            (_, Some(expr)) => {
                let existing_tz = match &existing.schedule {
                    zeroclaw_runtime::cron::Schedule::Cron { tz, .. } => tz.clone(),
                    _ => None,
                };
                (expr, existing_tz)
            }
            (zeroclaw_runtime::cron::Schedule::Cron { expr, tz }, None) => {
                (expr.clone(), tz.clone())
            }
            (_, None) => {
                return bad_request("tz can only be updated on cron schedules").into_response();
            }
        };
        let tz = match timezone_patch {
            CronTimezonePatch::Preserve => existing_tz,
            CronTimezonePatch::Set(tz) => Some(tz),
            CronTimezonePatch::Clear => None,
        };
        match cron_schedule_from_api(expr, tz) {
            Ok(schedule) => Some(schedule),
            Err(e) => return e.into_response(),
        }
    } else {
        None
    };
    let (patch_command, patch_prompt) = if is_agent {
        (None, command.or(prompt))
    } else {
        (command.or(prompt), None)
    };

    let patch = zeroclaw_runtime::cron::CronJobPatch {
        name,
        schedule,
        command: patch_command,
        prompt: patch_prompt,
        enabled,
        uses_memory,
        ..zeroclaw_runtime::cron::CronJobPatch::default()
    };

    match zeroclaw_runtime::cron::update_shell_job_with_approval(
        &config,
        &agent_alias,
        &id,
        patch,
        false,
    ) {
        Ok(job) => Json(serde_json::json!({"status": "ok", "job": job})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to update cron job: {e}")})),
        )
            .into_response(),
    }
}

/// DELETE /api/cron/:id — remove a cron job
pub async fn handle_api_cron_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    match zeroclaw_runtime::cron::remove_job(&config, &id) {
        Ok(()) => Json(serde_json::json!({"status": "ok"})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to remove cron job: {e}")})),
        )
            .into_response(),
    }
}

/// GET /api/cron/settings — return cron subsystem settings
pub async fn handle_api_cron_settings_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    Json(serde_json::json!({
        "enabled": config.scheduler.enabled,
        "catch_up_on_startup": config.scheduler.catch_up_on_startup,
        "max_run_history": config.scheduler.max_run_history,
    }))
    .into_response()
}

/// PATCH /api/cron/settings — update cron subsystem settings
pub async fn handle_api_cron_settings_patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    // Held through the swap below so a concurrent config writer can't land
    // between this read and the save.
    let _cfg_guard = std::sync::Arc::clone(&state.config_write_lock)
        .lock_owned()
        .await;
    let mut config = state.config.read().clone();

    if let Some(v) = body.get("enabled").and_then(|v| v.as_bool()) {
        config.scheduler.enabled = v;
        config.mark_dirty("scheduler.enabled");
    }
    if let Some(v) = body.get("catch_up_on_startup").and_then(|v| v.as_bool()) {
        config.scheduler.catch_up_on_startup = v;
        config.mark_dirty("scheduler.catch-up-on-startup");
    }
    if let Some(v) = body.get("max_run_history").and_then(|v| v.as_u64()) {
        config.scheduler.max_run_history = u32::try_from(v).unwrap_or(u32::MAX);
        config.mark_dirty("scheduler.max-run-history");
    }

    if let Err(e) = config.save_dirty().await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to save config: {e}")})),
        )
            .into_response();
    }

    *state.config.write() = config.clone();

    Json(serde_json::json!({
        "status": "ok",
        "enabled": config.scheduler.enabled,
        "catch_up_on_startup": config.scheduler.catch_up_on_startup,
        "max_run_history": config.scheduler.max_run_history,
    }))
    .into_response()
}

/// GET /api/integrations — list all integrations with status
pub async fn handle_api_integrations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    let entries = zeroclaw_runtime::integrations::registry::all_integrations(&config);

    let integrations: Vec<serde_json::Value> = entries.iter().map(integration_entry_json).collect();

    Json(serde_json::json!({"integrations": integrations})).into_response()
}

/// GET /api/integrations/settings — return per-integration settings (enabled + category)
pub async fn handle_api_integrations_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    let entries = zeroclaw_runtime::integrations::registry::all_integrations(&config);

    let mut settings = serde_json::Map::new();
    for entry in &entries {
        let enabled = matches!(
            entry.status,
            zeroclaw_runtime::integrations::IntegrationStatus::Active
        );
        settings.insert(
            entry.name.clone(),
            serde_json::json!({
                "enabled": enabled,
                "category": entry.category,
                "status": entry.status,
            }),
        );
    }

    Json(serde_json::json!({"settings": settings})).into_response()
}

/// POST /api/doctor — run diagnostics
pub async fn handle_api_doctor(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    let results = zeroclaw_runtime::doctor::diagnose(&config);

    let ok_count = results
        .iter()
        .filter(|r| r.severity == zeroclaw_runtime::doctor::Severity::Ok)
        .count();
    let warn_count = results
        .iter()
        .filter(|r| r.severity == zeroclaw_runtime::doctor::Severity::Warn)
        .count();
    let error_count = results
        .iter()
        .filter(|r| r.severity == zeroclaw_runtime::doctor::Severity::Error)
        .count();

    Json(serde_json::json!({
        "results": results,
        "summary": {
            "ok": ok_count,
            "warnings": warn_count,
            "errors": error_count,
        }
    }))
    .into_response()
}

async fn resolve_memory_handle(
    state: &AppState,
    agent_alias: Option<&str>,
) -> Result<std::sync::Arc<dyn zeroclaw_memory::Memory>, (StatusCode, Json<serde_json::Value>)> {
    let alias = match agent_alias.map(str::trim).filter(|s| !s.is_empty()) {
        Some(a) => a,
        None => return Ok(state.mem.clone()),
    };
    let config = state.config.read().clone();
    if config.agent(alias).is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!(
                "Unknown agent {alias:?} (no [agents.{alias}] entry configured)"
            )})),
        ));
    }
    let api_key = config
        .resolved_model_provider_for_agent(alias)
        .and_then(|(_, _, cfg)| cfg.api_key.clone());
    zeroclaw_memory::create_memory_for_agent(&config, alias, api_key.as_deref())
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({"error": format!("Failed to build per-agent memory: {e:#}")}),
                ),
            )
        })
}

/// GET /api/memory — list or search memory entries
pub async fn handle_api_memory_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<MemoryQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let mem = match resolve_memory_handle(&state, params.agent.as_deref()).await {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };

    // Use recall when query or time range is provided
    if params.query.is_some() || params.since.is_some() || params.until.is_some() {
        let query = params.query.as_deref().unwrap_or("");
        let since = params.since.as_deref();
        let until = params.until.as_deref();
        match mem.recall(query, 50, None, since, until).await {
            Ok(entries) => {
                let entries = match params.category.as_deref() {
                    Some(cat) => entries
                        .into_iter()
                        .filter(|e| e.category.to_string() == cat)
                        .collect(),
                    None => entries,
                };
                Json(serde_json::json!({
                    "entries": sanitize_memory_entries_for_api(entries)
                }))
                .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Memory recall failed: {e}")})),
            )
                .into_response(),
        }
    } else {
        // List mode
        let category = params.category.as_deref().map(|cat| match cat {
            "core" => zeroclaw_memory::MemoryCategory::Core,
            "daily" => zeroclaw_memory::MemoryCategory::Daily,
            "conversation" => zeroclaw_memory::MemoryCategory::Conversation,
            other => zeroclaw_memory::MemoryCategory::Custom(other.to_string()),
        });

        match mem.list(category.as_ref(), None).await {
            Ok(entries) => Json(serde_json::json!({
                "entries": sanitize_memory_entries_for_api(entries)
            }))
            .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Memory list failed: {e}")})),
            )
                .into_response(),
        }
    }
}

fn sanitize_memory_entries_for_api(entries: Vec<MemoryEntry>) -> Vec<MemoryEntry> {
    entries
        .into_iter()
        .map(|mut entry| {
            entry.content = truncate_with_ellipsis_total_chars(entry.content);
            entry
        })
        .collect()
}

fn truncate_with_ellipsis_total_chars(mut s: String) -> String {
    if s.char_indices().nth(MEMORY_API_CONTENT_MAX_CHARS).is_none() {
        return s;
    }

    let keep_chars = MEMORY_API_CONTENT_MAX_CHARS - 3;
    let cut_idx = s
        .char_indices()
        .nth(keep_chars)
        .map(|(idx, _)| idx)
        .unwrap_or(s.len());
    s.truncate(cut_idx);
    s.push_str("...");
    s
}

/// POST /api/memory — store a memory entry
pub async fn handle_api_memory_store(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<MemoryStoreBody>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let category = body
        .category
        .as_deref()
        .map(|cat| match cat {
            "core" => zeroclaw_memory::MemoryCategory::Core,
            "daily" => zeroclaw_memory::MemoryCategory::Daily,
            "conversation" => zeroclaw_memory::MemoryCategory::Conversation,
            other => zeroclaw_memory::MemoryCategory::Custom(other.to_string()),
        })
        .unwrap_or(zeroclaw_memory::MemoryCategory::Core);

    let mem = match resolve_memory_handle(&state, body.agent.as_deref()).await {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };

    match mem.store(&body.key, &body.content, category, None).await {
        Ok(()) => Json(serde_json::json!({"status": "ok"})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Memory store failed: {e}")})),
        )
            .into_response(),
    }
}

/// DELETE /api/memory/:key — delete a memory entry
pub async fn handle_api_memory_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    Query(query): Query<MemoryDeleteQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let mem = match resolve_memory_handle(&state, query.agent.as_deref()).await {
        Ok(m) => m,
        Err(e) => return e.into_response(),
    };

    match mem.forget(&key).await {
        Ok(deleted) => {
            Json(serde_json::json!({"status": "ok", "deleted": deleted})).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Memory forget failed: {e}")})),
        )
            .into_response(),
    }
}

/// Query parameters for `GET /api/cost`. When `agent` is set, the
/// returned summary filters to records attributed to that alias.
#[derive(Debug, Deserialize)]
pub struct CostQuery {
    #[serde(default)]
    pub agent: Option<String>,
    /// RFC3339 UTC instants — caller-computed window bounds. The
    /// dashboard derives them in the operator's local timezone so
    /// "today" means the operator's today, not the daemon's UTC today.
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
}

/// GET /api/cost — cost summary over `[from, to)` (either bound omitted
/// = unbounded on that side). Pass `?agent=<alias>` for the per-agent
/// view, which ignores from/to and returns the alias's session+daily
/// rollup.
pub async fn handle_api_cost(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CostQuery>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let parse_bound = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc))
    };
    let from = query.from.as_deref().and_then(parse_bound);
    let to = query.to.as_deref().and_then(parse_bound);

    if let Some(ref tracker) = state.cost_tracker {
        let result = match query.agent.as_deref().filter(|s| !s.is_empty()) {
            Some(alias) => tracker.get_summary_for_agent(alias),
            None => tracker.get_summary_in_bounds(from, to),
        };
        match result {
            Ok(summary) => Json(serde_json::json!({"cost": summary})).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Cost summary failed: {e}")})),
            )
                .into_response(),
        }
    } else {
        Json(serde_json::json!({
            "cost": {
                "session_cost_usd": 0.0,
                "daily_cost_usd": 0.0,
                "monthly_cost_usd": 0.0,
                "total_tokens": 0,
                "request_count": 0,
                "by_model": {},
                "by_agent": {},
            }
        }))
        .into_response()
    }
}

/// GET /api/channels — list configured channels with status
pub async fn handle_api_channels(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    let health = zeroclaw_runtime::health::snapshot();
    // One entry per `[channels.<type>.<alias>]` block. Owning
    // agent comes from the agents.<alias>.channels reverse lookup.
    let channels: Vec<serde_json::Value> = config
        .channels_by_alias()
        .into_iter()
        .map(|info| {
            let composite = format!("{}.{}", info.channel_type, info.alias);
            let compiled_key = compiled_readiness_key_for_alias(&config, &info);
            let compiled = zeroclaw_channels::listing::is_channel_type_compiled(compiled_key);
            let readiness = channel_readiness(&config, &info, &health, &state);
            let (status, health_status) = if compiled {
                channel_readiness_summary(&readiness)
            } else {
                ("not_compiled", "unavailable")
            };
            serde_json::json!({
                "name": composite,
                "type": info.channel_type,
                "alias": info.alias,
                "owning_agent": info.owning_agent,
                "enabled": info.enabled,
                "compiled": compiled,
                "status": status,
                "message_count": 0,
                "last_message_at": null,
                "health": health_status,
                "readiness": readiness,
            })
        })
        .collect();

    Json(serde_json::json!({ "channels": channels })).into_response()
}

/// POST /api/channels/{channel}/relink — replace a QR channel's pairing.
///
/// `{channel}` is the composite `<type>.<alias>` name returned by
/// `GET /api/channels`. Dispatches to the channel-owned relink hook
/// ([`zeroclaw_channels::login_relink::relink`]); the gateway performs no
/// file operations of its own and holds no knowledge of channel session
/// layouts.
///
/// Responses (all authenticated via the standard bearer guard):
///
/// - `200` with `"outcome": "cleared"` — persisted login removed
///   (`"removed"` lists the paths). `"restart_required": true`: the running
///   channel keeps its in-memory session until the daemon restarts it, so
///   the caller follows up with `POST /admin/reload` (which enforces its
///   own, stricter admin policy — relink deliberately does not bypass it).
/// - `200` with `"outcome": "nothing_to_clear"` — the channel supports
///   relinking but held no persisted login; the next start already mints a
///   fresh QR.
/// - `409` with `"outcome": "unsupported"` — the channel type has no relink
///   hook (it does not use QR-pairing sessions) or its feature is not
///   compiled into this binary. **Explicit no-op: nothing was touched.**
/// - `404` — no `[channels.<type>.<alias>]` block matches `{channel}`.
pub async fn handle_api_channel_relink(
    State(state): State<AppState>,
    Path(channel): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let config = state.config.read().clone();
    let Some(info) = config
        .channels_by_alias()
        .into_iter()
        .find(|info| format!("{}.{}", info.channel_type, info.alias) == channel)
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("unknown channel {channel} — use the composite name from GET /api/channels"),
            })),
        )
            .into_response();
    };

    // Resolve the string key to the typed QR-pairing channel once; probe
    // and relink dispatch on the same enum. `None` means the channel type
    // has no relink hook or its feature is not compiled — an explicit
    // no-op conflict where nothing is touched.
    let compiled_key = compiled_readiness_key_for_alias(&config, &info);
    let Some(qr_channel) = zeroclaw_channels::listing::qr_pairing_channel(compiled_key) else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "channel": channel,
                "outcome": "unsupported",
                "error": format!(
                    "channel type {} has no relink operation (it does not use QR-pairing sessions) \
                     or the feature is not compiled into this binary; nothing was changed",
                    info.channel_type
                ),
            })),
        )
            .into_response();
    };

    match zeroclaw_channels::login_relink::relink(qr_channel, &config, &info.alias) {
        Ok(zeroclaw_channels::login_relink::RelinkOutcome::Cleared { removed }) => {
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"channel": channel, "removed": removed})),
                "channel persisted login cleared for relink"
            );
            Json(serde_json::json!({
                "channel": channel,
                "outcome": "cleared",
                "removed": removed,
                "restart_required": true,
                "note": "restart the channel (POST /admin/reload) to begin the fresh QR pairing",
            }))
            .into_response()
        }
        Ok(zeroclaw_channels::login_relink::RelinkOutcome::NothingToClear) => {
            Json(serde_json::json!({
                "channel": channel,
                "outcome": "nothing_to_clear",
                "removed": [],
                "restart_required": false,
                "note": "no persisted login was stored; the next channel start already begins a fresh QR pairing",
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "channel": channel,
                "error": format!("failed to clear persisted login: {e}"),
            })),
        )
            .into_response(),
    }
}

/// GET /api/tuis — list connected TUI sessions
pub async fn handle_api_tuis(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let tuis: Vec<serde_json::Value> = state
        .tui_registry
        .as_ref()
        .map(|r| {
            r.list()
                .into_iter()
                .map(|e| {
                    serde_json::json!({
                        "tui_id": e.tui_id,
                        "connected_at": e.connected_at.to_rfc3339(),
                        "peer_label": e.peer_label,
                        "transport": e.transport,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Json(serde_json::json!({ "tuis": tuis })).into_response()
}

fn compiled_readiness_key_for_alias<'a>(config: &'a Config, info: &'a ChannelAliasInfo) -> &'a str {
    if info.channel_type == "whatsapp"
        && config
            .channels
            .whatsapp
            .get(&info.alias)
            .is_some_and(|whatsapp| whatsapp.backend_type() == "web")
    {
        "whatsapp-web"
    } else {
        info.channel_type.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ChannelReadinessState {
    Ready,
    Missing,
    Unknown,
}

const CHANNEL_LISTENER_HEALTH_MAX_AGE_SECS: i64 = 30;

#[derive(Debug, Clone, Serialize)]
struct ChannelReadiness {
    enabled: ChannelReadinessState,
    bound_to_agent: ChannelReadinessState,
    authenticated: ChannelReadinessState,
    listening: ChannelReadinessState,
    requirements: Vec<String>,
    notes: Vec<String>,
}

fn channel_readiness(
    config: &zeroclaw_config::schema::Config,
    info: &zeroclaw_config::schema::ChannelAliasInfo,
    health: &zeroclaw_runtime::health::HealthSnapshot,
    state: &AppState,
) -> ChannelReadiness {
    let mut readiness = ChannelReadiness {
        enabled: if info.enabled {
            ChannelReadinessState::Ready
        } else {
            ChannelReadinessState::Missing
        },
        bound_to_agent: if info.owning_agent.is_some() {
            ChannelReadinessState::Ready
        } else {
            ChannelReadinessState::Missing
        },
        authenticated: ChannelReadinessState::Unknown,
        listening: ChannelReadinessState::Unknown,
        requirements: Vec::new(),
        notes: Vec::new(),
    };

    if readiness.enabled == ChannelReadinessState::Missing {
        readiness
            .requirements
            .push("Enable this channel alias.".to_string());
    }
    if readiness.bound_to_agent == ChannelReadinessState::Missing {
        readiness
            .requirements
            .push("Bind this channel to an enabled agent.".to_string());
    }

    if readiness.enabled == ChannelReadinessState::Ready
        && readiness.bound_to_agent == ChannelReadinessState::Ready
    {
        if info.channel_type == "webhook" {
            apply_webhook_readiness(config, &info.alias, health, state, &mut readiness);
        } else {
            apply_persisted_login_readiness(config, info, &mut readiness);
        }
    }

    readiness
}

/// Fill `readiness.authenticated` from the channel-owned persisted-login
/// probe (`zeroclaw_channels::login_probe`). The probe resolves the same
/// on-disk session signal each QR-pairing channel uses at startup to decide
/// between resuming a session and minting a fresh QR code; nothing is
/// cached and nothing is written. Channel types without a typed QR-pairing
/// key (no probe, or feature not compiled) keep `authenticated: unknown`
/// and the existing "not checked yet" note.
fn apply_persisted_login_readiness(
    config: &zeroclaw_config::schema::Config,
    info: &zeroclaw_config::schema::ChannelAliasInfo,
    readiness: &mut ChannelReadiness,
) {
    use zeroclaw_channels::login_probe::PersistedLogin;

    // Resolve the string key to the typed QR-pairing channel once; all
    // downstream dispatch is on the enum.
    let compiled_key = compiled_readiness_key_for_alias(config, info);
    let Some(channel) = zeroclaw_channels::listing::qr_pairing_channel(compiled_key) else {
        readiness.notes.push(format!(
            "Live readiness is not checked for `{}` channels yet.",
            info.channel_type
        ));
        return;
    };

    match zeroclaw_channels::login_probe::persisted_login(channel, config, &info.alias) {
        PersistedLogin::Present => {
            readiness.authenticated = ChannelReadinessState::Ready;
            readiness.notes.push(format!(
                "Live listener readiness is not checked for `{}` channels yet.",
                info.channel_type
            ));
        }
        PersistedLogin::Absent => {
            readiness.authenticated = ChannelReadinessState::Missing;
            readiness.requirements.push(
                "Pair this channel: no persisted login session was found on disk.".to_string(),
            );
        }
    }
}

fn channel_readiness_summary(readiness: &ChannelReadiness) -> (&'static str, &'static str) {
    if readiness.enabled == ChannelReadinessState::Missing
        || readiness.bound_to_agent == ChannelReadinessState::Missing
    {
        return ("inactive", "degraded");
    }

    if readiness.authenticated == ChannelReadinessState::Missing
        || readiness.listening == ChannelReadinessState::Missing
    {
        return ("error", "down");
    }

    if readiness.authenticated == ChannelReadinessState::Ready
        && readiness.listening == ChannelReadinessState::Ready
    {
        ("active", "healthy")
    } else {
        // At least one probe is Unknown and none reported Missing: not
        // enough signal to call the channel either healthy or down.
        ("unknown", "degraded")
    }
}

fn apply_webhook_readiness(
    config: &zeroclaw_config::schema::Config,
    alias: &str,
    health: &zeroclaw_runtime::health::HealthSnapshot,
    state: &AppState,
    readiness: &mut ChannelReadiness,
) {
    let Some(webhook) = config.channels.webhook.get(alias) else {
        readiness.authenticated = ChannelReadinessState::Missing;
        readiness.listening = ChannelReadinessState::Missing;
        readiness
            .requirements
            .push("Webhook config block is missing.".to_string());
        return;
    };

    if state.pairing.require_pairing() && !state.pairing.is_paired() {
        readiness.authenticated = ChannelReadinessState::Missing;
        readiness
            .requirements
            .push("Pair the gateway before using the webhook endpoint.".to_string());
    } else {
        readiness.authenticated = ChannelReadinessState::Ready;
    }

    let component = format!("channel:webhook.{alias}");
    let component_health = health.components.get(&component);
    let component_status = component_health.map(|component| component.status.as_str());
    let supervised_listener_ok = component_health.is_some_and(component_health_ok_and_fresh);
    let listen_path = normalized_webhook_path(webhook.listen_path.as_deref());

    if supervised_listener_ok {
        readiness.listening = ChannelReadinessState::Ready;
    } else if component_status == Some("error") {
        readiness.listening = ChannelReadinessState::Missing;
        readiness.requirements.push(format!(
            "Resolve the listener error for `webhook.{alias}` before using this channel."
        ));
    } else {
        readiness.listening = ChannelReadinessState::Missing;
        readiness.requirements.push(format!(
            "Start a channel listener for `webhook.{alias}` on port {}{}.",
            webhook.port, listen_path
        ));
    }
}

fn component_health_ok_and_fresh(component: &zeroclaw_runtime::health::ComponentHealth) -> bool {
    if component.status != "ok" {
        return false;
    }

    let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(&component.updated_at) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(updated_at.with_timezone(&chrono::Utc));
    age >= chrono::Duration::zero()
        && age <= chrono::Duration::seconds(CHANNEL_LISTENER_HEALTH_MAX_AGE_SECS)
}

fn normalized_webhook_path(path: Option<&str>) -> String {
    let trimmed = path.unwrap_or("/webhook").trim();
    if trimmed.is_empty() {
        "/webhook".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn companion_outbox_from_state(state: &AppState) -> zeroclaw_api::companion::CompanionOutboxHealth {
    match state.companion_store.as_deref() {
        None => zeroclaw_api::companion::CompanionOutboxHealth::not_configured(),
        // Read-only SELECT. Aging WARN belongs to the 5-minute observe tick.
        Some(store) => store.outbox_health(),
    }
}

/// GET /api/health — component health snapshot
pub async fn handle_api_health(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let snapshot = zeroclaw_runtime::health::snapshot();
    let companion_outbox = companion_outbox_from_state(&state);
    Json(serde_json::json!({
        "health": snapshot,
        "companion_outbox": companion_outbox,
    }))
    .into_response()
}

// ── Helpers ─────────────────────────────────────────────────────

// ── Session API handlers ─────────────────────────────────────────

/// GET /api/sessions — list gateway sessions
pub async fn handle_api_sessions_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let Some(ref backend) = state.session_backend else {
        return Json(serde_json::json!({
            "sessions": [],
            "message": "Session persistence is disabled"
        }))
        .into_response();
    };

    // Include every session that's attributable (agent_alias stamped,
    // or a channel_id that resolves to an owning agent).
    // Pre-migration rows with neither set are skipped as orphans.
    let config = state.config.read().clone();
    let all_metadata = backend.list_sessions_with_metadata();
    let sessions: Vec<serde_json::Value> = all_metadata
        .into_iter()
        .filter(|meta| meta.agent_alias.is_some() || meta.channel_id.is_some())
        .map(|meta| {
            // Resolve owning agent: prefer the stamped alias, otherwise
            // reverse-look-up via channel_id (= `<type>.<alias>`) against
            // each agent's `channels` list.
            let agent_alias = meta.agent_alias.clone().or_else(|| {
                meta.channel_id
                    .as_deref()
                    .and_then(|c| config.agent_for_channel(c))
                    .map(str::to_string)
            });
            // Drop the gw_ prefix for display; channel keys stay as-is so
            // the frontend can show the channel context inline.
            let session_id = meta
                .key
                .strip_prefix("gw_")
                .map(str::to_string)
                .unwrap_or_else(|| meta.key.clone());
            let mut entry = serde_json::json!({
                // Display form: `gw_` stripped for gateway sessions, full
                // composite for channel-driven sessions.
                "session_id": session_id,
                // Full DB key for API operations (delete, messages, abort).
                "session_key": meta.key.clone(),
                "created_at": meta.created_at.to_rfc3339(),
                "last_activity": meta.last_activity.to_rfc3339(),
                "message_count": meta.message_count,
                "agent_alias": agent_alias,
                "channel_id": meta.channel_id,
            });
            if let Some(name) = meta.name {
                entry["name"] = serde_json::Value::String(name);
            }
            entry
        })
        .collect();

    Json(serde_json::json!({ "sessions": sessions })).into_response()
}

/// GET /api/sessions/{id}/messages — load persisted gateway WebSocket chat transcript
pub async fn handle_api_session_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let Some(ref backend) = state.session_backend else {
        return Json(serde_json::json!({
            "session_id": id,
            "messages": [],
            "session_persistence": false,
        }))
        .into_response();
    };

    // Accept either the full DB key (channel-driven sessions like
    // `discord.clamps_…`) or the stripped form (legacy callers that pass
    // just the UUID for gateway sessions).
    let session_key = if id.starts_with("gw_") || id.contains('_') {
        id.clone()
    } else {
        format!("gw_{id}")
    };
    let msgs = backend.load_with_timestamps(&session_key);
    let messages: Vec<serde_json::Value> = msgs
        .into_iter()
        .map(|m| {
            serde_json::json!({
                "role": m.message.role,
                "content": m.message.content,
                "created_at": m.created_at.map(|dt| dt.to_rfc3339()),
            })
        })
        .collect();

    Json(serde_json::json!({
        "session_id": id,
        "messages": messages,
        "session_persistence": true,
    }))
    .into_response()
}

/// POST /api/sessions/{id}/messages — push a visible notification into a gateway session
pub async fn handle_api_session_message_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<SessionMessagePostBody>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    if body.content.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "content is required"})),
        )
            .into_response();
    }

    let Some(ref backend) = state.session_backend else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Session persistence is disabled"})),
        )
            .into_response();
    };

    let session_key = format!("gw_{id}");
    if !backend
        .list_sessions()
        .iter()
        .any(|key| key == &session_key)
    {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Session not found"})),
        )
            .into_response();
    }

    let _session_guard = match state.session_queue.acquire(&session_key).await {
        Ok(guard) => guard,
        Err(crate::session_queue::SessionQueueError::QueueFull { .. }) => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"error": "Session queue is full"})),
            )
                .into_response();
        }
        Err(crate::session_queue::SessionQueueError::Timeout { .. }) => {
            return (
                StatusCode::REQUEST_TIMEOUT,
                Json(serde_json::json!({"error": "Timed out waiting for session queue"})),
            )
                .into_response();
        }
    };

    let message = zeroclaw_providers::ChatMessage::assistant(&body.content);
    if let Err(e) = backend.append(&session_key, &message) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to append session message: {e}")})),
        )
            .into_response();
    }

    // Use the raw dashboard session ID here to match the WS `?session_id=`
    // query parameter; the `gw_` storage key is only for persistence.
    let event = serde_json::json!({
        "type": "message",
        "session_id": id.clone(),
        "role": "assistant",
        "content": body.content.clone(),
        "source": "api",
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    let _ = state.event_tx.send(event);

    Json(serde_json::json!({
        "status": "ok",
        "session_id": id,
        "message": {
            "role": "assistant",
            "content": message.content,
        },
        "session_persistence": true,
    }))
    .into_response()
}

/// DELETE /api/sessions/{id} — delete a gateway session
pub async fn handle_api_session_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let Some(ref backend) = state.session_backend else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Session persistence is disabled"})),
        )
            .into_response();
    };

    let session_key = if id.starts_with("gw_") || id.contains('_') {
        id.clone()
    } else {
        format!("gw_{id}")
    };

    let token = state
        .cancel_tokens
        .lock()
        .expect("cancel_tokens lock poisoned")
        .remove(&session_key);
    if let Some(token) = token {
        token.cancel();
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"session_key": session_key})),
            "cancelled in-flight turn for deleted session"
        );
    }

    match backend.delete_session(&session_key) {
        Ok(true) => Json(serde_json::json!({"deleted": true, "session_id": id})).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Session not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to delete session: {e}")})),
        )
            .into_response(),
    }
}

/// PUT /api/sessions/{id} — rename a gateway session
pub async fn handle_api_session_rename(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let Some(ref backend) = state.session_backend else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Session persistence is disabled"})),
        )
            .into_response();
    };

    let name = body["name"].as_str().unwrap_or("").trim();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "name is required"})),
        )
            .into_response();
    }

    let session_key = format!("gw_{id}");

    // Verify the session exists before renaming
    let sessions = backend.list_sessions();
    if !sessions.contains(&session_key) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Session not found"})),
        )
            .into_response();
    }

    match backend.set_session_name(&session_key, name) {
        Ok(()) => Json(serde_json::json!({"session_id": id, "name": name})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to rename session: {e}")})),
        )
            .into_response(),
    }
}

/// GET /api/sessions/running — list sessions currently in "running" state
pub async fn handle_api_sessions_running(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let Some(ref backend) = state.session_backend else {
        return Json(serde_json::json!({
            "sessions": [],
            "message": "Session persistence is disabled"
        }))
        .into_response();
    };

    let running = backend.list_running_sessions();
    let sessions: Vec<serde_json::Value> = running
        .into_iter()
        .filter_map(|meta| {
            let session_id = meta.key.strip_prefix("gw_")?;
            Some(serde_json::json!({
                "session_id": session_id,
                "created_at": meta.created_at.to_rfc3339(),
                "last_activity": meta.last_activity.to_rfc3339(),
                "message_count": meta.message_count,
            }))
        })
        .collect();

    Json(serde_json::json!({ "sessions": sessions })).into_response()
}

/// GET /api/sessions/{id}/state — get session state
pub async fn handle_api_session_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let Some(ref backend) = state.session_backend else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Session persistence is disabled"})),
        )
            .into_response();
    };

    let session_key = format!("gw_{id}");
    match backend.get_session_state(&session_key) {
        Ok(Some(ss)) => {
            let mut resp = serde_json::json!({
                "session_id": id,
                "state": ss.state,
            });
            if let Some(turn_id) = ss.turn_id {
                resp["turn_id"] = serde_json::Value::String(turn_id);
            }
            if let Some(started) = ss.turn_started_at {
                resp["turn_started_at"] = serde_json::Value::String(started.to_rfc3339());
            }
            Json(resp).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Session not found"})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to get session state: {e}")})),
        )
            .into_response(),
    }
}

// ── Session abort endpoint ────────────────────────────────────────

pub async fn handle_api_session_abort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Err(e) = require_auth(&state, &headers) {
        return e.into_response();
    }

    let session_key = format!("gw_{id}");

    // Look up and cancel the token. Hold the lock only long enough to
    // clone the token — cancellation itself does not need the lock.
    let token = state
        .cancel_tokens
        .lock()
        .expect("cancel_tokens lock poisoned")
        .get(&session_key)
        .cloned();

    if let Some(token) = token {
        token.cancel();
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"session_key": session_key})),
            "session abort requested"
        );
        Json(serde_json::json!({ "status": "aborted" })).into_response()
    } else {
        Json(serde_json::json!({ "status": "no_active_response" })).into_response()
    }
}

// Shared test helper: `api_config` tests reuse this AppState builder for the
// agent rename/delete cascade handlers/coverage).

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
pub(crate) use tests::test_state;
