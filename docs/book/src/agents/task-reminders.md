# Task reminders

The durable task list (`task_list` tool) carries multi-day commitments; any
item can additionally arm **one reminder**: an attention notice the gateway
delivers through the existing bridge outbox when the time comes. There is no
second notification system: the outbox, the attention policy (quiet hours,
mutes, snooze) and the delivery sockets are the same ones cron results,
review notices, and delegated-run notices use.

## Arming

`task_list` accepts `remind_at` on `add` and `update`:

- RFC3339 **with a UTC offset** (`2026-11-07T09:00:00Z`,
  `2026-11-07T17:00:00+08:00`). A naive timestamp is an error: the agent
  asks for the timezone, it never guesses.
- On `update`, `remind_at: null` clears the reminder; omitting the field
  keeps it (description edits do not disturb an armed reminder).
- Setting it again reschedules: the old occurrence is invalidated and a new
  one armed, atomically in the same revision-checked batch.
- Completing or removing the task cancels its pending reminder; reopening a
  completed task never revives an old one.
- A reminder more than 24 hours past its due time is dead on arrival and
  refused at arming.

## Destination

Reminders go to one owner-configured destination, copied into the occurrence
at arming time:

```toml
[tasks.reminder_target]
bridge = "telegram" # Must already exist in gateway.bridges.
recipient = "<owner-chat-id>"
# thread_id = "<optional-topic-id>"
```

Without a valid target, `remind_at` is refused with an error that says so;
the agent relays it instead of silently dropping the reminder. The
destination is **bound at creation**: changing the target later never
rewrites or redirects already-armed reminders. The delivery gate compares a
queued reminder against the *current* target and withholds it on any
mismatch (target changed, removed, bridge removed, agent disabled);
restoring the original target resumes still-valid, unclaimed rows.

## Firing

A gateway background pass (once a minute) scans due occurrences. An
occurrence is deliverable for exactly 24 hours after its due time: an
absolute expiry, so a gateway that comes back online a month late finds
nothing to send rather than resurrecting stale reminders. Firing makes **no
model call**: the notice is a fixed localized line
(`task-reminder-notice`) carrying the task description.

Cancellation races are ordered, not guessed:

- the task is completed/cancelled **before the delivery claim** ⇒ no send
  (the claim runs inside the reminder store's write gate, serialized
  against the invalidating batch);
- the claim is already made ⇒ no promise of retraction (the outbox's
  existing uncertain-send semantics).

One candidate per occurrence, ever: the occurrence row is the durable
fire-once marker, the outbox's `(source_kind, source_id, event_id)` identity
covers the enqueue→mark crash window, and reminder receipts are protected
from capacity eviction until the occurrence's deadline passes. "At most one
delivery claim" is the invariant; exactly-once *platform* delivery is not
promised (see the bridge outbox chapter).
