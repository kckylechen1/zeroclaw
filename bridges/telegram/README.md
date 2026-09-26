# zeroclaw-bridge-telegram

A Telegram bridge for the ZeroClaw gateway. It runs as its own process and
attaches to the gateway's `/ws/chat` socket, the same way `zeroclaw chat`
does. The core has no Telegram code on this path. The bridge only relays
between one Telegram private chat and one gateway session.

```sh
cargo build -p zeroclaw-bridge-telegram
TELEGRAM_BOT_TOKEN=123:abc TELEGRAM_OWNER_ID=4242 ZEROCLAW_GATEWAY_TOKEN=zc_... \
  zeroclaw-bridge-telegram --agent assistant
```

## Flags

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--telegram-token` | `TELEGRAM_BOT_TOKEN` | required | Bot token from @BotFather |
| `--owner-id` | `TELEGRAM_OWNER_ID` | required | Numeric Telegram user id of the owner |
| `--agent` | | required | Agent alias (`[agents.<alias>]` on the gateway) |
| `--session` | | `main` | Gateway session the owner's chat maps to |
| `--gateway` | | `ws://127.0.0.1:42617` | Gateway WebSocket base URL |
| `--gateway-token` | `ZEROCLAW_GATEWAY_TOKEN` | none | Paired bearer token, when the gateway requires pairing |
| `--telegram-api` | | `https://api.telegram.org` | Bot API base URL (tests point it at a fake) |

Pass the tokens through the environment. Values given as flags show up in
the process list. Logs go to stderr, and `RUST_LOG=debug` adds ignored
updates.

## Owner only

The bridge serves exactly one Telegram user, the `--owner-id`, and only in
their private chat with the bot. Messages from anyone else and messages in
groups are ignored and logged at debug level. The same goes for button
presses from anyone but the owner. Approval buttons therefore only ever act
for the owner.

## Session sharing

The owner's chat maps to the configured agent and session, `main` by
default. Every client attached to that session shares one conversation:

- A turn started on Telegram streams to `zeroclaw chat -a <agent> -s main` on a
  laptop, and a turn started on the laptop is mirrored into the Telegram
  chat.
- A message sent while a turn runs steers that turn. The bridge answers
  "(added to the current turn)".
- `/cancel` stops the running turn.

## Behavior

- Replies stream into one Telegram message: it is sent on the first chunk and
  edited at most once a second, then edited a last time with the full
  response. Text over Telegram's 4096-character limit continues in more
  messages. Text is sent without a parse mode, so the model's output shows
  as written.
- The chat shows "typing" while a turn runs.
- An `approval_request` becomes a message with Approve / Always / Deny
  buttons. The button data is `ap:<request_id>:<y|a|n>`. A request id too long
  for Telegram's 64-byte limit gets a short local key instead. Pressing a
  button answers the gateway and marks the message with the decision.
- If the gateway goes away, the bridge reconnects with backoff (1s doubling
  to 30s) to the same session. A message without an `ack` yet is sent again
  under the same request id, and the gateway's request dedupe runs it at most
  once. A message typed while the gateway is down is queued and sent after
  the reconnect. A refusal the bridge cannot fix by waiting (a bad token or an
  unknown agent) stops the bridge.

Not yet: groups, attachments, `ask_user` questions, and proactive messages
(cron, `notify`). A turn's frames that arrive while the socket is down are
not replayed.
