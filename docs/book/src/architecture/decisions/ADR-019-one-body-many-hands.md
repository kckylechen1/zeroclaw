---
id: ADR-019
title: One body, many hands - when to split a crate or process, and how messaging platforms and devices attach
date: 2026-10-30
status: proposed
relates-to:
  - ADR-002
  - ADR-005
  - ADR-011
  - ADR-013
  - ADR-017
  - bridges/telegram
  - crates/zeroclaw-gateway-client
  - crates/zeroclaw-gateway/src/nodes.rs
  - crates/zeroclaw-yonbo-node
  - crates/zeroclaw-channels
---

# ADR-019: One Body, Many Hands - When to Split a Crate or Process, and How Messaging Platforms and Devices Attach

## Status

Proposed on 2026-10-30. Not accepted until the owner approves it.

This record keeps ADR-013 and ADR-017 unchanged. It writes down a rule both records already follow, so that new crates, processes, bridges, and device adapters are judged the same way. It also records how the next messaging platforms (WeChat, iMessage, Slack) attach after Telegram, and how the first physical body (the Yonbo X1 robot, #535) attaches.

## Context

- ADR-017 made the body one always-on process: gateway, agent loop, Soul and memory, approvals, and scheduler. ADR-013 made messaging platforms out-of-process bridges that speak `/ws/chat` or `/ws/bridge`.
- The workspace has 23 crates under `crates/`. `AGENTS.md` treats line and crate counts as budgets, not goals, but gives no test for when a new crate or process is justified.
- The first bridge, `bridges/telegram`, is partly delivered under #377. Its `bridge.rs` mixes platform-neutral work (durable intake receipts and cursor advance, reconnect and backoff, `/stop` target freezing, approval and question handling, streaming throttle, turn end) with direct calls to the Telegram API. A second platform would copy that file.
- Upstream now ships messaging platforms as in-process WASM channel plugins (v0.8.6, `zeroclaw-labs/zeroclaw-plugins`). The WeChat plugin there is a thin iLink HTTP translator: `wechat.rs` builds `getupdates` and `sendmessage` bodies and parses replies; it cannot perform QR login inside the sandbox and carries no media. Plugins depend on the channel orchestrator that ADR-013 removes, and this fork has retired the WASM plugin host (ADR-009). The plugins are a reference for platform API details, not an attachment path.
- Safe Node invocation now works on the production v2 path with a real-process loopback host (#514). The host currently lives in the root package and links the gateway crate.
- The first physical body is the Yonbo X1 robot (#535): Rockchip RK3568 on a LubanCat-2IO-based board, vendor kernel 4.19 that cannot be reflashed (no rescue path), reachable over Wi-Fi by key-only root SSH. Its vendor dialogue path is redirected to an owner-run adapter, and a minimal device process (`zeroclaw-yonbo-node`, N3B1 protocol) offers only `info`, `capabilities`, and a single-play allowlisted `face`.
- Field mapping of the robot's motion chain (#535) found three layers: a separate body MCU on a 115200 UART that owns the motors and power stage, speaks plain ASCII hex frames (4 servos with position and speed, 3 motors with duty and duration, two motorized ears), and reports battery, IMU and pick-up telemetry; a choreography layer of 49 named actions as keyframe timelines in JSON, plus a light table; and the vendor `sports_control` process that plays them. Its only observed upstream link is an anonymous socket pair from the vendor dialogue process.
- The likely next messaging platforms differ in kind, not only in API:

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

### 1. The core stays one process, and its authority is not replaceable

The body is not split into further processes. Soul, User Model, memory, approvals, the agent loop, the scheduler, and the gateway stay in one process and share state directly. They read each other on every turn, and the one-source-of-truth rule depends on them being together. Proposals to move any of them into a separate process or service need a new ADR.

This rule is about processes and authority, not code structure. Inside the body, replaceable implementations behind traits remain normal: memory backends (ADR-005), model providers, and tools (ADR-002). What configuration or an extension can never replace or override is the authority itself: Soul and User Model ownership, approvals, and safety boundaries. An extension, hook, or interceptor may refuse an action or narrow what is allowed. It may never approve an action that needs approval or widen what is allowed.

### 2. Edges split only for a named reason

A new process or crate at the edge is justified only by one of these reasons, named in the PR and the issue:

| Reason | Example |
| --- | --- |
| Different permissions | the iMessage bridge needs Full Disk Access; the body must not have it |
| Different place to run | iMessage must run on the signed-in Mac; Node hosts run on or near devices |
| Failure must not spread | a broken WeChat API must not stop Telegram, voice, or L0 work |
| Heavy or rarely used dependencies | platform SDKs and QR libraries stay out of the body |

If none applies, the code stays where it runs.

### 3. Three levels of splitting, chosen by cost

| Level | Cost | Use when |
| --- | --- | --- |
| Separate process (bridge, Node host, device adapter, Tachi) | high: a protocol, reconnect, ACK, deployment | permissions or place to run differ |
| Separate crate (`zeroclaw-gateway-client`, a future `zeroclaw-bridge-core` or `zeroclaw-node-host`) | medium: a manifest and a stable interface | two or more consumers, or a dependency boundary |
| Trait | low to add, costly to change once wrong | two or more real implementations exist |

A new crate should come with a deletion where possible, so the crate count trends down while `zeroclaw-channels` is retired (#378).

Additions to the core are paid for on every model request: each core tool schema, prompt section, and config key is carried into every turn. The bar for adding to the core is therefore higher than the bar for adding at the edge.

### 4. One door, one contract, any language

Every edge process talks to the body only through the gateway. All bridges share one bridge contract (`/ws/bridge` and the durable intake fields from #438), and all Node hosts share the v2 Node contract (`/ws/nodes`). No platform or device gets its own protocol into the body. A change to either contract is a change to a protected area (`AGENTS.md`) and needs the owner's decision.

Edges are language-agnostic. A bridge, Node host, device adapter, or MCP server may be written in whatever language suits its platform, as long as it speaks the shared contract. For example, the Yonbo dialogue adapter is Python, and an iMessage bridge could be Swift.

### 5. Talking to the owner is not acting for the owner

Each platform has two possible roles, implemented differently:

| | Conversation channel (Client) | Acting for the owner (tool) |
| --- | --- | --- |
| Direction | owner and agent | agent to other people |
| Example | the owner asks the agent something on WeChat | "send Mum an iMessage saying I'll be late" |
| Implementation | a bridge on `/ws/bridge`, owner-only | a tool call or MCP server, behind approval |

This gives a home to the platform tools listed in #378 (`send_via`, `reaction`, `channel_room` and similar): conversation behavior moves into a bridge, and acting behavior becomes a tool or MCP integration, or is retired after caller analysis.

### 6. Platform and device classes

- **Cloud platforms** (Telegram, WeChat, Slack, Lark): the bridge may run on the body's host or elsewhere.
- **Device-local platforms** (iMessage; later possibly WhatsApp Web or Android SMS): the bridge is an edge Client in the ADR-017 sense and runs on that device. It is always its own process, even on the body's host, because its permissions must not reach the body.
- **Embodied devices that cannot be reflashed** (the Yonbo X1 robot): the device keeps its vendor firmware and kernel. Owner code runs only in user space on the device, and anything heavier runs in an adapter process on a host the owner controls.

Per platform and device:

- **WeChat.** A cloud bridge, `bridges/wechat`. The bridge performs QR login itself, which the sandboxed plugin cannot. Platform request and parsing logic may be ported from `zeroclaw-plugins` `plugins/wechat` (MIT OR Apache-2.0) with provenance in the commit. Without message edits or buttons, streaming degrades to a typing indicator and one final message, and approvals degrade to numbered text replies.
- **iMessage.** A device-local bridge on the Mac. The agent uses its own Apple ID, so the owner's messages arrive as incoming messages. The bridge resumes from a persisted row-id cursor through the durable intake path, so messages received while it was down are not skipped. macOS updates may change the Messages database or AppleScript behavior; that risk is accepted and stays inside the bridge.
- **Slack.** A cloud bridge that accepts direct messages from the owner only, with streaming edits and button approvals. Posting into channels is acting for the owner (§5) and goes through a tool with approval. The agent taking part in group conversations as the owner is out of scope.
- **Yonbo X1 robot (#535).** The first physical body, in two roles:
  - *Client.* The vendor dialogue path is redirected to an owner-run adapter on the Mac that speaks the device bridge (#496, #498, #499). Switching the adapter's brain from the interim model to ZeroClaw is slice S5. In ZeroClaw mode the adapter injects no persona or memory of its own; identity comes only from the Soul.
  - *Node.* Body language (S8) is exposed at the level of named actions, faces and the mood light, never raw device frames. The device side is `zeroclaw-yonbo-node` (N3B1), extended with an action command that loads a vetted allowlist of action names. A v2 adapter process on the owner's host translates between the body and N3B1. The model sees only an enum of vetted names.
  - *Safety floor.* The body MCU, not the SoC, owns the motors, and each motor frame carries its own duration. This is the floor that still holds when the link drops, and it must be proven before it is relied on.
  - *Motion classes are derived from frame content, not action names.* Actions containing only light and ear frames are low risk. Actions with servo frames but no motor frames are allowed on the floor only. Any action containing a motor frame is high risk: unavailable until a lifted-robot test shows the MCU stops motors on its own (deadman and frame duration), and the high-risk grant path is validated. The classification is computed from the vendor JSON, so a vendor update is re-classified rather than trusted.
  - *Local admission.* Before playing an action, the device side checks MCU telemetry and refuses motor actions when the robot is picked up or tilted, and allows only low-risk actions on low battery. This is a last gate that holds even when the body approved the call.
  - *One writer.* The UART has exactly one writer. The device side either uses a named interface of the vendor player, if one exists, or replaces the vendor player entirely, never writes alongside it.
  - The robot may signal that an approval is waiting, but approval itself happens on an authenticated surface (phone, bridge buttons, CLI), never by voice near the robot.

### 7. The bridge contract carries an opaque source cursor

Platform cursors differ: Telegram `update_id` (integer), WeChat `get_updates_buf` (opaque string), iMessage row id (integer), Slack none. The durable intake contract should treat the source cursor and source message identity as opaque values chosen by the bridge, with ordering and recovery owned by the platform adapter. The shared code stores, acknowledges, and advances; it does not interpret.

Whether the current gateway-side intake storage already allows this is not yet verified. The first messaging slice below checks the `sessions.db` journal types and changes the contract only if needed, as a bounded, versioned change.

### 8. Shared edge code is extracted with the second consumer

`zeroclaw-bridge-core` is not created from Telegram alone. It is extracted while the WeChat bridge is written, from two real implementations. It owns the platform-neutral work listed in the Context and exposes a small platform interface: poll from a cursor, send, optional edit, and a capability description (edits, buttons, message length) that drives degradation. The exact interface is decided in that slice, not here.

The same rule applies to Node hosts. The loopback host from #514 moves into a `zeroclaw-node-host` crate that depends on the shared Node protocol types rather than on the gateway crate, when the Yonbo adapter becomes its second consumer.

### 9. Order

1. Telegram: finish #377 acceptance, including the real-bot manual end-to-end run.
2. Yonbo X1: S5 brain switch, then S8 body language (low-risk actions, faces and light first; motor actions only after the safety floor is proven).
3. WeChat: the owner's daily platform, and the case that tests the opaque cursor.
4. iMessage: when the owner wants it; the first device-local Client.
5. Slack: only for a concrete need, DM-only.

All other platforms in `zeroclaw-channels` are retired under #378: a legacy section loads with one actionable warning and activates nothing, and safety-relevant settings follow the #378 legacy-configuration policy.

## Consequences

Positive:

- New crates and processes are judged against one short test instead of taste.
- A broken or slow platform or device cannot affect the body or other edges.
- Device permissions such as Full Disk Access, and device credentials such as the robot's N3B1 key, stay with the process that needs them.
- Adding a platform or device becomes a small adapter once the shared edge crates exist.
- The conversation-versus-acting split gives the #378 platform tools a clear destination.
- The robot's vendor choreography gives the agent a body-language vocabulary without any motor-control code in the body.

Negative:

- Every bridge or adapter is one more process to install, start, and upgrade; the one-package install goal of ADR-017 has to cover them.
- Degraded platforms (no edits, no buttons) give a weaker experience than Telegram or Slack.
- The opaque-cursor change, if needed, touches a protected contract.
- iMessage depends on undocumented macOS internals.
- Embodied devices that cannot be reflashed depend on vendor firmware for their safety floor, which limits which capabilities can ever be exposed.
- If the robot's vendor player has no named interface, the device side must replace it and take over its telemetry parsing and vendor reactions (such as the pick-up sound).

## Implementation slices

Each slice is one PR:

1. **Cursor check.** Inspect the gateway intake journal types; if they assume an integer cursor, make cursor and source identity opaque in a versioned contract change. Telegram behavior unchanged.
2. **WeChat bridge with core extraction.** Add `bridges/wechat` and extract `zeroclaw-bridge-core` from it and `bridges/telegram`. If the diff is too large for one review, split into "extract core, Telegram unchanged" and "add WeChat", in that order, with the WeChat adapter written first on a branch to shape the interface.
3. **Node host crate.** Move the #514 host into `zeroclaw-node-host` without a gateway dependency; add per-capability schemas and deadlines.
4. **Yonbo action classification and trigger path.** Classify the vendor action list from frame content; settle the trigger path (named vendor interface, or a replacement player that is the only UART writer and parses telemetry).
5. **Yonbo body-language Node.** Add the action command and telemetry admission to `zeroclaw-yonbo-node`, a Rust N3B1 client, and a v2 adapter exposing low-risk actions, faces and light as enums. Motor actions stay unavailable.
6. **Yonbo motion gate.** Lifted-robot deadman and frame-duration test with recorded evidence; only then, and only through the validated high-risk grant path, expose motor actions.
7. **iMessage bridge**, when requested.
8. **Slack DM bridge**, when requested.

Before each slice, check upstream for equivalent work and record the result, per `AGENTS.md`.

## Acceptance

This ADR is implemented for a messaging platform when:

1. its bridge runs as its own process, depends only on `zeroclaw-gateway-client` (and `zeroclaw-bridge-core` once it exists) plus its platform SDK, and the dependency check refuses runtime, memory, provider, channel, or tool imports;
2. it serves the owner only and rejects other senders before processing;
3. it passes the #377 durable-intake cases against its own cursor type: failure before acceptance, crash after acceptance before ACK, lost ACK, duplicate source message, restart;
4. missing platform capabilities degrade as described in §6 and §8, with tests;
5. any "act for the owner" behavior for that platform is a tool behind approval, not part of the bridge.

It is implemented for an embodied device when:

1. its adapter runs as its own process and does not link the body's runtime;
2. every exposed capability is completed by the device itself, with a schema that cannot express anything outside the device's own vetted allowlist;
3. capability risk classes are computed from device data (for the robot, action frame content), not from names;
4. local admission refuses unsafe actions from device telemetry, with tests;
5. a lost device result is reported as an unknown outcome and never retried with a new device sequence;
6. revoking the adapter's v2 identity and revoking its device-side credential are both documented, and neither alone is described as full revocation.

## Open questions

- Does the current intake journal store the source cursor as an integer? (Slice 1.)
- Does the robot's body MCU stop motors on its own when frames stop, and does it honor each motor frame's duration? (Slice 6.)
- Does the vendor action player expose any named interface besides the anonymous socket pair? (Slice 4.)
- How should Slack Socket Mode recover messages missed during a disconnect? To be confirmed against Slack documentation before slice 8.
- Should bridges and device adapters be launched by the body's service manager, or installed and supervised separately?
- Should revoking a device adapter also revoke its device-side credential in one owner command?

## References

- [ADR-013: Channels are out-of-process gateway clients](./ADR-013-channels-as-gateway-clients.md)
- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- Issue #377: Durable Telegram intake, recovery and replacement acceptance
- Issue #378: Replace legacy channel consumers, then remove `zeroclaw-channels`
- PR #514: Safe v2 invocation and loopback info/echo host
- Issue #535: Yonbo robot state snapshot, including the motion chain mapping
- `zeroclaw-labs/zeroclaw-plugins` `plugins/wechat` (version 0.4.0), reference for the iLink API
