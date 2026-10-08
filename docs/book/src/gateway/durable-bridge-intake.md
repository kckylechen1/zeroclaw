# Durable bridge intake

Telegram uses an opt-in version 1 extension of `/ws/chat`. The existing
session backend owns input bodies, receipts and source cursors in `sessions.db`
(`bridge_sources` and `bridge_inputs`). There is no separate bridge database.
Legacy chat messages and their steering behavior keep their existing contract.

## Identity and authority

The bridge calls Telegram `getMe` and builds `telegram:<numeric bot id>:<owner id>`.
Each update has the stable request ID `tg:<bot id>:<owner id>:<update_id>`. Bot-token
rotation preserves this identity. The journal binds the configured Gateway bridge
name and source to one session and agent; changing that mapping fails closed.
Ambiguous credentials shared by multiple bridge entries cannot negotiate intake.

Source intake requires a current scoped bridge token, an enabled agent and a
durable session backend. Paired tokens, tokenless connections and unsupported
backends cannot negotiate it. Authority is rechecked before each input executes.
The existing bearer/header authentication transport remains unchanged.

## Handoff and recovery

A client first sends `{"type":"source_resume","source":"telegram:7:9"}`.
The Gateway answers `source_ready` with `intake_version: 1`, its committed cursor,
retained receipt metadata and IDs with unknown outcomes. Message frames add:

```json
{"type":"message","id":"tg:7:9:10","content":"hello","source":{"namespace":"telegram:7:9","update_id":10,"previous_cursor":0}}
```

`previous_cursor` links observed updates, rather than assuming integer IDs are
consecutive. Ignored, rejected and control updates use `source_disposition` with
the same identity and a `disposition` field. Unauthorized Telegram bodies are
never sent to the Gateway: only the ignored identity is recorded.

One transaction stores the complete input and advances the cursor over the
committed observed prefix. The ACK contains the matching ID, `status`
(`accepted` or `duplicate`), `durable: true`, `intake_version: 1`, `state` and
`source` (`namespace`, `update_id`, `cursor`). Socket-write success and unknown
acceptance semantics never release an update. The bridge only confirms Telegram
updates by polling with a safe offset after this durable handoff.

The accepted input payload also stores its optional chat surface. Recovery
resolves presentation from that immutable payload, not the most recently
attached socket. A duplicate under a different surface retains the original
payload; changed content or attachment handles remain conflicts. Older payloads
without a surface retain legacy presentation even when a new client registers
one. No schema migration or separate register store is needed.

Ordinary source inputs wait as `pending` and run as separate turns in source
order. A gap can be persisted but cannot execute or advance the prefix. An atomic
`pending` to `running` claim precedes the Agent call. Restart resumes only
`pending` inputs; `running`, `control` and `outcome_unknown` are reported without
automatic replay. `done`, `error`, `aborted`, `rejected` and `ignored` are terminal.
A crash after claim can leave an unknown outcome even before effects began.
Acceptance does not mean completed effects or successful Telegram reply delivery.

Controls are recorded before their existing waiter/callback/cancel operation.
A lost ACK or restart never automatically repeats them. Question/approval
waiters retain their existing scope and expiry checks. Full turn-bound `/stop`
and delegated-job cancellation acceptance remain tracked by #377/#381.

## Bounds and deletion

The journal allows 128 inputs per source, 4096 total inputs and 64 MiB of input
payloads, with at most 4096 source identities. Admission refuses excess capacity.
Only settled rows older than 16 minutes and below the committed cursor can be
reclaimed under pressure. Pending and uncertain rows remain. Source watermarks
never expire; an old ID without a retained receipt is refused rather than rerun.
Gateway scheduling admits at most 1024 registered sources per process.

Clearing or deleting session history erases journal bodies and retains identity
tombstones: pending becomes rejected, running becomes unknown. This does not
promise erasure of already-loaded Agent state or external effects. TTL cleanup
keeps sessions with unresolved intake. Input bodies have the same local database
privacy boundary as existing session history.

Attachment handles are stored with the input, but attachment bytes still expire
after 15 minutes and do not survive Gateway restart. If any handle is unavailable,
the whole input is rejected; the caption never runs alone. The owner must send a
new attachment explicitly. Outbound files and durable attachment bytes are separate
work. Turn-output frames missed during disconnect are not replayed.

Telegram retains unconfirmed updates for at most 24 hours. After prolonged idle,
its next ID may be chosen randomly. Startup and idle probes use offset zero; an
unrecognized ID below the watermark holds polling and fails closed for operator
reconciliation. No end-to-end exactly-once or indefinite offline-retention claim
is made.

## Upgrade, rollback and provenance

Upgrade Gateway before the Telegram bridge and retain the same bridge name,
session, agent and owner mapping. An older Gateway or unsupported source version
leaves polling closed. No new configuration is required. To roll back, stop the
bridge, retain `sessions.db`, then revert the implementation. An old bridge does
not provide durable intake; do not resume it as if it preserved this guarantee.

The upstream inventory used `zeroclaw-labs/zeroclaw` commit
`19c40eea3ec0926f5938f299a9007baaf9a9b09c`, particularly its Telegram ordered-prefix
and duplicate-queue handling. Its in-memory channel send was not adopted as a
durable acceptance boundary; the existing local session backend owns that gap.
Transport references: [getUpdates](https://core.telegram.org/bots/api#getupdates),
[getMe](https://core.telegram.org/bots/api#getme).
