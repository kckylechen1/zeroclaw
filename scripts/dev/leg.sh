#!/usr/bin/env bash
# leg.sh - run one test leg for local checks and the Quality Gate.
#
# The package set, per-leg features and extra filtered runs all come from
# dev/ci/test-partition.json. Both local checks and ci.yml use this runner.
# Optional extra-run packages and test_targets limit what Cargo builds.
# Fixed package/feature combinations avoid unnecessary crate variants in
# target/ when repeating a leg.
#
# Usage:
#   scripts/dev/leg.sh <leg> [cargo args...]   run the leg (e.g. `app`)
#   scripts/dev/leg.sh --dry-run <leg>         print the commands only
#   scripts/dev/leg.sh --no-run <leg>          compile every phase without running tests
#   scripts/dev/leg.sh --no-fail-fast <leg>    finish the tests within each phase
#   scripts/dev/leg.sh --list                  list legs and their packages
#   scripts/dev/leg.sh --check                 partition covers every member once
#   scripts/dev/leg.sh --help
#
# Options before <leg> apply to the main and extra runs. Cargo args after
# <leg> (including --no-run) still apply to the main run only.
# The runner is cargo-nextest when installed (as in CI), otherwise
# `cargo test`; force one with LEG_RUNNER=nextest|test.
#
# Prerequisites: cargo, jq

set -euo pipefail
unset CDPATH

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PARTITION="$REPO_ROOT/dev/ci/test-partition.json"
CARGO_LOCAL="$REPO_ROOT/scripts/dev/cargo-local.sh"

usage() {
  sed -n '2,/^$/ { /^#/s/^# \{0,1\}//p; }' "$0"
}

die() {
  echo "error: $*" >&2
  exit 1
}

command -v jq >/dev/null 2>&1 || die "jq not found"
[[ -f "$PARTITION" ]] || die "missing $PARTITION"

check_partition() {
  local members listed dupes
  members="$(cd "$REPO_ROOT" && "$CARGO_LOCAL" metadata --no-deps --format-version 1 --locked \
    | jq -r '.packages[].name' | sort)"
  listed="$(jq -r '[.legs[][], .excluded[]] | .[]' "$PARTITION" | sort)"
  dupes="$(printf '%s\n' "$listed" | uniq -d)"
  if [[ -n "$dupes" ]]; then
    die "listed more than once in test-partition.json: $dupes"
  fi
  if [[ "$members" != "$listed" ]]; then
    echo "error: test-partition.json does not match the workspace members" >&2
    diff <(printf '%s\n' "$members") <(printf '%s\n' "$listed") >&2 || true
    exit 1
  fi
  echo "test-partition.json covers every workspace member exactly once."
}

dry_run=0
compile_only=0
no_fail_fast=0
if [[ $# -eq 0 ]]; then
  usage >&2
  exit 2
fi
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --list)
      jq -r '.legs | to_entries[] | "\(.key): \(.value | join(" "))"' "$PARTITION"
      exit 0
      ;;
    --check)
      check_partition
      exit 0
      ;;
    --dry-run)
      dry_run=1
      shift
      ;;
    --no-run)
      compile_only=1
      shift
      ;;
    --no-fail-fast)
      no_fail_fast=1
      shift
      ;;
    --*)
      die "unknown option '$1' (try --help)"
      ;;
    *)
      break
      ;;
  esac
done

leg="${1:-}"
[[ -n "$leg" ]] || die "missing <leg> (try --list)"
shift

if ! jq -e --arg leg "$leg" '.legs | has($leg)' "$PARTITION" >/dev/null; then
  die "unknown leg '$leg' (known: $(jq -r '.legs | keys | join(", ")' "$PARTITION"))"
fi

mapfile -t pkg_args < <(jq -r --arg leg "$leg" '.legs[$leg][] | "-p", .' "$PARTITION")
features="$(jq -r --arg leg "$leg" '.features[$leg] // [] | join(",")' "$PARTITION")"

runner="${LEG_RUNNER:-}"
if [[ -z "$runner" ]]; then
  if "$CARGO_LOCAL" nextest --version >/dev/null 2>&1; then runner=nextest; else runner=test; fi
fi
case "$runner" in
  nextest) run_cmd=("$CARGO_LOCAL" nextest run --locked) ;;
  test) run_cmd=("$CARGO_LOCAL" test --locked) ;;
  *) die "LEG_RUNNER must be 'nextest' or 'test', got '$runner'" ;;
esac
[[ "$compile_only" -eq 0 ]] || run_cmd+=(--no-run)
[[ "$no_fail_fast" -eq 0 ]] || run_cmd+=(--no-fail-fast)

run() {
  echo "==> $*"
  if [[ "$dry_run" -eq 0 ]]; then
    # stdin from /dev/null so a command run inside the extra-run loop below
    # cannot swallow the remaining loop input.
    (cd "$REPO_ROOT" && "$@") </dev/null
  fi
}

main=("${run_cmd[@]}" "${pkg_args[@]}")
[[ -n "$features" ]] && main+=(--features "$features")
run "${main[@]}" "$@"

while IFS= read -r extra_run; do
  name="$(jq -r '.name' <<<"$extra_run")"
  extra="$(jq -r '.features | join(",")' <<<"$extra_run")"
  filter="$(jq -r '.filter' <<<"$extra_run")"
  extra_args=("${pkg_args[@]}")
  if jq -e 'has("packages")' <<<"$extra_run" >/dev/null; then
    mapfile -t extra_args < <(jq -r '.packages[] | "-p", .' <<<"$extra_run")
  fi
  while IFS= read -r target; do
    extra_args+=(--test "$target")
  done < <(jq -r '.test_targets[]?' <<<"$extra_run")
  echo "--- extra run: $name"
  run "${run_cmd[@]}" "${extra_args[@]}" --features "${features:+$features,}$extra" -- "$filter"
done < <(jq -c --arg leg "$leg" '.extra_runs[$leg] // [] | .[]' "$PARTITION")
