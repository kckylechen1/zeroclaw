#!/usr/bin/env bash
# target_sweep.sh - reclaim disk from a cargo target dir without losing the
# third-party dependency cache.
#
# Most of a ZeroClaw target dir is the workspace's own crates: every feature
# combination and every test binary leaves another copy of them, and they are
# rebuilt on the next change anyway. Third-party deps are the cheap part on
# disk but the expensive part to rebuild (~760 crates at opt-level 3), so this
# script removes only workspace-owned artifacts and never touches the rest.
# Use it instead of `cargo clean` or hand-deleting target/*/deps.
#
# Removed, per profile dir (default: debug):
#   deps/<own crate or test target>-<hash>*   (libs, rmeta, test executables)
#   .fingerprint/<own package>-<hash>
#   build/<own package>-<hash>               (own build-script output)
#   incremental/<own crate>-<hash>
# "Own" names come from `cargo metadata --no-deps` (every workspace package
# and target), plus anything matching *zeroclaw*. A name that is also a
# third-party package name is skipped.
#
# Usage:
#   scripts/dev/target_sweep.sh [--dry-run] [--profile <dir>]...
#   scripts/dev/target_sweep.sh --help
#
# The target dir is $CARGO_TARGET_DIR, else <repo>/target. The sweep refuses
# to run while any cargo or rustc process is alive, since deleting artifacts
# under a running build corrupts it.
#
# Prerequisites: cargo, jq

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"

usage() {
  sed -n '2,29p' "$0" | sed 's/^# \{0,1\}//'
}

die() {
  echo "error: $*" >&2
  exit 1
}

dry_run=0
profiles=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    -n | --dry-run)
      dry_run=1
      ;;
    --profile)
      [[ $# -ge 2 ]] || die "--profile needs a value (e.g. debug)"
      profiles+=("$2")
      shift
      ;;
    *)
      die "unknown argument '$1' (see --help)"
      ;;
  esac
  shift
done
[[ ${#profiles[@]} -gt 0 ]] || profiles=(debug)

command -v jq >/dev/null 2>&1 || die "jq not found"
[[ -d "$TARGET_DIR" ]] || die "target dir $TARGET_DIR does not exist"

if [[ "$dry_run" -eq 0 ]] && pgrep -x 'cargo|rustc|cargo-nextest' >/dev/null 2>&1; then
  echo "error: a cargo/rustc process is running; refusing to sweep under a live build:" >&2
  pgrep -l -x 'cargo|rustc|cargo-nextest' >&2 || true
  exit 1
fi

# Own names: package names (fingerprint/build dirs keep dashes) and target
# names (deps/incremental use underscores). Third-party package names are
# excluded so a colliding test-target name can never match a dependency.
metadata_own="$(cd "$REPO_ROOT" && cargo metadata --no-deps --format-version 1 --offline 2>/dev/null ||
  cargo metadata --no-deps --format-version 1)"
metadata_all="$(cd "$REPO_ROOT" && cargo metadata --format-version 1 --offline 2>/dev/null || true)"

mapfile -t own_names < <(
  jq -r '.packages[] | .name, (.targets[].name)' <<<"$metadata_own" |
    while IFS= read -r n; do
      printf '%s\n%s\n' "$n" "${n//-/_}"
    done | sort -u
)
mapfile -t third_party < <(
  if [[ -n "$metadata_all" ]]; then
    jq -r '[.workspace_members[]] as $ws
      | .packages[] | select(.id as $id | $ws | index($id) | not)
      | .name, (.targets[].name)' <<<"$metadata_all" |
      while IFS= read -r n; do
        printf '%s\n%s\n' "$n" "${n//-/_}"
      done | sort -u
  fi
)

declare -A own=() skip=()
for n in "${own_names[@]}"; do own["$n"]=1; done
for n in "${third_party[@]}"; do skip["$n"]=1; done

is_own() {
  local base="$1" name
  # Strip the trailing `-<hash>` (16 hex in deps/.fingerprint/build, base36
  # in incremental) and any file extension after it.
  if [[ "$base" =~ ^(.+)-[0-9a-z]{10,}(\..*)?$ ]]; then
    name="${BASH_REMATCH[1]}"
  else
    return 1
  fi
  [[ "$name" == *zeroclaw* ]] && return 0
  # rlib/rmeta/so files carry a `lib` prefix; test executables do not.
  if [[ -z "${own[$name]:-}" && "$name" == lib* ]]; then
    name="${name#lib}"
  fi
  [[ -n "${own[$name]:-}" && -z "${skip[$name]:-}" ]]
}

total_kb=0
count=0
for profile in "${profiles[@]}"; do
  dir="$TARGET_DIR/$profile"
  if [[ ! -d "$dir" ]]; then
    echo "skip: $dir does not exist"
    continue
  fi
  for sub in deps .fingerprint build incremental; do
    [[ -d "$dir/$sub" ]] || continue
    while IFS= read -r -d '' path; do
      base="${path##*/}"
      is_own "$base" || continue
      kb="$(du -sk "$path" 2>/dev/null | cut -f1)"
      total_kb=$((total_kb + ${kb:-0}))
      count=$((count + 1))
      if [[ "$dry_run" -eq 1 ]]; then
        echo "would remove: ${path#"$TARGET_DIR"/} (${kb:-0} KiB)"
      else
        rm -rf -- "$path"
      fi
    done < <(find "$dir/$sub" -mindepth 1 -maxdepth 1 -print0)
  done
done

verb="removed"
[[ "$dry_run" -eq 1 ]] && verb="would remove"
echo "$verb $count workspace-owned artifacts, $((total_kb / 1024)) MiB, under $TARGET_DIR (${profiles[*]})"
