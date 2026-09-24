# AGENTS.md - ZeroClaw

Instructions for AI coding agents in this repository. `CLAUDE.md` points here.

## Direction

This fork turns upstream ZeroClaw into a **personal controller**: one agent with a lasting identity and memory that acts for the owner and manages authorized external agent harnesses, tools, and edge devices as one package.

- one always-on body on the owner's computer;
- edge devices (speaker, phone, robot) join through the gateway as Clients and Nodes;
- the body answers directly, reasons, or hands work to external harnesses (Codex, Claude Code) only through Tachi.

Read in this order:

1. **#388**, the direction baseline;
2. **#374**, the only execution index (follow its dispatch order);
3. the child issue for your slice.

The decisions are recorded in [ADR-013](docs/book/src/architecture/decisions/ADR-013-channels-as-gateway-clients.md) and [ADR-017](docs/book/src/architecture/decisions/ADR-017-personal-agent-body-edges-and-delegation.md).

Protected (never weaken; stop and ask if a change seems to need it):

- Soul and User Model authority (ADR-014/015/016);
- approvals and safety boundaries;
- the `/ws/chat` contract;
- the Node path;
- the Tachi bridge;
- authentication, allowlist, credential, privacy, sandbox, and approval settings (they migrate or block a capability, never silently disappear).

Fork-specific facts (type placement, upstream divergences, known gaps) are in [`docs/book/src/contributing/fork-notes.md`](docs/book/src/contributing/fork-notes.md).

## Rules

1. **One issue slice = one PR.** Touch only what the slice names; no drive-by refactors or reformatting.
2. **Upstream first.** Before writing a capability, check upstream `zeroclaw-labs/zeroclaw`. When a PR ports, adapts, or skips an upstream change, name the source PR or commit, the equivalent code already here, and the real gap. Port bounded changes; never merge upstream wholesale.
3. **Replace, then delete.** Delete code only when it has no retained consumer, its behavior is explicitly retired, or a replacement works on the production path. Name them in the PR. Line and crate counts are budgets, not goals.
4. **Delete, don't feature-gate.** Removed code takes its exclusive tests, docs, config keys, features, and dependencies with it. Tests of shared guarantees stay.
5. **No new durable store without naming its owner** in the PR and the issue.
6. **One source of truth.** Before adding a field, cache, or table, name where the fact already lives and resolve it from there.
7. **Safety.**
   - Never commit secrets or personal data.
   - New external surfaces default closed.
   - Production paths propagate errors instead of `unwrap()`/`expect()`.
   - Remove unused code rather than silencing it.
8. **User-facing text** uses Fluent `fl!()` keys; logs stay English.
9. **Git.** Work on a non-`master` branch and open a PR to `master`.
   - PR titles are conventional commits with a scope, `type(scope): summary`; CI rejects others.
   - Use the PR template.
   - Do not add bot or AI attribution footers.

## Validation

Run what matches the change, and paste the commands and results in the PR:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features ci-all -- -D warnings
cargo test -p <changed crates>          # a full `cargo test` before an issue's last slice
bash scripts/ci/provider_dispatch_gate.sh   # model calls go through ProviderDispatch
bash scripts/ci/docs_quality_gate.sh && bash scripts/ci/docs_links_gate.sh   # docs changes
(cd web && npm ci && npm run build)     # gateway routes or web/ changed
```

`scripts/ci/toolchain_gate.sh` checks that the active toolchain matches `rust-toolchain.toml`. Tests that rely on a `chmod`-read-only directory fail when run as root; CI runs unprivileged.

Stop and ask the owner on the issue when:

- a deletion would break CLI chat, `/ws/chat`, cron, memory, approvals, the Node path, or the Tachi bridge;
- a step needs an endpoint, store, or config key the issue did not name;
- CI fails for a reason you cannot explain from the log.
