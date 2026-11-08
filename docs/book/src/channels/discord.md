# Discord

The Discord **bot runner** is retired: the in-process channel runners were
removed with the `zeroclaw-channels` crate (#579, #581), and no Discord
bridge binary exists. What is retained is the **tool configuration**:
`[channels.discord]` still feeds a live consumer; the `discord_search`
history tool; so the section is not dead config and is not slated for
removal (#584 keeps it behind an explicit owner migration if that ever
changes).

## What the retained config drives

Runtime tool construction reads the Discord archive configuration: when any
configured alias sets `archive = true`, ZeroClaw opens (or attaches to) the
sidecar `discord.db` memory store in the data directory and registers the
`discord_search` tool, which the agent can use to look up past conversation.

- The archive database is written by your Discord archiver (an external
  bridge or ingest process), not by this daemon: the runner that used to
  record messages was retired with the channels crate.
- `bot_token` and the other authorization fields remain part of the retained
  configuration and validation surface.

{{#config-where channels discord}}

{{#secret-config channels.discord.<alias>.bot_token}}

## Configuration

The full field list, derived from the live schema:

{{#config-fields channels.discord}}

## Identity and peers

{{#peer-group discord}}

`guild_ids` / `channel_ids` scope which servers and channels the
configuration describes (empty means all).

## See also

- [Email](./email.md); the other retained tool configuration
- [Channels overview](./overview.md)
