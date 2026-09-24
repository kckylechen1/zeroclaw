# AGENTS.md - ZeroClaw

Instructions for AI coding agents in this repository. `CLAUDE.md` points here.

## Direction

This fork turns upstream ZeroClaw into a small **personal agent**:

- one always-on body on the owner's computer;
- edge devices (speaker, phone) join through the gateway as Clients and Nodes;
- the body answers directly, reasons, or hands work to CLI agents only through Tachi.

Read [ADR-013](docs/book/src/architecture/decisions/ADR-013-channels-as-gateway-clients.md) and [ADR-017](docs/book/src/architecture/decisions/ADR-017-personal-agent-body-edges-and-delegation.md). The single execution plan is epic **#374**: take the next unchecked step.

Protected (never weaken; stop and ask if a change seems to need it):

- Soul and User Model authority (ADR-014/015/016);
- approvals and safety boundaries;
- the `/ws/chat` contract;
- the Node path;
- the Tachi bridge.

Fork-specific facts (type placement, upstream divergences, known gaps) are in [`docs/book/src/contributing/fork-notes.md`](docs/book/src/contributing/fork-notes.md).

## Rules

1. **One issue slice = one PR.** Touch only what the slice names; no drive-by refactors or reformatting.
2. **Delete, don't feature-gate.** Removed code takes its tests, docs, config keys, features, and dependencies with it.
3. **No new durable store without naming its owner** in the PR and the issue.
4. **One source of truth.** Before adding a field, cache, or table, name where the fact already lives and resolve it from there.
5. **Safety.**
   - Never commit secrets or personal data.
   - New external surfaces default closed.
   - Production paths propagate errors instead of `unwrap()`/`expect()`.
   - Remove unused code rather than silencing it.
6. **User-facing text** uses Fluent `fl!()` keys; logs stay English.
7. **Git.** Work on a non-`master` branch and open a PR to `master`.
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
