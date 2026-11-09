# Channels: Overview

A **channel** is a messaging surface the agent talks through. One ZeroClaw instance can bind multiple channels simultaneously; but since the in-process channel runners were retired with the `zeroclaw-channels` crate (#579, #581), the live channel surfaces are the ones that run **through the gateway**: bridges over `/ws/chat`, webhook ingress, the always-on CLI, ACP sessions, and the voice-duplex Node path.

An agent lists the channels it answers on; see [Agents](../agents/overview.md) for how channels attach to an agent (and how a [peer group](./peer-groups.md) lets agents on a shared channel address each other).

## Bridges

A **bridge** is a channel that runs as its own process and attaches to the gateway's `/ws/chat` socket, the same way `zeroclaw chat` does, instead of being compiled into the core (see [ADR-013](../architecture/decisions/ADR-013-channels-as-gateway-clients.md)). The first is the Telegram bridge, `zeroclaw-bridge-telegram`, built from `bridges/telegram` in the repository (`cargo build -p zeroclaw-bridge-telegram`). It serves one owner's private chat and maps it to a gateway session (`main` by default), so a conversation continues between Telegram and `zeroclaw chat -s main`. Its flags and behavior are in `bridges/telegram/README.md`. A bridge authenticates with its own token from `zeroclaw gateway bridge add <name>`, and receives cron output, heartbeat alerts and `notify` messages through the gateway's outbox; see [Channel bridges](../gateway/api.md#channel-bridges).

## Live surfaces

| Surface | Where it runs | Guide |
|---|---|---|
| CLI | Local stdin/stdout, always on | always on (no page) |
| Telegram | Gateway bridge (`zeroclaw-bridge-telegram`) | [Other chat platforms](./chat-others.md) |
| Webhook | Gateway ingress (`[channels.webhook]`) | [Webhooks](./webhook.md) |
| ACP (Agent Client Protocol) | JSON-RPC 2.0 over stdio: editor/IDE sessions | [ACP](./acp.md) |
| Voice duplex | Edge devices joining through the gateway as Nodes (`[channels.voice_duplex]`) | [Voice & telephony](./voice.md) |
| Gateway REST/WS | HTTP + WebSocket (`gateway`, enabled by default) | [Gateway](../gateway/api.md) |

### Retained tool configuration

Email and Discord are **not transports anymore, and not dead config either**: their `[channels.*]` sections feed live tools and are kept deliberately (#584).

| Surface | What it drives | Guide |
|---|---|---|
| Email | `email_search` / `email_read` tools (IMAP, read-only) + email authentication | [Email](./email.md) |
| Discord | `discord_search` archive-history tool (`archive = true`) | [Discord](./discord.md) |

The `channel-email` root feature forwards to the `email-tools` runtime feature and is part of the default build.

## Retired transports

The transports below lost their in-process runners with #579/#581. Their
`[channels.<kind>]` sections still parse; schema retirement is a separate,
evidence-gated step tracked in [#584](https://github.com/kckylechen1/zeroclaw/issues/584); but nothing consumes them, and every CLI surface (`zeroclaw channel list`, `status`, `self-test`, doctor) reports a populated one as a **retired transport** rather than a connectable channel.

`whatsapp`, `line`, `dingtalk`, `wecom`, `wecom_ws`, `wechat`, `qq`,
`twitter`, `mochat`, `nostr`, `clawdtalk`, `reddit`, `bluesky`, `git`,
`voice_call`, `voice_wake`, `amqp` (their schemas retire in bounded #584
slices; `slack`, `mattermost`, `imessage`, `matrix`, `signal`, `irc`,
`twitch`, and `lark` schemas are already removed and tombstone at load).

Per-family retirement pages (they keep their old links): [LINE](./line.md) · [WhatsApp](./whatsapp.md) · [Other chat platforms](./chat-others.md) (WeChat, DingTalk, QQ, Mochat, Notion) · [Social](./social.md) (Bluesky, Nostr, Twitter/X, Reddit) · [Git](./git.md) · [Voice](./voice.md) (ClawdTalk, Voice Call, Voice Wake) · [AMQP](./amqp.md).

> **Also retired earlier:** the MQTT and filesystem SOP-trigger channels were retired when the SOP run side was removed; their sections are **rejected at config parse** with the migration message ([MQTT](./mqtt.md), [Filesystem](./filesystem.md)). The retired transports above still parse by contrast; that difference is exactly what #584's per-schema evidence step resolves.

For historical context, the old `channel-*` cargo features remain as empty
compatibility stubs (installer preset compatibility); enabling them does not
bring a runner back. `channels-full`/`--preset full` therefore no longer add
channels; the live surface set is the table above.

## Configuration

Channel instances are configured under `[channels.<type>.<alias>]`, with `default` as the common first alias. Set them through any config surface:

{{#config-where channels}}

Secrets (bot tokens, API keys, passwords) are stored encrypted; set them through the gateway or `zeroclaw config set` (masked), never in plaintext. The `channels` entry on an agent binds a channel alias to that agent. Field names differ per channel; `zeroclaw config schema` is the authoritative list. Fields that recur across many channels:

| Key | What it does |
|---|---|
| `enabled` | On/off without removing the section |
| `mention_only` | Ignore messages that don't @-mention the bot (chat platforms) |
| `proxy_url` | Per-channel proxy (http/https/socks5/socks5h); overrides global `[proxy]` |
| `excluded_tools` | Tools withheld from the model when answering on this channel |
| `draft_update_interval_ms` | Streaming edit cadence (default 500 ms) |
| `approval_timeout_secs` | Seconds to wait for operator approval on `always_ask` tools before auto-denying |

Inbound senders are gated through [peer groups](./peer-groups.md), not a per-channel field.

## Adding a channel

New transports join as gateway bridges or Clients/Nodes (see [ADR-013](../architecture/decisions/ADR-013-channels-as-gateway-clients.md) and [ADR-017](../architecture/decisions/ADR-017-personal-agent-body-edges-and-delegation.md)); the in-process `Channel` implementation path retired with the `zeroclaw-channels` crate. The Telegram bridge (`bridges/telegram`) is the canonical reference.
