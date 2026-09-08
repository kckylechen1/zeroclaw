#!/usr/bin/env bash

# Test harness for advisory_exceptions_gate.sh.
# Asserts:
# 1. Production configs pass cleanly.
# 2. Presence of retired advisory RUSTSEC-2026-0268 in deny.toml fails.
# 3. Retired advisory on a multi-item line in deny.toml fails, specifically diagnosing the retired ID.
# 4. Presence of retired advisory RUSTSEC-2026-0269 in audit.toml fails.
# 5. Retired advisory on a multi-item line in audit.toml fails, specifically diagnosing the retired ID.
# 6. Reordered keys in deny.toml inline tables pass when valid.
# 7. Bare string in deny.toml fails (bypassing table structure).
# 8. Missing reason in deny.toml fails.
# 9. Commented-out ignore assignment before real assignment does not bypass the gate.
# 10. Table headers with trailing comments are parsed cleanly.
# 11. Loose prose without accountable owner and expiry condition fails.
# 12. Word-prefix false positives (e.g. "ownership unclear; reviewed recently") are strictly rejected.
# 13. Explicit structured lifecycle metadata (owner @alice; expires YYYY-MM-DD) passes.
# 14. Free-form review condition (review: <condition>) passes.
# 15. Qualified review due and date conditions pass.
# 16. Blank lifecycle values are rejected.
# 17. Invalid owner tokens are rejected.
# 18. Comment in audit.toml missing fails.
# 19. Missing config file fails strictly with exit status 2.
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

echo "=== Test 3: Retired advisory on multi-item line in deny.toml fails ==="
cat << 'DENYEOF' > "$tmp_dir/deny_multi_retired.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2025-0141", reason = "owner: @team; expires: 2026-12-31" }, { id = "RUSTSEC-2026-0268", reason = "owner: @team; expires: 2026-12-31" },
]
DENYEOF
set +e
err_out=$(DENY_TOML="$tmp_dir/deny_multi_retired.toml" bash "$gate" 2>&1)
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 in multi-item line deny test, got $status" >&2
    exit 1
fi
if ! echo "$err_out" | grep -q "Retired advisory 'RUSTSEC-2026-0268'"; then
    echo "FAIL: Expected error diagnostic for retired advisory RUSTSEC-2026-0268 in multi-item line deny test" >&2
    echo "Output was: $err_out" >&2
    exit 1
fi

echo "=== Test 4: Retired advisory in audit.toml fails ==="
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

echo "=== Test 5: Retired advisory on multi-item line in audit.toml fails ==="
cat << 'AUDITEOF' > "$tmp_dir/audit_multi_retired.toml"
[advisories]
ignore = [
    "RUSTSEC-2025-0141", "RUSTSEC-2026-0269", # tracking #8519; fix pending
]
AUDITEOF
set +e
err_out=$(AUDIT_TOML="$tmp_dir/audit_multi_retired.toml" bash "$gate" 2>&1)
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 in multi-item line audit test, got $status" >&2
    exit 1
fi
if ! echo "$err_out" | grep -q "Retired advisory 'RUSTSEC-2026-0269'"; then
    echo "FAIL: Expected error diagnostic for retired advisory RUSTSEC-2026-0269 in multi-item line audit test" >&2
    echo "Output was: $err_out" >&2
    exit 1
fi

echo "=== Test 6: Reordered keys in deny.toml inline tables pass ==="
cat << 'DENYEOF' > "$tmp_dir/deny_reordered.toml"
[advisories]
ignore = [
    { reason = "owner: @security-team; expires: 2027-01-01", id = "RUSTSEC-2025-0141" },
]
DENYEOF
if ! DENY_TOML="$tmp_dir/deny_reordered.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected success on reordered inline-table keys in deny.toml" >&2
    exit 1
fi

echo "=== Test 7: Bare string in deny.toml fails ==="
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

echo "=== Test 8: Missing reason in deny.toml fails ==="
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

echo "=== Test 9: Commented-out ignore assignment does not bypass gate ==="
cat << 'DENYEOF' > "$tmp_dir/deny_commented_ignore.toml"
[advisories]
# example: ignore = []
ignore = [
    { id = "RUSTSEC-2026-0268", reason = "owner: @security-team; expires: 2027-01-01" },
]
DENYEOF
set +e
err_out=$(DENY_TOML="$tmp_dir/deny_commented_ignore.toml" bash "$gate" 2>&1)
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 in commented-out ignore test, got $status" >&2
    exit 1
fi
if ! echo "$err_out" | grep -q "Retired advisory 'RUSTSEC-2026-0268'"; then
    echo "FAIL: Expected commented-out ignore to not shadow real ignore assignment" >&2
    echo "Output was: $err_out" >&2
    exit 1
fi

echo "=== Test 10: Table header with trailing comment is accepted ==="
cat << 'DENYEOF' > "$tmp_dir/deny_header_comment.toml"
[advisories] # dependency and security policies
ignore = [
    { id = "RUSTSEC-2025-0141", reason = "owner: @security-team; expires: 2027-01-01" },
]
[licenses] # license policies
allow = ["MIT"]
DENYEOF
if ! DENY_TOML="$tmp_dir/deny_header_comment.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected table header with trailing comment to be accepted" >&2
    exit 1
fi

echo "=== Test 11: Loose prose without accountable owner and expiry fails ==="
cat << 'DENYEOF' > "$tmp_dir/deny_loose_prose.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "transitive dependency is unmaintained" },
]
DENYEOF
if DENY_TOML="$tmp_dir/deny_loose_prose.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on loose prose 'transitive dependency is unmaintained'" >&2
    exit 1
fi

echo "=== Test 12: Word-prefix false positive is rejected ==="
cat << 'DENYEOF' > "$tmp_dir/deny_word_prefix.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "ownership unclear; reviewed recently" },
]
DENYEOF
if DENY_TOML="$tmp_dir/deny_word_prefix.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on word-prefix 'ownership unclear; reviewed recently'" >&2
    exit 1
fi

echo "=== Test 13: Explicit structured lifecycle metadata passes ==="
cat << 'DENYEOF' > "$tmp_dir/deny_explicit_lifecycle.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner @alice; expires 2099-12-31" },
]
DENYEOF
if ! DENY_TOML="$tmp_dir/deny_explicit_lifecycle.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected success on explicit 'owner @alice; expires 2099-12-31'" >&2
    exit 1
fi

echo "=== Test 14: Free-form review condition passes ==="
cat << 'DENYEOF' > "$tmp_dir/deny_freeform_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "tracking #123; review: quarterly" },
]
DENYEOF
if ! DENY_TOML="$tmp_dir/deny_freeform_review.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected success on free-form 'tracking #123; review: quarterly'" >&2
    exit 1
fi

echo "=== Test 15: Qualified review due and date conditions pass ==="
cat << 'DENYEOF' > "$tmp_dir/deny_qualified_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "tracking #123; review due: 2027-01-01" },
    { id = "RUSTSEC-2099-0002", reason = "tracking #123; review date: 2027-01-01" },
]
DENYEOF
if ! DENY_TOML="$tmp_dir/deny_qualified_review.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected success on qualified review due and date conditions" >&2
    exit 1
fi

echo "=== Test 16: Blank lifecycle values are rejected ==="
cat << 'DENYEOF' > "$tmp_dir/deny_blank_metadata.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: ; review: ; note" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_blank_metadata.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 on blank metadata, got $status" >&2
    exit 1
fi

echo "=== Test 17: Invalid owner tokens are rejected ==="
cat << 'DENYEOF' > "$tmp_dir/deny_invalid_owner.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: ???; review: quarterly" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_invalid_owner.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 on invalid owner token, got $status" >&2
    exit 1
fi

echo "=== Test 18: Missing comment in audit.toml fails ==="
cat << 'AUDITEOF' > "$tmp_dir/audit_no_comment.toml"
[advisories]
ignore = [
    "RUSTSEC-2025-0141",
]
AUDITEOF
if AUDIT_TOML="$tmp_dir/audit_no_comment.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on missing inline comment in audit.toml" >&2
    exit 1
fi

echo "=== Test 19: Missing config file fails strictly with exit status 2 ==="
set +e
AUDIT_TOML="$tmp_dir/nonexistent.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 2 ]; then
    echo "FAIL: Expected status 2 for missing config file, got $status" >&2
    exit 1
fi

echo "All advisory_exceptions_gate self-tests passed cleanly."
