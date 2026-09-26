use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;
use zeroclaw_config::schema::Config;

pub type StarterFuture = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

#[derive(Clone)]
pub struct GatewayReloadControls {
    pub shutdown_tx: watch::Sender<bool>,
    pub reload_tx: watch::Sender<bool>,
}

pub type GatewayStarter = Box<
    dyn Fn(
            String,
            u16,
            Config,
            Option<broadcast::Sender<Value>>,
            Option<GatewayReloadControls>,
        ) -> StarterFuture
        + Send
        + Sync,
>;

/// Starts the supervised channel orchestrator for one daemon run/reload iteration.
pub type ChannelsStarter = Box<dyn Fn(Config, CancellationToken) -> StarterFuture + Send + Sync>;

#[derive(Default)]
pub struct DaemonRegistry {
    gateway_start: Option<GatewayStarter>,
    channels_start: Option<ChannelsStarter>,
}

impl DaemonRegistry {
    /// Create an empty registry. Missing starters are treated as unwired
    /// optional subsystems by `daemon::run`.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_gateway(&mut self, starter: GatewayStarter) -> &mut Self {
        self.gateway_start = Some(starter);
        self
    }

    #[cfg(test)]
    fn has_gateway_start(&self) -> bool {
        self.gateway_start.is_some()
    }

    pub fn register_channels(&mut self, starter: ChannelsStarter) -> &mut Self {
        self.channels_start = Some(starter);
        self
    }

    #[cfg(test)]
    fn has_channels_start(&self) -> bool {
        self.channels_start.is_some()
    }

    pub(crate) fn take_gateway_start(&mut self) -> Option<GatewayStarter> {
        self.gateway_start.take()
    }

    pub(crate) fn take_channels_start(&mut self) -> Option<ChannelsStarter> {
        self.channels_start.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway_starter() -> GatewayStarter {
        Box::new(|_, _, _, _, _| Box::pin(async { Ok(()) }))
    }

    fn channels_starter() -> ChannelsStarter {
        Box::new(|_, _| Box::pin(async { Ok(()) }))
    }

    #[test]
    fn new_registry_has_no_start_hooks() {
        let registry = DaemonRegistry::new();

        assert!(!registry.has_gateway_start());
        assert!(!registry.has_channels_start());
    }

    #[test]
    fn builder_records_typed_start_hooks() {
        let mut registry = DaemonRegistry::new();
        registry
            .register_gateway(gateway_starter())
            .register_channels(channels_starter());

        assert!(registry.has_gateway_start());
        assert!(registry.has_channels_start());
    }

    #[test]
    fn taking_start_hooks_consumes_slots() {
        let mut registry = DaemonRegistry::new();
        registry
            .register_gateway(gateway_starter())
            .register_channels(channels_starter());

        assert!(registry.take_gateway_start().is_some());
        assert!(registry.take_channels_start().is_some());

        assert!(!registry.has_gateway_start());
        assert!(!registry.has_channels_start());
    }
}
