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
exec cargo "$@"
