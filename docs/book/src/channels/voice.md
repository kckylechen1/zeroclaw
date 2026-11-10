# Voice & Telephony

The legacy voice transports (ClawdTalk SIP voice, carrier Voice Call, and
local Voice Wake) are RETIRED and their schemas are removed: the runners
went with the `zeroclaw-channels` crate (#579, #581) and the schemas were
deleted under [#584](https://github.com/kckylechen1/zeroclaw/issues/584)
Slice 3. Leftover sections tombstone at load with a structured warning.

The live voice surface is the voice-duplex Node path (see the Nodes
documentation).
