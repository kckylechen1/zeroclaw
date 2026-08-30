#!/usr/bin/env bash

# Test harness for wire_budget_exception_gate.sh.
# Asserts that:
# 1. Default scripts/ci/wire_budget_exceptions.json passes.
# 2. Missing manifest fails.
# 3. Invalid JSON fails.
# 4. Altered ceiling_tokens fails.
# 5. Exception entries missing required fields fail.
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
  "ceiling_tokens": 6000,
  "exceptions": []
}
EOF
if bash "$gate" "${tmp_dir}/loosened.json" >/dev/null 2>&1; then
    echo "FAIL: Expected failure when ceiling_tokens is modified" >&2
    exit 1
fi

echo "=== Test 5: Incomplete exception entry fails ==="
cat > "${tmp_dir}/incomplete.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "ceiling_tokens": 5000,
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

echo "=== Test 6: Valid exception entry passes ==="
cat > "${tmp_dir}/valid_exc.json" << 'EOF'
{
  "$schema": "./wire_budget_exceptions.schema.json",
  "version": 1,
  "ceiling_tokens": 5000,
  "exceptions": [
    {
      "owner": "maintainer1",
      "tool_name": "essential_primitive",
      "rationale": "Kernel core residency proven in #213",
      "wire_tokens": 120,
      "sunset_decision": "Permanent retention"
    }
  ]
}
EOF
bash "$gate" "${tmp_dir}/valid_exc.json"

echo "wire_budget_exception_gate self-tests passed cleanly."
exit 0
