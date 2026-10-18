# Gateway HTTP API

The gateway is the one external door to the body (ADR-013, ADR-017). This page
lists the routes it registers and describes the config and review surfaces in
detail. The router in `crates/zeroclaw-gateway/src/lib.rs` is the authority for
the live surface.

## Routes

| Area | Routes |
|---|---|
| Liveness | `GET /health`, `GET /api/health`, `GET /api/status`, `GET /metrics` |
| Local ops (loopback only) | `POST /admin/shutdown`, `POST /admin/reload`, `GET /admin/paircode`, `POST /admin/paircode/new` |
| Client pairing | `POST /pair`, `GET /pair/code`, `POST /api/pairing/initiate`, `POST /api/pair`, `GET /api/devices`, `POST /api/devices/me/capabilities`, `DELETE /api/devices/{id}`, `POST /api/devices/{id}/token/rotate` |
| Chat | `GET /ws/chat` (WebSocket), `POST /webhook` |
| Channel bridges | `GET /ws/bridge` (WebSocket, bridge token only) |
| Events | `GET /api/events` (SSE), `GET /api/events/history` |
| Sessions | `GET /api/sessions`, `GET /api/sessions/running`, `GET/POST /api/sessions/{id}/messages`, `PUT/DELETE /api/sessions/{id}`, `GET /api/sessions/{id}/state`, `POST /api/sessions/{id}/abort` |
| Scheduling | `GET/POST /api/cron`, `GET/PATCH /api/cron/settings`, `PATCH/DELETE /api/cron/{id}`, `GET /api/cron/{id}/runs`, `POST /api/cron/{id}/run` |
| Memory and review | `GET/POST /api/memory`, `DELETE /api/memory/{key}`, `/api/user-model/*`, `/api/soul*`, `/api/review/inbox` |
| Personality and skills | `/api/personality*`, `/api/skills/*`, `GET /api/agents/{alias}/skills` |
| Diagnostics | `GET /api/logs`, `GET /api/cost`, `GET /api/tools` |
| Channels | `GET /api/channels`, `POST /api/channels/{channel}/relink` (until #378) |
| Config (dashboard editor, until #379) | `/api/config*`, `POST /api/channels/bind` |
| Edge devices (`nodes` feature) | `GET /ws/nodes` (WebSocket), `POST /api/node-identities/pairing`, `POST /api/node-identities`, `DELETE /api/node-identities/{id}` |
| Dashboard assets | `GET /_app/{*path}`, SPA fallback |

## Authentication

The configuration value reads and mutations described on this page are gated
by the existing pairing and bearer authentication. Config `OPTIONS` shape
discovery is public. A first-run pairing code is printed when the daemon
starts; subsequent authenticated calls send the derived bearer token in the
`Authorization` header.

Local-bound by default. Over-the-network access requires TLS termination at
the gateway or in front of it; the per-property and PATCH endpoints are not
safe to expose unauthenticated regardless of TLS posture.

See [durable bridge intake](durable-bridge-intake.md) for the opt-in source receipt
and recovery extension used by the Telegram bridge.

## WebSocket chat sessions

`GET /ws/chat?agent=<alias>&session_id=<id>` opens a chat socket. All sockets
that open the same session with the same agent share one conversation: one
agent, one history, one running turn.

- A turn's frames (`chunk`, `thinking`, `tool_call`, `tool_result`, `plan`,
  `approval_request`, `done`, `aborted`, `error`) go to every socket on the
  session, not only the one that sent the message.
- A `message` sent while a turn runs steers that turn. If the turn has
  stopped reading steering by the time it arrives, it runs as the next turn.
- Any socket may answer an `approval_request`; the first answer counts.
- Closing a socket does not stop the turn. It keeps running and its result
  is saved to the session. When no socket is left, a tool approval it asks
  for is denied at once, because nobody can answer it.
- To stop a turn, send `{"type":"cancel"}` or call
  `POST /api/sessions/{id}/abort`. Every socket then gets `aborted`. A
  `cancel` with no running turn is answered with the `NO_ACTIVE_TURN` error.
- The history and working directory are set up by the first socket to open
  the session. Later sockets join as they are; their `connect` frame's `cwd`
  is ignored while the conversation is live.
- A socket that falls far behind skips frames; the `done` frame still
  carries the full response.

### Presentation surface (version 1)

Clients may register `web`, `telegram` or `cli` through the `surface` query
parameter or their first `connect` frame, for example
`{"type":"connect","surface":"telegram"}`. Query and frame values must agree.
Unknown names, non-string frame values and explicit null are refused; an invalid
query receives HTTP 400 and an invalid frame receives `INVALID_SURFACE`.
`session_start` and `connected` advertise `surface_version: 1`. The first echoes
the query surface; `connected` echoes the resolved register after the connect
frame. Null denotes omission. Legacy clients may omit the field.

The register belongs to the initiating connection and is resolved for each turn,
not captured by the first socket's shared Agent. It adds one repository-owned
`## Surface` line about length and Markdown. Identity, Soul and Tone stay
identical; surface grants no authority and does not change authentication,
allowlists, session identity or the `wss` runtime channel. All subscribers receive
the same output. Steering and a late steering continuation retain the original
turn's register. Durable source inputs retain the register accepted with their
body, including after reconnect or restart.

The Web client sends `surface=web`; the CLI and Telegram bridge use the connect
frame and require a matching version 1 acknowledgement. Upgrade Gateway before
those clients; an older Gateway cannot satisfy their surface handshake. An older
Gateway ignores the Web client's optional query and uses legacy presentation. A legacy
client still uses the previous prompt without a Surface section. Reverting the
clients removes their registration; reverting Gateway removes the presentation
section. Pending source bodies can retain this optional field without a database
migration, but an older Gateway will ignore its formatting meaning. Retries of
surface-bearing inputs can conflict with an older Gateway's immutable payload
comparison. Resume from durable receipts; never rewrite accepted bodies or
force replay to make a downgrade succeed.

Upstream commit `05f9fe95d4a9be5ab76124ed3a40da3fb9697f3a` was checked: its
WebSocket handshake has no equivalent surface register. This bounded extension
implements the surface slice of #377/#380; surface-based recall and worker
projection rules remain separate work.

### Turn-bound bridge stop (control version 1)

The Telegram bridge requests `control_version: 1` on its existing
`source_resume` frame. `source_ready.control` must confirm version 1 and include
an explicit `turn_id` (UUID or null), a monotonic `revision`, and Gateway-localized
`idle_message`, `unknown_message`, and `not_sent_message`. Missing or unsupported control semantics
refuse the bridge handshake; there is no bare-cancel fallback. Legacy clients
can omit the extension and retain their existing frames.

The shared Conversation creates the turn UUID when it claims the turn slot and
uses the same UUID for persisted running-state and trace identity. After control
negotiation, `turn_state` frames carry `control_version`, `revision`, and
`turn_id`. Null means idle. Clients replace their revision baseline on reconnect
from `source_ready.control` and ignore older/equal buffered revisions. These
frames describe ephemeral ownership; they do not create another durable ledger.

A stop freezes the observed UUID before waiting for its durable source-control
receipt. The request is:

```json
{"type":"cancel","control_version":1,"id":"tg:7:9:42","turn_id":"a0000000-0000-4000-8000-000000000001"}
```

Gateway rechecks current device or exact-session bridge authority and the enabled
agent, then compares the target with the active turn under the admission lock.
It replies only to the sender with `cancel_ack`, the same version, ID and target,
a `status`, and a localized `message`. Status is `requested`, `stale`, `no_active`,
`unauthorized`, or `invalid`. `requested` means the cancellation token was
signalled; it does not mean effects have stopped. The existing terminal
`aborted` event remains the confirmation of turn cancellation. A stale target,
malformed version/target, or revoked authority cannot cancel another turn.

Only a matching known acknowledgement resolves the bridge's pending outcome.
The bridge bounds those waiters to 32 for 15 seconds, reports an uncertain
outcome after timeout, and never resends an attempted stop after reconnect.
Durable source acceptance and the stop outcome are distinct: a crash after
recording a control can leave its outcome unknown. Replayed source controls do
not execute again. Disconnect only detaches the chat; stopping a chat turn never
issues a delegated-job cancellation. That remains an explicit, run-bound Tachi
operation under #381.

Upstream `zeroclaw-labs/zeroclaw` was inspected at
`329e66f76394b00af9d00b767e00ad298133961e`: its WebSocket implementation preserves
detach-versus-abort but has no equivalent shared-Conversation bridge stop target.
This leaf extends the fork's existing Conversation owner; it does not import
upstream Agent orchestration. Real-bot/provider acceptance and the remainder of
#377 stay separate.

### Who can attach to a session

A session id names a conversation; it is not a secret or a credential. With
`require_pairing = true` (the default), every socket must present a paired
token, and any paired token may attach to any session: the gateway serves one
owner, and every paired device is that owner's. With pairing off, every client
is anonymous, so anyone who can reach the gateway and names a session id sees
its turns, can cancel them, and can answer its approvals. For that reason the
gateway refuses to bind a non-loopback address with pairing off unless
`allow_public_bind = true` explicitly accepts it.

### Request IDs and acceptance

A `message` frame may carry an `id`: a client-chosen string of 1 to 128
characters without control characters, unique within the session. An invalid
`id` is refused with `INVALID_REQUEST_ID` and nothing runs.

With an `id`, the gateway records the request before running it and answers
the sending socket first:

```json
{"type":"ack","id":"r-42","status":"accepted","turn":"started","durable":true}
```

- `turn` is `started` for a new turn, or `steered` when the message joined
  the running one.
- `durable` is `true` when the receipt is in the session store (the default
  SQLite backend) and survives a restart. It is `false` with the JSONL
  backend or with persistence off; bounded in-memory receipts survive idle
  conversation release for 16 minutes from first acceptance, but not restart.
- The ACK means accepted for processing, not finished. The turn's `done`,
  `aborted` or `error` frame carries the same `id`.
- Sending an `id` again, for example after a lost ACK, runs nothing and
  answers `{"type":"ack","id":"r-42","status":"duplicate","state":"done"}`.
  `state` is the last recorded one: `accepted`, `steered`, `done`,
  `aborted`, `error` or `rejected`. `accepted` after a restart means the
  outcome is unknown; the request is not replayed.
- If the receipt cannot be recorded, the message is refused with
  `REQUEST_NOT_RECORDED` and nothing runs.
- Each session admits at most 256 receipts. SQLite also caps the whole table
  at 16,384 receipts. Capacity reclamation only deletes receipts accepted more
  than 16 minutes ago; if all receipts are protected, new input is refused.
  Duplicate IDs still resolve while full. Beyond that window capacity reclamation
  may remove a receipt, so deduplication is bounded rather than permanent.
  The stale-session sweep
  (`session_ttl_hours`) deletes a swept session's receipts, and receipts
  older than the TTL whose session no longer exists.

Messages without an `id` behave as before: no ACK and no deduplication.

### Questions and answers

`ask_user` on `wss` emits a session-bound question to every attached client:

```json
{"type":"question","request_id":"<opaque UUID>","prompt":"Which?","choices":["alpha","beta"],"timeout_secs":120}
{"type":"answer","request_id":"<same UUID>","text":"2"}
{"type":"answer_ack","request_id":"<same UUID>","status":"accepted"}
```

An empty `choices` array accepts free text. Otherwise use the exact choice
text or its one-based number (valid numbers take precedence over matching
choice text). An answer never approves a tool. The Gateway
checks current paired-device or scoped bridge authority before consuming the
answer; anonymous sockets, including with pairing disabled, cannot answer.
A bridge removed or moved outside the session scope cannot answer on an old
socket. All paired devices belong to the single owner.

The shared Conversation owns pending questions in memory, separate from
message receipts and approval decisions. Only the first valid answer counts.
`answer_ack` is the authority: `accepted` is broadcast to the session;
`invalid`, `stale` and `unauthorized` go only to the sending socket. Invalid
text leaves the question open. These frames do not end the turn. An accepted
answer means handed to the live waiter, not that subsequent effects finished,
and is not a durable receipt across a process restart.

The limit is 16 pending questions, 32 choices, 3000 UTF-8 bytes across the
prompt and choices, 4096 bytes per answer, and a timeout of 1–300 seconds.
Cancelling/finishing the turn, dropping the waiting call, or losing the last
subscriber removes pending questions. The turn itself survives disconnection.
`question_closed` retires a question's UI controls. A new subscriber receives
still-pending questions with their original IDs and remaining timeout; prompts
and choices use the current outbound redaction policy at send/replay time.

Clients must not automatically replay answers after a lost ACK: an interrupted
answer has an unknown outcome. Questions do not survive a Gateway restart.
Older clients tolerate the additive frames but cannot answer; the request then
expires. `zeroclaw chat` supports `/answer <request_id> <text or choice number>`.

## Channel bridges

A bridge is a channel that runs as its own process, for example
`zeroclaw-bridge-telegram` (ADR-013). It relays conversations through
`/ws/chat` and receives proactive messages (cron output, heartbeat alerts,
the `notify` tool) on the `/ws/bridge` control socket.

### Bridge tokens

Each bridge has an entry under `[gateway.bridges.<name>]`:

```toml
[gateway.bridges.telegram]
token_hash = "<sha256 hex of the token>"
sessions = ["main"]        # exact chat sessions the token may open
session_prefix = "tg:"     # and any session whose id starts with this
```

`zeroclaw gateway bridge add telegram --session main` mints a random token,
prints it once and stores only its SHA-256 hash (the same hashing as paired
tokens). `--rotate` replaces the token of an existing bridge, `bridge remove`
revokes it and `bridge list` shows the scopes. Restart or reload the gateway
to apply a change. Pairing never issues bridge tokens, and a paired token is
not a bridge token.

- On `/ws/chat` a bridge token authenticates like a paired token, but only
  for sessions in its scope: `session_id` must be listed in `sessions` or
  start with `session_prefix`. Any other session, or no `session_id`, gets
  403. With neither field set the token opens no chat session. The agent is
  not restricted.
- `/ws/bridge` accepts only bridge tokens, whether or not `require_pairing`
  is set. The bridge's identity comes from the token, never from the query.

### Control socket

The gateway sends `{"type":"bridge_start","bridge":"<name>"}` when the socket
opens, then one frame per queued message:

```json
{"type":"deliver","id":"<uuid>","to":"4242","thread_id":"12","content":"..."}
```

`thread_id` is present only when the sender set one. The bridge answers
`{"type":"delivered","id":"<uuid>"}` after the platform accepted the
message, and the gateway retains a confirmed receipt. Control subprotocol `zeroclaw.bridge.v2` is required and checked by both peers;
old/new binaries fail closed. Upgrade the gateway and bridge together. The chat
subprotocol is unchanged. Each bridge has at most one
control socket: a new connection closes the old one (close code 4000).

### Outbox

Messages and attention facts share the existing SQLite outbox,
`<data_dir>/sessions/bridge_outbox.db`. Execution truth remains with the source.

- `accepted` means durably queued; `sent` means only WebSocket handoff;
  `confirmed` means the bridge received a successful platform response, not
  that the owner read it. `unknown` means a send may have happened without a
  usable receipt. A failed or partial Telegram send is never acknowledged as
  successful.
- Before network I/O the gateway durably claims the candidate. A crash, lost
  response or disconnect leaves an uncertain attempt. **Unknown attempts are
  never automatically resent.** Inspect and dismiss them after checking the
  platform; any replacement notice is a deliberate new source event. This
  intentionally tightens the former at-least-once reconnect behavior, including
  legacy outbox rows. A crash between the durable claim and the actual send can
  therefore leave an unsent notification requiring owner reconciliation.
- At most one candidate is in flight per control socket. The next candidate is
  not claimed until the current receipt arrives or the owner resolves it. A
  60-second receipt timeout closes the socket, leaving only the current attempt
  unknown and later candidates accepted for reconnect delivery.
- Accepted candidates are scanned in insertion order on every poll. Quiet,
  snoozed and muted candidates do not block later eligible candidates, and are
  revisited when policy permits. There is no persistent sequence cursor that
  skips deferred rows.
- Capacity is 1000 active candidates per bridge. At capacity new enqueue
  requests fail visibly; active and unknown rows are never evicted. Accepted
  candidates expire 24 hours after enqueue, including deferred candidates;
  expiry leaves an inspectable `expired` receipt. Unknown attempts do not expire.
- Confirmed, dismissed and expired receipts retain source deduplication until
  30 days after resolution or until displaced from the latest 10,000 terminal
  receipts per bridge, whichever is earlier. Cleanup runs on writes/drains.
  Re-observing a source event after this window may create a new notice.

### Owner attention policy

With no `[gateway.attention]` section, notifications retain immediate timing.
Once present, the section requires an explicit valid IANA timezone and local
`HH:MM` quiet boundaries. Invalid policy holds delivery and logs
`attention_invalid_policy`; it never falls back to the host timezone.

```toml
[gateway.attention]
timezone = "America/New_York"
quiet_start = "22:00"
quiet_end = "08:00"
important_sources = []
```

The start is inclusive and the end exclusive; equal boundaries mean all day.
Each UTC instant is evaluated against local wall time, so both occurrences of
an autumn repeated hour are quiet and spring skipped times need no guessed
boundary. Policy is resolved from live gateway config immediately before the
outbox claim. Config API changes, including removing the section, always need
a paired operator bearer even when generic pairing is disabled, with the shared
operator authentication and rate limits. Direct local
config edits use the existing reload path.

An owner may explicitly allow one source to bypass quiet hours with an
`important_sources` entry containing exact `bridge`, `recipient`,
`source_kind` and `source_id` values. For cron, use `source_kind = "cron"` and
the canonical job ID. No urgency label from the model can grant bypass. Mute,
snooze and expiry still apply to an allowed source.

Cron has a stable job source identity. Generic `notice` producers assign a new
source ID to each candidate: muting one such notice affects only that candidate,
not future heartbeat or notify emissions. Stable recurring heartbeat identities,
delegated results and weekly integration remain #63 follow-up work.

The following endpoints require the existing operator bearer; a bridge token
or anonymous caller is refused even with `require_pairing = false`:

- `GET /api/attention/{bridge}?after_seq=0` lists at most 100 receipts, without
  message bodies, plus at most 1000 persistent source `mutes`. Continue the
  receipts page with the last returned `seq`.
- `GET /api/attention/{bridge}/{id}` inspects a candidate's source identity,
  state, expiry, snooze and mute status.
- `POST /api/attention/{bridge}/{id}` applies an owner action. The JSON body
  must match the exact `recipient`, `source_kind` and `source_id` shown by GET,
  and contains `action` (`snooze`, `dismiss`, `mute`, or `unmute`). `snooze`
  additionally requires a future Unix-seconds `until`; it does not extend expiry.

Snooze applies only before an attempt. Dismiss resolves the notification and
never cancels the underlying job. Mute persists for that exact recipient/source
in the same outbox database until explicitly unmuted; it does not update an
inferred User Model preference. An action racing with a claim cannot retract
an already-started platform send. The mute list retains the creating candidate
ID so unmute still works after receipt cleanup. Creating more than 1000 mute
facts per bridge is refused; existing policies are not evicted.

The migration adds columns and a mute table to the existing database. Before
rolling back to an older binary, disconnect bridge delivery and preserve the
outbox for reconciliation: an older binary ignores delivery states and would
replay retained rows. Do not point an older gateway at a live migrated outbox.

This is the policy leaf of #63. Delegated-result and weekly-review producers,
and real Telegram quiet-hour acceptance, remain follow-up evidence; the policy
and simulated transport tests do not establish those integrations.

Three producers write to the outbox:

- **Cron.** A job whose delivery `channel` names a bridge is queued for it,
  with `to` and `thread_id` as given:

  ```toml
  [cron.digest.delivery]
  mode = "announce"
  channel = "telegram"   # a [gateway.bridges.<name>] entry
  to = "4242"            # Telegram chat id
  ```

  A bridge name takes precedence over an in-core channel of the same name.
  Delivery reports `accepted` once queued. Candidates bind the canonical job ID
  and run start timestamp; repeated observations of that run reuse its receipt.
- **Heartbeat.** `heartbeat.target` may name a bridge too.
- **`notify {bridge, to, text}`.** The model's tool for proactive messages,
  offered only when at least one bridge is configured. It is an ordinary
  side-effecting tool: it needs approval unless listed in `auto_approve`,
  and read-only agents cannot use it.

Cron results are no longer broadcast to every `/ws/chat` socket; they reach
people only through a bridge (or an in-core channel). The SSE stream at
`/api/events` still carries them.

## Discovering the surface

Two endpoints answer the question "what can I do here?":

- `OPTIONS /api/config` returns the JSON Schema for the whole-config type.
  Static per build; clients should cache against the `ETag` header. Its current
  `Allow` header still lists legacy `PUT`, which the router does not register.
- `OPTIONS /api/config/prop?path=<dotted>` returns the schema fragment for a
  specific path with `Allow: GET, PUT, DELETE, OPTIONS`. Returns 404 if the
  path doesn't exist in the schema.

`OPTIONS` returns capabilities. `GET /api/config/prop` and `GET /api/config/list` return the user's current values. Forms in the dashboard issue `OPTIONS` once at load time to learn types and constraints, then `GET` to populate fields, then `PUT`/`PATCH` to write. A compatibility `GET /api/config` also returns a whole-config snapshot with secrets masked so older bundled dashboard pages do not fail against newer gateways. New clients should prefer the per-property surface because it carries field metadata and explicit secret handling.

CORS preflight requests (those carrying `Access-Control-Request-Method`) get
the standard preflight response and short-circuit before the schema body is
returned.

## Per-property CRUD

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/api/config` | Compatibility whole-config snapshot with secrets masked; new clients should prefer the per-property surface. |
| `PATCH` | `/api/config` | Apply a JSON Patch (RFC 6902) document atomically. |
| `OPTIONS` | `/api/config` | Whole-config JSON Schema (capabilities, not values). |
| `GET` | `/api/config/prop?path=...` | Read one field. Secrets return `{path, populated}` only. |
| `PUT` | `/api/config/prop` | Write one field. Body: `{path, value, comment?}`. Secrets respond with `{path, populated: true}` only. |
| `DELETE` | `/api/config/prop?path=...` | Reset one field to its default. Secrets respond with `{path, populated: false}`. |
| `OPTIONS` | `/api/config/prop?path=...` | Per-field schema fragment. |
| `GET` | `/api/config/list?prefix=...` | Enumerate every reachable path with type and category. Secret entries carry `{path, populated, is_secret: true}` and no value. |
| `POST` | `/api/config/init?section=...` | Instantiate `None` nested sections with defaults. Dynamic-map aliases are not created here; use `POST /api/config/map-key`. |
| `POST` | `/api/config/migrate` | Apply on-disk schema migration in place. Mirrors `zeroclaw config migrate`. |

## Atomic batch writes: JSON Patch

`PATCH /api/config` accepts a JSON Patch document (RFC 6902). The supported
config operations are `add`, `replace`, `remove`, and `test`. ZeroClaw also
accepts a `comment` extension for config annotations. Config operations run
against an in-memory copy; once every operation has applied,
`Config::validate()` runs once on the result. If validation passes, the new
state is persisted and swapped in. If any operation or final validation fails,
on-disk and in-memory state are unchanged. Comment annotations are applied
after the save on a non-fatal, best-effort basis.

`move` and `copy` return `400 op_not_supported` because safe reference-graph
rewriting is not part of this surface. `test` against a `#[secret]` path is
rejected with `secret_test_forbidden`: a differential outcome would be the
only signal a client could read, and that would leak the value.

Path syntax: JSON Pointer (`/agents/researcher/model_provider`) or the
dotted form (`agents.researcher.model_provider`). Both are accepted; the
server normalises.

The CLI counterpart is `zeroclaw config patch <file-or-stdin>`, which applies
the same op set against the local Config and returns the same structured
response shape (`--json` for scripts).

## Secrets: write-only over HTTP

Per-property reads never expose secret fields (those marked `#[secret]` or
`#[derived_from_secret]` in the schema). Their responses carry
`{populated: bool}` only, with no value, length, masked stand-in, or hash. The
compatibility `GET /api/config` instead serializes the whole config after
applying `MaskSecrets`, so secret fields can appear there only as masked
placeholders. Neither config read surface returns the underlying secret value.

`PUT` and `PATCH` write the new secret value and respond with
`{populated: true}`; `DELETE` clears it and responds with
`{populated: false}`. There is no HTTP path to retrieve a secret by any means.

## Unified owner review inbox

`GET /api/review/inbox` requires the operator bearer, even when pairing is
optional. Node credentials, bridge identities and model tools grant no review
authority. The view reads pending Soul proposals, pending User Model candidates,
and the latest 20 reflection receipts per configured agent from their existing
stores. No inbox database or copied queue exists. Store errors return 503 rather
than a successful partial inbox.

The response is `{ "items": [...], "total": N, "next_offset": N | null }`.
Items carry a stable namespaced `id`, `kind` (`soul_proposal`,
`user_model_candidate`, or `reflection_receipt`), `created_at_unix`, and the
original `item`. Candidates appear oldest first, then receipts newest first.
Use `limit=1..200` (default 100) and `offset` for bounded pages; these are live
views, so concurrent review can change page positions. An optional `agent`
filters Soul and reflection history. The owner's shared User Model candidates
remain visible for every agent filter.

Each candidate has `review_url` pointing to its existing operator endpoint:

- Soul: submit `agent` and `resolution: "accepted"` or `"dismissed"`.
  `final_text` rewords an accepted Story entry or principle.
- User Model: submit `action: "accept"`, `"narrow"` or `"reject"`.
  `narrow` requires `narrowed_scope`; `final_text` optionally supplies the
  owner's wording for an accepted/narrowed statement. Wording is a non-empty
  single line of at most 240 UTF-8 bytes. Dismissal applies nothing. The
  original candidate/evidence remains intact and the approved revision owns
  the new wording. Repeated committed decisions return 409, except the one-time reject-to-narrow
  follow-up documented below.
- Reflection receipts have no review action: they report what ran and how many
  proposals/candidates were created. They grant no approval authority.

`note_owner_correction` puts a correction from the current owner message into
this same User Model queue. It uses the existing `user_model.db` owner and the
shared three-pending-candidate cap; duplicate pending corrections are not added.
The model supplies only kind, statement and semantic key. Runtime-bound evidence
records the owning agent, session, original owner text and ingress source. WS
text is captured before attachment expansion; file bytes never become owner
evidence, and an attachment-only or unbound input cannot support a correction. Channel
senders must match the current companion owner policy; paired operator WS input
is checked against current pairing membership. Bridge, anonymous, steering-only,
worker and unsupported CLI/ACP turns cannot mint correction evidence. A live
config handle is required, so snapshot-only tool registries fail closed.

Corrections start with `session:<originating-session>` applicability and change
no active User Model or Soul state. The runtime derives a separate session-local
correction key from the agent, session and submitted semantic key, so approving a
correction cannot supersede another session or a global preference. Repeated
approved corrections for that same agent/session/key replace that correction
head. Prompt and reflection readers resolve the owning agent from the immutable
source candidate evidence; malformed correction provenance is excluded. The
correction format is `oc.` plus 60 lowercase hex digits on a session-scoped
source candidate. Historical owner statements and global source candidates keep
their behavior even when their keys match that format. Same-second correction
approvals use revision insertion order, so the later approval wins. Any accepted steering
submission to an active WebSocket turn invalidates its initial correction
authority for the rest of that turn. Acceptance or rewording preserves that scope;
`narrow` cannot move the correction to another session or expand it to global.
Global reflection candidates retain their existing review behavior. The usual
operator review endpoint is the only promotion path. Disabling this tool removes
new correction intake without deleting pending candidates or review history.

Reflection uses immutable original ingress text and its source on each SQLite message row, rather
than the session's latest sender. Active and passive channel messages store
their actual sender; the current owner identity list is checked when reflecting.
Each ordinary paired operator WS turn marks its initial user input. Anonymous,
bridge and steering inputs without bound owner origin are excluded. A late
steering follow-up does not inherit the original socket's owner identity.
Hooks, link previews and media annotations remain in chat history but cannot
replace this original evidence. Historical rows and imported JSONL without this
ingress record stay readable as chat
history but are excluded from reflection; unknown origin is never guessed.

The PWA consumes this API under #379; phone review remains a separate slice.

## User Model review history

`GET /api/user-model/candidates/{id}` requires the operator identity used by
the other User Model routes. It returns the candidate and its evidence,
`review_receipts` in insertion order, and a `review_state` derived from the
last inserted receipt: `pending`, `accepted`, `rejected`, `narrowed`, or
`superseded`. A candidate with no receipt is `pending` and has an empty receipt
array. An unknown candidate ID returns 404. The read uses the existing local
User Model store, so committed reviews remain inspectable after a reconnect.
Receipt insertion order uses SQLite's implicit rowid in this append-only table.
That rowid is not a durable receipt identity;
external deletion, rowid updates, or a `VACUUM` rebuild can invalidate that
ordering for equal or backdated timestamps. The store does not perform those
operations on receipts.

`POST /api/user-model/candidates/{id}/review` accepts one committed decision
per candidate. A rejected candidate may be narrowed once; that explicit
follow-up appends an owner-ratified narrowed revision. Any other repeat returns
HTTP 409 with `code: "candidate_already_reviewed"` and writes no receipt or
revision. A failed narrow leaves the rejection available for a later valid
narrow. Unknown IDs still return 404. The decision and receipt write share one
SQLite write transaction, including across independent store connections.
The heads response advertises `supports_expected_head: true`. Review clients
bind approval to the displayed head with `expected_head: {"id":"<revision-id>"}`,
or `expected_head: {"id":null}` when no active head was displayed. Before
accept, narrow, or supersede writes anything, the transaction checks the exact
current eligible head for the candidate's semantic key using the same expiry
and tie ordering as the heads endpoint. A mismatch returns HTTP 409 with
`code: "head_conflict"` and writes nothing; refresh before retrying. Reject
has no replacement and does not compare heads. Omitting `expected_head` retains
legacy behavior; malformed objects are rejected. The phone review UI requires
the capability before offering approval against a gateway, while read and
dismiss remain available on older gateways.

Active User Model revisions reach the agent's system prompt on every surface:
`/ws/chat` (and the CLI and bridges that use it) as well as channel turns. Each
turn projects the revisions whose scope applies to it (`global`, the agent, the
channel, or the session), as an `## Owner profile (authoritative)` section of at
most 1,200 bytes at the end of the system prompt. A reviewed or owner-written
change applies from the next turn.

## Governed Soul

The agent's Soul is owner-governed
([ADR-015](../architecture/decisions/ADR-015-one-governed-soul.md),
[ADR-016](../architecture/decisions/ADR-016-growth-proposed-by-agent-approved-by-owner.md)).
Every route below requires the operator identity. The `agent` must be a
configured agent alias; an unknown alias returns 404 with
`code: "unknown_agent"`.

The four layers are Identity, Principles, Tone (how the agent sounds: the five
persona dials) and Story (what has happened between the agent and its owner;
[ADR-018](../architecture/decisions/ADR-018-soul-character-model.md)). They are
named `identity`, `principles`, `tone` and `story` in every route, JSON field
and tool argument. `soul.db` keeps its older stored identifiers (`voice` for
Tone and `growth` for Story) so a store stays readable by an earlier build;
those never appear in the API, and "voice" in this API means audio only.

| Route | Purpose |
| --- | --- |
| `GET /api/soul?agent=<alias>` | Current `identity`, `principles`, and `story` heads (each with `revision`, `source`, and `value`), `tone` (`configured` dials, `stored` heads, `effective` dials and per-key `sources`), `legacy_persona_files` (`injected` or `suppressed`), and `last_reflection`. Seeds missing layers on first read. |
| `GET /api/soul/history?agent=<alias>&layer=identity\|principles\|tone\|story` | Every revision of one layer, oldest first. |
| `GET /api/soul/export?agent=<alias>` | Download JSON (`format_version: 3`) containing `agent`, four current layers in `profile`, all four per-layer revision arrays and the stored `anchors` rows in `history` (oldest first), and `tone` (`configured`, `effective`, `sources`). Current heads and history come from one read snapshot. |
| `PUT /api/soul/identity` | Body `{ "agent", "expected_revision", "identity": { "name", "self_description"?, "primary_language"?, "pronouns"? } }`. |
| `PUT /api/soul/principles` | Body `{ "agent", "expected_revision", "items": [ ... ] }`, at most 8 single-line items of up to 240 bytes. |
| `PUT /api/soul/tone` | Body `{ "agent", "expected_revision", "tone": { "heads": { "<key>": "<level>" } } }`. Keys are the five persona dials; a stored head wins over the configured dial for its key. The owner may set any level, and an owner write starts no dial cooldown. |
| `PUT /api/soul/story` | Body `{ "agent", "expected_revision", "story": { "entries": [ { "kind": "self" \| "bond", "text" } ] } }`, at most 12 single-line entries of up to 200 bytes. `self` is concerns, interests and working habits; `bond` is shared names, references, running jokes and ways of working. |
| `POST /api/soul/rollback` | Body `{ "agent", "layer", "to_revision", "expected_revision" }`. Appends a copy of an earlier revision. |
| `GET /api/soul/drift?agent=<alias>` | `{ "agent", "anchor", "drift" }`: the current anchor and how far the Soul sits from it (see below). Read-only; it never stores a default anchor. |
| `POST /api/soul/anchor` | Body `{ "agent", "expected_tone_revision", "expected_story_revision" }`. "Keep": appends an anchor at the current Tone and Story heads and returns `{ "agent", "anchor" }`. Each expected revision is the layer's current head (`0` when it has none); a stale one returns 409 `revision_conflict` and stores nothing. |
| `POST /api/soul/anchor/rollback` | Same body. "Roll back": appends Tone and Story revisions equal to the anchor's, in one transaction, through the same path as `POST /api/soul/rollback` (`source: "owner"`, `rolled_back_from` set). Returns `{ "agent", "tone_revision", "story_revision" }`, each the new revision or `null` when that layer already equalled the anchor. An anchor with no Tone revision restores empty heads, and one with no Story revision restores no entries. The anchor stays where it was. A stale expected revision returns 409 `revision_conflict` and writes nothing. |
| `GET /api/soul/proposals?agent=<alias>[&pending=true]` | The agent's own proposals to change its tone, story, or principles, oldest first. A story proposal carries `story_kind`. |
| `POST /api/soul/proposals/{id}/resolve` | Body `{ "agent", "resolution": "accepted" \| "dismissed", "note"?, "final_text"? }`. Accepting applies the proposal and returns `applied_revision`. Each proposal resolves once; a repeat returns 409 with `code: "proposal_already_resolved"`. A Story retirement is bound to the entry it named when submitted (`retire_target`, read from `target_revision`); if the owner has since removed or reworded that entry, accepting returns 409 with `code: "proposal_stale"` and the proposal stays pending for dismissal. A Tone proposal that would lower `challenge` or change a dial inside its cooldown is refused when accepted, the same way (see below); the cooldown refusal returns 429 with `code: "tone_dial_cooldown"`, the `dial`, and `until_unix`. |

### Anchor and drift

An anchor is a pointer to one Tone revision and one Story revision that the owner
marks as "this is the agent I want"
([ADR-018](../architecture/decisions/ADR-018-soul-character-model.md) §4.3). The
`soul_anchors` table in `soul.db` stores pointers only (`tone_revision_id`,
`story_revision_id`, `set_at_unix`, `source`), append-only, and the latest row for
the agent is current. An agent with no row has a `default` anchor (`id: null`),
derived on read and never written: the earliest Tone revision and the earliest
Story revision whose source is `seed` or `owner`. A layer with none has a `null`
pointer, meaning no stored heads or no entries. Its `set_at_unix` is the later of
the two revisions' creation times (the agent's earliest revision when neither
exists). Owner-set anchors have `source: "owner"`.

`drift` is computed without a model:

| Field | Meaning |
| --- | --- |
| `anchor` | The anchor compared against. |
| `dials` | All five Tone dials as `{ "key", "anchor_level", "current_level", "steps" }`. Levels are effective: the stored heads of the anchor revision and of the current revision, each over the current configured persona, so changing configuration alone shows no drift. `steps` is signed; positive means the dial moved up. |
| `story_added`, `story_retired` | Multiset difference of `(kind, text)` Story entries between the anchor revision and the current one. |
| `approved_proposals_since_anchor` | Accepted proposals of this agent resolved at or after the anchor's `set_at_unix`. |

Every reflection receipt carries the `drift` that was true when it was written
(an object, or `null` on receipts that predate it or ran without a configured
Tone). It appears as `drift` on `last_reflection` and on the reflection receipts
of `GET /api/review/inbox`.

Exports do not seed missing layers: absent heads are `null` and their histories
are empty arrays. They contain only the requested agent's canonical layer
values and revision metadata, including rollback and approved-proposal IDs.
Proposal prose, rationale, session references, resolution notes, reflection
receipts, the User Model, chat history, and legacy workspace files are excluded.
The effective Tone uses the exported stored head plus the current configured
defaults; configuration is not a historical Soul revision. Downloads use
`Cache-Control: no-store` and a fixed `soul-export.json` attachment filename.
This is an owner archive, not an import format or a model tool.

Revisions are append-only. Seeded values have `source: "seed"`; owner writes
and rollbacks have `source: "owner"`; approved proposals have
`source: "approved_proposal"` and carry the `proposal_id`. Each write must name
the revision it replaces as `expected_revision` (`0` when the layer has none).
A stale value returns 409 with `code: "revision_conflict"` and
`current_revision`. Invalid input returns 400 with `code: "invalid"` and the
offending `field`. After the first owner-written Identity revision, the legacy
`SOUL.md` and `IDENTITY.md` workspace files stop being injected into the system
prompt.

A Soul change (an owner write, a rollback, or an accepted proposal) applies
from the next turn of a `/ws/chat` session that is already open; no reconnect is
needed. The agent checks the Soul's revision at the start of each turn and
re-renders its persona only when that revision moved, so an unchanged Soul
keeps the system prompt byte-identical. Delegated workers never receive the
Soul or the User Model.

Model file tools (`file_write`, `file_edit`, `file_download`, `browser` and
`screenshot` targets, `git_operations` worktree paths) cannot write `SOUL.md`,
`IDENTITY.md`, `USER.md`, `HEARTBEAT.md`, or `BOOTSTRAP.md` at an agent
workspace root; they share one
write-target check (`HEARTBEAT.md` is the heartbeat engine's task list and
`BOOTSTRAP.md` belongs to first-run scaffolding; neither is model-writable by
design). `USER.md` stops being injected once the User Model holds an
owner-authorized active revision for the agent (global or agent scope), the
same way `SOUL.md` and `IDENTITY.md` retire with an owner-authored Identity;
until then each start logs one WARN naming the migration path. The files the
model may still write (`AGENTS.md`, `TOOLS.md`, `MEMORY.md`) render after the
persona under a fixed `## Notes you wrote` heading and never carry authority.

Tone uses the same resolver in the prompt and `GET /api/soul`. The configured
selection is the agent's persona, otherwise its card's persona, otherwise the
built-in `medium` dials. Stored Tone keys override that selection individually;
removing a stored key restores its configured value. `tone.effective` returns
all five levels; `tone.sources` maps each key to `{ "kind": "builtin" }`,
`{ "kind": "persona", "persona": "<alias>", "card": "<alias or null>" }`, or
`{ "kind": "stored", "revision": 3 }`. The stored revision identifies the current
Tone layer snapshot, not the author of each individual dial; layer authorship
and proposal receipts remain in `tone.stored` and history.

Each of the five levels renders repository-owned behavioral guidance and a
one-line example for every dial, including `medium`. The complete `## Tone`
section stays within 1,024 bytes. Tone never changes honesty, permissions or
safety rules. The guidance has no surface input; surface formatting cannot
change its Tone bytes. Config-only persona edits still require an Agent
rebuild; stored Soul changes use the next-turn refresh described above.

The model's only path into its own Soul is a proposal, either from the
`propose_soul_change` tool mid-conversation or from the weekly reflection. At
most three proposals wait per agent, and identical pending proposals are not
stored twice. Identity is never proposable. Tone proposals can move a dial by
at most one level from its current effective value, and `challenge` may only be
proposed upward: any proposal that would set it below its current effective
level is refused with a localized `soul-tone-challenge-up-only` error. A dial
changed by an accepted proposal is closed to further proposals for 28 days
(`SOUL_TONE_DIAL_COOLDOWN_SECS`); the end time is derived from the Tone
revision with `source: "approved_proposal"` and its proposal's dial, so no
extra record exists, and owner writes and rollbacks neither respect nor start a
cooldown. The store checks the one-step bound, the `challenge` rule and the
cooldown at creation and again inside the approval transaction, so changed
config, changed stored heads, a dial that entered cooldown after a proposal was
filed and historical proposals cannot bypass them; a refused approval leaves
the proposal pending. A tool without a current config resolver refuses Tone
proposals. The owner may still set any level directly through
`PUT /api/soul/tone`, including large changes and `challenge = minimal`; the
honesty floor still renders. Under a prompt budget, the optional tail of the
persona section yields lowest precedence first, before the existing tail
truncation: Story, then Tone if the prompt is still over budget; Identity and
Principles retain their existing handling. Accepting a proposal holds the live configured Tone stable
through validation and commit in the same transaction; `final_text` lets the
owner reword a story entry or principle before it applies. If the apply fails
validation or the layer changed underneath it, the proposal stays pending.
Dismissing applies nothing.

Once every 7 days per agent, the daemon reflects: it reads only the owner's
own `user` messages since the previous reflection (at most the latest 32 KiB),
makes one model call with no tools, and stores at most three validated Soul
proposals plus three User Model candidates. Each domain has its own pending
cap of three, so a full Soul queue does not block User Model suggestions.
The reflection prompt states what each layer is for; an observation about tone
becomes a Tone proposal or nothing, never a Story entry. Its output names
layers `tone`, `story` and `principles`, and Story additions use `story_kind`.
Tone reflection compares Story bond entries with actual owner reactions.
Each Tone proposal must select exactly one supplied owner-message index; its
existing `session_ref` records the canonical session and timestamp (or a
`sha256:` digest of that canonical JSON when it exceeds the 128-byte field).
Its 480-byte rationale reserves space for a JSON-escaped owner quote before
shortening the model rationale at a UTF-8 boundary. Missing or out-of-range
references are refused. Familiarity alone is not approval to change tone.
User Model suggestions carry runtime-bound owner-message/session evidence and
remain global candidates until owner review; model-supplied evidence references
outside the input are refused. Nothing is applied. Owner messages require
the per-message ingress source described above: an operator origin or a
channel sender listed in the current `[companion_memory.owner].identities`.
The last sender of a shared session cannot authorize its other rows. Tool
results and injected memory are removed first. Link previews and other derived
annotations are never read from history. A `storage_write_failed` receipt retains
already committed candidate counts and the original period, delays retry for
six hours, and still propagates the storage error to the worker. The first check
only starts the clock, a
week with no owner messages makes no model call, and a failed call is retried
after 6 hours. `last_reflection` reports the period, the number of messages
read, the Soul proposals created, `user_model_candidates_created`, and the
outcome. Older receipts expose zero for the new count. Reflection logic lives
in `zeroclaw-memory`; the daemon only wires stores, providers and cadence.
Turn settlement no longer writes `NotEvaluated` capture placeholders to the
companion PortableKernel. Existing historical rows and outbox events are retained.

Weekly review notifications are opt-in through an explicit bridge destination:

```toml
[companion_memory.review_notification]
bridge = "telegram" # Must already exist in gateway.bridges.
recipient = "<owner-chat-id>"
# thread_id = "<optional-topic-id>"
```

Without this section no review notices are created. Creating, changing or
removing it, or changing/removing its selected bridge credential, through any
config API operation requires a paired operator, even
when generic gateway pairing is disabled; replacing a parent or the root object
does not bypass that check. The recipient is never inferred from owner identity
metadata, the last conversation or model output.

The gateway checks canonical reflection receipts every minute, independently of
the reflection model call. For each enabled agent it reads the latest 200 receipt
rows and selects nonzero-created receipts less than 24 hours old. Partial-failure
receipts with actual created items are included; zero-created, future and stale
receipts are skipped. A scan that reaches 200 rows emits a warning about older
unreconciled rows. The localized notice contains counts and `/review` navigation
under the running gateway's startup path prefix (for example `/controller/review`).
Changing live `gateway.path_prefix` does not remount that running router. Notices
never include proposal text, owner evidence, outcome prose or agent names.

Notices enter the existing BridgeOutbox as `source_kind = weekly_review`,
`source_id = <agent alias>`, `event_id = <canonical reflection row id>`. Repeated
checks and gateway restarts reuse its deduplication and delivery receipts;
enqueue errors are logged and retried without re-running the model. No separate
notification ledger is created. Existing quiet hours, exact-source mute,
snooze/dismiss and bridge acknowledgement semantics apply. Dismissing a notice
does not dismiss or approve any review item.

The same task also queues a Soul drift report per enabled agent, under the same
opt-in destination and the same eligibility and live-target rules (there is no
separate key). It computes the agent's drift against its anchor at check time,
with no model call, and queues a counts-only notice with the `/review` path as
`source_kind = soul_drift`, `source_id = <agent alias>` when either holds:

- threshold: any dial is two or more steps from the anchor; `event_id =
  threshold:<anchor>:<fingerprint>`, where `<anchor>` is the anchor row id or
  `default` and the fingerprint lists every dial's current level, so one drift
  state is reported once and a new state is reported again;
- periodic: the drift is non-empty (a dial moved, or Story entries were added or
  retired) and at least one 28-day bucket has elapsed since the anchor was set;
  `event_id = periodic:<anchor>:<bucket>`, with `bucket = (now - set_at) /
  (28 days)`, at most once per bucket.

Both may be queued for the same pass. Deduplication is the outbox's and keeps
its 30-day window, so a drift state that stays unchanged can be reported again
after that window. The notice offers the Review page, where the owner chooses
**Keep as anchor** or **Roll back to anchor**; neither is taken automatically.
Delivery gating treats `soul_drift` rows exactly like `weekly_review` rows.

The producer resolves the live destination for every enqueue. Before claiming an
unclaimed weekly or drift notice, delivery also checks the current destination and agent.
Disabling the target or changing bridge, recipient or thread holds old unclaimed
notices until they become eligible again or expire. Already in-flight messages
cannot be retracted and retain their normal receipt handling. A live data-root
change denies production and delivery through the old store until restart.

Changing bridge or recipient may deliver summaries from the recent 24-hour
window to the newly configured destination. Deduplication is scoped to bridge,
recipient, source and event, and retains the existing 30-day / 10,000-terminal-row
limits. Changing only `thread_id` holds old notices and affects new receipt
identities; it does not rewrite or replay an existing deduplicated event. These
are source/API and synthetic transport guarantees, not real Telegram delivery
or installed-service acceptance. Remove the config section to opt out; retain
existing outbox history and the live-target delivery check during rollback.

## Stable error codes

Errors return JSON with a stable `code` field plus a human-readable `message`.
Frontends and scripts match against the code; UI matches against the path.

| Code | Status | Meaning |
|---|---|---|
| `path_not_found` | 404 | The requested property does not exist in the schema. |
| `validation_failed` | 400 | The whole-config validator rejected the proposed state. |
| `dangling_reference` | 400 | A configured alias reference (e.g. `agents.<x>.model_provider`) names a missing target (e.g. `providers.models.<type>.<alias>`). |
| `value_type_mismatch` | 400 | The submitted JSON value cannot coerce into the target type. |
| `op_not_supported` | 400 | JSON Patch op is `move` / `copy` / unknown. |
| `secret_test_forbidden` | 400 | JSON Patch `test` op targeted a secret path. |
| `config_changed_externally` | 409 | The on-disk config drifted from the in-memory copy. (See drift detection.) |
| `reload_failed` | 500 | The save succeeded but daemon reload could not pick up the new state; on-disk reverted. |
| `internal_error` | 500 | Unclassified server-side failure. |

## Event stream contract

`GET /api/events` is a raw Server-Sent Events stream of observable runtime
events. It is not a deduplicated one-row-per-turn lifecycle timeline.

Gateway handlers, webhook handling, cron/heartbeat work, and agent-loop
observers can all publish lifecycle-shaped events into the same broadcast path.
Clients should treat the stream as an append-only observation log. If a
dashboard wants a compact turn timeline, it should group or deduplicate by the
identifiers present on the event payload rather than assuming each
`agent_start`, `llm_request`, or `agent_end` frame appears only once.

`GET /api/events/history` replays the retained recent events from the same
buffer, oldest first. It is a reconnect window for subscribers, not a separate
canonical lifecycle store.

## HTTP attachments

Attachments use HTTP bytes and opaque IDs; file bytes never travel in client
WS frames. Both routes require an `Authorization: Bearer` header containing a
current paired-device token or a bridge token scoped to `session_id`, even with
pairing disabled. Query/subprotocol tokens are not accepted here. `agent` must
name an enabled agent. An ID belongs to the exact token hash, session ID and
agent that uploaded it. Another token, session or agent cannot fetch or submit
that handle. Revocation and agent disablement apply to subsequent operations;
a slow upload rechecks authority after its body has been read.

- `POST /api/attachments?session_id=main&agent=assistant&file_name=note.txt`
  takes a raw body and its `Content-Type`. It returns HTTP 201 with
  `{id, file_name, mime_type, size, expires_in_secs}`. A filename is a display
  label only; paths, control characters and media-marker delimiters are refused.
- `GET /api/attachments/{id}?session_id=main&agent=assistant` returns those
  bytes as a download, with `Cache-Control: no-store` and `nosniff`. An expired,
  unknown or differently scoped handle returns 404 without revealing its owner.
- Upload and reference requests reject invalid authority. Unsupported MIME or
  image signatures return 415; invalid UTF-8 or oversized text returns 422;
  bodies above the file limit return 413; exhausted capacity returns 429.
  Redirects and remote URLs are not attachment inputs.

Supported input is PNG/JPEG/WebP/GIF or UTF-8 plain text, Markdown, CSV and JSON.
Each file is at most 2 MiB; a text file is at most 64 KiB. The canonical hub store
holds at most 128 items and 32 MiB, with at most 16 items per token/session/agent
scope. Four HTTP uploads may read bodies concurrently. Handles expire 15 minutes
after upload; expired bytes are pruned lazily on store access. This is an
in-memory payload store, with no new database or filesystem access. Handles and
unreferenced bytes do not survive Gateway restart.

Send at most four distinct IDs in a message:

```json
{"type":"message","id":"r-file-1","content":"Read this","attachments":["opaque-upload-id"]}
```

An attachment-only message may use empty `content`. The Gateway resolves every
handle before starting or steering the turn, quotes text as user-provided file
content, and turns image bytes into the existing inline vision representation.
Media-marker prefixes inside a document are quoted, including unclosed markers,
so file text cannot initiate a local-file or URL read. A vision-capable model or
the existing vision route is still required for images. Text remains untrusted
user content, not instructions or an approval decision.

The usual message ACK confirms runtime acceptance; HTTP 201 only confirms
payload storage. A repeated request ID returns its receipt without resolving
expired handles again or running another turn. An unavailable attachment on a
first request emits `ATTACHMENT_UNAVAILABLE`, starts no turn, and records that
request as rejected; resubmitting corrected input needs a new request ID.
Referenced content can enter the existing session history and provider context,
whose retention and visibility policies remain in force after handle expiry.
Scope protection of the HTTP bytes does not change shared conversation visibility.

This leaf supplies bounded HTTP upload/download and inbound text/image mapping.
It does not supply automatic outbound Telegram file delivery, audio transcription,
PDF/binary parsing or PWA upload UI. Telegram source intake/cursor recovery uses
the [durable bridge intake](durable-bridge-intake.md) extension; attachment bytes
remain ephemeral.

Without a durable session backend, request receipts survive idle conversation
release for 16 minutes from their first acceptance. The Gateway admits at most
256 unexpired IDs per session and 1024 such recent sessions; new inputs are
refused when either receipt bound is full rather than evicting live IDs. This does not provide
restart recovery. Attachment handles expire after 15 minutes regardless.
