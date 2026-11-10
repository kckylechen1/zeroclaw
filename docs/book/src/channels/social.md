# Social Channels

The social transports (Bluesky, Nostr, Twitter/X, Reddit) are RETIRED and
their schemas are removed: the runners went with the `zeroclaw-channels`
crate (#579, #581) and the schemas were deleted under
[#584](https://github.com/kckylechen1/zeroclaw/issues/584) Slice 3.
Leftover `[channels.bluesky]`, `[channels.nostr]`, `[channels.twitter]`,
and `[channels.reddit]` sections tombstone at load with a structured
warning. No inbound stream, posting path, or runner exists.
