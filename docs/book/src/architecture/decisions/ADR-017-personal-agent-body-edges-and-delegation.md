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
---

# ADR-017: One Personal Agent Body, Edge Devices, and Delegation Through Tachi

## Status

Accepted by the owner on 2026-09-23. It restores the product direction of issues #55 (device fabric) and #198 (ZeroClaw and Tachi convergence), which the 2026-09-23 issue reset had closed, and it fits that direction to ADR-013. It amends one ADR-013 point: the Node role is no longer deferred, and `nodes.rs` is no longer deleted (§2). Where this record and those issues differ, this record wins.

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

- Tachi already drives CLI agents over ACP. Its `tachi_staff` facade (`start` / `status` / `cancel`) selects a worker and backend, issues an execution grant, and launches `claude`, `codex`, `grok`, or `kimi` through `acpx` or its native ACP client. It persists the events and reaps the environment.
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

Nodes follow the #55 design:

- signed device identity and pairing (#60);
- a headless Node host first (#61);
- an invocation lifecycle with heartbeat, pending-before-send, cancellation, and stale-capability refusal (#62).

`crates/zeroclaw-gateway/src/nodes.rs` is kept as the starting point for that work, not deleted. `voice_duplex` is kept until the speaker Client is designed, and is deleted only if that design does not use it.

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

### 4. Rules that do not change

- **The owner talks only to the body.** A delegated agent's output is evidence. The body reads it and answers in its own voice. Delegated agents have no Soul.
- **Fail closed, never fall back.** If Tachi is not configured or not reachable, L0 and L1 keep working. A delegation request returns a typed "Tachi unavailable" answer and is never run locally instead. This keeps the existing routing law in `session_exec.rs` and `execution_subagent/router.rs`.
- **Approvals.** Approvals inside the body are local (ADR-013). Delegated work runs under Tachi's execution grant and permission profile, and its approval requests surface in the same owner inbox. Needing an approval is no longer, by itself, a reason to choose durable execution.
- **The voice path never blocks on delegation.** The body acknowledges ("I've asked Codex to do it; I'll tell you when it's done") and ends the turn.

### 5. What happens to the code

| Code | Decision |
|---|---|
| `tachi_bridge/` | **Keep.** Give it a real transport: ZeroClaw's MCP client calling `tachi-server`. Adapt its port (submit / get / watch / collect) to `tachi_staff` start / status / cancel. The mapping is designed in its own leaf before any code is written. |
| `execution_subagent/` (ZeroClaw's own ACP driver) | **Delete** after the Tachi transport passes an end-to-end run. Until then, keep it compiling and do not extend it. |
| `subagent_v1/`, `supervisor_v1/`, `procedure_v1/` | Keep the parts that `reasoning_subagent` (L1) and the Tachi bridge use. The rest is reviewed for deletion in #382 S5. |
| `gateway/src/nodes.rs` | **Keep.** Fix the two known races (#62), then build on it. |
| `gateway/src/voice_duplex` | **Keep** until the speaker Client is designed. |
| Channels, extra providers, config surface, hardware crates unrelated to Nodes, vendor webhooks, admin surfaces | Cut as planned in #374. |

### 6. Licensing note

ZeroClaw is MIT OR Apache-2.0. Tachi, memcore, and vault-kit are AGPL-3.0-only.

- Calling Tachi over MCP as a separate process leaves ZeroClaw's license unchanged. This is one more reason delegation goes over MCP.
- The optional `memory-tachi` feature links memcore directly. A binary built with it and **distributed to others** is covered by the AGPL. Personal use is not affected, and the default build does not enable the feature.

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

## References

- Issue #55: device fabric (Channel / Client / Node / Gateway)
- Issue #198: ZeroClaw and Tachi convergence (Reason / Ephemeral / Durable)
- Issues #60, #61, #62: Node identity, headless host, invocation lifecycle
- Issue #63: attention and proactive delivery
- Issue #381: Tachi integration
- [ADR-013: Channels as gateway clients](./ADR-013-channels-as-gateway-clients.md)
- Tachi `crates/tachi-server/src/dispatch_ops/` (`acpx`, `acp_native`, `tachi_staff`)
