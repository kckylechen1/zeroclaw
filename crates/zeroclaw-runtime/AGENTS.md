# zeroclaw-runtime — Runtime ownership and transition rules

This crate holds the personal agent's runtime and turn lifecycle during the
ongoing extraction. It is **not** a catch-all home for new subsystems, but it is
also not governed by the older plan to move every subsystem to a separate
crate or a WASM plugin.

## Authority

- The repository-root [AGENTS.md](../../AGENTS.md) is binding.
- The accepted [ADR-013](../../docs/book/src/architecture/decisions/ADR-013-channels-as-gateway-clients.md)
  places external channel ingress at gateway clients and the agent turn
  lifecycle in the runtime.
- The accepted [ADR-017](../../docs/book/src/architecture/decisions/ADR-017-personal-agent-body-edges-and-delegation.md)
  places external harness delegation through Tachi, and device execution
  on separately authorized Nodes.
- The current execution order and replacement gates live in
  [issue #374](https://github.com/kckylechen1/zeroclaw/issues/374);
  [fork notes](../../docs/book/src/contributing/fork-notes.md) record
  retained differences from upstream.

## Rules for changes here

1. Keep existing turn, approval, identity, recovery and security behavior
   working while ownership is migrated. Never interpret a planned extraction
   as evidence that a replacement is complete.
2. Do not add a second agent loop, in-process channel orchestrator, external
   harness launcher, independent durable execution ledger, or direct
   hardware/GPIO control path alongside the accepted owners.
3. Make bounded changes to the **current** runtime owner when required by an
   approved issue. Move or retire consumers only after the specified replacement,
   parity tests and end-to-end acceptance pass.
4. Do not weaken authentication, allowlists, credential handling, privacy,
   sandboxing, tool approvals, the `/ws/chat` contract, Node safety or the Tachi
   bridge to reduce the crate's size.

**Stability tier:** Experimental. The parent contract and accepted ADRs
override historical extraction roadmaps and LOC estimates.
