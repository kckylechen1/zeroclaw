//! Configurable-generated paths and legacy imports share the API contract.

use zeroclaw_api::config_metadata::{
    ChannelConfig, ChannelInfo, ConfigTab, CredentialSurfaceClass, IntegrationDescriptor,
    NestedOptionEntry, PropKind, SecretFieldInfo,
};
use zeroclaw_config::schema::{Config, TelegramConfig};

#[test]
fn generated_property_and_secret_metadata_use_api_types() {
    let mut channel = TelegramConfig::default();
    channel
        .set_prop("channels.telegram.bot_token", "test-token")
        .unwrap();
    let fields = channel.prop_fields();
    let token = fields
        .iter()
        .find(|field| field.name == "channels.telegram.bot_token")
        .unwrap();
    let kind: PropKind = token.kind;
    let tab: ConfigTab = token.tab;
    assert_eq!(kind, PropKind::String);
    assert_eq!(tab, ConfigTab::Connection);
    assert!(token.is_secret);
    assert!(!token.display_value.contains("test-token"));
    let secrets: Vec<SecretFieldInfo> = channel.secret_fields();
    assert!(
        secrets
            .iter()
            .any(|field| field.name == token.name && field.is_set)
    );

    let provider = zeroclaw_config::schema::OpenAITtsProviderConfig::default();
    let credential = provider
        .prop_fields()
        .into_iter()
        .find(|field| field.name.ends_with(".api_key"))
        .unwrap();
    let class: Option<CredentialSurfaceClass> = credential.credential_class;
    assert_eq!(class, Some(CredentialSurfaceClass::EncryptedSecret));

    // Both compatibility paths must resolve to the canonical API type.
    let _: zeroclaw_config::traits::PropKind = kind;
    let _: zeroclaw_config::config::ConfigTab = tab;
    let _: Option<zeroclaw_config::traits::CredentialSurfaceClass> = class;
}

#[test]
fn channel_and_integration_metadata_remain_derived_from_config() {
    let mut config = Config::default();
    let channels: Vec<ChannelInfo> = config.channels.channels();
    let telegram = channels
        .iter()
        .find(|channel| channel.kind == "telegram")
        .unwrap();
    assert_eq!(telegram.name, <TelegramConfig as ChannelConfig>::name());
    assert_eq!(telegram.desc, <TelegramConfig as ChannelConfig>::desc());
    assert!(!telegram.configured);
    config
        .channels
        .telegram
        .insert("test".into(), TelegramConfig::default());
    let channels: Vec<zeroclaw_config::traits::ChannelInfo> = config.channels.channels();
    assert!(
        channels
            .iter()
            .find(|channel| channel.kind == "telegram")
            .unwrap()
            .configured
    );

    config.browser.enabled = false;
    let before: IntegrationDescriptor = config.browser.integration_descriptor();
    config.browser.enabled = true;
    let after: zeroclaw_config::config::IntegrationDescriptor =
        config.browser.integration_descriptor();
    assert!(!before.active);
    assert!(after.active);
    assert_eq!(before.display_name, after.display_name);
}

#[test]
fn nested_option_metadata_uses_current_config_presence() {
    let mut config = Config::default();
    let before: Vec<NestedOptionEntry> = config.tunnel.nested_option_entries();
    let before = before
        .iter()
        .find(|entry| entry.field == "tailscale")
        .unwrap();
    assert!(!before.present);
    config.tunnel.tailscale = Some(Default::default());
    let after: Vec<zeroclaw_config::traits::NestedOptionEntry> =
        config.tunnel.nested_option_entries();
    let after = after
        .iter()
        .find(|entry| entry.field == "tailscale")
        .unwrap();
    assert!(after.present);
    assert_eq!(before.display_name, after.display_name);
    assert_eq!(before.description, after.description);
}
