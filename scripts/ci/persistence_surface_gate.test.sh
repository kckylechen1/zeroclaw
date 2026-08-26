#!/usr/bin/env bash

# Self-test for persistence_surface_gate.sh (runs in CI before the gate).
#
# Builds three fixture trees under mktemp -d:
#   clean/   — one listed store file + matching manifest  → gate passes
#   drift/   — same + one UNLISTED file with CREATE TABLE → gate fails
#   stale/   — manifest lists a file the tree lacks       → gate fails
# Also verifies the unlisted-sqlite-crate failure mode.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gate="$script_dir/persistence_surface_gate.sh"

if ! command -v rg >/dev/null 2>&1; then
    echo "FATAL: self-test requires ripgrep (rg)." >&2
    exit 2
fi

tmp_root="$(mktemp -d)"
trap 'rm -rf "$tmp_root"' EXIT

make_fixture() {
    local root="$1" kind="$2"
    mkdir -p "$root/crates/fixture-crate/src" "$root/crates/other-crate/src"
    cat >"$root/crates/fixture-crate/src/store.rs" <<'RS'
pub fn ddl() -> &'static str {
    "CREATE TABLE IF NOT EXISTS kept (id INTEGER PRIMARY KEY)"
}
RS
    cat >"$root/crates/fixture-crate/Cargo.toml" <<'TOML'
[package]
name = "fixture-crate"

[dependencies]
rusqlite = "0.37"
TOML
    if [[ "$kind" == "drift" ]]; then
        cat >"$root/crates/other-crate/src/smuggled.rs" <<'RS'
pub const DDL: &str = "CREATE TABLE task_ledger (id TEXT)";
RS
    fi
    local smuggled_entry=""
    if [[ "$kind" == "stale" ]]; then
        smuggled_entry=',{"path":"crates/other-crate/src/smuggled.rs","store":"x","role":"store","basis":"stale fixture"}'
    fi
    cat >"$root/manifest.json" <<JSON
{
  "version": 1,
  "law": "fixture",
  "exemptions": [],
  "sqlite_crates": ["fixture-crate"],
  "files": [
    {"path":"crates/fixture-crate/src/store.rs","store":"kept.db","role":"store","basis":"fixture"}$smuggled_entry
  ]
}
JSON
}

expect_pass() {
    local label="$1" root="$2"
    if SCAN_ROOT="$root" PERSISTENCE_MANIFEST="$root/manifest.json" bash "$gate" >/dev/null 2>&1; then
        echo "ok: $label passes"
    else
        echo "FAIL: $label should pass" >&2
        return 1
    fi
}

expect_fail() {
    local label="$1" root="$2" needle="$3"
    local out
    if out="$(SCAN_ROOT="$root" PERSISTENCE_MANIFEST="$root/manifest.json" bash "$gate" 2>&1)"; then
        echo "FAIL: $label should fail but passed" >&2
        return 1
    fi
    if [[ "$out" != *"$needle"* ]]; then
        echo "FAIL: $label failed without the expected message ($needle):" >&2
        echo "$out" >&2
        return 1
    fi
    echo "ok: $label fails with expected drift"
}

status=0
make_fixture "$tmp_root/clean" clean || status=1
make_fixture "$tmp_root/drift" drift || status=1
make_fixture "$tmp_root/stale" stale || status=1

expect_pass "clean tree" "$tmp_root/clean" || status=1
expect_fail "unlisted DDL file" "$tmp_root/drift" "UNLISTED PERSISTENCE-SURFACE FILE" || status=1
expect_fail "stale manifest entry" "$tmp_root/stale" "STALE MANIFEST ENTRY" || status=1

# Unlisted sqlite crate: clean tree manifest missing the other crate that
# now declares rusqlite (drift tree also covers it — assert the crate
# message on the drift run since other-crate has no Cargo.toml dep there;
# so build a dedicated tree).
mkdir -p "$tmp_root/crate-drift/crates/fixture-crate/src" "$tmp_root/crate-drift/crates/rogue-db/src"
cp "$tmp_root/clean/crates/fixture-crate/src/store.rs" "$tmp_root/crate-drift/crates/fixture-crate/src/store.rs"
cp "$tmp_root/clean/crates/fixture-crate/Cargo.toml" "$tmp_root/crate-drift/crates/fixture-crate/Cargo.toml"
cat >"$tmp_root/crate-drift/crates/rogue-db/Cargo.toml" <<'TOML'
[package]
name = "rogue-db"

[dependencies]
rusqlite = "0.37"
TOML
cat >"$tmp_root/crate-drift/manifest.json" <<JSON
{
  "version": 1,
  "law": "fixture",
  "exemptions": [],
  "sqlite_crates": ["fixture-crate"],
  "files": [
    {"path":"crates/fixture-crate/src/store.rs","store":"kept.db","role":"store","basis":"fixture"}
  ]
}
JSON
expect_fail "unlisted sqlite crate" "$tmp_root/crate-drift" "UNLISTED SQLITE CRATE" || status=1

if (( status != 0 )); then
    echo "persistence-surface gate self-test: FAILED" >&2
    exit 1
fi
echo "persistence-surface gate self-test: all cases pass."
