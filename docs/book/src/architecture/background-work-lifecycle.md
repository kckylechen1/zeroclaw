# Background work lifecycle

ZeroClaw has several ways to continue work after the inbound request that started it. Cron jobs, delegated tasks, and runtime-spawned subagents share some execution machinery, but they do not share one lifecycle or one durable store. (SOP runs were removed with the run side; see the section below.) Goal mode defines a related target contract that is not yet wired end to end.

Use this page when a change adds scheduled or autonomous work, introduces a wait or approval state, changes cancellation or restart behavior, or connects child work to an owning task. The first design question is not "how does it run in the background?" but "which subsystem owns its lifecycle?"

## Ownership map

| Work type | Current owner or status surface | Durable records |
| --- | --- | --- |
| Cron job | Cron scheduler and store | `data/cron/jobs.db` |
| SOP run | Removed with the run side | The legacy `SopEngine`/`SopRunStore` were demolished; a leftover `data/sop/runs.db` is left in place with a boot-time WARN |
| Background delegation | Removed with the control plane | The coordinator child store and the `data/control_plane.db` ledger were deleted; a leftover file is left in place with a boot-time WARN |
| Runtime-spawned subagent | V1 `reasoning_subagent` (run-scoped) and the Tachi bridge for durable work | None locally: durable task/attempt truth lives in Tachi |

Durable metadata is not the same as durable execution. A task row can preserve what was known and let recovery mark work lost, timed out, or terminal without preserving the process-local future that was doing the work.

## Cron jobs

Cron combines declarative membership with a SQLite execution store. Runtime-created jobs and reconciled config jobs both carry an owning `agent_alias`; execution resolves that agent's security policy instead of running under an ambient daemon identity.

The scheduler polls for due, enabled, unclaimed rows. Claiming a row prevents duplicate selection while it is in flight. Completion records bounded output, then reschedules a recurring job, deletes a successful auto-delete one-shot, or disables another one-shot. If the process exits before releasing a claim, the next scheduler startup clears the stale lock.

Startup behavior is explicit. With catch-up enabled, overdue jobs are considered for execution. Otherwise an overdue one-shot is disabled with a skipped result, while a recurring job advances to its next future occurrence without recording a run result. The scheduler checks its cancellation token between polling iterations, so shutdown waits for the current due-job batch to finish before the loop exits. Cancelling the scheduler is not a promise that an already-dispatched external side effect can be rolled back.

## SOP runs

SOP definitions live under the configured `sops` directory. Run progression, approval waits, checkpoints, and terminal transitions were owned by the legacy `SopEngine`, with `SopRunStore` as the concurrency source of truth; both were removed with the run side.

SOP run persistence was removed with the run side: `sop.persist_runs`, `run_store_backend`, and `run_state_dir` no longer configure anything, and the `SopEngine`/`SopRunStore` pair that consumed them was demolished. A leftover `data/sop/runs.db` from an older install is left in place and reported by a boot-time warning that names the migration path.

SOP audit records in the Memory backend are a separate observability surface. They do not replace the run store and must not be used as the authority for whether a run is active, paused, approved, or terminal.

Approval and checkpoint states are durable control states only when the run store is durable. Timeout policy remains fail-closed by default: a timed-out approval escalates and keeps waiting unless config explicitly selects cancellation or the legacy auto-approve behavior.

## Delegation and subagents

The in-kernel child-spawn tools are retired. The legacy `spawn_subagent` tool (in-turn or detached through the coordinator) and the older `delegate` tool both inherited full parent authority on child runs, which the frozen SubAgent contract forbids; `spawn_subagent` was removed in the #197 spawn wall, and bounded reasoning work now goes through the V1 `reasoning_subagent` entry point (no ambient parent inheritance, structured report, no detached mode). Durable/heavy background work belongs to the Tachi bridge.

The durable control plane itself (the coordinator child host with its admission, persistence, cancellation, and announce chain, plus the `data/control_plane.db` task ledger) was deleted with the control-plane migration wall: its last production writer died with the spawn wall, and durable task/attempt truth is Tachi's through the task-intent bridge (frozen bridge contract annex rows 1 and 6). A leftover `data/control_plane.db` on an older install is never read, migrated, rewritten, or deleted; the daemon reports it once per boot with a warning naming the disposition. Pre-migration `delegate_results/*.json` files are likewise ignored.

## Goal-mode target contract

[ADR-008](./decisions/ADR-008-goal-mode-control-plane-and-usage-accounting.md) accepted the task control plane as the future authority for goal lifecycle, ownership, route, principal, parent relation, and recovery eligibility; it is now superseded: the ZeroClaw-side control plane it anchored on was deleted by the control-plane migration wall, and durable task truth lives in Tachi through the task-intent bridge. Goal-mode execution is not wired end to end on any surface today.

A background path may participate in goal mode only after it preserves the owning goal relationship and reports terminal state and model usage back to it. Until then, that path is ordinary background work rather than goal-mode execution.

## Change checklist

For background-work changes, answer these before reviewer sign-off:

- Which subsystem owns the lifecycle and which store is authoritative?
- Is the work process-local, durably supervised, or actually restart-resumable?
- Which token or control-plane action cancels it, and what can remain in flight?
- Which parent task, agent, route, principal, recursion depth, and usage fields does this path actually populate?
- Are waiting, approval, checkpoint, lost, timed-out, and terminal states distinguishable?
- Can startup recovery duplicate a side effect or silently strand a claim?
- Does result delivery remain idempotent if completion is observed after restart?

## Source pointers

- Cron scheduler and persistence: `crates/zeroclaw-runtime/src/cron/scheduler.rs`, `crates/zeroclaw-runtime/src/cron/store.rs`
- SOP run side: removed with the run side; the former `sop/engine.rs` and `sop/store/` pointers no longer exist (a legacy `data/sop/runs.db` on an old install is left in place and reported by a boot-time warning)
- Delegation and subagent behavior: [Delegation & SubAgents](../agents/delegation.md), `crates/zeroclaw-runtime/src/subagent/mod.rs`, `crates/zeroclaw-runtime/src/subagent_v1/` (the retired `tools/spawn_subagent.rs` and `tools/delegate.rs` were deleted with their walls)
- Durable task control plane: removed with the control-plane migration wall; durable work runs through the Tachi bridge (`crates/zeroclaw-runtime/src/tachi_bridge/`)
- Goal-mode decision: [ADR-008](./decisions/ADR-008-goal-mode-control-plane-and-usage-accounting.md)
- SOP operator guide: [How SOPs run](../sop/how-it-works.md)
