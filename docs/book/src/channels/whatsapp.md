# WhatsApp

ZeroClaw connects to WhatsApp through the WhatsApp Web backend, configured under `[channels.whatsapp.<alias>]`. It links a regular WhatsApp account through the Web protocol.

An alias starts only when it sets a Web selector: `session_path`, `pair_phone`, `pair_code`, `ws_url`, or `mode = "personal"`. An alias without one is skipped with a warning.

The WhatsApp Cloud API (Meta Business) backend was removed together with its gateway webhook route. Its fields (`access_token`, `phone_number_id`, `verify_token`, `app_secret`, `proxy_url`) are ignored with a `whatsapp_cloud_backend_removed` config warning. Remove them and set a Web selector to keep using the alias.

## Who can talk to the agent

{{#peer-group whatsapp}}

## Web mode

WhatsApp Web mode links a regular WhatsApp account through the optional Web backend. It does not need a Meta Business account. It does need a ZeroClaw build with the `whatsapp-web` feature enabled and a persistent session database path.

On first start, the Web backend pairs the account using QR or pair-code linking (`pair_phone` seeds pair-code linking; leave it unset for QR). Keep `session_path` on persistent storage; removing it forces a fresh device link. Bind the channel to an agent via that agent's `channels` list.

When `interrupt_on_new_message` is enabled, a newer WhatsApp message from the same sender/chat cancels the in-flight response.

## Personal and business behavior

For Web mode, `mode = "personal"` applies separate DM, group, and self-chat policies:

| Field | Values | Effect |
|---|---|---|
| `dm_policy` | `allowlist`, `ignore`, `all` | Controls direct messages |
| `group_policy` | `allowlist`, `ignore`, `all` | Controls group chats |
| `self_chat_mode` | `true`, `false` | Controls the user's self-chat |
| `mention_only` | `true`, `false` | Requires group messages to mention the bot |
| `passive_group_context` | `true`, `false` | Records allowed unaddressed group messages as context only |

The default `mode = "business"` does not apply the personal DM/group policy split. For peer-gated regular-account deployments, use `mode = "personal"` with `dm_policy = "allowlist"` and `group_policy = "allowlist"`.

`passive_group_context = true` is opt-in and applies only to WhatsApp Web group chats. Allowed unaddressed group messages are stored in the room-scoped conversation history without starting an agent turn, sending reactions, downloading media, or calling the model. Later addressed messages in the same group can use that passive context.

## Restricting which groups (`allowed_groups`)

`allowed_groups` (Web mode) scopes the bot to a named set of group chats by JID. It is independent of `mode` - it applies in both business and personal mode, and runs before the chat-type policy. An empty list (the default) permits every group, so existing configs are unchanged. A non-empty list drops every group message whose chat JID matches no entry. **Direct messages always bypass this filter.**

Each entry matches either the full group JID (`123456789012345@g.us`) or the JID user part - the segment before `@` (`123456789012345`) - compared **exactly**, not as a string prefix (so `123` admits `123@g.us` but never `123999@g.us`). This gates group *identity*, which `group_policy` (chat type) and the sender allowlist (sender) do not.

```toml
[channels.whatsapp.myaccount]
enabled = true
session_path = "/var/lib/zeroclaw/wa.db"
# Only operate in these two groups; all other groups are dropped.
allowed_groups = ["120363012345678901@g.us", "120363098765432109"]
```

## Configuration surfaces

{{#config-fields channels.whatsapp}}

{{#config-where channels whatsapp}}

## Start and check

After configuring the channel, start the channel runner:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw channel start
```

</div>

Use `zeroclaw channel doctor` for a first check, and confirm the binary was built with `whatsapp-web`.
