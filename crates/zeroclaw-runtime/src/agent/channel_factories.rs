//! Binary-injected channel and peripheral tool factories for the agent loop.
//!
//! Extracted from `loop_.rs` so process-global wiring stays next to the
//! seed/load helpers rather than above cost/history re-exports.

use crate::tools::{self, Tool};
use std::sync::Arc;

/// CLI channel factory, injected by the binary. Returns a `Box<dyn Channel>` for interactive mode.
pub static CLI_CHANNEL_FN: std::sync::OnceLock<
    Box<dyn Fn() -> Box<dyn zeroclaw_api::channel::Channel> + Send + Sync>,
> = std::sync::OnceLock::new();

/// Register the CLI channel factory. Called once at startup by the binary.
pub fn register_cli_channel_fn(
    f: Box<dyn Fn() -> Box<dyn zeroclaw_api::channel::Channel> + Send + Sync>,
) {
    let _ = CLI_CHANNEL_FN.set(f);
}

/// Peripheral tools factory type — takes owned config so the returned future is 'static.
pub type PeripheralToolsFn = Box<
    dyn Fn(
            zeroclaw_config::schema::PeripheralsConfig,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<Vec<Box<dyn Tool>>>> + Send>,
        > + Send
        + Sync,
>;

/// Peripheral tools factory, injected by the binary when hardware feature is on.
static PERIPHERAL_TOOLS_FN: std::sync::OnceLock<PeripheralToolsFn> = std::sync::OnceLock::new();

/// Register the peripheral tools factory. Called once at startup by the binary.
pub fn register_peripheral_tools_fn(f: PeripheralToolsFn) {
    let _ = PERIPHERAL_TOOLS_FN.set(f);
}

/// Public helper for other crates (e.g. channels orchestrator) to load
/// peripheral tools through the registered factory. Returns empty vec
/// when nothing is registered (hardware feature off or not yet wired).
pub async fn load_peripheral_tools(
    config: zeroclaw_config::schema::PeripheralsConfig,
) -> Vec<Box<dyn Tool>> {
    if let Some(f) = PERIPHERAL_TOOLS_FN.get() {
        f(config).await.unwrap_or_default()
    } else {
        Vec::new()
    }
}

/// Channel map factory type — builds `channel_key → Arc<dyn Channel>` map.
/// Injected by the binary so `zeroclaw-runtime` doesn't depend on
/// `zeroclaw-channels`.
type ChannelMapFn = Box<
    dyn Fn()
            -> std::collections::HashMap<String, std::sync::Arc<dyn zeroclaw_api::channel::Channel>>
        + Send
        + Sync,
>;

/// Channel map factory, injected by the binary.
static CHANNEL_MAP_FN: std::sync::OnceLock<ChannelMapFn> = std::sync::OnceLock::new();

/// Register the channel map factory. Called once at startup by the binary.
pub fn register_channel_map_fn(f: ChannelMapFn) {
    let _ = CHANNEL_MAP_FN.set(f);
}

/// Populate the parent turn's channel-driven tool handles from the
/// registered factory.
///
/// SA-7c (frozen #202 contract, owner-ratified): a child run must not
/// inherit a live `ask_user` handle or any user-reaching channel Arc, on
/// ANY spawn path. The gate therefore lives in this single choke point:
/// `is_subagent` child runs seed NOTHING (the function returns 0 before
/// touching any handle), so a child's `ask_user`/`reaction`/... maps stay
/// empty and every channel tool fails closed inside that run. Parent
/// callers (CLI `run`, channel `process_message`) pass `false`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn seed_channel_handles(
    is_subagent: bool,
    ask_user_handle: &Option<tools::PerToolChannelHandle>,
    channel_room_handle: &Option<tools::PerToolChannelHandle>,
    reaction_handle: &tools::PerToolChannelHandle,
    poll_handle: &Option<tools::PerToolChannelHandle>,
    escalate_handle: &Option<tools::PerToolChannelHandle>,
) -> usize {
    if is_subagent {
        return 0;
    }
    let Some(factory) = CHANNEL_MAP_FN.get() else {
        return 0;
    };
    let map = factory();
    if map.is_empty() {
        return 0;
    }

    let handles = [
        ask_user_handle.as_ref(),
        channel_room_handle.as_ref(),
        Some(reaction_handle),
        poll_handle.as_ref(),
        escalate_handle.as_ref(),
    ];

    let mut count = 0;
    for (name, ch) in &map {
        for handle in handles.iter().flatten() {
            handle
                .write()
                .insert(name.clone(), std::sync::Arc::clone(ch));
        }
        count += 1;
    }
    count
}

pub(crate) fn live_channel_registry() -> Option<tools::PerToolChannelHandle> {
    let factory = CHANNEL_MAP_FN.get()?;
    let map = factory();
    if map.is_empty() {
        return None;
    }
    Some(Arc::new(parking_lot::RwLock::new(map)))
}
