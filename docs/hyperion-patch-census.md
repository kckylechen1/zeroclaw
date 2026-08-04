# Hyperion fork patch census

Measured on 2026-08-04 against:

- fork `master`: `53bb5664109347fc939a09ac32f812e54e20d08a`
- upstream `master`: `4770420ab3e937ededb3c3bbf0009dc237e0f87a`
- merge-base: `f3023663a08f668dcec60c8d6d6db7777c86955a`
- upstream-only commits: **104**; fork-only commits: **71**; relation: **diverged**.

This is the fork of a **product line** (the Hyperion Personal Assistant), not a
passive downstream mirror. The goal is therefore not to minimise divergence.
A patch earns its place by being useful here; a patch that upstream has genuinely
absorbed is dropped, not carried out of habit. The four verdicts used below:

- **carry** — the fork still needs this and upstream has not absorbed it.
- **absorbed upstream** — upstream now does this; the fork's copy is candidate
  for removal on the next intake (after a parity audit proves equivalence).
- **observe** — upstream has an open PR for this; track it, do not pre-empt.
- **skip** — not part of the PA product surface; do not ingest.

Every "upstream today" cell below was read out of `upstream/master` at the SHA
above, not inferred from changelogs. Re-run those checks on the next intake — a
patch that upstream absorbs should be dropped, not carried out of habit.

This census supersedes the 2026-07-26 draft, which described the retired
`hyperion-integration` branch (33 commits, deleted 2026-08-04). That branch's
core work — the Tachi memory backend — landed on `master` via PR #27 and is
fuller here (it adds `tachi_governance`, `tachi_scenarios`, `tachi_enrichment`).

---

## Fork-only commits (71) — thematic grouping

The 71 commits the fork is ahead of merge-base group into six themes. This is
the carry set that any upstream intake must preserve or consciously supersede.

| Theme | Commits | Role |
|---|---|---|
| Coordinator / control-plane / channels | 42 | the hyperion-integration-v2 line: detached subagents, announce chain, agent cards, persona dials, concurrency backstop |
| Memory / Tachi backend | 14 | feature-gated `tachi` backend + governance + LLM enrichment |
| Approval / security hardening | 4 | durable one-shot grants, MCP auto-admit made opt-in |
| Providers | 2 | `set_credential` + apply-rotated-key-on-429 |
| Config / `allowed_tools` adaptation | 5 | `Option<Vec<String>>` propagation after upstream Option-ified the field |
| Docs / census / build profile | 4 | this file, AGENTS.md Hyperion contract, dev-profile tuning |

The coordinator/control-plane line (42 commits) is **fork product surface, not
a patch on upstream** — upstream has no equivalent. It is the PA's subagent
runtime. It is not a candidate for "push upstream"; it is the PA's reason to
exist as a fork.

---

## Carried patches — verified against fork master `53bb56641`

Each row was verified by reading the code on fork master, not by trusting the
prior census. File:line citations are from the fork master tree.

### 1. `always_ask` outranks Full autonomy — **carry**

`ApprovalManager::approval_requirement` checks `always_ask` first, then Full:
under `AutonomyLevel::Full`, an exact or wildcard `always_ask` match returns
`Prompt`, otherwise `Approved`
(`crates/zeroclaw-runtime/src/approval/mod.rs:191-202`). Pinned by
`unattended_full_autonomy_still_honors_always_ask` (`mod.rs:561-571`) and
`full_autonomy_prompts_for_exact_always_ask_tool` (`mod.rs:437-445`). Consumed
at `agent/turn/approval_gate.rs:28-31`.

**Upstream today:** returns `Approved` for Full before consulting `always_ask`
— fail-open. Leaf #37 tracks the upstream contribution to fix this.

### 2. risk-profile `allowed_tools`: absent ≠ empty — **carry**

No `[]`→`None` collapse anywhere. `RiskProfileConfig.allowed_tools` is
`Option<Vec<String>>` with `#[serde(default)]`
(`crates/zeroclaw-config/src/schema.rs:11879`). `SecurityPolicy` field doc
states "`Some(vec![])` denies every tool"
(`crates/zeroclaw-config/src/policy.rs:187-190`). `from_profiles` passes
through with `.clone()` (`policy.rs:2131, 2186`); enforcement
`is_tool_allowed` uses `is_none_or` — `None` unrestricted, `Some([])` deny-all
(`policy.rs:221-223`). Test: `from_profiles_absent_allowed_tools_means_unrestricted`
(`policy.rs:2688`).

**Upstream today:** `SecurityPolicy::from_profiles()` collapses an empty `Vec`
to `None` → unrestricted. Leaf #39 tracks the upstream contribution.

### 3. cron `allowed_tools = []` means deny-all — **carry, deliberately conflicting**

`cron_add.rs:470-478` stores `Some(v)` for an explicit list (`// [] = deny-all;
omit field for unset/default`); test `empty_allowed_tools_stored_as_deny_all`
asserts `Some(vec![])` (`cron_add.rs:1110-1132`). Enforcement:
`cron_agent_run_security_policy` injects default scheduler exclusions only when
`job.allowed_tools.is_none()` (`cron/scheduler.rs:523-537`).

**Upstream today:** ships `empty_allowed_tools_stored_as_none`, a test asserting
the opposite intent. The fork's rename to `..._stored_as_deny_all` is
deliberate and will re-conflict on every rebase — loudly, which is the point.

### 4. `ModelProvider::set_credential` + real 429 rotation — **carry (evolved)**

Trait method `set_credential(&self, Option<String>) -> bool` defaulting to
`false` (`crates/zeroclaw-api/src/model_provider.rs:444`, Box forward `:716`);
overrides in `compatible.rs:2456`, `model_pin.rs:81-82` (forwards so a pin does
not swallow rotation), `reliable.rs:5438`. Rotation is **real, not log-only**:
`rotate_and_apply_key` (`reliable.rs:898`) parks the spent key and calls
`entry.provider().set_credential(Some(new_key))` (`reliable.rs:934`), invoked
from the 429 retry path (`reliable.rs:1233`).

> **Status corrected from the prior census.** The 2026-07-26 draft and the
> `AGENTS.md` carried-patches table said this "logs 'cannot apply … Retrying
> with original key' in 4 places." Commit `05a9ef31e` replaced that with actual
> application — the rotated key is now applied, not merely logged. The
> `AGENTS.md` row is stale on this point.

**Upstream today:** logs "cannot apply … Retrying with original key" — does
not apply the rotated key. Leaf #34 (B2 parity audit) is the intake gate; do
not replace the fork implementation merely because upstream has a newer PR
(#9419, open) — prove stronger semantics first.

### 5. WeChat atomic / non-blocking state persistence — **not ported**

`write_private` still does a blocking, non-atomic `std::fs::write` and chmods
*after* writing (`crates/zeroclaw-channels/src/wechat.rs:504-512`), so the file
holding the WeChat token is briefly readable at the default umask.
`save_account_data` is still a sync `fn` (`wechat.rs:860-886`) and ripples to
its callers if changed.

**Upstream today:** same blocking, non-atomic write. Neither side has fixed
this. Tracked under Workstream B3 (WeChat), upstream open PR zeroclaw-labs#9313.

### 6. Tachi memory backend — **carry (feature-gated)**

Optional git dep `memcore` rev `7ae2c0a0` + feature `tachi = ["dep:memcore"]`
(`crates/zeroclaw-memory/Cargo.toml:28,32`); workspace exposes
`memory-tachi = ["zeroclaw-memory/tachi"]` (`Cargo.toml:379`). Four modules
gated behind `#[cfg(feature = "tachi")]`: `tachi`, `tachi_enrichment`,
`tachi_governance`, `tachi_scenarios` (`crates/zeroclaw-memory/src/lib.rs`).
Costs nothing when the feature is off.

This is the fork's own agent-memory backend, **distinct from the Hyperion
trading memory path** (which routes through hapi memory-server MCP per the
#634 option C contract). It is not a candidate for upstream contribution —
it depends on a private crate (tachi).

### 7. HyperMemory custom CRUD backend — **retired (#634 option C)**

Confirmed absent. Zero matches for `hypermemory` / `hyperion-memory` in any
`.rs`, `.toml`, or `.md` outside the `AGENTS.md` retirement notes. No such
variant in `MemoryBackendKind`. Do not reintroduce.

### Additional fork-only hardening (not in the old carried-patches table)

These landed on the v2 line and are also carry-classified:

- **Durable one-shot approval grants** — `approval/store.rs` binds a grant to
  `(boot_id, run_id, tool_name, args_hash)` and redeems via a single
  conditional `UPDATE ... RETURNING`, so two racing calls cannot both redeem
  one grant (`8c25ef0b0`, `af9038028`). Upstream has neither.
- **MCP tool auto-admit made opt-in** — `... || name.contains("__")` auto-admit
  in `tools/delegate.rs` / `tools/tool_search.rs` now consults
  `risk_profile.mcp_discovered_tool_policy`, defaulting to `explicit_only`
  (`8c25ef0b0`). Upstream still auto-admits.

---

## Upstream intake candidates (104 upstream-only commits)

These are the commits upstream has that the fork does not. Intake is gated by
issue #31: not a whole-tree merge, not a reset-and-replay. Each candidate is
classified into a wave and must preserve every fork invariant.

### Wave A — MCP / Provider transport reliability

| Upstream SHA | Subject | Verdict | Note |
|---|---|---|---|
| `1e99541d1` | fix(mcp): multiplex stdio calls without replaying unknown outcomes (#9418) | **adopt** | 8 B1 invariants all adopted upstream, verified with file:line evidence in leaf #32. Fork's PR #13 (closed) targeted the same intent. |
| `4770420ab` | fix(providers): harden SSE completion and idle timeouts (#8838) | **review** | touches `anthropic.rs`, `compatible.rs`, `openai.rs`. Must not regress the fork's 429 rotation (#4 above). |
| `404c3e48d` | fix(providers): propagate Responses usage (#9360) | **review** | touches `openai.rs`, `openai_codex.rs`. |
| `53129fc28` | fix(providers): preserve native tool pairs on context retry (#9372) | **review** | touches `multimodal.rs`, `reliable.rs` (+301/-29) — overlaps the fork's `reliable.rs` rotation work; merge carefully. |

### Wave B — Sandbox / config consistency / security

| Upstream SHA | Subject | Verdict | Note |
|---|---|---|---|
| `841f28c7f` | fix(gateway): serialize config writes (#9519) | **review** | gateway config write serialization; the fork does not currently carry this. |
| `f79c13d2f` | fix(security): preserve shell cwd through Seatbelt (#9401) | **review** | macOS Seatbelt cwd preservation. |
| `0b8d9cbd7` | fix(runtime/security): allow various devices and files on landlock sandbox (#9114) | **review** | titled "allow various devices and files" (permissive-sounding) but is the Landlock change #31 Wave B names as fail-closed; verify the actual behavior before quoting the title. |

### Wave C — deployed channels only

Track only channels the Hyperion PA actually deploys. Upstream open PRs:
- zeroclaw-labs#9313 (WeChat sync-cursor-after-enqueue) — **observe**
- zeroclaw-labs#9314 (Telegram typed disposition) — **observe**
- Discord — no upstream home; fork-only residual (leaf #33), deferred unless deployed.

### Wave D — PA / Persona / Edge

Deferred until Waves A–C land. Default-skip categories from #31 (do not ingest
without a concrete PA requirement): Zerocode/TUI visual, web dashboard,
unused channels, hardware/GPIO, WASM channel/plugin, installer/packaging,
SOP fan-in, session-scoped task-plan (duplicates Tachi), dormant shared-budget.

### #37 / #38 / #39 — upstream contribution leaves (special mapping)

These three fork leaves target upstream directly (clean branches off
`4770420ab`, no fork-master basing). Once their fork→upstream PRs are open:

- upstream **not merged** → keep the fork's implementation (patches #1–3 above).
- upstream **later merges** → compare semantics, delete the redundant fork patch.
- upstream **merges a different implementation** → parity audit (same method as
  the #13 vs `1e99541d` audit in leaf #32).

---

## Open gaps — neither upstream nor this branch

1. **WeChat state persistence** — blocking, non-atomic `std::fs::write` +
   post-write chmod (#5 above). The fix changes `save_account_data` from a sync
   `fn` and ripples to callers; it is its own change, not a rebase carry.
2. **Tool receipts durability** — the approval path is durable, but upstream's
   *tool receipts* are HMAC'd with an in-memory key, cover only successful
   calls, and are gone on restart. Wiring receipts into the durable trail is
   the obvious next step and is not done.
3. **`ChildResult` telemetry is computed then discarded** — `tool_calls` /
   token counts / `child_session_id` / `worktree_path`. Per owner decision
   (#28), not persisted. Note `tool_calls: 0`, `turns: 1`, `worktree_path: None`
   are hardcoded constants, not measurements — no production code reads them
   today, but surface them honestly before exposing `ChildCompletionSummary`.

---

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

### Unblock

tachi PR [#1453](https://github.com/kckylechen1/tachi/pull/1453) widens that
manifest to `rusqlite = ">=0.37, <0.39"`. Standalone tachi still resolves to
0.38, so nothing changes there; a consumer holding 0.37 unifies instead of
failing.

**When #1453 merges:** point `memcore` at the merged `main` rev and delete the
carried compatibility commit for good. Then verify the schema **v23** migration
against real data before trusting it — the existing `.tachi/` store predates it,
and that check must not run during market hours.

---

## CI note (not a patch, but it gates intake validation)

Fork `master` CI is red on every code job. Root cause: `zeroclaw-memory`
depends on the private `tachi` git repo, and the CI `GITHUB_TOKEN` cannot read
it — dependency resolution dies before any compile. Tracked by issue #28. This
gates the validation step of every intake wave; it must be unblocked (PAT,
open-source, or local memcore convergence per #16) before intake PRs can claim
CI-green evidence.
