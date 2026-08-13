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

## Where New Types Go

Measured, not stylistic: `crates/zeroclaw-config/src/schema.rs` is ~37k lines
holding 252 config structs and ~390 derive macros. One debug rlib of that crate
is **254 MB**, and it sits on `agent-runtime`'s mandatory path — so every
consumer pays for every subsystem's config whether or not the feature is on.
Upstream's own manifest states the cause: "All channel schema types compile
unconditionally."

The release binary is fine — 21 MB, because LTO discards what is never
instantiated. The cost lands on build time and disk, and it compounds: turning
off 30 of 36 channels changes the dependency graph by 3.5%.

So the rule for anything new:

1. **Shared wire/domain types go in `zeroclaw-api`.** It is already the types
   crate — 7.9k lines, deps limited to serde, tokio and small utilities. A type
   two crates both name belongs here, not in whichever crate happened to define
   it first.
2. **Logic stays where it runs.** `zeroclaw-api` holds the shape; the crate
   that owns the behaviour holds the behaviour. A consumer that only needs to
   *read* a result must not have to link the runtime that produces it.
3. **New config sections get their own module** under `zeroclaw-config/src/`,
   not another block in `schema.rs`. See `persona.rs` and `card.rs`.
4. **Do not restructure `schema.rs` to fix this.** It grows ~6k lines a month
   upstream; a fork that reorganises it re-fights that churn every rebase, and
   measurement says the ceiling on the win is small — removing an entire derive
   (`JsonSchema`) cuts the rlib by 16%.

The rule is "new things follow the new shape", not "go fix the old shape".
New files rebase for free; edits inside upstream's hot files are charged at the
rate upstream churns them.

## Hyperion Integration Context

This repo is the primary agent harness for the **Hyperion** quantitative trading
system (`~/Projects/Quant_Analyzer_2026`), replacing Python Hermes-Agent.

### Architecture

```
ZeroClaw (this repo)
  └── WeChat iLink / WeCom gateway
  └── LLM agent loop
  └── MCP client → hapi-edge (Go)
      ├─ trading facades: snapshot / batch_snapshot / history_klines / portfolio_*
      └─ memory facades: typed actions only (Tool Authority Catalog §1/§10)
         — NO direct connection to any memory backend (#2389 / #2432)
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
| WeChat atomic / non-blocking state persistence | `write_private` still does blocking `std::fs::write`, non-atomic truncate, chmod after write | ✅ carried — atomic tmp+chmod+rename with best-effort fsync (process-crash safe, not a power-loss guarantee); `save_account_data` is `async` via `spawn_blocking` |
| Tachi memory backend | absent | ✅ carried, feature-gated behind `tachi` — the fork's own agent-memory backend, distinct from the Hyperion trading memory path |
| HyperMemory custom CRUD backend | absent | ❌ retired (#634 option C) — never re-add |

### Known gaps neither side has fixed

- **MCP tools bypass `allowed_tools`.** Any tool name containing `__` is
  auto-admitted even under a non-empty allow-list (`tools/delegate.rs`,
  `tools/tool_search.rs`, `tools/mcp_deferred.rs`, `config/helpers.rs`). The
  first line of defence stays server-side: expose no trading write tools on the
  hapi-edge profile the agent connects to.
- **Approvals are memory-only in practice.** The durable half exists but is
  never wired in: `crates/zeroclaw-runtime/src/approval/store.rs` implements
  one-shot grants bound to boot + run + tool + args hash (300s TTL,
  single-consume redeem) plus an `approval_audit` table, and the approval gate
  already calls it — but every production constructor passes `store: None`, so
  a restart still erases all approval state and no durable audit trail is
  written. Activation and the Tachi grant/receipt contract are tracked in #58.

### Memory Contract (#634 option C; direct-leg mandate superseded by #2389 / #2432)

The custom HyperMemory CRUD backend was protocol-mismatched with the live memory backend (wrong transport AND wrong tool API) and has been retired. Do NOT reintroduce a `hypermemory` memory backend.

**SUPERSEDED (#2389 / #2432):** ZeroClaw V1 is forbidden from connecting directly to any memory backend. All supported memory access goes through typed hapi-edge facade actions governed by the **Tool Authority Catalog** (`docs/Spec/TOOL_AUTHORITY_CATALOG.md` §1 / §10, in the Hyperion-Quant-SRC repo). The earlier mandate to register the local HTTP memory port as a `[[mcp.servers]]` entry is withdrawn; do not re-add it.

- Memory facade: `hapi_memory`. Allowed and denied actions are governed by the Tool Authority Catalog (§1/§10). ZeroClaw does not depend on backend endpoints or backend-native tool names — earlier references to `hapi_save` / `hapi_search` as independent tools were the old direct-connect contract and are withdrawn.
- Namespace: `hyperion` / project `hyperion` / domain `equity_trading`
- Path prefix: `/trading/equity/...`
- **Route through hapi-edge facade actions only — never host tachi MCP directly, never write Tachi DBs directly, never connect to a memory backend directly.**

### Key Rules

1. Memory → typed hapi-edge facade actions only (Tool Authority Catalog §1/§10); never connect to a memory backend directly, never touch `data/hapi.db` directly
2. Trading tools → hapi-edge MCP only, never direct Longbridge/Tushare calls
3. Real position writes require human OTP confirmation (Trading Harness P1)
4. Timezone: `Asia/Shanghai`
5. A-share lot: 100 shares (STAR: min 200 then 1-share increments)
