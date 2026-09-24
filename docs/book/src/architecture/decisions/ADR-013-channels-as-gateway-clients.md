---
id: ADR-013
title: Channels are out-of-process gateway clients
date: 2026-09-23
status: accepted
relates-to:
  - ADR-006
  - ADR-007
  - docs/book/src/foundations/fnd-001-intentional-architecture.md
  - docs/book/src/architecture/channel-runtime-lifecycle.md
  - crates/zeroclaw-gateway
  - crates/zeroclaw-channels
---

# ADR-013: Channels Are Out-of-Process Gateway Clients

## Context

This fork is being reduced from the upstream multi-channel platform to a small personal agent. The channel layer is the largest single obstacle:

- `zeroclaw-channels` is about 142k lines covering more than 40 messaging platforms. Every change to the agent turn has to keep all of them compiling and passing tests.
- `zeroclaw-channels/src/orchestrator` (about 36k lines) owns runtime-sized work: the dispatch loop, hooks, memory recall, the tool loop, draft streaming, cancellation, receipts, and cost. [Channel runtime lifecycle](../channel-runtime-lifecycle.md) already records this as transition debt.
- The runtime links concrete channels. [FND-001](../../foundations/fnd-001-intentional-architecture.md) says that a runtime importing `TelegramChannel` violates the architecture. Today only review prevents it; the compiler does not.
- [ADR-006](./ADR-006-runtime-channel-plugins.md) chose WASM runtime plugins as the target for optional channels. None of its four acceptance conditions has shipped. The host capabilities that WASM channels would need (long-lived network listeners, webhook ingress, secret delivery, media, signing and distribution) are ecosystem infrastructure that a single-owner personal agent does not need.
- The gateway already exposes the client contract a channel needs. `/ws/chat` accepts `message` and `approval_response` frames. It emits `chunk`, `done`, `tool_call`, `tool_result`, `approval_request`, `cron_result`, and `error` frames. It authenticates paired tokens and binds each connection to an agent alias and a session.

Comparable personal agents (Meta Muse, and the OpenMuse and nanoMuse community projects) run one always-on process. That process exposes REST and a WebSocket event stream, and every surface (phone app, CLI, messaging) is a client of that one API. Issue #55 defined the same roles for this repository (Channel, Client, Node, Gateway) and chose to evolve the existing gateway rather than create a second one.

## Decision

1. **The gateway is the only external door to the agent.** The kernel ships two ingress surfaces: the local CLI and the gateway (REST plus the `/ws/chat` event stream). The gateway owns only realtime coordination: sockets, streaming, pairing and authentication, bounded reconnect state, and approval relay. This matches #180 §5. It does not own durable task, memory, or identity truth.
2. **A channel is a bridge: a client of the gateway API.** A bridge translates one messaging platform to and from `/ws/chat` frames. It does not link `zeroclaw-runtime`, `zeroclaw-memory`, or `zeroclaw-tools`. It depends only on a small gateway client library and its platform SDK. For deployment convenience a bridge may ship as a subcommand of the main binary (for example `zeroclaw bridge telegram`). It still runs as a separate task with its own connection and never calls runtime internals.
3. **The agent turn lifecycle belongs to the runtime.** The dispatch loop, turn processing, approvals, cancellation, and delivery move out of `zeroclaw-channels/src/orchestrator`. After that move, `zeroclaw-channels` holds only platform adapters. It is deleted after the shared conversation service (#376) carries its general behavior and the first bridge passes a real end-to-end run with acceptance, recovery, and approvals (#377, #378).
4. **Supersession.**
   - This ADR supersedes [ADR-006](./ADR-006-runtime-channel-plugins.md) for channels. WASM plugins under [ADR-009](./ADR-009-wit-wasmtime-plugin-execution.md) remain the extension model for tools.
   - It also supersedes [ADR-007](./ADR-007-gateway-extraction.md). The personal agent runs as one process, and bridges are the process boundary that matters.
5. **Owner rulings recorded with this decision.**
   - **Approvals:** the local approval path (`approval`, `ws_approval`) is the approval authority for the body's own work. Delegated work runs under Tachi's grant and surfaces its approval requests in the same owner inbox (ADR-017 §4). New work must not add a second approval store.
   - **Nodes:** amended by ADR-017 §2. The Node role is active, and `nodes.rs` and `/ws/nodes` are kept as the base for production invocation (#61, #62).
   - **Shell:** `shell`, `file_write`, and `file_edit` stay in the minimal composition behind the existing approval chain. They must not become a hidden second path for launching external harnesses; that path is Tachi (ADR-017 §3).

## Consequences

Positive consequences:

- Removing `zeroclaw-channels` deletes about 140k lines and the vendor SDK dependencies they pull in.
- The compiler enforces the FND-001 layering: a bridge cannot import runtime internals.
- Channels can be added one at a time, at any pace, without touching the kernel. A bridge can be written in any language.
- Phone, web, and messaging surfaces share one approval, streaming, and proactive-delivery path. Per-platform callback handling no longer needs to be reimplemented in each channel.

Negative consequences:

- A bridge holds a persistent connection and depends on gateway availability. A gateway restart interrupts every bridge until it reconnects.
- The bridge has to enforce platform-level sender allowlists itself, because the gateway sees a single paired identity per bridge.
- Per-platform features beyond text, choice prompts, and attachments need an explicit extension to the `/ws/chat` frame set instead of an ad-hoc channel method.
- Channels that exist only upstream are no longer available until someone writes a bridge for them.

## Acceptance

This ADR is implemented when:

- `zeroclaw-gateway` no longer depends on `zeroclaw-channels`;
- the turn lifecycle formerly in `zeroclaw-channels/src/orchestrator` runs from `zeroclaw-runtime`;
- a Telegram bridge built only on the gateway client library supports conversation, streaming, approvals, and `cron_result` delivery, and advances its platform offset only after an application-level acceptance ACK (#377); and
- `zeroclaw-channels` is removed from the workspace.

## References

- [FND-001: Intentional architecture](../../foundations/fnd-001-intentional-architecture.md)
- [Channel runtime lifecycle](../channel-runtime-lifecycle.md)
- [Gateway API](../../gateway/api.md)
- [ADR-006: Runtime channel plugins](./ADR-006-runtime-channel-plugins.md)
- [ADR-007: Gateway extraction](./ADR-007-gateway-extraction.md)
- `crates/zeroclaw-gateway/src/ws.rs`
- `crates/zeroclaw-gateway/src/ws_approval.rs`
- [OpenMuse](https://github.com/OpenMuseAgent/OpenMuse)
