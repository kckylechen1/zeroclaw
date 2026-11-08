# AMQP

The AMQP transport is RETIRED: the in-process channel runners were removed
with the `zeroclaw-channels` crate (#579, #581). A `[channels.amqp]` section
still parses today (its former `dispatch` field is already a parse-time
tombstone), but nothing consumes it; no broker consumer or runner exists.
Removing the section itself is gated on the per-schema consumer evidence
tracked in [#584](https://github.com/kckylechen1/zeroclaw/issues/584).

Until then, `zeroclaw channel list`, `zeroclaw status`, `zeroclaw self-test`,
and doctor report a populated AMQP section as a retired transport so it is
never mistaken for an event source.

## What to do

Remove `[channels.amqp]` from your configuration. There is no replacement
broker event source; the live ingress surfaces are listed in
[Channels overview](./overview.md).

For the historical SOP trigger syntax, routing-key matching, and the retired
dispatch mode, see [SOP Fan-In: AMQP](../sop/fan-in/amqp.md).

## See also

- [Channels overview](./overview.md)
- [SOP Fan-In: AMQP](../sop/fan-in/amqp.md)
