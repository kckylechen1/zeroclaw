#!/usr/bin/env bash

# Advisory exception lifecycle gate.
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
import datetime
import os

today = datetime.date.fromisoformat(os.environ["GATE_CURRENT_DATE"]) if "GATE_CURRENT_DATE" in os.environ else datetime.date.today()

audit_path = sys.argv[1]
deny_path = sys.argv[2]

RETIRED_ADVISORIES = {
    "RUSTSEC-2026-0268": "Cleared by wasmtime 47.0.4 update (#285, #296) - host heap allocation via WASIp3",
    "RUSTSEC-2026-0269": "Cleared by wasmtime 47.0.4 update (#285, #296) - filesystem sandbox escape",
}

class TomlArrayParser:
    """Robust parser for TOML [advisories].ignore array elements."""

    def __init__(self, text):
        self.text = text
        self.pos = 0
        self.length = len(text)

    def parse_advisories_ignore(self):
        sec_m = re.search(r'^[ \t]*\[advisories\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|$)', self.text, re.MULTILINE)
        if not sec_m:
            return None, "No [advisories] section found"

        start_sec = sec_m.end()
        next_sec_m = re.search(r'^[ \t]*\[[^\]]+\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|$)', self.text[start_sec:], re.MULTILINE)
        sec_end = start_sec + next_sec_m.start() if next_sec_m else self.length
        sec_text = self.text[start_sec:sec_end]

        # Locate ignore = [ within [advisories], skipping comments outside strings
        pos = 0
        in_quote = False
        quote_char = ''
        array_start = None
        while pos < len(sec_text):
            ch = sec_text[pos]
            if ch in ('"', "'"):
                if not in_quote:
                    in_quote = True
                    quote_char = ch
                elif ch == quote_char and (pos == 0 or sec_text[pos-1] != '\\'):
                    in_quote = False
                pos += 1
            elif ch == '#' and not in_quote:
                while pos < len(sec_text) and sec_text[pos] != '\n':
                    pos += 1
            elif not in_quote:
                if sec_text[pos:pos+6] == 'ignore':
                    prev_ch = sec_text[pos-1] if pos > 0 else '\n'
                    if not (prev_ch.isalnum() or prev_ch == '_'):
                        k = pos + 6
                        while k < len(sec_text) and sec_text[k] in ' \t\r\n':
                            k += 1
                        if k < len(sec_text) and sec_text[k] == '=':
                            k += 1
                            while k < len(sec_text) and sec_text[k] in ' \t\r\n':
                                k += 1
                            if k < len(sec_text) and sec_text[k] == '[':
                                array_start = start_sec + k + 1
                                break
                pos += 1
            else:
                pos += 1

        if array_start is None:
            return None, "No ignore = [ array found in [advisories] section"

        self.pos = array_start
        elements = []
        while self.pos < self.length:
            self._skip_ws_and_comments()
            if self.pos >= self.length:
                return None, "Unclosed array: EOF reached before ']'"

            if self.text[self.pos] == ']':
                self.pos += 1
                return elements, None

            elem, err = self._parse_element()
            if err:
                return None, err

            comment = self._grab_inline_comment()
            had_comma = False

            while self.pos < self.length and self.text[self.pos] in ' \t':
                self.pos += 1

            if self.pos < self.length and self.text[self.pos] == ',':
                had_comma = True
                self.pos += 1
                if not comment:
                    comment = self._grab_inline_comment()

            self._skip_ws_and_comments()
            if self.pos >= self.length:
                return None, "Unclosed array: EOF reached before ']'"

            if self.text[self.pos] == ']':
                elements.append((elem, comment))
                self.pos += 1
                return elements, None

            if not had_comma:
                if self.text[self.pos] == ',':
                    had_comma = True
                    self.pos += 1
                    self._skip_ws_and_comments()
                    if self.pos >= self.length:
                        return None, "Unclosed array: EOF reached before ']'"
                    if self.text[self.pos] == ']':
                        elements.append((elem, comment))
                        self.pos += 1
                        return elements, None
                else:
                    return None, f"Expected ',' between array elements, found '{self.text[self.pos]}'"

            elements.append((elem, comment))

        return None, "Unclosed array: EOF reached before ']'"

    def _skip_ws(self):
        while self.pos < self.length and self.text[self.pos] in ' \t\r\n':
            self.pos += 1

    def _skip_ws_and_comments(self):
        while self.pos < self.length:
            if self.text[self.pos] in ' \t\r\n':
                self.pos += 1
            elif self.text[self.pos] == '#':
                while self.pos < self.length and self.text[self.pos] != '\n':
                    self.pos += 1
            else:
                break

    def _grab_inline_comment(self):
        while self.pos < self.length and self.text[self.pos] in ' \t':
            self.pos += 1
        comment = ""
        if self.pos < self.length and self.text[self.pos] == '#':
            comm_start = self.pos + 1
            while self.pos < self.length and self.text[self.pos] not in '\r\n':
                self.pos += 1
            comment = self.text[comm_start:self.pos].strip()
        return comment

    def _parse_element(self):
        ch = self.text[self.pos]
        if ch in ('"', "'"):
            return self._parse_string()
        elif ch == '{':
            return self._parse_inline_table()
        else:
            start = self.pos
            while self.pos < self.length and self.text[self.pos] not in ',]\r\n#':
                self.pos += 1
            raw = self.text[start:self.pos].strip()
            return raw, None

    def _parse_string(self):
        quote_char = self.text[self.pos]
        self.pos += 1
        if quote_char == "'":
            end = self.text.find("'", self.pos)
            if end == -1:
                return None, "Unterminated literal string literal"
            val = self.text[self.pos:end]
            self.pos = end + 1
            return val, None

        res = []
        ESCAPE_MAP = {
            '"': '"',
            '\\': '\\',
            'b': '\b',
            't': '\t',
            'n': '\n',
            'f': '\f',
            'r': '\r',
        }
        while self.pos < self.length:
            ch = self.text[self.pos]
            if ch == '\\':
                self.pos += 1
                if self.pos >= self.length:
                    return None, "Unfinished escape sequence in string literal"
                esc = self.text[self.pos]
                if esc in ESCAPE_MAP:
                    res.append(ESCAPE_MAP[esc])
                    self.pos += 1
                elif esc == 'u':
                    self.pos += 1
                    if self.pos + 4 > self.length:
                        return None, "Incomplete \\u unicode escape"
                    hex_str = self.text[self.pos:self.pos+4]
                    if not all(c in '0123456789abcdefABCDEF' for c in hex_str):
                        return None, f"Invalid unicode escape \\u{hex_str}"
                    self.pos += 4
                    res.append(chr(int(hex_str, 16)))
                elif esc == 'U':
                    self.pos += 1
                    if self.pos + 8 > self.length:
                        return None, "Incomplete \\U unicode escape"
                    hex_str = self.text[self.pos:self.pos+8]
                    if not all(c in '0123456789abcdefABCDEF' for c in hex_str):
                        return None, f"Invalid unicode escape \\U{hex_str}"
                    self.pos += 8
                    try:
                        res.append(chr(int(hex_str, 16)))
                    except (ValueError, OverflowError):
                        return None, f"Invalid unicode codepoint \\U{hex_str}"
                else:
                    return None, f"Unknown escape sequence \\{esc} in string literal"
            elif ch == quote_char:
                self.pos += 1
                return "".join(res), None
            else:
                res.append(ch)
                self.pos += 1
        return None, "Unterminated string literal"

    def _parse_inline_table(self):
        self.pos += 1
        table = {}
        while self.pos < self.length:
            self._skip_ws_and_comments()
            if self.pos >= self.length:
                return None, "Unclosed inline table: EOF reached before '}'"
            ch = self.text[self.pos]
            if ch == '}':
                self.pos += 1
                return table, None

            key_start = self.pos
            while self.pos < self.length and self.text[self.pos] not in '= \t\r\n}':
                self.pos += 1
            key = self.text[key_start:self.pos].strip().strip('"\'')

            self._skip_ws()
            if self.pos >= self.length or self.text[self.pos] != '=':
                return None, f"Expected '=' after key '{key}' in inline table"
            self.pos += 1
            self._skip_ws()

            if self.pos >= self.length:
                return None, "Expected value after '=' in inline table"

            vch = self.text[self.pos]
            if vch in ('"', "'"):
                val, err = self._parse_string()
            else:
                vstart = self.pos
                while self.pos < self.length and self.text[self.pos] not in ',}\r\n#':
                    self.pos += 1
                val = self.text[vstart:self.pos].strip()
                err = None

            if err:
                return None, err
            table[key] = val

            self._skip_ws()
            if self.pos < self.length and self.text[self.pos] == ',':
                self.pos += 1
            elif self.pos < self.length and self.text[self.pos] == '}':
                self.pos += 1
                return table, None
        return None, "Unclosed inline table: EOF reached before '}'"


DISALLOWED_OWNERS = {
    "none", "null", "nil", "na", "n/a", "n_a", "n / a", "unassigned", "tbd",
    "to be determined", "to be decided", "todo", "to do", "placeholder",
    "nobody", "no body", "no one", "no-one", "no_one", "not assigned",
    "not yet assigned", "unknown", "assigned", "undefined", "anyone", "someone",
    "pending", "false", "empty", "blank", "missing", "unspecified", "no owner",
    "no maintainer", "not yet", "wontfix"
}

DISALLOWED_REVIEWS = {
    "none", "null", "nil", "na", "n/a", "n_a", "never", "no", "false",
    "unassigned", "tbd", "todo", "placeholder", "unknown", "undefined",
    "not needed", "not planned", "not required", "not applicable",
    "no review", "no review needed", "no review planned", "unnecessary",
    "wontfix", "won't fix", "n / a", "empty", "blank"
}

OWNER_FIELD_PATTERN = re.compile(
    r"\b(?:owner|maintainer)\b(?:\s*[:=]\s*|\s+@)([^;,]+)",
    re.IGNORECASE
)

BARE_HANDLE_PATTERN = re.compile(
    r"(?<!\w)@([a-zA-Z0-9_-]+)",
    re.IGNORECASE
)

TRACKING_PATTERN = re.compile(
    r"(?:\btracking\b\s*(?:issue\s*)?(?:#\d+|https?://\S+)|(?<!\w)#\d+\b)",
    re.IGNORECASE
)

UPSTREAM_PATTERN = re.compile(
    r"(?:"
    r"\b(?:transitive\s+via|pinned\s+(?:transitively\s+)?by|direct\s+dep)\s+[\w-]+"
    r"|\bawaiting\s+[\w-]+\s+upstream\b"
    r")",
    re.IGNORECASE
)

def has_accountable_owner(text):
    if TRACKING_PATTERN.search(text):
        return True
    if UPSTREAM_PATTERN.search(text):
        return True

    for m in OWNER_FIELD_PATTERN.finditer(text):
        start_idx = m.start()
        prefix = text[:start_idx]
        if re.search(r"\b(?:no|without|missing|unassigned|not)\s+$", prefix, re.IGNORECASE):
            continue
        raw_val = m.group(1).strip()
        clean_val = re.sub(r"\s+", " ", raw_val.lstrip("@")).strip().lower()
        if not clean_val:
            continue
        if clean_val in DISALLOWED_OWNERS:
            continue
        if re.match(r"^(?:no|not|without|missing|unassigned|none)\b", clean_val):
            continue
        if " " in clean_val:
            continue
        if not re.match(r"^[a-zA-Z0-9][a-zA-Z0-9_/-]*$", clean_val):
            continue
        return True

    for m in BARE_HANDLE_PATTERN.finditer(text):
        handle = m.group(1).lower()
        if handle not in DISALLOWED_OWNERS and len(handle) > 0:
            start_idx = m.start()
            prefix = text[:start_idx]
            if not re.search(r"\b(?:no|without|missing|not)\s+(?:an?\s+)?(?:owner|maintainer)?\s*[:=]?\s*$", prefix, re.IGNORECASE):
                return True

    return False

EXPIRY_DATE_PATTERN = re.compile(
    r"\b(?:expires?|expiry)\b\s*[:=]?\s*(\d{4}-\d{2}-\d{2})\b",
    re.IGNORECASE
)

REVIEW_CONDITION_PATTERN = re.compile(
    r"(?:\breview\b(?:\s+(?:by|due|before|at|on|date)\b\s*[:=]?|\s*[:=])|\brevisit\b\s+(?:when|after|on|at)\b\s+)([^;,]+)",
    re.IGNORECASE
)

LIFECYCLE_OTHER_PATTERN = re.compile(
    r"(?:"
    r"\bawaiting\b\s+(?:upstream|[\w-]+\s+upgrade|[\w-]+\s+migration|cleanup|migration|fix|upgrade)\b"
    r"|\b(?:upstream\s+)?fix\s+pending\b"
    r"|\b(?:fixed|patched)\b\s+(?:in|at|>=|>)\s*[\w.-]+"
    r"|\b(?:predates|outside\s+affected\s+range)\b"
    r"|\bno\s+compatible\s+fix\b(?:\s+in\s+[\w.-]+)?"
    r"|\binformational(?:\s+only)?(?:\s*,\s*no\s+cve|\s+advisory)\b"
    r")",
    re.IGNORECASE
)

def has_lifecycle_condition(text):
    for m in EXPIRY_DATE_PATTERN.finditer(text):
        prefix = text[:m.start()]
        if re.search(r"\b(?:no|not|without|never)\s+$", prefix, re.IGNORECASE):
            continue
        date_str = m.group(1)
        try:
            exp_date = datetime.date.fromisoformat(date_str)
        except ValueError:
            continue
        if exp_date < today:
            continue
        return True

    for m in REVIEW_CONDITION_PATTERN.finditer(text):
        prefix = text[:m.start()]
        if re.search(r"\b(?:no|not|without|never)\s+$", prefix, re.IGNORECASE):
            continue
        raw_cond = m.group(1).strip()
        clean_cond = re.sub(r"\s+", " ", raw_cond).lower()
        if not re.search(r"[a-zA-Z0-9]", clean_cond):
            continue
        if clean_cond in DISALLOWED_REVIEWS:
            continue
        if re.match(r"^(?:no|not|never|without)\b", clean_cond):
            continue
        date_m = re.search(r"\b(\d{4}-\d{2}-\d{2})\b", clean_cond)
        if date_m:
            try:
                rev_date = datetime.date.fromisoformat(date_m.group(1))
            except ValueError:
                continue
            if rev_date < today:
                continue
        return True

    for m in LIFECYCLE_OTHER_PATTERN.finditer(text):
        prefix = text[:m.start()]
        if re.search(r"\b(?:no|not|without|never)\s+$", prefix, re.IGNORECASE):
            continue
        return True

    return False

def validate_lifecycle(text):
    has_owner = has_accountable_owner(text)
    has_expiry = has_lifecycle_condition(text)
    return has_owner, has_expiry

errors = []

# --- 1. Validate deny.toml ---
try:
    with open(deny_path, "r", encoding="utf-8") as f:
        deny_content = f.read()
except Exception as e:
    print(f"FATAL: cannot read deny.toml: {e}", file=sys.stderr)
    sys.exit(2)

deny_parser = TomlArrayParser(deny_content)
deny_elements, parse_err = deny_parser.parse_advisories_ignore()
if parse_err:
    errors.append(f"deny.toml: Failed to parse [advisories].ignore: {parse_err}")
else:
    for elem, _ in deny_elements:
        if isinstance(elem, dict):
            adv_id = elem.get("id", "").strip()
            reason = elem.get("reason", "").strip()
            if not adv_id:
                errors.append(f"deny.toml: Inline table entry missing required 'id' key: {elem}")
                continue

            if adv_id in RETIRED_ADVISORIES:
                errors.append(f"deny.toml: Retired advisory '{adv_id}' is still present in deny.toml: {RETIRED_ADVISORIES[adv_id]}")

            if not reason:
                errors.append(f"deny.toml: Advisory '{adv_id}' missing 'reason' field with owner and review/expiry condition")
            else:
                has_owner, has_expiry = validate_lifecycle(reason)
                if not has_owner or not has_expiry:
                    errors.append(
                        f"deny.toml: Advisory '{adv_id}' reason '{reason}' lacks required lifecycle metadata: "
                        f"owner={'ok' if has_owner else 'MISSING'}, review/expiry={'ok' if has_expiry else 'MISSING'}"
                    )
        elif isinstance(elem, str):
            adv_id = elem.strip()
            if adv_id in RETIRED_ADVISORIES:
                errors.append(f"deny.toml: Retired advisory '{adv_id}' is still present in deny.toml: {RETIRED_ADVISORIES[adv_id]}")
            errors.append(f"deny.toml: Advisory '{adv_id}' specified as bare string; must be an inline table with 'id' and 'reason' enforcing owner and review/expiry lifecycle")
        else:
            errors.append(f"deny.toml: Malformed or unexpected advisory ignore element: {elem!r}")

# --- 2. Validate .cargo/audit.toml ---
try:
    with open(audit_path, "r", encoding="utf-8") as f:
        audit_content = f.read()
except Exception as e:
    print(f"FATAL: cannot read audit.toml: {e}", file=sys.stderr)
    sys.exit(2)

audit_parser = TomlArrayParser(audit_content)
audit_elements, parse_err = audit_parser.parse_advisories_ignore()
if parse_err:
    errors.append(f".cargo/audit.toml: Failed to parse [advisories].ignore: {parse_err}")
else:
    for elem, comment in audit_elements:
        if isinstance(elem, str):
            adv_id = elem.strip()
            if adv_id in RETIRED_ADVISORIES:
                errors.append(f".cargo/audit.toml: Retired advisory '{adv_id}' is still present in audit.toml: {RETIRED_ADVISORIES[adv_id]}")

            if not comment:
                errors.append(f".cargo/audit.toml: Advisory '{adv_id}' missing inline comment with owner and review/expiry condition")
            else:
                has_owner, has_expiry = validate_lifecycle(comment)
                if not has_owner or not has_expiry:
                    errors.append(
                        f".cargo/audit.toml: Advisory '{adv_id}' comment '{comment}' lacks required lifecycle metadata: "
                        f"owner={'ok' if has_owner else 'MISSING'}, review/expiry={'ok' if has_expiry else 'MISSING'}"
                    )
        else:
            errors.append(f".cargo/audit.toml: Expected string advisory entry, found {type(elem).__name__}: {elem!r}")

if errors:
    print("advisory-exceptions gate: FAIL", file=sys.stderr)
    for err in errors:
        print(f"  - {err}", file=sys.stderr)
    sys.exit(1)

print("advisory-exceptions gate: clean")
sys.exit(0)
PYEOF
