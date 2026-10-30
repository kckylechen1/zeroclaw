---
id: ADR-021
title: Full-duplex voice at the edge - audio stays out of the body
date: 2026-10-30
status: proposed
relates-to:
  - ADR-013
  - ADR-017
  - ADR-019
  - ADR-020
  - crates/zeroclaw-yonbo-node
  - crates/zeroclaw-gateway
---

# ADR-021: Full-Duplex Voice at the Edge - Audio Stays Out of the Body

## Status

Proposed on 2026-10-30. Not accepted until the owner approves it.

This record keeps ADR-017, ADR-019, and ADR-020 unchanged. It decides where full-duplex speech (listening while speaking, with interruption) is implemented, and what the body must add to support it.

## Context

- Today's voice path on the Yonbo X1 (#535) is half-duplex and turn-based: the vendor app recognizes speech, sends text to `GPTUrl`, which points at the owner's Mac adapter; the adapter calls the brain and returns `say` audio through `TTSUrl`. By default the owner's boot chain freezes the vendor recognition processes (`asr.xapp`, `audio_capture.x`) and mutes the speaker. The vendor wake word ("元宝") has no gating, so the vendor stack listens all the time.
- The robot has a SmartIC USB microphone that streams regardless of capture switches, a speaker, PulseAudio, an RK3568 (4 × A55), a live NPU, and an unused hardware VAD block. The RGB light is driven by the vendor playback process (`audio_play.xapp`), so audio playback and the light are coupled.
- On the body, the voice surface (#512) renders only the final reply for voice; streamed voice chunks are empty. A spoken reply therefore cannot start until the whole reply is finished. Turn controls (#498) already allow cancelling a turn.
- ADR-020 §8 requires text produced before a tool call (an acknowledgement or confirmation) to reach the surface before the call runs.
- Full duplex needs, in order of dependency: acoustic echo cancellation (AEC) using the speaker signal as a reference, so the robot does not hear itself; voice activity detection (VAD); streaming recognition; end-of-turn detection; streaming synthesis; interruption (barge-in) handling; and a record of what the owner actually heard.
- Two families of solutions exist:
  - End-to-end speech models with realtime APIs (for example Qwen-Omni realtime, Doubao realtime, open full-duplex speech-to-speech models). They listen, reason, call tools, and speak in one model.
  - Cascaded pipelines: streaming recognition, a text brain, streaming synthesis, coordinated by a state machine. Open Chinese components exist for each stage (for example FunASR streaming recognition and CosyVoice streaming synthesis).
- End-to-end models reason and call tools on their own. Used as the brain, they would bypass the Soul, memory, approvals, and Tachi delegation, all of which live in the body (ADR-017 §1, ADR-019 §1).

## Decision

### 1. Cascaded, and the body never handles audio

The brain is always the body's agent loop. Speech is converted to text before it reaches the body, and text is converted to speech after it leaves the body. End-to-end speech models are not used as the brain. They may be used later only as a recognition or synthesis component, with text out or text in.

### 2. Three places, three jobs

| Where | Owns |
| --- | --- |
| Device (robot, user space only) | microphone capture, AEC with the playback reference, VAD, the local interruption fast path, playback, the mic-open light, wake trigger |
| Voice adapter (on an owner-controlled host; the current Mac adapter grows into it) | streaming recognition, end-of-turn detection, the duplex state machine, sentence buffering, streaming synthesis, the `/ws/chat` voice surface connection |
| Body | text turns, streamed spoken text, cancellation, attention state, history |

The splits follow ADR-019 §2: AEC must run next to the speaker and microphone (place to run); recognition and synthesis models are heavy (dependencies); a broken voice pipeline must not affect text surfaces (failure isolation). Both edge processes may be written in any language (ADR-019 §4).

### 3. Echo cancellation runs on the device, continuously

- AEC runs on the device, where the microphone and the playback reference share a clock. It does not run on the adapter host, where network jitter would break the alignment.
- Preferred: PulseAudio's echo-cancel module with the WebRTC method, if the vendor build supports it. Otherwise the device process runs a WebRTC audio-processing or speexdsp canceller itself, and must then own playback so it has the reference.
- AEC stays on for the whole conversation window, not only while the robot speaks, because the room's echo tail continues after playback ends.

### 4. The body streams spoken text

- The voice surface streams spoken text in sentence-sized chunks instead of only the final reply, rendered with the #512 renderer.
- The #512 markup rule applies per chunk: once markup is detected, the voice surface stops speaking that reply and says the localized notice instead.
- Text produced before a tool call (ADR-020 §8) is flushed immediately, so the acknowledgement is spoken while the tool runs.
- This is a change to the voice surface contract, a protected area (`AGENTS.md`). It is versioned and stays behind the default-off `voice-surface` feature.

### 5. Interruption in two steps

1. **Local and immediate.** When the device detects the owner's voice during playback, it lowers the playback volume at once, with no network round trip.
2. **Decided by the adapter.**
   - Sustained speech or recognized words: stop playback, cancel the current turn's foreground work (#498, ADR-020 §5), and start a new owner turn from what was said.
   - Short backchannels ("嗯", "对", "好"): restore the volume and keep speaking.
   - Explicit stop phrases ("停", "等一下", "不是"): stop immediately without waiting for more speech.

Background work, such as a running Tachi job, is not affected by an interruption (ADR-020 §5, §8).

### 6. History records what was heard

- The adapter reports how far playback actually got. The body commits the assistant message only up to that point, with an interrupted marker. Text that was generated but never played is kept for diagnostics, not as conversation history. Otherwise the agent would later believe it had said things the owner never heard.
- Each owner utterance gets a request identity from the adapter (ADR-020 §2), so a reconnect does not create a duplicate turn.
- Accepting a "heard up to" correction is part of the versioned contract change in §4.

### 7. Listening is bounded

- Audio leaves the device only after a wake trigger (a wake word detected on the device, or a touch or button) and only during an open conversation window. The window closes after a silence timeout. The light shows whenever the microphone is open to the adapter.
- Wake detection runs on the device: first on the CPU, later on the NPU or the hardware VAD block if that proves usable.
- Voice input is unverified speaker input (#512): it never becomes owner evidence for learning, and a spoken answer never satisfies an approval (ADR-020 §8).
- The vendor cloud keeps receiving no audio: the vendor recognition path stays frozen.

### 8. Voice state and attention state

The adapter's local duplex state (listening, owner speaking, robot speaking, overlap) drives the robot's speaking and listening behavior directly, because it changes faster than a round trip to the body. The body's attention state (ADR-020 §7: thinking, working, needs-you, done, error) drives everything else. When both apply, the robot speaking or listening takes precedence for the face, and the attention state for the light.

### 9. Engines are adapter configuration

Recognition and synthesis engines are chosen in the adapter's configuration, local or cloud, and can be swapped without touching the body. Candidates to measure, not decisions: FunASR streaming recognition (`paraformer-zh-streaming`) or Fun-ASR-Nano, and CosyVoice streaming synthesis, on the DGX Spark or the Mac; macOS `say` as the fallback. Cloud engines are allowed as an owner choice. Sending audio to a cloud provider is a privacy decision recorded in the configuration.

### 10. Latency targets

Proposed targets, to be measured and adjusted by the owner:

| Moment | Target |
| --- | --- |
| Owner stops speaking to first audio, direct answer | about 1.2 s (p50) |
| Owner stops speaking to acknowledgement, slow work | about 1 s |
| Owner starts speaking to playback lowered | about 200 ms |
| Owner starts speaking to playback stopped, real interruption | about 500 ms |

### 11. Not adopted

- **An end-to-end speech model as the brain.** It would bypass the Soul, memory, approvals, and delegation.
- **AEC on the adapter host.** The reference and microphone would not share a clock.
- **Streaming the room to the host all the time.** Listening is bounded by the wake trigger and the conversation window.
- **The vendor cloud recognition and synthesis path** for ZeroClaw conversations.

## Consequences

Positive:

- The owner can interrupt the robot naturally, and the robot does not answer itself.
- The Soul, memory, approvals, and delegation stay the only brain, whatever voice engines are used.
- Recognition and synthesis engines can be swapped, local or cloud, without a body change.
- Text surfaces keep working when the voice pipeline is down.
- History matches what the owner actually heard.

Negative:

- Two more edge pieces to run and upgrade: the device voice process and the grown adapter.
- The voice surface contract change touches a protected area.
- Echo cancellation quality depends on the vendor audio stack, which cannot be replaced at the kernel level.
- Taking over playback may also mean taking over the light, because the vendor couples them.
- Running a model, recognition, and synthesis on one DGX Spark may exceed its capacity.

## Implementation slices

Each slice is one PR:

1. **Device audio probe.** On the robot: can a new process capture the microphone while the vendor capture processes are frozen; is PulseAudio echo cancellation available; who owns playback and the light; measure how much echo remains. Evidence only, no product change.
2. **Device voice process.** User-space process under `/userdata`: capture, AEC, VAD, playback, light, and an authenticated audio link to the adapter. Its credential follows the ADR-019 revocation rule.
3. **Adapter streaming, half-duplex first.** Streaming recognition, end-of-turn detection, sentence buffering, streaming synthesis. No interruption yet.
4. **Body streaming voice surface.** Sentence-sized spoken chunks, immediate flush of pre-tool text, per-chunk markup rule, "heard up to" commit; versioned, behind `voice-surface`.
5. **Interruption.** Local volume drop, adapter decision, backchannel handling, stop phrases, turn cancellation.
6. **Bounded listening.** Wake trigger, conversation window, mic-open light.

Slices 3 and 4 can land with S5; slice 5 after the owner has used half-duplex streaming for a while. Before each slice, check upstream for equivalent work and record the result, per `AGENTS.md`.

## Acceptance

This ADR is implemented when:

1. the body links no audio library, and stopping the adapter leaves text surfaces working;
2. a long reply played in a quiet room produces no owner turn (the robot does not hear itself);
3. interrupting mid-sentence lowers and then stops playback within the targets, cancels the turn's foreground work, and leaves only the played part in history with an interrupted marker;
4. a short backchannel during playback does not stop it;
5. for a slow request, the acknowledgement is heard before any tool result (ADR-020 acceptance 7);
6. no audio leaves the device outside an open conversation window, and the light is on whenever one is open;
7. voice input never becomes owner evidence and never satisfies an approval, with tests.

## Open questions

- Can a new process capture the microphone while the vendor capture processes are frozen, or do they hold the device? (Slice 1.)
- Is PulseAudio echo cancellation available on the vendor build? (Slice 1.)
- If the device takes over playback, how is the light driven? (Slice 1.)
- Which audio transport: an extension of the N3B1 link, or a separate authenticated stream?
- Local or cloud recognition and synthesis, and can the DGX Spark run model, recognition, and synthesis together?
- Keep "元宝" as the wake word, or use the duck's own name?
- How long should the conversation window stay open after the last speech?

## References

- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-019: One body, many hands](./ADR-019-one-body-many-hands.md)
- [ADR-020: Durable turns, replay classes, and attention](./ADR-020-durable-turns-replay-and-attention.md)
- Issue #535 (robot state, audio and light ownership), PR #512 (voice surface), #498 (turn controls), #499 (adapter acceptance)
- FunASR (`modelscope/FunASR`) and CosyVoice, as candidate engines
