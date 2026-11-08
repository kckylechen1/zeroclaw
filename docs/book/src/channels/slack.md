# Slack

The Slack transport is RETIRED: the in-process channel runners were removed
with the `zeroclaw-channels` crate (#579, #581). A `[channels.slack]`
section still parses today, but nothing consumes it; no runner, bridge,
Node, or Tachi reader exists. Removing the section itself is gated on the
per-schema consumer evidence tracked in [#584](https://github.com/kckylechen1/zeroclaw/issues/584).

Until then, `zeroclaw channel list`, `zeroclaw status`, `zeroclaw self-test`,
and doctor report a populated Slack section as a retired transport so it is
never mistaken for a connectable channel.

## What to do

Remove `[channels.slack]` from your configuration. There is no replacement
Slack transport; the live messaging surfaces are listed in
[Channels overview](./overview.md).

## See also

- [Channels overview](./overview.md)
- [Mattermost](./mattermost.md) (also retired)
