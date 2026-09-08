#!/usr/bin/env bash

# Test harness for advisory_exceptions_gate.sh.
# Asserts:
# 1. Production configs pass cleanly.
# 2. Presence of retired advisory RUSTSEC-2026-0268 in deny.toml fails.
# 3. Presence of retired advisory RUSTSEC-2026-0269 in audit.toml fails.
# 4. Bare string in deny.toml fails (bypassing table structure).
# 5. Missing reason in deny.toml fails.
# 6. Reason without owner or review/expiry condition fails.
# 7. Comment in audit.toml without owner or review/expiry condition fails.
# 8. Missing config file fails strictly with exit status 2.
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
    { id = "RUSTSEC-2026-0268", reason = "tracking #8519; fix pending" },
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
    "RUSTSEC-2026-0269", # tracking #8519; fix pending
]
AUDITEOF
if AUDIT_TOML="$tmp_dir/audit_bad.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on retired advisory in audit.toml" >&2
    exit 1
fi

echo "=== Test 4: Bare string in deny.toml fails ==="
cat << 'DENYEOF' > "$tmp_dir/deny_bare.toml"
[advisories]
ignore = [
    "RUSTSEC-2025-0141",
]
DENYEOF
if DENY_TOML="$tmp_dir/deny_bare.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on bare string in deny.toml" >&2
    exit 1
fi

echo "=== Test 5: Missing reason in deny.toml fails ==="
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

echo "=== Test 6: Reason without owner or review/expiry condition fails ==="
cat << 'DENYEOF' > "$tmp_dir/deny_no_lifecycle.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "temporarily ignored" },
]
DENYEOF
if DENY_TOML="$tmp_dir/deny_no_lifecycle.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on reason without owner/expiry in deny.toml" >&2
    exit 1
fi

echo "=== Test 7: Comment in audit.toml without owner or review/expiry condition fails ==="
cat << 'AUDITEOF' > "$tmp_dir/audit_no_lifecycle.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # temporary ignore
]
AUDITEOF
if AUDIT_TOML="$tmp_dir/audit_no_lifecycle.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on audit comment without owner/expiry" >&2
    exit 1
fi

echo "=== Test 8: Missing config file fails strictly with exit status 2 ==="
set +e
AUDIT_TOML="$tmp_dir/nonexistent.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 2 ]; then
    echo "FAIL: Expected status 2 for missing config file, got $status" >&2
    exit 1
fi

echo "All advisory_exceptions_gate self-tests passed cleanly."
