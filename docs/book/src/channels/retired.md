# Retired transports

These transports lost their in-process runners with the `zeroclaw-channels`
retirement (#579, #581). Their `[channels.<kind>]` sections still parse;
schema retirement is a separate, evidence-gated step tracked in
[#584](https://github.com/kckylechen1/zeroclaw/issues/584). CLI surfaces
report a populated one as a retired transport, never as a connectable
channel. The [channels overview](./overview.md) lists the live surfaces.

- Matrix: schema removed (#584 Slice 3); leftover `[channels.matrix]` sections tombstone at load
- Slack: schema removed (#584 Slice 3); leftover `[channels.slack]` sections tombstone at load
- Mattermost: schema removed (#584 Slice 3); leftover `[channels.mattermost]` sections tombstone at load
- LINE: schema removed (#584 Slice 3); leftover `[channels.line]` sections tombstone at load
- Signal: schema removed (#584 Slice 3); leftover `[channels.signal]` sections tombstone at load
- WhatsApp: schema removed (#584 Slice 3); leftover `[channels.whatsapp]` and V1 `[channels_config.whatsapp]` sections warn at load. Authentication, allowlist, policy and session fields have no active consumer; remove the section. No WhatsApp bridge or replacement transport is enabled.
- [Other chat platforms](./chat-others.md): chat transports whose schemas are removed (iMessage, WeChat, DingTalk, Lark/Feishu, QQ, IRC, Mochat, Twitch); Notion is a retired SaaS integration
- [Social](./social.md): Bluesky, Nostr, Twitter/X, Reddit
- Git: schema removed (#584 Slice 3); leftover `[channels.git]` sections tombstone at load
- [Voice](./voice.md): ClawdTalk, Voice Call, Voice Wake
- AMQP: schema removed (#584 Slice 3); leftover `[channels.amqp]` sections tombstone at load
- [MQTT](./mqtt.md) and [Filesystem](./filesystem.md): already rejected at config parse
