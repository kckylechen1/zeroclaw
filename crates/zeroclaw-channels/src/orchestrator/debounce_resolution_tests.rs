//! Debounce resolution tests for the channel orchestrator.
use super::resolve_effective_debounce_window;
use std::collections::HashMap;
use std::time::Duration;
use zeroclaw_config::schema::TelegramConfig;

#[test]
fn per_channel_debounce_zero_falls_back_to_global() {
    let mut telegram_configs = HashMap::new();
    telegram_configs.insert(
        "default".into(),
        TelegramConfig {
            debounce_ms: Some(0),
            ..Default::default()
        },
    );
    let duration =
        resolve_effective_debounce_window(1000, "telegram", Some("default"), &telegram_configs);
    assert_eq!(duration, Duration::from_millis(1000));
}

#[test]
fn per_channel_debounce_positive_overrides_global() {
    let mut telegram_configs = HashMap::new();
    telegram_configs.insert(
        "default".into(),
        TelegramConfig {
            debounce_ms: Some(500),
            ..Default::default()
        },
    );
    let duration =
        resolve_effective_debounce_window(1000, "telegram", Some("default"), &telegram_configs);
    assert_eq!(duration, Duration::from_millis(500));
}

#[test]
fn per_channel_debounce_none_falls_back_to_global() {
    let mut telegram_configs = HashMap::new();
    telegram_configs.insert(
        "default".into(),
        TelegramConfig {
            debounce_ms: None,
            ..Default::default()
        },
    );
    let duration =
        resolve_effective_debounce_window(1000, "telegram", Some("default"), &telegram_configs);
    assert_eq!(duration, Duration::from_millis(1000));
}

#[test]
fn non_telegram_channel_uses_global() {
    let telegram_configs = HashMap::new();
    let duration = resolve_effective_debounce_window(1000, "discord", None, &telegram_configs);
    assert_eq!(duration, Duration::from_millis(1000));
}

#[test]
fn unknown_telegram_alias_uses_global() {
    let telegram_configs = HashMap::new();
    let duration =
        resolve_effective_debounce_window(1000, "telegram", Some("nonexistent"), &telegram_configs);
    assert_eq!(duration, Duration::from_millis(1000));
}
