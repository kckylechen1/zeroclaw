use super::ModelProvider;
use super::dispatch::ProviderDispatch;
use super::stream_guard::AbortOnDrop;
use super::traits::{
    ChatMessage, ChatRequest, ChatResponse, StreamChunk, StreamEvent, StreamOptions, StreamResult,
};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Info about a model_provider fallback that occurred during a request.
#[derive(Debug, Clone)]
pub struct ProviderFallbackInfo {
    /// ModelProvider that was originally requested.
    pub requested_provider: String,
    /// Model that was originally requested.
    pub requested_model: String,
    /// ModelProvider that actually served the request.
    pub actual_provider: String,
    /// Model that actually served the request.
    pub actual_model: String,
}

tokio::task_local! {
    static PROVIDER_FALLBACK: RefCell<Option<ProviderFallbackInfo>>;
}

/// Take (consume) the last model_provider fallback info, if any.
/// Must be called within a `scope_provider_fallback` scope.
pub fn take_last_provider_fallback() -> Option<ProviderFallbackInfo> {
    PROVIDER_FALLBACK
        .try_with(|cell| cell.borrow_mut().take())
        .ok()
        .flatten()
}

/// Run the given future within a provider-fallback scope.
/// Both `record_provider_fallback` (inside ReliableModelProvider) and
/// `take_last_provider_fallback` (post-loop channel code) must execute
/// within this scope for the data to be visible.
pub async fn scope_provider_fallback<F: std::future::Future>(future: F) -> F::Output {
    PROVIDER_FALLBACK.scope(RefCell::new(None), future).await
}

/// Record a model_provider fallback event.
fn record_provider_fallback(
    requested_provider: &str,
    requested_model: &str,
    actual_provider: &str,
    actual_model: &str,
) {
    let _ = PROVIDER_FALLBACK.try_with(|cell| {
        *cell.borrow_mut() = Some(ProviderFallbackInfo {
            requested_provider: requested_provider.to_string(),
            requested_model: requested_model.to_string(),
            actual_provider: actual_provider.to_string(),
            actual_model: actual_model.to_string(),
        });
    });
}

struct ProviderFallbackRecord {
    requested_provider: String,
    requested_model: String,
    actual_provider: String,
    actual_model: String,
}

impl ProviderFallbackRecord {
    fn new_if_recovered(
        requested_provider: &str,
        requested_model: &str,
        actual_provider: &str,
        actual_model: &str,
    ) -> Option<Self> {
        if requested_provider == actual_provider && requested_model == actual_model {
            return None;
        }

        Some(Self {
            requested_provider: requested_provider.to_string(),
            requested_model: requested_model.to_string(),
            actual_provider: actual_provider.to_string(),
            actual_model: actual_model.to_string(),
        })
    }

    fn record(&self) {
        record_provider_fallback(
            &self.requested_provider,
            &self.requested_model,
            &self.actual_provider,
            &self.actual_model,
        );
    }
}

fn stream_with_success_recording<T, IsFinal>(
    rx: tokio::sync::mpsc::Receiver<StreamResult<T>>,
    guard: AbortOnDrop,
    fallback_record: Option<ProviderFallbackRecord>,
    is_final: IsFinal,
) -> stream::BoxStream<'static, StreamResult<T>>
where
    T: Send + 'static,
    IsFinal: Fn(&T) -> bool + Send + 'static,
{
    stream::unfold(
        (rx, guard, fallback_record, false, false, is_final),
        |(mut rx, guard, fallback_record, saw_error, recorded, is_final)| async move {
            match rx.recv().await {
                Some(event) => {
                    let mut saw_error = saw_error;
                    let mut recorded = recorded;
                    match &event {
                        Ok(value) if !saw_error && !recorded && is_final(value) => {
                            if let Some(record) = &fallback_record {
                                record.record();
                            }
                            recorded = true;
                        }
                        Err(_) => {
                            saw_error = true;
                        }
                        Ok(_) => {}
                    }
                    Some((
                        event,
                        (rx, guard, fallback_record, saw_error, recorded, is_final),
                    ))
                }
                None => {
                    if !saw_error
                        && !recorded
                        && let Some(record) = &fallback_record
                    {
                        record.record();
                    }
                    None
                }
            }
        },
    )
    .boxed()
}

pub fn transient_error_hint(err: &anyhow::Error) -> Option<&'static str> {
    let msg = err.to_string();
    // 503 / service unavailable / high demand (Gemini, OpenAI, etc.)
    if msg.contains("503")
        || msg.to_ascii_lowercase().contains("unavailable")
        || msg.to_ascii_lowercase().contains("high demand")
        || msg.to_ascii_lowercase().contains("overloaded")
    {
        return Some(
            "I'm temporarily unable to reach my AI backend — please try again in a moment.",
        );
    }
    // 429 / quota / rate limit
    if msg.contains("429")
        || msg.to_ascii_lowercase().contains("rate limit")
        || msg.to_ascii_lowercase().contains("quota")
    {
        return Some("I've hit a usage limit — please try again shortly.");
    }
    None
}

/// Check if an error is non-retryable (client errors that won't resolve with retries).
pub fn is_non_retryable(err: &anyhow::Error) -> bool {
    // Context window errors are NOT non-retryable — they can be recovered
    // by truncating conversation history, so let the retry loop handle them.
    if is_context_window_exceeded(err) {
        return false;
    }

    // Tool schema validation errors are NOT non-retryable — the model_provider's
    // built-in fallback in compatible.rs can recover by switching to
    // prompt-guided tool instructions.
    if is_tool_schema_error(err) {
        return false;
    }

    // 4xx errors are generally non-retryable (bad request, auth failure, etc.),
    // except 429 (rate-limit — transient) and 408 (timeout — worth retrying).
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>()
        && let Some(status) = reqwest_err.status()
    {
        let code = status.as_u16();
        return status.is_client_error() && code != 429 && code != 408;
    }
    // Fallback: parse status codes from stringified errors (some model_providers
    // embed codes in error messages rather than returning typed HTTP errors).
    let msg = err.to_string();
    for word in msg.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(code) = word.parse::<u16>()
            && (400..500).contains(&code)
        {
            return code != 429 && code != 408;
        }
    }

    // Heuristic: detect auth/model failures by keyword when no HTTP status
    // is available (e.g. gRPC or custom transport errors).
    let msg_lower = msg.to_lowercase();
    let auth_failure_hints = [
        "invalid api key",
        "incorrect api key",
        "missing api key",
        "api key not set",
        "authentication failed",
        "auth failed",
        "unauthorized",
        "forbidden",
        "permission denied",
        "access denied",
        "invalid token",
    ];

    if auth_failure_hints
        .iter()
        .any(|hint| msg_lower.contains(hint))
    {
        return true;
    }

    msg_lower.contains("model")
        && (msg_lower.contains("not found")
            || msg_lower.contains("unknown")
            || msg_lower.contains("unsupported")
            || msg_lower.contains("does not exist")
            || msg_lower.contains("invalid"))
}

/// Check if an error indicates an authentication/authorization failure.
/// Used by channels to evict cached model_providers whose OAuth tokens may have
/// expired so the next request triggers a fresh credential resolution.
pub fn is_auth_error(err: &anyhow::Error) -> bool {
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>()
        && let Some(status) = reqwest_err.status()
    {
        let code = status.as_u16();
        return code == 401 || code == 403;
    }

    let msg_lower = err.to_string().to_lowercase();
    let hints = [
        "401 unauthorized",
        "403 forbidden",
        "invalid api key",
        "incorrect api key",
        "authentication failed",
        "auth failed",
        "unauthorized",
        "invalid token",
        "token expired",
        "access_token",
    ];

    hints.iter().any(|hint| msg_lower.contains(hint))
}

pub fn is_tool_schema_error(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    let hints = [
        "tool call validation failed",
        "was not in request",
        "not found in tool list",
        "invalid_tool_call",
    ];
    hints.iter().any(|hint| lower.contains(hint))
}

pub fn is_context_window_exceeded(err: &anyhow::Error) -> bool {
    let lower = err.to_string().to_lowercase();
    let hints = [
        "exceeds the context window",
        "exceeds the available context size",
        "context window of this model",
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
        "prompt exceeds max length",
    ];

    hints.iter().any(|hint| lower.contains(hint))
}

/// Check if an error is a rate-limit (429) error.
fn is_rate_limited(err: &anyhow::Error) -> bool {
    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>()
        && let Some(status) = reqwest_err.status()
    {
        return status.as_u16() == 429;
    }
    let msg = err.to_string();
    msg.contains("429")
        && (msg.contains("Too Many") || msg.contains("rate") || msg.contains("limit"))
}

fn is_non_retryable_rate_limit(err: &anyhow::Error) -> bool {
    if !is_rate_limited(err) {
        return false;
    }

    let msg = err.to_string();
    let lower = msg.to_lowercase();

    let business_hints = [
        "plan does not include",
        "doesn't include",
        "not include",
        "insufficient balance",
        "insufficient_balance",
        "insufficient quota",
        "insufficient_quota",
        "quota exhausted",
        "out of credits",
        "no available package",
        "package not active",
        "purchase package",
        "model not available for your plan",
    ];

    if business_hints.iter().any(|hint| lower.contains(hint)) {
        return true;
    }

    // Known model_provider business codes observed for 429 where retry is futile.
    for token in lower.split(|c: char| !c.is_ascii_digit()) {
        if let Ok(code) = token.parse::<u16>()
            && matches!(code, 1113 | 1311)
        {
            return true;
        }
    }

    false
}

/// Try to extract a Retry-After value (in milliseconds) from an error message.
/// Looks for patterns like `Retry-After: 5` or `retry_after: 2.5` in the error string.
fn parse_retry_after_ms(err: &anyhow::Error) -> Option<u64> {
    let msg = err.to_string();
    let lower = msg.to_lowercase();

    // Look for "retry-after: <number>" or "retry_after: <number>"
    for prefix in &[
        "retry-after:",
        "retry_after:",
        "retry-after ",
        "retry_after ",
    ] {
        if let Some(pos) = lower.find(prefix) {
            let after = &msg[pos + prefix.len()..];
            let num_str: String = after
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(secs) = num_str.parse::<f64>()
                && secs.is_finite()
                && secs >= 0.0
            {
                let millis = Duration::from_secs_f64(secs).as_millis();
                if let Ok(value) = u64::try_from(millis) {
                    return Some(value);
                }
            }
        }
    }
    None
}

fn failure_reason(rate_limited: bool, non_retryable: bool) -> &'static str {
    if rate_limited && non_retryable {
        "rate_limited_non_retryable"
    } else if rate_limited {
        "rate_limited"
    } else if non_retryable {
        "non_retryable"
    } else {
        "retryable"
    }
}

fn compact_error_detail(err: &anyhow::Error) -> String {
    super::sanitize_api_error(&format!("{err:#}"))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderErrorDiagnostic {
    kind: &'static str,
    phase: &'static str,
    hint: &'static str,
    endpoint: Option<String>,
}

fn sanitized_url_endpoint(mut url: reqwest::Url) -> String {
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    super::sanitize_api_error(url.as_ref())
}

fn endpoint_from_error_text(text: &str) -> Option<String> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let raw = text[start..]
        .split(|c: char| c.is_whitespace() || matches!(c, ')' | ',' | ';' | '"'))
        .next()
        .unwrap_or("");
    let url = reqwest::Url::parse(raw)
        .or_else(|_| reqwest::Url::parse(raw.trim_end_matches([':', '.'])))
        .ok()?;
    Some(sanitized_url_endpoint(url))
}

fn provider_error_diagnostic(err: &anyhow::Error) -> ProviderErrorDiagnostic {
    let error_detail = compact_error_detail(err);
    let lower = error_detail.to_lowercase();
    let endpoint = err
        .downcast_ref::<reqwest::Error>()
        .and_then(|reqwest_err| reqwest_err.url().cloned().map(sanitized_url_endpoint))
        .or_else(|| endpoint_from_error_text(&error_detail));

    if is_context_window_exceeded(err) {
        return ProviderErrorDiagnostic {
            kind: "context_window",
            phase: "request_validation",
            hint: "reduce context or use a larger-context model",
            endpoint,
        };
    }

    if is_auth_error(err) {
        return ProviderErrorDiagnostic {
            kind: "auth",
            phase: "http_response",
            hint: "check provider credentials",
            endpoint,
        };
    }

    if is_rate_limited(err) {
        return ProviderErrorDiagnostic {
            kind: "rate_limited",
            phase: "http_response",
            hint: "wait, change key/quota, or switch provider",
            endpoint,
        };
    }

    if let Some(reqwest_err) = err.downcast_ref::<reqwest::Error>() {
        if let Some(status) = reqwest_err.status() {
            let code = status.as_u16();
            let (kind, hint) = if status.is_server_error() {
                (
                    "provider_server",
                    "provider returned a server error; retry or switch provider",
                )
            } else if code == 404 {
                (
                    "model_not_found",
                    "check the configured model id for this provider",
                )
            } else if status.is_client_error() {
                (
                    "client_error",
                    "provider rejected the request; check config, model, or request shape",
                )
            } else {
                ("http_error", "inspect provider response or switch provider")
            };
            return ProviderErrorDiagnostic {
                kind,
                phase: "http_response",
                hint,
                endpoint,
            };
        }

        if reqwest_err.is_timeout() && reqwest_err.is_connect() {
            return ProviderErrorDiagnostic {
                kind: "connect_timeout",
                phase: "tls_or_connect",
                hint: "connection reached the host but timed out during connect/TLS; check VPN, firewall, routing, or switch provider",
                endpoint,
            };
        }

        if reqwest_err.is_timeout() {
            return ProviderErrorDiagnostic {
                kind: "timeout",
                phase: "request",
                hint: "provider request timed out; retry or switch provider",
                endpoint,
            };
        }

        if reqwest_err.is_connect() {
            return ProviderErrorDiagnostic {
                kind: "connect",
                phase: "connect",
                hint: "could not open provider connection; check network, VPN, or firewall",
                endpoint,
            };
        }
    }

    if (lower.contains("client error (connect)") && lower.contains("timed out"))
        || lower.contains("ssl connection timeout")
        || (lower.contains("tls") && lower.contains("timeout"))
    {
        return ProviderErrorDiagnostic {
            kind: "connect_timeout",
            phase: "tls_or_connect",
            hint: "connection reached the host but timed out during connect/TLS; check VPN, firewall, routing, or switch provider",
            endpoint,
        };
    }

    if lower.contains("timed out") || lower.contains("timeout") {
        return ProviderErrorDiagnostic {
            kind: "timeout",
            phase: "request",
            hint: "provider request timed out; retry or switch provider",
            endpoint,
        };
    }

    if lower.contains("dns") || lower.contains("resolve") {
        return ProviderErrorDiagnostic {
            kind: "dns",
            phase: "dns",
            hint: "DNS resolution failed; check network or provider host",
            endpoint,
        };
    }

    if lower.contains("model")
        && (lower.contains("not found")
            || lower.contains("unknown")
            || lower.contains("unsupported")
            || lower.contains("does not exist")
            || lower.contains("invalid"))
    {
        return ProviderErrorDiagnostic {
            kind: "model_not_found",
            phase: "http_response",
            hint: "check the configured model id for this provider",
            endpoint,
        };
    }

    ProviderErrorDiagnostic {
        kind: "provider_error",
        phase: "unknown",
        hint: "inspect provider error or switch provider",
        endpoint,
    }
}

fn provider_failure_attrs(
    provider_name: &str,
    model: &str,
    error_detail: &str,
    diagnostic: &ProviderErrorDiagnostic,
) -> serde_json::Value {
    serde_json::json!({
        "model_provider": provider_name,
        "model": model,
        "error": error_detail,
        "error_kind": diagnostic.kind,
        "error_phase": diagnostic.phase,
        "endpoint": diagnostic.endpoint.as_deref(),
        "hint": diagnostic.hint,
    })
}

fn provider_retry_attrs(
    provider_name: &str,
    model: &str,
    attempt: u32,
    backoff_ms: u64,
    reason: &str,
    error_detail: &str,
    diagnostic: &ProviderErrorDiagnostic,
) -> serde_json::Value {
    serde_json::json!({
        "model_provider": provider_name,
        "model": model,
        "attempt": attempt,
        "backoff_ms": backoff_ms,
        "reason": reason,
        "error": error_detail,
        "error_kind": diagnostic.kind,
        "error_phase": diagnostic.phase,
        "endpoint": diagnostic.endpoint.as_deref(),
        "hint": diagnostic.hint,
    })
}

fn provider_exhausted_attrs(
    provider_name: &str,
    model: &str,
    last_error_detail: Option<&str>,
    last_diagnostic: Option<&ProviderErrorDiagnostic>,
) -> serde_json::Value {
    serde_json::json!({
        "model_provider": provider_name,
        "model": model,
        "error": last_error_detail,
        "error_kind": last_diagnostic.map(|diagnostic| diagnostic.kind),
        "error_phase": last_diagnostic.map(|diagnostic| diagnostic.phase),
        "endpoint": last_diagnostic.and_then(|diagnostic| diagnostic.endpoint.as_deref()),
        "hint": last_diagnostic.map(|diagnostic| diagnostic.hint),
    })
}

fn is_context_turn_boundary(message: &ChatMessage) -> bool {
    message.role == "user"
        && !crate::multimodal::is_prompt_tool_result_message(message)
        && !message.is_pruned_context_separator()
}

fn context_truncation_limit(messages: &[ChatMessage]) -> &'static str {
    if messages.iter().any(is_context_turn_boundary) {
        "only one complete user turn remains"
    } else {
        "history contains no complete user turn"
    }
}

/// Truncate conversation history at a user-turn boundary near the oldest half.
/// Returns the number of non-system messages dropped while keeping at least the
/// most recent complete turn and preserving tool calls with all of their
/// results.
fn truncate_for_context(messages: &mut Vec<ChatMessage>) -> usize {
    let non_system: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != "system")
        .map(|(i, _)| i)
        .collect();

    let turn_boundaries: Vec<usize> = non_system
        .iter()
        .enumerate()
        .filter_map(|(position, &message_index)| {
            is_context_turn_boundary(&messages[message_index]).then_some(position)
        })
        .collect();
    if turn_boundaries.len() <= 1 {
        return 0;
    }

    let target_drop = non_system.len() / 2;
    let Some(&last_boundary) = turn_boundaries.last() else {
        return 0;
    };
    let first_kept_position = turn_boundaries
        .iter()
        .copied()
        .skip(1)
        .find(|&position| position >= target_drop)
        .unwrap_or(last_boundary);
    let first_kept_index = non_system[first_kept_position];
    let mut original_index = 0usize;
    messages.retain(|message| {
        let keep = message.role == "system" || original_index >= first_kept_index;
        original_index += 1;
        keep
    });

    first_kept_position
}

fn push_failure(
    failures: &mut Vec<String>,
    provider_name: &str,
    model: &str,
    attempt: u32,
    max_attempts: u32,
    reason: &str,
    error_detail: &str,
    diagnostic: Option<&ProviderErrorDiagnostic>,
) {
    let mut failure = format!(
        "model_provider={provider_name} model={model} attempt {attempt}/{max_attempts}: {reason}; error={error_detail}"
    );
    if let Some(diagnostic) = diagnostic {
        failure.push_str(&format!(
            "; kind={}; phase={}; hint={}",
            diagnostic.kind, diagnostic.phase, diagnostic.hint
        ));
        if let Some(endpoint) = diagnostic.endpoint.as_deref() {
            failure.push_str(&format!("; endpoint={endpoint}"));
        }
    }
    failures.push(failure);
}

fn is_empty_completion(resp: &ChatResponse) -> bool {
    resp.text_or_empty().trim().is_empty()
        && resp.tool_calls.is_empty()
        && resp
            .reasoning_content
            .as_deref()
            .is_none_or(|r| r.trim().is_empty())
}

enum ReliableModelProviderEntryProvider {
    Direct(Box<dyn ModelProvider>),
    Pinned(crate::model_pin::ModelPinnedProvider),
}

impl ReliableModelProviderEntryProvider {
    fn as_model_provider(&self) -> &dyn ModelProvider {
        match self {
            Self::Direct(provider) => provider.as_ref(),
            Self::Pinned(provider) => provider,
        }
    }

    fn served_model<'a>(&'a self, requested_model: &'a str) -> &'a str {
        match self {
            Self::Direct(_) => requested_model,
            Self::Pinned(provider) => provider.pinned_model(),
        }
    }
}

pub(crate) struct ReliableModelProviderEntry {
    display_name: String,
    cooldown_key: String,
    provider: ReliableModelProviderEntryProvider,
}

impl ReliableModelProviderEntry {
    pub(crate) fn new(
        display_name: impl Into<String>,
        cooldown_key: impl Into<String>,
        provider: Box<dyn ModelProvider>,
    ) -> Self {
        Self {
            display_name: display_name.into(),
            cooldown_key: cooldown_key.into(),
            provider: ReliableModelProviderEntryProvider::Direct(provider),
        }
    }

    /// Build an entry that serves `pinned_model` regardless of the requested
    /// model. The [`crate::model_pin::ModelPinnedProvider`] wrapper is the
    /// source of truth for the pinned model; this entry reads it from the
    /// wrapper at use-time.
    pub(crate) fn new_pinned(
        display_name: impl Into<String>,
        cooldown_key: impl Into<String>,
        alias: &str,
        pinned_model: &str,
        inner: Box<dyn ModelProvider>,
    ) -> Self {
        Self {
            display_name: display_name.into(),
            cooldown_key: cooldown_key.into(),
            provider: ReliableModelProviderEntryProvider::Pinned(
                crate::model_pin::ModelPinnedProvider::builder(alias)
                    .pinned_model(pinned_model)
                    .inner(inner)
                    .build(),
            ),
        }
    }

    /// Model this entry serves for `requested_model`: the pinned model when
    /// the entry is model-pinned, otherwise the requested model unchanged.
    fn served_model<'a>(&'a self, requested_model: &'a str) -> &'a str {
        self.provider.served_model(requested_model)
    }

    fn provider(&self) -> &dyn ModelProvider {
        self.provider.as_model_provider()
    }
}

/// ModelProvider wrapper with retry + auth-key rotation. The model_provider Vec exists
/// for tests to exercise multi-provider failover; production wiring always
/// passes a single primary. Per-model failover chains are also test-only —
/// the schema no longer surfaces them.
pub struct ReliableModelProvider {
    /// `[providers.models.<family>.<alias>]` config-key alias.
    alias: String,
    model_providers: Vec<ReliableModelProviderEntry>,
    max_retries: u32,
    base_backoff_ms: u64,
    /// Extra API keys for rotation (index tracks round-robin position).
    api_keys: Vec<String>,
    key_index: AtomicUsize,
    /// Per-model failover chains. Test-only: model_name → [alt1, alt2, ...].
    model_fallbacks: HashMap<String, Vec<String>>,
    /// Transient provider cooldowns after retryable rate limits.
    /// Source of truth: live provider 429 / Retry-After evidence observed by
    /// this wrapper. It is intentionally in-memory and per wrapper instance.
    rate_limit_cooldowns: Mutex<HashMap<String, Instant>>,
    /// Per-key cooldowns, keyed by the API key itself. A key that just earned a
    /// 429 is parked here so round-robin cannot hand it straight back on the
    /// very next attempt.
    key_cooldowns: Mutex<HashMap<String, Instant>>,
    /// The key this wrapper last installed on a provider, so the next 429 knows
    /// which key to park. `None` means no rotation has happened yet and the
    /// provider is still on its construction-time credential.
    last_installed_key: Mutex<Option<String>>,
}

impl ReliableModelProvider {
    pub fn new(
        alias: &str,
        model_providers: Vec<(String, Box<dyn ModelProvider>)>,
        max_retries: u32,
        base_backoff_ms: u64,
    ) -> Self {
        let model_providers = model_providers
            .into_iter()
            .map(|(display_name, provider)| {
                ReliableModelProviderEntry::new(display_name.clone(), display_name, provider)
            })
            .collect();

        Self::new_with_entries(alias, model_providers, max_retries, base_backoff_ms)
    }

    pub(crate) fn new_with_entries(
        alias: &str,
        model_providers: Vec<ReliableModelProviderEntry>,
        max_retries: u32,
        base_backoff_ms: u64,
    ) -> Self {
        Self {
            alias: alias.to_string(),
            model_providers,
            max_retries,
            base_backoff_ms: base_backoff_ms.max(50),
            api_keys: Vec::new(),
            key_index: AtomicUsize::new(0),
            model_fallbacks: HashMap::new(),
            rate_limit_cooldowns: Mutex::new(HashMap::new()),
            key_cooldowns: Mutex::new(HashMap::new()),
            last_installed_key: Mutex::new(None),
        }
    }
    /// Set additional API keys for round-robin rotation on rate-limit errors.
    pub fn with_api_keys(mut self, keys: Vec<String>) -> Self {
        self.api_keys = keys;
        self
    }

    #[cfg(test)]
    pub fn with_model_fallbacks(mut self, fallbacks: HashMap<String, Vec<String>>) -> Self {
        self.model_fallbacks = fallbacks;
        self
    }

    /// Build the list of models to try: [original, alt1, alt2, ...]
    fn model_chain<'a>(&'a self, model: &'a str) -> Vec<&'a str> {
        let mut chain = vec![model];
        if let Some(fallbacks) = self.model_fallbacks.get(model) {
            chain.extend(fallbacks.iter().map(|s| s.as_str()));
        }
        chain
    }

    /// Advance to the next API key that is not inside a cooldown window.
    ///
    /// `None` means either no keys are configured or every configured key is
    /// still cooling. Callers must treat `None` as "no rotation happened" —
    /// handing back a cooling key would retry straight into the quota that
    /// just rejected us.
    fn rotate_key(&self) -> Option<String> {
        if self.api_keys.is_empty() {
            return None;
        }

        let now = Instant::now();
        let mut cooldowns = self
            .key_cooldowns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cooldowns.retain(|_, until| *until > now);

        // One full pass over the ring: every key gets considered exactly once,
        // and the index still advances so callers keep round-robining.
        (0..self.api_keys.len()).find_map(|_| {
            let idx = self.key_index.fetch_add(1, Ordering::Relaxed) % self.api_keys.len();
            let candidate = &self.api_keys[idx];
            (!cooldowns.contains_key(candidate)).then(|| candidate.clone())
        })
    }

    /// Park `key` for the same window the provider asked us to back off for.
    fn cool_down_key(&self, key: String, err: &anyhow::Error) {
        let cooldown = parse_retry_after_ms(err)
            .map(|ms| Duration::from_millis(ms.min(60_000)))
            .unwrap_or(Self::RATE_LIMIT_COOLDOWN);
        self.key_cooldowns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, Instant::now() + cooldown);
    }

    /// Rotate to a fresh API key and actually install it on `entry`'s provider.
    ///
    /// Returns `true` only when a key was selected *and* the provider reported
    /// that it applied it. `false` means the next attempt will reuse the
    /// current credential, so the caller must fall back to cooling the whole
    /// provider rather than retrying into the same exhausted quota.
    fn rotate_and_apply_key(
        &self,
        entry: &ReliableModelProviderEntry,
        err: &anyhow::Error,
        provider_name: &str,
        error_detail: &str,
    ) -> bool {
        if self.api_keys.is_empty() {
            return false;
        }

        // Park the key that just earned this 429 before picking the next one.
        let spent = self
            .last_installed_key
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(spent) = spent {
            self.cool_down_key(spent, err);
        }

        let Some(new_key) = self.rotate_key() else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "model_provider": provider_name,
                        "error": error_detail,
                        "api_keys": self.api_keys.len(),
                    })),
                "Rate limited; every configured API key is cooling down — not rotating"
            );
            return false;
        };

        let applied = entry.provider().set_credential(Some(new_key.clone()));
        let tail = &new_key[new_key.len().saturating_sub(4)..];
        if applied {
            *self
                .last_installed_key
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(new_key.clone());
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "model_provider": provider_name,
                        "error": error_detail,
                    })),
                &format!("Rate limited; rotated to API key ending ...{tail}")
            );
        } else {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({
                        "model_provider": provider_name,
                        "error": error_detail,
                    })),
                &format!(
                    "Rate limited; selected API key ending ...{tail} but this provider does not \
                     support runtime credential rotation — retrying with the original key"
                )
            );
        }
        applied
    }

    /// Compute backoff duration, respecting Retry-After if present.
    fn compute_backoff(&self, base: u64, err: &anyhow::Error) -> u64 {
        if let Some(retry_after) = parse_retry_after_ms(err) {
            // Use Retry-After but cap at 30s to avoid indefinite waits
            retry_after.min(30_000).max(base)
        } else {
            base
        }
    }

    /// Default cooldown after a retryable 429 when Retry-After is absent.
    const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(10);

    /// Returns whether a cooldown is active and prunes expired cooldowns.
    fn provider_cooldown_active(&self, cooldown_key: &str) -> bool {
        let now = Instant::now();
        let mut cooldowns = self
            .rate_limit_cooldowns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        match cooldowns.get(cooldown_key).copied() {
            Some(deadline) if now < deadline => true,
            Some(_) => {
                cooldowns.remove(cooldown_key);
                false
            }
            None => false,
        }
    }

    fn provider_should_skip_for_cooldown(&self, entry: &ReliableModelProviderEntry) -> bool {
        self.model_providers.len() > 1 && self.provider_cooldown_active(&entry.cooldown_key)
    }

    fn record_cooldown_skip_failure(failures: &mut Vec<String>, provider_name: &str, model: &str) {
        failures.push(format!(
            "model_provider={provider_name} model={model}: skipped; reason=rate_limit_cooldown"
        ));
    }

    fn log_cooldown_skip(&self, provider_name: &str) {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"model_provider": provider_name})),
            "Skipping model_provider during rate-limit cooldown"
        );
    }

    fn set_rate_limit_cooldown(&self, cooldown_key: &str, err: &anyhow::Error) -> Duration {
        let cooldown = parse_retry_after_ms(err)
            .map(|ms| Duration::from_millis(ms.min(60_000)))
            .unwrap_or(Self::RATE_LIMIT_COOLDOWN);

        let mut cooldowns = self
            .rate_limit_cooldowns
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cooldowns.insert(cooldown_key.to_string(), Instant::now() + cooldown);
        cooldown
    }

    fn cool_down_rate_limited_provider(
        &self,
        entry: &ReliableModelProviderEntry,
        model: &str,
        err: &anyhow::Error,
    ) -> Duration {
        let cooldown = self.set_rate_limit_cooldown(&entry.cooldown_key, err);
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(
                ::serde_json::json!({
                    "model_provider": entry.display_name,
                    "model": model,
                    "cooldown_ms": cooldown.as_millis(),
                })
            ),
            "ModelProvider rate-limited; trying next provider"
        );
        cooldown
    }

    /// Shared tail of the empty-completion retry path used by every chat method:
    /// record the empty attempt, warn, sleep the current backoff, then double it
    /// (capped). The caller keeps the emptiness check (it differs per return
    /// type) and the `continue`. See [`is_empty_completion`].
    async fn backoff_after_empty_completion(
        &self,
        failures: &mut Vec<String>,
        provider_name: &str,
        model: &str,
        attempt: u32,
        backoff_ms: &mut u64,
    ) {
        push_failure(
            failures,
            provider_name,
            model,
            attempt + 1,
            self.max_retries + 1,
            "empty_response",
            "model_provider returned an empty completion",
            None,
        );
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "model_provider": provider_name,
                    "model": model,
                    "attempt": attempt + 1,
                    "backoff_ms": *backoff_ms
                })),
            "Empty completion; retrying"
        );
        tokio::time::sleep(Duration::from_millis(*backoff_ms)).await;
        *backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
    }
}

#[async_trait]
impl ModelProvider for ReliableModelProvider {
    async fn warmup(&self) -> anyhow::Result<()> {
        for entry in &self.model_providers {
            let provider_name = entry.display_name.as_str();
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(::serde_json::json!({"model_provider": provider_name})),
                "Warming up model_provider connection pool"
            );
            if ProviderDispatch::from_ref(entry.provider())
                .warmup()
                .await
                .is_err()
            {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(::serde_json::json!({"model_provider": provider_name})),
                    "Warmup failed (non-fatal)"
                );
            }
        }
        Ok(())
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();

        // Outer: model fallback chain. Middle: model_provider priority. Inner: retries.
        // Each iteration: attempt one (model_provider, model) call. On success, return
        // immediately. On non-retryable error, break to next model_provider. On
        // retryable error, sleep with exponential backoff and retry.
        for current_model in &models {
            for entry in &self.model_providers {
                let provider_name = entry.display_name.as_str();
                if self.provider_should_skip_for_cooldown(entry) {
                    self.log_cooldown_skip(provider_name);
                    Self::record_cooldown_skip_failure(&mut failures, provider_name, current_model);
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;
                let mut last_error_detail: Option<String> = None;
                let mut last_diagnostic: Option<ProviderErrorDiagnostic> = None;

                for attempt in 0..=self.max_retries {
                    match ProviderDispatch::from_ref(entry.provider())
                        .chat_with_system(system_prompt, message, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            // Re-roll a transient empty completion instead of
                            // returning a blank turn (bounded by `max_retries`).
                            if attempt < self.max_retries && resp.trim().is_empty() {
                                self.backoff_after_empty_completion(
                                    &mut failures,
                                    provider_name,
                                    current_model,
                                    attempt,
                                    &mut backoff_ms,
                                )
                                .await;
                                continue;
                            }
                            let served_model = entry.served_model(current_model);
                            if attempt > 0
                                || served_model != model
                                || self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    != Some(provider_name)
                            {
                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": served_model, "attempt": attempt, "original_model": model})), "ModelProvider recovered (failover/retry)");
                                let primary = self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    .unwrap_or("");
                                record_provider_fallback(
                                    primary,
                                    model,
                                    provider_name,
                                    served_model,
                                );
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            // Context window exceeded: no history to truncate
                            // in chat_with_system, bail immediately.
                            if is_context_window_exceeded(&e) {
                                let error_detail = compact_error_detail(&e);
                                push_failure(
                                    &mut failures,
                                    provider_name,
                                    current_model,
                                    attempt + 1,
                                    self.max_retries + 1,
                                    "non_retryable",
                                    &error_detail,
                                    None,
                                );
                                anyhow::bail!(
                                    "Request exceeds model context window. Attempts:\n{}",
                                    failures.join("\n")
                                );
                            }

                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            let diagnostic = provider_error_diagnostic(&e);
                            last_error_detail = Some(error_detail.clone());
                            last_diagnostic = Some(diagnostic.clone());

                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                self.max_retries + 1,
                                failure_reason,
                                &error_detail,
                                Some(&diagnostic),
                            );

                            // Rate-limit with rotatable keys: cycle to the next API key
                            // so the retry hits a different quota bucket.
                            let rotated = rate_limited
                                && !non_retryable_rate_limit
                                && self.rotate_and_apply_key(
                                    entry,
                                    &e,
                                    provider_name,
                                    &error_detail,
                                );

                            if non_retryable {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        provider_failure_attrs(
                                            provider_name,
                                            current_model,
                                            &error_detail,
                                            &diagnostic,
                                        )
                                    ),
                                    "Non-retryable error, moving on"
                                );
                                break;
                            }

                            // A successful key swap already moved us to a fresh
                            // quota bucket — cooling the whole provider on top
                            // of that would strand a provider that is fine.
                            if rate_limited && !rotated && self.model_providers.len() > 1 {
                                self.cool_down_rate_limited_provider(entry, current_model, &e);
                                break;
                            }

                            if attempt < self.max_retries {
                                let wait = self.compute_backoff(backoff_ms, &e);
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        provider_retry_attrs(
                                            provider_name,
                                            current_model,
                                            attempt + 1,
                                            wait,
                                            failure_reason,
                                            &error_detail,
                                            &diagnostic,
                                        )
                                    ),
                                    "ModelProvider call failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_millis(wait)).await;
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            }
                        }
                    }
                }

                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(provider_exhausted_attrs(
                            provider_name,
                            current_model,
                            last_error_detail.as_deref(),
                            last_diagnostic.as_ref(),
                        )),
                    "Exhausted retries, trying next model_provider/model"
                );
            }

            if *current_model != model {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"original_model": model, "fallback_model": *current_model})), "Model fallback exhausted all model_providers, trying next fallback model");
            }
        }

        anyhow::bail!(
            "All model_providers/models failed. Attempts:\n{}",
            failures.join("\n")
        )
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<String> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();
        let mut effective_messages = messages.to_vec();
        let mut context_truncated = false;

        for current_model in &models {
            for entry in &self.model_providers {
                let provider_name = entry.display_name.as_str();
                if self.provider_should_skip_for_cooldown(entry) {
                    self.log_cooldown_skip(provider_name);
                    Self::record_cooldown_skip_failure(&mut failures, provider_name, current_model);
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;
                let mut last_error_detail: Option<String> = None;
                let mut last_diagnostic: Option<ProviderErrorDiagnostic> = None;

                for attempt in 0..=self.max_retries {
                    match ProviderDispatch::from_ref(entry.provider())
                        .chat_with_history(&effective_messages, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            // Re-roll a transient empty completion instead of
                            // returning a blank turn (bounded by `max_retries`).
                            if attempt < self.max_retries && resp.trim().is_empty() {
                                self.backoff_after_empty_completion(
                                    &mut failures,
                                    provider_name,
                                    current_model,
                                    attempt,
                                    &mut backoff_ms,
                                )
                                .await;
                                continue;
                            }
                            let served_model = entry.served_model(current_model);
                            if attempt > 0
                                || served_model != model
                                || context_truncated
                                || self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    != Some(provider_name)
                            {
                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": served_model, "attempt": attempt, "original_model": model, "context_truncated": context_truncated})), "ModelProvider recovered (failover/retry)");
                                let primary = self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    .unwrap_or("");
                                record_provider_fallback(
                                    primary,
                                    model,
                                    provider_name,
                                    served_model,
                                );
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            // Context window exceeded: truncate history and retry
                            if is_context_window_exceeded(&e) && !context_truncated {
                                let dropped = truncate_for_context(&mut effective_messages);
                                if dropped > 0 {
                                    context_truncated = true;
                                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": *current_model, "dropped": dropped, "remaining": effective_messages.len()})), "Context window exceeded; truncated history and retrying");
                                    continue; // Retry with truncated messages (counts as an attempt)
                                }
                                // No complete older turn can be removed safely.
                                let error_detail = compact_error_detail(&e);
                                let truncation_limit =
                                    context_truncation_limit(&effective_messages);
                                push_failure(
                                    &mut failures,
                                    provider_name,
                                    current_model,
                                    attempt + 1,
                                    self.max_retries + 1,
                                    "non_retryable",
                                    &error_detail,
                                    None,
                                );
                                anyhow::bail!(
                                    "Request exceeds model context window and cannot be reduced without \
                                     breaking message/tool pairing ({truncation_limit}). \
                                     Try using a model with a larger context window, reducing the number \
                                     of tools/skills, or enabling compact_context in config. Attempts:\n{}",
                                    failures.join("\n")
                                );
                            }

                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            let diagnostic = provider_error_diagnostic(&e);
                            last_error_detail = Some(error_detail.clone());
                            last_diagnostic = Some(diagnostic.clone());

                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                self.max_retries + 1,
                                failure_reason,
                                &error_detail,
                                Some(&diagnostic),
                            );

                            let rotated = rate_limited
                                && !non_retryable_rate_limit
                                && self.rotate_and_apply_key(
                                    entry,
                                    &e,
                                    provider_name,
                                    &error_detail,
                                );

                            if non_retryable {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        provider_failure_attrs(
                                            provider_name,
                                            current_model,
                                            &error_detail,
                                            &diagnostic,
                                        )
                                    ),
                                    "Non-retryable error, moving on"
                                );
                                break;
                            }

                            // A successful key swap already moved us to a fresh
                            // quota bucket — cooling the whole provider on top
                            // of that would strand a provider that is fine.
                            if rate_limited && !rotated && self.model_providers.len() > 1 {
                                self.cool_down_rate_limited_provider(entry, current_model, &e);
                                break;
                            }

                            if attempt < self.max_retries {
                                let wait = self.compute_backoff(backoff_ms, &e);
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        provider_retry_attrs(
                                            provider_name,
                                            current_model,
                                            attempt + 1,
                                            wait,
                                            failure_reason,
                                            &error_detail,
                                            &diagnostic,
                                        )
                                    ),
                                    "ModelProvider call failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_millis(wait)).await;
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            }
                        }
                    }
                }

                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(provider_exhausted_attrs(
                            provider_name,
                            current_model,
                            last_error_detail.as_deref(),
                            last_diagnostic.as_ref(),
                        )),
                    "Exhausted retries, trying next model_provider/model"
                );
            }
        }

        anyhow::bail!(
            "All model_providers/models failed. Attempts:\n{}",
            failures.join("\n")
        )
    }

    fn capabilities(&self) -> crate::traits::ProviderCapabilities {
        let mut capabilities = self
            .model_providers
            .first()
            .map(|entry| entry.provider().capabilities())
            .unwrap_or_default();
        // A request may advance past the primary after a retryable failure.
        // Report vision only when every reachable provider can accept images;
        // otherwise the turn engine must select a dedicated vision route before
        // dispatch instead of admitting an image that a fallback could reject.
        capabilities.vision = !self.model_providers.is_empty()
            && self
                .model_providers
                .iter()
                .all(|entry| entry.provider().supports_vision());
        capabilities
    }

    fn capabilities_for_model(&self, model: &str) -> crate::traits::ProviderCapabilities {
        let mut capabilities = self
            .model_providers
            .first()
            .map(|entry| entry.provider().capabilities_for_model(model))
            .unwrap_or_default();
        capabilities.vision = !self.model_providers.is_empty()
            && self
                .model_providers
                .iter()
                .all(|entry| entry.provider().capabilities_for_model(model).vision);
        capabilities
    }

    fn supports_native_tools(&self) -> bool {
        self.model_providers
            .first()
            .map(|entry| entry.provider().supports_native_tools())
            .unwrap_or(false)
    }

    fn supports_vision(&self) -> bool {
        self.capabilities().vision
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();
        let mut effective_messages = messages.to_vec();
        let mut context_truncated = false;

        for current_model in &models {
            for entry in &self.model_providers {
                let provider_name = entry.display_name.as_str();
                if self.provider_should_skip_for_cooldown(entry) {
                    self.log_cooldown_skip(provider_name);
                    Self::record_cooldown_skip_failure(&mut failures, provider_name, current_model);
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;
                let mut last_error_detail: Option<String> = None;
                let mut last_diagnostic: Option<ProviderErrorDiagnostic> = None;

                for attempt in 0..=self.max_retries {
                    match ProviderDispatch::from_ref(entry.provider())
                        .chat_with_tools(&effective_messages, tools, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            // Re-roll a transient empty completion instead of
                            // returning a blank turn (bounded by `max_retries`;
                            // see `is_empty_completion`).
                            if attempt < self.max_retries && is_empty_completion(&resp) {
                                self.backoff_after_empty_completion(
                                    &mut failures,
                                    provider_name,
                                    current_model,
                                    attempt,
                                    &mut backoff_ms,
                                )
                                .await;
                                continue;
                            }
                            let served_model = entry.served_model(current_model);
                            if attempt > 0
                                || served_model != model
                                || context_truncated
                                || self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    != Some(provider_name)
                            {
                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": served_model, "attempt": attempt, "original_model": model, "context_truncated": context_truncated})), "ModelProvider recovered (failover/retry)");
                                let primary = self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    .unwrap_or("");
                                record_provider_fallback(
                                    primary,
                                    model,
                                    provider_name,
                                    served_model,
                                );
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            // Context window exceeded: truncate history and retry
                            if is_context_window_exceeded(&e) && !context_truncated {
                                let dropped = truncate_for_context(&mut effective_messages);
                                if dropped > 0 {
                                    context_truncated = true;
                                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": *current_model, "dropped": dropped, "remaining": effective_messages.len()})), "Context window exceeded; truncated history and retrying");
                                    continue; // Retry with truncated messages (counts as an attempt)
                                }
                                // No complete older turn can be removed safely.
                                let error_detail = compact_error_detail(&e);
                                let truncation_limit =
                                    context_truncation_limit(&effective_messages);
                                push_failure(
                                    &mut failures,
                                    provider_name,
                                    current_model,
                                    attempt + 1,
                                    self.max_retries + 1,
                                    "non_retryable",
                                    &error_detail,
                                    None,
                                );
                                anyhow::bail!(
                                    "Request exceeds model context window and cannot be reduced without \
                                     breaking message/tool pairing ({truncation_limit}). \
                                     Try using a model with a larger context window, reducing the number \
                                     of tools/skills, or enabling compact_context in config. Attempts:\n{}",
                                    failures.join("\n")
                                );
                            }

                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            let diagnostic = provider_error_diagnostic(&e);
                            last_error_detail = Some(error_detail.clone());
                            last_diagnostic = Some(diagnostic.clone());

                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                self.max_retries + 1,
                                failure_reason,
                                &error_detail,
                                Some(&diagnostic),
                            );

                            let rotated = rate_limited
                                && !non_retryable_rate_limit
                                && self.rotate_and_apply_key(
                                    entry,
                                    &e,
                                    provider_name,
                                    &error_detail,
                                );

                            if non_retryable {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        provider_failure_attrs(
                                            provider_name,
                                            current_model,
                                            &error_detail,
                                            &diagnostic,
                                        )
                                    ),
                                    "Non-retryable error, moving on"
                                );
                                break;
                            }

                            // A successful key swap already moved us to a fresh
                            // quota bucket — cooling the whole provider on top
                            // of that would strand a provider that is fine.
                            if rate_limited && !rotated && self.model_providers.len() > 1 {
                                self.cool_down_rate_limited_provider(entry, current_model, &e);
                                break;
                            }

                            if attempt < self.max_retries {
                                let wait = self.compute_backoff(backoff_ms, &e);
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        provider_retry_attrs(
                                            provider_name,
                                            current_model,
                                            attempt + 1,
                                            wait,
                                            failure_reason,
                                            &error_detail,
                                            &diagnostic,
                                        )
                                    ),
                                    "ModelProvider call failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_millis(wait)).await;
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            }
                        }
                    }
                }

                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(provider_exhausted_attrs(
                            provider_name,
                            current_model,
                            last_error_detail.as_deref(),
                            last_diagnostic.as_ref(),
                        )),
                    "Exhausted retries, trying next model_provider/model"
                );
            }
        }

        anyhow::bail!(
            "All model_providers/models failed. Attempts:\n{}",
            failures.join("\n")
        )
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
    ) -> anyhow::Result<ChatResponse> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();
        let mut effective_messages = request.messages.to_vec();
        let mut context_truncated = false;

        for current_model in &models {
            for entry in &self.model_providers {
                let provider_name = entry.display_name.as_str();
                if self.provider_should_skip_for_cooldown(entry) {
                    self.log_cooldown_skip(provider_name);
                    Self::record_cooldown_skip_failure(&mut failures, provider_name, current_model);
                    continue;
                }

                let mut backoff_ms = self.base_backoff_ms;
                let mut last_error_detail: Option<String> = None;
                let mut last_diagnostic: Option<ProviderErrorDiagnostic> = None;

                for attempt in 0..=self.max_retries {
                    let req = ChatRequest {
                        messages: &effective_messages,
                        tools: request.tools,
                        thinking: request.thinking,
                    };
                    match ProviderDispatch::from_ref(entry.provider())
                        .chat(req, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            // Re-roll a transient empty completion instead of
                            // returning a blank turn (bounded by `max_retries`;
                            // see `is_empty_completion`).
                            if attempt < self.max_retries && is_empty_completion(&resp) {
                                self.backoff_after_empty_completion(
                                    &mut failures,
                                    provider_name,
                                    current_model,
                                    attempt,
                                    &mut backoff_ms,
                                )
                                .await;
                                continue;
                            }
                            let served_model = entry.served_model(current_model);
                            if attempt > 0
                                || served_model != model
                                || context_truncated
                                || self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    != Some(provider_name)
                            {
                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": served_model, "attempt": attempt, "original_model": model, "context_truncated": context_truncated})), "ModelProvider recovered (failover/retry)");
                                let primary = self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    .unwrap_or("");
                                record_provider_fallback(
                                    primary,
                                    model,
                                    provider_name,
                                    served_model,
                                );
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            // Context window exceeded: truncate history and retry
                            if is_context_window_exceeded(&e) && !context_truncated {
                                let dropped = truncate_for_context(&mut effective_messages);
                                if dropped > 0 {
                                    context_truncated = true;
                                    ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": *current_model, "dropped": dropped, "remaining": effective_messages.len()})), "Context window exceeded; truncated history and retrying");
                                    continue; // Retry with truncated messages (counts as an attempt)
                                }
                                // No complete older turn can be removed safely.
                                let error_detail = compact_error_detail(&e);
                                let truncation_limit =
                                    context_truncation_limit(&effective_messages);
                                push_failure(
                                    &mut failures,
                                    provider_name,
                                    current_model,
                                    attempt + 1,
                                    self.max_retries + 1,
                                    "non_retryable",
                                    &error_detail,
                                    None,
                                );
                                anyhow::bail!(
                                    "Request exceeds model context window and cannot be reduced without \
                                     breaking message/tool pairing ({truncation_limit}). \
                                     Try using a model with a larger context window, reducing the number \
                                     of tools/skills, or enabling compact_context in config. Attempts:\n{}",
                                    failures.join("\n")
                                );
                            }

                            let non_retryable_rate_limit = is_non_retryable_rate_limit(&e);
                            let non_retryable = is_non_retryable(&e) || non_retryable_rate_limit;
                            let rate_limited = is_rate_limited(&e);
                            let failure_reason = failure_reason(rate_limited, non_retryable);
                            let error_detail = compact_error_detail(&e);
                            let diagnostic = provider_error_diagnostic(&e);
                            last_error_detail = Some(error_detail.clone());
                            last_diagnostic = Some(diagnostic.clone());

                            push_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                                attempt + 1,
                                self.max_retries + 1,
                                failure_reason,
                                &error_detail,
                                Some(&diagnostic),
                            );

                            let rotated = rate_limited
                                && !non_retryable_rate_limit
                                && self.rotate_and_apply_key(
                                    entry,
                                    &e,
                                    provider_name,
                                    &error_detail,
                                );

                            if non_retryable {
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        provider_failure_attrs(
                                            provider_name,
                                            current_model,
                                            &error_detail,
                                            &diagnostic,
                                        )
                                    ),
                                    "Non-retryable error, moving on"
                                );
                                break;
                            }

                            // A successful key swap already moved us to a fresh
                            // quota bucket — cooling the whole provider on top
                            // of that would strand a provider that is fine.
                            if rate_limited && !rotated && self.model_providers.len() > 1 {
                                self.cool_down_rate_limited_provider(entry, current_model, &e);
                                break;
                            }

                            if attempt < self.max_retries {
                                let wait = self.compute_backoff(backoff_ms, &e);
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        provider_retry_attrs(
                                            provider_name,
                                            current_model,
                                            attempt + 1,
                                            wait,
                                            failure_reason,
                                            &error_detail,
                                            &diagnostic,
                                        )
                                    ),
                                    "ModelProvider call failed, retrying"
                                );
                                tokio::time::sleep(Duration::from_millis(wait)).await;
                                backoff_ms = (backoff_ms.saturating_mul(2)).min(10_000);
                            }
                        }
                    }
                }

                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                        .with_attrs(provider_exhausted_attrs(
                            provider_name,
                            current_model,
                            last_error_detail.as_deref(),
                            last_diagnostic.as_ref(),
                        )),
                    "Exhausted retries, trying next model_provider/model"
                );
            }

            if *current_model != model {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"original_model": model, "fallback_model": *current_model})), "Model fallback exhausted all model_providers, trying next fallback model");
            }
        }

        anyhow::bail!(
            "All model_providers/models failed. Attempts:\n{}",
            failures.join("\n")
        )
    }

    fn supports_streaming(&self) -> bool {
        self.model_providers
            .iter()
            .any(|entry| entry.provider().supports_streaming())
    }

    fn supports_streaming_tool_events(&self) -> bool {
        self.model_providers
            .iter()
            .any(|entry| entry.provider().supports_streaming_tool_events())
    }

    fn stream_chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamEvent>> {
        let needs_tool_events = request.tools.is_some_and(|tools| !tools.is_empty());

        for entry in &self.model_providers {
            let provider_name = entry.display_name.as_str();
            let model_provider = entry.provider();
            if !model_provider.supports_streaming() || !options.enabled {
                continue;
            }

            if needs_tool_events && !model_provider.supports_streaming_tool_events() {
                continue;
            }

            if self.provider_should_skip_for_cooldown(entry) {
                self.log_cooldown_skip(provider_name);
                continue;
            }

            let provider_clone = provider_name.to_string();

            let current_model = self
                .model_chain(model)
                .first()
                .copied()
                .unwrap_or(model)
                .to_string();
            let served_model = entry.served_model(&current_model).to_string();
            let fallback_record = ProviderFallbackRecord::new_if_recovered(
                self.model_providers
                    .first()
                    .map(|entry| entry.display_name.as_str())
                    .unwrap_or(""),
                model,
                provider_name,
                &served_model,
            );

            let req = ChatRequest {
                messages: request.messages,
                tools: request.tools,
                thinking: request.thinking,
            };
            let stream = ProviderDispatch::from_ref(model_provider).stream_chat(
                req,
                &current_model,
                temperature,
                options,
            );
            let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamEvent>>(100);

            let handle = ::zeroclaw_spawn::spawn!(async move {
                let mut stream = stream;
                while let Some(event) = stream.next().await {
                    if let Err(ref e) = event {
                        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": provider_clone, "model": current_model, "e": e.to_string()})), "Streaming error: ");
                    }
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            });

            let guard = AbortOnDrop::new(handle.abort_handle());
            return stream_with_success_recording(rx, guard, fallback_record, |event| {
                matches!(event, StreamEvent::Final)
            });
        }

        let message = if needs_tool_events {
            "No model_provider supports streaming tool events".to_string()
        } else {
            "No model_provider supports streaming".to_string()
        };
        stream::once(async move { Err(super::traits::StreamError::ModelProvider(message)) }).boxed()
    }

    fn stream_chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        // Try each model_provider/model combination for streaming
        // For streaming, we use the first model_provider that supports it and has streaming enabled
        for entry in &self.model_providers {
            let provider_name = entry.display_name.as_str();
            let model_provider = entry.provider();
            if !model_provider.supports_streaming() || !options.enabled {
                continue;
            }

            if self.provider_should_skip_for_cooldown(entry) {
                self.log_cooldown_skip(provider_name);
                continue;
            }

            // Clone model_provider data for the stream
            let provider_clone = provider_name.to_string();

            // Try the first model in the chain for streaming
            let current_model = match self.model_chain(model).first() {
                Some(m) => (*m).to_string(),
                None => model.to_string(),
            };
            let served_model = entry.served_model(&current_model).to_string();
            let fallback_record = ProviderFallbackRecord::new_if_recovered(
                self.model_providers
                    .first()
                    .map(|entry| entry.display_name.as_str())
                    .unwrap_or(""),
                model,
                provider_name,
                &served_model,
            );

            // For streaming, we attempt once and propagate errors
            // The caller can retry the entire request if needed
            let stream = model_provider.stream_chat_with_system(
                system_prompt,
                message,
                &current_model,
                temperature,
                options,
            );

            // Use a channel to bridge the stream with logging
            let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);

            let handle = ::zeroclaw_spawn::spawn!(async move {
                let mut stream = stream;
                while let Some(chunk) = stream.next().await {
                    if let Err(ref e) = chunk {
                        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": provider_clone, "model": current_model, "e": e.to_string()})), "Streaming error: ");
                    }
                    if tx.send(chunk).await.is_err() {
                        break; // Receiver dropped
                    }
                }
            });

            // Convert channel receiver to stream
            let guard = AbortOnDrop::new(handle.abort_handle());
            return stream_with_success_recording(rx, guard, fallback_record, |chunk| {
                chunk.is_final
            });
        }

        // No streaming support available
        stream::once(async move {
            Err(super::traits::StreamError::ModelProvider(
                "No model_provider supports streaming".to_string(),
            ))
        })
        .boxed()
    }

    fn stream_chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: Option<f64>,
        options: StreamOptions,
    ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
        // Try each model_provider/model combination for streaming with history.
        // Mirrors stream_chat_with_system but delegates to the underlying
        // model_provider's stream_chat_with_history, preserving the full conversation.
        for entry in &self.model_providers {
            let provider_name = entry.display_name.as_str();
            let model_provider = entry.provider();
            if !model_provider.supports_streaming() || !options.enabled {
                continue;
            }

            if self.provider_should_skip_for_cooldown(entry) {
                self.log_cooldown_skip(provider_name);
                continue;
            }

            let provider_clone = provider_name.to_string();

            let current_model = match self.model_chain(model).first() {
                Some(m) => (*m).to_string(),
                None => model.to_string(),
            };
            let served_model = entry.served_model(&current_model).to_string();
            let fallback_record = ProviderFallbackRecord::new_if_recovered(
                self.model_providers
                    .first()
                    .map(|entry| entry.display_name.as_str())
                    .unwrap_or(""),
                model,
                provider_name,
                &served_model,
            );

            let stream = model_provider.stream_chat_with_history(
                messages,
                &current_model,
                temperature,
                options,
            );

            let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);

            let handle = ::zeroclaw_spawn::spawn!(async move {
                let mut stream = stream;
                while let Some(chunk) = stream.next().await {
                    if let Err(ref e) = chunk {
                        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"model_provider": provider_clone, "model": current_model, "e": e.to_string()})), "Streaming error: ");
                    }
                    if tx.send(chunk).await.is_err() {
                        break; // Receiver dropped
                    }
                }
            });

            let guard = AbortOnDrop::new(handle.abort_handle());
            return stream_with_success_recording(rx, guard, fallback_record, |chunk| {
                chunk.is_final
            });
        }

        // No streaming support available
        stream::once(async move {
            Err(super::traits::StreamError::ModelProvider(
                "No model_provider supports streaming".to_string(),
            ))
        })
        .boxed()
    }
}

impl ::zeroclaw_api::attribution::Attributable for ReliableModelProvider {
    fn role(&self) -> ::zeroclaw_api::attribution::Role {
        match self.model_providers.first() {
            Some(entry) => ::zeroclaw_api::attribution::Attributable::role(entry.provider()),
            None => ::zeroclaw_api::attribution::Role::System,
        }
    }

    fn alias(&self) -> &str {
        // Delegate to the primary inner provider for the same reason
        // as `role()`. Falls back to the wrapper's own configured alias
        // when no inner provider is registered.
        match self.model_providers.first() {
            Some(entry) => ::zeroclaw_api::attribution::Attributable::alias(entry.provider()),
            None => &self.alias,
        }
    }
}

#[cfg(test)]
mod tests;
