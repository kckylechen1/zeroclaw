# Fork Notes

This page keeps the fork-specific facts that used to live in `AGENTS.md`. The short rules are in [`AGENTS.md`](https://github.com/kckylechen1/zeroclaw/blob/master/AGENTS.md); product direction is in [ADR-013](../architecture/decisions/ADR-013-channels-as-gateway-clients.md) and [ADR-017](../architecture/decisions/ADR-017-personal-agent-body-edges-and-delegation.md).

## Where new types go

`crates/zeroclaw-config/src/schema.rs` is about 22.5k production lines of config structs on `agent-runtime`'s mandatory path, so every consumer pays for every subsystem's config at build time. The release binary is unaffected because LTO discards what is never instantiated; the cost is build time and disk.

1. **Shared wire and domain types go in `zeroclaw-api`.** A type two crates both name belongs there.
2. **Logic stays where it runs.** `zeroclaw-api` holds the shape; the owning crate holds the behavior. A consumer that only reads a result must not link the runtime that produces it.
3. **New config sections get their own module** under `zeroclaw-config/src/` (see `persona.rs`), not another block in `schema.rs`.
4. **Do not restructure `schema.rs` to fix this.** Shrink it by deleting sections whose readers are gone (#382 S7). Schema unit tests live in `zeroclaw-config/src/schema/tests.rs`.

## Upstream relationship

This repository is an independent project (decision 2026-08-16). Upstream `zeroclaw-labs/zeroclaw` is a reference to port from deliberately, not a stream to track: no scheduled rebases and no wholesale merges.

- **Baseline:** history contains upstream work through upstream PR #9356; `master` `40cc158e8` (2026-08-16) is the last wholesale-aligned point.
- **Porting:** read the upstream diff, adapt it, validate it at the touched surface's risk level, and record provenance in the commit message ("ported from upstream #NNNN").
- **Dependency CVEs** stay covered by `cargo audit` and `cargo deny`.

### Deliberate behavioral divergences

| Divergence | Upstream | Status |
|---|---|---|
| `always_ask` outranks Full autonomy | returns `Approved` for Full before consulting `always_ask` (fail-open) | deliberate |
| Risk-profile `allowed_tools`: absent is not empty | maps `[]` to `None`, which is unrestricted | deliberate |
| Cron `allowed_tools = []` means deny-all | asserts the opposite in a test | deliberate |
| `ModelProvider::set_credential` with real 429 rotation | logs "cannot apply … Retrying with original key" | deliberate |
| WeChat state persistence is atomic and non-blocking | blocking, non-atomic write | deliberate |
| Tachi memory backend behind the `tachi` feature | absent | deliberate |
| HyperMemory custom CRUD backend | absent | retired; never re-add |

### Known gaps

- **MCP `__` auto-admit is opt-in.** The default `mcp_discovered_tool_policy` is `explicit_only`. Setting `auto_admit` on a risk profile admits any unlisted `<server>__<tool>`, so keep write tools off the MCP servers such a profile connects to.
- **Approvals.** Local grants persist in `data_dir/approvals.db` with an `approval_audit` trail. Node grants and receipts are not built yet (#62).

## Phone candidate-history pages

The #379 history leaf extends the existing operator-only
`GET /api/user-model/candidates` route. Its sqlite User Model store remains the
single history owner. Legacy requests, including `pending=true`, keep the
existing `{ "candidates": [...] }` response with all matching history.

Opt in with `?limit=50` (accepted range: 1 through 200). Paged responses add
`next_cursor`, either `null` at the end or `{ "created_at_unix": 123, "id": "..." }`.
For the next page, pass both returned values as `before_created_at` and
URL-encoded `before_id`, retaining the same `pending` filter and `limit`.
Cursors without a limit, partial cursors, invalid limits and out-of-range
sqlite timestamps are rejected after operator authentication and before the
store opens. Bridge and Node tokens do not gain owner access.

Ordering is newest first by `(created_at_unix, id)`, including ties. Each
request selects at most `limit + 1` IDs before fetching their evidence; the extra
row determines whether another page exists. This bounds evidence materialization
without an index or schema migration. Metadata selection still scans and sorts
matching history; a page is not a database snapshot across requests. Candidates
inserted newer than the cursor appear on refresh, and pending rows reviewed
between requests disappear from subsequent pending pages.

The phone view requests 50 candidates at a time, appends unique IDs after an
explicit load, and keeps its existing rows and cursor when a page fails. Refresh,
Agent/tab changes, owner authorization failure and successful review mutations
reset the loaded view. Old in-flight reads cannot repopulate the reset view.
Gateways that ignore the new query and omit `next_cursor` produce an upgrade
message. Upgrade the gateway before serving the new web bundle; a rollback to
the older bundle keeps its legacy route contract. No data migration is needed.

### Upstream comparison for this leaf

Source inventory: upstream `zeroclaw-labs/zeroclaw` at
`329e66f76394b00af9d00b767e00ad298133961e`; fork baseline
`36b362f509a4dde72e686b4aca6b640b5537b495`.

| Source | Existing local equivalent | Decision and remaining gap |
|---|---|---|
| #9353, merged `1c7ef505b29ca28d7bcec3594c8c5c69c063c159` | Alias-scoped hydration and bounded browser transcript cache | Defer port: independent conversations, picker and per-conversation draft lifecycle remain a separate leaf. |
| #9355, merged `e7548768e0d21411008f130f324cfa06b10be4a1` | Alias-keyed `openChats` workspace | Defer port: upstream per-pane keys, session selection and sibling collision guards are not equivalent to this workspace. |
| #10544, merged `e5d556a17d4633218aa70f82ed8aab58e821a37d` | Existing authenticated HTTP attachment handles for shared intake | Defer port: the web composer has no image attachment control; preserve the fork's handle contract when adapting it. |

The pinned upstream tree has no `api_user_model.rs`, `Review.tsx` or `review.ts`
equivalent. This pagination leaf extends the fork's existing owner-review
surface and does not port the unrelated upstream lifecycle or upload changes.
It adds no endpoint, configuration, store or dependency, and retires no route.
Phone pairing/chat/attachment and PWA installability acceptance remain open.

## Hyperion (dormant)

Hyperion, the owner's quantitative-trading system, is not an active consumer of this fork (#267 to #269 are closed). If that work resumes, these rules still apply:

1. Memory goes through typed hapi-edge facade actions only. Never connect to a memory backend directly, never touch `data/hapi.db`, and never re-add HyperMemory.
2. Trading tools go through the hapi-edge MCP server only, never direct broker or data-vendor calls.
3. Real position writes require human OTP confirmation.
4. Timezone `Asia/Shanghai`; A-share lot 100 shares (STAR: minimum 200, then 1-share increments).

The companion memcore stores (#49) are a separate, feature-gated, in-process exception. They never share a live database with, or connect to, a network memory service.

## Yonbo device host import

The optional [Yonbo host](../../../../crates/zeroclaw-yonbo-node/README.md)
imports the independently exercised thin-device prototype as a pure-std crate.
It defaults to no listener and no display execution. Its PSK maintenance protocol
is not Gateway Node v2; it does not replace DeviceIdentity, policy ceilings,
approvals or the missing body-to-device invocation lifecycle in tracker #61.
The crate owns only its explicitly configured device-local credential/sequence
store and runtime files. It is not another Agent, task store or controller.

Upstream was checked at `5d4b18d335780af8bf15979b2f45e541f91c5eb9`, against fork
baseline `fa986b5eb62274e04e254692bfa8f8aeb45d4f00`:

| Source | Local equivalent and decision |
| --- | --- |
| Upstream `gateway/src/nodes.rs` and `node_tool.rs` | Skip the older registration/invocation route. This fork keeps v2 signed identity and admission; importing an `approved` argument shortcut would not provide its required authority. |
| Upstream `zeroclaw-tls` and `zeroclaw-relay-proto` | Defer transport integration to the v2 leaf. They do not provide the bounded display host and would change this pure-std device footprint. |
| Fork `gateway/src/nodes.rs`, `api/src/node.rs` | Retain unchanged. The live socket handles identity and capability advertisement, but this import adds no production Invoke/result-completer connection to it. |

Only the device prototype is adapted here. A physical consumer now exists, which
makes this bounded source-import leaf useful; it does not close #61 or activate
high-risk device authority. Boot installers and cross-build/deployment acceptance
remain separate. No existing controller dependency or default capability is added.

## Restricted device chat admission

The #495 leaf under #377 adds an optional `gateway.bridges.<name>.device`
policy to the existing shared chat route. Its configuration is the authority
owner; the existing session backend and Conversation remain the history and
turn owners. See [restricted device bridges](../gateway/api.md#restricted-device-bridges)
for bindings, per-operation grants, revocation and downgrade discipline.
Chat authority never grants owner approval or Operator learning provenance.

Upstream `zeroclaw-labs/zeroclaw` was inspected at
`5d4b18d335780af8bf15979b2f45e541f91c5eb9`, against fork baseline
`79a8320132f5866b3846ca561eaaf054241d37b5`. Upstream `src/gateway/ws.rs` and
`src/gateway/ws_approval.rs` contain chat/approval transport, but no equivalent
restricted bridge policy. This fork already owns shared Conversations, bridge
credentials and session scopes, so this leaf extends those owners rather than
porting the upstream session implementation. No existing route or consumer is
retired. Turn streaming/recovery, adapter integration and Node invocation remain
separate slices; #377 and #380 remain open.

## Non-Telegram turn control

The #497 leaf adds opt-in request/turn correlation to the existing shared
Conversation, following restricted bridge authority in #495. It does not borrow
Telegram source identifiers, replace the session ledger, or add another Agent.

Upstream comparison: `zeroclaw-labs/zeroclaw` at
`5d4b18d335780af8bf15979b2f45e541f91c5eb9`,
`crates/zeroclaw-gateway/src/ws.rs`; fork baseline `eaf007c95`.
The upstream socket lifecycle and cancel-token helpers are not equivalent to
this fork's shared Conversation and durable request receipts. Retain the local
owners and extend their transport boundary; do not port a second runtime.
The extension's bounded in-process replay cache is owned by ConversationHub.
It is not a durable input queue or an execution-retry promise. Missing recovery
state is unknown, never permission to repeat effects.
