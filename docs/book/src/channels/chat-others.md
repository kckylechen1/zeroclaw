# Other Chat Platforms

The chat transports that used to live on this page (iMessage, IRC,
Lark/Feishu, Twitch, WeChat personal iLink Bot, DingTalk, QQ, and Mochat)
are RETIRED and their schemas are removed: the runners went with the
`zeroclaw-channels` crate (#579, #581) and the schemas were deleted under
[#584](https://github.com/kckylechen1/zeroclaw/issues/584) Slice 3.
Leftover sections tombstone at load with a structured warning.

## Live chat surfaces

- **Telegram**; the gateway bridge (`zeroclaw-bridge-telegram`); see
  [Channels overview](./overview.md). Long polling is the default; no public
  URL required. Streaming draft edits are supported but capped by Telegram's
  rate limit; tune `draft_update_interval_ms` if you see "Too Many Requests".

  {{#peer-group telegram}}

- **Webhook**; gateway ingress; see [Webhooks](./webhook.md).
- **Email / Discord**; retained tool configuration (see their pages), not
  bot runners.
- **CLI / ACP**; always-on local surfaces.

## Reply pacing fields

Channel configs still carry `reply_min_interval_secs` and
`reply_queue_depth_max` schema fields (`0` disables), and agent-side pacing
config remains in use elsewhere. The orchestrator-side per-recipient pacing
wrapper this page used to describe was part of the retired in-process
runners; a nonzero floor on a section nothing consumes has no effect.

## See also

- [Channels overview](./overview.md)
