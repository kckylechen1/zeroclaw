//! `/api/agents/{alias}/backup*` and `/api/agents/{alias}/data-retention*`
//! — trusted operator surface for workspace backup and data-retention
//! operations.
//!
//! These are operator-authority actions, not ordinary model tools: they
//! overwrite workspace directories (restore) and irreversibly delete
//! workspace files (purge). Every route is operator-bearer-gated via the
//! shared [`crate::operator_auth`] contract, exactly like the User Model
//! review surface. The handlers are thin: they invoke the same
//! `BackupTool` / `DataManagementTool` command methods the model-visible
//! tools dispatch to, so the safety semantics (restore requires
//! `confirm`, purge defaults to `dry_run`) cannot drift between the two
//! surfaces.

use std::net::SocketAddr;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use zeroclaw_tools::backup_tool::BackupTool;
use zeroclaw_tools::data_management::DataManagementTool;

use crate::AppState;

fn error_json(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": message }))).into_response()
}

/// Small by-value error for helper fallibles, converted to a `Response` at
/// the call site (an `axum::Response` is far too large for a `Result`
/// Err-variant).
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn into_response(self) -> Response {
        error_json(self.status, &self.message)
    }
}

/// Resolved per-agent workspace plus the backup/retention policy knobs
/// that scope what the operator endpoints may touch.
struct WorkspaceContext {
    workspace: std::path::PathBuf,
    backup: zeroclaw_config::schema::BackupConfig,
    retention_days: u64,
}

/// Resolve an agent's workspace and the backup/retention policy knobs
/// after the operator gate. Unknown aliases are a 404 — the operator API
/// must not synthesize workspaces for agents the config never declared.
fn workspace_context(state: &AppState, alias: &str) -> Result<WorkspaceContext, ApiError> {
    let config = state.config.read();
    if !config.agents.contains_key(alias) {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("unknown agent '{alias}'"),
        });
    }
    Ok(WorkspaceContext {
        workspace: config.agent_workspace_dir(alias),
        backup: config.backup.clone(),
        retention_days: config.data_retention.retention_days,
    })
}

/// Translate a tool command result into an HTTP response. Success carries
/// the command's JSON payload; a tool-level failure keeps that payload
/// under `result` alongside the error message so operators see the
/// mismatch details on a failed verification.
fn tool_response(result: anyhow::Result<zeroclaw_api::tool::ToolResult>) -> Response {
    match result {
        Ok(tool_result) => {
            let payload: serde_json::Value =
                serde_json::from_str(&tool_result.output).unwrap_or(serde_json::Value::Null);
            if tool_result.success {
                (StatusCode::OK, axum::Json(payload)).into_response()
            } else {
                let status = if tool_result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("Backup not found")
                {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::CONFLICT
                };
                (
                    status,
                    axum::Json(serde_json::json!({
                        "error": tool_result.error.unwrap_or_else(|| "operation failed".into()),
                        "result": payload,
                    })),
                )
                    .into_response()
            }
        }
        Err(err) => error_json(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Parse an optional JSON object body into `T`. An absent/empty body uses
/// `T::default()`; a malformed body is a 400, never a silent default —
/// destructive flags must be explicit, not guessed from a broken payload.
fn parse_body<T: serde::de::DeserializeOwned + Default>(body: &Bytes) -> Result<T, ApiError> {
    if body.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(body).map_err(|err| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: err.to_string(),
    })
}

/// `POST /api/agents/{alias}/backup` — create a timestamped workspace
/// backup using the configured `backup.include_dirs` / `backup.max_keep`.
pub async fn create_backup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(alias): Path<String>,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let ctx = match workspace_context(&state, &alias) {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let tool = BackupTool::new(ctx.workspace, ctx.backup.include_dirs, ctx.backup.max_keep);
    tool_response(tool.cmd_create().await)
}

/// `GET /api/agents/{alias}/backup` — list backups, newest first.
pub async fn list_backups(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(alias): Path<String>,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let ctx = match workspace_context(&state, &alias) {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let tool = BackupTool::new(ctx.workspace, ctx.backup.include_dirs, ctx.backup.max_keep);
    tool_response(tool.cmd_list().await)
}

/// `POST /api/agents/{alias}/backup/{name}/verify` — checksum-verify a
/// backup against its manifest.
pub async fn verify_backup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((alias, name)): Path<(String, String)>,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let ctx = match workspace_context(&state, &alias) {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let tool = BackupTool::new(ctx.workspace, ctx.backup.include_dirs, ctx.backup.max_keep);
    tool_response(tool.cmd_verify(&name).await)
}

#[derive(serde::Deserialize, Default)]
struct RestoreBody {
    /// `false` (default) returns a dry-run preview; only `true` restores.
    confirm: Option<bool>,
}

/// `POST /api/agents/{alias}/backup/{name}/restore` — restore a backup.
/// Body `{"confirm": true}` is required for an actual restore; without it
/// the endpoint returns the same dry-run preview the tool does and
/// mutates nothing.
pub async fn restore_backup(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((alias, name)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let body: RestoreBody = match parse_body(&body) {
        Ok(body) => body,
        Err(err) => return err.into_response(),
    };
    let ctx = match workspace_context(&state, &alias) {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let tool = BackupTool::new(ctx.workspace, ctx.backup.include_dirs, ctx.backup.max_keep);
    tool_response(tool.cmd_restore(&name, body.confirm.unwrap_or(false)).await)
}

/// `GET /api/agents/{alias}/data-retention/status` — retention window and
/// how many files are past it.
pub async fn retention_status(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(alias): Path<String>,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let ctx = match workspace_context(&state, &alias) {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let tool = DataManagementTool::new(ctx.workspace, ctx.retention_days);
    tool_response(tool.cmd_retention_status().await)
}

/// `GET /api/agents/{alias}/data-retention/stats` — workspace storage
/// statistics.
pub async fn retention_stats(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(alias): Path<String>,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let ctx = match workspace_context(&state, &alias) {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let tool = DataManagementTool::new(ctx.workspace, ctx.retention_days);
    tool_response(tool.cmd_stats().await)
}

#[derive(serde::Deserialize, Default)]
struct PurgeBody {
    /// `true` (default) only reports what would be deleted; only an
    /// explicit `false` deletes.
    dry_run: Option<bool>,
}

/// `POST /api/agents/{alias}/data-retention/purge` — delete workspace
/// files older than the retention window. Defaults to dry-run; an actual
/// purge requires an explicit `{"dry_run": false}`.
pub async fn retention_purge(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(alias): Path<String>,
    body: Bytes,
) -> Response {
    if let Some(err) = crate::operator_auth::gate_operator_identity(&state, peer, &headers) {
        return err;
    }
    let body: PurgeBody = match parse_body(&body) {
        Ok(body) => body,
        Err(err) => return err.into_response(),
    };
    let ctx = match workspace_context(&state, &alias) {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let tool = DataManagementTool::new(ctx.workspace, ctx.retention_days);
    tool_response(tool.cmd_purge(body.dry_run.unwrap_or(true)).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use http_body_util::BodyExt;
    use std::sync::Arc;

    fn loopback_peer() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 9))
    }

    fn operator_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_str("Bearer op-token").unwrap(),
        );
        headers
    }

    fn anon_headers() -> HeaderMap {
        HeaderMap::new()
    }

    /// State whose install root is the tempdir, so the `main` agent's
    /// workspace resolves to `<tmp>/agents/main/workspace`. The tempdir is
    /// returned so the caller can keep it alive for the test's duration.
    fn state_with_workspace() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = zeroclaw_config::schema::Config {
            config_path: dir.path().join("config.toml"),
            ..Default::default()
        };
        config.agents.insert(
            "main".to_string(),
            zeroclaw_config::schema::AliasedAgentConfig::default(),
        );
        let mut state = crate::api::test_state(config);
        state.pairing = Arc::new(zeroclaw_runtime::security::pairing::PairingGuard::new(
            true,
            &["op-token".into()],
        ));
        (dir, state)
    }

    async fn json_of(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!({ "raw": String::from_utf8_lossy(&bytes) }));
        (status, json)
    }

    fn body<T: serde::Serialize>(value: &T) -> Bytes {
        Bytes::from(serde_json::to_vec(value).unwrap())
    }

    #[tokio::test]
    async fn operator_gate_rejects_anonymous_and_wrong_token() {
        let (_dir, state) = state_with_workspace();

        let (status, _) = json_of(
            create_backup(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                anon_headers(),
                Path("main".to_string()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "anonymous must be rejected"
        );

        let mut wrong = operator_headers();
        wrong.insert(
            "authorization",
            HeaderValue::from_str("Bearer wrong-token").unwrap(),
        );
        let (status, _) = json_of(
            retention_stats(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                wrong,
                Path("main".to_string()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "bad token must be rejected"
        );
    }

    #[tokio::test]
    async fn unknown_agent_is_not_found() {
        let (_dir, state) = state_with_workspace();
        let (status, payload) = json_of(
            list_backups(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("ghost".to_string()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(payload["error"].as_str().unwrap().contains("ghost"));
    }

    #[tokio::test]
    async fn backup_create_list_verify_restore_roundtrip() {
        let (_dir, state) = state_with_workspace();
        let workspace = state.config.read().agent_workspace_dir("main");

        // Seed a workspace file covered by the default include_dirs.
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(workspace.join("config/app.toml"), "v1").unwrap();

        // Create → manifest-backed backup.
        let (status, created) = json_of(
            create_backup(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("main".to_string()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "create body={created}");
        let backup_name = created["backup"].as_str().unwrap().to_string();
        assert_eq!(created["file_count"], 1);

        // List → the new backup is present.
        let (status, listed) = json_of(
            list_backups(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("main".to_string()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&backup_name.as_str()));

        // Verify → passes while the backup is untouched.
        let (status, verified) = json_of(
            verify_backup(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(("main".to_string(), backup_name.clone())),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "verify body={verified}");
        assert_eq!(verified["pass"], true);

        // Restore without confirm → dry-run preview, workspace untouched.
        std::fs::write(workspace.join("config/app.toml"), "v2").unwrap();
        let (status, preview) = json_of(
            restore_backup(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(("main".to_string(), backup_name.clone())),
                Bytes::new(),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "dry-run body={preview}");
        assert_eq!(preview["dry_run"], true);
        assert_eq!(
            std::fs::read_to_string(workspace.join("config/app.toml")).unwrap(),
            "v2",
            "restore without confirm must not overwrite"
        );

        // Restore with confirm → workspace reverted to backup content.
        let (status, restored) = json_of(
            restore_backup(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(("main".to_string(), backup_name.clone())),
                body(&serde_json::json!({ "confirm": true })),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "restore body={restored}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("config/app.toml")).unwrap(),
            "v1",
            "confirmed restore must overwrite workspace content"
        );

        // Unknown backup names are 404s on both verify and restore.
        let (status, _) = json_of(
            verify_backup(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(("main".to_string(), "backup-missing".to_string())),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = json_of(
            restore_backup(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(("main".to_string(), "backup-missing".to_string())),
                body(&serde_json::json!({ "confirm": true })),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn corrupted_backup_verify_reports_conflict() {
        let (_dir, state) = state_with_workspace();
        let workspace = state.config.read().agent_workspace_dir("main");
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(workspace.join("config/app.toml"), "original").unwrap();

        let (_, created) = json_of(
            create_backup(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("main".to_string()),
            )
            .await
            .into_response(),
        )
        .await;
        let name = created["backup"].as_str().unwrap().to_string();
        std::fs::write(
            workspace
                .join("backups")
                .join(&name)
                .join("config/app.toml"),
            "corrupted",
        )
        .unwrap();

        let (status, payload) = json_of(
            verify_backup(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path(("main".to_string(), name)),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(payload["result"]["pass"], false);
        assert!(
            !payload["result"]["mismatches"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn purge_defaults_to_dry_run_until_explicitly_disabled() {
        let (_dir, state) = state_with_workspace();
        // retention_days = 0 → cutoff is "now"; a file written at least a
        // second ago is always past it (mtime strictly < cutoff).
        state.config.write().data_retention.retention_days = 0;
        let workspace = state.config.read().agent_workspace_dir("main");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("stale.txt"), "old").unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // Status sees the file.
        let (status, status_payload) = json_of(
            retention_status(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("main".to_string()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(status_payload["retention_days"], 0);
        assert_eq!(status_payload["affected_files"], 1);

        // Purge with no body → dry-run: reported, not deleted.
        let (status, dry) = json_of(
            retention_purge(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("main".to_string()),
                Bytes::new(),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "purge body={dry}");
        assert_eq!(dry["dry_run"], true);
        assert_eq!(dry["files"], 1);
        assert!(
            workspace.join("stale.txt").exists(),
            "dry-run purge must not delete"
        );

        // Malformed body → 400, never a guessed default.
        let (status, _) = json_of(
            retention_purge(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("main".to_string()),
                Bytes::from_static(b"{not json"),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Explicit dry_run=false → actually deletes.
        let (status, purged) = json_of(
            retention_purge(
                State(state.clone()),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("main".to_string()),
                body(&serde_json::json!({ "dry_run": false })),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "purge body={purged}");
        assert_eq!(purged["dry_run"], false);
        assert_eq!(purged["files"], 1);
        assert!(
            !workspace.join("stale.txt").exists(),
            "explicit purge must delete"
        );

        // Stats reflects the now-empty workspace.
        let (status, stats) = json_of(
            retention_stats(
                State(state),
                ConnectInfo(loopback_peer()),
                operator_headers(),
                Path("main".to_string()),
            )
            .await
            .into_response(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stats["total_files"], 0);
    }
}
