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
  - crates/zeroclaw-channels
---

# ADR-019: One Body, Many Hands - When to Split a Crate or Process, and How Messaging Platforms and Devices Attach

## Status

The legacy Yonbo N3B1 crate has been retired from ZeroClaw source; its complete 11-file snapshot is preserved in the [Zeroclaw-Robotic archive](https://github.com/kckylechen1/Zeroclaw-Robotic/tree/main/integration/zeroclaw/source/crates/zeroclaw-yonbo-node). Future device engineering belongs there. This changes neither running devices and keys nor Gateway Node v2, and does not establish a replacement or physical acceptance.

Proposed on 2026-10-30. Not accepted until the owner approves it.

Merging this proposed record makes it available for review; it does not accept
the architecture, activate an implementation slice, or change a protected
contract. #374 remains the sole execution index.

Amended on 2026-11-10 with the device evidence reported in #626 and the contract design in #630: the Node path carries intents and events, not control loops (§4); the vendor preset actions cannot supply the first body-language set (§6, §9, slices 4 and 5); the motor safety floor is still unproven and a reboot moves the robot (§6). Evidence from #626 that the reviewer has not accepted is marked as reported.

This record keeps ADR-013 and ADR-017 unchanged. It writes down a rule both records already follow, so that new crates, processes, bridges, and device adapters are judged the same way. It also records how the next messaging platforms (WeChat, iMessage, Slack) attach after Telegram, and how the first physical body (the Yonbo X1 robot, #535) attaches.

## Context

- ADR-017 made the body one always-on process: gateway, agent loop, Soul and memory, approvals, and scheduler. ADR-013 made messaging platforms out-of-process Clients: `/ws/chat` carries conversation and authorized channel intake; `/ws/bridge` carries proactive delivery for authorized channel bridges.
- The workspace spans multiple crates. `AGENTS.md` treats line and crate counts as budgets, not goals; this proposal names the reasons for a new crate or process.
- The first bridge, `bridges/telegram`, is partly delivered under #377. Its `bridge.rs` mixes platform-neutral work (durable intake receipts and cursor advance, reconnect and backoff, `/stop` target freezing, approval and question handling, streaming throttle, turn end) with direct calls to the Telegram API. A second platform would copy that file.
- Upstream now ships messaging platforms as in-process WASM channel plugins (v0.8.6, `zeroclaw-labs/zeroclaw-plugins`). The WeChat plugin there is a thin iLink HTTP translator: `wechat.rs` builds `getupdates` and `sendmessage` bodies and parses replies; it cannot perform QR login inside the sandbox and carries no media. Plugins depend on the channel orchestrator that ADR-013 removes, and this fork has retired the WASM plugin host (ADR-009). The plugins are a reference for platform API details, not an attachment path.
- Safe Node invocation now works on the production v2 path with a real-process loopback host (#514). The host currently lives in the root package and links the gateway crate.
- The first physical body is the Yonbo X1 robot (#535): Rockchip RK3568, as corrected by the later device-tree inspection on #535, on a LubanCat-2IO-based board, vendor kernel 4.19 that cannot be reflashed (no rescue path), reachable over Wi-Fi by key-only root SSH. Its vendor dialogue path is redirected to an owner-run adapter. The deployed `node3b` prototype and its repository successor `zeroclaw-yonbo-node` speak N3B1 and offer only `info`, `capabilities`, and a single-play allowlisted `face`; the repository successor is not yet installed on the robot.
- Field mapping of the robot's motion chain (#535) found three layers: a separate body MCU on a 115200 UART that owns the motors and power stage, speaks plain ASCII hex frames (4 servos with position and speed, 3 motors with duty and duration, two motorized ears), and reports battery, IMU and pick-up telemetry; a choreography layer of named actions as keyframe timelines in JSON, plus a light table; and the vendor `sports_control` process that plays them. Its only observed upstream link is an anonymous socket pair from the vendor dialogue process.
- The #626 device survey classified the preset library by frame content: 50 actions, of which 49 contain at least one motor frame and one is empty. No preset consists only of light and ear frames. Which body part each of the three motor channels drives is not yet known. #626 also reports that motion commands arrive as D-Bus signals; that is inferred from names, not observed, and conflicts with the socket-pair observation above until a passive capture settles it.
- The likely next messaging platforms differ in kind, not only in API:

| | WeChat (iLink) | iMessage | Slack |
| --- | --- | --- | --- |
| Transport | cloud HTTP long poll | no API: local Messages database plus AppleScript on a signed-in Mac | official API, Socket Mode WebSocket |
| Must run on | any networked host | the Mac signed in to the agent's Apple ID | any networked host |
| Permissions | bot token | Full Disk Access and Automation | bot and app tokens |
| Missed-message recovery | server-side opaque cursor | local, monotonic row id | no cursor; recovery is an explicit history read |
| Edit sent message | no | no | yes |
| Inline buttons | no | no | yes |
| Proposed accepted conversation scope | owner-only direct conversation | owner-only direct conversation | owner-only direct message |

The accepted-sender scope is a bridge policy, not a guarantee about platform,
account administrator, or device access to the conversation.

## Decision

### 1. The core stays one process, and its authority is not replaceable

The body is not split into further processes. Soul, User Model, memory, approvals, the agent loop, the scheduler, and the gateway stay in one process and share state directly. Co-location keeps their ownership boundaries straightforward; one source of truth remains a requirement independently of process placement. Proposals to move any of them into a separate process or service need a new ADR.

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

Always-exposed tool schemas and active prompt sections can add per-request context cost. Additional configuration also adds maintenance and validation surface. Optional or scoped capabilities are included only when enabled and authorized; runtime cost must be measured rather than inferred from crate or configuration-key counts.

### 4. One door, one contract, any language

Conversation Clients enter through the gateway's existing `/ws/chat` contract. Channel bridges use `/ws/chat` for conversation and the versioned durable intake extension from #438; authorized channel bridges additionally use `/ws/bridge` for proactive delivery and its receipts. Scoped device and owner bridges retain their narrower grants and do not acquire channel intake or outbox authority. Node hosts use the v2 Node contract (`/ws/nodes`). A new platform reuses the contract for its role; changes to a protected contract follow `AGENTS.md` and require a separately reviewed slice.

The Node contract carries intents, their results, and device events once #630 adds them. A loop that must react faster than a turn does not run through it. Such a loop lives in the edge process next to the device link, chooses only among that edge's vetted behaviors, and holds no Soul, memory, approval authority or body tools. The body sets the intent; the edge decides the moment-to-moment behavior within it; the device enforces its own floor. The same split applies to any other edge with a fast loop, such as order execution.

Tachi and other MCP services remain body-initiated MCP integrations. They do not become gateway Clients and are not required to speak a bridge protocol. Edges are language-agnostic and use the existing protocol for their role. For example, the Yonbo dialogue adapter is Python, and an iMessage bridge could be Swift.

### 5. Talking to the owner is not acting for the owner

Each platform has two possible roles, implemented differently:

| | Conversation channel (Client) | Acting for the owner (tool) |
| --- | --- | --- |
| Direction | owner and agent | agent to other people |
| Example | the owner asks the agent something on WeChat | "send Mum an iMessage saying I'll be late" |
| Implementation | a Client on `/ws/chat`, with channel intake and outbox only where authorized | a tool call or MCP server, behind the existing authorization and approval policy |

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
  - *Node.* Body language (S8) is exposed at the level of named actions, faces and the mood light, never raw device frames. The proposed device side is the archived `zeroclaw-yonbo-node` (N3B1), extended with an action command that loads a vetted allowlist of action names. A v2 adapter process on the owner's host translates between the body and N3B1. The model sees only an enum of vetted names. Capability names, schemas and risk classes are defined in the kernel's catalog (#630); the device advertises names only.
  - *Behavior loop.* An optional small model in the owner-host adapter may pick faces, ear and light behaviors a few times a second from conversation and attention state (§4). It talks to the device process directly, not through the gateway, and can select nothing outside the device allowlist. When the link or the model is unavailable the device returns to its idle behavior on its own.
  - *Safety floor.* The body MCU, not the SoC, owns the motors, and each motor frame carries its own duration. This is the floor that still holds when the link drops, and it must be proven before it is relied on. The #626 freeze test ran with the robot idle: no frame was written and nothing moved, which shows only that an absent controller causes no motion. It also found no host-loss watchdog on the MCU over 60 seconds. Whether a motor stops when the controller disappears mid-action, and whether the MCU enforces a frame's duration, are still unknown.
  - *Power-on motion.* The vendor stack plays its start-up action, which includes motor frames, about five seconds after power-on. Any reboot therefore moves the robot. This is outside every capability and grant: operating procedure must place the robot where that movement is safe, and no remote path may reboot it unattended.
  - *Motion classes are derived from frame content, not action names.* Actions containing only light and ear frames are low risk. No vendor preset is in that class, so the first body-language set is authored from light and ear frames as new vetted actions, not selected from the presets. Actions with servo frames but no motor frames are allowed on the floor only. Any action containing a motor frame is high risk: unavailable until a lifted-robot test shows the MCU stops motors on its own (deadman and frame duration), and the high-risk grant path is validated. The classification is computed from the vendor JSON, so a vendor update is re-classified rather than trusted.
  - *Local admission.* Before playing an action, the device side checks MCU telemetry and refuses motor actions when the robot is picked up or tilted, and allows only low-risk actions on low battery. This is a last gate that holds even when the body approved the call.
  - *One writer.* The UART has exactly one writer. The device side either uses a named interface of the vendor player, if one exists, or replaces the vendor player entirely, never writes alongside it.
  - The robot may signal that an approval is waiting, but approval itself happens on an authenticated surface (phone, bridge buttons, CLI), never by voice near the robot.

### 7. The bridge contract carries an opaque source cursor

Platform cursors differ: Telegram `update_id` (integer), WeChat `get_updates_buf` (opaque string), iMessage row id (integer), Slack none. A future platform-neutral contract should distinguish upstream cursor data from the ordering and source identity required for durable intake.

At baseline `47baee196`, the v1 intake API uses signed 64-bit integer update IDs and cursors; the `sessions.db` journal stores INTEGER values and enforces ordering, watermarks, and replay-prevention checks. This is a known constraint, not an unverified field type. Supporting an opaque upstream cursor requires a separately reviewed, versioned design that preserves source binding, observed-prefix ordering, durable ACKs, deduplication, and replay-prevention tombstones. This proposal does not authorize changing the protected contract or reinterpreting existing Telegram rows.

### 8. Shared edge code is extracted with the second consumer

`zeroclaw-bridge-core` is not created from Telegram alone. It is extracted while the WeChat bridge is written, from two real implementations. It owns the platform-neutral work listed in the Context and exposes a small platform interface: poll from a cursor, send, optional edit, and a capability description (edits, buttons, message length) that drives degradation. The exact interface is decided in that slice, not here.

The same rule applies to Node hosts. The loopback host from #514 moves into a `zeroclaw-node-host` crate that depends on the shared Node protocol types rather than on the gateway crate, when the Yonbo adapter becomes its second consumer.

### 9. Order

This is the proposed order within edge-adapter work only. The active Web,
delegation, and delivery priorities in #374 remain unchanged; no new platform
is activated here.

1. Telegram: finish #377 acceptance, including the real-bot manual end-to-end run.
2. Yonbo X1: S5 brain switch, then S8 body language (faces and authored light and ear actions first; anything with a servo or motor frame only after its gate in §6).
3. WeChat: a candidate second messaging bridge that tests the opaque cursor, when selected through #374.
4. iMessage: when the owner wants it; the first device-local Client.
5. Slack: only for a concrete need, DM-only.

Other legacy platform implementations remain subject to the retained-consumer census and replacement or explicit-retirement gates in #378. Only after a platform is retired does its legacy section load with an actionable warning and activate nothing; safety-relevant settings follow the #378 migration-or-block policy.

## Consequences

Positive:

- New crates and processes are judged against one short test instead of taste.
- Process boundaries can isolate a failed platform or device; bounded resource use and protocol handling still need validation.
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

1. **Cursor design.** Specify how opaque upstream cursors coexist with the current integer-ordered intake journal. Review source identity, ACK ordering, migration and tombstones before a bounded versioned change; preserve Telegram behavior and existing receipts.
2. **WeChat bridge with core extraction.** Add `bridges/wechat` and extract `zeroclaw-bridge-core` from it and `bridges/telegram`. If the diff is too large for one review, split into "extract core, Telegram unchanged" and "add WeChat", in that order, with the WeChat adapter written first on a branch to shape the interface.
3. **Node host crate with its second consumer.** Extract the #514 root-package host together with the real Yonbo host-side adapter into `zeroclaw-node-host`, depending on shared protocol types rather than the gateway. Preserve existing capability schemas and deadlines; do not create the crate ahead of those two consumers.
4. **Yonbo action classification and trigger path.** The frame-content classification is reported in #626; complete it with the per-channel breakdown (which motor channels each action uses, maximum duty and duration) and identify what each motor channel drives. Settle the trigger path with a passive capture (named vendor interface, or a replacement player that is the only UART writer and parses telemetry).
5. **Yonbo body-language Node.** Add the action command and telemetry admission to a maintained device-side Node in Zeroclaw-Robotic, a Rust N3B1 client, and a v2 adapter exposing faces and authored light and ear actions as enums, registered in the kernel catalog (#630). Servo and motor actions stay unavailable.
6. **Yonbo motion gate.** Lifted-robot deadman and frame-duration test with recorded evidence; only then, and only through the validated high-risk grant path, expose motor actions.
7. **iMessage bridge**, when requested.
8. **Slack DM bridge**, when requested.

Before each slice, check upstream for equivalent work and record the result, per `AGENTS.md`.

## Acceptance

This ADR is implemented for a messaging platform when:

1. its bridge runs as its own process and uses the existing gateway contracts; Rust bridges depend on `zeroclaw-gateway-client` (and `zeroclaw-bridge-core` once it exists) plus their platform SDK, while other languages use equivalent protocol clients; dependency checks refuse body runtime, memory, provider, channel, or tool imports;
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

- How should opaque upstream cursors map to the current integer-ordered journal without losing source binding, durable ACK, or replay-prevention guarantees? (Slice 1.)
- Does the robot's body MCU stop a motor when the controller disappears mid-action, and does it honor each motor frame's duration? The idle-state test in #626 does not answer this. (Slice 6.)
- What does each of the three motor channels drive? If some are not drive wheels, the risk class of the presets that use only those channels may differ. (Slice 4.)
- Does the vendor action player expose any named interface besides the anonymous socket pair? #626 reports D-Bus signal names without an observed message. (Slice 4.)
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
