//! Bounded device-bridge ingress over the same Tachi admission as model tools.
//! No operator tokens, second task store, detached agent or local harness runner.

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use zeroclaw_api::delegation_control::{
    DelegationControlRequest, DelegationControlResponse, DelegationOperation, VERSION,
};
use zeroclaw_config::schema::{Config, GatewayBridgeConfig};
use zeroclaw_policy::PerSenderTracker;
use zeroclaw_runtime::approval::ApprovalManager;
use zeroclaw_tools::tachi_delegation::{APPROVAL_REQUIRED, TachiDelegation};

use crate::AppState;

const BODY_LIMIT: usize = 24 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(125);

/// Transport admission only. Task truth remains in Tachi and the existing
/// SQLite binding; this context holds no task IDs, results or worker handles.
struct ControlContext {
    selected_data_dir: PathBuf,
    tracker: PerSenderTracker,
    slots: Arc<tokio::sync::Semaphore>,
    timeout: Duration,
}

#[derive(Clone)]
struct Binding {
    name: String,
    initial: GatewayBridgeConfig,
    agent: String,
    session: String,
}

impl Binding {
    fn allows(&self, config: &Config, operation: DelegationOperation) -> bool {
        let Some(current) = config.gateway.bridges.get(&self.name) else {
            return false;
        };
        let hash = self.initial.token_hash.to_ascii_lowercase();
        let same_hash = |other: &str| {
            zeroclaw_config::tokens::constant_time_eq(&hash, &other.to_ascii_lowercase())
        };
        same_hash(&current.token_hash)
            && !config
                .gateway
                .operator
                .token_hashes
                .iter()
                .any(|stored| same_hash(stored))
            && config
                .gateway
                .bridges
                .values()
                .filter(|b| same_hash(&b.token_hash))
                .count()
                == 1
            && config.agents.get(&self.agent).is_some_and(|a| a.enabled)
            && [&self.initial, current].into_iter().all(|bridge| {
                bridge.owner.is_none()
                    && bridge.allows_session(&self.session)
                    && bridge
                        .device
                        .as_ref()
                        .is_some_and(|device| device.allows(&self.agent, operation.tool_name()))
            })
    }
}

fn response(
    status: StatusCode,
    operation: &str,
    request_id: Option<String>,
    success: bool,
    output: Value,
    error: Option<String>,
) -> Response {
    (
        status,
        Json(DelegationControlResponse {
            version: VERSION,
            operation: operation.to_string(),
            request_id,
            success,
            output,
            error,
        }),
    )
        .into_response()
}

fn refused(
    status: StatusCode,
    operation: &str,
    request_id: Option<String>,
    code: &str,
) -> Response {
    response(
        status,
        operation,
        request_id,
        false,
        json!({"code":code}),
        Some(zeroclaw_runtime::i18n::get_required_cli_string(
            "gateway-bridge-access-denied",
        )),
    )
}

pub(crate) fn routes(state: AppState) -> Router {
    let context = Arc::new(ControlContext {
        selected_data_dir: state.config.read().data_dir.clone(),
        tracker: PerSenderTracker::new(),
        slots: Arc::new(tokio::sync::Semaphore::new(8)),
        timeout: CONTROL_TIMEOUT,
    });
    router(state, context)
}

fn router(state: AppState, context: Arc<ControlContext>) -> Router {
    Router::new()
        .route("/api/agents/{agent}/delegations/{operation}", post(control))
        .layer(Extension(context))
        .with_state(state)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(BODY_LIMIT))
        // Tachi's bounded start may outlive the ordinary 30-second HTTP budget.
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT, CONTROL_TIMEOUT + Duration::from_secs(5),
        ))
}

async fn control(
    State(state): State<AppState>,
    Extension(context): Extension<Arc<ControlContext>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    Path((agent, operation)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(op) = DelegationOperation::parse(&operation) else {
        return refused(
            StatusCode::NOT_FOUND,
            "unsupported",
            None,
            "unsupported_operation",
        );
    };
    let rate_key =
        crate::client_key_from_request(Some(peer), &headers, state.trust_forwarded_headers);
    if state
        .auth_limiter
        .check_rate_limit_strict(&rate_key)
        .is_err()
    {
        return refused(
            StatusCode::TOO_MANY_REQUESTS,
            &operation,
            None,
            "rate_limited",
        );
    }
    let token = crate::operator_auth::extract_bearer(&headers).filter(|t| !t.is_empty());
    let bridge = token.and_then(|token| {
        state
            .config
            .read()
            .gateway
            .bridge_for_token(token)
            .map(|(name, b)| (name.to_owned(), b.clone()))
    });
    let Some((name, initial)) = bridge else {
        state.auth_limiter.record_attempt_strict(&rate_key);
        return refused(
            StatusCode::UNAUTHORIZED,
            &operation,
            None,
            "bridge_token_required",
        );
    };
    if state.pairing.tokens().iter().any(|stored| {
        zeroclaw_config::tokens::constant_time_eq(stored, &initial.token_hash.to_ascii_lowercase())
    }) {
        return refused(
            StatusCode::FORBIDDEN,
            &operation,
            None,
            "bridge_token_required",
        );
    }
    let request: DelegationControlRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => {
            return refused(
                StatusCode::BAD_REQUEST,
                &operation,
                None,
                "invalid_arguments",
            );
        }
    };
    let args = match request.arguments(op) {
        Some(args) => args,
        None => {
            return refused(
                StatusCode::BAD_REQUEST,
                &operation,
                request.request_id,
                "invalid_arguments",
            );
        }
    };
    let binding = Binding {
        name,
        initial,
        agent: agent.clone(),
        session: request.session_id,
    };
    if !binding.allows(&state.config.read(), op) {
        return refused(
            StatusCode::FORBIDDEN,
            &operation,
            request.request_id,
            "bridge_scope_denied",
        );
    }
    if *state.shutdown_tx.borrow() {
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            &operation,
            request.request_id,
            "shutting_down",
        );
    }
    if !state.rate_limiter.allow_webhook(&rate_key) {
        return refused(
            StatusCode::TOO_MANY_REQUESTS,
            &operation,
            request.request_id,
            "rate_limited",
        );
    }
    let permit = match context.slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return refused(
                StatusCode::TOO_MANY_REQUESTS,
                &operation,
                request.request_id,
                "control_busy",
            );
        }
    };
    let (live, authority, limits, pairing) = (
        state.config.clone(),
        binding.clone(),
        context.clone(),
        state.pairing.clone(),
    );
    let service = TachiDelegation::new(&agent, move |name, action| {
        let current = live.read();
        if !authority.allows(&current, op)
            || pairing.tokens().iter().any(|stored| {
                zeroclaw_config::tokens::constant_time_eq(
                    stored,
                    &authority.initial.token_hash.to_ascii_lowercase(),
                )
            })
        {
            return Err("bridge_scope_denied".to_string());
        }
        // Supervised Tachi actions require fresh owner approval even when
        // auto_approve contains '*'. This surface has no approval authority.
        let profile = current
            .risk_profile_for_agent(&authority.agent)
            .ok_or_else(|| "delegation denied by current agent policy".to_string())?;
        if ApprovalManager::from_risk_profile(profile).needs_approval(name) {
            return Err(APPROVAL_REQUIRED.to_string());
        }
        zeroclaw_runtime::tools::delegation::resolve(
            &current,
            &limits.selected_data_dir,
            &authority.agent,
            &limits.tracker,
            name,
            action,
        )
    });
    // A lost HTTP caller does not cancel an already admitted start. Only this
    // bounded admission/receipt operation is detached, never an agent turn or
    // harness runner. On deadline/shutdown its durable claim stays unresolved;
    // a client inspects the same request ID instead of issuing another start.
    let mut shutdown = state.shutdown_tx.subscribe();
    let control_timeout = context.timeout;
    let submitted = zeroclaw_spawn::spawn!(async move {
        let _permit = permit;
        zeroclaw_api::TOOL_LOOP_THREAD_ID.scope(Some(format!("delegation:{agent}")), async {
            tokio::select! {
                result = tokio::time::timeout(control_timeout, service.control(op, args)) => result.ok(),
                _ = async {
                    while !*shutdown.borrow() {
                        if shutdown.changed().await.is_err() { break; }
                    }
                } => None,
            }
        }).await
    });
    let result = submitted.await;
    if !binding.allows(&state.config.read(), op)
        || state.pairing.tokens().iter().any(|stored| {
            zeroclaw_config::tokens::constant_time_eq(
                stored,
                &binding.initial.token_hash.to_ascii_lowercase(),
            )
        })
    {
        return refused(
            StatusCode::FORBIDDEN,
            &operation,
            request.request_id,
            "access_revoked",
        );
    }
    match result {
        Ok(Some(Ok(tool))) => {
            let Some(output) = tool
                .output
                .data()
                .filter(|value| value.is_object())
                .cloned()
            else {
                return refused(
                    StatusCode::BAD_GATEWAY,
                    &operation,
                    request.request_id,
                    "invalid_receipt",
                );
            };
            let status = if tool.success {
                StatusCode::OK
            } else if matches!(
                output["code"].as_str(),
                Some("denied" | "approval_required")
            ) {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::CONFLICT
            };
            response(
                status,
                &operation,
                request.request_id,
                tool.success,
                output,
                tool.error,
            )
        }
        // Neither an error nor a missing receipt is proof that Tachi did not act.
        _ => refused(
            StatusCode::GATEWAY_TIMEOUT,
            &operation,
            request.request_id,
            if op == DelegationOperation::Start {
                "submission_unknown"
            } else {
                "control_outcome_unknown"
            },
        ),
    }
}

#[cfg(test)]
#[path = "api_delegation_tests.rs"]
mod tests;
