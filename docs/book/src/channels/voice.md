# Voice & Telephony

The voice transports are RETIRED: ClawdTalk (SIP real-time voice), Voice Call
(carrier voice), and Voice Wake (local wake-word) runners were removed with
the `zeroclaw-channels` crate (#579, #581). Their `[channels.clawdtalk]`,
`[channels.voice_call]`, and `[channels.voice_wake]` sections still parse
today, but nothing consumes them. Removing the sections themselves is gated
on the per-schema consumer evidence tracked in
[#584](https://github.com/kckylechen1/zeroclaw/issues/584); until then the
CLI surfaces report any populated one as a retired transport.

The retained voice path is the **voice-duplex Node** configuration
(`[channels.voice_duplex]`): edge devices join through the gateway as Nodes
(see [ADR-013](../architecture/decisions/ADR-013-channels-as-gateway-clients.md)
and [ADR-017](../architecture/decisions/ADR-017-personal-agent-body-edges-and-delegation.md)).

## TTS (outbound speech synthesis)

TTS is an output service, not an inbound channel. Global defaults live under
`tts`. TTS provider instances are configured under `providers.tts.<type>.<alias>`
(OpenAI, ElevenLabs, Google, Edge, Piper) and selected per agent via the
agent's `tts_provider`. See [Model Providers](../providers/overview.md) for
the provider entries and per-agent wiring. Provider API keys are secrets; set
them through the gateway or `zeroclaw config set`, never in plaintext.

## STT

Speech-to-text is configured separately from voice channels; see the
`[transcription]` config in the [Config reference](../reference/config.md).
Voice surfaces invoke whichever transcription provider is active when they
need to turn audio into text.

## See also

- [Channels overview](./overview.md)
- [ADR-017: personal agent body, edges, delegation](../architecture/decisions/ADR-017-personal-agent-body-edges-and-delegation.md)
