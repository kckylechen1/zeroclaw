//! Enumerate the channel types compiled into this binary.

use zeroclaw_api::config_metadata::ChannelInfo;
use zeroclaw_config::schema::ChannelsConfig;

struct ChannelCompileSpec {
    /// Display name from `ChannelsConfig::channels()`, when the channel lives
    /// in the schema channel inventory. ACP is configured under `[acp]`, so it
    /// participates in type readiness without appearing in `compiled_channels`.
    schema_name: Option<&'static str>,
    /// Accepted config/API type keys. Include legacy underscore aliases where
    /// earlier channel references allowed them.
    type_keys: &'static [&'static str],
    compiled: bool,
}

// Single source of truth for both display inventory and per-config type
// readiness. Keep this schema-backed: tests assert enabled display names exist
// in the config crate's canonical channel inventory and that keys do not drift.
const CHANNEL_COMPILE_SPECS: &[ChannelCompileSpec] = &[
    ChannelCompileSpec {
        schema_name: Some("Telegram"),
        type_keys: &["telegram"],
        compiled: cfg!(feature = "channel-telegram"),
    },
    ChannelCompileSpec {
        schema_name: Some("Discord"),
        type_keys: &["discord"],
        compiled: cfg!(feature = "channel-discord"),
    },
    ChannelCompileSpec {
        schema_name: Some("Slack"),
        type_keys: &["slack"],
        compiled: cfg!(feature = "channel-slack"),
    },
    ChannelCompileSpec {
        schema_name: Some("Mattermost"),
        type_keys: &["mattermost"],
        compiled: cfg!(feature = "channel-mattermost"),
    },
    ChannelCompileSpec {
        schema_name: Some("iMessage"),
        type_keys: &["imessage"],
        compiled: cfg!(feature = "channel-imessage"),
    },
    ChannelCompileSpec {
        schema_name: Some("Matrix"),
        type_keys: &["matrix"],
        compiled: cfg!(feature = "channel-matrix"),
    },
    ChannelCompileSpec {
        schema_name: Some("Signal"),
        type_keys: &["signal"],
        compiled: cfg!(feature = "channel-signal"),
    },
    ChannelCompileSpec {
        // `[channels.whatsapp.<alias>]` is served only by the WhatsApp Web
        // backend, so the config type key shares its compile flag.
        schema_name: Some("WhatsApp Web"),
        type_keys: &["whatsapp", "whatsapp-web", "whatsapp_web"],
        compiled: cfg!(feature = "whatsapp-web"),
    },
    ChannelCompileSpec {
        schema_name: Some("Email"),
        type_keys: &["email"],
        compiled: cfg!(feature = "channel-email"),
    },
    ChannelCompileSpec {
        schema_name: Some("IRC"),
        type_keys: &["irc"],
        compiled: cfg!(feature = "channel-irc"),
    },
    ChannelCompileSpec {
        schema_name: Some("Twitch"),
        type_keys: &["twitch"],
        compiled: cfg!(feature = "channel-twitch"),
    },
    ChannelCompileSpec {
        schema_name: Some("Lark"),
        type_keys: &["lark", "feishu"],
        compiled: cfg!(feature = "channel-lark"),
    },
    ChannelCompileSpec {
        schema_name: Some("DingTalk"),
        type_keys: &["dingtalk"],
        compiled: cfg!(feature = "channel-dingtalk"),
    },
    ChannelCompileSpec {
        schema_name: Some("WeCom"),
        type_keys: &["wecom"],
        compiled: cfg!(feature = "channel-wecom"),
    },
    ChannelCompileSpec {
        schema_name: Some("WeCom WebSocket"),
        type_keys: &["wecom-ws", "wecom_ws"],
        compiled: cfg!(feature = "channel-wecom-ws"),
    },
    ChannelCompileSpec {
        schema_name: Some("WeChat"),
        type_keys: &["wechat"],
        compiled: cfg!(feature = "channel-wechat"),
    },
    ChannelCompileSpec {
        schema_name: Some("QQ Official"),
        type_keys: &["qq"],
        compiled: cfg!(feature = "channel-qq"),
    },
    ChannelCompileSpec {
        schema_name: Some("Nostr"),
        type_keys: &["nostr"],
        compiled: cfg!(feature = "channel-nostr"),
    },
    ChannelCompileSpec {
        schema_name: Some("ClawdTalk"),
        type_keys: &["clawdtalk"],
        compiled: cfg!(feature = "channel-clawdtalk"),
    },
    ChannelCompileSpec {
        schema_name: Some("Reddit"),
        type_keys: &["reddit"],
        compiled: cfg!(feature = "channel-reddit"),
    },
    ChannelCompileSpec {
        schema_name: Some("Bluesky"),
        type_keys: &["bluesky"],
        compiled: cfg!(feature = "channel-bluesky"),
    },
    ChannelCompileSpec {
        schema_name: Some("Git"),
        type_keys: &["git"],
        compiled: cfg!(feature = "channel-git"),
    },
    ChannelCompileSpec {
        schema_name: Some("X/Twitter"),
        type_keys: &["twitter"],
        compiled: cfg!(feature = "channel-twitter"),
    },
    ChannelCompileSpec {
        schema_name: Some("Mochat"),
        type_keys: &["mochat"],
        compiled: cfg!(feature = "channel-mochat"),
    },
    ChannelCompileSpec {
        schema_name: Some("LINE"),
        type_keys: &["line"],
        compiled: cfg!(feature = "channel-line"),
    },
    ChannelCompileSpec {
        schema_name: Some("Voice Call"),
        type_keys: &["voice-call", "voice_call"],
        compiled: cfg!(feature = "channel-voice-call"),
    },
    ChannelCompileSpec {
        schema_name: Some("VoiceWake"),
        type_keys: &["voice-wake", "voice_wake"],
        compiled: cfg!(feature = "voice-wake"),
    },
    ChannelCompileSpec {
        schema_name: Some("AMQP"),
        type_keys: &["amqp"],
        compiled: cfg!(feature = "channel-amqp"),
    },
    ChannelCompileSpec {
        schema_name: Some("Webhook"),
        type_keys: &["webhook"],
        compiled: cfg!(feature = "channel-webhook"),
    },
    ChannelCompileSpec {
        schema_name: None,
        type_keys: &["acp-server", "acp_server"],
        compiled: cfg!(feature = "channel-acp-server"),
    },
];

fn compiled_channel_names() -> impl Iterator<Item = &'static str> {
    CHANNEL_COMPILE_SPECS
        .iter()
        .filter(|spec| spec.compiled)
        .filter_map(|spec| spec.schema_name)
}

/// Channels available in this binary: compiled feature specs minus retired
/// transports. Retired kinds (empty `channel-*` feature stubs since the
/// runner retirement, #579/#581) must never be listed as connectable — the
/// feature flag alone no longer proves a runner exists.
pub fn compiled_channels(cfg: &ChannelsConfig) -> Vec<ChannelInfo> {
    cfg.channels()
        .into_iter()
        .filter(|info| !info.retired && compiled_channel_names().any(|name| name == info.name))
        .collect()
}

/// Live channel kinds that are configured but whose feature is not enabled
/// in this build. Retired transports are excluded — they belong to
/// [`configured_retired_channels`], not to a rebuild hint (rebuilding with
/// their stub feature would not bring a runner back).
pub fn configured_uncompiled_channels(cfg: &ChannelsConfig) -> Vec<ChannelInfo> {
    cfg.channels()
        .into_iter()
        .filter(|info| info.configured && !info.retired && !is_channel_type_compiled(info.kind))
        .collect()
}

/// Retired transports with a populated `[channels.<kind>]` section. The
/// section still parses (schema retirement is evidence-gated in #584), but
/// nothing consumes it — surfaces report these distinctly instead of
/// presenting them as configured channels.
pub fn configured_retired_channels(cfg: &ChannelsConfig) -> Vec<ChannelInfo> {
    cfg.channels()
        .into_iter()
        .filter(|info| info.configured && info.retired)
        .collect()
}

/// Returns whether a schema channel type key is compiled into this binary.
/// Accepts both kebab-case keys emitted by the config schema and legacy
/// underscore spellings used in channel references.
pub fn is_channel_type_compiled(channel_type: &str) -> bool {
    for spec in CHANNEL_COMPILE_SPECS {
        if spec.type_keys.contains(&channel_type) {
            return spec.compiled;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{CHANNEL_COMPILE_SPECS, ChannelsConfig};
    use super::{
        compiled_channels, configured_retired_channels, configured_uncompiled_channels,
        is_channel_type_compiled,
    };
    use std::collections::BTreeSet;

    #[cfg(all(
        feature = "channel-telegram",
        feature = "channel-email",
        feature = "channel-webhook",
        feature = "channel-acp-server",
        feature = "channel-discord"
    ))]
    #[test]
    fn channel_type_compilation_tracks_enabled_features() {
        assert!(is_channel_type_compiled("telegram"));
        assert!(is_channel_type_compiled("email"));
        assert!(is_channel_type_compiled("webhook"));
        assert!(is_channel_type_compiled("acp-server"));
        assert!(is_channel_type_compiled("discord"));
        // Retired channel types (inbound webhook routes removed) resolve
        // as unknown, never as compiled.
        for retired in ["linq", "wati", "nextcloud_talk", "gmail_push"] {
            assert!(!is_channel_type_compiled(retired), "{retired}");
        }
    }

    #[test]
    fn compiled_channel_names_are_schema_names() {
        let cfg = ChannelsConfig::default();
        let schema_names: BTreeSet<_> = cfg.channels().into_iter().map(|info| info.name).collect();

        for name in CHANNEL_COMPILE_SPECS
            .iter()
            .filter_map(|spec| spec.schema_name)
        {
            assert!(
                schema_names.contains(name),
                "compiled channel name `{name}` is missing from ChannelsConfig::channels()"
            );
        }
    }

    #[test]
    fn compiled_channels_match_expected_schema_names() {
        let cfg = ChannelsConfig::default();
        let actual: BTreeSet<_> = compiled_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();
        let expected: BTreeSet<_> = CHANNEL_COMPILE_SPECS
            .iter()
            .filter(|spec| spec.compiled)
            .filter(|spec| !zeroclaw_config::schema::is_retired_transport(spec.type_keys[0]))
            .filter_map(|spec| spec.schema_name)
            .collect();

        assert_eq!(actual, expected);
    }

    #[test]
    fn configured_uncompiled_channels_uses_schema_config_status() {
        // Webhook is a live surface: with its feature off it is exactly
        // "configured but not compiled" — a rebuild genuinely fixes it.
        let mut cfg = ChannelsConfig::default();
        cfg.webhook.insert(
            "default".to_string(),
            zeroclaw_config::schema::WebhookConfig::default(),
        );

        let names: BTreeSet<_> = configured_uncompiled_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();

        assert_eq!(
            names.contains("Webhook"),
            !cfg!(feature = "channel-webhook")
        );
    }

    #[test]
    fn retired_transports_are_never_compiled_or_uncompiled() {
        let mut cfg = ChannelsConfig::default();
        cfg.slack.insert(
            "default".to_string(),
            zeroclaw_config::schema::SlackConfig::default(),
        );

        let compiled: BTreeSet<_> = compiled_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();
        let uncompiled: BTreeSet<_> = configured_uncompiled_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();
        let retired: BTreeSet<_> = configured_retired_channels(&cfg)
            .into_iter()
            .map(|info| info.name)
            .collect();

        assert!(!compiled.contains("Slack"));
        // A configured Slack section is retired, never "not compiled" —
        // even when its stub feature happens to be enabled, rebuilding
        // would not bring a runner back.
        assert!(!uncompiled.contains("Slack"));
        assert!(retired.contains("Slack"));
    }

    #[test]
    fn channel_type_compilation_matches_inventory_specs() {
        for spec in CHANNEL_COMPILE_SPECS {
            for key in spec.type_keys {
                assert_eq!(
                    is_channel_type_compiled(key),
                    spec.compiled,
                    "channel type key `{key}` drifted from its compile spec"
                );
            }
        }

        assert!(!is_channel_type_compiled("not-a-channel"));
    }

    #[test]
    fn channel_compile_specs_do_not_duplicate_entries() {
        let mut seen_names = BTreeSet::new();
        let mut seen_keys = BTreeSet::new();

        for spec in CHANNEL_COMPILE_SPECS {
            if let Some(name) = spec.schema_name {
                assert!(
                    seen_names.insert(name),
                    "compiled channel name `{name}` appears more than once"
                );
            }

            for key in spec.type_keys {
                assert!(
                    seen_keys.insert(*key),
                    "compiled channel type key `{key}` appears more than once"
                );
            }
        }
    }

    #[test]
    fn channel_compile_specs_cover_schema_channel_types() {
        let out_of_scope = BTreeSet::from([
            // Voice duplex is a gateway event-stream config surface, not a
            // Channel implementation with its own feature.
            "voice_duplex",
        ]);

        for channel_type in zeroclaw_config::schema::v2::V3_CHANNEL_TYPES {
            if out_of_scope.contains(channel_type) {
                continue;
            }
            let kebab = channel_type.replace('_', "-");
            assert!(
                CHANNEL_COMPILE_SPECS.iter().any(|spec| spec
                    .type_keys
                    .iter()
                    .any(|key| *key == *channel_type || *key == kebab)),
                "schema channel type `{channel_type}` is missing from CHANNEL_COMPILE_SPECS"
            );
        }
    }
}
