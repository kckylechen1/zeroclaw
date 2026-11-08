# Email

The email **channel runner** is retired: the in-process poller/reply loop was
removed with the `zeroclaw-channels` crate (#579, #581); ZeroClaw no longer
polls IMAP for inbound turns or sends SMTP replies. What is retained is the
**tool configuration**: `[channels.email]` still feeds live consumers; the
`email_search` and `email_read` tools, and email authentication (mailbox
aliases, OAuth2, secrets, allowlists); so the section is not dead config
(#584 keeps it behind an explicit owner migration if that ever changes).

## What the retained config drives

- `email_search`; IMAP SEARCH over the configured mailbox (filter by
  sender, subject keyword, date, unread; folder selectable, default INBOX).
- `email_read`; fetch one message's full content by UID, read-only
  (`BODY.PEEK`; never marks mail as seen).
- Email authentication resolves configured mailbox aliases and secret
  references.

Both tools are gated by the `channel-email` feature (included in the default
build via `email-tools`).

{{#config-where channels email}}

`password` (and `smtp_password` if configured for the relay) are secrets:

{{#secret-config channels.email.<alias>.password}}

## Configuration

The full field list, derived from the live schema:

{{#config-fields channels.email}}

### Gmail notes

- **App passwords required** if 2FA is on; regular account passwords are
  rejected by the IMAP tooling.

## Who the tools answer for

{{#peer-group email}}

Peer-group gating still applies to the identity surfaces that consume the
mailbox aliases.

## Gmail Push (removed)

The `gmail_push` channel depended on the gateway's `/webhook/gmail` Pub/Sub
endpoint, which was removed (#375), and the channel type has since been
deleted. A leftover `[channels.gmail_push]` section is ignored and reported
with the `inbound_webhook_channel_removed` config warning. Use the retained
email configuration above.

## See also

- [Discord](./discord.md); the other retained tool configuration
- [Channels overview](./overview.md)
