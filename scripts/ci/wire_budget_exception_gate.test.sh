#!/usr/bin/env bash

# Test harness for wire_budget_exception_gate.sh.
# Asserts that:
# 1. Default scripts/ci/wire_budget_exceptions.json passes.
# 2. Missing manifest fails.
# 3. Invalid JSON fails.
# 4. Altered wire_budget_tokens_ceiling fails.
# 5. Exception entries missing required fields fail.
# 6. Exception entry with boolean wire_tokens fails.
# 7. Disallowed additional properties fail.
# 8. Duplicate exception tool entries fail.
# 9. Valid exception entry with all required fields passes.
#
# Exit status: 0 = all test assertions pass, nonzero = test failure.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
gate="${script_dir}/wire_budget_exception_gate.sh"

tmp_dir="$(mktemp -d)"
trap 'rm -rf "$tmp_dir"' EXIT

echo "=== Test 1: Production manifest passes ==="
bash "$gate"

echo "=== Test 2: Missing manifest fails ==="
if bash "$gate" "${tmp_dir}/nonexistent.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on missing manifest" >&2
    exit 1
fi

echo "=== Test 3: Invalid JSON fails ==="
echo "{ invalid json" > "${tmp_dir}/invalid.json"
if bash "$gate" "${tmp_dir}/invalid.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on invalid JSON" >&2
    exit 1
fi

echo "=== Test 4: Loosened ceiling fails ==="
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

echo "=== Test 5: Incomplete exception entry fails ==="
cat > "${tmp_dir}/incomplete.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": 5000,
  "exceptions": [
    {
      "owner": "alice",
      "tool_name": "custom_tool"
    }
  ]
}
EOF
if bash "$gate" "${tmp_dir}/incomplete.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on incomplete exception entry" >&2
    exit 1
fi

echo "=== Test 6: Boolean wire_tokens fails ==="
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

echo "=== Test 7: Disallowed additional property fails ==="
cat > "${tmp_dir}/extra_prop.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "wire_budget_tokens_ceiling": 5000,
  "unauthorized_field": "bypass_attempt",
  "exceptions": []
}
EOF
if bash "$gate" "${tmp_dir}/extra_prop.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure on disallowed root properties" >&2
    exit 1
fi

echo "=== Test 8: Duplicate tool name fails ==="
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

echo "=== Test 9: Valid exception entry passes ==="
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

