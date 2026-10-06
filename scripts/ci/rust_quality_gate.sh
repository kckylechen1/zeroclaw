#!/usr/bin/env bash

set -euo pipefail

if [ "$#" -gt 1 ]; then
    echo "Usage: $0 [--correctness|--strict|--both]" >&2
    exit 2
fi
case "${1---correctness}" in
    --correctness) MODES=(correctness) ;;
    --strict) MODES=(strict) ;;
    --both) MODES=(correctness strict) ;;
    *) echo "Usage: $0 [--correctness|--strict|--both]" >&2; exit 2 ;;
esac

echo "==> rust quality: toolchain pin guard"
"$(dirname "${BASH_SOURCE[0]}")/toolchain_gate.sh"

echo "==> rust quality: cargo fmt --all -- --check"
cargo fmt --all -- --check

CLIPPY_WORKSPACE_ARGS=(--workspace --all-targets)

for MODE in "${MODES[@]}"; do
    if [ "$MODE" = "strict" ]; then
        # Local `--strict` path: same lint set and feature surface as required
        # CI (both compile with `--features ci-all`).
        echo "==> rust quality: cargo clippy --locked --workspace --all-targets --features ci-all -- -D warnings"
        cargo clippy --locked "${CLIPPY_WORKSPACE_ARGS[@]}" --features ci-all -- -D warnings
    else
        # Local `--correctness` path: deny `clippy::correctness` on the
        # default-feature surface plus the gated channels/runtime heavy-tests suites.
        # Full-surface validation runs via `--strict` or in CI.
        echo "==> rust quality: cargo clippy --locked --workspace --all-targets --features zeroclaw-channels/heavy-tests,zeroclaw-runtime/heavy-tests -- -D clippy::correctness"
        cargo clippy --locked "${CLIPPY_WORKSPACE_ARGS[@]}" --features zeroclaw-channels/heavy-tests,zeroclaw-runtime/heavy-tests -- -D clippy::correctness
    fi
    # Keep the first provider check before strict Clippy when running both.
    if [ "$MODE" = "${MODES[0]}" ]; then
        "$(dirname "${BASH_SOURCE[0]}")/provider_dispatch_gate.sh"
    fi
done
