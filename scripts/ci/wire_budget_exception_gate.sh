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

if [ -n "$schema_file" ] && [ ! -f "$schema_file" ]; then
    echo "FAIL: schema file '$schema_file' does not exist." >&2
    exit 1
fi

python3 - "$manifest_file" "$schema_file" <<'PYEOF'
import json
import sys

manifest_path = sys.argv[1]
schema_path = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else ""

schema_data = None
if schema_path:
    try:
        with open(schema_path, 'r', encoding='utf-8') as f:
            schema_data = json.load(f)
    except Exception as e:
        print(f"FAIL: invalid JSON in schema file '{schema_path}': {e}", file=sys.stderr)
        sys.exit(1)

    if not isinstance(schema_data, dict) or schema_data.get("title") != "WireBudgetExceptions" or "properties" not in schema_data:
        print(f"FAIL: schema file '{schema_path}' is not a valid WireBudgetExceptions schema", file=sys.stderr)
        sys.exit(1)

try:
    with open(manifest_path, 'r', encoding='utf-8') as f:
        data = json.load(f)
except Exception as e:
    print(f"FAIL: invalid JSON in manifest '{manifest_path}': {e}", file=sys.stderr)
    sys.exit(1)

if not isinstance(data, dict):
    print("FAIL: manifest root must be a JSON object", file=sys.stderr)
    sys.exit(1)

if schema_data:
    allowed_root_keys = set(schema_data.get("properties", {}).keys())
    required_root_keys = list(schema_data.get("required", ["version", "wire_budget_tokens_ceiling", "exceptions"]))
    schema_version = schema_data["properties"]["version"]["const"]
    schema_ceiling = schema_data["properties"]["wire_budget_tokens_ceiling"]["const"]
    items_schema = schema_data["properties"]["exceptions"]["items"]
    required_fields = list(items_schema.get("required", []))
    allowed_entry_keys = set(items_schema.get("properties", {}).keys())
    min_wire_tokens = items_schema.get("properties", {}).get("wire_tokens", {}).get("minimum", 1)
else:
    allowed_root_keys = {"$schema", "version", "wire_budget_tokens_ceiling", "exceptions"}
    required_root_keys = ["version", "wire_budget_tokens_ceiling", "exceptions"]
    schema_version = 1
    schema_ceiling = 5000
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
    min_wire_tokens = 1

# Check root properties (additionalProperties: false)
extra_root = set(data.keys()) - allowed_root_keys
if extra_root:
    print(f"FAIL: disallowed additional properties at root: {sorted(extra_root)}", file=sys.stderr)
    sys.exit(1)

for req in required_root_keys:
    if req not in data:
        print(f"FAIL: manifest root missing required property '{req}'", file=sys.stderr)
        sys.exit(1)

if "$schema" in data:
    schema_val = data["$schema"]
    if type(schema_val) is not str or not schema_val.strip():
        print(f"FAIL: manifest '$schema' must be a non-empty string, got {schema_val!r}", file=sys.stderr)
        sys.exit(1)

version = data.get("version")
if type(version) is not int or type(version) is bool or version != schema_version:
    print(f"FAIL: manifest version must be integer {schema_version}, got {version}", file=sys.stderr)
    sys.exit(1)

ceiling = data.get("wire_budget_tokens_ceiling")
if type(ceiling) is not int or type(ceiling) is bool or ceiling != schema_ceiling:
    print(f"FAIL: manifest wire_budget_tokens_ceiling must equal {schema_ceiling} (owner-ratified ceiling), got {ceiling}", file=sys.stderr)
    sys.exit(1)

exceptions = data.get("exceptions")
if not isinstance(exceptions, list):
    print("FAIL: exceptions field must be a list", file=sys.stderr)
    sys.exit(1)

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
            if type(val) is not int or type(val) is bool or val < min_wire_tokens:
                print(f"FAIL: exception entry [{idx}] 'wire_tokens' must be an integer >= {min_wire_tokens}, got {val!r}", file=sys.stderr)
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
