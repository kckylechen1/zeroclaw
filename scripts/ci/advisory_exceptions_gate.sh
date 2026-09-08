#!/usr/bin/env bash

# Advisory exception lifecycle gate (Issue #296).
# Validates .cargo/audit.toml and deny.toml:
# 1. Configuration files must exist and contain valid advisory ignore sections.
# 2. Resolved/retired exceptions (e.g. Wasmtime RUSTSEC-2026-0268, RUSTSEC-2026-0269)
#    must not remain in either file.
# 3. Every ignored advisory in deny.toml must define an explicit table with owner
#    and review/expiry lifecycle conditions (bare strings are rejected).
# 4. Every ignored advisory in .cargo/audit.toml must have an inline explanation
#    with owner and review/expiry lifecycle conditions.
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

RETIRED_ADVISORIES = {
    "RUSTSEC-2026-0268": "Cleared by wasmtime 47.0.4 update (#285, #296) - host heap allocation via WASIp3",
    "RUSTSEC-2026-0269": "Cleared by wasmtime 47.0.4 update (#285, #296) - filesystem sandbox escape",
}

def strip_comment(line):
    in_quote = False
    quote_char = ''
    for i, ch in enumerate(line):
        if ch in ('"', "'"):
            if not in_quote:
                in_quote = True
                quote_char = ch
            elif ch == quote_char and (i == 0 or line[i-1] != '\\'):
                in_quote = False
        elif ch == '#' and not in_quote:
            return line[:i].strip(), line[i+1:].strip()
    return line.strip(), ""

def validate_lifecycle(text):
    has_owner = bool(re.search(
        r'(?:tracking\s*#|#\d+|@[\w-]+|upstream|team|maintainer|zeroclaw|transitive|direct|probe-rs|tauri|webkit2gtk|glib|rumqttc|nostr-sdk|ratatui|rand|bincode|rustls|unic|wasmtime)',
        text, re.IGNORECASE
    ))
    has_expiry = bool(re.search(
        r'(?:awaiting|fixed|patched|predates|unmaintained|informational|upgrade|migration|pending|revisit|cleared|latest compatible|no compatible fix|no fix|outside)',
        text, re.IGNORECASE
    ))
    return has_owner, has_expiry

errors = []

# --- 1. Validate deny.toml ---
try:
    with open(deny_path, "r", encoding="utf-8") as f:
        deny_content = f.read()
except Exception as e:
    print(f"FATAL: cannot read deny.toml: {e}", file=sys.stderr)
    sys.exit(2)

adv_m = re.search(r'\[advisories\]\s*(?:[^\n]*\n)*?ignore\s*=\s*\[(.*?)\](?:\s*\n\s*\[|\s*\Z)', deny_content, re.DOTALL)
if not adv_m:
    errors.append("deny.toml: Could not locate valid [advisories].ignore array or array is unclosed")
else:
    raw_block = adv_m.group(1)
    for line in raw_block.splitlines():
        line_code, _ = strip_comment(line)
        if not line_code:
            continue
        
        # Check for bare string ignores (e.g. "RUSTSEC-...")
        bare_m = re.search(r'^"(RUSTSEC-[^"]+)"', line_code)
        if bare_m:
            adv_id = bare_m.group(1)
            if adv_id in RETIRED_ADVISORIES:
                errors.append(f"deny.toml: Retired advisory '{adv_id}' is still present in deny.toml: {RETIRED_ADVISORIES[adv_id]}")
            errors.append(f"deny.toml: Advisory '{adv_id}' specified as bare string; must be an inline table with 'id' and 'reason' enforcing owner and review/expiry lifecycle")
            continue

        # Check for inline table ignores (e.g. { id = "...", reason = "..." })
        tbl_m = re.search(r'\{\s*id\s*=\s*"([^"]+)"(?:,\s*reason\s*=\s*"([^"]*)")?\s*\}', line_code)
        if tbl_m:
            adv_id = tbl_m.group(1)
            reason = tbl_m.group(2) or ""
            if adv_id in RETIRED_ADVISORIES:
                errors.append(f"deny.toml: Retired advisory '{adv_id}' is still present in deny.toml: {RETIRED_ADVISORIES[adv_id]}")
            if not reason.strip():
                errors.append(f"deny.toml: Advisory '{adv_id}' missing reason field with owner and review/expiry condition")
            else:
                has_owner, has_expiry = validate_lifecycle(reason)
                if not has_owner or not has_expiry:
                    errors.append(
                        f"deny.toml: Advisory '{adv_id}' reason '{reason}' lacks required lifecycle fields: "
                        f"owner={'ok' if has_owner else 'MISSING'}, review/expiry={'ok' if has_expiry else 'MISSING'}"
                    )
        elif "RUSTSEC-" in line_code:
            errors.append(f"deny.toml: Malformed or unparseable advisory entry in ignore list: '{line_code}'")

# --- 2. Validate .cargo/audit.toml ---
try:
    with open(audit_path, "r", encoding="utf-8") as f:
        audit_content = f.read()
except Exception as e:
    print(f"FATAL: cannot read audit.toml: {e}", file=sys.stderr)
    sys.exit(2)

audit_m = re.search(r'\[advisories\]\s*(?:[^\n]*\n)*?ignore\s*=\s*\[(.*?)\](?:\s*\n\s*\[|\s*\Z)', audit_content, re.DOTALL)
if not audit_m:
    errors.append(".cargo/audit.toml: Could not locate valid [advisories].ignore array or array is unclosed")
else:
    raw_block = audit_m.group(1)
    for line in raw_block.splitlines():
        line_code, line_comment = strip_comment(line)
        if not line_code:
            continue
        
        id_m = re.search(r'"(RUSTSEC-[^"]+)"', line_code)
        if id_m:
            adv_id = id_m.group(1)
            if adv_id in RETIRED_ADVISORIES:
                errors.append(f".cargo/audit.toml: Retired advisory '{adv_id}' is still present in audit.toml: {RETIRED_ADVISORIES[adv_id]}")
            
            if not line_comment:
                errors.append(f".cargo/audit.toml: Advisory '{adv_id}' missing inline comment with owner and review/expiry condition")
            else:
                has_owner, has_expiry = validate_lifecycle(line_comment)
                if not has_owner or not has_expiry:
                    errors.append(
                        f".cargo/audit.toml: Advisory '{adv_id}' comment '{line_comment}' lacks required lifecycle fields: "
                        f"owner={'ok' if has_owner else 'MISSING'}, review/expiry={'ok' if has_expiry else 'MISSING'}"
                    )
        elif "RUSTSEC-" in line_code:
            errors.append(f".cargo/audit.toml: Malformed or unparseable advisory entry in ignore list: '{line_code}'")

if errors:
    print("advisory-exceptions gate: FAIL", file=sys.stderr)
    for err in errors:
        print(f"  - {err}", file=sys.stderr)
    sys.exit(1)

print("advisory-exceptions gate: clean")
sys.exit(0)
PYEOF
