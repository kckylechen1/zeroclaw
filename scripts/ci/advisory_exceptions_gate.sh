#!/usr/bin/env bash

# Advisory exception lifecycle gate (Issue #296).
# Validates .cargo/audit.toml and deny.toml:
# 1. Configuration files must exist and be parseable.
# 2. Resolved/retired exceptions (e.g. Wasmtime RUSTSEC-2026-0268, RUSTSEC-2026-0269)
#    must not remain in either file.
# 3. Every ignored advisory in deny.toml must define an explicit non-empty reason.
# 4. Every ignored advisory in .cargo/audit.toml must have an inline explanation.
#
# Exit status: 0 = clean, 1 = validation failure, 2 = fatal error.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="${REPO_ROOT:-$(git -C "$script_dir" rev-parse --show-toplevel 2>/dev/null || printf '%s' "$script_dir/../..")}"

audit_toml="${AUDIT_TOML:-$repo_root/.cargo/audit.toml}"
deny_toml="${DENY_TOML:-$repo_root/deny.toml}"

if [[ ! -f "$audit_toml" ]]; then
    echo "FATAL: audit config not found at $audit_toml" >&2
    exit 2
fi

if [[ ! -f "$deny_toml" ]]; then
    echo "FATAL: deny config not found at $deny_toml" >&2
    exit 2
fi

python3 - "$audit_toml" "$deny_toml" <<'PYEOF'
import sys
import re

audit_path = sys.argv[1]
deny_path = sys.argv[2]

# Explicitly retired / resolved advisories that must not be re-introduced
# without documented justification and lifecycle owner.
RETIRED_ADVISORIES = {
    "RUSTSEC-2026-0268": "Cleared by wasmtime 47.0.4 update (#285, #296)",
    "RUSTSEC-2026-0269": "Cleared by wasmtime 47.0.4 update (#285, #296)",
}

errors = []

# --- 1. Check deny.toml ---
try:
    with open(deny_path, "r", encoding="utf-8") as f:
        deny_lines = f.readlines()
except Exception as e:
    print(f"FATAL: cannot read deny.toml: {e}", file=sys.stderr)
    sys.exit(2)

in_advisories_ignore = False
for idx, line in enumerate(deny_lines, 1):
    stripped = line.strip()
    if stripped.startswith("[advisories]"):
        in_advisories_ignore = False
    elif stripped.startswith("ignore = ["):
        in_advisories_ignore = True
        continue
    elif in_advisories_ignore and stripped.startswith("]"):
        in_advisories_ignore = False
        continue

    if in_advisories_ignore:
        # Match entries like: { id = "RUSTSEC-...", reason = "..." }
        m = re.search(r'id\s*=\s*"([^"]+)"', line)
        if m:
            adv_id = m.group(1)
            if adv_id in RETIRED_ADVISORIES:
                errors.append(
                    f"deny.toml:{idx}: Retired advisory '{adv_id}' is still present in deny.toml ignore list. "
                    f"Disposition: {RETIRED_ADVISORIES[adv_id]}"
                )

            # Check reason field exists and is descriptive
            r = re.search(r'reason\s*=\s*"([^"]*)"', line)
            if not r or len(r.group(1).strip()) < 8:
                errors.append(
                    f"deny.toml:{idx}: Advisory '{adv_id}' must specify an explicit, descriptive reason in deny.toml."
                )

# --- 2. Check .cargo/audit.toml ---
try:
    with open(audit_path, "r", encoding="utf-8") as f:
        audit_lines = f.readlines()
except Exception as e:
    print(f"FATAL: cannot read audit.toml: {e}", file=sys.stderr)
    sys.exit(2)

in_audit_ignore = False
for idx, line in enumerate(audit_lines, 1):
    stripped = line.strip()
    if stripped.startswith("[advisories]"):
        in_audit_ignore = False
    elif stripped.startswith("ignore = ["):
        in_audit_ignore = True
        continue
    elif in_audit_ignore and stripped.startswith("]"):
        in_audit_ignore = False
        continue

    if in_audit_ignore:
        # Match entries like: "RUSTSEC-..."
        m = re.search(r'"(RUSTSEC-[^"]+)"', line)
        if m:
            adv_id = m.group(1)
            if adv_id in RETIRED_ADVISORIES:
                errors.append(
                    f".cargo/audit.toml:{idx}: Retired advisory '{adv_id}' is still present in audit.toml ignore list. "
                    f"Disposition: {RETIRED_ADVISORIES[adv_id]}"
                )

            # Check for trailing inline comment explaining the ignore
            if "#" not in line or len(line.split("#", 1)[1].strip()) < 5:
                errors.append(
                    f".cargo/audit.toml:{idx}: Advisory '{adv_id}' must have an inline comment explaining its rationale/owner."
                )

if errors:
    print("advisory-exceptions gate: FAIL", file=sys.stderr)
    for err in errors:
        print(f"  - {err}", file=sys.stderr)
    sys.exit(1)

print("advisory-exceptions gate: clean")
sys.exit(0)
PYEOF
chmod +x scripts/ci/advisory_exceptions_gate.sh