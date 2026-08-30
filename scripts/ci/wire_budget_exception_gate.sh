#!/usr/bin/env bash

# Provider-wire token budget exception manifest gate.
# Validates scripts/ci/wire_budget_exceptions.json against schema and integrity rules:
# 1. Manifest file must exist and be valid JSON.
# 2. Schema version must equal 1, wire_budget_tokens_ceiling must equal 5000.
# 3. No extra root or entry properties allowed (additionalProperties: false).
# 4. Every recorded exception must define: owner, tool_name, rationale, wire_tokens,
#    sunset_decision, security_privacy_impact, dependency_cost_rationale, pr.
# 5. wire_tokens must be an integer > 0 (strictly not boolean).
# 6. Tool names across exceptions must be distinct.
#
# Exit codes: 0 = clean, 1 = validation failure, 2 = fatal error.

set -euo pipefail

if ! repo_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo "FATAL: wire-budget exception gate must run inside a Git work tree." >&2
    exit 2
fi
cd "$repo_root"

manifest_file="${1:-scripts/ci/wire_budget_exceptions.json}"
schema_file="${2:-scripts/ci/wire_budget_exceptions.schema.json}"

if [ ! -f "$manifest_file" ]; then
    echo "FAIL: wire-budget exception manifest '$manifest_file' does not exist." >&2
    exit 1
fi

python3 - "$manifest_file" "$schema_file" <<'PYEOF'
import json
import sys

manifest_path = sys.argv[1]
schema_path = sys.argv[2] if len(sys.argv) > 2 else ""

try:
    with open(manifest_path, 'r', encoding='utf-8') as f:
        data = json.load(f)
except Exception as e:
    print(f"FAIL: invalid JSON in manifest '{manifest_path}': {e}", file=sys.stderr)
    sys.exit(1)

if not isinstance(data, dict):
    print("FAIL: manifest root must be a JSON object", file=sys.stderr)
    sys.exit(1)

# Check root properties (additionalProperties: false)
allowed_root_keys = {"$schema", "version", "wire_budget_tokens_ceiling", "exceptions"}
extra_root = set(data.keys()) - allowed_root_keys
if extra_root:
    print(f"FAIL: disallowed additional properties at root: {sorted(extra_root)}", file=sys.stderr)
    sys.exit(1)

version = data.get("version")
if type(version) is not int or version != 1:
    print(f"FAIL: manifest version must be integer 1, got {version}", file=sys.stderr)
    sys.exit(1)

ceiling = data.get("wire_budget_tokens_ceiling")
if type(ceiling) is not int or ceiling != 5000:
    print(f"FAIL: manifest wire_budget_tokens_ceiling must equal 5000 (owner-ratified ceiling), got {ceiling}", file=sys.stderr)
    sys.exit(1)

exceptions = data.get("exceptions")
if not isinstance(exceptions, list):
    print("FAIL: exceptions field must be a list", file=sys.stderr)
    sys.exit(1)

required_fields = [
    "owner",
    "tool_name",
    "rationale",
    "wire_tokens",
    "sunset_decision",
    "security_privacy_impact",
    "dependency_cost_rationale",
    "pr",
]
allowed_entry_keys = set(required_fields)
seen_tools = set()

for idx, exc in enumerate(exceptions):
    if not isinstance(exc, dict):
        print(f"FAIL: exception entry [{idx}] must be an object", file=sys.stderr)
        sys.exit(1)

    extra_entry = set(exc.keys()) - allowed_entry_keys
    if extra_entry:
        print(f"FAIL: exception entry [{idx}] has disallowed additional properties: {sorted(extra_entry)}", file=sys.stderr)
        sys.exit(1)

    for field in required_fields:
        if field not in exc:
            print(f"FAIL: exception entry [{idx}] missing required field '{field}'", file=sys.stderr)
            sys.exit(1)
        val = exc[field]
        if field == "wire_tokens":
            if type(val) is not int or type(val) is bool or val <= 0:
                print(f"FAIL: exception entry [{idx}] 'wire_tokens' must be a positive integer, got {val!r}", file=sys.stderr)
                sys.exit(1)
        else:
            if type(val) is not str or not val.strip():
                print(f"FAIL: exception entry [{idx}] '{field}' must be a non-empty string, got {val!r}", file=sys.stderr)
                sys.exit(1)

    tool_name = exc["tool_name"].strip()
    if tool_name in seen_tools:
        print(f"FAIL: duplicate exception entry for tool '{tool_name}'", file=sys.stderr)
        sys.exit(1)
    seen_tools.add(tool_name)

print(f"wire_budget_exception_gate: valid manifest '{manifest_path}' ({len(exceptions)} active exceptions)")
sys.exit(0)
PYEOF

