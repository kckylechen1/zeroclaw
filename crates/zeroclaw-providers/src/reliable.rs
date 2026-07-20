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
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Identity of a credential slot on one physical provider/credential pool.
///
/// `Primary` plus optional `Extra(i)` from `[reliability].api_keys`. Cooldown
/// identity is `(cooldown_key, CredentialIdentity)` so model-pinned entries that
/// share the same physical provider/credential pool cool together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CredentialIdentity {
    Primary,
    Extra(usize),
}

impl CredentialIdentity {
    fn label(self) -> String {
        match self {
            Self::Primary => "primary".to_string(),
            Self::Extra(i) => format!("extra:{i}"),
        }
    }
}

/// Builds a provider for one credential attempt from canonical live config.
///
/// Source of truth for secrets is the live config (or test factory); this
/// wrapper does **not** retain API key copies across attempts.
pub(crate) type CredentialFactory =
    Arc<dyn Fn() -> anyhow::Result<Box<dyn ModelProvider>> + Send + Sync>;

/// One credential identity with an on-demand factory (no long-lived key copy).
struct CredentialSlot {
    identity: CredentialIdentity,
    factory: CredentialFactory,
}

/// Live credential cooldowns (429 evidence), keyed by physical provider scope
/// (`cooldown_key`) + credential identity — not per model-pin entry index.
#[derive(Default)]
struct CredentialRotationState {
    by_scope: HashMap<(String, CredentialIdentity), Instant>,
}

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

/// `anyhow::Error` does not carry typed HTTP `Retry-After` metadata. Do not
/// scrape error strings and pretend they are response headers — cooldown and
/// backoff use the wrapper's default durations instead.

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

/// Truncate conversation history by dropping the oldest non-system messages.
/// Returns the number of messages dropped. Keeps at least the system message
/// (if any) and the most recent user message.
fn truncate_for_context(messages: &mut Vec<ChatMessage>) -> usize {
    // Find all non-system message indices
    let non_system: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != "system")
        .map(|(i, _)| i)
        .collect();

    // Keep at least the last non-system message (most recent user turn)
    if non_system.len() <= 1 {
        return 0;
    }

    // Drop the oldest half of non-system messages
    let drop_count = non_system.len() / 2;
    let indices_to_remove: Vec<usize> = non_system[..drop_count].to_vec();

    // Remove in reverse order to preserve indices
    for &idx in indices_to_remove.iter().rev() {
        messages.remove(idx);
    }

    drop_count
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

pub(crate) struct ReliableModelProviderEntry {
    display_name: String,
    /// Physical provider/credential pool id (`family.alias`). Shared across
    /// model-pinned entries so cooldown identity stays stable.
    cooldown_key: String,
    /// Primary first, then Extra(i). Each slot is an on-demand factory that
    /// resolves secrets from live canonical config at attempt time.
    credentials: Vec<CredentialSlot>,
}

/// Wrap a constructed provider so each attempt gets an `Arc` handle without
/// retaining a raw key string on the reliable wrapper.
fn factory_from_provider(provider: Box<dyn ModelProvider>) -> CredentialFactory {
    let shared: Arc<dyn ModelProvider> = Arc::from(provider);
    Arc::new(move || Ok(Box::new(Arc::clone(&shared)) as Box<dyn ModelProvider>))
}

impl ReliableModelProviderEntry {
    pub(crate) fn new(
        display_name: impl Into<String>,
        cooldown_key: impl Into<String>,
        primary: CredentialFactory,
    ) -> Self {
        Self {
            display_name: display_name.into(),
            cooldown_key: cooldown_key.into(),
            credentials: vec![CredentialSlot {
                identity: CredentialIdentity::Primary,
                factory: primary,
            }],
        }
    }

    /// Build an entry from already-constructed providers (tests / transitional
    /// call sites). Production wiring prefers live-config factories so key
    /// material is not retained outside canonical config.
    pub(crate) fn from_providers(
        display_name: impl Into<String>,
        cooldown_key: impl Into<String>,
        primary: Box<dyn ModelProvider>,
        extras: Vec<Box<dyn ModelProvider>>,
    ) -> Self {
        Self::new(display_name, cooldown_key, factory_from_provider(primary))
            .with_extra_factories(extras.into_iter().map(factory_from_provider).collect())
    }

    /// Attach Extra(i) factories built from `[reliability].api_keys` indices.
    pub(crate) fn with_extra_factories(mut self, extras: Vec<CredentialFactory>) -> Self {
        for (i, factory) in extras.into_iter().enumerate() {
            self.credentials.push(CredentialSlot {
                identity: CredentialIdentity::Extra(i),
                factory,
            });
        }
        self
    }

    fn build_credential(
        &self,
        identity: CredentialIdentity,
    ) -> anyhow::Result<Box<dyn ModelProvider>> {
        self.credentials
            .iter()
            .find(|slot| slot.identity == identity)
            .map(|slot| (slot.factory)())
            .unwrap_or_else(|| anyhow::bail!("unknown credential identity"))
    }

    fn build_primary(&self) -> anyhow::Result<Box<dyn ModelProvider>> {
        self.build_credential(CredentialIdentity::Primary)
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
    /// Per-entry credential cooldowns (Primary|Extra(i)). SoT: live 429s.
    credential_rotation: Mutex<CredentialRotationState>,
    /// Per-model failover chains. Test-only: model_name → [alt1, alt2, ...].
    model_fallbacks: HashMap<String, Vec<String>>,
    /// Transient provider cooldowns after retryable rate limits.
    /// Source of truth: live provider 429 / Retry-After evidence observed by
    /// this wrapper. It is intentionally in-memory and per wrapper instance.
    rate_limit_cooldowns: Mutex<HashMap<String, Instant>>,
    /// Test-only: (cooldown_key, identity) actually applied on each attempt.
    #[cfg(test)]
    applied_credentials: Mutex<Vec<(String, CredentialIdentity)>>,
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
                ReliableModelProviderEntry::from_providers(
                    display_name.clone(),
                    display_name,
                    provider,
                    Vec::new(),
                )
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
            credential_rotation: Mutex::new(CredentialRotationState::default()),
            model_fallbacks: HashMap::new(),
            rate_limit_cooldowns: Mutex::new(HashMap::new()),
            #[cfg(test)]
            applied_credentials: Mutex::new(Vec::new()),
        }
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

    /// Default cooldown for a credential after a retryable 429.
    /// Not derived from HTTP `Retry-After` — that metadata is unavailable
    /// through `anyhow::Error`.
    const CREDENTIAL_COOLDOWN: Duration = Duration::from_secs(10);

    /// Select a non-cooled credential for this entry's physical scope.
    /// Prefers Primary, then Extra(i). Never returns a cooled identity.
    fn select_live_credential(
        &self,
        entry: &ReliableModelProviderEntry,
    ) -> Option<CredentialIdentity> {
        let mut state = self
            .credential_rotation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        state.by_scope.retain(|_, deadline| *deadline > now);

        for slot in &entry.credentials {
            let key = (entry.cooldown_key.clone(), slot.identity);
            if !state.by_scope.contains_key(&key) {
                return Some(slot.identity);
            }
        }
        None
    }

    /// Cool the credential that actually returned 429 for this physical scope.
    fn cool_credential(
        &self,
        entry: &ReliableModelProviderEntry,
        identity: CredentialIdentity,
        _err: &anyhow::Error,
    ) -> Duration {
        let cooldown = Self::CREDENTIAL_COOLDOWN;
        let mut state = self
            .credential_rotation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.by_scope.insert(
            (entry.cooldown_key.clone(), identity),
            Instant::now() + cooldown,
        );
        cooldown
    }

    #[cfg(test)]
    fn credential_cooldown_active(&self, cooldown_key: &str, identity: CredentialIdentity) -> bool {
        let state = self
            .credential_rotation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .by_scope
            .get(&(cooldown_key.to_string(), identity))
            .is_some_and(|deadline| Instant::now() < *deadline)
    }

    #[cfg(test)]
    fn take_applied_credentials(&self) -> Vec<(String, CredentialIdentity)> {
        std::mem::take(
            &mut *self
                .applied_credentials
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    #[cfg(test)]
    fn record_applied_credential(&self, cooldown_key: &str, identity: CredentialIdentity) {
        self.applied_credentials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push((cooldown_key.to_string(), identity));
    }

    /// Resolve a live credential for this attempt, or report all-cooled.
    /// Constructs the provider on demand from the credential factory.
    fn begin_credential_attempt(
        &self,
        entry: &ReliableModelProviderEntry,
    ) -> Result<(CredentialIdentity, Box<dyn ModelProvider>), ()> {
        let Some(identity) = self.select_live_credential(entry) else {
            return Err(());
        };
        let provider = entry.build_credential(identity).map_err(|_| ())?;
        #[cfg(test)]
        self.record_applied_credential(&entry.cooldown_key, identity);
        Ok((identity, provider))
    }

    /// Compute backoff duration. Typed HTTP `Retry-After` is not available on
    /// `anyhow::Error`, so this uses exponential base only.
    fn compute_backoff(&self, base: u64, _err: &anyhow::Error) -> u64 {
        base
    }

    /// Default provider-level cooldown after retryable 429 when failing over
    /// across distinct provider entries. Not an HTTP Retry-After honor path.
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

    fn set_rate_limit_cooldown(&self, cooldown_key: &str, _err: &anyhow::Error) -> Duration {
        let cooldown = Self::RATE_LIMIT_COOLDOWN;
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
            for slot in &entry.credentials {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_attrs(::serde_json::json!({
                            "model_provider": provider_name,
                            "credential": slot.identity.label(),
                        })),
                    "Warming up model_provider connection pool"
                );
                let Ok(provider) = (slot.factory)() else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "model_provider": provider_name,
                                "credential": slot.identity.label(),
                            })),
                        "Warmup skipped; credential factory failed"
                    );
                    continue;
                };
                if ProviderDispatch::from_ref(provider.as_ref())
                    .warmup()
                    .await
                    .is_err()
                {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "model_provider": provider_name,
                                "credential": slot.identity.label(),
                            })),
                        "Warmup failed (non-fatal)"
                    );
                }
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
                    let (applied_identity, provider) = match self.begin_credential_attempt(entry) {
                        Ok(v) => v,
                        Err(()) => {
                            Self::record_cooldown_skip_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                            );
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "model_provider": provider_name,
                                    "model": *current_model,
                                })),
                                "All credentials cooled for model_provider; unavailable"
                            );
                            break;
                        }
                    };
                    match ProviderDispatch::from_ref(provider.as_ref())
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
                            if attempt > 0
                                || *current_model != model
                                || self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    != Some(provider_name)
                            {
                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": *current_model, "attempt": attempt, "original_model": model})), "ModelProvider recovered (failover/retry)");
                                let primary = self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    .unwrap_or("");
                                record_provider_fallback(
                                    primary,
                                    model,
                                    provider_name,
                                    current_model,
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

                            let mut entry_credentials_exhausted = false;
                            if rate_limited && !non_retryable_rate_limit {
                                let cooldown = self.cool_credential(entry, applied_identity, &e);
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "model_provider": provider_name,
                                            "credential": applied_identity.label(),
                                            "cooldown_ms": cooldown.as_millis(),
                                            "error": error_detail,
                                        })
                                    ),
                                    "Rate limited; cooling credential and retrying with next live key"
                                );
                                if self.select_live_credential(entry).is_none() {
                                    entry_credentials_exhausted = true;
                                    Self::record_cooldown_skip_failure(
                                        &mut failures,
                                        provider_name,
                                        current_model,
                                    );
                                }
                            }

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

                            if entry_credentials_exhausted {
                                if self.model_providers.len() > 1 {
                                    self.cool_down_rate_limited_provider(entry, current_model, &e);
                                }
                                break;
                            }

                            if attempt < self.max_retries {
                                // Retry-After applies to the cooled credential.
                                // When another live credential remains, try it
                                // promptly instead of waiting out that cool-down.
                                let wait = if rate_limited
                                    && !non_retryable_rate_limit
                                    && self.select_live_credential(entry).is_some()
                                {
                                    self.base_backoff_ms
                                } else {
                                    self.compute_backoff(backoff_ms, &e)
                                };
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
                    let (applied_identity, provider) = match self.begin_credential_attempt(entry) {
                        Ok(v) => v,
                        Err(()) => {
                            Self::record_cooldown_skip_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                            );
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "model_provider": provider_name,
                                    "model": *current_model,
                                })),
                                "All credentials cooled for model_provider; unavailable"
                            );
                            break;
                        }
                    };
                    match ProviderDispatch::from_ref(provider.as_ref())
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
                            if attempt > 0
                                || *current_model != model
                                || context_truncated
                                || self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    != Some(provider_name)
                            {
                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": *current_model, "attempt": attempt, "original_model": model, "context_truncated": context_truncated})), "ModelProvider recovered (failover/retry)");
                                let primary = self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    .unwrap_or("");
                                record_provider_fallback(
                                    primary,
                                    model,
                                    provider_name,
                                    current_model,
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
                                // Nothing to truncate (system prompt alone exceeds
                                // the model's context window) — bail immediately
                                // instead of wasting retry attempts.
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
                                    "Request exceeds model context window and cannot be reduced further. \
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

                            let mut entry_credentials_exhausted = false;
                            if rate_limited && !non_retryable_rate_limit {
                                let cooldown = self.cool_credential(entry, applied_identity, &e);
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "model_provider": provider_name,
                                            "credential": applied_identity.label(),
                                            "cooldown_ms": cooldown.as_millis(),
                                            "error": error_detail,
                                        })
                                    ),
                                    "Rate limited; cooling credential and retrying with next live key"
                                );
                                if self.select_live_credential(entry).is_none() {
                                    entry_credentials_exhausted = true;
                                    Self::record_cooldown_skip_failure(
                                        &mut failures,
                                        provider_name,
                                        current_model,
                                    );
                                }
                            }

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

                            if entry_credentials_exhausted {
                                if self.model_providers.len() > 1 {
                                    self.cool_down_rate_limited_provider(entry, current_model, &e);
                                }
                                break;
                            }

                            if attempt < self.max_retries {
                                // Retry-After applies to the cooled credential.
                                // When another live credential remains, try it
                                // promptly instead of waiting out that cool-down.
                                let wait = if rate_limited
                                    && !non_retryable_rate_limit
                                    && self.select_live_credential(entry).is_some()
                                {
                                    self.base_backoff_ms
                                } else {
                                    self.compute_backoff(backoff_ms, &e)
                                };
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

    fn supports_native_tools(&self) -> bool {
        self.model_providers
            .first()
            .and_then(|entry| entry.build_primary().ok())
            .is_some_and(|p| p.supports_native_tools())
    }

    fn supports_vision(&self) -> bool {
        self.model_providers
            .first()
            .and_then(|entry| entry.build_primary().ok())
            .is_some_and(|p| p.supports_vision())
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
                    let (applied_identity, provider) = match self.begin_credential_attempt(entry) {
                        Ok(v) => v,
                        Err(()) => {
                            Self::record_cooldown_skip_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                            );
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "model_provider": provider_name,
                                    "model": *current_model,
                                })),
                                "All credentials cooled for model_provider; unavailable"
                            );
                            break;
                        }
                    };
                    match ProviderDispatch::from_ref(provider.as_ref())
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
                            if attempt > 0
                                || *current_model != model
                                || context_truncated
                                || self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    != Some(provider_name)
                            {
                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": *current_model, "attempt": attempt, "original_model": model, "context_truncated": context_truncated})), "ModelProvider recovered (failover/retry)");
                                let primary = self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    .unwrap_or("");
                                record_provider_fallback(
                                    primary,
                                    model,
                                    provider_name,
                                    current_model,
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
                                // Nothing to truncate (system prompt alone exceeds
                                // the model's context window) — bail immediately
                                // instead of wasting retry attempts.
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
                                    "Request exceeds model context window and cannot be reduced further. \
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

                            let mut entry_credentials_exhausted = false;
                            if rate_limited && !non_retryable_rate_limit {
                                let cooldown = self.cool_credential(entry, applied_identity, &e);
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "model_provider": provider_name,
                                            "credential": applied_identity.label(),
                                            "cooldown_ms": cooldown.as_millis(),
                                            "error": error_detail,
                                        })
                                    ),
                                    "Rate limited; cooling credential and retrying with next live key"
                                );
                                if self.select_live_credential(entry).is_none() {
                                    entry_credentials_exhausted = true;
                                    Self::record_cooldown_skip_failure(
                                        &mut failures,
                                        provider_name,
                                        current_model,
                                    );
                                }
                            }

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

                            if entry_credentials_exhausted {
                                if self.model_providers.len() > 1 {
                                    self.cool_down_rate_limited_provider(entry, current_model, &e);
                                }
                                break;
                            }

                            if attempt < self.max_retries {
                                // Retry-After applies to the cooled credential.
                                // When another live credential remains, try it
                                // promptly instead of waiting out that cool-down.
                                let wait = if rate_limited
                                    && !non_retryable_rate_limit
                                    && self.select_live_credential(entry).is_some()
                                {
                                    self.base_backoff_ms
                                } else {
                                    self.compute_backoff(backoff_ms, &e)
                                };
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
                    let (applied_identity, provider) = match self.begin_credential_attempt(entry) {
                        Ok(v) => v,
                        Err(()) => {
                            Self::record_cooldown_skip_failure(
                                &mut failures,
                                provider_name,
                                current_model,
                            );
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "model_provider": provider_name,
                                    "model": *current_model,
                                })),
                                "All credentials cooled for model_provider; unavailable"
                            );
                            break;
                        }
                    };
                    match ProviderDispatch::from_ref(provider.as_ref())
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
                            if attempt > 0
                                || *current_model != model
                                || context_truncated
                                || self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    != Some(provider_name)
                            {
                                ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"model_provider": provider_name, "model": *current_model, "attempt": attempt, "original_model": model, "context_truncated": context_truncated})), "ModelProvider recovered (failover/retry)");
                                let primary = self
                                    .model_providers
                                    .first()
                                    .map(|entry| entry.display_name.as_str())
                                    .unwrap_or("");
                                record_provider_fallback(
                                    primary,
                                    model,
                                    provider_name,
                                    current_model,
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
                                // Nothing to truncate (system prompt alone exceeds
                                // the model's context window) — bail immediately
                                // instead of wasting retry attempts.
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
                                    "Request exceeds model context window and cannot be reduced further. \
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

                            let mut entry_credentials_exhausted = false;
                            if rate_limited && !non_retryable_rate_limit {
                                let cooldown = self.cool_credential(entry, applied_identity, &e);
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(
                                        ::serde_json::json!({
                                            "model_provider": provider_name,
                                            "credential": applied_identity.label(),
                                            "cooldown_ms": cooldown.as_millis(),
                                            "error": error_detail,
                                        })
                                    ),
                                    "Rate limited; cooling credential and retrying with next live key"
                                );
                                if self.select_live_credential(entry).is_none() {
                                    entry_credentials_exhausted = true;
                                    Self::record_cooldown_skip_failure(
                                        &mut failures,
                                        provider_name,
                                        current_model,
                                    );
                                }
                            }

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

                            if entry_credentials_exhausted {
                                if self.model_providers.len() > 1 {
                                    self.cool_down_rate_limited_provider(entry, current_model, &e);
                                }
                                break;
                            }

                            if attempt < self.max_retries {
                                // Retry-After applies to the cooled credential.
                                // When another live credential remains, try it
                                // promptly instead of waiting out that cool-down.
                                let wait = if rate_limited
                                    && !non_retryable_rate_limit
                                    && self.select_live_credential(entry).is_some()
                                {
                                    self.base_backoff_ms
                                } else {
                                    self.compute_backoff(backoff_ms, &e)
                                };
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
        self.model_providers.iter().any(|entry| {
            entry
                .build_primary()
                .ok()
                .is_some_and(|p| p.supports_streaming())
        })
    }

    fn supports_streaming_tool_events(&self) -> bool {
        self.model_providers.iter().any(|entry| {
            entry
                .build_primary()
                .ok()
                .is_some_and(|p| p.supports_streaming_tool_events())
        })
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
            if self.provider_should_skip_for_cooldown(entry) {
                self.log_cooldown_skip(provider_name);
                continue;
            }

            let Ok((_identity, model_provider)) = self.begin_credential_attempt(entry) else {
                continue;
            };
            if !model_provider.supports_streaming() || !options.enabled {
                continue;
            }
            if needs_tool_events && !model_provider.supports_streaming_tool_events() {
                continue;
            }

            let provider_clone = provider_name.to_string();
            let current_model = self
                .model_chain(model)
                .first()
                .copied()
                .unwrap_or(model)
                .to_string();

            let req = ChatRequest {
                messages: request.messages,
                tools: request.tools,
                thinking: request.thinking,
            };
            let stream = ProviderDispatch::from_ref(model_provider.as_ref()).stream_chat(
                req,
                &current_model,
                temperature,
                options,
            );
            let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamEvent>>(100);

            let handle = ::zeroclaw_spawn::spawn!(async move {
                let mut stream = stream;
                while let Some(event) = stream.next().await {
                    let event = match event {
                        Ok(v) => Ok(v),
                        Err(e) => {
                            let sanitized = super::sanitize_api_error(&e.to_string());
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "model_provider": provider_clone,
                                    "model": current_model,
                                    "e": sanitized,
                                })),
                                "Streaming error"
                            );
                            Err(super::traits::StreamError::ModelProvider(sanitized))
                        }
                    };
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            });

            let guard = AbortOnDrop::new(handle.abort_handle());
            return stream::unfold((rx, guard), |(mut rx, guard)| async move {
                rx.recv().await.map(|event| (event, (rx, guard)))
            })
            .boxed();
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
        for entry in &self.model_providers {
            let provider_name = entry.display_name.as_str();
            if self.provider_should_skip_for_cooldown(entry) {
                self.log_cooldown_skip(provider_name);
                continue;
            }
            let Ok((_identity, model_provider)) = self.begin_credential_attempt(entry) else {
                continue;
            };
            if !model_provider.supports_streaming() || !options.enabled {
                continue;
            }

            let provider_clone = provider_name.to_string();
            let current_model = match self.model_chain(model).first() {
                Some(m) => (*m).to_string(),
                None => model.to_string(),
            };

            let stream = model_provider.stream_chat_with_system(
                system_prompt,
                message,
                &current_model,
                temperature,
                options,
            );
            let (tx, rx) = tokio::sync::mpsc::channel::<StreamResult<StreamChunk>>(100);

            let handle = ::zeroclaw_spawn::spawn!(async move {
                let mut stream = stream;
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(v) => Ok(v),
                        Err(e) => {
                            let sanitized = super::sanitize_api_error(&e.to_string());
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "model_provider": provider_clone,
                                    "model": current_model,
                                    "e": sanitized,
                                })),
                                "Streaming error"
                            );
                            Err(super::traits::StreamError::ModelProvider(sanitized))
                        }
                    };
                    if tx.send(chunk).await.is_err() {
                        break;
                    }
                }
            });

            let guard = AbortOnDrop::new(handle.abort_handle());
            return stream::unfold((rx, guard), |(mut rx, guard)| async move {
                rx.recv().await.map(|chunk| (chunk, (rx, guard)))
            })
            .boxed();
        }

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
        for entry in &self.model_providers {
            let provider_name = entry.display_name.as_str();
            if self.provider_should_skip_for_cooldown(entry) {
                self.log_cooldown_skip(provider_name);
                continue;
            }
            let Ok((_identity, model_provider)) = self.begin_credential_attempt(entry) else {
                continue;
            };
            if !model_provider.supports_streaming() || !options.enabled {
                continue;
            }

            let provider_clone = provider_name.to_string();
            let current_model = match self.model_chain(model).first() {
                Some(m) => (*m).to_string(),
                None => model.to_string(),
            };

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
                    let chunk = match chunk {
                        Ok(v) => Ok(v),
                        Err(e) => {
                            let sanitized = super::sanitize_api_error(&e.to_string());
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({
                                    "model_provider": provider_clone,
                                    "model": current_model,
                                    "e": sanitized,
                                })),
                                "Streaming error"
                            );
                            Err(super::traits::StreamError::ModelProvider(sanitized))
                        }
                    };
                    if tx.send(chunk).await.is_err() {
                        break;
                    }
                }
            });

            let guard = AbortOnDrop::new(handle.abort_handle());
            return stream::unfold((rx, guard), |(mut rx, guard)| async move {
                rx.recv().await.map(|chunk| (chunk, (rx, guard)))
            })
            .boxed();
        }

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
        self.model_providers
            .first()
            .and_then(|entry| entry.build_primary().ok())
            .map(|provider| ::zeroclaw_api::attribution::Attributable::role(provider.as_ref()))
            .unwrap_or(::zeroclaw_api::attribution::Role::System)
    }

    fn alias(&self) -> &str {
        &self.alias
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::Arc;
    use zeroclaw_api::tool::ToolSpec;

    struct MockModelProvider {
        calls: Arc<AtomicUsize>,
        fail_until_attempt: usize,
        response: &'static str,
        error: &'static str,
    }

    #[async_trait]
    impl ModelProvider for MockModelProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.fail_until_attempt {
                anyhow::bail!(self.error);
            }
            Ok(self.response.to_string())
        }

        async fn chat_with_history(
            &self,
            _messages: &[ChatMessage],
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.fail_until_attempt {
                anyhow::bail!(self.error);
            }
            Ok(self.response.to_string())
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for MockModelProvider {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "MockModelProvider"
        }
    }

    /// Mock that records which model was used for each call.
    struct ModelAwareMock {
        calls: Arc<AtomicUsize>,
        models_seen: parking_lot::Mutex<Vec<String>>,
        fail_models: Vec<&'static str>,
        response: &'static str,
    }

    #[async_trait]
    impl ModelProvider for ModelAwareMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.models_seen.lock().push(model.to_string());
            if self.fail_models.contains(&model) {
                anyhow::bail!("500 model {} unavailable", model);
            }
            Ok(self.response.to_string())
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for ModelAwareMock {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "ModelAwareMock"
        }
    }

    // ── Existing tests (preserved) ──

    #[tokio::test]
    async fn succeeds_without_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(MockModelProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 0,
                    response: "ok",
                    error: "boom",
                }),
            )],
            2,
            1,
        );

        let result = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_then_recovers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(MockModelProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 1,
                    response: "recovered",
                    error: "temporary",
                }),
            )],
            2,
            1,
        );

        let result = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "recovered");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn falls_back_after_retries_exhausted() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "primary down",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "from fallback",
                        error: "fallback down",
                    }),
                ),
            ],
            1,
            1,
        );

        let result = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "from fallback");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    /// Returns an empty completion (blank `chat_with_system` text, which the
    /// default `chat`/`chat_with_tools`/`chat_with_history` impls surface as a
    /// blank `ChatResponse`) for the first `empty_until_attempt` calls, then a
    /// non-empty response. Counts total calls so tests can assert re-rolls.
    struct EmptyThenTextMock {
        calls: Arc<AtomicUsize>,
        empty_until_attempt: usize,
        response: &'static str,
    }

    #[async_trait]
    impl ModelProvider for EmptyThenTextMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.empty_until_attempt {
                Ok(String::new())
            } else {
                Ok(self.response.to_string())
            }
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for EmptyThenTextMock {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "EmptyThenTextMock"
        }
    }

    #[tokio::test]
    async fn chat_retries_empty_completion_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(EmptyThenTextMock {
                    calls: Arc::clone(&calls),
                    empty_until_attempt: 1,
                    response: "recovered",
                }),
            )],
            3,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let result = model_provider
            .chat(request, "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("recovered"));
        // One empty completion + one successful re-roll.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chat_with_tools_retries_empty_completion_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(EmptyThenTextMock {
                    calls: Arc::clone(&calls),
                    empty_until_attempt: 1,
                    response: "recovered",
                }),
            )],
            3,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let result = model_provider
            .chat_with_tools(&messages, &[], "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("recovered"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chat_with_history_retries_empty_string_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(EmptyThenTextMock {
                    calls: Arc::clone(&calls),
                    empty_until_attempt: 1,
                    response: "recovered",
                }),
            )],
            3,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let result = model_provider
            .chat_with_history(&messages, "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "recovered");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chat_with_system_retries_empty_string_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(EmptyThenTextMock {
                    calls: Arc::clone(&calls),
                    empty_until_attempt: 1,
                    response: "recovered",
                }),
            )],
            3,
            1,
        );

        // `simple_chat` routes through `ReliableModelProvider::chat_with_system`,
        // the path subagent delegation uses.
        let result = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "recovered");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chat_persistent_empty_returns_blank_without_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(EmptyThenTextMock {
                    calls: Arc::clone(&calls),
                    empty_until_attempt: usize::MAX, // always empty
                    response: "never",
                }),
            )],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        // Exhausting the empty re-rolls returns the last (blank) response rather
        // than erroring — strictly never worse than the pre-fix behavior.
        let result = model_provider
            .chat(request, "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some(""));
        // Initial attempt + max_retries (2) re-rolls = 3 calls.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn chat_nonempty_response_is_not_retried() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(EmptyThenTextMock {
                    calls: Arc::clone(&calls),
                    empty_until_attempt: 0, // never empty
                    response: "direct",
                }),
            )],
            3,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let result = model_provider
            .chat(request, "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("direct"));
        // A non-empty response must not trigger any re-roll.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn returns_aggregated_error_when_all_providers_fail() {
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "p1".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "p1 error",
                    }),
                ),
                (
                    "p2".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "p2 error",
                    }),
                ),
            ],
            0,
            1,
        );

        let err = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .expect_err("all model_providers should fail");
        let msg = err.to_string();
        assert!(msg.contains("All model_providers/models failed"));
        assert!(msg.contains("model_provider=p1 model=test"));
        assert!(msg.contains("model_provider=p2 model=test"));
        assert!(msg.contains("error=p1 error"));
        assert!(msg.contains("error=p2 error"));
        assert!(msg.contains("retryable"));
    }

    #[test]
    fn non_retryable_detects_common_patterns() {
        assert!(is_non_retryable(&anyhow::Error::msg("400 Bad Request")));
        assert!(is_non_retryable(&anyhow::Error::msg("401 Unauthorized")));
        assert!(is_non_retryable(&anyhow::Error::msg("403 Forbidden")));
        assert!(is_non_retryable(&anyhow::Error::msg("404 Not Found")));
        assert!(is_non_retryable(&anyhow::Error::msg(
            "invalid api key provided"
        )));
        assert!(is_non_retryable(&anyhow::Error::msg(
            "authentication failed"
        )));
        assert!(is_non_retryable(&anyhow::Error::msg(
            "model glm-4.7 not found"
        )));
        assert!(is_non_retryable(&anyhow::Error::msg(
            "unsupported model: glm-4.7"
        )));
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "429 Too Many Requests"
        )));
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "408 Request Timeout"
        )));
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "500 Internal Server Error"
        )));
        assert!(!is_non_retryable(&anyhow::Error::msg("502 Bad Gateway")));
        assert!(!is_non_retryable(&anyhow::Error::msg("timeout")));
        assert!(!is_non_retryable(&anyhow::Error::msg("connection reset")));
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "model overloaded, try again later"
        )));
        // Context window errors are now recoverable (not non-retryable)
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "OpenAI Codex stream error: Your input exceeds the context window of this model."
        )));
    }

    #[test]
    fn auth_error_detects_common_patterns() {
        assert!(is_auth_error(&anyhow::Error::msg("401 Unauthorized")));
        assert!(is_auth_error(&anyhow::Error::msg("403 Forbidden")));
        assert!(is_auth_error(&anyhow::Error::msg("invalid api key")));
        assert!(is_auth_error(&anyhow::Error::msg("authentication failed")));
        assert!(is_auth_error(&anyhow::Error::msg("token expired")));
        assert!(!is_auth_error(&anyhow::Error::msg("400 Bad Request")));
        assert!(!is_auth_error(&anyhow::Error::msg("429 Too Many Requests")));
        assert!(!is_auth_error(&anyhow::Error::msg("timeout")));
        assert!(!is_auth_error(&anyhow::Error::msg("connection reset")));
    }

    #[test]
    fn provider_error_diagnostic_identifies_connect_timeout_endpoint() {
        let err = anyhow::Error::msg(
            "error sending request for url (https://api.deepseek.com/chat/completions): \
             client error (Connect): operation timed out",
        );

        let diagnostic = provider_error_diagnostic(&err);

        assert_eq!(diagnostic.kind, "connect_timeout");
        assert_eq!(diagnostic.phase, "tls_or_connect");
        assert_eq!(
            diagnostic.endpoint.as_deref(),
            Some("https://api.deepseek.com/chat/completions")
        );
        assert!(diagnostic.hint.contains("VPN"));
    }

    #[test]
    fn endpoint_from_error_text_strips_url_userinfo() {
        let endpoint = endpoint_from_error_text(
            "error sending request for url \
             (https://user:hunter2@inference.host/v1?token=hunter2#debug): timed out",
        );

        assert_eq!(endpoint.as_deref(), Some("https://inference.host/v1"));
    }

    #[test]
    fn sanitized_url_endpoint_scrubs_secret_like_path_segments() {
        let endpoint = sanitized_url_endpoint(
            reqwest::Url::parse(
                "https://user:hunter2@inference.host/v1/sk-secretvalue123/chat?token=hunter2#debug",
            )
            .expect("test URL parses"),
        );

        assert_eq!(endpoint, "https://inference.host/v1/[REDACTED]/chat");
        assert!(!endpoint.contains("secretvalue123"));
        assert!(!endpoint.contains("hunter2"));
    }

    #[test]
    fn endpoint_from_error_text_drops_unparseable_urls() {
        let endpoint = endpoint_from_error_text("error sending request to https://:not-a-url");

        assert_eq!(endpoint, None);
    }

    #[test]
    fn endpoint_from_error_text_preserves_ipv6_host_brackets() {
        let bare = endpoint_from_error_text("error sending request for url (http://[::1]): failed");
        let with_port = endpoint_from_error_text(
            "error sending request for url (http://[::1]:8080/v1): failed",
        );

        assert_eq!(bare.as_deref(), Some("http://[::1]/"));
        assert_eq!(with_port.as_deref(), Some("http://[::1]:8080/v1"));
    }

    #[test]
    fn provider_error_diagnostic_classifies_text_error_branches() {
        let cases = [
            (
                "input exceeds the context window of this model",
                "context_window",
                "request_validation",
                "larger-context model",
            ),
            (
                "401 Unauthorized: invalid api key",
                "auth",
                "http_response",
                "credentials",
            ),
            (
                "429 Too Many Requests",
                "rate_limited",
                "http_response",
                "quota",
            ),
            (
                "client error (Connect): operation timed out",
                "connect_timeout",
                "tls_or_connect",
                "VPN",
            ),
            (
                "request timed out while waiting for provider",
                "timeout",
                "request",
                "timed out",
            ),
            ("dns resolve failed for provider host", "dns", "dns", "DNS"),
            (
                "model gpt-missing does not exist",
                "model_not_found",
                "http_response",
                "model id",
            ),
            (
                "provider returned an opaque transport error",
                "provider_error",
                "unknown",
                "inspect provider error",
            ),
        ];

        for (message, expected_kind, expected_phase, expected_hint) in cases {
            let diagnostic = provider_error_diagnostic(&anyhow::Error::msg(message));

            assert_eq!(diagnostic.kind, expected_kind, "{message}");
            assert_eq!(diagnostic.phase, expected_phase, "{message}");
            assert!(diagnostic.hint.contains(expected_hint), "{message}");
        }
    }

    #[test]
    fn failure_summary_includes_provider_diagnostic_fields() {
        let diagnostic = ProviderErrorDiagnostic {
            kind: "connect_timeout",
            phase: "tls_or_connect",
            hint: "check network, VPN, or firewall",
            endpoint: Some("https://api.deepseek.com/chat/completions".to_string()),
        };
        let mut failures = Vec::new();

        push_failure(
            &mut failures,
            "deepseek",
            "deepseek-reasoner",
            1,
            3,
            "retryable",
            "operation timed out",
            Some(&diagnostic),
        );

        let summary = failures.join("\n");
        assert!(summary.contains("kind=connect_timeout"));
        assert!(summary.contains("phase=tls_or_connect"));
        assert!(summary.contains("endpoint=https://api.deepseek.com/chat/completions"));
        assert!(summary.contains("hint=check network, VPN, or firewall"));
    }

    #[tokio::test]
    async fn context_window_error_aborts_retries_and_model_fallbacks() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut model_fallbacks = std::collections::HashMap::new();
        model_fallbacks.insert(
            "gpt-5.3-codex".to_string(),
            vec!["gpt-5.2-codex".to_string()],
        );

        let model_provider = ReliableModelProvider::new("test", vec![(
                "openai-codex".into(),
                Box::new(MockModelProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "OpenAI Codex stream error: Your input exceeds the context window of this model. Please adjust your input and try again.",
                }),
            )],
            4,
            1,
        )
        .with_model_fallbacks(model_fallbacks);

        let err = model_provider
            .simple_chat("hello", "gpt-5.3-codex", Some(0.0))
            .await
            .expect_err("context window overflow should fail fast");
        let msg = err.to_string();

        assert!(msg.contains("context window"));
        // chat_with_system has no history to truncate, so it bails immediately
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn aggregated_error_marks_non_retryable_model_mismatch_with_details() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "custom".into(),
                Box::new(MockModelProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "unsupported model: glm-4.7",
                }),
            )],
            3,
            1,
        );

        let err = model_provider
            .simple_chat("hello", "glm-4.7", Some(0.0))
            .await
            .expect_err("model_provider should fail");
        let msg = err.to_string();

        assert!(msg.contains("non_retryable"));
        assert!(msg.contains("error=unsupported model: glm-4.7"));
        // Non-retryable errors should not consume retry budget.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn skips_retries_on_non_retryable_error() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "401 Unauthorized",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "from fallback",
                        error: "fallback err",
                    }),
                ),
            ],
            3,
            1,
        );

        let result = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "from fallback");
        // Primary should have been called only once (no retries)
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn chat_with_history_retries_then_recovers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(MockModelProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 1,
                    response: "history ok",
                    error: "temporary",
                }),
            )],
            2,
            1,
        );

        let messages = vec![ChatMessage::system("system"), ChatMessage::user("hello")];
        let result = model_provider
            .chat_with_history(&messages, "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "history ok");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn chat_with_history_falls_back() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "primary down",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "fallback ok",
                        error: "fallback err",
                    }),
                ),
            ],
            1,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let result = model_provider
            .chat_with_history(&messages, "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "fallback ok");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    // ── New tests: model failover ──

    #[tokio::test]
    async fn model_failover_tries_fallback_model() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = Arc::new(ModelAwareMock {
            calls: Arc::clone(&calls),
            models_seen: parking_lot::Mutex::new(Vec::new()),
            fail_models: vec!["claude-opus"],
            response: "ok from sonnet",
        });

        let mut fallbacks = HashMap::new();
        fallbacks.insert("claude-opus".to_string(), vec!["claude-sonnet".to_string()]);

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "anthropic".into(),
                Box::new(mock.clone()) as Box<dyn ModelProvider>,
            )],
            0, // no retries — force immediate model failover
            1,
        )
        .with_model_fallbacks(fallbacks);

        let result = model_provider
            .simple_chat("hello", "claude-opus", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "ok from sonnet");

        let seen = mock.models_seen.lock();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], "claude-opus");
        assert_eq!(seen[1], "claude-sonnet");
    }

    #[tokio::test]
    async fn model_failover_all_models_fail() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = Arc::new(ModelAwareMock {
            calls: Arc::clone(&calls),
            models_seen: parking_lot::Mutex::new(Vec::new()),
            fail_models: vec!["model-a", "model-b", "model-c"],
            response: "never",
        });

        let mut fallbacks = HashMap::new();
        fallbacks.insert(
            "model-a".to_string(),
            vec!["model-b".to_string(), "model-c".to_string()],
        );

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "p1".into(),
                Box::new(mock.clone()) as Box<dyn ModelProvider>,
            )],
            0,
            1,
        )
        .with_model_fallbacks(fallbacks);

        let err = model_provider
            .simple_chat("hello", "model-a", Some(0.0))
            .await
            .expect_err("all models should fail");
        assert!(
            err.to_string()
                .contains("All model_providers/models failed")
        );

        let seen = mock.models_seen.lock();
        assert_eq!(seen.len(), 3);
    }

    #[tokio::test]
    async fn no_model_fallbacks_behaves_like_before() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(MockModelProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 0,
                    response: "ok",
                    error: "boom",
                }),
            )],
            2,
            1,
        );
        // No model_fallbacks set — should work exactly as before
        let result = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // ── Credential rotation (real apply via constructed providers) ──

    /// Mock that optionally rate-limits and records that it was invoked.
    struct CredMock {
        label: &'static str,
        calls: Arc<AtomicUsize>,
        rate_limit: bool,
        response: &'static str,
    }

    #[async_trait]
    impl ModelProvider for CredMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.rate_limit {
                anyhow::bail!("429 Too Many Requests, Retry-After: 2");
            }
            Ok(format!("{}:{}", self.label, self.response))
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for CredMock {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            self.label
        }
    }

    fn entry_with_creds(
        name: &str,
        primary: CredMock,
        extras: Vec<CredMock>,
    ) -> ReliableModelProviderEntry {
        ReliableModelProviderEntry::from_providers(
            name,
            name,
            Box::new(primary) as Box<dyn ModelProvider>,
            extras
                .into_iter()
                .map(|m| Box::new(m) as Box<dyn ModelProvider>)
                .collect(),
        )
    }

    #[tokio::test]
    async fn on_demand_factory_observes_canonical_key_reload() {
        // SoT: credential factories resolve from a live canonical cell each
        // attempt — mutating the cell between calls must be observed.
        let canonical = Arc::new(Mutex::new("key-v1".to_string()));
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let canon = Arc::clone(&canonical);
        let obs = Arc::clone(&observed);
        let factory: CredentialFactory = Arc::new(move || {
            let key = canon.lock().unwrap_or_else(|p| p.into_inner()).clone();
            obs.lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(key.clone());
            let label: &'static str = Box::leak(key.into_boxed_str());
            Ok(Box::new(CredMock {
                label,
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: false,
                response: "ok",
            }) as Box<dyn ModelProvider>)
        });
        let entry = ReliableModelProviderEntry::new("p", "scope.p", factory);
        let model_provider = ReliableModelProvider::new_with_entries("test", vec![entry], 0, 1);

        assert_eq!(
            model_provider
                .simple_chat("hi", "m", Some(0.0))
                .await
                .unwrap(),
            "key-v1:ok"
        );
        *canonical.lock().unwrap_or_else(|p| p.into_inner()) = "key-v2".to_string();
        assert_eq!(
            model_provider
                .simple_chat("hi", "m", Some(0.0))
                .await
                .unwrap(),
            "key-v2:ok"
        );
        let seen = observed.lock().unwrap_or_else(|p| p.into_inner()).clone();
        assert_eq!(seen, vec!["key-v1".to_string(), "key-v2".to_string()]);
    }

    #[tokio::test]
    async fn model_pinned_entries_share_cooldown_identity() {
        // Two model-pinned entries with the same cooldown_key must share
        // credential cooldown state (physical provider/credential pool).
        let entry_a = ReliableModelProviderEntry::from_providers(
            "openai",
            "openai.work",
            Box::new(CredMock {
                label: "primary-a",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: true,
                response: "nope",
            }) as Box<dyn ModelProvider>,
            vec![Box::new(CredMock {
                label: "extra-a",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: false,
                response: "ok-a",
            }) as Box<dyn ModelProvider>],
        );
        let entry_b = ReliableModelProviderEntry::from_providers(
            "openai",
            "openai.work",
            Box::new(CredMock {
                label: "primary-b",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: false,
                response: "should-not-use-primary",
            }) as Box<dyn ModelProvider>,
            Vec::new(),
        );
        let model_provider =
            ReliableModelProvider::new_with_entries("test", vec![entry_a, entry_b], 1, 1);
        let result = model_provider
            .simple_chat("hi", "m", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "extra-a:ok-a");
        assert!(
            model_provider.credential_cooldown_active("openai.work", CredentialIdentity::Primary),
            "primary cooled on first pin"
        );
        assert_ne!(
            model_provider.select_live_credential(&model_provider.model_providers[1]),
            Some(CredentialIdentity::Primary),
            "shared cooldown must prevent Primary on the sibling pin"
        );
    }

    #[tokio::test]
    async fn credential_rotation_applies_next_provider_after_429() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let extra_calls = Arc::new(AtomicUsize::new(0));
        let entry = entry_with_creds(
            "p",
            CredMock {
                label: "primary-key",
                calls: Arc::clone(&primary_calls),
                rate_limit: true,
                response: "nope",
            },
            vec![CredMock {
                label: "extra-0",
                calls: Arc::clone(&extra_calls),
                rate_limit: false,
                response: "ok",
            }],
        );
        let model_provider = ReliableModelProvider::new_with_entries("test", vec![entry], 1, 1);

        let result = model_provider
            .simple_chat("hello", "m", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "extra-0:ok");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(extra_calls.load(Ordering::SeqCst), 1);

        let applied = model_provider.take_applied_credentials();
        assert_eq!(
            applied,
            vec![
                ("p".to_string(), CredentialIdentity::Primary),
                ("p".to_string(), CredentialIdentity::Extra(0)),
            ],
            "each attempt must record the credential actually applied"
        );
        assert!(
            model_provider.credential_cooldown_active("p", CredentialIdentity::Primary),
            "primary that returned 429 must be cooled"
        );
        assert!(
            !model_provider.credential_cooldown_active("p", CredentialIdentity::Extra(0)),
            "successful extra must not be cooled"
        );
    }

    #[tokio::test]
    async fn concurrent_429_cools_only_failing_credential() {
        let entry = entry_with_creds(
            "p",
            CredMock {
                label: "primary-key",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: true,
                response: "nope",
            },
            vec![
                CredMock {
                    label: "extra-0",
                    calls: Arc::new(AtomicUsize::new(0)),
                    rate_limit: false,
                    response: "ok",
                },
                CredMock {
                    label: "extra-1",
                    calls: Arc::new(AtomicUsize::new(0)),
                    rate_limit: false,
                    response: "ok",
                },
            ],
        );
        let model_provider = Arc::new(ReliableModelProvider::new_with_entries(
            "test",
            vec![entry],
            1,
            1,
        ));

        let a = {
            let p = Arc::clone(&model_provider);
            zeroclaw_spawn::spawn!(async move { p.simple_chat("a", "m", Some(0.0)).await })
        };
        let b = {
            let p = Arc::clone(&model_provider);
            zeroclaw_spawn::spawn!(async move { p.simple_chat("b", "m", Some(0.0)).await })
        };
        let _ = a.await.unwrap();
        let _ = b.await.unwrap();

        assert!(
            model_provider.credential_cooldown_active("p", CredentialIdentity::Primary),
            "primary is the failing credential under concurrent 429s"
        );
        assert!(
            !model_provider.credential_cooldown_active("p", CredentialIdentity::Extra(0)),
            "extra-0 must not be false-cooled"
        );
        assert!(
            !model_provider.credential_cooldown_active("p", CredentialIdentity::Extra(1)),
            "extra-1 must not be false-cooled"
        );
    }

    #[tokio::test]
    async fn credential_identity_scoped_per_provider_entry() {
        let entry_a = entry_with_creds(
            "provider-a",
            CredMock {
                label: "a-primary",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: true,
                response: "nope",
            },
            vec![CredMock {
                label: "a-extra",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: true,
                response: "nope",
            }],
        );
        let entry_b = entry_with_creds(
            "provider-b",
            CredMock {
                label: "b-primary",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: false,
                response: "ok",
            },
            vec![CredMock {
                label: "b-extra",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: false,
                response: "unused",
            }],
        );
        let model_provider = ReliableModelProvider::new_with_entries(
            "test",
            vec![entry_a, entry_b],
            2, // enough retries to cool entry-a Primary+Extra then failover
            1,
        );

        let result = model_provider
            .simple_chat("hello", "m", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "b-primary:ok");

        assert!(
            model_provider.credential_cooldown_active("provider-a", CredentialIdentity::Primary),
            "provider-a primary cooled"
        );
        assert!(
            model_provider.credential_cooldown_active("provider-a", CredentialIdentity::Extra(0)),
            "provider-a extra cooled"
        );
        assert!(
            !model_provider.credential_cooldown_active("provider-b", CredentialIdentity::Primary),
            "entry 1 primary must remain independent (scoped per entry)"
        );
        assert!(
            !model_provider.credential_cooldown_active("provider-b", CredentialIdentity::Extra(0)),
            "entry 1 extra must remain independent (scoped per entry)"
        );
    }

    #[tokio::test]
    async fn all_cooled_returns_unavailable_without_choosing_cooled() {
        let entry = entry_with_creds(
            "p",
            CredMock {
                label: "primary-key",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: true,
                response: "nope",
            },
            vec![CredMock {
                label: "extra-0",
                calls: Arc::new(AtomicUsize::new(0)),
                rate_limit: true,
                response: "nope",
            }],
        );
        let model_provider = ReliableModelProvider::new_with_entries("test", vec![entry], 3, 1);

        let err = model_provider
            .simple_chat("hello", "m", Some(0.0))
            .await
            .expect_err("all credentials cooled must fail");
        let msg = err.to_string();
        assert!(
            msg.contains("All model_providers/models failed")
                || msg.contains("unavailable")
                || msg.contains("rate_limit"),
            "expected unavailable-style failure, got: {msg}"
        );

        // After both are cooled, select must refuse (never hand out a cooled key).
        assert!(
            model_provider
                .select_live_credential(&model_provider.model_providers[0])
                .is_none(),
            "all-cooled must not choose a cooled credential"
        );
        let applied = model_provider.take_applied_credentials();
        assert_eq!(
            applied,
            vec![
                ("p".to_string(), CredentialIdentity::Primary),
                ("p".to_string(), CredentialIdentity::Extra(0)),
            ],
            "each credential applied once, then unavailable — no cooled re-choice: {applied:?}"
        );
    }

    #[test]
    fn rate_limited_detection() {
        assert!(is_rate_limited(&anyhow::Error::msg(
            "429 Too Many Requests"
        )));
        assert!(is_rate_limited(&anyhow::Error::msg(
            "HTTP 429 rate limit exceeded"
        )));
        assert!(!is_rate_limited(&anyhow::Error::msg("401 Unauthorized")));
        assert!(!is_rate_limited(&anyhow::Error::msg(
            "500 Internal Server Error"
        )));
    }

    #[test]
    fn non_retryable_rate_limit_detects_plan_restricted_model() {
        let err = anyhow::Error::msg(
            "API error (429 Too Many Requests): {\"code\":1311,\"message\":\"the current account plan does not include glm-5\"}",
        );
        assert!(
            is_non_retryable_rate_limit(&err),
            "plan-restricted 429 should skip retries"
        );
    }

    #[test]
    fn non_retryable_rate_limit_detects_insufficient_balance() {
        let err = anyhow::Error::msg(
            "API error (429 Too Many Requests): {\"code\":1113,\"message\":\"insufficient balance\"}",
        );
        assert!(
            is_non_retryable_rate_limit(&err),
            "insufficient-balance 429 should skip retries"
        );
    }

    #[test]
    fn non_retryable_rate_limit_does_not_flag_generic_429() {
        let err = anyhow::Error::msg("429 Too Many Requests: rate limit exceeded");
        assert!(
            !is_non_retryable_rate_limit(&err),
            "generic rate-limit 429 should remain retryable"
        );
    }

    #[test]
    fn compute_backoff_uses_base_without_typed_retry_after() {
        // anyhow::Error does not carry HTTP Retry-After metadata; backoff is
        // the exponential base only (no string scraping pretending to honor headers).
        let model_provider = ReliableModelProvider::new("test", vec![], 0, 500);
        let err = anyhow::Error::msg("429 Retry-After: 120");
        assert_eq!(model_provider.compute_backoff(500, &err), 500);
        let err = anyhow::Error::msg("500 Server Error");
        assert_eq!(model_provider.compute_backoff(500, &err), 500);
    }

    // ── §2.1 API auth error (401/403) tests ──────────────────

    #[test]
    fn non_retryable_detects_401() {
        let err = anyhow::Error::msg("API error (401 Unauthorized): invalid api key");
        assert!(
            is_non_retryable(&err),
            "401 errors must be detected as non-retryable"
        );
    }

    #[test]
    fn non_retryable_detects_403() {
        let err = anyhow::Error::msg("API error (403 Forbidden): access denied");
        assert!(
            is_non_retryable(&err),
            "403 errors must be detected as non-retryable"
        );
    }

    #[test]
    fn non_retryable_detects_404() {
        let err = anyhow::Error::msg("API error (404 Not Found): model not found");
        assert!(
            is_non_retryable(&err),
            "404 errors must be detected as non-retryable"
        );
    }

    #[test]
    fn non_retryable_does_not_flag_429() {
        let err = anyhow::Error::msg("429 Too Many Requests");
        assert!(
            !is_non_retryable(&err),
            "429 must NOT be treated as non-retryable (it is retryable with backoff)"
        );
    }

    #[test]
    fn non_retryable_does_not_flag_408() {
        let err = anyhow::Error::msg("408 Request Timeout");
        assert!(
            !is_non_retryable(&err),
            "408 must NOT be treated as non-retryable (it is retryable)"
        );
    }

    #[test]
    fn non_retryable_does_not_flag_500() {
        let err = anyhow::Error::msg("500 Internal Server Error");
        assert!(
            !is_non_retryable(&err),
            "500 must NOT be treated as non-retryable (server errors are retryable)"
        );
    }

    #[test]
    fn non_retryable_does_not_flag_502() {
        let err = anyhow::Error::msg("502 Bad Gateway");
        assert!(
            !is_non_retryable(&err),
            "502 must NOT be treated as non-retryable"
        );
    }

    #[test]
    fn rate_limited_false_for_generic_error() {
        let err = anyhow::Error::msg("Connection refused");
        assert!(
            !is_rate_limited(&err),
            "generic errors must not be flagged as rate-limited"
        );
    }

    // ── §2.3 Malformed API response error classification ─────

    #[tokio::test]
    async fn non_retryable_skips_retries_for_401() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(MockModelProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "API error (401 Unauthorized): invalid key",
                }),
            )],
            5,
            1,
        );

        let result = model_provider.simple_chat("hello", "test", Some(0.0)).await;
        assert!(result.is_err(), "401 should fail without retries");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "must not retry on 401 — should be exactly 1 call"
        );
    }

    #[tokio::test]
    async fn non_retryable_rate_limit_skips_retries_for_plan_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(MockModelProvider {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: usize::MAX,
                    response: "never",
                    error: "API error (429 Too Many Requests): {\"code\":1311,\"message\":\"plan does not include glm-5\"}",
                }),
            )],
            5,
            1,
        );

        let result = model_provider.simple_chat("hello", "test", Some(0.0)).await;
        assert!(
            result.is_err(),
            "plan-restricted 429 should fail quickly without retrying"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "must not retry non-retryable 429 business errors"
        );
    }

    #[test]
    fn cooldown_state_uses_default_duration_without_retry_after_header() {
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(MockModelProvider {
                    calls: Arc::new(AtomicUsize::new(0)),
                    fail_until_attempt: 0,
                    response: "ok",
                    error: "boom",
                }),
            )],
            0,
            1,
        );
        let err = anyhow::Error::msg("429 Too Many Requests, Retry-After: 0");

        let cooldown = model_provider.set_rate_limit_cooldown("primary", &err);

        assert_eq!(cooldown, ReliableModelProvider::RATE_LIMIT_COOLDOWN);
        assert!(
            model_provider.provider_cooldown_active("primary"),
            "default cooldown must be active (no typed Retry-After metadata)"
        );
    }

    #[tokio::test]
    async fn retryable_rate_limit_cools_down_provider_and_uses_fallback() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "HTTP 429 Too Many Requests, Retry-After: 30",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "from fallback",
                        error: "fallback down",
                    }),
                ),
            ],
            5,
            1,
        );

        let result = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .unwrap();

        assert_eq!(result, "from fallback");
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            1,
            "retryable 429 should not spend every retry on the cooled-down provider"
        );
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
        assert!(
            model_provider.provider_cooldown_active("primary"),
            "primary provider should remain cooled down after Retry-After"
        );
    }

    #[tokio::test]
    async fn retryable_rate_limit_cools_down_shared_provider_identity() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let shared_model_fallback_calls = Arc::new(AtomicUsize::new(0));
        let downstream_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new_with_entries(
            "test",
            vec![
                ReliableModelProviderEntry::from_providers(
                    "primary",
                    "openai.work",
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "HTTP 429 Too Many Requests, Retry-After: 30",
                    }),
                    Vec::new(),
                ),
                ReliableModelProviderEntry::from_providers(
                    "primary",
                    "openai.work",
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&shared_model_fallback_calls),
                        fail_until_attempt: 0,
                        response: "should be skipped",
                        error: "shared down",
                    }),
                    Vec::new(),
                ),
                ReliableModelProviderEntry::from_providers(
                    "downstream",
                    "anthropic.work",
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&downstream_calls),
                        fail_until_attempt: 0,
                        response: "downstream fallback",
                        error: "downstream down",
                    }),
                    Vec::new(),
                ),
            ],
            5,
            1,
        );

        let result = model_provider
            .simple_chat("hello", "test", Some(0.0))
            .await
            .unwrap();

        assert_eq!(result, "downstream fallback");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            shared_model_fallback_calls.load(Ordering::SeqCst),
            0,
            "entries sharing a cooldown key should be skipped as one provider"
        );
        assert_eq!(downstream_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retryable_rate_limit_cools_down_provider_for_history_chat() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response: "never",
                        error: "HTTP 429 Too Many Requests, Retry-After: 30",
                    }),
                ),
                (
                    "fallback".into(),
                    Box::new(MockModelProvider {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response: "history fallback",
                        error: "fallback down",
                    }),
                ),
            ],
            5,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let result = model_provider
            .chat_with_history(&messages, "test", Some(0.0))
            .await
            .unwrap();

        assert_eq!(result, "history fallback");
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    // Arc<ModelAwareMock> ModelProvider impl provided by blanket impl in zeroclaw-types.

    /// Mock model_provider that implements `chat()` with native tool support.
    struct NativeToolMock {
        calls: Arc<AtomicUsize>,
        fail_until_attempt: usize,
        response_text: &'static str,
        tool_calls: Vec<super::super::traits::ToolCall>,
        error: &'static str,
    }

    #[async_trait]
    impl ModelProvider for NativeToolMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok(self.response_text.to_string())
        }

        fn supports_native_tools(&self) -> bool {
            true
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt <= self.fail_until_attempt {
                anyhow::bail!(self.error);
            }
            Ok(ChatResponse {
                text: Some(self.response_text.to_string()),
                tool_calls: self.tool_calls.clone(),
                usage: None,
                reasoning_content: None,
            })
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for NativeToolMock {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "NativeToolMock"
        }
    }

    #[tokio::test]
    async fn chat_delegates_to_inner_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tool_call = super::super::traits::ToolCall {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"date"}"#.to_string(),
            extra_content: None,
        };
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(NativeToolMock {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 0,
                    response_text: "ok",
                    tool_calls: vec![tool_call.clone()],
                    error: "boom",
                }) as Box<dyn ModelProvider>,
            )],
            2,
            1,
        );

        let messages = vec![ChatMessage::user("what time is it?")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let result = model_provider
            .chat(request, "test-model", Some(0.0))
            .await
            .unwrap();

        assert_eq!(result.text.as_deref(), Some("ok"));
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "shell");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn chat_retries_and_recovers() {
        let calls = Arc::new(AtomicUsize::new(0));
        let tool_call = super::super::traits::ToolCall {
            id: "call_1".to_string(),
            name: "shell".to_string(),
            arguments: r#"{"command":"date"}"#.to_string(),
            extra_content: None,
        };
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(NativeToolMock {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 2,
                    response_text: "recovered",
                    tool_calls: vec![tool_call],
                    error: "temporary failure",
                }) as Box<dyn ModelProvider>,
            )],
            3,
            1,
        );

        let messages = vec![ChatMessage::user("test")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let result = model_provider
            .chat(request, "test-model", Some(0.0))
            .await
            .unwrap();

        assert_eq!(result.text.as_deref(), Some("recovered"));
        assert!(
            calls.load(Ordering::SeqCst) > 1,
            "should have retried at least once"
        );
    }

    #[tokio::test]
    async fn chat_preserves_native_tools_support() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(NativeToolMock {
                    calls: Arc::clone(&calls),
                    fail_until_attempt: 0,
                    response_text: "ok",
                    tool_calls: vec![],
                    error: "boom",
                }) as Box<dyn ModelProvider>,
            )],
            2,
            1,
        );

        assert!(
            model_provider.supports_native_tools(),
            "ReliableModelProvider must propagate supports_native_tools from inner model_provider"
        );
    }

    // ── Gap 2-4: Parity tests for chat() ────────────────────────

    #[tokio::test]
    async fn chat_returns_aggregated_error_when_all_providers_fail() {
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "p1".into(),
                    Box::new(NativeToolMock {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response_text: "never",
                        tool_calls: vec![],
                        error: "p1 chat error",
                    }) as Box<dyn ModelProvider>,
                ),
                (
                    "p2".into(),
                    Box::new(NativeToolMock {
                        calls: Arc::new(AtomicUsize::new(0)),
                        fail_until_attempt: usize::MAX,
                        response_text: "never",
                        tool_calls: vec![],
                        error: "p2 chat error",
                    }) as Box<dyn ModelProvider>,
                ),
            ],
            0,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let err = model_provider
            .chat(request, "test", Some(0.0))
            .await
            .expect_err("all model_providers should fail");
        let msg = err.to_string();
        assert!(msg.contains("All model_providers/models failed"));
        assert!(msg.contains("model_provider=p1 model=test"));
        assert!(msg.contains("model_provider=p2 model=test"));
        assert!(msg.contains("error=p1 chat error"));
        assert!(msg.contains("error=p2 chat error"));
        assert!(msg.contains("retryable"));
    }

    /// Mock that records model names and can fail specific models,
    /// implementing `chat()` for native tool calling parity tests.
    struct NativeModelAwareMock {
        calls: Arc<AtomicUsize>,
        models_seen: parking_lot::Mutex<Vec<String>>,
        fail_models: Vec<&'static str>,
        response_text: &'static str,
    }

    #[async_trait]
    impl ModelProvider for NativeModelAwareMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok(self.response_text.to_string())
        }

        fn supports_native_tools(&self) -> bool {
            true
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.models_seen.lock().push(model.to_string());
            if self.fail_models.contains(&model) {
                anyhow::bail!("500 model {} unavailable", model);
            }
            Ok(ChatResponse {
                text: Some(self.response_text.to_string()),
                tool_calls: vec![],
                usage: None,
                reasoning_content: None,
            })
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for NativeModelAwareMock {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "NativeModelAwareMock"
        }
    }

    // Arc<NativeModelAwareMock> ModelProvider impl provided by blanket impl in zeroclaw-types.

    #[tokio::test]
    async fn chat_tries_model_failover_on_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = Arc::new(NativeModelAwareMock {
            calls: Arc::clone(&calls),
            models_seen: parking_lot::Mutex::new(Vec::new()),
            fail_models: vec!["claude-opus"],
            response_text: "ok from sonnet",
        });

        let mut fallbacks = HashMap::new();
        fallbacks.insert("claude-opus".to_string(), vec!["claude-sonnet".to_string()]);

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "anthropic".into(),
                Box::new(mock.clone()) as Box<dyn ModelProvider>,
            )],
            0, // no retries — force immediate model failover
            1,
        )
        .with_model_fallbacks(fallbacks);

        let messages = vec![ChatMessage::user("hello")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let result = model_provider
            .chat(request, "claude-opus", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("ok from sonnet"));

        let seen = mock.models_seen.lock();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], "claude-opus");
        assert_eq!(seen[1], "claude-sonnet");
    }

    #[tokio::test]
    async fn chat_skips_non_retryable_errors() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(NativeToolMock {
                        calls: Arc::clone(&primary_calls),
                        fail_until_attempt: usize::MAX,
                        response_text: "never",
                        tool_calls: vec![],
                        error: "401 Unauthorized",
                    }) as Box<dyn ModelProvider>,
                ),
                (
                    "fallback".into(),
                    Box::new(NativeToolMock {
                        calls: Arc::clone(&fallback_calls),
                        fail_until_attempt: 0,
                        response_text: "from fallback",
                        tool_calls: vec![],
                        error: "fallback err",
                    }) as Box<dyn ModelProvider>,
                ),
            ],
            3,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let request = ChatRequest {
            messages: &messages,
            tools: None,
            thinking: None,
        };
        let result = model_provider
            .chat(request, "test", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result.text.as_deref(), Some("from fallback"));
        // Primary should have been called only once (no retries)
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    // ── Context window truncation tests ─────────────────────────

    #[test]
    fn context_window_error_is_not_non_retryable() {
        // Context window errors should be recoverable via truncation
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "exceeds the context window"
        )));
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "maximum context length exceeded"
        )));
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "too many tokens in the request"
        )));
        assert!(!is_non_retryable(&anyhow::Error::msg(
            "token limit exceeded"
        )));
    }

    #[test]
    fn is_context_window_exceeded_detects_llamacpp() {
        assert!(is_context_window_exceeded(&anyhow::Error::msg(
            "request (8968 tokens) exceeds the available context size (8448 tokens), try increasing it"
        )));
    }

    #[test]
    fn truncate_for_context_drops_oldest_non_system() {
        let mut messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("msg1"),
            ChatMessage::assistant("resp1"),
            ChatMessage::user("msg2"),
            ChatMessage::assistant("resp2"),
            ChatMessage::user("msg3"),
        ];

        let dropped = truncate_for_context(&mut messages);

        // 5 non-system messages, drop oldest half = 2
        assert_eq!(dropped, 2);
        // System message preserved
        assert_eq!(messages[0].role, "system");
        // Remaining messages should be the newer ones
        assert_eq!(messages.len(), 4); // system + 3 remaining non-system
        // The last message should still be the most recent user message
        assert_eq!(messages.last().unwrap().content, "msg3");
    }

    #[test]
    fn truncate_for_context_preserves_system_and_last_message() {
        // Only one non-system message: nothing to drop
        let mut messages = vec![ChatMessage::system("sys"), ChatMessage::user("only")];
        let dropped = truncate_for_context(&mut messages);
        assert_eq!(dropped, 0);
        assert_eq!(messages.len(), 2);

        // No system message, only one user message
        let mut messages = vec![ChatMessage::user("only")];
        let dropped = truncate_for_context(&mut messages);
        assert_eq!(dropped, 0);
        assert_eq!(messages.len(), 1);
    }

    /// Mock that fails with context error on first N calls, then succeeds.
    /// Tracks the number of messages received on each call.
    struct ContextOverflowMock {
        calls: Arc<AtomicUsize>,
        fail_until_attempt: usize,
        message_counts: parking_lot::Mutex<Vec<usize>>,
    }

    #[async_trait]
    impl ModelProvider for ContextOverflowMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }

        async fn chat_with_history(
            &self,
            messages: &[ChatMessage],
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.message_counts.lock().push(messages.len());
            if attempt <= self.fail_until_attempt {
                anyhow::bail!(
                    "request (8968 tokens) exceeds the available context size (8448 tokens), try increasing it"
                );
            }
            Ok("recovered after truncation".to_string())
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for ContextOverflowMock {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "ContextOverflowMock"
        }
    }

    #[tokio::test]
    async fn chat_with_history_truncates_on_context_overflow() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = ContextOverflowMock {
            calls: Arc::clone(&calls),
            fail_until_attempt: 1, // fail first call, succeed after truncation
            message_counts: parking_lot::Mutex::new(Vec::new()),
        };

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![("local".into(), Box::new(mock) as Box<dyn ModelProvider>)],
            3,
            1,
        );

        let messages = vec![
            ChatMessage::system("system prompt"),
            ChatMessage::user("old message 1"),
            ChatMessage::assistant("old response 1"),
            ChatMessage::user("old message 2"),
            ChatMessage::assistant("old response 2"),
            ChatMessage::user("current question"),
        ];

        let result = model_provider
            .chat_with_history(&messages, "local-model", Some(0.0))
            .await
            .unwrap();
        assert_eq!(result, "recovered after truncation");
        // Should have been called twice: once with full messages, once with truncated
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn context_overflow_with_no_history_to_truncate_bails_immediately() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mock = ContextOverflowMock {
            calls: Arc::clone(&calls),
            fail_until_attempt: 999, // always fail
            message_counts: parking_lot::Mutex::new(Vec::new()),
        };

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![("local".into(), Box::new(mock) as Box<dyn ModelProvider>)],
            3,
            1,
        );

        // Only system + one user message — nothing to truncate
        let messages = vec![
            ChatMessage::system("huge system prompt that exceeds context window"),
            ChatMessage::user("hello"),
        ];

        let result = model_provider
            .chat_with_history(&messages, "local-model", Some(0.0))
            .await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("cannot be reduced further"),
            "Should bail with actionable message, got: {err_msg}"
        );
        // Should only be called once — no useless retries
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "Should not retry when truncation is impossible"
        );
    }

    // ── Tool schema error detection tests ───────────────────────────────

    #[test]
    fn tool_schema_error_detects_groq_validation_failure() {
        let msg = r#"Groq API error (400 Bad Request): {"error":{"message":"tool call validation failed: attempted to call tool 'memory_recall' which was not in request"}}"#;
        let err = anyhow::Error::msg(msg.to_string());
        assert!(is_tool_schema_error(&err));
    }

    #[test]
    fn tool_schema_error_detects_not_in_request() {
        let err = anyhow::Error::msg("tool 'search' was not in request");
        assert!(is_tool_schema_error(&err));
    }

    #[test]
    fn tool_schema_error_detects_not_found_in_tool_list() {
        let err = anyhow::Error::msg("function 'foo' not found in tool list");
        assert!(is_tool_schema_error(&err));
    }

    #[test]
    fn tool_schema_error_detects_invalid_tool_call() {
        let err = anyhow::Error::msg("invalid_tool_call: no matching function");
        assert!(is_tool_schema_error(&err));
    }

    #[test]
    fn tool_schema_error_ignores_unrelated_errors() {
        let err = anyhow::Error::msg("invalid api key");
        assert!(!is_tool_schema_error(&err));

        let err = anyhow::Error::msg("model not found");
        assert!(!is_tool_schema_error(&err));
    }

    #[test]
    fn non_retryable_returns_false_for_tool_schema_400() {
        // A 400 error with tool schema validation text should NOT be non-retryable.
        let msg = "400 Bad Request: tool call validation failed: attempted to call tool 'x' which was not in request";
        let err = anyhow::Error::msg(msg.to_string());
        assert!(!is_non_retryable(&err));
    }

    #[test]
    fn non_retryable_returns_true_for_other_400_errors() {
        // A regular 400 error (e.g. invalid API key) should still be non-retryable.
        let err = anyhow::Error::msg("400 Bad Request: invalid api key provided");
        assert!(is_non_retryable(&err));
    }

    struct StreamingToolEventMock {
        stream_calls: Arc<AtomicUsize>,
        supports_tool_events: bool,
    }

    impl StreamingToolEventMock {
        fn new(supports_tool_events: bool) -> Self {
            Self {
                stream_calls: Arc::new(AtomicUsize::new(0)),
                supports_tool_events,
            }
        }
    }

    #[async_trait]
    impl ModelProvider for StreamingToolEventMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }

        fn supports_streaming(&self) -> bool {
            true
        }

        fn supports_streaming_tool_events(&self) -> bool {
            self.supports_tool_events
        }

        fn stream_chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: Option<f64>,
            _options: StreamOptions,
        ) -> stream::BoxStream<'static, StreamResult<StreamEvent>> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            stream::iter(vec![
                Ok(StreamEvent::ToolCall(super::super::traits::ToolCall {
                    id: "call_1".to_string(),
                    name: "shell".to_string(),
                    arguments: r#"{"command":"date"}"#.to_string(),
                    extra_content: None,
                })),
                Ok(StreamEvent::Final),
            ])
            .boxed()
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for StreamingToolEventMock {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "StreamingToolEventMock"
        }
    }

    // Arc<StreamingToolEventMock> ModelProvider impl provided by blanket impl in zeroclaw-types.

    #[tokio::test]
    async fn stream_chat_prefers_provider_with_tool_event_support() {
        let primary = Arc::new(StreamingToolEventMock::new(false));
        let fallback = Arc::new(StreamingToolEventMock::new(true));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(Arc::clone(&primary)) as Box<dyn ModelProvider>,
                ),
                (
                    "fallback".into(),
                    Box::new(Arc::clone(&fallback)) as Box<dyn ModelProvider>,
                ),
            ],
            0,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let tools = vec![ToolSpec::new(
            "shell",
            "run shell",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                }
            }),
        )];
        let mut stream = model_provider.stream_chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
                thinking: None,
            },
            "model",
            Some(0.0),
            StreamOptions::new(true),
        );

        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();
        assert!(stream.next().await.is_none());

        match first {
            StreamEvent::ToolCall(call) => assert_eq!(call.name, "shell"),
            other => panic!("expected tool-call event, got {other:?}"),
        }
        assert!(matches!(second, StreamEvent::Final));
        assert_eq!(primary.stream_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fallback.stream_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_chat_errors_when_no_provider_supports_tool_events() {
        let primary = Arc::new(StreamingToolEventMock::new(false));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(Arc::clone(&primary)) as Box<dyn ModelProvider>,
            )],
            0,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let tools = vec![ToolSpec::new(
            "shell",
            "run shell",
            serde_json::json!({"type": "object"}),
        )];
        let mut stream = model_provider.stream_chat(
            ChatRequest {
                messages: &messages,
                tools: Some(&tools),
                thinking: None,
            },
            "model",
            Some(0.0),
            StreamOptions::new(true),
        );

        let first = stream.next().await.unwrap();
        let err = first.expect_err("stream should fail without tool-event support");
        assert!(
            err.to_string()
                .contains("No model_provider supports streaming tool events"),
            "unexpected stream error: {err}"
        );
        assert!(stream.next().await.is_none());
        assert_eq!(primary.stream_calls.load(Ordering::SeqCst), 0);
    }

    // ── stream_chat_with_history failover tests ──────────────────────

    /// Mock model_provider that supports streaming via stream_chat_with_history.
    struct StreamingHistoryMock {
        stream_calls: Arc<AtomicUsize>,
        supports: bool,
    }

    #[async_trait]
    impl ModelProvider for StreamingHistoryMock {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            Ok("ok".to_string())
        }

        fn supports_streaming(&self) -> bool {
            self.supports
        }

        fn stream_chat_with_history(
            &self,
            messages: &[ChatMessage],
            _model: &str,
            _temperature: Option<f64>,
            _options: StreamOptions,
        ) -> stream::BoxStream<'static, StreamResult<StreamChunk>> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            // Echo the number of messages as the delta to verify history was passed through
            let msg_count = messages.len().to_string();
            stream::iter(vec![
                Ok(StreamChunk::delta(msg_count)),
                Ok(StreamChunk::final_chunk()),
            ])
            .boxed()
        }
    }
    impl ::zeroclaw_api::attribution::Attributable for StreamingHistoryMock {
        fn role(&self) -> ::zeroclaw_api::attribution::Role {
            ::zeroclaw_api::attribution::Role::Provider(
                ::zeroclaw_api::attribution::ProviderKind::Model(
                    ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }
        fn alias(&self) -> &str {
            "StreamingHistoryMock"
        }
    }

    #[tokio::test]
    async fn stream_chat_with_history_delegates_to_streaming_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "primary".into(),
                Box::new(StreamingHistoryMock {
                    stream_calls: Arc::clone(&calls),
                    supports: true,
                }) as Box<dyn ModelProvider>,
            )],
            0,
            1,
        );

        let messages = vec![
            ChatMessage::system("system"),
            ChatMessage::user("msg1"),
            ChatMessage::assistant("resp1"),
            ChatMessage::user("msg2"),
        ];
        let mut stream = model_provider.stream_chat_with_history(
            &messages,
            "model",
            Some(0.0),
            StreamOptions::new(true),
        );

        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(
            first.delta, "4",
            "should pass all 4 messages to model_provider"
        );
        let second = stream.next().await.unwrap().unwrap();
        assert!(second.is_final);
        assert!(stream.next().await.is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_chat_with_history_skips_non_streaming_providers() {
        let non_streaming_calls = Arc::new(AtomicUsize::new(0));
        let streaming_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "non-streaming".into(),
                    Box::new(StreamingHistoryMock {
                        stream_calls: Arc::clone(&non_streaming_calls),
                        supports: false,
                    }) as Box<dyn ModelProvider>,
                ),
                (
                    "streaming".into(),
                    Box::new(StreamingHistoryMock {
                        stream_calls: Arc::clone(&streaming_calls),
                        supports: true,
                    }) as Box<dyn ModelProvider>,
                ),
            ],
            0,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let mut stream = model_provider.stream_chat_with_history(
            &messages,
            "model",
            Some(0.0),
            StreamOptions::new(true),
        );

        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.delta, "1");
        assert_eq!(
            non_streaming_calls.load(Ordering::SeqCst),
            0,
            "non-streaming model_provider should be skipped"
        );
        assert_eq!(
            streaming_calls.load(Ordering::SeqCst),
            1,
            "streaming model_provider should be used"
        );
    }

    #[tokio::test]
    async fn stream_chat_with_history_skips_cooled_down_provider() {
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let fallback_calls = Arc::new(AtomicUsize::new(0));

        let model_provider = ReliableModelProvider::new_with_entries(
            "test",
            vec![
                ReliableModelProviderEntry::from_providers(
                    "primary",
                    "openai.work",
                    Box::new(StreamingHistoryMock {
                        stream_calls: Arc::clone(&primary_calls),
                        supports: true,
                    }) as Box<dyn ModelProvider>,
                    Vec::new(),
                ),
                ReliableModelProviderEntry::from_providers(
                    "fallback",
                    "anthropic.work",
                    Box::new(StreamingHistoryMock {
                        stream_calls: Arc::clone(&fallback_calls),
                        supports: true,
                    }) as Box<dyn ModelProvider>,
                    Vec::new(),
                ),
            ],
            0,
            1,
        );
        let err = anyhow::Error::msg("429 Too Many Requests");
        model_provider.set_rate_limit_cooldown("openai.work", &err);

        let messages = vec![ChatMessage::user("hello")];
        let mut stream = model_provider.stream_chat_with_history(
            &messages,
            "model",
            Some(0.0),
            StreamOptions::new(true),
        );

        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.delta, "1");
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            0,
            "cooled-down streaming provider should be skipped"
        );
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stream_chat_with_history_errors_when_no_provider_supports_streaming() {
        let model_provider = ReliableModelProvider::new(
            "test",
            vec![(
                "non-streaming".into(),
                Box::new(StreamingHistoryMock {
                    stream_calls: Arc::new(AtomicUsize::new(0)),
                    supports: false,
                }) as Box<dyn ModelProvider>,
            )],
            0,
            1,
        );

        let messages = vec![ChatMessage::user("hello")];
        let mut stream = model_provider.stream_chat_with_history(
            &messages,
            "model",
            Some(0.0),
            StreamOptions::new(true),
        );

        let first = stream.next().await.unwrap();
        let err = first.expect_err("should fail when no model_provider supports streaming");
        assert!(
            err.to_string()
                .contains("No model_provider supports streaming"),
            "unexpected error: {err}"
        );
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn fallback_records_provider_fallback_info() {
        scope_provider_fallback(async {
            let model_provider = ReliableModelProvider::new(
                "test",
                vec![
                    (
                        "broken".into(),
                        Box::new(MockModelProvider {
                            calls: Arc::new(AtomicUsize::new(0)),
                            fail_until_attempt: 99, // always fail
                            response: "unused",
                            error: "401 Unauthorized",
                        }),
                    ),
                    (
                        "working".into(),
                        Box::new(MockModelProvider {
                            calls: Arc::new(AtomicUsize::new(0)),
                            fail_until_attempt: 0,
                            response: "hello from working",
                            error: "unused",
                        }),
                    ),
                ],
                2,
                1,
            );

            let resp = model_provider
                .simple_chat("hi", "test-model", Some(0.0))
                .await
                .unwrap();
            assert_eq!(resp, "hello from working");

            let fb = take_last_provider_fallback();
            assert!(fb.is_some(), "fallback info should be recorded");
            let fb = fb.unwrap();
            assert_eq!(fb.requested_provider, "broken");
            assert_eq!(fb.actual_provider, "working");
            assert_eq!(fb.actual_model, "test-model");

            // Second take should be None.
            assert!(take_last_provider_fallback().is_none());
        })
        .await;
    }

    // ReliableModelProvider::supports_vision() must reflect the
    // primary (first) provider, not .any() across the fallback chain. This mirrors
    // supports_native_tools() which already uses .first().
    #[test]
    fn supports_vision_reflects_first_provider_not_any_fallback() {
        struct VisionMock(bool);

        #[async_trait]
        impl ModelProvider for VisionMock {
            async fn chat_with_system(
                &self,
                _system_prompt: Option<&str>,
                _message: &str,
                _model: &str,
                _temperature: Option<f64>,
            ) -> anyhow::Result<String> {
                Ok(String::new())
            }

            fn supports_vision(&self) -> bool {
                self.0
            }
        }
        impl ::zeroclaw_api::attribution::Attributable for VisionMock {
            fn role(&self) -> ::zeroclaw_api::attribution::Role {
                ::zeroclaw_api::attribution::Role::Provider(
                    ::zeroclaw_api::attribution::ProviderKind::Model(
                        ::zeroclaw_api::attribution::ModelProviderKind::Custom,
                    ),
                )
            }
            fn alias(&self) -> &str {
                "VisionMock"
            }
        }

        let provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(VisionMock(false)) as Box<dyn ModelProvider>,
                ),
                (
                    "fallback".into(),
                    Box::new(VisionMock(true)) as Box<dyn ModelProvider>,
                ),
            ],
            0,
            0,
        );

        assert!(
            !provider.supports_vision(),
            "ReliableModelProvider with non-vision primary must report supports_vision()=false even when a fallback supports vision"
        );

        let provider = ReliableModelProvider::new(
            "test",
            vec![
                (
                    "primary".into(),
                    Box::new(VisionMock(true)) as Box<dyn ModelProvider>,
                ),
                (
                    "fallback".into(),
                    Box::new(VisionMock(false)) as Box<dyn ModelProvider>,
                ),
            ],
            0,
            0,
        );

        assert!(provider.supports_vision());
    }

    #[tokio::test]
    async fn reliable_wrapper_exposes_inner_provider_attribution() {
        use crate::ProviderDispatch;
        use std::sync::Arc;
        use zeroclaw_api::attribution::Attributable;

        let inner_mock = MockModelProvider {
            calls: Arc::new(AtomicUsize::new(0)),
            fail_until_attempt: 0,
            response: "ok",
            error: "",
        };
        let inner_role = inner_mock.role();
        let inner_alias = inner_mock.alias().to_string();

        let reliable = ReliableModelProvider::new(
            "wrapped-alias",
            vec![("primary".into(), Box::new(inner_mock))],
            0,
            0,
        );
        // Role still comes from an ephemeral primary build; alias is the
        // wrapper's configured alias (factories cannot borrow inner &str).
        assert_eq!(reliable.role(), inner_role, "wrapper must delegate role()",);
        assert_eq!(
            reliable.alias(),
            "wrapped-alias",
            "wrapper alias is its configured alias (SoT factories are ephemeral)"
        );
        let _ = inner_alias;

        // End-to-end through ProviderDispatch: the captured event
        // must report the inner provider's `model_provider_type`,
        // never `reliable`.
        let _writer_guard = zeroclaw_log::__private_test_writer_lock();
        let _hook_guard = zeroclaw_log::__private_test_hook_lock();
        zeroclaw_log::try_install_capture_subscriber();
        let mut rx = zeroclaw_log::subscribe_or_install();
        while rx.try_recv().is_ok() {}

        let reliable: Arc<dyn ModelProvider> = Arc::new(reliable);
        let dispatch = ProviderDispatch::new(reliable);
        let req = ChatRequest {
            messages: &[],
            tools: None,
            thinking: None,
        };
        let _ = dispatch.chat(req, "m", None).await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut found_type: Option<String> = None;
        while found_type.is_none() && std::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let step = remaining.min(std::time::Duration::from_millis(50));
            match tokio::time::timeout(step, rx.recv()).await {
                Ok(Ok(value)) => {
                    if let Some(zc) = value.get("zeroclaw")
                        && let Some(t) = zc.get("model_provider_type").and_then(|v| v.as_str())
                    {
                        found_type = Some(t.to_string());
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
                Err(_elapsed) => {}
            }
        }
        assert_ne!(
            found_type.as_deref(),
            Some("reliable"),
            "ReliableModelProvider must not surface as model_provider_type=reliable",
        );
        zeroclaw_log::clear_broadcast_hook();
    }
}
