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
# third-party package or cached external target name is skipped, including
# standalone tools that are absent from the current dependency graph.
#
# Usage:
#   scripts/dev/target_sweep.sh [--dry-run] [--profile <dir>]...
#   scripts/dev/target_sweep.sh --help
#
# The target is resolved exactly like cargo-local.sh (main checkout by default).
# A shared target lock excludes wrapper builds for the entire sweep. The process
# check remains a best-effort guard for bare cargo/rustc outside that protocol.
#
# Prerequisites: Bash 4+, Python 3 (macOS/Linux), cargo, jq

set -euo pipefail
unset CDPATH

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

usage() {
  sed -n '2,29p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

die() {
  echo "error: $*" >&2
  exit 1
}

# Called only by the supervising lock holder; sourcing does not start a sweep.
target_sweep_main() {
  TARGET_DIR="${CARGO_TARGET_DIR:?shared target launcher required}"
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
  metadata_all="$(cd "$REPO_ROOT" && cargo metadata --format-version 1 --offline 2>/dev/null ||
    cargo metadata --format-version 1 2>/dev/null || true)"
  # Fail closed: without the full graph there is no third-party skip list, and a
  # generic own target name (e.g. `component`, `system`) could match a dependency.
  [[ -n "$metadata_all" ]] || die "cargo metadata (full graph) failed; cannot build the third-party skip list"
  jq -e '(.packages | type == "array") and (.workspace_members | type == "array")' \
    <<<"$metadata_all" >/dev/null || die "invalid full metadata graph; refusing to sweep"

  mapfile -t own_names < <(
    jq -r '.packages[] | .name, (.targets[].name)' <<<"$metadata_own" |
      while IFS= read -r n; do
        printf '%s\n%s\n' "$n" "${n//-/_}"
      done | sort -u
  )
  mapfile -t third_party < <(
    jq -r '[.workspace_members[]] as $ws
      | .packages[] | select(.id as $id | $ws | index($id) | not)
      | .name, (.targets[].name)' <<<"$metadata_all" |
      while IFS= read -r n; do
        printf '%s\n%s\n' "$n" "${n//-/_}"
      done | sort -u
  )

  declare -A own=() skip=()
  for n in "${own_names[@]}"; do own["$n"]=1; done
  for n in "${third_party[@]}"; do skip["$n"]=1; done

  # The target can also contain cargo-install tools and older dependencies that
  # are absent from metadata. Fingerprint directories identify their packages;
  # marker filenames identify targets whose names differ from their package.
  # Gather every selected profile before deleting anything: an external package
  # found in release must also protect its colliding target name in debug.
  declare -A own_packages=()
  while IFS= read -r n; do own_packages["$n"]=1; done < <(
    jq -r '.packages[].name' <<<"$metadata_own"
  )
  for profile in "${profiles[@]}"; do
    for path in "$TARGET_DIR/$profile/.fingerprint/"*; do
      [[ -d "$path" ]] || continue
      base="${path##*/}"
      [[ "$base" =~ ^(.+)-[0-9a-f]{16}$ ]] || continue
      n="${BASH_REMATCH[1]}"
      [[ -z "${own_packages[$n]:-}" ]] || continue
      skip["$n"]=1
      skip["${n//-/_}"]=1
      for marker in "$path/"*; do
        [[ -f "$marker" ]] || continue
        base="${marker##*/}"
        [[ "$base" != *.json ]] || continue
        if [[ "$base" =~ ^(test-)?(bin|lib|example|integration-test|bench)-(.+)$ ]]; then
          n="${BASH_REMATCH[3]}"
          skip["$n"]=1
          skip["${n//-/_}"]=1
        fi
      done
    done
  done

  is_own() {
    local base="$1" name
    # Strip the trailing `-<hash>` (16 hex in deps/.fingerprint/build, base36
    # in incremental) and any file extension after it.
    if [[ "$base" =~ ^(.+)-[0-9a-z]{10,}(\..*)?$ ]]; then
      name="${BASH_REMATCH[1]}"
    else
      return 1
    fi
    # Third-party ownership wins, including names containing zeroclaw and
    # collisions with workspace target names. Check both artifact spellings.
    [[ -z "${skip[$name]:-}" ]] || return 1
    if [[ "$name" == lib* ]]; then
      [[ -z "${skip[${name#lib}]:-}" ]] || return 1
      [[ -n "${own[$name]:-}" ]] || name="${name#lib}"
    fi
    [[ "$name" == *zeroclaw* || -n "${own[$name]:-}" ]]
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
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  for arg in "$@"; do
    if [[ "$arg" == -h || "$arg" == --help ]]; then usage; exit 0; fi
  done
  exec python3 "$REPO_ROOT/scripts/dev/shared_target.py" sweep "$REPO_ROOT" "$BASH" "$@"
fi
