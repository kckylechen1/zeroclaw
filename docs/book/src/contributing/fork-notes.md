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

## Hyperion (dormant)

Hyperion, the owner's quantitative-trading system, is not an active consumer of this fork (#267 to #269 are closed). If that work resumes, these rules still apply:

1. Memory goes through typed hapi-edge facade actions only. Never connect to a memory backend directly, never touch `data/hapi.db`, and never re-add HyperMemory.
2. Trading tools go through the hapi-edge MCP server only, never direct broker or data-vendor calls.
3. Real position writes require human OTP confirmation.
4. Timezone `Asia/Shanghai`; A-share lot 100 shares (STAR: minimum 200, then 1-share increments).

The companion memcore stores (#49) are a separate, feature-gated, in-process exception. They never share a live database with, or connect to, a network memory service.
