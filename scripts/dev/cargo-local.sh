#!/usr/bin/env bash
# Run host Cargo from this checkout, sharing the main checkout's target.
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
if [[ -z "${CARGO_TARGET_DIR:-}" ]]; then
  common_dir="$(git rev-parse --path-format=absolute --git-common-dir)"
  main_root="$(cd "$common_dir/.." && pwd)"
  if [[ "${common_dir##*/}" != .git || ! -f "$main_root/Cargo.toml" ]]; then
    echo 'Set CARGO_TARGET_DIR explicitly for a checkout with a separate Git directory.' >&2
    exit 2
  fi
  export CARGO_TARGET_DIR="$main_root/target"
fi
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
bash scripts/ci/toolchain_gate.sh >&2
# A managed machine has one host gate independent of this checkout or PATH.
# Hosted CI and contributors without that installation keep normal Cargo.
# Keep the marker separate: a broken/uninstalled client must not fall through.
if [[ -e "$HOME/.local/state/host-build/managed.json" || -L "$HOME/.local/state/host-build/managed.json" ]]; then
  if [[ ! -x "$HOME/bin/host-build" ]]; then
    echo 'managed host admission is unavailable; refusing ungated Cargo' >&2
    exit 70
  fi
  exec "$HOME/bin/host-build" -- cargo "$@"
fi
exec cargo "$@"
