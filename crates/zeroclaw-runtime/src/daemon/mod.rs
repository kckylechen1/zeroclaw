use anyhow::Result;
use chrono::Utc;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use tokio::time::Duration;
use zeroclaw_config::schema::Config;

mod registry;
pub use registry::{DaemonRegistry, GatewayReloadControls};

const STATUS_FLUSH_SECONDS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonExit {
    Shutdown,
    Reload,
}

const EPHEMERAL_GRACE_SECS: u64 = 1;

#[cfg(test)]
static SCHEDULER_CLEAN_SHUTDOWN_OBSERVED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
pub(crate) fn reset_scheduler_clean_shutdown_observed() {
    SCHEDULER_CLEAN_SHUTDOWN_OBSERVED.store(false, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn scheduler_clean_shutdown_observed() -> bool {
    SCHEDULER_CLEAN_SHUTDOWN_OBSERVED.load(std::sync::atomic::Ordering::SeqCst)
}

async fn wait_for_exit_signal(
    mut reload_rx: tokio::sync::watch::Receiver<bool>,
    ephemeral: bool,
    client_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Result<DaemonExit> {
    use std::sync::atomic::Ordering;

    // Future that resolves when ephemeral shutdown is triggered:
    // waits for at least one client to connect, then for all clients to
    // disconnect, then sleeps the grace period. Pending forever if not
    // ephemeral.
    let ephemeral_shutdown = async {
        if !ephemeral {
            return std::future::pending::<()>().await;
        }
        // Wait until at least one client has connected.
        loop {
            if client_count.load(Ordering::Relaxed) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        // Wait until all clients disconnect.
        loop {
            if client_count.load(Ordering::Relaxed) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"grace_secs": EPHEMERAL_GRACE_SECS})),
            "All socket clients disconnected; starting ephemeral grace period"
        );
        // Grace period — if a client reconnects, abort.
        for _ in 0..EPHEMERAL_GRACE_SECS {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if client_count.load(Ordering::Relaxed) > 0 {
                // Client reconnected — restart the whole wait.
                return Box::pin(wait_for_ephemeral(client_count.clone())).await;
            }
        }
    };
    tokio::pin!(ephemeral_shutdown);

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sighup = signal(SignalKind::hangup())?;

        loop {
            tokio::select! {
                _ = sigint.recv() => {
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "Received SIGINT, shutting down...");
                    return Ok(DaemonExit::Shutdown);
                }
                _ = sigterm.recv() => {
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "Received SIGTERM, shutting down...");
                    return Ok(DaemonExit::Shutdown);
                }
                _ = sighup.recv() => {
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "Received SIGHUP, ignoring (daemon stays running)");
                }
                changed = reload_rx.changed() => {
                    if changed.is_err() {
                        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown), "Reload sender dropped; shutting down");
                        return Ok(DaemonExit::Shutdown);
                    }
                    if *reload_rx.borrow_and_update() {
                        ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "Reload requested via /admin/reload");
                        return Ok(DaemonExit::Reload);
                    }
                }
                _ = &mut ephemeral_shutdown => {
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "Ephemeral daemon: no clients remaining, shutting down");
                    return Ok(DaemonExit::Shutdown);
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        // In-process shutdown trigger (no SIGTERM on Windows): the gateway fires
        // this to request a graceful exit, e.g. for post-upgrade self-respawn.
        let respawn_shutdown = crate::restart::shutdown_notify().notified();
        tokio::pin!(respawn_shutdown);
        loop {
            tokio::select! {
                res = tokio::signal::ctrl_c() => {
                    res?;
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "Received Ctrl+C, shutting down...");
                    return Ok(DaemonExit::Shutdown);
                }
                _ = &mut respawn_shutdown => {
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "In-process shutdown requested, shutting down...");
                    return Ok(DaemonExit::Shutdown);
                }
                changed = reload_rx.changed() => {
                    if changed.is_err() {
                        ::zeroclaw_log::record!(WARN, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_outcome(::zeroclaw_log::EventOutcome::Unknown), "Reload sender dropped; shutting down");
                        return Ok(DaemonExit::Shutdown);
                    }
                    if *reload_rx.borrow_and_update() {
                        ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "Reload requested via /admin/reload");
                        return Ok(DaemonExit::Reload);
                    }
                }
                _ = &mut ephemeral_shutdown => {
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note), "Ephemeral daemon: no clients remaining, shutting down");
                    return Ok(DaemonExit::Shutdown);
                }
            }
        }
    }
}

/// Recursive helper: wait for clients to connect then all disconnect, with grace period.
async fn wait_for_ephemeral(client_count: std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::Ordering;
    // Wait until all clients disconnect again.
    loop {
        if client_count.load(Ordering::Relaxed) == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
            .with_attrs(::serde_json::json!({"grace_secs": EPHEMERAL_GRACE_SECS})),
        "All socket clients disconnected; starting ephemeral grace period"
    );
    for _ in 0..EPHEMERAL_GRACE_SECS {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if client_count.load(Ordering::Relaxed) > 0 {
            return Box::pin(wait_for_ephemeral(client_count)).await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatewayBindMode {
    /// Address is free (or an ephemeral port): start and supervise our own gateway.
    StartFresh,
    /// A ZeroClaw gateway already holds the address (e.g. a standalone
    /// `zeroclaw gateway start`): fail fast rather than start a second gateway
    /// on the same port.
    GatewayAlreadyRunning,
    /// Address is held by some other process: fail fast rather than degrade into
    /// a supervisor retry loop on the bind.
    PortOccupied,
}

/// Map the configured gateway bind host to a concrete authority reachable for a
/// local `/health` probe, formatted for a URL. Mirrors the CLI `self_test`
/// probe: wildcard `0.0.0.0` -> `127.0.0.1`, IPv6 wildcard `::`/`[::]` ->
/// `[::1]`; a bare concrete IPv6 host is bracketed.
fn gateway_probe_authority(host: &str) -> String {
    match host {
        "0.0.0.0" => "127.0.0.1".to_string(),
        "::" | "[::]" => "[::1]".to_string(),
        other if other.contains(':') && !other.starts_with('[') => format!("[{other}]"),
        other => other.to_string(),
    }
}

/// Build the `/health` probe URL for the configured gateway, honouring the
/// gateway's TLS scheme and `path_prefix` so a prefixed or HTTPS gateway is
/// probed where it actually serves health.
fn gateway_health_probe_url(config: &Config, host: &str, port: u16) -> String {
    let scheme = if config.gateway.tls.as_ref().is_some_and(|tls| tls.enabled) {
        "https"
    } else {
        "http"
    };
    // `path_prefix` is validated to start with `/` and not end with `/`.
    let prefix = config.gateway.path_prefix.as_deref().unwrap_or("");
    format!(
        "{scheme}://{}:{port}{prefix}/health",
        gateway_probe_authority(host)
    )
}

async fn zeroclaw_gateway_responds(config: &Config, host: &str, port: u16) -> bool {
    let url = gateway_health_probe_url(config, host, port);
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(500))
        .build()
    else {
        return false;
    };
    let Ok(response) = client.get(&url).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    matches!(
        response.json::<serde_json::Value>().await,
        Ok(body)
            if body.get("status").and_then(|s| s.as_str()) == Some("ok")
                && body
                    .get("require_pairing")
                    .is_some_and(serde_json::Value::is_boolean)
                && body.get("runtime").is_some_and(serde_json::Value::is_object)
    )
}

pub async fn detect_gateway_bind_mode(config: &Config, host: &str, port: u16) -> GatewayBindMode {
    // Port 0 is a kernel-assigned ephemeral port: it cannot already be bound,
    // so always start fresh.
    if port == 0 {
        return GatewayBindMode::StartFresh;
    }

    // Mirror the gateway's own bind exactly. If host:port does not parse as a
    // socket address, defer to the gateway (it has its own fallback) rather
    // than pre-judging the address.
    let Ok(addr) = zeroclaw_infra::parse_gateway_bind_socket_addr(host, port) else {
        return GatewayBindMode::StartFresh;
    };

    classify_gateway_bind_outcome(
        tokio::net::TcpListener::bind(addr).await,
        config,
        host,
        port,
    )
    .await
}

async fn classify_gateway_bind_outcome(
    bind: std::io::Result<tokio::net::TcpListener>,
    config: &Config,
    host: &str,
    port: u16,
) -> GatewayBindMode {
    match bind {
        Ok(listener) => {
            drop(listener);
            GatewayBindMode::StartFresh
        }
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if zeroclaw_gateway_responds(config, host, port).await {
                GatewayBindMode::GatewayAlreadyRunning
            } else {
                GatewayBindMode::PortOccupied
            }
        }
        Err(_) => GatewayBindMode::StartFresh,
    }
}

pub async fn run(
    mut config: Config,
    host: String,
    port: u16,
    mut registry: DaemonRegistry,
    ephemeral: bool,
) -> Result<DaemonExit> {
    config.gateway.host = host.clone();
    if port != 0 {
        config.gateway.port = port;
    }

    let initial_backoff = config.reliability.channel_initial_backoff_secs.max(1);
    let max_backoff = config
        .reliability
        .channel_max_backoff_secs
        .max(initial_backoff);

    crate::health::mark_component_ok("daemon");

    // Shared broadcast channel so all daemon components (gateway, cron,
    // heartbeat) can publish real-time events to dashboard clients.
    let (event_tx, _rx) = tokio::sync::broadcast::channel::<serde_json::Value>(256);

    zeroclaw_log::set_broadcast_hook(event_tx.clone());

    if config.heartbeat.enabled
        && let Ok((_, heartbeat_workspace_dir)) = resolve_heartbeat_workspace_dir(&config)
    {
        let _ = crate::heartbeat::engine::HeartbeatEngine::ensure_heartbeat_file(
            &heartbeat_workspace_dir,
        )
        .await;
    }

    crate::agent::pricing_catalog::load_global_pricing_catalog(&config.data_dir);

    let mut handles: Vec<JoinHandle<()>> = vec![spawn_state_writer(config.clone())];

    // Reload channel: gateway's /admin/reload writes here; our wait loop
    // (below) selects on it alongside OS signals. Cross-platform.
    let (reload_tx, reload_rx) = tokio::sync::watch::channel::<bool>(false);

    let channels_cancel = tokio_util::sync::CancellationToken::new();
    let (gateway_shutdown_tx, _) = tokio::sync::watch::channel::<bool>(false);

    // Construct the TUI registry early so both the gateway (for /api/tuis)
    // and the RPC socket (for tui/list) share the same Arc.
    let tui_registry =
        std::sync::Arc::new(crate::rpc::tui_identity::TuiRegistry::new(&config.data_dir));

    if let Some(gateway_start) = registry.take_gateway_start() {
        let gateway_cfg = config.clone();
        let gateway_host = host.clone();
        let gateway_event_tx = event_tx.clone();
        let gateway_reload_controls = GatewayReloadControls {
            shutdown_tx: gateway_shutdown_tx.clone(),
            reload_tx: reload_tx.clone(),
        };
        let gateway_tui_registry = tui_registry.clone();
        let gateway_start = std::sync::Arc::new(gateway_start);
        handles.push(spawn_component_supervisor(
            "gateway",
            initial_backoff,
            max_backoff,
            channels_cancel.clone(),
            move || {
                let cfg = gateway_cfg.clone();
                let host = gateway_host.clone();
                let tx = gateway_event_tx.clone();
                let reload_controls = gateway_reload_controls.clone();
                let tui_reg = gateway_tui_registry.clone();
                let start = gateway_start.clone();
                async move {
                    start(
                        host,
                        port,
                        cfg,
                        Some(tx),
                        Some(reload_controls),
                        Some(tui_reg),
                    )
                    .await
                }
            },
        ));
    }

    // Wall 4 (issue 197): the durable control-plane is no longer booted. Its
    // only production writer (the coordinator child host) lost its last spawn
    // producer with the spawn wall, and durable execution truth is owned by
    // Tachi through the bridge (frozen contract annex rows 1 and 6). A legacy
    // `<data_dir>/control_plane.db` is never read, migrated, rewritten, or
    // deleted — but an existing install is told so, once per process (reload
    // iterations re-run this function; the latch keeps the notice at true
    // boot frequency).
    static LEGACY_PLANE_WARNED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let legacy_control_plane = config.data_dir.join("control_plane.db");
    if LEGACY_PLANE_WARNED.set(()).is_ok() && legacy_control_plane.exists() {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                .with_attrs(::serde_json::json!({
                    "path": legacy_control_plane.display().to_string(),
                })),
            "legacy control-plane database is retired and no longer read; it is \
             left in place untouched — durable task/attempt truth now lives in \
             Tachi through the task-intent bridge"
        );
    }

    if let Some(channels_start) = registry.take_channels_start() {
        if has_supervised_channels(&config) {
            let channels_cfg = config.clone();
            let channels_start = std::sync::Arc::new(channels_start);
            let cancel_for_supervisor = channels_cancel.clone();
            handles.push(spawn_component_supervisor(
                "channels",
                initial_backoff,
                max_backoff,
                channels_cancel.clone(),
                move || {
                    let cfg = channels_cfg.clone();
                    let start = channels_start.clone();
                    let cancel = cancel_for_supervisor.clone();
                    async move { start(cfg, cancel).await }
                },
            ));
        } else {
            crate::health::mark_component_ok("channels");
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                "No channels configured; channel supervisor disabled"
            );
        }
    } else {
        crate::health::mark_component_ok("channels");
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "Channels subsystem not wired; channel supervisor disabled"
        );
    }

    // RPC transports: Unix socketand WSS (remote TUI connections).
    // Build the shared RpcContext if either transport is configured.
    let socket_client_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let need_rpc_ctx = registry.has_socket_start() || registry.has_wss_start();

    let rpc_ctx = if need_rpc_ctx {
        use crate::rpc::context::RpcContext;
        use crate::rpc::session::SessionStore;
        use zeroclaw_infra::session_queue::SessionActorQueue;

        let session_queue = std::sync::Arc::new(SessionActorQueue::new(32, 30, 600));
        let sessions = std::sync::Arc::new(SessionStore::new(64, session_queue.clone()));

        {
            let reaper_queue = std::sync::Arc::clone(&session_queue);
            zeroclaw_spawn::spawn!(async move {
                const TICK: std::time::Duration = std::time::Duration::from_secs(60);
                let mut interval = tokio::time::interval(TICK);
                interval.tick().await;
                loop {
                    interval.tick().await;
                    let queue_evicted = reaper_queue.evict_idle().await;
                    if queue_evicted > 0 {
                        let span = ::zeroclaw_log::info_span!(
                            target: "zeroclaw_log_internal_scope",
                            "zeroclaw_scope",
                            channel = "rpc",
                        );
                        let _guard = span.enter();
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note,
                            )
                            .with_category(::zeroclaw_log::EventCategory::Agent)
                            .with_attrs(::serde_json::json!({
                                "evicted_queue_slots": queue_evicted,
                            })),
                            "Session queue: released idle actor-queue slots"
                        );
                        crate::util::release_freed_heap();
                    }
                }
            });
        }
        let session_backend = zeroclaw_infra::make_session_backend(
            &config.data_dir,
            &config.channels.session_backend,
        )
        .ok();

        // Wire the memory subsystem so `memory/list` and `memory/search`
        // work over RPC transports (same pattern as the gateway).
        let rpc_memory: Option<std::sync::Arc<dyn zeroclaw_api::memory_traits::Memory>> = if config
            .agents
            .is_empty()
        {
            None
        } else {
            match zeroclaw_memory::create_memory_from_config(&config, None) {
                Ok(mem) => Some(std::sync::Arc::from(mem)),
                Err(_e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                        "RPC memory subsystem unavailable"
                    );
                    None
                }
            }
        };

        // Open the ACP session DB at boot so the file exists from the
        // moment the daemon is up, not when (if ever) `zeroclaw acp`
        // runs. Best-effort: on failure, log and continue with `None`.
        let acp_session_store: Option<
            std::sync::Arc<zeroclaw_infra::acp_session_store::AcpSessionStore>,
        > = match zeroclaw_infra::acp_session_store::AcpSessionStore::new(&config.data_dir) {
            Ok(s) => Some(std::sync::Arc::new(s)),
            Err(e) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({"error": e.to_string()})),
                    "Failed to open ACP session store at daemon boot"
                );
                None
            }
        };

        let hooks: Option<std::sync::Arc<crate::hooks::HookRunner>> = if config.hooks.enabled {
            Some(std::sync::Arc::new(crate::hooks::HookRunner::from_config(
                &config.hooks,
            )))
        } else {
            None
        };

        Some(std::sync::Arc::new(RpcContext {
            config: std::sync::Arc::new(parking_lot::RwLock::new(config.clone())),
            config_write_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            sessions,
            session_backend,
            memory: rpc_memory,
            // Process-global tracker shared with the gateway and channel
            // supervisor. Without this the RPC/zerocode-TUI turn path has no
            // tracker to record into and model cost is silently dropped
            cost_tracker: crate::cost::CostTracker::get_or_init_global(
                config.cost.clone(),
                &config.data_dir,
            ),
            event_tx: Some(event_tx.clone()),
            reload_tx: Some(reload_tx.clone()),
            gateway_shutdown_tx: Some(gateway_shutdown_tx.clone()),
            approval_pending: std::sync::Arc::new(
                crate::rpc::context::ApprovalPendingMap::default(),
            ),
            tui_registry,
            acp_session_store,
            hooks,
        }))
    } else {
        None
    };

    // Local IPC RPC listener (Unix socket on Unix, Named Pipe on Windows).
    if let Some(socket_start) = registry.take_socket_start() {
        let rpc_ctx = rpc_ctx
            .clone()
            .expect("rpc_ctx built when socket_start is Some");
        let socket_start = std::sync::Arc::new(socket_start);
        let socket_cancel = channels_cancel.clone();
        let count = socket_client_count.clone();
        handles.push(spawn_component_supervisor(
            "socket",
            initial_backoff,
            max_backoff,
            socket_cancel.clone(),
            move || {
                let ctx = rpc_ctx.clone();
                let start = socket_start.clone();
                let cancel = socket_cancel.clone();
                let count = count.clone();
                async move { start(ctx, cancel, count).await }
            },
        ));
    }

    // WSS RPC listener (remote TUI connections).
    if let Some(wss_start) = registry.take_wss_start() {
        let rpc_ctx = rpc_ctx
            .clone()
            .expect("rpc_ctx built when wss_start is Some");
        let wss_start = std::sync::Arc::new(wss_start);
        let wss_cancel = channels_cancel.clone();
        let count = socket_client_count.clone();
        handles.push(spawn_component_supervisor(
            "wss",
            initial_backoff,
            max_backoff,
            wss_cancel.clone(),
            move || {
                let ctx = rpc_ctx.clone();
                let start = wss_start.clone();
                let cancel = wss_cancel.clone();
                let count = count.clone();
                async move { start(ctx, cancel, count).await }
            },
        ));
    }

    if config.heartbeat.enabled {
        let heartbeat_cfg = config.clone();
        handles.push(spawn_component_supervisor(
            "heartbeat",
            initial_backoff,
            max_backoff,
            channels_cancel.clone(),
            move || {
                let cfg = heartbeat_cfg.clone();
                async move { Box::pin(run_heartbeat_worker(cfg)).await }
            },
        ));
    }

    if config.scheduler.enabled {
        let scheduler_cfg = config.clone();
        let scheduler_event_tx = event_tx.clone();
        let scheduler_cancel = channels_cancel.clone();
        handles.push(spawn_component_supervisor(
            "scheduler",
            initial_backoff,
            max_backoff,
            channels_cancel.clone(),
            move || {
                let cfg = scheduler_cfg.clone();
                let tx = scheduler_event_tx.clone();
                let cancel = scheduler_cancel.clone();
                async move { Box::pin(crate::cron::scheduler::run(cfg, Some(tx), cancel)).await }
            },
        ));
    } else {
        crate::health::mark_component_ok("scheduler");
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "Cron disabled; scheduler supervisor not started"
        );
    }

    record_daemon_started(&config, &host, port);

    // Wait for shutdown (SIGINT/SIGTERM/Ctrl+C) or reload (in-process channel).
    let exit = wait_for_exit_signal(reload_rx, ephemeral, socket_client_count).await?;
    crate::health::mark_component_error(
        "daemon",
        match exit {
            DaemonExit::Shutdown => "shutdown requested",
            DaemonExit::Reload => "reload requested",
        },
    );

    channels_cancel.cancel();

    const GRACE_WINDOW: Duration = Duration::from_millis(500);
    let deadline = tokio::time::Instant::now() + GRACE_WINDOW;
    let mut remaining: Vec<JoinHandle<()>> = Vec::new();
    for mut handle in handles {
        tokio::select! {
            biased;
            _ = &mut handle => {
                // Cooperative handle exited cleanly during grace window.
            }
            _ = tokio::time::sleep_until(deadline) => {
                // Grace window expired; force-abort and re-join later.
                handle.abort();
                remaining.push(handle);
            }
        }
    }
    // Await remaining (aborted) handles. Already-completed handles from
    // the grace window are not re-await, so "JoinHandle polled after
    // completion" is avoided.
    for handle in remaining {
        let _ = handle.await;
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }

    Ok(exit)
}

pub fn state_file_path(config: &Config) -> PathBuf {
    config
        .config_path
        .parent()
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
        .join("state")
        .join("daemon_state.json")
}

fn record_daemon_started(config: &Config, host: &str, port: u16) {
    ::zeroclaw_log::record!(
        INFO,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Start)
            .with_category(::zeroclaw_log::EventCategory::System)
            .with_outcome(::zeroclaw_log::EventOutcome::Success)
            .with_attrs(::serde_json::json!({
                "requested_gateway": format!("http://{host}:{port}"),
                "socket": crate::rpc::local::socket_path(config).display().to_string(),
                "pairing_enabled": config.gateway.require_pairing,
                "stop_signal": "Ctrl+C or SIGTERM",
            })),
        "ZeroClaw daemon started"
    );
}

fn spawn_state_writer(config: Config) -> JoinHandle<()> {
    zeroclaw_spawn::spawn!(async move {
        let path = state_file_path(&config);
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }

        let mut interval = tokio::time::interval(Duration::from_secs(STATUS_FLUSH_SECONDS));
        loop {
            interval.tick().await;
            let mut json = crate::health::snapshot_json();
            if let Some(obj) = json.as_object_mut() {
                obj.insert(
                    "written_at".into(),
                    serde_json::json!(Utc::now().to_rfc3339()),
                );
            }
            let data = serde_json::to_vec_pretty(&json).unwrap_or_else(|_| b"{}".to_vec());
            let _ = tokio::fs::write(&path, data).await;
        }
    })
}

fn spawn_component_supervisor<F, Fut>(
    name: &'static str,
    initial_backoff_secs: u64,
    max_backoff_secs: u64,
    cancel: tokio_util::sync::CancellationToken,
    mut run_component: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    zeroclaw_spawn::spawn!(async move {
        let mut backoff = initial_backoff_secs.max(1);
        let max_backoff = max_backoff_secs.max(backoff);

        let stable_run = Duration::from_secs(initial_backoff_secs.max(1).saturating_mul(5));

        loop {
            crate::health::mark_component_ok(name);
            let run_started = std::time::Instant::now();
            let outcome = run_component().await;
            let ran_for = run_started.elapsed();
            match outcome {
                Ok(()) => {
                    if cancel.is_cancelled() {
                        crate::health::mark_component_ok(name);
                        #[cfg(test)]
                        if name == "scheduler" {
                            SCHEDULER_CLEAN_SHUTDOWN_OBSERVED
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Success)
                            .with_attrs(::serde_json::json!({"name": name})),
                            &format!(
                                "Daemon component '{name}' shut down cleanly via cancellation token"
                            )
                        );
                        return;
                    }
                    crate::health::mark_component_error(name, "component exited unexpectedly");
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "name": name,
                                "ran_for_secs": ran_for.as_secs(),
                            })),
                        &format!("Daemon component '{name}' exited unexpectedly")
                    );
                    if ran_for >= stable_run {
                        backoff = initial_backoff_secs.max(1);
                    }
                }
                Err(e) => {
                    crate::health::mark_component_error(name, e.to_string());
                    ::zeroclaw_log::record!(
                        ERROR,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "error": format!("{}", e),
                                "name": name,
                                "ran_for_secs": ran_for.as_secs(),
                            })),
                        &format!("Daemon component '{name}' failed: {e}")
                    );
                    // A long-lived run that eventually errors is not a
                    // fast-fail loop; let it reset so a component that ran fine
                    // for hours and then hit a transient error retries quickly
                    // rather than inheriting a huge stale backoff.
                    if ran_for >= stable_run {
                        backoff = initial_backoff_secs.max(1);
                    }
                }
            }

            crate::health::bump_component_restart(name);
            crate::util::release_freed_heap();
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            // Double backoff AFTER sleeping so first error uses initial_backoff
            backoff = backoff.saturating_mul(2).min(max_backoff);
        }
    })
}

fn resolve_heartbeat_workspace_dir(config: &Config) -> Result<(String, PathBuf)> {
    let agent_alias = config.heartbeat.agent.trim().to_string();
    if agent_alias.is_empty() {
        anyhow::bail!(
            "heartbeat worker requires `[heartbeat] agent = \"<alias>\"` naming a configured agent"
        );
    }
    if config.agent(&agent_alias).is_none() {
        anyhow::bail!(
            "[heartbeat] agent = {agent_alias:?} is not configured ([agents.{agent_alias}] missing)"
        );
    }
    let workspace_dir = config.agent_workspace_dir(&agent_alias);
    Ok((agent_alias, workspace_dir))
}

/// Test-only hook for [`connect_heartbeat_mcp_registry`]. The daemon
/// heartbeat worker builds an `Arc<McpRegistry>` once at worker start
/// and shares it across every tick so that stdio MCP children live
/// for the daemon's lifetime. Tests inject a hook here to
/// count invocations and assert the registry is constructed at most
/// once for N simulated ticks.
///
/// Hooks receive the resolved agent alias and the pre-computed list of
/// MCP server configs granted to that agent by `mcp_bundles`. They
/// MUST return an `Arc<McpRegistry>` whose inner server lifetime
/// outlives the simulated ticks (returning a fresh registry per call
/// would create a new stdio child on every tick).
#[cfg(test)]
type HeartbeatMcpRegistryTestHook = std::sync::Arc<
    dyn Fn(
            &str,
            &[zeroclaw_config::schema::McpServerConfig],
        ) -> std::sync::Arc<crate::tools::McpRegistry>
        + Send
        + Sync,
>;

#[cfg(test)]
static HEARTBEAT_MCP_REGISTRY_TEST_HOOK: std::sync::Mutex<Option<HeartbeatMcpRegistryTestHook>> =
    std::sync::Mutex::new(None);

/// Serializes the regression tests for the daemon heartbeat MCP
/// registry hook. The hook itself is process-global, so a
/// test that installs the hook, runs assertions, and then resets
/// cannot safely interleave with another test doing the same: a
/// concurrent `reset_heartbeat_mcp_registry_test_hook` would clear
/// the hook before the first test observes it, and a concurrent
/// `set_heartbeat_mcp_registry_test_hook` from another test would
/// swap in a hook whose counter Arc belongs to the other test. To
/// keep the regression tests deterministic, every hook-using test
/// takes this mutex (via [`HeartbeatMcpRegistryTestHookGuard`]) for
/// the entire duration of its hook-installed work.
#[cfg(test)]
static HEARTBEAT_MCP_REGISTRY_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// RAII guard that ties a test-only MCP registry hook installation
/// to the global serialising lock. Construction takes the global
/// mutex, installs the supplied hook, and returns a guard whose
/// `Drop` clears the hook and releases the mutex. Tests that need
/// the MCP registry hook to be observed by the daemon MUST hold
/// this guard for the duration of the work that depends on it;
/// otherwise a parallel test could clobber the hook (or reset it
/// while the current test is still running) and the assertion would
/// see a stale or absent hook.
#[cfg(test)]
pub(crate) struct HeartbeatMcpRegistryTestHookGuard {
    serial_lock: Option<std::sync::MutexGuard<'static, ()>>,
}

#[cfg(test)]
impl HeartbeatMcpRegistryTestHookGuard {
    /// Install `hook` under the global serialising lock and return a
    /// guard whose `Drop` clears the hook and releases the lock.
    fn install(hook: HeartbeatMcpRegistryTestHook) -> Self {
        // Hold the serial lock before mutating the hook global so a
        // concurrent test cannot observe a torn state (hook swapped
        // halfway, or reset between this test's set and use).
        // Poison here only means an earlier test panicked while serialized.
        // The guard's Drop runs during that unwind and clears the hook, so
        // the protected state is already clean — recover instead of turning
        // one genuine failure into a cascade of phantom ones.
        let serial_lock = HEARTBEAT_MCP_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut guard = HEARTBEAT_MCP_REGISTRY_TEST_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(hook);
        // Drop the hook global mutex immediately — the serial lock
        // is what prevents another test from racing with us now, and
        // the hook global only needs its inner value read once per
        // helper invocation.
        drop(guard);
        Self {
            serial_lock: Some(serial_lock),
        }
    }

    /// Serialize a real-path test (one that wants NO hook, e.g. a real
    /// stdio connect) against hook-installing tests. Takes the same lock
    /// the hook guard takes, installs nothing; Drop's hook-clear is a
    /// no-op by construction. Held as a struct field so the guard can
    /// live across the test's await points.
    fn serialize_real_path() -> Self {
        let serial_lock = HEARTBEAT_MCP_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self {
            serial_lock: Some(serial_lock),
        }
    }
}

#[cfg(test)]
impl Drop for HeartbeatMcpRegistryTestHookGuard {
    fn drop(&mut self) {
        // Clear the hook first (still under the serial lock taken by
        // `install`) so the next test sees a clean slate. Recover from
        // poison — skipping the clear would leak a stale hook into the
        // next test, which is strictly worse than any poison state.
        *HEARTBEAT_MCP_REGISTRY_TEST_HOOK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        // Releasing the serial lock last allows the next waiting
        // test to proceed only after our hook is gone.
        drop(self.serial_lock.take());
    }
}

/// Install a test-only hook that returns a pre-built `Arc<McpRegistry>`
/// for a given `(agent_alias, server_configs)` pair. Used by the
/// regression test in `tests` to bypass the real `connect_all` while
/// still counting constructions via the user's own counter logic.
///
/// Returns a guard that MUST be held for the duration of the test
/// work that depends on the hook; on drop, the hook is cleared and
/// the serialising lock is released so the next queued test can run.
/// Spinning off a detached future that outlives the guard will leave
/// the hook pointing at a stale closure and is not supported.
#[cfg(test)]
pub(crate) fn set_heartbeat_mcp_registry_test_hook(
    hook: HeartbeatMcpRegistryTestHook,
) -> HeartbeatMcpRegistryTestHookGuard {
    HeartbeatMcpRegistryTestHookGuard::install(hook)
}

/// Snapshot the current test hook (cloned). Returns `None` when no
/// hook is installed. Used by [`connect_heartbeat_mcp_registry`]
/// during the registry-construction phase of the heartbeat worker.
#[cfg(test)]
fn current_heartbeat_mcp_registry_test_hook() -> Option<HeartbeatMcpRegistryTestHook> {
    let guard = HEARTBEAT_MCP_REGISTRY_TEST_HOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.as_ref().cloned()
}

/// Connect the daemon's shared MCP registry for the heartbeat
/// agent. Called ONCE per `run_heartbeat_worker` invocation, the
/// returned `Arc<McpRegistry>` is then cloned into every
/// `AgentRunOverrides::mcp_registry` for the lifetime of the worker.
///
/// Returns `Ok(None)` when MCP is disabled, no servers are granted
/// to this agent, or the connection itself fails (fail-open: a
/// granted-but-unreachable MCP server must NOT crash the heartbeat
/// worker under supervisor backoff — the per-run `agent::run` MCP
/// path itself fails open, so we mirror that here. Because the
/// connection failed there is no stdio child to spawn, so the
/// "construct the registry once per worker" guarantee still holds
/// whenever the registry IS reachable — the healthy case this
/// targets).
///
/// When `Some`, the worker drops the registry on exit and the MCP
/// stdio children are reaped cleanly via
/// `tokio::process::Child::kill_on_drop(true)`.
/// Compute the subset of `granted` that is missing or dead in `current` --
/// i.e. the servers that actually need a fresh connection. A name with a
/// healthy handle in `current` is never included, so calling this
/// repeatedly while that handle stays healthy keeps excluding it instead
/// of re-including it on every heartbeat tick (the partial-outage churn
/// the retry path still had: a granted list of {A, B} with A healthy
/// and B down previously caused every tick to reconnect BOTH A and B via
/// `McpRegistry::connect_all`, even though A never needed it).
fn missing_or_dead_servers(
    granted: Vec<zeroclaw_config::schema::McpServerConfig>,
    current: Option<&std::sync::Arc<crate::tools::McpRegistry>>,
) -> Vec<zeroclaw_config::schema::McpServerConfig> {
    let Some(cur) = current else {
        return granted;
    };
    let dead: std::collections::HashSet<String> = cur.health_check_all().into_iter().collect();
    let healthy_names: std::collections::HashSet<String> = cur
        .server_handles()
        .into_iter()
        .filter(|(name, _)| !dead.contains(name))
        .map(|(name, _)| name)
        .collect();
    granted
        .into_iter()
        .filter(|s| !healthy_names.contains(&s.name))
        .collect()
}

async fn connect_heartbeat_mcp_registry(
    config: &Config,
    agent_alias: &str,
    current: Option<&std::sync::Arc<crate::tools::McpRegistry>>,
) -> Result<Option<std::sync::Arc<crate::tools::McpRegistry>>> {
    // Only (re)connect what `current` doesn't already have healthy --
    // a healthy server must never be respawned/re-handshaked just
    // because a sibling grant is missing or dead (see
    // `missing_or_dead_servers`). `current` is `None` at worker boot,
    // where every granted server is by definition missing. Computed
    // unconditionally (pure, no I/O) so the test hook below observes
    // the same filtered subset the real connect path would use.
    let granted = config.mcp_servers_for_agent(agent_alias);
    let servers = missing_or_dead_servers(granted, current);

    #[cfg(test)]
    if let Some(hook) = current_heartbeat_mcp_registry_test_hook() {
        return Ok(Some(hook(agent_alias, &servers)));
    }

    if !config.mcp.enabled {
        return Ok(None);
    }
    if servers.is_empty() {
        // Nothing is missing/dead. `reconcile_heartbeat_mcp_registry`
        // treats `fresh = None` as "keep current unchanged", so the
        // caller's existing registry is left exactly as it was.
        return Ok(None);
    }
    // Fail-open, mirroring the per-run `agent::run` MCP path: a
    // granted MCP server being unreachable must NOT take the
    // heartbeat worker down under supervisor backoff. On connect
    // failure we log and return `Ok(None)`; each tick then falls
    // back to the per-run path (which itself fails open). Because
    // the connection failed there is no stdio child to spawn, so
    // the "construct the registry once per worker" guarantee
    // still holds whenever the registry IS reachable — the healthy
    // case this targets.
    match crate::tools::McpRegistry::connect_all(&servers).await {
        Ok(registry) => Ok(Some(std::sync::Arc::new(registry))),
        Err(e) => {
            ::zeroclaw_log::record!(
                ERROR,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "agent": agent_alias,
                        "error": format!("{:#}", e),
                    })),
                "heartbeat worker: failed to connect shared MCP registry; continuing without MCP tools"
            );
            Ok(None)
        }
    }
}

/// Additively reconcile the heartbeat worker's MCP registry.
///
/// The heartbeat worker's lifetime invariant is that a healthy
/// live `McpServer` connection must NEVER be silently disconnected and
/// respawned just because a peer's discovery result changed shape. This
/// function preserves that invariant under all of:
///
///   * steady state (both registries match by name + `McpServer` Arc
///     identity — return `None`, no churn);
///   * partial outage (one granted server healthy, another flaky —
///     keep the healthy handle, admit the freshly-discovered peer
///     additively);
///   * dead transport (a server whose child exited after startup —
///     drop it from `current` while keeping the rest, admit anything
///     `fresh` brought back);
///
/// while never silently dropping the live registry when both current
/// and fresh are present. The additive merge rebuilds the registry
/// from `healthy_current + fresh_new`, Arc-cloning each surviving
/// `McpServer` so its live transport is reused — no disconnect, no
/// respawn.
///
/// Returns `Some(merged_registry)` when the caller should replace
/// `current` with a new Arc, `None` when `current` should stay
/// unchanged.
async fn reconcile_heartbeat_mcp_registry(
    current: Option<&std::sync::Arc<crate::tools::McpRegistry>>,
    fresh: Option<&std::sync::Arc<crate::tools::McpRegistry>>,
) -> Option<std::sync::Arc<crate::tools::McpRegistry>> {
    let Some(current_arc) = current else {
        // No current registry — use fresh (if any). This is the boot
        // case where `shared` was `None`.
        return fresh.map(std::sync::Arc::clone);
    };
    let Some(fresh_arc) = fresh else {
        // No fresh registry — keep current. A failed `connect_all`
        // must not silently drop a live registry (fail-open).
        return None;
    };

    // Step 1: split current into healthy (kept) and dead (dropped /
    // replaced by fresh). `health_check_all` is read-only, so it
    // works against the shared Arc without `Arc::get_mut`.
    let dead: std::collections::HashSet<String> =
        current_arc.health_check_all().into_iter().collect();
    let current_handles = current_arc.server_handles();
    let healthy_handles: Vec<(String, crate::tools::McpServer)> = current_handles
        .into_iter()
        .filter(|(name, _)| !dead.contains(name))
        .collect();
    let healthy_names: std::collections::HashSet<String> =
        healthy_handles.iter().map(|(n, _)| n.clone()).collect();

    // Step 2: identify the slice of `fresh` that is NOT already
    // covered by a healthy current server — those are the recovered
    // servers we want to admit additively. We keep a sorted-by-name
    // copy of all fresh handles for the merged-equals-fresh check
    // in step 5 (avoids recomputing `server_handles()` again).
    let fresh_handles: Vec<(String, crate::tools::McpServer)> = fresh_arc.server_handles();
    let fresh_new: Vec<(String, crate::tools::McpServer)> = fresh_handles
        .iter()
        .filter(|(name, _)| !healthy_names.contains(name))
        .map(|(n, s)| (n.clone(), s.clone()))
        .collect();

    // Step 3: merged set, sorted by name for determinism.
    let mut merged: Vec<(String, crate::tools::McpServer)> = healthy_handles;
    merged.extend(fresh_new);
    merged.sort_by(|a, b| a.0.cmp(&b.0));

    // Step 4: identity-stable no-churn check. If the merged set is
    // exactly the healthy-current set (same names AND same `McpServer`
    // Arc identity for each name), then the merged registry would be
    // functionally identical to `current` and there is nothing to
    // do — return `None` so the caller keeps the existing Arc.
    if merged.is_empty() {
        // No healthy current and no fresh-new — `current` may still
        // hold dead handles, but `fresh` did not bring a recovery.
        // Keep current (it might be the boot empty registry that
        // the test hook installed; the next tick will try again).
        return None;
    }
    let current_after_drop = current_arc.server_handles();
    // A merged entry is "the same" as a current entry iff they share
    // the name AND the underlying McpServer transport. When the
    // healthy-current handle for name N is exactly the same McpServer
    // we ended up with in `merged` for name N (which is the case
    // whenever `fresh_new` didn't carry the same name), no churn.
    let mut current_by_name: std::collections::HashMap<String, crate::tools::McpServer> =
        current_after_drop
            .into_iter()
            .filter(|(name, _)| !dead.contains(name))
            .collect();
    let mut churn = false;
    for (name, server) in &merged {
        match current_by_name.remove(name) {
            Some(existing) if existing.ptr_eq(server) => {
                // Same handle — preserved connection. No churn on
                // this entry.
            }
            Some(_) | None => {
                // Either the entry came from `fresh_new` (a brand-new
                // server we just admitted), or the healthy current
                // server's identity drifted away (uncommon — same
                // name but a different handle, which only happens
                // when `fresh` rebuilt the connection). Either way,
                // a new registry allocation is required.
                churn = true;
            }
        }
    }
    if !churn && current_by_name.is_empty() {
        // Every merged name matched a healthy current handle
        // identity, and no healthy current handle was left over
        // unmatched. The merged set is identical to the healthy
        // current set — no churn, no replacement.
        return None;
    }

    // Step 5: when the merged set is exactly `fresh`'s server set
    // (same names, same `McpServer` Arc identity for each name),
    // there is nothing for the daemon to rebuild — reuse `fresh`'s
    // Arc directly. This avoids a redundant `McpRegistry::from_servers`
    // allocation AND preserves the caller's "the recovery Arc IS
    // the fresh Arc" expectation: tick 1 of the recovery sequence
    // must hand the worker the same Arc pointer the hook returned.
    // (Both `merged` and `fresh_handles` are sorted by name, so the
    // zip is name-aligned.)
    if merged.len() == fresh_handles.len() {
        let all_match = merged
            .iter()
            .zip(fresh_handles.iter())
            .all(|((mn, ms), (fn_, fs))| mn == fn_ && ms.ptr_eq(fs));
        if all_match {
            return Some(std::sync::Arc::clone(fresh_arc));
        }
    }

    // Step 6: build the new registry from the merged handles.
    // `from_servers` rebuilds the tool_index from each handle's
    // advertised capabilities — empty for stub servers in tests,
    // non-empty for real stdio children.
    let servers: Vec<crate::tools::McpServer> = merged.into_iter().map(|(_, s)| s).collect();
    let new_registry = crate::tools::McpRegistry::from_servers(servers).await;
    Some(std::sync::Arc::new(new_registry))
}

/// Reconnect and reconcile the daemon-level MCP registry.
///
/// Called on each heartbeat tick. When the registry is incomplete (fewer
/// connected servers than granted) or health checks detect dead connections,
/// this function rebuilds the registry and uses [`reconcile_heartbeat_mcp_registry`]
/// to decide whether to replace the current registry.
///
/// Returns immediately (Ok(())) when no reconnection is needed.
async fn retry_heartbeat_mcp_registry(
    shared: &mut Option<std::sync::Arc<crate::tools::McpRegistry>>,
    config: &Config,
    agent_alias: &str,
) -> Result<()> {
    // Always attempt to reconnect and reconcile. This ensures that:
    // - Dead servers (after startup) are detected and replaced
    // - New servers (that came up later) are picked up
    // - Healthy servers are preserved via identity-aware reconciliation
    //   (live `McpServer` Arc identity is reused; no churn on the
    //   healthy side when only a peer server's discovery result
    //   changed shape).
    let granted = config.mcp_servers_for_agent(agent_alias);
    let granted_count = granted.len();
    let current_count = shared.as_ref().map_or(0, |r| r.server_count());

    // When the registry is complete and we own the Arc (single strong
    // ref), we can do a live health check and skip reconnect if healthy.
    // When the Arc is shared (e.g. an `AgentRunOverrides` clone exists),
    // we can still do the health check (it's read-only), but we may get
    // a stale result. Since `health_check_all` is read-only, we use
    // `shared.as_ref()` so `Arc::get_mut` is no longer required.
    let should_reconnect = if current_count >= granted_count {
        // Complete registry: only reconnect if health check fails.
        // `health_check_all` is read-only, so it works with shared Arc refs.
        match shared.as_ref().map(|arc| arc.health_check_all()) {
            Some(dead) => !dead.is_empty(),
            None => true, // no registry → reconnect
        }
    } else if current_count > 0 {
        // Partially complete — always reconnect to pick up missing servers.
        true
    } else {
        // Incomplete: always reconnect.
        true
    };

    if should_reconnect {
        // Kill dead connections if we can (no-op for test stubs).
        if let Some(arc) = shared
            && let Some(reg) = std::sync::Arc::get_mut(arc)
        {
            let _dead = reg.kill_dead_connections().await;
        }
        let fresh = connect_heartbeat_mcp_registry(config, agent_alias, shared.as_ref()).await?;
        // Let the additive reconciler decide whether the live registry
        // needs to be replaced; it returns `None` when the healthy
        // current handles are sufficient (preserves the
        // no-churn steady state) and `Some(merged)` when `fresh` adds
        // a recovered server or replaces a dead one.
        if let Some(replaced) =
            reconcile_heartbeat_mcp_registry(shared.as_ref(), fresh.as_ref()).await
        {
            *shared = Some(replaced);
        }
    }
    Ok(())
}

async fn run_heartbeat_worker(config: Config) -> Result<()> {
    use crate::heartbeat::engine::{
        HeartbeatEngine, HeartbeatTask, TaskPriority, TaskStatus, compute_adaptive_interval,
    };
    use std::sync::Arc;

    let (agent_alias, heartbeat_workspace_dir) = resolve_heartbeat_workspace_dir(&config)?;

    // Build the daemon-level MCP registry ONCE per worker. With this
    // owner in place, every `agent::run` tick below reuses the same
    // `Arc<McpRegistry>` and the stdio MCP children live for the
    // worker's whole lifetime.
    //
    // The variable is `mut` so `retry_heartbeat_mcp_registry` below
    // can replace the stored registry with a fresh one when a granted
    // MCP server is missing from the registry (e.g. it was down at
    // worker boot and comes up later). Once `server_count ==
    // granted.len()` the call is a no-op and the Arc pointer survives
    // across ticks — the no-churn steady state is preserved.
    let mut shared_mcp_registry: Option<Arc<crate::tools::McpRegistry>> =
        connect_heartbeat_mcp_registry(&config, &agent_alias, None).await?;

    let observer: std::sync::Arc<dyn crate::observability::Observer> =
        std::sync::Arc::from(crate::observability::create_observer(&config.observability));
    let engine = HeartbeatEngine::new(config.heartbeat.clone(), heartbeat_workspace_dir, observer);
    let metrics = engine.metrics();
    let delivery = resolve_heartbeat_delivery(&config)?;
    let two_phase = config.heartbeat.two_phase;
    let adaptive = config.heartbeat.adaptive;
    let start_time = std::time::Instant::now();

    // ── Deadman watcher ──────────────────────────────────────────
    let deadman_timeout = config.heartbeat.deadman_timeout_minutes;
    if deadman_timeout > 0 {
        let dm_metrics = Arc::clone(&metrics);
        let dm_config = config.clone();
        let dm_delivery = delivery.clone();
        zeroclaw_spawn::spawn!(async move {
            let check_interval = Duration::from_secs(60);
            let timeout = chrono::Duration::minutes(i64::from(deadman_timeout));
            loop {
                tokio::time::sleep(check_interval).await;
                let last_tick = dm_metrics.lock().last_tick_at;
                if let Some(last) = last_tick
                    && chrono::Utc::now() - last > timeout
                {
                    let alert = format!(
                        "⚠️ Heartbeat dead-man's switch: no tick in {deadman_timeout} minutes"
                    );
                    let (channel, target) = if let Some(ch) = &dm_config.heartbeat.deadman_channel {
                        let to = dm_config
                            .heartbeat
                            .deadman_to
                            .as_deref()
                            .or(dm_config.heartbeat.to.as_deref())
                            .unwrap_or_default();
                        (ch.clone(), to.to_string())
                    } else if let Some((ch, to)) = &dm_delivery {
                        (ch.clone(), to.clone())
                    } else {
                        continue;
                    };
                    let delivery_fut = crate::cron::scheduler::deliver_announcement(
                        &dm_config, &channel, &target, None, &alert,
                    );
                    match tokio::time::timeout(Duration::from_secs(30), delivery_fut).await {
                        Ok(Err(e)) => {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                                "Deadman alert delivery failed"
                            );
                        }
                        Err(_) => {
                            ::zeroclaw_log::record!(
                                WARN,
                                ::zeroclaw_log::Event::new(
                                    module_path!(),
                                    ::zeroclaw_log::Action::Note
                                )
                                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                                "Deadman alert delivery timed out (30s)"
                            );
                        }
                        Ok(Ok(())) => {}
                    }
                }
            }
        });
    }

    let base_interval = config.heartbeat.interval_minutes.max(1);
    let mut sleep_mins = base_interval;

    loop {
        tokio::time::sleep(Duration::from_secs(u64::from(sleep_mins) * 60)).await;

        // Update uptime
        {
            let mut m = metrics.lock();
            m.uptime_secs = start_time.elapsed().as_secs();
        }

        let tick_start = std::time::Instant::now();

        // ── retry-while-incomplete ───────────────────────────
        // When the registry is incomplete (fewer connected servers than
        // granted) or health checks detect dead connections, this call
        // rebuilds the registry and uses `reconcile_heartbeat_mcp_registry`
        // to decide whether to replace the current registry.
        retry_heartbeat_mcp_registry(&mut shared_mcp_registry, &config, &agent_alias).await?;

        // Collect runnable tasks (active only, sorted by priority)
        let mut tasks = engine.collect_runnable_tasks().await?;
        let has_high_priority = tasks.iter().any(|t| t.priority == TaskPriority::High);

        if tasks.is_empty() {
            if let Some(fallback) = config
                .heartbeat
                .message
                .as_deref()
                .map(str::trim)
                .filter(|m| !m.is_empty())
            {
                tasks.push(HeartbeatTask {
                    text: fallback.to_string(),
                    priority: TaskPriority::Medium,
                    status: TaskStatus::Active,
                });
            } else {
                #[allow(clippy::cast_precision_loss)]
                let elapsed = tick_start.elapsed().as_millis() as f64;
                metrics.lock().record_success(elapsed);
                continue;
            }
        }

        // ── Phase 1: LLM decision (two-phase mode) ──────────────
        let tasks_to_run = if two_phase {
            let decision_prompt = format!(
                "[Heartbeat Task | decision] {}",
                HeartbeatEngine::build_decision_prompt(&tasks),
            );
            let phase1_fut = Box::pin(crate::agent::run(
                config.clone(),
                &agent_alias,
                Some(decision_prompt),
                None,
                None,
                Some(0.0),
                vec![],
                false,
                None,
                None,
                zeroclaw_api::ingress::TurnOrigin::Daemon,
                crate::agent::loop_::AgentRunOverrides {
                    mcp_registry: shared_mcp_registry.as_ref().map(Arc::clone),
                    ..crate::agent::loop_::AgentRunOverrides::default()
                },
            ));
            let phase1_result = if config.heartbeat.task_timeout_secs > 0 {
                match tokio::time::timeout(
                    Duration::from_secs(config.heartbeat.task_timeout_secs),
                    phase1_fut,
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Timeout
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "phase": "phase1_decision",
                                "timeout_secs": config.heartbeat.task_timeout_secs,
                            })),
                            "heartbeat: phase1 decision timed out"
                        );
                        Err(anyhow::Error::msg(format!(
                            "Phase 1 decision timed out ({}s)",
                            config.heartbeat.task_timeout_secs
                        )))
                    }
                }
            } else {
                phase1_fut.await
            };
            match phase1_result {
                Ok(response) => {
                    let indices = HeartbeatEngine::parse_decision_response(&response, tasks.len());
                    if indices.is_empty() {
                        ::zeroclaw_log::record!(
                            INFO,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            ),
                            "heartbeat phase 1: skip (nothing to do)"
                        );
                        crate::health::mark_component_ok("heartbeat");
                        #[allow(clippy::cast_precision_loss)]
                        let elapsed = tick_start.elapsed().as_millis() as f64;
                        metrics.lock().record_success(elapsed);
                        continue;
                    }
                    ::zeroclaw_log::record!(INFO, ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note).with_attrs(::serde_json::json!({"selected": indices.len(), "total": tasks.len()})), "heartbeat phase 1: running task subset");
                    indices
                        .into_iter()
                        .filter_map(|i| tasks.get(i).cloned())
                        .collect()
                }
                Err(e) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "heartbeat phase 1 failed; running all tasks"
                    );
                    tasks
                }
            }
        } else {
            tasks
        };

        // ── Phase 2: Execute selected tasks ─────────────────────
        // Re-read session context on every tick so we pick up messages
        // that arrived since the daemon started.
        let session_context = if config.heartbeat.load_session_context {
            load_heartbeat_session_context(&config)
        } else {
            None
        };

        let heartbeat_memory: Option<Box<dyn zeroclaw_memory::Memory>> =
            zeroclaw_memory::create_memory_from_config(
                &config,
                config
                    .model_provider_for_agent(&agent_alias)
                    .and_then(|e| e.api_key.as_deref()),
            )
            .ok();

        let mut tick_had_error = false;
        for task in &tasks_to_run {
            let task_start = std::time::Instant::now();
            let task_prompt = format!("[Heartbeat Task | {}] {}", task.priority, task.text);

            // Memory context is injected once in the engine, keyed on the
            // Daemon origin (agent::memory_inject): Conversation entries are
            // excluded for scheduled origins. `heartbeat_memory` stays for
            // the post-run auto-save consolidation below.
            let prompt = match &session_context {
                Some(sc) => format!("{sc}\n\n{task_prompt}"),
                None => task_prompt,
            };
            let temp: Option<f64> = config
                .model_provider_for_agent(&agent_alias)
                .and_then(|e| e.temperature);
            let phase2_fut = Box::pin(crate::agent::run(
                config.clone(),
                &agent_alias,
                Some(prompt),
                None,
                None,
                temp,
                vec![],
                false,
                None,
                None,
                zeroclaw_api::ingress::TurnOrigin::Daemon,
                crate::agent::loop_::AgentRunOverrides {
                    mcp_registry: shared_mcp_registry.as_ref().map(Arc::clone),
                    ..crate::agent::loop_::AgentRunOverrides::default()
                },
            ));
            let phase2_result = if config.heartbeat.task_timeout_secs > 0 {
                match tokio::time::timeout(
                    Duration::from_secs(config.heartbeat.task_timeout_secs),
                    phase2_fut,
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Timeout
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({
                                "phase": "phase2_heartbeat",
                                "timeout_secs": config.heartbeat.task_timeout_secs,
                            })),
                            "heartbeat task timed out"
                        );
                        Err(anyhow::Error::msg(format!(
                            "Heartbeat task timed out ({}s)",
                            config.heartbeat.task_timeout_secs
                        )))
                    }
                }
            } else {
                phase2_fut.await
            };
            match phase2_result {
                Ok(output) => {
                    crate::health::mark_component_ok("heartbeat");
                    #[allow(clippy::cast_possible_truncation)]
                    let duration_ms = task_start.elapsed().as_millis() as i64;
                    let now = chrono::Utc::now();
                    let _ = crate::heartbeat::store::record_run(
                        &config.data_dir,
                        &task.text,
                        &task.priority.to_string(),
                        now - chrono::Duration::milliseconds(duration_ms),
                        now,
                        "ok",
                        Some(output.as_str()),
                        duration_ms,
                        config.heartbeat.max_run_history,
                    );
                    // Consolidate heartbeat output to memory for cross-session awareness.
                    if config.memory.auto_save
                        && output.chars().count() >= 50
                        && let Some(ref mem) = heartbeat_memory
                    {
                        let key = format!("heartbeat_{}", uuid::Uuid::new_v4());
                        let summary = if output.len() > 500 {
                            // Find a valid UTF-8 char boundary at or before 500.
                            let mut end = 500;
                            while end > 0 && !output.is_char_boundary(end) {
                                end -= 1;
                            }
                            &output[..end]
                        } else {
                            &output
                        };
                        let _ = mem
                            .store(
                                &key,
                                &format!("Heartbeat task '{}': {}", task.text, summary),
                                zeroclaw_memory::MemoryCategory::Daily,
                                None,
                            )
                            .await;
                    }

                    let announcement = if output.trim().is_empty() {
                        format!("💓 heartbeat task completed: {}", task.text)
                    } else {
                        output
                    };
                    let suppress_delivery =
                        !crate::cron::scheduler::announce_delivery_decision(&announcement)
                            .should_deliver();
                    if suppress_delivery {
                        ::zeroclaw_log::record!(
                            DEBUG,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Note
                            )
                            .with_outcome(::zeroclaw_log::EventOutcome::Success)
                            .with_attrs(::serde_json::json!({"task": task.text})),
                            "Heartbeat task returned NO_REPLY sentinel — skipping delivery"
                        );
                    }
                    if let Some((channel, target)) = &delivery
                        && !suppress_delivery
                    {
                        let delivery_result = tokio::time::timeout(
                            Duration::from_secs(30),
                            crate::cron::scheduler::deliver_announcement(
                                &config,
                                channel,
                                target,
                                None,
                                &announcement,
                            ),
                        )
                        .await;
                        match delivery_result {
                            Ok(Err(e)) => {
                                crate::health::mark_component_error(
                                    "heartbeat",
                                    format!("delivery failed: {e}"),
                                );
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                                    .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                                    "Heartbeat delivery failed"
                                );
                            }
                            Err(_) => {
                                crate::health::mark_component_error(
                                    "heartbeat",
                                    "delivery timed out (30s)".to_string(),
                                );
                                ::zeroclaw_log::record!(
                                    WARN,
                                    ::zeroclaw_log::Event::new(
                                        module_path!(),
                                        ::zeroclaw_log::Action::Note
                                    )
                                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                                    "Heartbeat delivery timed out (30s)"
                                );
                            }
                            Ok(Ok(())) => {}
                        }
                    }
                }
                Err(e) => {
                    tick_had_error = true;
                    #[allow(clippy::cast_possible_truncation)]
                    let duration_ms = task_start.elapsed().as_millis() as i64;
                    let now = chrono::Utc::now();
                    let _ = crate::heartbeat::store::record_run(
                        &config.data_dir,
                        &task.text,
                        &task.priority.to_string(),
                        now - chrono::Duration::milliseconds(duration_ms),
                        now,
                        "error",
                        Some(&e.to_string()),
                        duration_ms,
                        config.heartbeat.max_run_history,
                    );
                    crate::health::mark_component_error("heartbeat", e.to_string());
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                        "Heartbeat task failed"
                    );
                }
            }
        }

        // Update metrics
        #[allow(clippy::cast_precision_loss)]
        let tick_elapsed = tick_start.elapsed().as_millis() as f64;
        {
            let mut m = metrics.lock();
            if tick_had_error {
                m.record_failure(tick_elapsed);
            } else {
                m.record_success(tick_elapsed);
            }
        }

        // Compute next sleep interval
        if adaptive {
            let failures = metrics.lock().consecutive_failures;
            sleep_mins = compute_adaptive_interval(
                base_interval,
                config.heartbeat.min_interval_minutes,
                config.heartbeat.max_interval_minutes,
                failures,
                has_high_priority,
            );
        } else {
            sleep_mins = base_interval;
        }
    }
}

/// Resolve delivery target: explicit config > auto-detect first configured channel.
fn resolve_heartbeat_delivery(config: &Config) -> Result<Option<(String, String)>> {
    let channel = config
        .heartbeat
        .target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let target = config
        .heartbeat
        .to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match (channel, target) {
        // Both explicitly set — validate and use.
        (Some(channel), Some(target)) => {
            validate_heartbeat_channel_config(config, channel)?;
            Ok(Some((channel.to_string(), target.to_string())))
        }
        // Only one set — error.
        (Some(_), None) => anyhow::bail!("heartbeat.to is required when heartbeat.target is set"),
        (None, Some(_)) => anyhow::bail!("heartbeat.target is required when heartbeat.to is set"),
        // Neither set — try auto-detect the first configured channel.
        (None, None) => Ok(auto_detect_heartbeat_channel(config)),
    }
}

const HEARTBEAT_SESSION_CONTEXT_MESSAGES: usize = 20;

fn load_heartbeat_session_context(config: &Config) -> Option<String> {
    use zeroclaw_providers::traits::ChatMessage;

    let channel = config
        .heartbeat
        .target
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())?;
    let to = config
        .heartbeat
        .to
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())?;

    if channel.contains('/') || channel.contains('\\') || to.contains('/') || to.contains('\\') {
        ::zeroclaw_log::record!(
            WARN,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
            "heartbeat session context: channel/to contains path separators, skipping"
        );
        return None;
    }

    let sessions_dir = config.data_dir.join("sessions");

    // Find the most recently modified JSONL file that belongs to this target.
    // Matches both `{channel}_{to}.jsonl` and `{channel}_{anything}_{to}.jsonl`.
    let prefix = format!("{channel}_");
    let suffix = format!("_{to}.jsonl");
    let exact = format!("{channel}_{to}.jsonl");
    let mid_prefix = format!("{channel}_{to}_");

    let path = std::fs::read_dir(&sessions_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".jsonl")
                && (name == exact
                    || (name.starts_with(&prefix) && name.ends_with(&suffix))
                    || name.starts_with(&mid_prefix))
        })
        .max_by_key(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
        .map(|e| e.path())?;

    if !path.exists() {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                .with_attrs(::serde_json::json!({"channel": channel, "to": to})),
            "heartbeat session context: no session file found"
        );
        return None;
    }

    let messages = load_jsonl_messages(&path);
    if messages.is_empty() {
        return None;
    }

    let recent: Vec<&ChatMessage> = messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .rev()
        .take(HEARTBEAT_SESSION_CONTEXT_MESSAGES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    // Only inject context if there is at least one real user message in the
    // window. If the JSONL contains only assistant messages (e.g. previous
    // heartbeat outputs with no reply yet), skip context to avoid feeding
    // Monika's own messages back to her in a loop.
    let has_user_message = recent.iter().any(|m| m.role == "user");
    if !has_user_message {
        ::zeroclaw_log::record!(
            DEBUG,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "💓 Heartbeat session context: no user messages in recent history — skipping"
        );
        return None;
    }

    // Use the session file's mtime as a proxy for when the last message arrived.
    let last_message_age = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|mtime| mtime.elapsed().ok());

    let silence_note = match last_message_age {
        Some(age) => {
            let mins = age.as_secs() / 60;
            if mins < 60 {
                format!("(last message ~{mins} minutes ago)\n")
            } else {
                let hours = mins / 60;
                let rem = mins % 60;
                if rem == 0 {
                    format!("(last message ~{hours}h ago)\n")
                } else {
                    format!("(last message ~{hours}h {rem}m ago)\n")
                }
            }
        }
        None => String::new(),
    };

    ::zeroclaw_log::record!(
        DEBUG,
        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
        &format!(
            "💓 Heartbeat session context: {} messages from {}, silence: {}",
            recent.len(),
            path.display().to_string(),
            silence_note.trim()
        )
    );

    let mut ctx = format!(
        "[Recent conversation history — use this for context when composing your message] {silence_note}",
    );
    for msg in &recent {
        let label = if msg.role == "user" { "User" } else { "You" };
        // Truncate very long messages to avoid bloating the prompt.
        // Use char_indices to avoid panicking on multi-byte UTF-8 characters.
        let content = if msg.content.len() > 500 {
            let truncate_at = msg
                .content
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= 500)
                .last()
                .unwrap_or(0);
            format!("{}…", &msg.content[..truncate_at])
        } else {
            msg.content.clone()
        };
        ctx.push_str(label);
        ctx.push_str(": ");
        ctx.push_str(&content);
        ctx.push('\n');
    }

    Some(ctx)
}

/// Read the last `HEARTBEAT_SESSION_CONTEXT_MESSAGES` `ChatMessage` lines from
/// a JSONL session file using a bounded rolling window so we never hold the
/// entire file in memory.
fn load_jsonl_messages(path: &std::path::Path) -> Vec<zeroclaw_providers::traits::ChatMessage> {
    use std::collections::VecDeque;
    use std::io::BufRead;

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = std::io::BufReader::new(file);
    let mut window: VecDeque<zeroclaw_providers::traits::ChatMessage> =
        VecDeque::with_capacity(HEARTBEAT_SESSION_CONTEXT_MESSAGES + 1);
    for line in reader.lines() {
        let Ok(line) = line else { continue };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<zeroclaw_providers::traits::ChatMessage>(trimmed) {
            window.push_back(msg);
            if window.len() > HEARTBEAT_SESSION_CONTEXT_MESSAGES {
                window.pop_front();
            }
        }
    }
    window.into_iter().collect()
}

/// Auto-detect the best channel for heartbeat delivery by checking which
/// channels are configured. Returns the first match in priority order.
fn auto_detect_heartbeat_channel(config: &Config) -> Option<(String, String)> {
    // Priority order: telegram > discord > slack > mattermost
    // Find the first external peer authorized on a telegram channel
    // (peer authorization lives in peer_groups in V3, not on the
    // channel block).
    if !config.channels.telegram.is_empty() {
        for alias in config.channels.telegram.keys() {
            let peers = config.channel_external_peers("telegram", alias);
            if let Some(target) = peers.into_iter().next() {
                return Some(("telegram".to_string(), target));
            }
        }
    }
    if !config.channels.discord.is_empty() {
        // Discord requires explicit target — can't auto-detect
        return None;
    }
    if !config.channels.slack.is_empty() {
        // Slack requires explicit target
        return None;
    }
    if !config.channels.mattermost.is_empty() {
        // Mattermost requires explicit target
        return None;
    }
    None
}

fn validate_heartbeat_channel_config(config: &Config, channel: &str) -> Result<()> {
    if !config.channels.is_known_channel(channel) {
        anyhow::bail!("unsupported heartbeat.target channel: {channel}");
    }
    if !config.channels.is_channel_configured(channel) {
        anyhow::bail!(
            "heartbeat.target is set to {channel} but channels.{channel} is not configured"
        );
    }
    if !config.channels.is_channel_deliverable(channel) {
        anyhow::bail!(
            "heartbeat.target is set to {channel} but {channel} is an input-only channel that cannot deliver outbound messages"
        );
    }
    Ok(())
}

fn has_supervised_channels(config: &Config) -> bool {
    config.channels.has_any_enabled()
}

#[cfg(test)]
mod tests;
