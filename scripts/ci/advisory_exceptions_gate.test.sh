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
# 18. Negative bare-owner prose is rejected.
# 19. Punctuation-leading and qualified review conditions pass.
# 20. Retired advisory with TOML unicode escape fails in deny.toml and audit.toml.
# 21. Placeholder owner values (e.g. none, unassigned, tbd) are rejected.
# 22. Negated delimited owner prose is rejected.
# 23. Adjacent array elements without separating comma fail strictly.
# 24. Invalid calendar dates in expiry conditions are rejected.
# 25. Placeholder or negated review conditions are rejected.
# 26. Multi-word placeholder owners are rejected.
# 27. Generic upstream phrases without a concrete crate or tracking issue fail.
# 28. Expired lifecycle dates in the past fail and override fallback markers.
# 29. Negated and qualified negative review conditions and milestones fail.
# 30. Invalid calendar dates in review conditions fail.
# 31. Resolved-state prose without review or expiry trigger fails.
# 32. Negated owner shortcuts and placeholder upstream crates fail.
# 33. Explicit malformed or stale expiry declarations override fallbacks and fail.
# 34. Future expiry date does not bypass expired or invalid review date.
# 35. Placeholder and resolved-status review values fail.
# 36. Negation inside awaiting clauses fails.
# 37. Bare issue numbers without tracking qualifier fail to satisfy owner.
# 38. Lifecycle words captured as dependency names fail.
# 39. Enclosing punctuation, quotes, and leading status phrases in reviews fail.
# 40. Shared advisory entries with mismatched metadata across configs fail.
# 41. Duplicate advisory IDs fail in deny.toml and audit.toml.
# 42. Missing comma separator between inline table keys fails.
# 43. Negated or placeholder expiry values fail.
# 44. Terminal review conditions fail.
# 45. Multiline basic and literal strings in inline tables pass.
# 46. Dependency provenance without accountable owner fails.
# 47. Incomplete version review conditions and bare milestone words fail.
# 48. Actionable semver requirements and milestone conditions pass.
# 49. Undeclared one-sided advisory exceptions fail.
# 50. Zero-valued issue numbers in tracker or review fail.
# 51. Multiline strings ending in 4 and 5 quotes pass.
# 52. Grandfathered baseline entries without review condition pass.
# 53. Modified grandfathered entry without review condition fails strictly.
# 54. Non-baseline entries without review condition fail strictly.
# 55. Negated and ambiguous tool scope markers in one-sided exceptions fail.
# 56. Malformed tracking-reference suffixes fail strictly.
# 57. Expiration prose without deadline delimiter passes.
# 58. Multi-date review conditions with expired date fail.
# 59. Adding audit-only baseline ID to deny.toml without review condition fails.
# 60. Valid TOML table headers and quoted keys pass.
# 61. Comment in audit.toml missing fails.
# 62. Missing config file fails strictly with exit status 2.
# 63. Non-delimited expiry prose passes without false failure.
# 64. Modifying audit entry with empty baseline comment without compliant review metadata fails.
# 65. GITHUB_BASE_REF and GITHUB_EVENT_PATH resolve base commit for stacked PRs.
# 66. Negation before a period or newline does not leak into subsequent clauses.
# 67. Bare handles with placeholder or negation prefixes fail to satisfy owner.
# 68. Non-delimited placeholder expiry tokens fail strictly while normal expiry prose passes.
# 69. Tool-specific exceptions derive scopes dynamically and reject one-sided additions lacking scope.
# 70. Fallback baseline fails closed when neither event base nor master exists.
# 71. Tracking reference without accountable owner fails strictly.
# 72. Punctuation-only bare handles fail strictly.
# 73. Contracted negations in lifecycle conditions fail strictly.
# 74. Non-exclusive multi-tool declarations in one-sided exceptions fail strictly.
#
# Exit status: 0 = all assertions pass, nonzero = test failure.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gate="${script_dir}/advisory_exceptions_gate.sh"

tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t 'adv_gate')"
trap 'rm -rf "$tmp_dir"' EXIT

echo "=== Test 1: Production configs pass ==="
bash "$gate"

# Pin deterministic reference date for fixture tests (2026-09-08) so future dates
# like 2026-12-01 or 2027-01-01 remain deterministic across time.
export GATE_CURRENT_DATE="2026-09-08"

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
    { reason = "owner: @security-team; expires: 2027-01-01", id = "RUSTSEC-2099-0001" },
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
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; expires: 2027-01-01" },
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
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; review: quarterly" },
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
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; review due: 2027-01-01" },
    { id = "RUSTSEC-2099-0002", reason = "owner: @security-team; tracking #123; review date: 2027-01-01" },
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

echo "=== Test 18: Negative bare-owner prose is rejected ==="
cat << 'DENYEOF' > "$tmp_dir/deny_no_owner_prose.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "no owner assigned; review: quarterly" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_no_owner_prose.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 on 'no owner assigned', got $status" >&2
    exit 1
fi

echo "=== Test 19: Punctuation-leading and qualified review conditions pass ==="
cat << 'DENYEOF' > "$tmp_dir/deny_punct_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: security-team; review: >= 47.0.5" },
    { id = "RUSTSEC-2099-0002", reason = "owner: @security-team; tracking #123; review on #123" },
]
DENYEOF
if ! DENY_TOML="$tmp_dir/deny_punct_review.toml" bash "$gate" >/dev/null 2>&1; then
    echo "FAIL: Expected success on punctuation-leading review conditions" >&2
    exit 1
fi

echo "=== Test 20: Retired advisory with TOML unicode escape fails ==="
cat << 'DENYEOF' > "$tmp_dir/deny_escaped_retired.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2026-0\u003268", reason = "owner: @team; review: 2026-12-01" },
]
DENYEOF
set +e
err_out=$(DENY_TOML="$tmp_dir/deny_escaped_retired.toml" bash "$gate" 2>&1)
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 on unicode-escaped retired advisory in deny.toml, got $status" >&2
    exit 1
fi
if ! echo "$err_out" | grep -q "Retired advisory 'RUSTSEC-2026-0268'"; then
    echo "FAIL: Expected error diagnostic for retired advisory RUSTSEC-2026-0268 in unicode escape test" >&2
    exit 1
fi

cat << 'AUDITEOF' > "$tmp_dir/audit_escaped_retired.toml"
[advisories]
ignore = [
    "RUSTSEC-2026-0\u003269",  # owner: @team; review: 2026-12-01
]
AUDITEOF
set +e
err_out=$(AUDIT_TOML="$tmp_dir/audit_escaped_retired.toml" bash "$gate" 2>&1)
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 on unicode-escaped retired advisory in audit.toml, got $status" >&2
    exit 1
fi
if ! echo "$err_out" | grep -q "Retired advisory 'RUSTSEC-2026-0269'"; then
    echo "FAIL: Expected error diagnostic for retired advisory RUSTSEC-2026-0269 in unicode escape test" >&2
    exit 1
fi

echo "=== Test 21: Placeholder owner values are rejected ==="
for bad_owner in "owner: none" "owner: unassigned" "owner: tbd" "owner: n/a" "owner: placeholder" "maintainer: null"; do
    cat << DENYEOF > "$tmp_dir/deny_placeholder_owner.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_owner}; review: quarterly" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_placeholder_owner.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on placeholder owner '${bad_owner}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 22: Negated delimited owner prose is rejected ==="
for bad_prose in "no owner: assigned" "without owner: none" "no maintainer = assigned"; do
    cat << DENYEOF > "$tmp_dir/deny_negated_owner.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_prose}; review: quarterly" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_negated_owner.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on negated owner prose '${bad_prose}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 23: Adjacent array elements without separating comma fail strictly ==="
cat << 'DENYEOF' > "$tmp_dir/deny_no_comma.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @team; review: quarterly" }
    { id = "RUSTSEC-2099-0002", reason = "owner: @team; review: quarterly" }
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_no_comma.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 on adjacent elements without comma in deny.toml, got $status" >&2
    exit 1
fi

cat << 'AUDITEOF' > "$tmp_dir/audit_no_comma.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001" # owner: @team; review: quarterly
    "RUSTSEC-2099-0002" # owner: @team; review: quarterly
]
AUDITEOF
set +e
AUDIT_TOML="$tmp_dir/audit_no_comma.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 on adjacent strings without comma in audit.toml, got $status" >&2
    exit 1
fi

echo "=== Test 24: Invalid calendar dates in expiry conditions are rejected ==="
for bad_date in "2026-99-99" "2026-02-30" "2026-13-01"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_calendar.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @team; expires: ${bad_date}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_calendar.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on invalid calendar date '${bad_date}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 25: Placeholder or negated review conditions are rejected ==="
for bad_review in "review: none" "review: not needed" "review: never" "review: not planned" "review: tbd"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @team; ${bad_review}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_review.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bad review condition '${bad_review}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 26: Multi-word placeholder owners are rejected ==="
for bad_owner in "owner: not assigned" "owner: to be determined" "owner: none assigned" "owner: security team"; do
    cat << DENYEOF > "$tmp_dir/deny_multi_word_owner.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_owner}; review: quarterly" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_multi_word_owner.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on multi-word placeholder owner '${bad_owner}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 27: Generic upstream phrases without concrete crate or tracking issue fail ==="
cat << 'DENYEOF' > "$tmp_dir/deny_generic_upstream.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "transitive dep; awaiting fix" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_generic_upstream.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected validation failure status 1 on generic 'transitive dep' without crate or issue, got $status" >&2
    exit 1
fi

echo "=== Test 28: Expired lifecycle dates in the past fail ==="
cat << 'DENYEOF' > "$tmp_dir/deny_past_expiry.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @team; expires: 2000-01-01" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_past_expiry.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on expired date 2000-01-01, got status $status" >&2
    exit 1
fi

cat << 'DENYEOF' > "$tmp_dir/deny_past_expiry_fallback.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; expires: 2000-01-01; awaiting upgrade" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_past_expiry_fallback.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on expired deadline with fallback marker, got status $status" >&2
    exit 1
fi

cat << 'DENYEOF' > "$tmp_dir/deny_past_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @team; review due: 2000-01-01" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_past_review.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on past review date 2000-01-01, got status $status" >&2
    exit 1
fi

echo "=== Test 29: Negated review conditions and milestones fail ==="
for bad_lifecycle in "no review: quarterly" "not awaiting fix" "no longer awaiting fix" "not currently awaiting fix" "no upstream fix pending"; do
    cat << DENYEOF > "$tmp_dir/deny_negated_lifecycle.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @team; ${bad_lifecycle}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_negated_lifecycle.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on negated lifecycle '${bad_lifecycle}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 30: Invalid calendar dates in review conditions fail ==="
for bad_date in "review due: 2026-02-30" "review: 2026-99-99" "revisit after 2026-13-01"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_review_date.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @team; ${bad_date}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_review_date.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on invalid review date '${bad_date}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 31: Resolved-state prose without review or expiry trigger fails ==="
for resolved_prose in "patched in 1.2.3" "fixed in 1.2.3" "predates 1.0 and is not affected"; do
    cat << DENYEOF > "$tmp_dir/deny_resolved_prose.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; ${resolved_prose}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_resolved_prose.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on resolved-state prose without trigger '${resolved_prose}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 32: Negated owner shortcuts and placeholder upstream crates fail ==="
for bad_owner in \
    "no tracking issue #123; awaiting upgrade" \
    "not tracking #123; awaiting upgrade" \
    "not transitive via foo; awaiting upgrade" \
    "awaiting unknown upstream; awaiting upgrade" \
    "awaiting none upstream; awaiting upgrade" \
    "awaiting tbd upstream; awaiting upgrade" \
    "transitive via unknown; awaiting upgrade" \
    "pinned by placeholder; awaiting upgrade"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_owner_shortcut.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_owner}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_owner_shortcut.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bad owner shortcut '${bad_owner}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 33: Explicit malformed or stale expiry declarations override fallbacks ==="
for bad_expiry in \
    "owner: @security-team; tracking #123; expires: TBD; awaiting upgrade" \
    "owner: @security-team; tracking #123; expires: none; awaiting upgrade" \
    "owner: @security-team; tracking #123; expires: 2026-02-30; awaiting upgrade" \
    "owner: @security-team; tracking #123; expires: 2099-01-01; expires: 2000-01-01" \
    "owner: @security-team; tracking #123; expires: 2000-01-01; expires: 2099-01-01" \
    "owner: @security-team; tracking #123; review due: 2099-01-01; review due: 2000-01-01"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_expiry_override.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_expiry}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_expiry_override.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bad expiry override '${bad_expiry}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 34: Future expiry date does not bypass expired or invalid review date ==="
for bad_combo in \
    "owner: @security-team; tracking #123; expires: 2099-01-01; review: 2000-01-01" \
    "owner: @security-team; tracking #123; expires: 2099-01-01; review: 2026-02-30" \
    "owner: @security-team; tracking #123; expires: 2099-01-01; review: pending"; do
    cat << DENYEOF > "$tmp_dir/deny_expiry_review_combo.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_combo}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_expiry_review_combo.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bad expiry/review combo '${bad_combo}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 35: Placeholder and resolved-status review values fail ==="
for bad_review in \
    "review: pending" \
    "review: in progress" \
    "review: resolved" \
    "review: fixed" \
    "review: closed" \
    "review: open" \
    "review: tbd" \
    "review: todo"; do
    cat << DENYEOF > "$tmp_dir/deny_placeholder_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; ${bad_review}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_placeholder_review.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on placeholder review '${bad_review}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 36: Negation inside awaiting clauses fails ==="
for bad_awaiting in \
    "awaiting no upgrade" \
    "awaiting not fix" \
    "awaiting without upgrade" \
    "awaiting never migration"; do
    cat << DENYEOF > "$tmp_dir/deny_awaiting_negated.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; ${bad_awaiting}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_awaiting_negated.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on negated awaiting '${bad_awaiting}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 37: Bare issue numbers without tracking qualifier fail to satisfy owner ==="
for bare_num in \
    "patch #123 was rejected; awaiting upgrade" \
    "commit #8519 fixed it; awaiting upgrade" \
    "step #2 in plan; awaiting upgrade"; do
    cat << DENYEOF > "$tmp_dir/deny_bare_num.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bare_num}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bare_num.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bare issue number '${bare_num}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 38: Lifecycle words captured as dependency names fail ==="
for bad_dep in \
    "direct dep awaiting upgrade" \
    "transitive via awaiting upgrade" \
    "pinned by awaiting fix" \
    "pinned transitively by awaiting cleanup" \
    "awaiting fix upstream" \
    "awaiting upgrade upstream"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_dep_name.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_dep}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_dep_name.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on lifecycle word as dep name '${bad_dep}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 39: Enclosing punctuation, quotes, and leading status phrases in reviews fail ==="
for bad_norm in \
    'review: (pending)' \
    'review: [TBD]' \
    'review: "never"' \
    'review: -- resolved' \
    'review: open until fixed' \
    'review: in progress until Q4' \
    'review: later this year' \
    'review: soon after release' \
    'review: future cleanup' \
    'review: closed after upstream issue'; do
    cat << DENYEOF > "$tmp_dir/deny_bad_norm_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; ${bad_norm}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_norm_review.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bad review normalization '${bad_norm}', got status $status" >&2
        exit 1
    fi
done

# Assert valid version review condition passes
cat << 'DENYEOF' > "$tmp_dir/deny_version_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; review: >= 47.0.5" },
]
DENYEOF
DENY_TOML="$tmp_dir/deny_version_review.toml" bash "$gate" >/dev/null

echo "=== Test 40: Shared advisory entries with mismatched metadata across configs fail ==="
cat << 'DENYEOF' > "$tmp_dir/deny_mismatch.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @alice; expires: 2099-12-31" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_mismatch.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # owner: @bob; review: quarterly
]
AUDITEOF
set +e
DENY_TOML="$tmp_dir/deny_mismatch.toml" AUDIT_TOML="$tmp_dir/audit_mismatch.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on metadata mismatch between deny.toml and audit.toml, got status $status" >&2
    exit 1
fi

echo "=== Test 41: Duplicate advisory IDs fail in deny.toml and audit.toml ==="
cat << 'DENYEOF' > "$tmp_dir/deny_duplicate.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @alice; review: quarterly" },
    { id = "RUSTSEC-2099-0001", reason = "owner: @bob; review: monthly" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_duplicate.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on duplicate advisory ID in deny.toml, got status $status" >&2
    exit 1
fi

cat << 'AUDITEOF' > "$tmp_dir/audit_duplicate.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # owner: @alice; review: quarterly
    "RUSTSEC-2099-0001", # owner: @bob; review: monthly
]
AUDITEOF
set +e
AUDIT_TOML="$tmp_dir/audit_duplicate.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on duplicate advisory ID in audit.toml, got status $status" >&2
    exit 1
fi

echo "=== Test 42: Missing comma separator between inline table keys fails ==="
cat << 'DENYEOF' > "$tmp_dir/deny_no_comma.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001" reason = "owner: @alice; review: quarterly" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_no_comma.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on missing comma separator in inline table, got status $status" >&2
    exit 1
fi

echo "=== Test 43: Negated or placeholder expiry values fail ==="
for bad_exp in \
    "owner: @security-team; tracking #123; expires: not 2099-01-01" \
    "owner: @security-team; tracking #123; expires: TBD 2099-01-01" \
    "owner: @security-team; tracking #123; expires: none 2099-01-01"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_exp_val.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_exp}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_exp_val.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bad expiry value '${bad_exp}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 44: Terminal review conditions fail ==="
for terminal_review in \
    "review: completed" \
    "review: done" \
    "review: finished" \
    "review: passed" \
    "review: approved"; do
    cat << DENYEOF > "$tmp_dir/deny_terminal_review.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; ${terminal_review}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_terminal_review.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on terminal review condition '${terminal_review}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 45: Multiline basic and literal strings in inline tables pass ==="
cat << 'AUDITEOF' > "$tmp_dir/audit_multiline.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # owner: @security-lead; review: quarterly
    "RUSTSEC-2099-0002", # owner: @security-team; tracking #123; expires: 2099-01-01
]
AUDITEOF

cat << 'DENYEOF' > "$tmp_dir/deny_multiline.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = """
owner: @security-lead;
review: quarterly
""" },
    { id = "RUSTSEC-2099-0002", reason = '''
owner: @security-team; tracking #123;
expires: 2099-01-01
''' },
]
DENYEOF
DENY_TOML="$tmp_dir/deny_multiline.toml" AUDIT_TOML="$tmp_dir/audit_multiline.toml" bash "$gate" >/dev/null

echo "=== Test 46: Dependency provenance without accountable owner fails ==="
for bad_owner in \
    "transitive via foo; upstream fix pending" \
    "transitive via probe-rs; review: quarterly" \
    "pinned by rumqttc; awaiting upgrade" \
    "copy via rumqttc; review: quarterly" \
    "direct dep is affected; awaiting fix" \
    "transitive via the dependency; awaiting fix" \
    "transitive via a crate; awaiting fix" \
    "pinned by an unmaintained lib; awaiting fix"; do
    cat << DENYEOF > "$tmp_dir/deny_prose_owner.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_owner}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_prose_owner.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on provenance without owner '${bad_owner}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 47: Incomplete version review conditions and bare milestone words fail ==="
for bad_rev in \
    "owner: @security-team; tracking #123; review: >= TBD" \
    "owner: @security-team; tracking #123; review: vTBD" \
    "owner: @security-team; tracking #123; review: when" \
    "owner: @security-team; tracking #123; review: when fixed" \
    "owner: @security-team; tracking #123; review: when done" \
    "owner: @security-team; tracking #123; review: on tbd"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_rev_expr.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_rev}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_rev_expr.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bad review expression '${bad_rev}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 48: Actionable semver requirements and milestone conditions pass ==="
cat << 'AUDITEOF' > "$tmp_dir/audit_valid_reviews.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # owner: @security-team; tracking #123; review: >= 47.0.5
    "RUSTSEC-2099-0002", # owner: @security-team; tracking #123; review: v0.103.13
    "RUSTSEC-2099-0003", # owner: @security-team; tracking #123; review: on next release
    "RUSTSEC-2099-0004", # owner: @security-team; tracking #123; review: upon upstream migration
]
AUDITEOF

cat << 'DENYEOF' > "$tmp_dir/deny_valid_reviews.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; review: >= 47.0.5" },
    { id = "RUSTSEC-2099-0002", reason = "owner: @security-team; tracking #123; review: v0.103.13" },
    { id = "RUSTSEC-2099-0003", reason = "owner: @security-team; tracking #123; review: on next release" },
    { id = "RUSTSEC-2099-0004", reason = "owner: @security-team; tracking #123; review: upon upstream migration" },
]
DENYEOF
DENY_TOML="$tmp_dir/deny_valid_reviews.toml" AUDIT_TOML="$tmp_dir/audit_valid_reviews.toml" bash "$gate" >/dev/null

echo "=== Test 49: Undeclared one-sided advisory exceptions fail ==="
cat << 'DENYEOF' > "$tmp_dir/deny_onesided.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; review: quarterly" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_empty.toml"
[advisories]
ignore = []
AUDITEOF
set +e
DENY_TOML="$tmp_dir/deny_onesided.toml" AUDIT_TOML="$tmp_dir/audit_empty.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on undeclared one-sided exception in deny.toml, got status $status" >&2
    exit 1
fi

cat << 'AUDITEOF' > "$tmp_dir/audit_onesided.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # owner: @security-team; tracking #123; review: quarterly
]
AUDITEOF
cat << 'DENYEOF' > "$tmp_dir/deny_empty.toml"
[advisories]
ignore = []
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_empty.toml" AUDIT_TOML="$tmp_dir/audit_onesided.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on undeclared one-sided exception in audit.toml, got status $status" >&2
    exit 1
fi

echo "=== Test 50: Zero-valued issue numbers in tracker or review fail ==="
for bad_zero in \
    "owner: @security-team; tracking #0; review: quarterly" \
    "owner: @security-team; tracking #123; review: #0" \
    "owner: @security-team; tracking #123; review: on upstream #0"; do
    cat << DENYEOF > "$tmp_dir/deny_zero_issue.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_zero}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_zero_issue.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on zero-valued issue '${bad_zero}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 51: Multiline strings ending in 4 and 5 quotes pass ==="
cat << 'DENYEOF' > "$tmp_dir/deny_multiline_quotes.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = """"owner: @security-team; tracking #123; review: quarterly"""" },
    { id = "RUSTSEC-2099-0002", reason = ''''owner: @security-team; tracking #123; expires: 2099-01-01'''' },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_multiline_quotes.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # "owner: @security-team; tracking #123; review: quarterly"
    "RUSTSEC-2099-0002", # 'owner: @security-team; tracking #123; expires: 2099-01-01'
]
AUDITEOF
DENY_TOML="$tmp_dir/deny_multiline_quotes.toml" AUDIT_TOML="$tmp_dir/audit_multiline_quotes.toml" bash "$gate" >/dev/null

echo "=== Test 52: Grandfathered baseline entries without review condition pass ==="
cat << 'BASEEOF' > "$tmp_dir/base_deny_grandfathered.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "legacy crate binding unmaintained; tracking #8519" },
]
BASEEOF
cat << 'BASEEOF' > "$tmp_dir/base_audit_grandfathered.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # legacy crate binding unmaintained; tracking #8519
]
BASEEOF
cat << 'DENYEOF' > "$tmp_dir/deny_grandfathered.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "legacy crate binding unmaintained; tracking #8519" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_grandfathered.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # legacy crate binding unmaintained; tracking #8519
]
AUDITEOF
BASE_DENY_TOML="$tmp_dir/base_deny_grandfathered.toml" BASE_AUDIT_TOML="$tmp_dir/base_audit_grandfathered.toml" \
DENY_TOML="$tmp_dir/deny_grandfathered.toml" AUDIT_TOML="$tmp_dir/audit_grandfathered.toml" bash "$gate" >/dev/null

echo "=== Test 53: Modified grandfathered entry without review condition fails strictly ==="
cat << 'DENYEOF' > "$tmp_dir/deny_modified_grandfathered.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "legacy crate binding changed text; tracking #8519" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_modified_grandfathered.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # legacy crate binding changed text; tracking #8519
]
AUDITEOF
set +e
BASE_DENY_TOML="$tmp_dir/base_deny_grandfathered.toml" BASE_AUDIT_TOML="$tmp_dir/base_audit_grandfathered.toml" \
DENY_TOML="$tmp_dir/deny_modified_grandfathered.toml" AUDIT_TOML="$tmp_dir/audit_modified_grandfathered.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on modified grandfathered entry without review condition, got status $status" >&2
    exit 1
fi

echo "=== Test 54: Non-baseline entries without review condition fail strictly ==="
cat << 'DENYEOF' > "$tmp_dir/deny_non_baseline.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-9999", reason = "gdkwayland-sys unmaintained gtk-rs GTK3 bindings; tracking #8519" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_non_baseline.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-9999", # gdkwayland-sys unmaintained gtk-rs GTK3 bindings; tracking #8519
]
AUDITEOF
set +e
DENY_TOML="$tmp_dir/deny_non_baseline.toml" AUDIT_TOML="$tmp_dir/audit_non_baseline.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on non-baseline entry without review condition, got status $status" >&2
    exit 1
fi

echo "=== Test 55: Negated and ambiguous tool scope markers in one-sided exceptions fail ==="
for bad_scope in \
    "not cargo-deny only; owner: @security-team; tracking #123; review: quarterly" \
    "cargo-deny is affected; owner: @security-team; tracking #123; review: quarterly" \
    "affects cargo-deny; owner: @security-team; tracking #123; review: quarterly"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_scope.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_scope}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_scope.toml" AUDIT_TOML="$tmp_dir/audit_empty.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on ambiguous/negated tool scope '${bad_scope}', got status $status" >&2
        exit 1
    fi
done

# Valid explicit scope passes for one-sided exception
cat << 'DENYEOF' > "$tmp_dir/deny_valid_scope.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "cargo-deny only; owner: @security-team; tracking #123; review: quarterly" },
]
DENYEOF
DENY_TOML="$tmp_dir/deny_valid_scope.toml" AUDIT_TOML="$tmp_dir/audit_empty.toml" bash "$gate" >/dev/null

echo "=== Test 56: Malformed tracking-reference suffixes fail strictly ==="
for bad_track in \
    "owner: @security-team; tracking #123oops; review: quarterly" \
    "owner: @security-team; tracking #123_bad; review: quarterly" \
    "owner: @security-team; tracking #123; review: #456oops" \
    "owner: @security-team; tracking #123; review: on upstream #456oops"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_track_suffix.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_track}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_track_suffix.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on malformed tracking suffix '${bad_track}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 57: Expiration prose without deadline delimiter passes ==="
cat << 'DENYEOF' > "$tmp_dir/deny_expiry_prose.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; certificate expires unexpectedly; tracking #123; awaiting fix" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_expiry_prose.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # owner: @security-team; certificate expires unexpectedly; tracking #123; awaiting fix
]
AUDITEOF
DENY_TOML="$tmp_dir/deny_expiry_prose.toml" AUDIT_TOML="$tmp_dir/audit_expiry_prose.toml" bash "$gate" >/dev/null

echo "=== Test 58: Multi-date review conditions with expired date fail ==="
for bad_multidate in \
    "owner: @security-team; tracking #123; review: 2099-01-01 or 2000-01-01" \
    "owner: @security-team; tracking #123; review: by 2099-01-01, expired 2000-01-01"; do
    cat << DENYEOF > "$tmp_dir/deny_multidate.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_multidate}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_multidate.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on multi-date review with expired date '${bad_multidate}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 59: Adding audit-only baseline ID to deny.toml without review condition fails ==="
cat << 'BASEEOF' > "$tmp_dir/base_deny_empty.toml"
[advisories]
ignore = []
BASEEOF
cat << 'BASEEOF' > "$tmp_dir/base_audit_onesided.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # legacy audit-only advisory; tracking #8519
]
BASEEOF
cat << 'DENYEOF' > "$tmp_dir/deny_new_from_audit.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "legacy audit-only advisory; tracking #8519" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_new_from_audit.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # legacy audit-only advisory; tracking #8519
]
AUDITEOF
set +e
BASE_DENY_TOML="$tmp_dir/base_deny_empty.toml" BASE_AUDIT_TOML="$tmp_dir/base_audit_onesided.toml" \
DENY_TOML="$tmp_dir/deny_new_from_audit.toml" AUDIT_TOML="$tmp_dir/audit_new_from_audit.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure when audit-only ID added to deny.toml without review metadata, got status $status" >&2
    exit 1
fi

echo "=== Test 60: Valid TOML table headers and quoted keys pass ==="
cat << 'DENYEOF' > "$tmp_dir/deny_toml_headers.toml"
[ advisories ]
"ignore" = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; review: quarterly" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_toml_headers.toml"
["advisories"]
'ignore' = [
    "RUSTSEC-2099-0001", # owner: @security-team; tracking #123; review: quarterly
]
AUDITEOF
DENY_TOML="$tmp_dir/deny_toml_headers.toml" AUDIT_TOML="$tmp_dir/audit_toml_headers.toml" bash "$gate" >/dev/null

echo "=== Test 61: Missing comment in audit.toml fails ==="
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

echo "=== Test 62: Missing config file fails strictly with exit status 2 ==="
set +e
AUDIT_TOML="$tmp_dir/nonexistent.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 2 ]; then
    echo "FAIL: Expected status 2 for missing config file, got $status" >&2
    exit 1
fi

echo "=== Test 63: Non-delimited expiry prose passes without false failure ==="
for valid_prose in \
    "owner: @security-team; certificate expired at runtime; tracking #123; awaiting fix" \
    "owner: @security-team; certificate expires on reconnect; tracking #123; awaiting fix" \
    "owner: @security-team; cache expiry date handling panics; tracking #123; awaiting fix"; do
    cat << DENYEOF > "$tmp_dir/deny_expiry_prose_cases.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${valid_prose}" },
]
DENYEOF
    cat << AUDITEOF > "$tmp_dir/audit_expiry_prose_cases.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # ${valid_prose}
]
AUDITEOF
    DENY_TOML="$tmp_dir/deny_expiry_prose_cases.toml" AUDIT_TOML="$tmp_dir/audit_expiry_prose_cases.toml" bash "$gate" >/dev/null
done

echo "=== Test 64: Modifying audit entry with empty baseline comment without compliant review metadata fails ==="
cat << 'BASEEOF' > "$tmp_dir/base_audit_uncommented.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001",
]
BASEEOF
cat << 'DENYEOF' > "$tmp_dir/deny_empty.toml"
[advisories]
ignore = []
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_arbitrary_comment.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # cargo-audit only; arbitrary changed rationale without owner or review
]
AUDITEOF
set +e
BASE_AUDIT_TOML="$tmp_dir/base_audit_uncommented.toml" BASE_DENY_TOML="$tmp_dir/deny_empty.toml" \
AUDIT_TOML="$tmp_dir/audit_arbitrary_comment.toml" DENY_TOML="$tmp_dir/deny_empty.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure when modifying uncommented baseline audit entry without compliant review metadata, got status $status" >&2
    exit 1
fi

echo "=== Test 65: GITHUB_BASE_REF and GITHUB_EVENT_PATH resolve base commit for stacked PRs ==="
stacked_git_dir="$tmp_dir/stacked_repo"
mkdir -p "$stacked_git_dir/.cargo"
git -C "$stacked_git_dir" init -q -b master
git -C "$stacked_git_dir" config user.email "ci@example.com"
git -C "$stacked_git_dir" config user.name "CI"

cat << 'AUDITEOF' > "$stacked_git_dir/.cargo/audit.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # cargo-audit only; legacy unmaintained; tracking #8519
]
AUDITEOF
cat << 'DENYEOF' > "$stacked_git_dir/deny.toml"
[advisories]
ignore = []
DENYEOF
git -C "$stacked_git_dir" add .
git -C "$stacked_git_dir" commit -qm "master baseline with grandfathered exception"

# Parent branch removes the exception
git -C "$stacked_git_dir" checkout -qb parent
cat << 'AUDITEOF' > "$stacked_git_dir/.cargo/audit.toml"
[advisories]
ignore = []
AUDITEOF
git -C "$stacked_git_dir" commit -qam "remove exception in parent PR"
parent_sha=$(git -C "$stacked_git_dir" rev-parse HEAD)

# Child branch re-adds the exception without review metadata
git -C "$stacked_git_dir" checkout -qb child
cat << 'AUDITEOF' > "$stacked_git_dir/.cargo/audit.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # cargo-audit only; legacy unmaintained; tracking #8519
]
AUDITEOF
git -C "$stacked_git_dir" commit -qam "re-add unreviewed exception in child PR"

# 0. When evaluated against master baseline, it passes because it matches master
(cd "$stacked_git_dir" && REPO_ROOT="$stacked_git_dir" BASE_REF="master" bash "$gate" >/dev/null)

# 1. When evaluated against GITHUB_BASE_REF=parent, it must fail because the exception is new relative to parent
set +e
(cd "$stacked_git_dir" && REPO_ROOT="$stacked_git_dir" BASE_REF="" GITHUB_BASE_REF="parent" bash "$gate" >/dev/null 2>&1)
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure when stacked child re-adds exception against GITHUB_BASE_REF=parent, got status $status" >&2
    exit 1
fi

# 2. When evaluated using GITHUB_EVENT_PATH with base SHA of parent, it must also fail
event_json="$tmp_dir/event.json"
cat << EVENTEOF > "$event_json"
{
  "pull_request": {
    "base": {
      "sha": "$parent_sha"
    }
  }
}
EVENTEOF
set +e
(cd "$stacked_git_dir" && REPO_ROOT="$stacked_git_dir" BASE_REF="" GITHUB_EVENT_PATH="$event_json" GITHUB_BASE_REF="" bash "$gate" >/dev/null 2>&1)
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure when stacked child re-adds exception against GITHUB_EVENT_PATH base sha, got status $status" >&2
    exit 1
fi

echo "=== Test 66: Negation before a period or newline does not leak into subsequent clauses ==="
cat << 'DENYEOF' > "$tmp_dir/deny_sentence_negation.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "not exploitable on default builds. owner: @security-team; review: quarterly" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_sentence_negation.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # not exploitable on default builds. owner: @security-team; review: quarterly
]
AUDITEOF
DENY_TOML="$tmp_dir/deny_sentence_negation.toml" AUDIT_TOML="$tmp_dir/audit_sentence_negation.toml" bash "$gate" >/dev/null

echo "=== Test 67: Bare handles with placeholder or negation prefixes fail ==="
for bad_handle in \
    "@not-assigned" \
    "@no-maintainer" \
    "@none" \
    "@tbd" \
    "@placeholder" \
    "@unassigned" \
    "@unknown"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_handle.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: ${bad_handle}; review: quarterly" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_handle.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bad owner handle '${bad_handle}', got status $status" >&2
        exit 1
    fi

    # Also test as bare handle without owner: prefix
    cat << DENYEOF > "$tmp_dir/deny_bare_bad_handle.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "assigned to ${bad_handle}; review: quarterly" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bare_bad_handle.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on bare bad handle '${bad_handle}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 68: Non-delimited placeholder expiry tokens fail strictly ==="
for bad_expiry in \
    "owner: @security-team; tracking #123; expires TBD; awaiting fix" \
    "owner: @security-team; tracking #123; expires never; awaiting fix" \
    "owner: @security-team; tracking #123; expires none; awaiting fix" \
    "owner: @security-team; tracking #123; expiry todo; awaiting fix"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_expiry_token.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_expiry}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_expiry_token.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on non-delimited placeholder expiry '${bad_expiry}', got status $status" >&2
        exit 1
    fi
done

# Non-placeholder expiry prose still passes
cat << 'DENYEOF' > "$tmp_dir/deny_expiry_prose_pass.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; tracking #123; expired at runtime; awaiting fix" },
]
DENYEOF
DENY_TOML="$tmp_dir/deny_expiry_prose_pass.toml" bash "$gate" >/dev/null

echo "=== Test 69: Tool-specific exceptions derive scopes dynamically and reject one-sided additions lacking scope ==="
cat << 'DENYEOF' > "$tmp_dir/deny_0253_no_scope.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2026-0253", reason = "upstream fix pending; owner: @security-team; tracking zeroclaw-labs/zeroclaw#8519" },
]
DENYEOF
cat << 'AUDITEOF' > "$tmp_dir/audit_empty_69.toml"
[advisories]
ignore = []
AUDITEOF
set +e
DENY_TOML="$tmp_dir/deny_0253_no_scope.toml" AUDIT_TOML="$tmp_dir/audit_empty_69.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on one-sided RUSTSEC-2026-0253 without cargo-deny only scope, got status $status" >&2
    exit 1
fi

cat << 'DENYEOF' > "$tmp_dir/deny_0253_with_scope.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2026-0253", reason = "upstream fix pending; owner: @security-team; tracking zeroclaw-labs/zeroclaw#8519; cargo-deny only" },
]
DENYEOF
DENY_TOML="$tmp_dir/deny_0253_with_scope.toml" AUDIT_TOML="$tmp_dir/audit_empty_69.toml" bash "$gate" >/dev/null

echo "=== Test 70: Fallback baseline fails closed when neither event base nor master exists ==="
nomaster_git_dir="$tmp_dir/nomaster_repo"
mkdir -p "$nomaster_git_dir/.cargo"
git -C "$nomaster_git_dir" init -q -b main-trunk
git -C "$nomaster_git_dir" config user.email "ci@example.com"
git -C "$nomaster_git_dir" config user.name "CI"

cat << 'AUDITEOF' > "$nomaster_git_dir/.cargo/audit.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001", # unreviewed legacy entry without owner or expiry
]
AUDITEOF
cat << 'DENYEOF' > "$nomaster_git_dir/deny.toml"
[advisories]
ignore = []
DENYEOF
git -C "$nomaster_git_dir" add .
git -C "$nomaster_git_dir" commit -qm "initial commit on non-master branch"

# Add a second commit so HEAD~1 exists
echo "# update" >> "$nomaster_git_dir/deny.toml"
git -C "$nomaster_git_dir" commit -qam "second commit on non-master branch"

# In a repository without master or origin/master, and no BASE_REF/GITHUB_BASE_REF,
# it must NOT fallback to HEAD~1 to grandfather penultimate entries.
# It must fail closed because baseline is empty.
set +e
(cd "$nomaster_git_dir" && REPO_ROOT="$nomaster_git_dir" BASE_REF="" GITHUB_BASE_REF="" GITHUB_EVENT_PATH="" bash "$gate" >/dev/null 2>&1)
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on unreviewed entry in non-master repo without baseline fallback, got status $status" >&2
    exit 1
fi

echo "=== Test 71: Tracking reference without accountable owner fails strictly ==="
cat << 'DENYEOF' > "$tmp_dir/deny_tracker_only.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "tracking #123; review: quarterly" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_tracker_only.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure when tracking reference lacks accountable owner, got status $status" >&2
    exit 1
fi

echo "=== Test 72: Punctuation-only bare handles fail strictly ==="
for bad_punct in "@_" "@-" "@__" "@--"; do
    cat << DENYEOF > "$tmp_dir/deny_punct_handle.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "assigned to ${bad_punct}; review: quarterly" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_punct_handle.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on punctuation-only handle '${bad_punct}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 73: Contracted negations in lifecycle conditions fail strictly ==="
for bad_contraction in \
    "owner: @security-team; isn't awaiting fix" \
    "owner: @security-team; doesn't have upstream fix pending" \
    "owner: @security-team; won't fix" \
    "owner: @security-team; cannot be fixed"; do
    cat << DENYEOF > "$tmp_dir/deny_contracted_negation.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_contraction}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_contracted_negation.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on contracted negation '${bad_contraction}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 74: Non-exclusive multi-tool declarations in one-sided exceptions fail strictly ==="
for bad_multi_scope in \
    "scope: cargo-deny and cargo-audit; owner: @security-team; review: quarterly" \
    "cargo-deny and cargo-audit only; owner: @security-team; review: quarterly" \
    "scope: cargo-audit and cargo-deny; owner: @security-team; review: quarterly"; do
    cat << DENYEOF > "$tmp_dir/deny_multi_scope.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_multi_scope}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_multi_scope.toml" AUDIT_TOML="$tmp_dir/audit_empty.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on non-exclusive scope '${bad_multi_scope}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 75: Negated tails in review conditions fail strictly ==="
for bad_review_tail in \
    "review: quarterly but not required" \
    "review: quarterly, but not required" \
    "review: quarterly not required" \
    "review: quarterly isn't required" \
    "review: quarterly won't happen" \
    "review: quarterly cannot be done" \
    "review: quarterly - placeholder" \
    "review: quarterly - tbd" \
    "review: quarterly - unassigned" \
    "review: on release but not planned" \
    "review: on release but not required"; do
    cat << DENYEOF > "$tmp_dir/deny_review_tail.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; ${bad_review_tail}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_review_tail.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on review with negated tail '${bad_review_tail}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 76: Invalid owner clauses and former/previous owner prefixes fail strictly ==="
for bad_owner_clause in \
    "owner: @alice is not responsible; review: quarterly" \
    "owner: @alice isn't responsible; review: quarterly" \
    "former owner: @alice; review: quarterly" \
    "previous owner: @alice; review: quarterly" \
    "ex-owner: @alice; review: quarterly" \
    "past owner: @alice; review: quarterly" \
    "owner: not @alice; review: quarterly" \
    "not owner: @alice; review: quarterly" \
    "owner: @alice (former owner); review: quarterly"; do
    cat << DENYEOF > "$tmp_dir/deny_bad_owner_clause.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_owner_clause}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_bad_owner_clause.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on invalid owner clause '${bad_owner_clause}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 77: Multi-tool scope declarations across clauses fail strictly ==="
for bad_multi_clause_scope in \
    "scope: cargo-deny, cargo-audit; owner: @security-team; review: quarterly" \
    "scope: cargo-deny; scope: cargo-audit; owner: @security-team; review: quarterly" \
    "scope: cargo-deny; tool: cargo-audit; owner: @security-team; review: quarterly" \
    "cargo-deny only, cargo-audit only; owner: @security-team; review: quarterly" \
    "scope: cargo-deny; cargo-audit only; owner: @security-team; review: quarterly"; do
    cat << DENYEOF > "$tmp_dir/deny_multi_clause_scope.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "${bad_multi_clause_scope}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_multi_clause_scope.toml" AUDIT_TOML="$tmp_dir/audit_empty.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on multi-clause scope '${bad_multi_clause_scope}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 78: Unresolvable explicit BASE_REF fails closed with status 2 ==="
set +e
BASE_REF="definitely_nonexistent_ref_123456" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 2 ]; then
    echo "FAIL: Expected exit 2 on unresolvable explicit BASE_REF, got status $status" >&2
    exit 1
fi

echo "=== Test 79: Push event before SHA resolution detects added exceptions ==="
head_commit=$(git rev-parse HEAD)
parent_commit=$(git rev-parse HEAD~1 2>/dev/null || git rev-parse HEAD)
cat << JEOF > "$tmp_dir/push_event.json"
{
  "before": "$parent_commit",
  "after": "$head_commit"
}
JEOF
cat << DENYEOF > "$tmp_dir/deny_push_test.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "no owner or expiry here" },
]
DENYEOF
set +e
GITHUB_EVENT_PATH="$tmp_dir/push_event.json" BASE_REF="" DENY_TOML="$tmp_dir/deny_push_test.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on new exception using push event before SHA baseline, got status $status" >&2
    exit 1
fi

echo "=== Test 80: Exclusive tool scope declared on shared exception fails strictly ==="
cat << DENYEOF > "$tmp_dir/deny_shared_scoped.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "scope: cargo-deny; owner: @security-team; review: quarterly" },
]
DENYEOF
cat << AUDITEOF > "$tmp_dir/audit_shared_scoped.toml"
[advisories]
ignore = [
    "RUSTSEC-2099-0001",  # scope: cargo-deny; owner: @security-team; review: quarterly
]
AUDITEOF
set +e
DENY_TOML="$tmp_dir/deny_shared_scoped.toml" AUDIT_TOML="$tmp_dir/audit_shared_scoped.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on shared exception declaring exclusive tool scope, got status $status" >&2
    exit 1
fi

echo "=== Test 81: Shorthand tool scope with neighboring other-tool reference fails strictly ==="
cat << DENYEOF > "$tmp_dir/deny_shorthand_multi.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "cargo-deny only and cargo-audit; owner: @security-team; review: quarterly" },
]
DENYEOF
set +e
DENY_TOML="$tmp_dir/deny_shorthand_multi.toml" AUDIT_TOML="$tmp_dir/audit_empty.toml" bash "$gate" >/dev/null 2>&1
status=$?
set -e
if [ "$status" -ne 1 ]; then
    echo "FAIL: Expected failure on shorthand tool scope with other-tool in clause, got status $status" >&2
    exit 1
fi

echo "=== Test 82: Negated review clauses across delimiters fail strictly ==="
for bad_negated_clause in \
    "review: quarterly; no review required" \
    "review: quarterly; not required" \
    "review: quarterly; review not required" \
    "review: quarterly; no review needed" \
    "review: quarterly; unnecessary"; do
    cat << DENYEOF > "$tmp_dir/deny_negated_clause.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; ${bad_negated_clause}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_negated_clause.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 1 ]; then
        echo "FAIL: Expected failure on negated review clause across delimiter '${bad_negated_clause}', got status $status" >&2
        exit 1
    fi
done

echo "=== Test 83: Actionable fix-based review conditions pass ==="
for good_fix_review in \
    "review: when upstream fix lands" \
    "review: when replacement crate published" \
    "review: upon fix" \
    "review: when PR #123 merged"; do
    cat << DENYEOF > "$tmp_dir/deny_good_fix.toml"
[advisories]
ignore = [
    { id = "RUSTSEC-2099-0001", reason = "owner: @security-team; ${good_fix_review}" },
]
DENYEOF
    set +e
    DENY_TOML="$tmp_dir/deny_good_fix.toml" bash "$gate" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 0 ]; then
        echo "FAIL: Expected success on actionable fix condition '${good_fix_review}', got status $status" >&2
        exit 1
    fi
done

echo "All advisory_exceptions_gate self-tests passed cleanly."


