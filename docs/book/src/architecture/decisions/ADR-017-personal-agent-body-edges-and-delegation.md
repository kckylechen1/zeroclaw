---
id: ADR-017
title: One personal agent body, edge devices, and delegation through Tachi
date: 2026-09-23
status: accepted
relates-to:
  - ADR-011
  - ADR-013
  - ADR-015
  - ADR-016
  - crates/zeroclaw-gateway/src/nodes.rs
  - crates/zeroclaw-runtime/src/tachi_bridge/
  - crates/zeroclaw-runtime/src/execution_subagent/
  - crates/zeroclaw-api/src/session_exec.rs
  - crates/zeroclaw-config/src/tachi.rs
---

# ADR-017: One Personal Agent Body, Edge Devices, and Delegation Through Tachi

## Status

Accepted by the owner on 2026-09-23. It restores the product direction of issues #55 (device fabric) and #198 (ZeroClaw and Tachi convergence), which the 2026-09-23 issue reset had closed, and it fits that direction to ADR-013. It amends one ADR-013 point: the Node role is no longer deferred, and `nodes.rs` is no longer deleted (§2). Where this record and those issues differ, this record wins.

Amended on 2026-09-24 to follow the direction baseline in #388: upstream reuse comes first, a replacement is proven before code is deleted, and hardware leaves the body for a Node host instead of being dropped (§2, §5, §7). The reading order for implementation work is #388, then the execution index #374, then the child issue.

Amended on 2026-09-26 for #381: §8 records how ZeroClaw maps onto Tachi's `tachi_staff` tool, and the Context now describes the launch path Tachi's Staff route really takes (CLI and opencode processes, not `acpx` or native ACP).

## Context

This fork cuts upstream ZeroClaw down to a small personal agent. The owner chose ZeroClaw over heavier agents such as OpenClaw because it is Rust and small.

The owner wants three things from the product:

1. **A companion that grows** (ADR-015, ADR-016).
2. **A body that can reach devices.** It first runs on a computer. A smart speaker, a phone, or a watch then connects to it as an edge device.
3. **A manager of other agents.** It hands work to CLI agents such as Codex and Claude Code, either directly or through Tachi.

The 2026-09-23 reset went too far on the second and third points:

- It closed #55 and #198.
- It planned to delete the Node code (`nodes.rs`) and `voice_duplex` (#375).
- It parked Tachi with a plan to delete it after M1 (#381, #382 S8).

The code also shows a duplication:

- Tachi already drives CLI agents. Its `tachi_staff` facade (`start` / `status` / `cancel`) resolves a dispatch profile to a worker and backend, issues an execution grant, and launches the worker (`claude`, `codex`, `grok`, `kimi`, and others) as a CLI process, or through `opencode` for opencode profiles. It persists the run receipt and reaps the environment. Tachi also has `acpx` and native ACP backends, but the Staff route does not reach them: its profile resolution yields only the `cli`, `opencode_cli`, and `opencode_serve` transports.
- ZeroClaw has its own ACP driver for Codex (`execution_subagent`, about 10.8k lines).
- ZeroClaw also has a `tachi_bridge` port (about 4.6k lines) whose only implementations are in-memory and test ones. Nothing connects it to a real Tachi.

## Decision

### 1. One body

ZeroClaw runs as **one always-on process on a computer the owner controls**, such as a Mac mini, a desktop, or a home server. That process holds:

- the gateway;
- the agent loop;
- the Soul and companion memory;
- the approvals;
- the scheduler.

It is the only identity the owner talks to. Moving the body onto a constrained device is a later goal. It does not shape v1.

### 2. Edge devices are Nodes

A speaker, a phone, a watch, or another computer connects to the body **through the gateway** (ADR-013). #55 gives it two possible roles, and one device may hold both:

- **Client:** a surface that carries conversation. For example, a speaker captures voice and plays replies, and a phone shows chat and approvals.
- **Node:** a device that offers bounded, typed capabilities, such as "play audio", "turn on the light", or "read a sensor".

A Node capability call is an ordinary tool call inside a turn. It is not delegation. The Node enforces its own local permissions, and high-risk capabilities go through the approval chain.

A small robot is a Node too. It runs a Node host on board (for example a Raspberry Pi, or a microcontroller behind a bridge) and advertises capabilities such as `drive`, `look`, `sense`, `speak`, and `emote`. Safety stays on the robot: emergency stop, speed limits, and collision checks run locally and still hold when the link to the body drops. The body never drives serial ports or GPIO in its own process. The in-process hardware crates leave the body (#382 S2). Before they are removed, the slice records their Node-side home or a pinned source reference (`robot-kit` at `e42142161`) with its tests, and it keeps the capability schemas, the admission rules, and the `node_capabilities.rs` approval table. The `robot-kit` capability set and `safety.rs` are the reference for the future robot Node.

Nodes follow the #55 design:

- signed device identity and pairing (#60);
- a headless Node host first (#61);
- an invocation lifecycle with heartbeat, pending-before-send, cancellation, and stale-capability refusal (#62).

`crates/zeroclaw-gateway/src/nodes.rs` already carries the v2 handshake and signed device identity, and it is kept as the starting point, not deleted. What is missing is invocation: a turn cannot yet call a Node capability. `voice_duplex` is kept until the speaker Client is designed, and is deleted only if that design does not use it.

### 3. Three levels of work; Tachi only for delegation

Every request starts at the lowest level. It moves up only when it needs something that level cannot give.

| Level | Who does the work | Examples | Tachi |
|---|---|---|---|
| **L0 Direct** | The body's own turn: tools, MCP servers, and Node capabilities | weather, chat, reminders, calendar, memory, lights, a quick search | never |
| **L1 Reason** | A bounded reasoning subagent inside the body, with no side effects | compare options, summarize a long document | never |
| **L2 Delegate** | A CLI agent (Codex, Claude Code, and others), started and tracked by **Tachi** through `tachi_staff` over MCP | fix a bug, write a script, run a long investigation | always |

Most requests end at L0. A spoken "what is the weather" never waits on Tachi or a subagent.

Delegation has one path. Short versus long, and ephemeral versus durable, become **options of the Tachi request**, not separate code paths in ZeroClaw:

- A **short job** is watched live by the body, which reports the result in the same conversation.
- A **long job** lives on Tachi's board. It survives restarts, and it reaches the owner through proactive delivery (#63) when it finishes.

A request may name a registered, authorized worker or profile as a typed preference, for example "Codex implements, Claude Code reviews". Task text that mentions a harness is ordinary content, not a reason to refuse it. The command, working directory, credentials, transport, sandbox, and allowed tools still come from controlled configuration and policy, never from task text (#381).

An admitted harness may use its own native subagents inside its grant. Those substeps do not route back through the body or Tachi.

### 4. Rules that do not change

- **The owner talks only to the body.** A delegated agent's output is evidence. The body reads it and answers in its own voice. Delegated agents have no Soul.
- **Fail closed, never fall back.** If Tachi is not configured or not reachable, L0 and L1 keep working. A delegation request returns a typed "Tachi unavailable" answer and is never run locally instead. This keeps the existing routing law in `session_exec.rs` and `execution_subagent/router.rs`.
- **Tachi owns delegated run truth.** The body keeps references to Tachi runs and projections of their state, not a second execution ledger. Upstream `control_plane` is not restored beside Tachi.
- **Approvals.** Approvals inside the body are local (ADR-013). Delegated work runs under Tachi's execution grant and permission profile, and its approval requests surface in the same owner inbox. Needing an approval is no longer, by itself, a reason to choose durable execution.
- **The voice path never blocks on delegation.** The body acknowledges ("I've asked Codex to do it; I'll tell you when it's done") and ends the turn.

### 5. What happens to the code

| Code | Decision |
|---|---|
| `tachi_bridge/` | **Keep.** Its real transport is `TachiStaffClient`, ZeroClaw's MCP HTTP client calling `tachi-server`, with the mapping in §8. The older TaskIntent port (submit / get / watch / collect) has no Tachi server; it stays only while `procedure_v1` and `supervisor_v1` compile against it. |
| `execution_subagent/` (ZeroClaw's own ACP driver) | **Delete** after the Tachi transport passes an end-to-end run. Until then, keep it compiling and do not extend it. |
| `subagent_v1/`, `supervisor_v1/`, `procedure_v1/` | Keep the parts that `reasoning_subagent` (L1) and the Tachi bridge use. The rest is reviewed for deletion in #382 S5. |
| `gateway/src/nodes.rs` | **Keep** the v2 handshake, signed device identity, and capability admission. The v1 invocation path (`NodeTool`, `NodeInfo`, `register`) was never reached in production and is removed. The invocation lifecycle is rebuilt on the v2 socket in #62. |
| `gateway/src/voice_duplex` | **Keep** until the speaker Client is designed. |
| Channels, extra providers, config surface, hardware crates, vendor webhooks, admin surfaces | Removed in #374 slices, each only after its consumers are gone, its behavior is explicitly retired, or a replacement is proven (§7). |

### 6. Licensing note

ZeroClaw is MIT OR Apache-2.0. Tachi, memcore, and vault-kit are AGPL-3.0-only.

- Calling Tachi over MCP as a separate process leaves ZeroClaw's license unchanged. This is one more reason delegation goes over MCP.
- The optional `memory-tachi` feature links memcore directly. A binary built with it and **distributed to others** is covered by the AGPL. Personal use is not affected, and the default build does not enable the feature.

### 7. Reuse before rewrite, replace before delete

The body keeps what makes it the owner's agent: identity, Soul and memory, conversation, everyday tools, bounded reasoning, the delegation decision, and the final report. Beneath that, it reuses existing work instead of rebuilding it.

- **Upstream first.** Before writing a capability, check upstream ZeroClaw and the reference projects named in #388. A PR that adopts, adapts, or skips an upstream change names the source PR or commit, the equivalent code already in this fork, and the real gap. Bounded changes are ported; upstream is never merged wholesale, and retired apps and platforms are not restored by it.
- **Replace before delete.** A deletion slice names the real consumers, the replacement or explicit retirement, the tests that keep shared guarantees, and the rollback. Line, package, and crate counts are budgets, not goals, and they never outrank working behavior or tests.
- **Security settings never vanish silently.** Retired UI or channel settings may warn. Authentication, allowlist, credential, privacy, sandbox, and approval settings migrate or block the affected capability.
- **One conversation owner.** Each logical conversation or run has one runtime owner. WebSocket, CLI, and cron adapters call it (the RPC socket was retired in [#408](https://github.com/kckylechen1/zeroclaw/pull/408)) instead of each building their own agent, history, and approvals (#376). Disconnecting a client, cancelling the current turn, and cancelling a delegated job are separate actions.
- **Modules before processes.** A module boundary is not automatically a process boundary. The default install stays one manageable package; Tachi and any optional inference gateway or relay are installed, started, checked, and upgraded as part of it.

### 8. ZeroClaw ↔ `tachi_staff` mapping

This section is the contract between the body and Tachi for L2 work (#381). The client is `TachiStaffClient` in `crates/zeroclaw-runtime/src/tachi_bridge/staff.rs`. It is checked against Tachi's golden fixture `external-staffing-contract-v1.fixture.json` and against a scripted MCP server in its tests.

**Transport.** ZeroClaw calls the Tachi daemon over MCP streamable HTTP, using its own MCP transport. It opens one MCP session and reuses it. If Tachi restarts and the session goes stale, the client opens a new session once. Each session sends these headers:

| Header | Value |
|---|---|
| `x-tachi-profile` | `standard` |
| `x-tachi-agent-identity` | `[tachi].agent_identity`, or `zeroclaw:<agent alias>` |
| `x-tachi-client` | `zeroclaw` |
| `x-tachi-project` | `[tachi].project`, only when it is set |

**Operations.**

| ZeroClaw | Tachi call | Answer |
|---|---|---|
| `start(harness, task, reason, refs)` | `tachi_staff` `action=start`, with `task`, `staffing_reason`, `profile`, and the optional `project`, `issue_ref`, `pr_ref`, and `flow_id` | a receipt: `dispatch_id`, `state` (`TASK_STATE_WORKING`), `run_dir` |
| `status(dispatch_id)` | `tachi_staff` `action=status` | the canonical `status.json`: `state`, `status_revision`, `closure_kind`, and more |
| `result(dispatch_id)` | `tachi_task` `action=status`, `include_result=true` | the `result.md` text, capped by Tachi at 8000 characters |
| `cancel(dispatch_id, revision)` | `tachi_staff` `action=cancel`, `expected_status_revision` | a cancellation receipt, or a typed "unavailable" |

A run is finished when its state is `TASK_STATE_COMPLETED`, `TASK_STATE_FAILED`, or `TASK_STATE_CANCELED`, or when it is `TASK_STATE_INPUT_REQUIRED` with `closure_kind = "partial"`.

The client reads only the fields it uses and ignores the rest. Tachi refuses unknown request fields, so the client sends only fields that `TachiStaffParams` declares. It never sends `worker`, a command, a working directory, credentials, or a sandbox. Tachi resolves all of those from the profile.

**Harness to profile.** The body asks for a harness by name, for example `codex`. `[tachi.harnesses]` maps each name to a Tachi dispatch profile, for example `codex = "codex_55_review"`. A name that is not listed is refused before Tachi is contacted. The list is empty by default, so no harness is available until the owner adds one.

**Staffing reason.** Tachi refuses a start without a reason. When the owner asked for the delegation, the reason is `explicit_user_request`. Otherwise the model picks the true reason from Tachi's list: `durable_cross_session`, `cross_device_remote`, or `native_subagent_unavailable`. The client never picks a default reason.

**Polling.** Tachi has no event stream for Staff runs yet, so the body polls `status`. The interval is `[tachi].poll_secs`, 15 seconds by default. The body keeps only the `dispatch_id`, never a copy of the run.

**Cancel.** Tachi can cancel only runs that it manages itself on the same daemon (`managed_custom`). For a CLI run, the answer is `CancelUnsupported`, and the body tells the owner plainly that the run cannot be stopped from here. A cancel quotes the `status_revision` from the last status read. A stale revision is reported as `StaleRevision`, and the body reads the status again.

**Same machine only.** `[tachi].endpoint` must use `http://` with a loopback host (`127.0.0.0/8`, `::1`, or `localhost`). Otherwise validation fails. Tachi's HTTP MCP endpoint does not authenticate callers, so v1 has no setting to allow a remote Tachi. A later change can allow one after Tachi adds caller authentication.

**Fail closed.** `[tachi]` is disabled by default. If it is disabled or invalid, if the daemon is down, or if the transport fails, the answer is `Unavailable`, and nothing runs locally instead (§4). Other errors are typed too:

- `Refused`: Tachi rejected the request (a tool error or a JSON-RPC error).
- `UnknownHarness`: the harness name is not in `[tachi.harnesses]`.
- `CancelUnsupported`: Tachi cannot cancel the run.
- `StaleRevision`: the cancel quoted an old `status_revision`.
- `Protocol`: the response could not be read.

**Task text.** A task that mentions a harness, vendor, or model is sent as written (§3). The TaskIntent composer no longer refuses vendor names. It still refuses text that tries to choose where or how the work runs, such as a working directory, a worktree, tmux, SSH, a sandbox, or CLI flags.

**Worker context.** Only the task text, the staffing reason, and the references leave the body. The body never copies Soul or User Model bytes into a start (ADR-014, ADR-015, ADR-016). A worker gets the task, not the owner's identity or profile. When a task needs a fact about the owner, the body states that one fact in the task text.

**Gaps tracked in Tachi** ([kckylechen1/Tachi#2003](https://github.com/kckylechen1/Tachi/issues/2003)):

- Tachi cannot cancel CLI-backed Staff runs.
- There is no event stream, so the body has to poll.
- `result.md` is served by `tachi_task`, not by `tachi_staff`.
- The HTTP endpoint does not authenticate callers.
- A start has no idempotency key. If the connection drops after the request is sent, the body cannot tell whether the run started. It reports `Unavailable` and does not retry the start.

## Consequences

Positive consequences:

- One CLI-driver implementation, in Tachi. Supporting a new agent is a Tachi change only.
- ZeroClaw gets smaller: about 10.8k lines of ACP driver are replaced by a thin MCP-backed bridge.
- Everyday use (L0 and L1) has no dependency on Tachi.
- The speaker, phone, and future devices share one gateway and one identity.

Negative consequences:

- Delegation requires a running Tachi daemon on the same computer or on a reachable one.
- Two repositories must agree on the `tachi_staff` wire contract. A change there is a cross-repository change.
- The Node work (#60–#62) returns to the plan, and it is not small.

## Acceptance

This ADR is implemented when:

1. a spoken or typed L0 request (for example, weather) completes with Tachi stopped;
2. a delegation request with Tachi stopped returns a typed "unavailable" answer and runs nothing locally (test);
3. `tachi_bridge` has a production MCP transport, and one real Codex or Claude Code job runs end to end: started by the body, tracked, and reported in the body's voice;
4. `execution_subagent` is deleted;
5. one headless Node pairs, advertises a capability, is invoked by a turn, disconnects, reconnects, and is revoked (#61, #62).

The whole-package acceptance (two harnesses, one Node, shared conversation across clients) is tracked in #374.

## References

- Issue #55: device fabric (Channel / Client / Node / Gateway)
- Issue #198: ZeroClaw and Tachi convergence (Reason / Ephemeral / Durable)
- Issues #60, #61, #62: Node identity, headless host, invocation lifecycle
- Issue #63: attention and proactive delivery
- Issue #381: Tachi integration
- Issue #382: reuse and replacement-first deletion
- Issue #388: direction baseline (reuse, module boundaries, replace before delete)
- [ADR-013: Channels as gateway clients](./ADR-013-channels-as-gateway-clients.md)
- Tachi `crates/tachi-server/src/tools/workflow_facade.rs` (`tachi_staff` handler), `crates/tachi-server/src/staffing_ops/`, and `crates/tachi-params/src/facade/orchestration.rs` (`TachiStaffParams`)
- Tachi `docs/engineering/architecture/external-staffing-contract-v1.fixture.json` (golden Staff contract)
- [kckylechen1/Tachi#2003](https://github.com/kckylechen1/Tachi/issues/2003): cancel of CLI runs, events, results on the staff facade, and caller authentication
