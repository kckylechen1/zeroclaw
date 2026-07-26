# AGENTS.md - ZeroClaw

Core instructions for AI coding assistants working in this repository. Use `docs/book/src/contributing/architecture-map.md` to load only the references needed for a non-trivial task.

## Single Source Of Truth

Do not duplicate state. Before adding a struct field, config entry, schema field, runtime cache, or parallel lookup table, identify the canonical source:

1. If the new field creates the fact, state that explicitly.
2. If the fact already exists, resolve it from that source at use time.

Prefer borrowed config, getters, resolver closures over live config, on-demand materialized views, or generated surfaces from one input. Do not snapshot live policy into long-lived handles. A restart-only snapshot is not a substitute for resolving canonical state.

## Safety And Privacy

- Never commit secrets, tokens, credentials, personal data, or real identities.
- Do not weaken permissions, allowlists, sandboxing, approvals, or other trust boundaries without making the behavior and risk explicit.
- New external surfaces default closed. Prefer allowlists to blocklists.
- Do not hide behavior changes inside refactors or bypass failing checks.
- Production paths must propagate errors. Avoid `unwrap()` and `expect()` unless a documented invariant makes panic impossible.
- Do not suppress unused production code with underscore names or `#[allow(dead_code)]`; remove it, connect it, or track it. Underscore names remain valid for required but intentionally unused API, trait, or callback parameters.

## Working Rules

1. Read the owning module, factory wiring, adjacent tests, and relevant docs before editing.
2. For architecture, config, security, workflow, governance, CI, release, or agent-assisted changes, start with `docs/book/src/contributing/architecture-map.md`.
3. Name the source of truth before introducing state.
4. Keep one concern per PR. Avoid unrelated cleanup and do not mix broad formatting changes with functional changes.
5. Do not add heavy dependencies for minor convenience, speculative abstractions, or config keys and feature flags without a concrete use case.
6. Add the smallest useful implementation and tests at the real behavior boundary.
7. Validate at the change's risk level, report commands actually run, and document behavior, risk, side effects, and rollback.
8. Use a non-`master` branch, open a PR to `master`, and never push directly to `master`.
9. Use conventional commits and the full PR template. Prefer small PRs and do not add bot or AI attribution footers.
10. Declare stacked work with `Depends on #...` and replacement work with `Supersedes #...`.

Subagents must set their working directory to the repository root before shell or filesystem work. Do not assume an inherited working directory.

## User-Facing Text

- User-facing runtime CLI, tool, and onboarding text uses Fluent `fl!()` keys rather than bare literals.
- Zerocode uses its independent Fluent catalogue through its documented `crate::i18n` helpers. Web dashboard text follows the TypeScript `web/src/lib/i18n.ts` contract, not Rust `fl!()`.
- Logs, tracing fields, and panic text remain English and use stable error keys where the logging contract requires them.
- English Markdown is the documentation source of truth. Follow the documented localization workflow instead of editing generated translations by hand.

## Validation

Choose checks that match the changed surface. Common code checks are:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Use `./dev/ci.sh all` for full pre-PR validation when the scope warrants it. Docs-only changes use `scripts/ci/docs_quality_gate.sh` and `scripts/ci/docs_links_gate.sh`. Bootstrap script changes add `bash -n install.sh`.

## Task References

The architecture map routes task-specific documentation. Consult `docs/book/src/contributing/agent-guidelines.md` only for detailed agent examples, risk and stability policy, skill discovery, and protected operational documents. Do not skip a required contract because it is no longer embedded in this bootstrap file.

---

## Hyperion Integration Context

This repo is the primary agent harness for the **Hyperion** quantitative trading
system (`~/Projects/Quant_Analyzer_2026`), replacing Python Hermes-Agent.

### Architecture

```
ZeroClaw (this repo)
  └── WeChat iLink / WeCom gateway
  └── LLM agent loop
  └── MCP client → hapi-edge (Go)
  │                   └── snapshot / batch_snapshot / history_klines / portfolio_*
  └── MCP client → hapi memory-server (HTTP :6888)
                      └── hapi_save / hapi_search / hapi_memory
```

### Carried patches

This is a fork. Rebasing onto upstream is how we pull their work in — it is not
a reason to keep our own surface small. Every row below states what upstream does
today so each rebase can re-test whether a patch has been absorbed; a patch that
is still ours simply stays.

| Patch | Upstream today | Status |
|---|---|---|
| `always_ask` outranks Full autonomy | `approval/mod.rs` returns `Approved` for Full **before** consulting `always_ask` — fail-open | ✅ carried |
| risk-profile `allowed_tools`: absent ≠ empty | maps `[]` → `None` → unrestricted | ✅ carried |
| cron `allowed_tools = []` means deny-all | ships a test asserting the opposite (`empty_allowed_tools_stored_as_none`) | ✅ carried, deliberately conflicts |
| `ModelProvider::set_credential` + real 429 rotation | logs "cannot apply … Retrying with original key" in 4 places | ✅ re-derived on upstream tip |
| WeChat atomic / non-blocking state persistence | `write_private` still does blocking `std::fs::write`, non-atomic truncate, chmod after write | ⬜ not ported — `save_account_data` is sync `fn`; porting changes its signature and ripples to callers |
| Tachi memory backend | absent | ✅ carried, feature-gated behind `tachi` — the fork's own agent-memory backend, distinct from the Hyperion trading memory path |
| HyperMemory custom CRUD backend | absent | ❌ retired (#634 option C) — never re-add |

### Known gaps neither side has fixed

- **MCP tools bypass `allowed_tools`.** Any tool name containing `__` is
  auto-admitted even under a non-empty allow-list (`tools/delegate.rs`,
  `tools/tool_search.rs`, `tools/mcp_deferred.rs`, `config/helpers.rs`). The
  first line of defence stays server-side: expose no trading write tools on the
  hapi-edge profile the agent connects to.
- **Approvals are memory-only.** No one-shot approval bound to a run + tool +
  args hash, and no durable audit trail that survives a restart.

### Memory Contract (#634 option C)

The custom HyperMemory CRUD backend was protocol-mismatched with the live
memory-server (wrong transport AND wrong tool API) and has been retired. Do NOT
reintroduce a `hypermemory` memory backend.

- Consume the memory-server as a standard `[[mcp.servers]]` entry via the native MCP client.
- MCP endpoint: `http://127.0.0.1:6888/mcp` (or `HAPI_MEMORY_MCP_URL`). ZeroClaw's memory MUST route through :6888 only — never through hapi-edge's HTTP serve on :8890. That serve path is currently disabled/crash-looping; note :8890 is still hapi-edge serve's default HTTP port (`HAPI_EDGE_PORT`) used elsewhere in Quant (Crimson UI proxy, local HTTP MCP surface), so this is a ZeroClaw-memory-dependency rule, not a claim that :8890 is globally dead.
- Tools: `hapi_save`, `hapi_search`, `hapi_memory` (NOT `save_memory`/`list_memories` — those never existed server-side).
- Namespace: `hyperion` / project `hyperion` / domain `equity_trading`
- Path prefix: `/trading/equity/...`
- **Route through hapi-edge / hapi memory-server MCP only — never host tachi MCP directly, never write Tachi DBs directly**

### Key Rules

1. Memory → hapi memory-server MCP only, never `data/hapi.db` directly
2. Trading tools → hapi-edge MCP only, never direct Longbridge/Tushare calls
3. Real position writes require human OTP confirmation (Trading Harness P1)
4. Timezone: `Asia/Shanghai`
5. A-share lot: 100 shares (STAR: min 200 then 1-share increments)
