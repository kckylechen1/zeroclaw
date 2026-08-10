#!/usr/bin/env bash

set -euo pipefail

# The CI test job runs as four matrix legs, each testing the packages listed
# for it in dev/ci/test-partition.json. That file is hand-maintained, so a
# newly added workspace member could silently end up tested by no leg at all.
# This guard makes drift impossible to miss: every workspace member must
# appear in exactly one leg (or in the explicit excluded list), and nothing
# in the partition may reference a package that no longer exists.

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

partition="dev/ci/test-partition.json"

if ! command -v jq >/dev/null 2>&1; then
    echo "jq is required but was not found on PATH." >&2
    exit 1
fi

if [ ! -f "$partition" ]; then
    echo "Partition file not found: $partition" >&2
    exit 1
fi

echo "==> test partition guard: comparing $partition against cargo metadata"

# Every leg must be a non-empty array — a missing or empty leg would make the
# CI matrix run `cargo test` with no packages, which tests the whole
# workspace and silently defeats the split.
if ! jq -e '.legs | type == "object" and length > 0
        and (to_entries | all(.value | type == "array" and length > 0))' \
        "$partition" >/dev/null; then
    echo "::error file=${partition}::Every entry under .legs must be a non-empty array of package names."
    exit 1
fi

workspace_members="$(mktemp)"
partitioned="$(mktemp)"
trap 'rm -f "$workspace_members" "$partitioned"' EXIT

CARGO_NET_GIT_FETCH_WITH_CLI=true cargo metadata --no-deps --locked --format-version 1 \
    | jq -r '.packages[].name' | sort > "$workspace_members"

jq -r '(.legs[][]), (.excluded[])' "$partition" | sort > "$partitioned"

duplicates="$(uniq -d < "$partitioned")"
if [ -n "$duplicates" ]; then
    echo "::error file=${partition}::Each package must appear in exactly one leg (or in excluded). Duplicated entries:"
    sed 's/^/  /' <<<"$duplicates"
    exit 1
fi

missing="$(comm -23 "$workspace_members" "$partitioned")"
stale="$(comm -13 "$workspace_members" "$partitioned")"

if [ -n "$missing" ]; then
    echo "::error file=${partition}::Workspace members not assigned to any test leg (add them to a leg, or to excluded):"
    sed 's/^/  /' <<<"$missing"
fi

if [ -n "$stale" ]; then
    echo "::error file=${partition}::Partition entries that are not workspace members (remove or rename them):"
    sed 's/^/  /' <<<"$stale"
fi

if [ -n "$missing" ] || [ -n "$stale" ]; then
    exit 1
fi

# The CI matrix hand-lists these same four legs; a leg added here without a
# matching matrix entry would silently never run. Force the two lists to be
# reconciled together.
expected_legs="app channels runtime tail"
actual_legs="$(jq -r '.legs | keys[]' "$partition" | sort | tr '\n' ' ' | sed 's/ $//')"
if [ "$actual_legs" != "$expected_legs" ]; then
    echo "::error file=${partition}::Leg set mismatch: partition has [${actual_legs}], guard expects [${expected_legs}]. Update the CI matrix, this guard, and the partition file together."
    exit 1
fi

leg_count="$(jq -r '.legs | length' "$partition")"
member_count="$(jq -r '[.legs[][]] | length' "$partition")"
excluded_count="$(jq -r '.excluded | length' "$partition")"
echo "test partition guard passed: ${member_count} packages across ${leg_count} legs, ${excluded_count} excluded."
