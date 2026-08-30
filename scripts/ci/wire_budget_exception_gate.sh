#!/usr/bin/env bash

# Provider-wire token budget exception manifest gate.
# Validates scripts/ci/wire_budget_exceptions.json against schema and integrity rules:
# 1. Manifest file must exist and be valid JSON.
# 2. Schema version must equal 1, ceiling_tokens must equal 5000.
# 3. Every recorded exception must define owner, tool_name, rationale, wire_tokens, sunset_decision.
#
# Exit codes: 0 = clean, 1 = validation failure, 2 = fatal error.

set -euo pipefail

if ! repo_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo "FATAL: wire-budget exception gate must run inside a Git work tree." >&2
    exit 2
fi
cd "$repo_root"

manifest_file="${1:-scripts/ci/wire_budget_exceptions.json}"

if [ ! -f "$manifest_file" ]; then
    echo "FAIL: wire-budget exception manifest '$manifest_file' does not exist." >&2
    exit 1
fi

python3 - "$manifest_file" <<'PYEOF'
import json
import sys

manifest_path = sys.argv[1]

try:
    with open(manifest_path, 'r', encoding='utf-8') as f:
        data = json.load(f)
except Exception as e:
    print(f"FAIL: invalid JSON in manifest '{manifest_path}': {e}", file=sys.stderr)
    sys.exit(1)

if not isinstance(data, dict):
    print("FAIL: manifest root must be a JSON object", file=sys.stderr)
    sys.exit(1)

version = data.get("version")
if version != 1:
    print(f"FAIL: manifest version must be 1, got {version}", file=sys.stderr)
    sys.exit(1)

ceiling = data.get("ceiling_tokens")
if ceiling != 5000:
    print(f"FAIL: manifest ceiling_tokens must equal 5000 (owner-ratified ceiling), got {ceiling}", file=sys.stderr)
    sys.exit(1)

exceptions = data.get("exceptions")
if not isinstance(exceptions, list):
    print("FAIL: exceptions field must be a list", file=sys.stderr)
    sys.exit(1)

required_fields = ["owner", "tool_name", "rationale", "wire_tokens", "sunset_decision"]

for idx, exc in enumerate(exceptions):
    if not isinstance(exc, dict):
        print(f"FAIL: exception entry [{idx}] must be an object", file=sys.stderr)
        sys.exit(1)
    for field in required_fields:
        val = exc.get(field)
        if val is None or (isinstance(val, str) and not val.strip()):
            print(f"FAIL: exception entry [{idx}] missing or empty required field '{field}'", file=sys.stderr)
            sys.exit(1)
    if not isinstance(exc.get("wire_tokens"), int) or exc["wire_tokens"] <= 0:
        print(f"FAIL: exception entry [{idx}] wire_tokens must be a positive integer", file=sys.stderr)
        sys.exit(1)

print(f"wire_budget_exception_gate: valid manifest '{manifest_path}' ({len(exceptions)} active exceptions)")
sys.exit(0)
PYEOF
