# zeroclaw-bridge-telegram

A Telegram bridge for the ZeroClaw gateway. It runs as its own process and
attaches to the gateway's `/ws/chat` socket, the same way `zeroclaw chat`
does. The core has no Telegram code on this path. The bridge only relays
between one Telegram private chat and one gateway session.

```sh
cargo build -p zeroclaw-bridge-telegram
zeroclaw gateway bridge add telegram --session main   # prints a zcb_... token once
TELEGRAM_BOT_TOKEN=123:abc TELEGRAM_OWNER_ID=4242 ZEROCLAW_GATEWAY_TOKEN=zcb_... \
  zeroclaw-bridge-telegram --agent assistant
```

The gateway token is a bridge token (`[gateway.bridges.telegram]`), scoped
to the chat sessions it may open. Pass `--session` for the session the
bridge uses (`main` by default). A paired token still works for the chat
relay, but the gateway refuses it on the control socket, so proactive
messages stay off.

## Flags

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--telegram-token` | `TELEGRAM_BOT_TOKEN` | required | Bot token from @BotFather |
| `--owner-id` | `TELEGRAM_OWNER_ID` | required | Numeric Telegram user id of the owner |
| `--agent` | | required | Agent alias (`[agents.<alias>]` on the gateway) |
| `--session` | | `main` | Gateway session the owner's chat maps to |
| `--gateway` | | `ws://127.0.0.1:42617` | Gateway WebSocket base URL |
| `--gateway-token` | `ZEROCLAW_GATEWAY_TOKEN` | none | Bridge token from `zeroclaw gateway bridge add` |
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

## Proactive messages

With a bridge token the bridge also keeps the gateway's `/ws/bridge` control
socket open. Cron jobs whose delivery `channel` is the bridge's name
(`telegram` above), a heartbeat targeting it, and the `notify` tool queue
messages in the gateway's outbox, and the gateway sends them here as
`deliver` frames.

- `to` is the Telegram chat id and `thread_id` becomes `message_thread_id`.
  Only the owner's chat is served: a message for any other chat is dropped
  with a warning.
- The bridge acknowledges a message (`delivered`) only after Telegram
  accepted it. The gateway keeps unacknowledged messages and sends them again
  on the next connection, oldest first; ids already delivered are
  acknowledged without sending twice.
- If Telegram fails with a retryable error (a timeout, a rate limit, a
  server error), the bridge drops the control socket and reconnects with
  backoff, and the message comes again. A permanent refusal, such as a
  chat Telegram does not know, is dropped and logged so it cannot block the
  queue.
- If the gateway refuses the control socket (for example because the token is
  a paired token, not a bridge token), the bridge logs an error and keeps
  relaying chat without proactive messages.

Not yet: groups, attachments and `ask_user` questions. A turn's frames that
arrive while the chat socket is down are not replayed.
