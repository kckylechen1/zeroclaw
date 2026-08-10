//! Feature-gated channel omission tests for the orchestrator.
#[cfg(not(feature = "channel-telegram"))]
#[test]
fn collect_configured_channels_omits_telegram_when_compiled_out() {
    use super::*;
    let mut config = Config::default();
    config.channels.telegram.insert(
        "default".to_string(),
        zeroclaw_config::schema::TelegramConfig {
            enabled: true,
            ..Default::default()
        },
    );
    let config_arc = Arc::new(RwLock::new(config));
    let channels = collect_configured_channels(&config_arc, "test", &[], None, None);
    assert!(
        channels.iter().all(|c| c.display_name != "Telegram"),
        "Telegram must be absent from collect_configured_channels when \
             channel-telegram feature is not compiled in"
    );
}
