# Hyperion fork patch census

Base: `upstream/master` @ `f3023663a`. Old fork: `hyperion-integration` @ `6b1d11867`
(33 commits ahead of merge-base `efe1eb5b2`, 154 behind upstream).

Every "upstream today" cell was read out of `upstream/master` at `f3023663a`, not
inferred from changelogs. Re-run those checks on the next rebase — a patch that
upstream absorbs should be dropped, not carried out of habit.

**This is a fork, not a downstream.** Rebasing exists to pull upstream's work in,
not to keep our divergence small. Divergence is not a cost to be minimised: a
patch earns its place by being useful here, and nothing needs to justify itself
against "could this go upstream instead". The verdicts below drop commits that
upstream has genuinely absorbed or that were pure noise — not commits that are
merely ours.

## Verdicts

| Old commit | Verdict | Why |
|---|---|---|
| `deb379d78` approval: always_ask under Full | **carry** | `approval/mod.rs:177` still returns `Approved` for Full autonomy before it looks at `always_ask` |
| `6a907792c` config: absent vs empty allowed_tools | **carry** | upstream adopted `Option<Vec<String>>` but `policy.rs:2182` collapses `[]` → `None` → unrestricted |
| `39b1683da` propagate Option allowed_tools | **carry** | follows from the above |
| `86d1e650d` wrap test literals in Some | **carry** | follows from the above |
| `273d70f58` cron: empty allowed_tools is deny-all | **carry, conflicting** | upstream ships `empty_allowed_tools_stored_as_none`, a test asserting the opposite intent. This patch renames it to `..._stored_as_deny_all`. Expect this to re-break on every rebase — loudly, which is the point |
| `5470141f4` 429 rotation hot-swap | **re-derived** | see below |
| `19e8d614d` skip rate-limited keys | **re-derived** | see below |
| `c5fc1dfe6` skip provider cooldown after rotation | **re-derived** | see below |
| `c9398657d` fail closed on all-cooling rotation | **re-derived** | see below |
| `97686295b` set_credential via entry | **re-derived** | see below |
| `3cca4e443` forward through ModelPinnedProvider | **re-derived** | see below |
| `418c12c1c` drop conflict marker | **drop** | noise from the previous port |
| `4aae590e9` restore fmt/clippy gates | **drop** | noise from the previous port |
| `ce5307f66` rustfmt | **drop** | noise from the previous port |
| `518fe442e` WeChat non-blocking persistence | **deferred** | still unfixed upstream — see "Open" below |
| `17d49caaf` WeChat atomic persistence | **deferred** | same |
| `e14cfef98` `c7025ff85` `aee449ea1` gitleaks/CI | **drop** | these fixed the *old* fork tip's CI. The gitleaks hook runs clean on this branch; re-derive only if it fires |
| `68e40a53b` AGENTS.md Hyperion contract | **carry, rewritten** | upstream cut AGENTS.md from 278 to 59 lines. Taking upstream's file and appending the Hyperion section is the correct resolution; replaying the old diff would have reverted upstream's trim |
| 13 × `tachi*` memory commits | **carry** | rebased onto this branch. Feature-gated behind `tachi`, so it costs nothing when off. It is not the Hyperion *trading* memory path — that still routes through `hapi-memory` MCP on :6888 — but it is the fork's own agent-memory backend and belongs on the main line |

33 old commits in; 23 commits on this branch. The nine rotation commits collapse
into two re-derived ones. Six are genuinely dropped — three fmt/conflict-marker
cleanups from the previous port, and three gitleaks/CI commits that were fixing
the old fork tip's CI rather than anything real. Two (WeChat) are deferred for
the reason below.

## The rotation patch, re-derived rather than replayed

Replaying the nine rotation commits would have meant hand-resolving conflicts
against upstream's 806-line rewrite of `reliable.rs` and its move of
`OpenAiCompatibleModelProvider` construction to a builder. The net semantics
were re-implemented on the new code instead:

- `ModelProvider::set_credential(&self, Option<String>) -> bool`, defaulting to
  `false`. The boolean is the point — it lets a provider that cannot rotate say
  so instead of letting the caller log a rotation that did not happen.
- `OpenAiCompatibleModelProvider.credential` behind `Arc<RwLock<_>>`.
- `ModelPinnedProvider` forwards, so a pin does not swallow rotation.
- Per-key cooldowns, so round-robin cannot return the key that just 429'd.
- Rotation returns `None` when every key is cooling, and says so.
- A successful swap suppresses the provider-wide cooldown.

Two things from the original were deliberately dropped:

- `as_any()` on the trait. It was never called; the `'static` bound on
  `ModelProvider` existed only to support it. Both are gone.
- The hand-written 24-field `Clone` impl. `Arc` sharing keeps `#[derive(Clone)]`
  working *and* makes clones observe later rotations, which a deep copy would
  silently miss.

Scope limit worth knowing: only the OpenAI-compatible provider implements
`set_credential`. Anthropic, OpenAI, OpenRouter and Azure providers keep their
own `credential: Option<String>` fields and inherit the `false` default, so a
429 on those logs "does not support runtime credential rotation" — honest, but
still not rotating.

## Open — neither upstream nor this branch

1. **MCP tools bypass `allowed_tools`.** Any tool name containing `__` is
   auto-admitted even under a non-empty allow-list — `config/helpers.rs:633`,
   `tools/delegate.rs:942`, `tools/tool_search.rs:62`,
   `tools/mcp_deferred.rs:181`, identical on both sides. First line of defence
   stays server-side: expose no trading write tools on the hapi-edge profile
   the agent connects to.
2. **One-shot durable approval.** Approvals and the session allow-list are
   memory-only. Nothing binds an approval to a run + tool + args hash, and
   nothing expires it on restart.
3. **Durable audit outbox.** Upstream's tool receipts are HMAC'd with an
   in-memory key, cover only successful calls, and do not survive a restart.
4. **Non-blocking `spawn_subagent` + announce.** Still synchronous; the parent
   turn blocks until children finish. `delegate(background=true)` routes around
   part of this but has no progress or completion-announce path.
5. **WeChat state persistence.** `write_private` still does a blocking,
   non-atomic `std::fs::write` and chmods *after* writing, so the file holding
   the WeChat token is briefly readable at the default umask. Not ported here
   because the fix changes `save_account_data` from a sync `fn` and ripples to
   its callers; that is its own change, not a rebase carry.

## The memcore pin

`crates/zeroclaw-memory/Cargo.toml` pins `memcore` to tachi rev `7ae2c0a0`.
That rev is **diverged from tachi main, not behind it**: it is tachi's
`dd1dc14a` plus one commit that was never merged there —
`chore(memcore): align rusqlite to 0.37 for ZeroClaw workspace co-existence`.
Bumping the pin naively drops that commit and the build stops resolving.

Tachi main is 468 commits ahead, ≥100 of them touching `crates/memcore`
(schema v23 migration, authorizer/guard boundaries, trigger inventory,
evidence guards). Its `memcore` version string is still `1.9.0`, so the
version number carries no signal — compare revs, not versions.

### Why the pin is stuck

```
memcore @ tachi main   → rusqlite 0.38 → libsqlite3-sys 0.36
matrix-sdk-sqlite 0.18 → rusqlite 0.37 → libsqlite3-sys 0.35
```

Both link the native `sqlite3` library, and cargo permits exactly one package
with a given `links` value. `matrix-sdk-sqlite 0.18` is the latest release on
crates.io with no 0.38-based successor, and `channel-matrix` ships in
`dist_extra_features` and `ci-all` — so ZeroClaw cannot simply move to 0.38
without dropping the Matrix channel from distribution builds.

### Measured, not assumed

- The memcore API surface this fork uses is eight symbols — `MemoryStore`,
  `MemoryEntry`, `SearchOptions`, `HybridWeights`, `AnchorKind`,
  `NEAR_DUP_RAW_SCAN_CAP`, `near_duplicate_raw_pairs`, `types::default_tier`.
  All eight still resolve at tachi main. `default_tier` moved into
  `types/entry.rs`, but `types.rs` re-exports with `pub use entry::*`.
- memcore at tachi main **compiles against rusqlite 0.37**.
- With the requirement widened, a full workspace check with `tachi` *and*
  `channel-matrix` both enabled resolves to a single `libsqlite3-sys` and
  builds clean.

So the blocker was never the API and never the code — only the exact-version
pin in tachi's workspace manifest.

### Unblock

tachi PR [#1453](https://github.com/kckylechen1/tachi/pull/1453) widens that
manifest to `rusqlite = ">=0.37, <0.39"`. Standalone tachi still resolves to
0.38, so nothing changes there; a consumer holding 0.37 unifies instead of
failing.

**When #1453 merges:** point `memcore` at the merged `main` rev and delete the
carried compatibility commit for good. Then verify the schema **v23** migration
against real data before trusting it — the existing `.tachi/` store predates it,
and that check must not run during market hours.
