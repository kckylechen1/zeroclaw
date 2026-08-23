#!/usr/bin/env bash

# Fixture tests for tracked_state_gate.sh: each planted index violation must
# fire, each legal shape must pass, and the guard must read the Git index
# rather than the developer filesystem.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gate="${script_dir}/tracked_state_gate.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"
git init -q .

pass_count=0
fail_count=0

plant() {
    mkdir -p "$(dirname "$1")"
    : >"$1"
    git add "$1"
}

unplant() {
    git rm -q --cached "$1"
    rm -f "$1"
}

expect_fail() {
    local name="$1" path="$2"
    local out status
    set +e
    out="$(bash "$gate" 2>&1)"
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "NOT CAUGHT (wanted exit 1, got ${status}): ${name}"
        fail_count=$((fail_count + 1))
    elif ! grep -qF "$path" <<<"$out"; then
        echo "PATH NOT LISTED: ${name} (wanted '${path}')"
        echo "$out" | grep '^FAIL' || true
        fail_count=$((fail_count + 1))
    else
        pass_count=$((pass_count + 1))
    fi
    unplant "$path"
}

expect_pass() {
    local name="$1" path="$2" keep="${3:-}"
    local out status
    set +e
    out="$(bash "$gate" 2>&1)"
    status=$?
    set -e
    if [ "$status" -ne 0 ]; then
        echo "FALSE POSITIVE: ${name}"
        echo "$out" | grep '^  \[' || true
        fail_count=$((fail_count + 1))
    else
        pass_count=$((pass_count + 1))
    fi
    if [ "$keep" != "keep" ]; then
        unplant "$path"
    fi
}

# ── violations must fire ────────────────────────────────────────────────

# Plain-file case first: later cases create the .tachi directory, and a
# file cannot be planted where a directory of that name already exists.
plant ".tachi"
expect_fail "runtime-state root as a tracked plain file" ".tachi"

plant ".tachi/memory.db.migration-bak"
expect_fail "runtime-state root with migration backup name" ".tachi/memory.db.migration-bak"

plant ".zcode/plans/plan.md"
expect_fail "runtime-state root with plain markdown" ".zcode/plans/plan.md"

plant "tachi.log"
expect_fail "runtime log at repo root" "tachi.log"

plant "data/tachi.log"
expect_fail "runtime log nested in tree" "data/tachi.log"

plant "store/state.db.migration-bak"
expect_fail "migration backup form anywhere" "store/state.db.migration-bak"

plant "store/state.db.migration-bak2"
expect_fail "migration backup form with suffix digits" "store/state.db.migration-bak2"

plant "old/keep.legacy-v1"
expect_fail "legacy migration form" "old/keep.legacy-v1"

plant "old/keep.legacy-v12.toml"
expect_fail "legacy migration form multi-digit with extension" "old/keep.legacy-v12.toml"

plant ".tachi/sub dir/state"
expect_fail "runtime-state path containing a space" ".tachi/sub dir/state"

nl_path=$'.tachi/bad\nname.migration-bak'
mkdir -p ".tachi"
: >"$nl_path"
git add "$nl_path"
set +e
out="$(bash "$gate" 2>&1)"
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "NEWLINE PATH NOT CAUGHT (wanted exit 1, got ${status})"
    fail_count=$((fail_count + 1))
elif ! grep -qF 'bad\nname.migration-bak' <<<"$out"; then
    echo "NEWLINE PATH NOT SANITIZED IN REPORT"
    echo "$out" | grep '^  \[' || true
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi
git rm -q --cached "$nl_path"
rm -f "$nl_path"

# ── legal shapes must pass ──────────────────────────────────────────────

plant "src/main.rs"
expect_pass "ordinary source file" "src/main.rs"

plant "tests/fixtures/store.db"
expect_pass "intentional SQLite test fixture" "tests/fixtures/store.db"

plant "docs/legacy-vault.md"
expect_pass "legacy- name not followed by a digit" "docs/legacy-vault.md"

plant "src/my file.rs"
expect_pass "source path containing a space" "src/my file.rs"

# On-disk-only runtime state (never staged) must not fire: the guard reads
# the index, not `find .`.
mkdir -p ".tachi"
: >".tachi/on_disk_only.db"
expect_pass "untracked runtime-state file ignored (index is canonical)" ".tachi/on_disk_only.db" "keep"
rm -rf ".tachi"

# ── allowlist mechanism ─────────────────────────────────────────────────

gate_allow="${tmp}/gate_allow.sh"
cp "$gate" "$gate_allow"
perl -0pi -e 's/allowlist=\(\n\)/allowlist=(\n    ".tachi\/fixture\/kept.db"\n)/' "$gate_allow"

plant ".tachi/fixture/kept.db"
set +e
out="$(bash "$gate" 2>&1)"
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "ALLOWLIST SANITY: pristine gate must reject the fixture (got ${status})"
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi
set +e
out="$(bash "$gate_allow" 2>&1)"
status=$?
set -e
if [ "$status" -ne 0 ]; then
    echo "ALLOWLIST ENTRY NOT HONORED: ${out}"
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi
unplant ".tachi/fixture/kept.db"

# ── unreadable index must be FATAL, never a silent pass ─────────────────

printf 'corrupt' > .git/index
set +e
out="$(bash "$gate" 2>&1)"
status=$?
set -e
if [ "$status" -ne 2 ]; then
    echo "CORRUPT INDEX NOT FATAL (wanted 2, got ${status})"
    echo "$out"
    fail_count=$((fail_count + 1))
elif ! grep -qF 'FATAL' <<<"$out"; then
    echo "FATAL DIAGNOSTIC MISSING ON CORRUPT INDEX"
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi
rm -f .git/index

# ── fatal path: outside any Git work tree ───────────────────────────────

outside="$(mktemp -d)"
set +e
out="$(cd "$outside" && bash "$gate" 2>&1)"
status=$?
set -e
rm -rf "$outside"
if [ "$status" -ne 2 ]; then
    echo "NON-REPO FATAL NOT PROPAGATED (wanted 2, got ${status})"
    fail_count=$((fail_count + 1))
elif ! grep -qF 'FATAL' <<<"$out"; then
    echo "FATAL DIAGNOSTIC MISSING"
    fail_count=$((fail_count + 1))
else
    pass_count=$((pass_count + 1))
fi

echo
echo "${pass_count} passed, ${fail_count} failed"
[ "$fail_count" -eq 0 ]
