#!/usr/bin/env bash
# Run host Cargo from this checkout, sharing the main checkout's target.
set -euo pipefail
unset CDPATH
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}"
# Local edit/test loops keep symbols but omit debuginfo. The test profile
# inherits dev unless explicitly overridden; restore line-tables-only when
# investigating a backtrace. Release profiles keep their own settings.
export CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG:-0}"
bash scripts/ci/toolchain_gate.sh >&2
# A managed machine has one host gate independent of this checkout or PATH.
# Hosted CI and contributors without that installation keep normal Cargo.
# Keep the marker separate: a broken/uninstalled client must not fall through.
host_launcher=""
if [[ -e "$HOME/.local/state/host-build/managed.json" || -L "$HOME/.local/state/host-build/managed.json" ]]; then
  if [[ ! -x "$HOME/bin/host-build" ]]; then
    echo 'managed host admission is unavailable; refusing ungated Cargo' >&2
    exit 70
  fi
  host_launcher="$HOME/bin/host-build"
fi
exec python3 "$repo_root/scripts/dev/shared_target.py" cargo "$repo_root" "$host_launcher" "$@"
