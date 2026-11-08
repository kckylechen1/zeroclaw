# Social Channels (Bluesky, Nostr, Twitter/X, Reddit)

The social transports are RETIRED: the in-process channel runners were
removed with the `zeroclaw-channels` crate (#579, #581). Their
`[channels.bluesky]`, `[channels.nostr]`, `[channels.twitter]`, and
`[channels.reddit]` sections still parse today, but nothing consumes them;
no inbound stream, posting path, or runner exists. Removing the sections
themselves is gated on the per-schema consumer evidence tracked in
[#584](https://github.com/kckylechen1/zeroclaw/issues/584).

Until then, `zeroclaw channel list`, `zeroclaw status`, `zeroclaw self-test`,
and doctor report a populated social section as a retired transport so it is
never mistaken for a connectable channel.

## What to do

Remove the social `[channels.*]` sections and their credentials from your
configuration. There are no replacement social transports; the live messaging
surfaces are listed in [Channels overview](./overview.md).

The safety guidance that used to live here (gate inbound senders with peer
groups; keep public-facing agents at `Supervised` autonomy or lower) still
applies to every live surface; see
[Peer Groups](./peer-groups.md) and the autonomy settings in the
[Config reference](../reference/config.md).

## See also

- [Channels overview](./overview.md)
- [Peer Groups](./peer-groups.md)
