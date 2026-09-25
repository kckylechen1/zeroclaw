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
| Events | `GET /api/events` (SSE), `GET /api/events/history` |
| Sessions | `GET /api/sessions`, `GET /api/sessions/running`, `GET/POST /api/sessions/{id}/messages`, `PUT/DELETE /api/sessions/{id}`, `GET /api/sessions/{id}/state`, `POST /api/sessions/{id}/abort` |
| Scheduling | `GET/POST /api/cron`, `GET/PATCH /api/cron/settings`, `PATCH/DELETE /api/cron/{id}`, `GET /api/cron/{id}/runs`, `POST /api/cron/{id}/run` |
| Memory and review | `GET/POST /api/memory`, `DELETE /api/memory/{key}`, `/api/user-model/*`, `/api/soul*` |
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

## Governed Soul

The agent's Soul is owner-governed
([ADR-015](../architecture/decisions/ADR-015-one-governed-soul.md),
[ADR-016](../architecture/decisions/ADR-016-growth-proposed-by-agent-approved-by-owner.md)).
Every route below requires the operator identity. The `agent` must be a
configured agent alias; an unknown alias returns 404 with
`code: "unknown_agent"`.

| Route | Purpose |
| --- | --- |
| `GET /api/soul?agent=<alias>` | Current `identity`, `principles`, and `growth` heads (each with `revision`, `source`, and `value`), `voice` (`configured` dials and `stored` per-key heads), `legacy_persona_files` (`injected` or `suppressed`), and `last_reflection`. Seeds missing layers on first read. |
| `GET /api/soul/history?agent=<alias>&layer=identity\|principles\|growth\|voice` | Every revision of one layer, oldest first. |
| `PUT /api/soul/identity` | Body `{ "agent", "expected_revision", "identity": { "name", "self_description"?, "primary_language"?, "pronouns"? } }`. |
| `PUT /api/soul/principles` | Body `{ "agent", "expected_revision", "items": [ ... ] }`, at most 8 single-line items of up to 240 bytes. |
| `PUT /api/soul/growth` | Body `{ "agent", "expected_revision", "growth": { "entries": [ { "kind": "self" \| "bond", "text" } ] } }`, at most 12 single-line entries of up to 200 bytes. |
| `PUT /api/soul/voice` | Body `{ "agent", "expected_revision", "voice": { "heads": { "<key>": "<level>" } } }`. Keys are the five persona dials; a stored head wins over the configured dial for its key. |
| `POST /api/soul/rollback` | Body `{ "agent", "layer", "to_revision", "expected_revision" }`. Appends a copy of an earlier revision. |
| `GET /api/soul/proposals?agent=<alias>[&pending=true]` | The agent's own proposals to change its growth, voice, or principles, oldest first. |
| `POST /api/soul/proposals/{id}/resolve` | Body `{ "agent", "resolution": "accepted" \| "dismissed", "note"?, "final_text"? }`. Accepting applies the proposal and returns `applied_revision`. Each proposal resolves once; a repeat returns 409 with `code: "proposal_already_resolved"`. A Growth retirement is bound to the entry it named when submitted (`retire_target`, read from `target_revision`); if the owner has since removed or reworded that entry, accepting returns 409 with `code: "proposal_stale"` and the proposal stays pending for dismissal. |

Revisions are append-only. Seeded values have `source: "seed"`; owner writes
and rollbacks have `source: "owner"`; approved proposals have
`source: "approved_proposal"` and carry the `proposal_id`. Each write must name
the revision it replaces as `expected_revision` (`0` when the layer has none).
A stale value returns 409 with `code: "revision_conflict"` and
`current_revision`. Invalid input returns 400 with `code: "invalid"` and the
offending `field`. After the first owner-written Identity revision, the legacy
`SOUL.md` and `IDENTITY.md` workspace files stop being injected into the system
prompt.

Model file tools cannot write `SOUL.md`, `IDENTITY.md`, or `USER.md` at an
agent workspace root.

The model's only path into its own Soul is a proposal, either from the
`propose_soul_change` tool mid-conversation or from the weekly reflection. At
most three proposals wait per agent, and identical pending proposals are not
stored twice. Identity is never proposable, and the agent cannot propose
`challenge` below `low`. Accepting a proposal applies it in the same
transaction; `final_text` lets the owner reword a growth entry or principle
before it applies. If the apply fails validation or the layer changed
underneath it, the proposal stays pending. Dismissing applies nothing.

Once every 7 days per agent, the daemon reflects: it reads only the owner's
own `user` messages since the previous reflection (at most the latest 32 KiB),
makes one model call with no tools, and stores at most three validated
proposals. Nothing is applied. The owner's messages are those from operator
surfaces (gateway chat, CLI, TUI) plus channel sessions whose sender is listed
in `[companion_memory.owner].identities`; tool results, injected memory, and
link previews are removed first. The first check only starts the clock, a
week with no owner messages makes no model call, and a failed call is retried
after 6 hours. `last_reflection` reports the period, the number of messages
read, the proposals created, and the outcome.

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
