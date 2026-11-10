---
id: ADR-022
title: One small Node, many devices - speakers, screens, and off-the-shelf voice satellites
date: 2026-10-30
status: proposed
relates-to:
  - ADR-017
  - ADR-019
  - ADR-020
  - ADR-021
  - crates/zeroclaw-yonbo-node
  - crates/zeroclaw-gateway/src/nodes.rs
---

# ADR-022: One Small Node, Many Devices - Speakers, Screens, and Off-the-Shelf Voice Satellites

## Status

Proposed on 2026-10-30. Not accepted until the owner approves it. Merging this document neither accepts the architecture nor activates a device, route, or capability. [#374](https://github.com/kckylechen1/zeroclaw/issues/374) remains the execution index; the slices below are candidates for that queue.

Amended on 2026-11-10 to match the contract design in #630: capability definitions belong to the kernel's catalog, not to a manifest shipped with the device (§3), and a bounded behavior model may run in an owner-host adapter without becoming a second agent (§1, §8).

This record keeps ADR-017, ADR-019, ADR-020, and ADR-021 unchanged. It restores the device direction of #55 (one controller, many Clients and Nodes) for devices beyond the Yonbo X1, and proposes how speakers, screens, and off-the-shelf voice satellites attach to the body.

## Context

- ADR-017 §2 says a speaker, phone, watch, or other computer attaches through the gateway as a Client, a Node, or both. #55 adds that the product is one controller, "not a separate full Agent installed on every device", and that constrained hardware is a Client or Node, not a reason to embed the runtime.
- The owner wants a small program that can run on a smart speaker, a TV, or similar, talks to the body, and uses the body's models.
- Current state in this repository:
  - The gateway's v2 Node path (`/ws/nodes`) has real-process loopback evidence (#514) for device identity, connection generation, revocation, and safe invocation. Its host is currently `src/bin/zeroclaw-node-host.rs` plus `src/node_host.rs`, imports gateway code, and accepts only loopback addresses. This is not a shipped general device SDK or proof of physical-device safety.
  - `zeroclaw-yonbo-node` is a standalone device process with no async runtime or external dependencies. It speaks N3B1 (per-host PSK, HMAC frames, strict sequence numbers), not v2. #61 is closed historical groundwork; #535 tracks the current Yonbo work.
  - ADR-019 §§6 and 8 propose retaining N3B1 on the robot behind an owner-host v2 adapter, and extracting a `zeroclaw-node-host` crate when that real adapter joins the #514 loopback host as the second implementation.
  - ADR-021 proposes one voice adapter at the edge, with capture, bounded local audio processing, and playback on the device. The multi-device voice behavior below is proposed, not delivered by #514.
- What others do:
  - **OpenClaw** documents paired gateway Nodes that advertise device commands, with centrally managed voice-wake configuration. Its [Node](https://docs.openclaw.ai/nodes) and [voice-wake](https://docs.openclaw.ai/nodes/voicewake) contracts are comparisons, not ZeroClaw implementation evidence.
  - **ESPHome** provides a [native API](https://esphome.io/components/api/) and [voice-assistant component](https://esphome.io/components/voice_assistant/). An external client can implement the voice flow, but stock-firmware compatibility must be proven for a pinned device, firmware, protocol, and client version. The [voice-assistant implementation](https://github.com/esphome/esphome/blob/dev/esphome/components/voice_assistant/voice_assistant.cpp) admits one voice-assistant subscriber; this is not a one-client limit for the entire API. [`esphome-native-api`](https://github.com/UbiHome/esphome-native-api) is a candidate Rust client, not yet a validated ZeroClaw dependency.
  - **Echo cancellation** depends on the actual audio path. [Home Assistant Voice PE](https://github.com/esphome/home-assistant-voice-pe) has a dedicated voice-processing pipeline. [`linux-voice-assistant`](https://github.com/OHF-Voice/linux-voice-assistant) supports local wake detection and Linux audio devices, but its [`webrtc.py`](https://github.com/OHF-Voice/linux-voice-assistant/blob/main/linux_voice_assistant/webrtc.py) configures noise suppression and gain control without a playback-reference input; it is not evidence of playback-reference echo cancellation. PipeWire availability or an AEC-capable chip alone does not prove full duplex on a specific device.
  - **Android TV**: Android's [audio-input sharing rules](https://developer.android.com/media/platform/sharing-audio-input) constrain background capture and give some privileged apps priority. A foreground screen with push-to-talk is the initial candidate; always-on capture is not assumed.
  - Closed commercial speakers are out of scope without a supported local interface. #535 is evidence about the particular Yonbo X1 under test, not a promise about arbitrary retail devices.

## Decision

### 1. Devices are thin; conversational authority stays in the body

No device runs a conversational agent, owns a separate persona or personal memory, or holds model-service credentials. Bounded local wake-word, VAD, and audio-processing models are allowed; they do not decide intent, identity, or permission. Devices never call conversational or speech model-service endpoints directly. Speech goes through the voice adapter (ADR-021), and thinking goes through the body.

An owner-host adapter may run a small behavior model for its device (ADR-019 §4, §6). That model chooses among the device's vetted presentation behaviors, such as a face or a light pattern, from conversation and attention state. It is not a conversational agent: it answers nothing, holds no Soul, memory or model-service credential of the body, calls no body tool, and can select nothing outside the device allowlist. It lives in the adapter, not on the device, and the device keeps working in its idle behavior without it. The body owns the same Soul, memory, and rules, but each device receives only the conversation and projections its source is authorized to access.

One inventory identity may associate several links with a physical device. This is an administrative grouping, not a new identity store or authorization source; existing gateway and adapter credential owners remain authoritative for their links. It does not make credentials interchangeable: v2 Node authentication, voice access, ESPHome API encryption, and legacy N3B1 authentication have separate audiences, scopes, and revocation procedures. Each process holds only the credentials for the links it needs.

### 2. Three ways in

| Way in | For | Talks to |
| --- | --- | --- |
| **Our Node program and device adapters** | devices we can run code on: the Yonbo X1, a Raspberry Pi speaker, a Linux box | the body over v2 `/ws/nodes` for capabilities, and the voice adapter for audio; Yonbo retains its N3B1 device process behind the owner-host v2 adapter (ADR-019 §6) |
| **ESPHome satellite** | candidate stock-firmware devices such as Voice PE or ReSpeaker Lite, and compatible Linux satellites | the voice adapter acting as an ESPHome API client; operation without firmware changes is a pinned trial goal |
| **Native app** | Android TV, phones, later others | the same gateway contracts (ADR-019 §4), in whatever language suits the platform |

No device class gets its own protocol into the body. ESPHome satellites never talk to the body directly; the voice adapter translates.

### 3. What our Node program contains

| Module | Does | Used by |
| --- | --- | --- |
| Identity and pairing | v2 identity and connection generation at the gateway-facing host; distinct credentials for audio and any device-side protocol | our Node hosts and their attached devices |
| Capability advertisement | advertises the names of the capabilities and events it implements, and holds the local permission that must also allow each one. The schema, risk class and replay class (ADR-020 §3) are defined in the kernel's capability catalog (#630), never supplied by the device: a device that described its own capability could authorize it | our Node hosts |
| Audio | capture, echo cancellation, voice detection, wake trigger, playback, the local interruption fast path, the mic-open indicator (ADR-021 §2 device duties) | devices with a microphone and speaker |
| Screen | an authorized, read-only attention or conversation projection; no automatic access to another source's history | devices with a screen |
| Device-specific | for example the robot's faces, actions, and light (ADR-019 §6) | each device |

For owner-built devices, prefer one small local program for capture, playback, and device capabilities where their permission boundaries allow it. Host-side adapters remain separate where required. Audio and Node links have distinct, audience-scoped credentials or grants; neither credential authorizes the other protocol. Indicators and the speaker each have one local owner. Hardware mute and privacy state, then actual microphone capture, take precedence over conversation or action animations.

Effective capability follows #55: what the device advertises now, intersected with the approved ceiling, the requester's grants, and the device's local permission. Advertising a capability is not permission to run it.

### 4. Extract with the second consumer

Following ADR-019 §8, the shared program is not designed ahead of time:

- First write the real owner-host v2-to-N3B1 Yonbo adapter. It and the existing #514 loopback host shape the shared v2 host code. Only with those two implementations does that code become a `zeroclaw-node-host` crate, depending on shared protocol types rather than on the gateway crate.
- Retain `zeroclaw-yonbo-node` and N3B1 on the robot, including its per-host keys and local refusal rules. #535 is the current execution evidence; #61 is closed. A native-v2 rewrite of the robot process is not required by this proposal.
- Later, the robot and the first owner-built speaker (§7) are the two physical consumers that can justify extracting shared audio or other device modules. This is a separate extraction from the shared v2 host code.
- Target a small process suitable for the constrained device, without a conversational model runtime. Memory, CPU, latency, and dependency footprint must be measured on that device; a 512 MB target is not a validated result.

### 5. One voice adapter for all devices

- One voice adapter service (ADR-021) can share speech engines and front-harness implementation. Audio buffers, transcript projections, reply context, and playback remain isolated by admitted source, device, conversation, and active turn/window. Sharing a service never merges histories or identifies the person speaking in a room. Playback or interruption receipts annotate the canonical transcript without truncating it. Each source must pass its own admission policy before reaching the body.
- Trial one ESPHome device with pinned firmware, protocol, and client versions. Verify the required voice messages, wake behavior, playback, and interruption; API presence alone proves none of them. Only one voice-assistant subscriber drives that device, so ZeroClaw and Home Assistant cannot simultaneously own its voice pipeline; unrelated API clients are not ruled out.
- If synthesized speech is fetched by URL, the serving endpoint must enforce device/session scope, short expiry, cleanup and revocation, and an authenticated, encrypted fetch path supported by the pinned firmware. URLs and caches must not become a public or persistent audio store. If stock firmware cannot meet these requirements, record that limit and do not enable private-audio delivery on it.
- The initial ESPHome trial targets turn-based interaction; wake and stop-word behavior are verified for that firmware. Our Node path is the candidate for full duplex, gated by ADR-021's actual playback-reference echo-cancellation and interruption evidence. Running a Node process alone is not that evidence.
- Wake words are configured in one place, the owner's configuration, and pushed to devices that support it, as OpenClaw does.

### 6. Several devices in one home

- **Who answers.** Within an admitted wake window, the adapter selects at most one eligible device; the others stay silent. Timing, signal strength, and room priority are arbitration hints, not proof of the owner's identity or permission to join another conversation. If no device is eligible, none answers.
- **Where proactive messages go** (#63). Use the existing outbox and an explicitly authorized destination, subject to content privacy, quiet hours, and delivery policy. Recent activity may rank already eligible targets; it cannot authorize a new room, device, or phone fallback. With no eligible target, hold the notification. Delivery status remains separate from execution status.
- **Shared devices are not authenticated owner surfaces.** Anyone in the room can speak to a speaker or a TV. Device pairing, source isolation, wake detection, and room presence do not authenticate that person. Voice input is never owner evidence and never completes an approval (ADR-020 §8, ADR-021 §7). An authorized screen may show that approval is pending without exposing private request details; approval requires the existing authenticated owner surface and protected approval flow, not merely a phone or a button.
- **Per-device revocation.** Inventory associates the device's separate Node, voice, ESPHome, and N3B1 links. Retirement closes active sessions and audio-fetch grants and revokes each applicable link, reporting incomplete steps honestly. Revoking a gateway v2 identity does not revoke the robot's N3B1 host key; #535 records that separate operation. Revoking one device must not revoke other devices or widen their authority.
- **Cameras and screens.** A device camera, screen capture, or location capability needs a separate, visible permission and a retention rule before it is exposed (#55 invariant 6).

### 7. Device classes and order

| Class | Way in | Notes |
| --- | --- | --- |
| Yonbo X1 robot | retained N3B1 process plus owner-host adapters | first physical device; Client plus Node (ADR-019 §6, ADR-021), subject to #535 evidence |
| Off-the-shelf satellite (Voice PE, ReSpeaker Lite) | ESPHome satellite | candidate stock-firmware trial; compatibility and the actual audio path must be demonstrated |
| Owner-built speaker (Raspberry Pi with a suitable microphone board) | our Node program | second physical consumer for shared audio/device modules; full duplex requires measured proof |
| Android TV | native app | authorized attention and conversation projections, push-to-talk from the remote; no assumed always-on capture |
| Phone | native app or existing chat bridges | an approval surface only with the existing owner authentication and approval contract |
| Closed commercial speakers | none | out of scope unless a supported local interface exists |

Proposed order: robot first; then a pinned ESPHome trial; then an owner-built speaker as the second physical consumer for shared device modules; a TV app only when wanted. #374 determines execution order and acceptance gates.

### 8. Not adopted

- **A conversational model or a full agent on the device.** #55 rules out a separate full agent per device. Bounded wake, VAD, and audio-processing models remain allowed under §1.
- **Devices calling model endpoints directly**, even "just for speed". Speed comes from the voice adapter and its front harness, and from the adapter-side behavior model of §1 for presentation.
- **A persona per device.** One agent everywhere.
- **Home Assistant as a required dependency.** It can coexist, but a satellite has one voice-assistant subscriber at a time.
- **Wyoming as the protocol for this device trial.** The proposed trial uses the ESPHome native API; this does not retire existing speech-service protocols.
- **Reflashing commercial speakers** as a product path.

## Consequences

Positive:

- Authorized devices reach the same body without gaining access to every conversation or private memory projection.
- A successful pinned trial may make stock satellites usable without firmware work.
- One voice adapter service can share speech engines while isolating sources and sessions.
- A device inventory makes the separate link-revocation steps explicit and auditable.
- Shared v2 host code and later physical-device modules each have two concrete consumers before extraction.

Negative:

- Two device paths (our Node program and ESPHome satellites) with different capabilities and audio evidence requirements.
- Implementing the ESPHome voice-assistant client subset and tracking its protocol versions is ongoing work.
- Shared-room privacy and destination policy need explicit configuration; wake arbitration cannot infer owner identity or route authority.
- Each new device class is one more thing to install, update, and revoke.

## Implementation slices

These are proposed slices for #374, each bounded to a reviewable PR: The kernel capability catalog and device events are tracked separately in #630 (S1, S2); the slices below register their capabilities and events there.

1. **Shared v2 host with a real second implementation.** Write the owner-host Yonbo v2-to-N3B1 adapter, then extract shared host code from it and #514 without a gateway dependency, as ADR-019 §8 requires. If reviews need separate extraction and adapter PRs, write the adapter first on a branch to shape the boundary; do not create a speculative crate.
2. **Physical Yonbo integration.** Under #535, verify the adapter against the retained N3B1 process, including per-hop authentication, local refusals, and separate revocation. Follow ADR-019's physical safety gates; a successful protocol exchange alone never enables motion.
3. **ESPHome satellite trial.** Pin one device, firmware, protocol, and client version. Prove source admission, wake, audio input, transcript delivery, scoped and expiring audio retrieval, playback, and interruption. Record unsupported behavior before proposing activation.
4. **Owner-built speaker.** Add its capture/playback path and extract only modules genuinely shared with the robot. Measure resource use and playback-reference echo cancellation before proposing full duplex.
5. **Several devices.** Test wake arbitration, source/conversation isolation, per-device revocation, and explicitly authorized #63 outbox destinations, including no-target and shared-room privacy cases.
6. **Android TV app**, when wanted: authorized attention and conversation projections, push-to-talk, and no voice-based approval.

Before each slice, check upstream for equivalent work and record the result, per `AGENTS.md`.

## Acceptance

This ADR is implemented when:

1. no device runs a conversational agent or owns model-service credentials, a persona, or personal memory; bounded local wake/audio processing is documented, and stopping one device leaves the body and other devices working;
2. the #514 host and a real Yonbo adapter use the shared v2 host code while the robot retains N3B1; physical audio/device modules are extracted only when the robot and an owner-built speaker both use them;
3. two physical devices reach the same body only within their authorized sources and conversations; tests prevent cross-device transcript, reply-context, playback, and private-screen leakage;
4. one pinned stock ESPHome configuration works through the adapter without firmware changes, with documented voice-subscription limits; mismatched, expired, and revoked audio-fetch grants are refused and private audio is not exposed through an insecure fetch path;
5. when eligible devices hear the same wake word, one answers; with no eligible device, none answers; any full-duplex claim has actual playback-reference and interruption evidence;
6. a proactive message reaches at most one explicitly authorized target through #63, or stays pending; recent activity never creates a route or automatic fallback;
7. revocation stops all applicable links and fetch grants for that device without disrupting others, and incomplete multi-link revocation is visible; tests reject credential reuse across protocols and prevent shared-device voice from proving owner identity or completing an approval.

## Open questions

- Which pinned ESPHome firmware, protocol, and client versions cover the required voice messages and authenticated, encrypted audio retrieval? (Slice 3.)
- Should ESPHome satellites also expose their entities (volume, LED, mute) to the body as Node capabilities, through the voice adapter acting as a Node host on their behalf?
- How should the voice adapter choose between devices that hear the same wake word: earliest detection, loudest, or a configured room priority?
- Which explicit device/destination privacy settings should the owner configure when room presence and speaker identity are unverified?
- Which microphone board for the owner-built speaker: an XMOS-based board with hardware echo cancellation, or software echo cancellation on a plainer board?
- Should the Node program be installed and supervised by the body's service manager, or separately? (Shared with ADR-019.)

## References

- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-019: One body, many hands](./ADR-019-one-body-many-hands.md)
- [ADR-020: Durable turns, replay classes, and attention](./ADR-020-durable-turns-replay-and-attention.md)
- [ADR-021: Full-duplex voice at the edge](./ADR-021-full-duplex-voice-at-the-edge.md)
- Issues [#55](https://github.com/kckylechen1/zeroclaw/issues/55) (device direction), [#61](https://github.com/kckylechen1/zeroclaw/issues/61) (closed device-host groundwork), [#63](https://github.com/kckylechen1/zeroclaw/issues/63) (attention and delivery), and [#374](https://github.com/kckylechen1/zeroclaw/issues/374) (execution index)
- [#514](https://github.com/kckylechen1/zeroclaw/pull/514) (loopback v2 Node invocation), [#535](https://github.com/kckylechen1/zeroclaw/issues/535) (Yonbo evidence), and its [separate v2/N3B1 revocation record](https://github.com/kckylechen1/zeroclaw/issues/535#issuecomment-6522227798)
- Primary comparison and implementation sources are linked beside the claims in Context; a future trial must record its exact versions and device evidence.
