---
id: ADR-019
title: One body, many hands - when to split a crate or process, and how messaging platforms attach
date: 2026-10-30
status: proposed
relates-to:
  - ADR-011
  - ADR-013
  - ADR-017
  - bridges/telegram
  - crates/zeroclaw-gateway-client
  - crates/zeroclaw-channels
---

# ADR-019: One Body, Many Hands - When to Split a Crate or Process, and How Messaging Platforms Attach

## Status

Proposed on 2026-10-30. Not accepted until the owner approves it.

This record keeps ADR-013 and ADR-017 unchanged. It writes down a rule both records already follow, so that new crates, processes, and bridges are judged the same way, and it records how the next messaging platforms (WeChat, iMessage, Slack) attach after Telegram.

## Context

- ADR-017 made the body one always-on process: gateway, agent loop, Soul and memory, approvals, and scheduler. ADR-013 made messaging platforms out-of-process bridges that speak `/ws/chat` or `/ws/bridge`.
- The workspace has 23 crates under `crates/`. `AGENTS.md` treats line and crate counts as budgets, not goals, but gives no test for when a new crate or process is justified.
- The first bridge, `bridges/telegram`, is partly delivered under #377. Its `bridge.rs` mixes platform-neutral work (durable intake receipts and cursor advance, reconnect and backoff, `/stop` target freezing, approval and question handling, streaming throttle, turn end) with direct calls to the Telegram API. A second platform would copy that file.
- Upstream now ships messaging platforms as in-process WASM channel plugins (v0.8.6, `zeroclaw-labs/zeroclaw-plugins`). The WeChat plugin there is a thin iLink HTTP translator: `wechat.rs` builds `getupdates` and `sendmessage` bodies and parses replies; it cannot perform QR login inside the sandbox and carries no media. Plugins depend on the channel orchestrator that ADR-013 removes, and this fork has retired the WASM plugin host (ADR-009). The plugins are a reference for platform API details, not an attachment path.
- The likely next platforms differ in kind, not only in API:

| | WeChat (iLink) | iMessage | Slack |
| --- | --- | --- | --- |
| Transport | cloud HTTP long poll | no API: local Messages database plus AppleScript on a signed-in Mac | official API, Socket Mode WebSocket |
| Must run on | any networked host | the Mac signed in to the agent's Apple ID | any networked host |
| Permissions | bot token | Full Disk Access and Automation | bot and app tokens |
| Missed-message recovery | server-side opaque cursor | local, monotonic row id | no cursor; recovery is an explicit history read |
| Edit sent message | no | no | yes |
| Inline buttons | no | no | yes |
| Who else can read the conversation | owner only | owner only | possibly colleagues |

## Decision

### 1. The core stays whole

The body is not split further. Soul, User Model, memory, approvals, the agent loop, the scheduler, and the gateway stay in one process and share state directly. They read each other on every turn, and the one-source-of-truth rule depends on them being together. Proposals to move any of them into a plugin, a separate service, or a second process need a new ADR.

### 2. Edges split only for a named reason

A new process or crate at the edge is justified only by one of these reasons, named in the PR and the issue:

| Reason | Example |
| --- | --- |
| Different permissions | the iMessage bridge needs Full Disk Access; the body must not have it |
| Different place to run | iMessage must run on the signed-in Mac; Nodes run on devices |
| Failure must not spread | a broken WeChat API must not stop Telegram, voice, or L0 work |
| Heavy or rarely used dependencies | platform SDKs and QR libraries stay out of the body |

If none applies, the code stays where it runs.

### 3. Three levels of splitting, chosen by cost

| Level | Cost | Use when |
| --- | --- | --- |
| Separate process (bridge, Node host, Tachi) | high: a protocol, reconnect, ACK, deployment | permissions or place to run differ |
| Separate crate (`zeroclaw-gateway-client`, a future `zeroclaw-bridge-core`) | medium: a manifest and a stable interface | two or more consumers, or a dependency boundary |
| Trait | low to add, costly to change once wrong | two or more real implementations exist |

A new crate should come with a deletion where possible, so the crate count trends down while `zeroclaw-channels` is retired (#378).

### 4. One door, one contract

Every edge process talks to the body only through the gateway. All bridges share one bridge contract (`/ws/bridge` and the durable intake fields from #438). No platform gets its own protocol. A change to that contract is a change to a protected area (`AGENTS.md`) and needs the owner's decision.

### 5. Talking to the owner is not acting for the owner

Each platform has two possible roles, implemented differently:

| | Conversation channel (Client) | Acting for the owner (tool) |
| --- | --- | --- |
| Direction | owner and agent | agent to other people |
| Example | the owner asks the agent something on WeChat | "send Mum an iMessage saying I'll be late" |
| Implementation | a bridge on `/ws/bridge`, owner-only | a tool call or MCP server, behind approval |

This gives a home to the platform tools listed in #378 (`send_via`, `reaction`, `channel_room` and similar): conversation behavior moves into a bridge, and acting behavior becomes a tool or MCP integration, or is retired after caller analysis.

### 6. Platform classes

- **Cloud platforms** (Telegram, WeChat, Slack, Lark): the bridge may run on the body's host or elsewhere.
- **Device-local platforms** (iMessage; later possibly WhatsApp Web or Android SMS): the bridge is an edge Client in the ADR-017 sense and runs on that device. It is always its own process, even on the body's host, because its permissions must not reach the body.

Per platform:

- **WeChat.** A cloud bridge, `bridges/wechat`. The bridge performs QR login itself, which the sandboxed plugin cannot. Platform request and parsing logic may be ported from `zeroclaw-plugins` `plugins/wechat` (MIT OR Apache-2.0) with provenance in the commit. Without message edits or buttons, streaming degrades to a typing indicator and one final message, and approvals degrade to numbered text replies.
- **iMessage.** A device-local bridge on the Mac. The agent uses its own Apple ID, so the owner's messages arrive as incoming messages. The bridge resumes from a persisted row-id cursor through the durable intake path, so messages received while it was down are not skipped. macOS updates may change the Messages database or AppleScript behavior; that risk is accepted and stays inside the bridge.
- **Slack.** A cloud bridge that accepts direct messages from the owner only, with streaming edits and button approvals. Posting into channels is acting for the owner (§5) and goes through a tool with approval. The agent taking part in group conversations as the owner is out of scope.

### 7. The bridge contract carries an opaque source cursor

Platform cursors differ: Telegram `update_id` (integer), WeChat `get_updates_buf` (opaque string), iMessage row id (integer), Slack none. The durable intake contract should treat the source cursor and source message identity as opaque values chosen by the bridge, with ordering and recovery owned by the platform adapter. The shared code stores, acknowledges, and advances; it does not interpret.

Whether the current gateway-side intake storage already allows this is not yet verified. The first slice below checks the `sessions.db` journal types and changes the contract only if needed, as a bounded, versioned change.

### 8. Shared bridge code is extracted with the second platform

`zeroclaw-bridge-core` is not created from Telegram alone. It is extracted while the WeChat bridge is written, from two real implementations. It owns the platform-neutral work listed in the Context and exposes a small platform interface: poll from a cursor, send, optional edit, and a capability description (edits, buttons, message length) that drives degradation. The exact interface is decided in that slice, not here.

### 9. Order

1. Telegram: finish #377 acceptance, including the real-bot manual end-to-end run.
2. WeChat: the owner's daily platform, and the case that tests the opaque cursor.
3. iMessage: when the owner wants it; the first device-local Client.
4. Slack: only for a concrete need, DM-only.

All other platforms in `zeroclaw-channels` are retired under #378: a legacy section loads with one actionable warning and activates nothing, and safety-relevant settings follow the #378 legacy-configuration policy.

## Consequences

Positive:

- New crates and processes are judged against one short test instead of taste.
- A broken or slow platform cannot affect the body or other platforms.
- Device permissions such as Full Disk Access stay with the process that needs them.
- Adding a platform becomes a small adapter once `zeroclaw-bridge-core` exists.
- The conversation-versus-acting split gives the #378 platform tools a clear destination.

Negative:

- Every bridge is one more process to install, start, and upgrade; the one-package install goal of ADR-017 has to cover them.
- Degraded platforms (no edits, no buttons) give a weaker experience than Telegram or Slack.
- The opaque-cursor change, if needed, touches a protected contract.
- iMessage depends on undocumented macOS internals.

## Implementation slices

Each slice is one PR:

1. **Cursor check.** Inspect the gateway intake journal types; if they assume an integer cursor, make cursor and source identity opaque in a versioned contract change. Telegram behavior unchanged.
2. **WeChat bridge with core extraction.** Add `bridges/wechat` and extract `zeroclaw-bridge-core` from it and `bridges/telegram`. If the diff is too large for one review, split into "extract core, Telegram unchanged" and "add WeChat", in that order, with the WeChat adapter written first on a branch to shape the interface.
3. **iMessage bridge**, when requested.
4. **Slack DM bridge**, when requested.

Before each slice, check upstream for equivalent work and record the result, per `AGENTS.md`.

## Acceptance

This ADR is implemented for a platform when:

1. its bridge runs as its own process, depends only on `zeroclaw-gateway-client` (and `zeroclaw-bridge-core` once it exists) plus its platform SDK, and the dependency check refuses runtime, memory, provider, channel, or tool imports;
2. it serves the owner only and rejects other senders before processing;
3. it passes the #377 durable-intake cases against its own cursor type: failure before acceptance, crash after acceptance before ACK, lost ACK, duplicate source message, restart;
4. missing platform capabilities degrade as described in §6 and §8, with tests;
5. any "act for the owner" behavior for that platform is a tool behind approval, not part of the bridge.

## Open questions

- Does the current intake journal store the source cursor as an integer? (Slice 1.)
- How should Slack Socket Mode recover messages missed during a disconnect? To be confirmed against Slack documentation before slice 4.
- Should bridges be launched by the body's service manager, or installed and supervised separately?

## References

- [ADR-013: Channels are out-of-process gateway clients](./ADR-013-channels-as-gateway-clients.md)
- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- Issue #377: Durable Telegram intake, recovery and replacement acceptance
- Issue #378: Replace legacy channel consumers, then remove `zeroclaw-channels`
- `zeroclaw-labs/zeroclaw-plugins` `plugins/wechat` (version 0.4.0), reference for the iLink API
