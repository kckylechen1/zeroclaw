#!/usr/bin/env bash

# Test harness for advisory_exceptions_gate.sh.
# Asserts:
# 1. Production configs pass cleanly.
# 2. Presence of retired advisory RUSTSEC-2026-0268 in deny.toml fails.
# 3. Presence of retired advisory RUSTSEC-2026-0269 in audit.toml fails.
# 4. Missing reason in deny.toml fails.
# 5. Missing explanation comment in audit.toml fails.
# 6. Missing config file fails with fatal status.
#
# Exit status: 0 = all assertions pass, nonzero = test failure.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gate="${script_dir}/advisory_exceptions_gate.sh"

tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t 'adv_gate')"
trap 'rm -rf "$tmp_dir"' EXIT

echo "=== Test 1: Production configs pass ==="
bash "$gate"

echo "=== Test 2: Retired advisory in deny.toml fails ==="
cat << 'DENYEOF' > "$tmp_dir/deny_bad.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2026-0268", reason = "temporarily ignored" },
]
DENYEOF
if DENY_TOML="$tmp_dir/deny_bad.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on retired advisory in deny.toml" >&2
    exit 1
fi

echo "=== Test 3: Retired advisory in audit.toml fails ==="
cat << 'AUDITEOF' > "$tmp_dir/audit_bad.toml"
[advisories]
ignore = [
    "RUSTSEC-2026-0269", # wasmtime sandbox escape
]
AUDITEOF
if AUDIT_TOML="$tmp_dir/audit_bad.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on retired advisory in audit.toml" >&2
    exit 1
fi

echo "=== Test 4: Missing reason in deny.toml fails ==="
cat << 'DENYEOF' > "$tmp_dir/deny_no_reason.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "" },
]
DENYEOF
if DENY_TOML="$tmp_dir/deny_no_reason.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on empty reason in deny.toml" >&2
    exit 1
fi

echo "=== Test 5: Missing comment in audit.toml fails ==="
cat << 'AUDITEOF' > "$tmp_dir/audit_no_comment.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001",
]
AUDITEOF
if AUDIT_TOML="$tmp_dir/audit_no_comment.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on missing inline comment in audit.toml" >&2
    exit 1
fi

echo "=== Test 6: Missing config file fails with status 2 ==="
if AUDIT_TOML="$tmp_dir/nonexistent.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected fatal error on missing config file" >&2
    exit 1
fi

echo "All advisory_exceptions_gate self-tests passed cleanly."
