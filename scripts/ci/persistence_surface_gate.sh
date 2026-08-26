#!/usr/bin/env bash

# Persistence-surface manifest gate (TB-22, frozen contract rev 3 —
# owner-ratified no-new-ledgers law; vertical V2b DoD row 11).
#
# Freeze-no-growth: no new durable task/attempt/workspace/eval/approval/
# delivery store, and no new WRITER PATH to any annex table. Enforcement
# is a checked-in manifest (`persistence_surface.json`) enumerating the
# codebase's detected persistence surface:
#
#   1. every source file containing sqlite DDL (case-insensitive
#      CREATE/ALTER TABLE, including inside test blocks and string
#      literals - drift is what this gate flags; judgment happens in the
#      PR that updates the manifest); .txt/.md files with DDL count too
#      (DDL smuggled outside .rs);
#   2. every .sql file containing DDL;
#   3. every source file opening a rusqlite connection, OR mentioning a
#      store-crate name at all (alias proof: `use rusqlite as db` still
#      contains the crate name);
#   4. every source file using OpenOptions (the hand-rolled durable
#      file-store shape - JSONL append ledgers live here);
#   5. every crate declaring an embedded-store dependency (rusqlite,
#      sled, redb, rocksdb).
#
# Detection is signature-based, not semantic: a determined author can
# still evade it (constructed DDL fragments, file writes without
# OpenOptions in fresh code paths). The gate's contract is drift
# VISIBILITY plus a PR-visible manifest change - human review stays the
# authority.
#
# Exit status: 0 = surface matches the manifest; 1 = drift found (a
# PR-visible manifest change citing a TB-22 exemption is required);
# 2 = fatal error.
#
# Overridable inputs (used by the self-test):
#   SCAN_ROOT               — tree to scan (default: repo root)
#   PERSISTENCE_MANIFEST    — manifest path (default: beside this script)

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
scan_root="${SCAN_ROOT:-$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null || printf '%s' "$script_dir/..")}"
manifest="${PERSISTENCE_MANIFEST:-$script_dir/persistence_surface.json}"

if ! command -v rg >/dev/null 2>&1; then
    echo "FATAL: persistence-surface gate requires ripgrep (rg)." >&2
    exit 2
fi
if [[ ! -f "$manifest" ]]; then
    echo "FATAL: persistence-surface manifest not found at $manifest" >&2
    exit 2
fi
if [[ ! -d "$scan_root" ]]; then
    echo "FATAL: scan root not a directory: $scan_root" >&2
    exit 2
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

# Detected surface: DDL (incl. .txt/.md), connection-open sites,
# store-crate mentions (alias-proof), OpenOptions write paths, and
# store-crate dependencies under the workspace source trees.
{
    rg -l --no-messages -i -U \
        'create[[:space:]]+table|alter[[:space:]]+table|create[[:space:]]*/\*[^*]*\*/[[:space:]]*table' \
        "$scan_root/crates" "$scan_root/apps" -g '*.rs' -g '*.txt' -g '*.md' 2>/dev/null || true
    rg -l --no-messages 'Connection::open' \
        "$scan_root/crates" "$scan_root/apps" -g '*.rs' 2>/dev/null || true
    rg -l --no-messages --glob '*.sql' . "$scan_root/crates" "$scan_root/apps" 2>/dev/null || true
    rg -l --no-messages -w -e 'rusqlite' -e 'sled' -e 'redb' -e 'rocksdb' \
        "$scan_root/crates" "$scan_root/apps" -g '*.rs' 2>/dev/null || true
    rg -l --no-messages 'OpenOptions' \
        "$scan_root/crates" "$scan_root/apps" -g '*.rs' 2>/dev/null || true
} | sed -E "s#^$scan_root/##" | sort -u >"$tmp_dir/detected_files"

{
    rg -l --no-messages \
        -e "^[[:space:]]*(rusqlite|sled|redb|rocksdb)[[:space:]]*=" \
        -e "^\\[[^]]*dependencies\\.(rusqlite|sled|redb|rocksdb)\\]" \
        "$scan_root/crates" -g 'Cargo.toml' 2>/dev/null || true
    rg -l --no-messages \
        -e "^[[:space:]]*(rusqlite|sled|redb|rocksdb)[[:space:]]*=" \
        -e "^\\[[^]]*dependencies\\.(rusqlite|sled|redb|rocksdb)\\]" \
        "$scan_root/apps" -g 'Cargo.toml' 2>/dev/null || true
} | sed -E "s#^$scan_root/##" | sed -E 's#^((crates|apps)/[^/]+)/Cargo.toml$#\1#' | sort -u >"$tmp_dir/detected_crates"

python3 - "$manifest" "$tmp_dir/detected_files" "$tmp_dir/detected_crates" <<'PYEOF'
import json
import sys

manifest_path, detected_files_path, detected_crates_path = sys.argv[1:4]
with open(detected_files_path, encoding="utf-8") as fh:
    detected_files = {line.strip() for line in fh if line.strip()}
with open(detected_crates_path, encoding="utf-8") as fh:
    detected_crates = {line.strip() for line in fh if line.strip()}

with open(manifest_path, encoding="utf-8") as fh:
    manifest = json.load(fh)

listed_files = {entry["path"] for entry in manifest["files"]}
listed_crates = set(manifest["store_crates"])

problems = []
for path in sorted(detected_files - listed_files):
    problems.append(
        f"UNLISTED PERSISTENCE-SURFACE FILE: {path} contains sqlite DDL or a "
        "connection-open site but is not in the persistence-surface manifest. "
        "TB-22 freeze-no-growth: add it to scripts/ci/persistence_surface.json "
        "with a basis citation, or remove the store."
    )
for path in sorted(listed_files - detected_files):
    problems.append(
        f"STALE MANIFEST ENTRY: {path} is listed but no longer matches the "
        "detection scan; prune it so the manifest stays trustworthy."
    )
for crate in sorted(detected_crates - listed_crates):
    problems.append(
        f"UNLISTED STORE CRATE: {crate} declares an embedded-store dependency "
        "but is not in the manifest store_crates list (TB-22: a new durable "
        "store crate needs a PR-visible manifest change citing an exemption)."
    )

if problems:
    print("persistence-surface gate: DRIFT FOUND (TB-22) - refusing.")
    for problem in problems:
        print(f"  - {problem}")
    sys.exit(1)

print(
    f"persistence-surface gate: clean "
    f"({len(listed_files)} listed files, {len(listed_crates)} store crates)."
)
PYEOF
