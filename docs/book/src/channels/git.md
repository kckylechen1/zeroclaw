# Git

The Git-forge transport is RETIRED: the in-process channel runners were
removed with the `zeroclaw-channels` crate (#579, #581). A `[channels.git]`
section still parses today, but nothing consumes it; no issue/PR polling,
event routing, or runner exists. Removing the section itself is gated on the
per-schema consumer evidence tracked in [#584](https://github.com/kckylechen1/zeroclaw/issues/584).

Until then, `zeroclaw channel list`, `zeroclaw status`, `zeroclaw self-test`,
and doctor report a populated Git section as a retired transport so it is
never mistaken for a connectable channel.

## What to do

Remove `[channels.git]` (and its GitHub App / Gitea token credentials) from
your configuration. There is no replacement Git-forge channel; the live
messaging surfaces are listed in [Channels overview](./overview.md).

## See also

- [Channels overview](./overview.md)
