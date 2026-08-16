#!/usr/bin/env bash

set -euo pipefail

MODE="correctness"
if [ "${1:-}" = "--strict" ]; then
    MODE="strict"
fi

echo "==> rust quality: cargo fmt --all -- --check"
cargo fmt --all -- --check

CLIPPY_WORKSPACE_ARGS=(--workspace --exclude zeroclaw-desktop --all-targets)

if [ "$MODE" = "strict" ]; then
    # Local `--strict` path: same lint set and feature surface as required
    # CI (both compile with `--features ci-all`).
    echo "==> rust quality: cargo clippy --locked --workspace --exclude zeroclaw-desktop --all-targets --features ci-all -- -D warnings"
    cargo clippy --locked "${CLIPPY_WORKSPACE_ARGS[@]}" --features ci-all -- -D warnings
else
    # Local `--correctness` path: deny `clippy::correctness` on the
    # default-feature surface plus the gated channels/runtime heavy-tests suites.
    # Full-surface validation runs via `--strict` or in CI.
    echo "==> rust quality: cargo clippy --locked --workspace --exclude zeroclaw-desktop --all-targets --features zeroclaw-channels/heavy-tests,zeroclaw-runtime/heavy-tests -- -D clippy::correctness"
    cargo clippy --locked "${CLIPPY_WORKSPACE_ARGS[@]}" --features zeroclaw-channels/heavy-tests,zeroclaw-runtime/heavy-tests -- -D clippy::correctness
fi

"$(dirname "${BASH_SOURCE[0]}")/provider_dispatch_gate.sh"
