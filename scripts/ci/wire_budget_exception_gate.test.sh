#!/usr/bin/env bash

# Test harness for wire_budget_exception_gate.sh.
# Asserts that:
# 1. Default scripts/ci/wire_budget_exceptions.json passes with default schema.
# 2. Missing manifest fails.
# 3. Missing schema argument fails.
# 4. Invalid JSON in manifest fails.
# 5. Invalid JSON in schema file fails.
# 6. Unrelated schema JSON fails schema title validation.
# 7. Non-string / boolean $schema in manifest fails.
# 8. Altered wire_budget_tokens_ceiling fails.
# 9. Non-integer / boolean wire_budget_tokens_ceiling fails.
# 10. Non-integer / boolean version fails.
# 11. Individual missing required fields fail (one-fault tests for all 8 fields).
# 12. Boolean wire_tokens fails.
# 13. Disallowed additional root properties fail.
# 14. Disallowed additional entry properties fail.
# 15. Duplicate exception tool entries fail.
# 16. Valid exception entry with all required fields passes.
#
# Exit status: 0 = all test assertions pass, nonzero = test failure.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gate="${script_dir}/wire_budget_exception_gate.sh"

tmp_dir="$(mktemp -d 2>/dev/null || mktemp -d -t 'wb_gate')"
trap 'rm -rf "$tmp_dir"' EXIT

echo "=== Test 1: Production manifest passes ==="
bash "$gate"

echo "=== Test 2: Missing manifest fails ==="
if bash "$gate" "${tmp_dir}/nonexistent.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on missing manifest" >&2
    exit 1
fi

echo "=== Test 3: Missing schema file argument fails ==="
if bash "$gate" "scripts/ci/wire_budget_exceptions.json" "${tmp_dir}/nonexistent_schema.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on nonexistent schema file" >&2
    exit 1
fi

echo "=== Test 4: Invalid JSON in manifest fails ==="
echo "{ invalid json" > "${tmp_dir}/invalid_manifest.json"
if bash "$gate" "${tmp_dir}/invalid_manifest.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on invalid manifest JSON" >&2
    exit 1
fi

echo "=== Test 5: Invalid JSON in schema file fails ==="
echo "{ invalid schema json" > "${tmp_dir}/invalid_schema.json"
if bash "$gate" "scripts/ci/wire_budget_exceptions.json" "${tmp_dir}/invalid_schema.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on invalid schema JSON" >&2
    exit 1
fi

echo "=== Test 6: Unrelated schema JSON fails ==="
if bash "$gate" "scripts/ci/wire_budget_exceptions.json" "dev/ci/test-partition.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on unrelated schema JSON" >&2
    exit 1
fi

echo "=== Test 7: Non-string \$schema in manifest fails ==="
cat > "${tmp_dir}/bool_dollar_schema.json" << 'EOF'
{
  "$schema": false,
  "version": 1,
  "wire_budget_tokens_ceiling": 5000,
  "exceptions": []
}
EOF
if bash "$gate" "${tmp_dir}/bool_dollar_schema.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure when \$schema is boolean" >&2
    exit 1
fi

echo "=== Test 8: Loosened ceiling fails ==="
cat > "${tmp_dir}/loosened.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": 6000,
  "exceptions": []
}
EOF
if bash "$gate" "${tmp_dir}/loosened.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure when wire_budget_tokens_ceiling is modified" >&2
    exit 1
fi

echo "=== Test 9: Boolean ceiling fails ==="
cat > "${tmp_dir}/bool_ceiling.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": true,
  "exceptions": []
}
EOF
if bash "$gate" "${tmp_dir}/bool_ceiling.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure when wire_budget_tokens_ceiling is boolean" >&2
    exit 1
fi

echo "=== Test 10: Boolean version fails ==="
cat > "${tmp_dir}/bool_version.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": true,
  "wire_budget_tokens_ceiling": 5000,
  "exceptions": []
}
EOF
if bash "$gate" "${tmp_dir}/bool_version.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure when version is boolean" >&2
    exit 1
fi

echo "=== Test 11: One-fault missing required field tests ==="
required_fields=("owner" "tool_name" "rationale" "wire_tokens" "sunset_decision" "security_privacy_impact" "dependency_cost_rationale" "pr")

for missing in "${required_fields[@]}"; do
    python3 - "$missing" "${tmp_dir}/missing_${missing}.json" <<'PYEOF'
import json, sys
missing_field = sys.argv[1]
out_path = sys.argv[2]

entry = {
    "owner": "alice",
    "tool_name": "test_tool",
    "rationale": "Kernel core residency",
    "wire_tokens": 100,
    "sunset_decision": "Permanent",
    "security_privacy_impact": "None",
    "dependency_cost_rationale": "Zero cost",
    "pr": "262"
}
del entry[missing_field]

data = {
    "$schema": "./wire_budget_exceptions.schema.json",
    "version": 1,
    "wire_budget_tokens_ceiling": 5000,
    "exceptions": [entry]
}

with open(out_path, 'w', encoding='utf-8') as f:
    json.dump(data, f)
PYEOF

    if bash "$gate" "${tmp_dir}/missing_${missing}.json" >/dev/null 2>&1; then
        echo "FAIL: Expected failure when field '${missing}' is missing from exception entry" >&2
        exit 1
    fi
done

echo "=== Test 12: Boolean wire_tokens fails ==="
cat > "${tmp_dir}/bool_tokens.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": 5000,
  "exceptions": [
    {
      "owner": "maintainer1",
      "tool_name": "tool1",
      "rationale": "Rationale",
      "wire_tokens": true,
      "sunset_decision": "Decision",
      "security_privacy_impact": "None",
      "dependency_cost_rationale": "Zero cost",
      "pr": "123"
    }
  ]
}
EOF
if bash "$gate" "${tmp_dir}/bool_tokens.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure when wire_tokens is boolean" >&2
    exit 1
fi

echo "=== Test 13: Disallowed additional root property fails ==="
cat > "${tmp_dir}/extra_root_prop.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": 5000,
  "unauthorized_field": "bypass_attempt",
  "exceptions": []
}
EOF
if bash "$gate" "${tmp_dir}/extra_root_prop.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on disallowed root properties" >&2
    exit 1
fi

echo "=== Test 14: Disallowed additional entry property fails ==="
cat > "${tmp_dir}/extra_entry_prop.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": 5000,
  "exceptions": [
    {
      "owner": "maintainer1",
      "tool_name": "tool1",
      "rationale": "Rationale",
      "wire_tokens": 100,
      "sunset_decision": "Decision",
      "security_privacy_impact": "None",
      "dependency_cost_rationale": "Zero cost",
      "pr": "123",
      "unauthorized_entry_field": "leak"
    }
  ]
}
EOF
if bash "$gate" "${tmp_dir}/extra_entry_prop.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on disallowed entry properties" >&2
    exit 1
fi

echo "=== Test 15: Duplicate tool name fails ==="
cat > "${tmp_dir}/duplicate.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": 5000,
  "exceptions": [
    {
      "owner": "maintainer1",
      "tool_name": "duplicate_tool",
      "rationale": "Rationale 1",
      "wire_tokens": 100,
      "sunset_decision": "Decision",
      "security_privacy_impact": "None",
      "dependency_cost_rationale": "Zero cost",
      "pr": "123"
    },
    {
      "owner": "maintainer2",
      "tool_name": "duplicate_tool",
      "rationale": "Rationale 2",
      "wire_tokens": 200,
      "sunset_decision": "Decision",
      "security_privacy_impact": "None",
      "dependency_cost_rationale": "Zero cost",
      "pr": "124"
    }
  ]
}
EOF
if bash "$gate" "${tmp_dir}/duplicate.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on duplicate tool name exceptions" >&2
    exit 1
fi

echo "=== Test 16: Valid exception entry passes ==="
cat > "${tmp_dir}/valid_exc.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": 5000,
  "exceptions": [
    {
      "owner": "maintainer1",
      "tool_name": "essential_primitive",
      "rationale": "Kernel core residency proven in #213",
      "wire_tokens": 120,
      "sunset_decision": "Permanent retention",
      "security_privacy_impact": "Read-only workspace access, no credentials",
      "dependency_cost_rationale": "Zero new external dependencies",
      "pr": "262"
    }
  ]
}
EOF
bash "$gate" "${tmp_dir}/valid_exc.json"

echo "wire_budget_exception_gate self-tests passed cleanly."
exit 0
