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

Proposed on 2026-10-30. Not accepted until the owner approves it.

This record keeps ADR-017, ADR-019, ADR-020, and ADR-021 unchanged. It restores the device direction of #55 (one controller, many Clients and Nodes) for devices beyond the Yonbo X1, and decides how speakers, screens, and off-the-shelf voice satellites attach to the body.

## Context

- ADR-017 §2 says a speaker, phone, watch, or other computer attaches through the gateway as a Client, a Node, or both. #55 adds that the product is one controller, "not a separate full Agent installed on every device", and that constrained hardware is a Client or Node, not a reason to embed the runtime.
- The owner wants a small program that can run on a smart speaker, a TV, or similar, talks to the body, and uses the body's models.
- Current state in this repository:
  - The gateway's v2 Node path (`/ws/nodes`) works on the production path with a real-process loopback host (#514): device identity, connection generation, revocation, and safe invocation.
  - `zeroclaw-yonbo-node` is a standalone device process with no async runtime or external dependencies. It speaks its own N3B1 protocol (per-host PSK, HMAC frames, strict sequence numbers), not v2. Reconciling it with v2 is tracked in #61.
  - ADR-019 §8 moves the #514 host into a `zeroclaw-node-host` crate when it gets a second consumer.
  - ADR-021 puts all speech work in one voice adapter at the edge, with device duties (capture, echo cancellation, playback) on the device.
- What others do:
  - **OpenClaw** has the same shape: companion apps on macOS, iOS, Android, and watchOS connect to its gateway with a node role, declare commands (camera, location, talk, voice wake), and are paired and approved by the gateway. One wake-word list is owned centrally and pushed to every node. Nodes are peripherals; messages from chat platforms land on the gateway, not on nodes.
  - **Home Assistant** voice satellites now speak the **ESPHome native API** (protobuf over TCP port 6053, optional Noise encryption), on both ESP32 devices (Home Assistant Voice PE, ReSpeaker Lite, ESP32-S3-BOX-3) and Linux (`linux-voice-assistant`). The older `wyoming-satellite` is archived; Wyoming remains the protocol between a pipeline and its speech services. The ESPHome API is not tied to Home Assistant: the device is the server, and any client that implements the voice-assistant subset (answer `VoiceAssistantRequest`, receive 16 kHz `VoiceAssistantAudio`, send pipeline events, serve synthesized speech as an HTTP URL the device fetches) can drive stock firmware without reflashing. A Rust crate exists (`esphome-native-api`, MIT, client and server). Limits: only one client can subscribe to a device at a time; Voice PE starts its wake word only while a client is connected; the voice flow is turn-based, with a stop word for interruption.
  - **Hardware echo cancellation**: Voice PE and the ReSpeaker Lite use an XMOS XU316 voice processor for echo cancellation, noise suppression, and gain control. The ESP32-S3-BOX-3 has software echo cancellation only. A Pi Zero 2 W with a 2-mic HAT has none in hardware; `linux-voice-assistant` uses WebRTC echo cancellation through PipeWire.
  - **Android TV**: since Android 9, an app that is not in the foreground or running a foreground service receives silence from the microphone; since Android 10, privileged apps such as the built-in assistant have priority over ordinary apps. A TV app is therefore suited to a screen and push-to-talk, not always-on listening.
  - Closed commercial speakers (Xiaomi, Tmall Genie, Echo) run locked firmware; the Yonbo X1 only works because its root shell was reachable (#535).

## Decision

### 1. Devices are thin, and they reach the body, never a model

No device holds a model, a persona, memory, or model credentials. A device never calls a model endpoint directly. Speech goes through the voice adapter (ADR-021), and thinking goes through the body, which calls its own models. Whatever device the owner speaks to, the same agent answers, with the same Soul, memory, and rules. A device holds only its own pairing credential, which can be revoked alone.

### 2. Three ways in

| Way in | For | Talks to |
| --- | --- | --- |
| **Our Node program** | devices we can run code on: the Yonbo X1, a Raspberry Pi speaker, a Linux box | the body over v2 `/ws/nodes` for capabilities, and the voice adapter for audio (ADR-021) |
| **ESPHome satellite** | off-the-shelf voice satellites with stock firmware (Voice PE, ReSpeaker Lite, ESP32-S3-BOX-3) and `linux-voice-assistant` | the voice adapter, which acts as their ESPHome API client; no firmware change |
| **Native app** | Android TV, phones, later others | the same gateway contracts (ADR-019 §4), in whatever language suits the platform |

No device class gets its own protocol into the body. ESPHome satellites never talk to the body directly; the voice adapter translates.

### 3. What our Node program contains

| Module | Does | Used by |
| --- | --- | --- |
| Identity and pairing | v2 device identity, connection generation, revocation (#514) | every device |
| Capability manifest | declares each capability with its schema, replay class (ADR-020 §3), risk class, and the local permission that must also allow it | every device |
| Audio | capture, echo cancellation, voice detection, wake trigger, playback, the local interruption fast path, the mic-open indicator (ADR-021 §2 device duties) | devices with a microphone and speaker |
| Screen | the attention state (ADR-020 §7) and the current conversation, read-only | devices with a screen |
| Device-specific | for example the robot's faces, actions, and light (ADR-019 §6) | each device |

A device runs one program. Its audio stream and its Node capabilities share one process and one credential, with two separate links: audio to the voice adapter, capabilities to the body. Indicators and the speaker have one owner inside that process, and the mic-open indicator always wins.

Effective capability follows #55: what the device advertises now, intersected with the approved ceiling, the requester's grants, and the device's local permission. Advertising a capability is not permission to run it.

### 4. Extract with the second consumer

Following ADR-019 §8, the shared program is not designed ahead of time:

- The #514 host becomes `zeroclaw-node-host`, depending on the shared Node protocol types and not on the gateway crate.
- `zeroclaw-yonbo-node` moves onto it, reconciling N3B1 with v2 under #61. The robot is the first consumer.
- The first owner-built speaker (§7) is the second consumer. The shared modules are extracted while it is written, from two real devices.
- The program stays small: suitable for a 512 MB Linux device, with no model runtime and no gateway dependency.

### 5. One voice adapter for all devices

- One voice adapter (ADR-021) serves every device, holding one session per device. The front harness (ADR-021 §10) and the speech engines are shared.
- ESPHome satellites attach through an ESPHome API client inside the voice adapter (for example the `esphome-native-api` crate). Their synthesized speech is served as an HTTP URL from the adapter. Because only one client may subscribe to a satellite, a satellite used with ZeroClaw is not also driven by Home Assistant.
- ESPHome satellites get the turn-based experience with wake word and stop word. Full duplex (ADR-021 §5) is available only on devices running our Node program, where we control playback and echo cancellation.
- Wake words are configured in one place, the owner's configuration, and pushed to devices that support it, as OpenClaw does.

### 6. Several devices in one home

- **Who answers.** When several devices detect the wake word within a short window, the voice adapter picks one (the earliest or strongest detection) and the others stay silent for that turn.
- **Where proactive messages go** (#63). To the device where the owner last spoke within a recent window, otherwise to the phone. Never to every device at once.
- **Shared devices are not trusted speakers.** Anyone in the room can speak to a speaker or a TV. Voice input is never owner evidence and never approves anything (ADR-020 §8, ADR-021 §7). A TV or speaker may show that an approval is waiting; approval happens only on the owner's phone or chat buttons.
- **Per-device identity.** Each device has its own credential and can be revoked alone. Losing one device affects only that device.
- **Cameras and screens.** A device camera, screen capture, or location capability needs a separate, visible permission and a retention rule before it is exposed (#55 invariant 6).

### 7. Device classes and order

| Class | Way in | Notes |
| --- | --- | --- |
| Yonbo X1 robot | our Node program | first; Client plus Node (ADR-019 §6, ADR-021) |
| Off-the-shelf satellite (Voice PE, ReSpeaker Lite) | ESPHome satellite | cheapest second device: no firmware work, hardware echo cancellation |
| Owner-built speaker (Raspberry Pi with an XMOS microphone board, or `linux-voice-assistant`-class hardware) | our Node program | the second consumer for extraction; full duplex possible |
| Android TV | native app | screen for attention and conversation, push-to-talk from the remote; no always-on listening |
| Phone | native app or existing chat bridges | approvals live here |
| Closed commercial speakers | none | out of scope unless a supported local interface exists |

Order: robot first; then an off-the-shelf ESPHome satellite to prove multi-device voice cheaply; then an owner-built speaker as the second Node consumer; a TV app only when wanted.

### 8. Not adopted

- **A model or a full agent on the device.** #55 rules out a separate full agent per device; a device with a model would answer without the Soul, memory, or approvals.
- **Devices calling model endpoints directly**, even "just for speed". Speed comes from the voice adapter and its front harness.
- **A persona per device.** One agent everywhere.
- **Home Assistant as a required dependency.** It can coexist, but a satellite is driven by one client at a time.
- **Wyoming as the device protocol.** Its satellite program is archived; the current device protocol is the ESPHome native API.
- **Reflashing commercial speakers** as a product path.

## Consequences

Positive:

- Any device the owner talks to is the same agent.
- Off-the-shelf satellites work without firmware work, so a second room is cheap.
- One voice adapter and one set of speech engines serve the whole home.
- Losing or retiring a device is one revocation.
- The shared Node program is shaped by two real devices, not by guesswork.

Negative:

- Two device paths (our Node program and ESPHome satellites) with different capabilities: full duplex only on the first.
- Implementing the ESPHome voice-assistant client subset and tracking its protocol versions is ongoing work.
- Choosing which device answers and where proactive messages go can surprise the owner until tuned.
- Each new device class is one more thing to install, update, and revoke.

## Implementation slices

Each slice is one PR:

1. **Node host crate.** Move the #514 host into `zeroclaw-node-host` without a gateway dependency (same as ADR-019 slice 3).
2. **Robot on v2.** Reconcile `zeroclaw-yonbo-node` (N3B1) with v2 under #61, as the first consumer.
3. **ESPHome satellite trial.** In the voice adapter, an ESPHome API client for one stock Voice PE or ReSpeaker Lite: wake, audio in, transcript to the body, synthesized speech served as a URL, stop word. Evidence first, then a PR.
4. **Owner-built speaker.** A Raspberry Pi speaker running our Node program with the audio module; extract the shared modules from it and the robot.
5. **Several devices.** Wake arbitration in the voice adapter; proactive delivery target (#63).
6. **Android TV app**, when wanted: attention and conversation screen, push-to-talk.

Before each slice, check upstream for equivalent work and record the result, per `AGENTS.md`.

## Acceptance

This ADR is implemented when:

1. no device holds a model, model credentials, or a persona, and stopping any device leaves the body and other devices working;
2. two different devices (the robot and one other) are paired, advertise their capabilities, and answer as the same agent with the same memory;
3. a stock ESPHome satellite works through the voice adapter without firmware changes;
4. when two devices hear the same wake word, exactly one answers;
5. a proactive message reaches one device, chosen by the delivery rule;
6. revoking one device stops only that device, and a shared device can never complete an approval, with tests.

## Open questions

- Which ESPHome protocol version does the `esphome-native-api` crate support, and does it cover the voice-assistant messages we need? (Slice 3.)
- Should ESPHome satellites also expose their entities (volume, LED, mute) to the body as Node capabilities, through the voice adapter acting as a Node host on their behalf?
- How should the voice adapter choose between devices that hear the same wake word: earliest detection, loudest, or a configured room priority?
- How does the body know which room the owner is in for proactive delivery, without location tracking?
- Which microphone board for the owner-built speaker: an XMOS-based board with hardware echo cancellation, or software echo cancellation on a plainer board?
- Should the Node program be installed and supervised by the body's service manager, or separately? (Shared with ADR-019.)

## References

- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-019: One body, many hands](./ADR-019-one-body-many-hands.md)
- [ADR-020: Durable turns, replay classes, and attention](./ADR-020-durable-turns-replay-and-attention.md)
- [ADR-021: Full-duplex voice at the edge](./ADR-021-full-duplex-voice-at-the-edge.md)
- Issues #55 (device direction), #61 (device host), #63 (attention and delivery), #514 (v2 Node invocation), #535 (Yonbo robot)
- OpenClaw documentation: Nodes, Voice wake, Discovery and pairing
- ESPHome native API and the Home Assistant Voice PE audio pipeline; `OHF-Voice/linux-voice-assistant`; Rust crate `esphome-native-api` (`UbiHome/esphome-native-api`)
- Android documentation: sharing audio input, and restrictions on starting foreground services from the background
