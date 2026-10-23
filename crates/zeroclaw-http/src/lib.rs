//! Shared outbound HTTP clients and process proxy application.
//!
//! `zeroclaw-config` owns the persisted proxy schema and validation. This crate
//! owns its runtime application and disposable connection cache; callers refresh
//! that projection from the loaded config at startup and reload.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use zeroclaw_config::schema::{ProxyConfig, ProxyScope, normalize_proxy_url_option};

static RUNTIME_PROXY_CONFIG: OnceLock<RwLock<ProxyConfig>> = OnceLock::new();
static RUNTIME_PROXY_CLIENT_CACHE: OnceLock<RwLock<HashMap<String, reqwest::Client>>> =
    OnceLock::new();

fn apply_proxy_to_builder(
    config: &ProxyConfig,
    mut builder: reqwest::ClientBuilder,
    service_key: &str,
) -> reqwest::ClientBuilder {
    if !config.should_apply_to_service(service_key) {
        return builder;
    }

    let no_proxy = no_proxy_value(config);

    if let Some(url) = normalize_proxy_url_option(config.all_proxy.as_deref()) {
        match reqwest::Proxy::all(&url) {
            Ok(proxy) => {
                builder = builder.proxy(apply_no_proxy(proxy, no_proxy.clone()));
            }
            Err(error) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": error.to_string()})), "Ignoring invalid all_proxy URL: ");
            }
        }
    }

    if let Some(url) = normalize_proxy_url_option(config.http_proxy.as_deref()) {
        match reqwest::Proxy::http(&url) {
            Ok(proxy) => {
                builder = builder.proxy(apply_no_proxy(proxy, no_proxy.clone()));
            }
            Err(error) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": error.to_string()})), "Ignoring invalid http_proxy URL: ");
            }
        }
    }

    if let Some(url) = normalize_proxy_url_option(config.https_proxy.as_deref()) {
        match reqwest::Proxy::https(&url) {
            Ok(proxy) => {
                builder = builder.proxy(apply_no_proxy(proxy, no_proxy));
            }
            Err(error) => {
                ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": url, "service_key": service_key, "error": error.to_string()})), "Ignoring invalid https_proxy URL: ");
            }
        }
    }

    builder
}

fn no_proxy_value(config: &ProxyConfig) -> Option<reqwest::NoProxy> {
    let joined = {
        let list = config.normalized_no_proxy();
        (!list.is_empty()).then(|| list.join(","))
    };
    joined.as_deref().and_then(reqwest::NoProxy::from_string)
}
fn apply_no_proxy(proxy: reqwest::Proxy, no_proxy: Option<reqwest::NoProxy>) -> reqwest::Proxy {
    proxy.no_proxy(no_proxy)
}
fn apply_to_process_env(config: &ProxyConfig) {
    set_proxy_env_pair("HTTP_PROXY", config.http_proxy.as_deref());
    set_proxy_env_pair("HTTPS_PROXY", config.https_proxy.as_deref());
    set_proxy_env_pair("ALL_PROXY", config.all_proxy.as_deref());

    let no_proxy_joined = {
        let list = config.normalized_no_proxy();
        (!list.is_empty()).then(|| list.join(","))
    };
    set_proxy_env_pair("NO_PROXY", no_proxy_joined.as_deref());
}

fn set_proxy_env_pair(key: &str, value: Option<&str>) {
    let lowercase_key = key.to_ascii_lowercase();
    if let Some(value) = value.and_then(|candidate| normalize_proxy_url_option(Some(candidate))) {
        // SAFETY: called during single-threaded config init before async runtime starts.
        unsafe {
            std::env::set_var(key, &value);
            std::env::set_var(lowercase_key, value);
        }
    } else {
        // SAFETY: called during single-threaded config init before async runtime starts.
        unsafe {
            std::env::remove_var(key);
            std::env::remove_var(lowercase_key);
        }
    }
}

fn runtime_proxy_state() -> &'static RwLock<ProxyConfig> {
    RUNTIME_PROXY_CONFIG.get_or_init(|| RwLock::new(ProxyConfig::default()))
}

fn runtime_proxy_client_cache() -> &'static RwLock<HashMap<String, reqwest::Client>> {
    RUNTIME_PROXY_CLIENT_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

fn clear_runtime_proxy_client_cache() {
    match runtime_proxy_client_cache().write() {
        Ok(mut guard) => {
            guard.clear();
        }
        Err(poisoned) => {
            poisoned.into_inner().clear();
        }
    }
}

fn runtime_proxy_cache_key(
    service_key: &str,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
) -> String {
    format!(
        "{}|timeout={}|connect_timeout={}",
        service_key.trim().to_ascii_lowercase(),
        timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string()),
        connect_timeout_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string())
    )
}

fn runtime_proxy_cached_client(cache_key: &str) -> Option<reqwest::Client> {
    match runtime_proxy_client_cache().read() {
        Ok(guard) => guard.get(cache_key).cloned(),
        Err(poisoned) => poisoned.into_inner().get(cache_key).cloned(),
    }
}

fn set_runtime_proxy_cached_client(cache_key: String, client: reqwest::Client) {
    match runtime_proxy_client_cache().write() {
        Ok(mut guard) => {
            guard.insert(cache_key, client);
        }
        Err(poisoned) => {
            poisoned.into_inner().insert(cache_key, client);
        }
    }
}

pub fn set_runtime_proxy_config(config: ProxyConfig) {
    match runtime_proxy_state().write() {
        Ok(mut guard) => {
            *guard = config;
        }
        Err(poisoned) => {
            *poisoned.into_inner() = config;
        }
    }

    clear_runtime_proxy_client_cache();
}

pub fn runtime_proxy_config() -> ProxyConfig {
    match runtime_proxy_state().read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Reapply the persisted canonical `[proxy]` config at process startup.
///
/// The model-visible `proxy_config` tool no longer registers, so the
/// persisted config file is the only way proxy state changes; the daemon
/// must therefore make the loaded file effective on its own:
///
/// - the runtime proxy global (read by every proxy-aware client builder)
///   is seeded unconditionally, whatever the scope, and
/// - process proxy environment variables are broadcast only when the
///   config is enabled with `scope = "environment"`, matching the
///   condition the retired `apply_env` action enforced.
///
/// A config that fails `validate()` is applied disabled (mirroring the
/// original config-load behavior): the global gets the explicit default
/// and no environment variable is written. Callers must invoke this once
/// per process boot, before the multi-threaded async runtime (and any
/// task that reads proxy environment variables) starts — process-env
/// mutation is only sound there. A daemon reload refreshes only the
/// runtime global (`set_runtime_proxy_config`), keeping live-env
/// application restart-only.
pub fn apply_persisted_proxy_on_boot(proxy: &ProxyConfig) {
    let mut effective = proxy.clone();
    if let Err(error) = effective.validate() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({"error": error.to_string()})),
            "Invalid persisted proxy configuration ignored at startup; applying disabled proxy"
        );
        effective.enabled = false;
    }
    set_runtime_proxy_config(effective.clone());
    if effective.enabled && effective.scope == ProxyScope::Environment {
        apply_to_process_env(&effective);
    }
}

pub fn apply_runtime_proxy_to_builder(
    builder: reqwest::ClientBuilder,
    service_key: &str,
) -> reqwest::ClientBuilder {
    apply_proxy_to_builder(&runtime_proxy_config(), builder, service_key)
}

pub fn build_runtime_proxy_client(service_key: &str) -> reqwest::Client {
    let cache_key = runtime_proxy_cache_key(service_key, None, None);
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return client;
    }

    let builder = apply_runtime_proxy_to_builder(reqwest::Client::builder(), service_key);
    let client = builder.build().unwrap_or_else(|error| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(
                    ::serde_json::json!({"service_key": service_key, "error": error.to_string()})
                ),
            "Failed to build proxied client: "
        );
        reqwest::Client::new()
    });
    set_runtime_proxy_cached_client(cache_key, client.clone());
    client
}

pub fn build_runtime_proxy_client_with_timeouts(
    service_key: &str,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> reqwest::Client {
    let cache_key =
        runtime_proxy_cache_key(service_key, Some(timeout_secs), Some(connect_timeout_secs));
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return client;
    }

    let builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(connect_timeout_secs));
    let builder = apply_runtime_proxy_to_builder(builder, service_key);
    let client = builder.build().unwrap_or_else(|error| {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(
                    ::serde_json::json!({"service_key": service_key, "error": error.to_string()})
                ),
            "Failed to build proxied timeout client: "
        );
        reqwest::Client::new()
    });
    set_runtime_proxy_cached_client(cache_key, client.clone());
    client
}

/// Build an HTTP client for a channel, using an explicit per-channel proxy URL
/// when configured. Falls back to the global runtime proxy when `proxy_url` is
/// `None` or empty.
pub fn build_channel_proxy_client(service_key: &str, proxy_url: Option<&str>) -> reqwest::Client {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => build_explicit_proxy_client(service_key, &url, None, None),
        None => build_runtime_proxy_client(service_key),
    }
}

/// Build an HTTP client for a channel with custom timeouts, using an explicit
/// per-channel proxy URL when configured. Falls back to the global runtime
/// proxy when `proxy_url` is `None` or empty.
pub fn build_channel_proxy_client_with_timeouts(
    service_key: &str,
    proxy_url: Option<&str>,
    timeout_secs: u64,
    connect_timeout_secs: u64,
) -> reqwest::Client {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => build_explicit_proxy_client(
            service_key,
            &url,
            Some(timeout_secs),
            Some(connect_timeout_secs),
        ),
        None => build_runtime_proxy_client_with_timeouts(
            service_key,
            timeout_secs,
            connect_timeout_secs,
        ),
    }
}

/// Apply an explicit proxy URL to a `reqwest::ClientBuilder`, returning the
/// modified builder. Used by channels that specify a per-channel `proxy_url`.
pub fn apply_channel_proxy_to_builder(
    builder: reqwest::ClientBuilder,
    service_key: &str,
    proxy_url: Option<&str>,
) -> reqwest::ClientBuilder {
    match normalize_proxy_url_option(proxy_url) {
        Some(url) => apply_explicit_proxy_to_builder(builder, service_key, &url),
        None => apply_runtime_proxy_to_builder(builder, service_key),
    }
}

/// Build a client with a single explicit proxy URL (http+https via `Proxy::all`).
fn build_explicit_proxy_client(
    service_key: &str,
    proxy_url: &str,
    timeout_secs: Option<u64>,
    connect_timeout_secs: Option<u64>,
) -> reqwest::Client {
    let cache_key = format!(
        "explicit|{}|{}|timeout={}|connect_timeout={}",
        service_key.trim().to_ascii_lowercase(),
        proxy_url,
        timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        connect_timeout_secs
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
    );
    if let Some(client) = runtime_proxy_cached_client(&cache_key) {
        return client;
    }

    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout_secs {
        builder = builder.timeout(std::time::Duration::from_secs(t));
    }
    if let Some(ct) = connect_timeout_secs {
        builder = builder.connect_timeout(std::time::Duration::from_secs(ct));
    }
    builder = apply_explicit_proxy_to_builder(builder, service_key, proxy_url);
    let client = builder.build().unwrap_or_else(|error| {
        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"service_key": service_key, "proxy_url": proxy_url, "error": error.to_string()})), "Failed to build channel proxy client: ");
        reqwest::Client::new()
    });
    set_runtime_proxy_cached_client(cache_key, client.clone());
    client
}

/// Apply a single explicit proxy URL to a builder via `Proxy::all`.
fn apply_explicit_proxy_to_builder(
    mut builder: reqwest::ClientBuilder,
    service_key: &str,
    proxy_url: &str,
) -> reqwest::ClientBuilder {
    match reqwest::Proxy::all(proxy_url) {
        Ok(proxy) => {
            builder = builder.proxy(proxy);
        }
        Err(error) => {
            ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown).with_attrs(::serde_json::json!({"proxy_url": proxy_url, "service_key": service_key, "error": error.to_string()})), "Ignoring invalid channel proxy_url: ");
        }
    }
    builder
}

#[cfg(test)]
mod tests;
