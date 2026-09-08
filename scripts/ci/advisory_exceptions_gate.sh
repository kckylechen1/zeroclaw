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

if [ -z "${BASE_REF:-}" ]; then
    if [ -n "${GITHUB_EVENT_PATH:-}" ] && [ -f "$GITHUB_EVENT_PATH" ]; then
        event_base_sha=$(python3 -c "import json, sys; d=json.load(open(sys.argv[1])); print(d.get('pull_request', {}).get('base', {}).get('sha', ''))" "$GITHUB_EVENT_PATH" 2>/dev/null || true)
        if [ -n "$event_base_sha" ] && git rev-parse --verify "$event_base_sha" >/dev/null 2>&1; then
            BASE_REF="$event_base_sha"
        fi
    fi
    if [ -z "${BASE_REF:-}" ] && [ -n "${GITHUB_BASE_REF:-}" ]; then
        if git rev-parse --verify "origin/$GITHUB_BASE_REF" >/dev/null 2>&1; then
            BASE_REF="origin/$GITHUB_BASE_REF"
        elif git rev-parse --verify "$GITHUB_BASE_REF" >/dev/null 2>&1; then
            BASE_REF="$GITHUB_BASE_REF"
        fi
    fi
    if [ -z "${BASE_REF:-}" ]; then
        if git rev-parse --verify origin/master >/dev/null 2>&1; then
            BASE_REF="origin/master"
        elif git rev-parse --verify master >/dev/null 2>&1; then
            BASE_REF="master"
        else
            BASE_REF=""
        fi
    fi
fi
if [ -n "${BASE_REF:-}" ] && ! git rev-parse --verify "$BASE_REF" >/dev/null 2>&1; then
    BASE_REF=""
fi
export BASE_REF

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
        # Match valid TOML table header: [advisories], [ advisories ], ["advisories"], ['advisories']
        sec_m = re.search(
            r'^[ \t]*\[[ \t]*(?:"advisories"|\'advisories\'|advisories)[ \t]*\][ \t]*(?:#[^\r\n]*)?(?:\r?\n|$)',
            self.text,
            re.MULTILINE
        )
        if not sec_m:
            return None, "No [advisories] section found"

        start_sec = sec_m.end()
        next_sec_m = re.search(
            r'^[ \t]*\[+[ \t]*(?:[^\]\r\n]+)[ \t]*\]+[ \t]*(?:#[^\r\n]*)?(?:\r?\n|$)',
            self.text[start_sec:],
            re.MULTILINE
        )
        sec_end = start_sec + next_sec_m.start() if next_sec_m else self.length
        sec_text = self.text[start_sec:sec_end]

        # Locate ignore = [ within [advisories], skipping comments outside strings.
        # Key can be bare: ignore, or quoted: "ignore" or 'ignore'.
        pos = 0
        in_quote = False
        quote_char = ''
        array_start = None
        while pos < len(sec_text):
            ch = sec_text[pos]
            if ch == '#' and not in_quote:
                while pos < len(sec_text) and sec_text[pos] != '\n':
                    pos += 1
                continue

            if in_quote:
                if ch == quote_char and (pos == 0 or sec_text[pos-1] != '\\'):
                    in_quote = False
                pos += 1
                continue

            matched_key_len = 0
            if sec_text[pos:pos+8] == '"ignore"' or sec_text[pos:pos+8] == "'ignore'":
                matched_key_len = 8
            elif sec_text[pos:pos+6] == 'ignore':
                prev_ch = sec_text[pos-1] if pos > 0 else '\n'
                if not (prev_ch.isalnum() or prev_ch in '_-'):
                    matched_key_len = 6

            if matched_key_len > 0:
                k = pos + matched_key_len
                while k < len(sec_text) and sec_text[k] in ' \t\r\n':
                    k += 1
                if k < len(sec_text) and sec_text[k] == '=':
                    k += 1
                    while k < len(sec_text) and sec_text[k] in ' \t\r\n':
                        k += 1
                    if k < len(sec_text) and sec_text[k] == '[':
                        array_start = start_sec + k + 1
                        break

            if ch in ('"', "'"):
                in_quote = True
                quote_char = ch
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
        if self.text[self.pos:self.pos+3] == "'''":
            self.pos += 3
            if self.pos < self.length and self.text[self.pos] == '\n':
                self.pos += 1
            elif self.pos + 1 < self.length and self.text[self.pos:self.pos+2] == '\r\n':
                self.pos += 2
            res = []
            while self.pos < self.length:
                ch = self.text[self.pos]
                if ch == "'":
                    qcount = 0
                    qpos = self.pos
                    while qpos < self.length and self.text[qpos] == "'":
                        qcount += 1
                        qpos += 1
                    if qcount >= 3:
                        if qcount == 3:
                            self.pos += 3
                            return "".join(res), None
                        elif qcount == 4:
                            res.append("'")
                            self.pos += 4
                            return "".join(res), None
                        elif qcount == 5:
                            res.append("''")
                            self.pos += 5
                            return "".join(res), None
                        else:
                            return None, "Invalid run of 6 or more single quotes in multiline literal string"
                    else:
                        res.append(self.text[self.pos:self.pos+qcount])
                        self.pos += qcount
                else:
                    res.append(ch)
                    self.pos += 1
            return None, "Unterminated multiline literal string (missing ''')"

        if self.text[self.pos:self.pos+3] == '"""':
            self.pos += 3
            if self.pos < self.length and self.text[self.pos] == '\n':
                self.pos += 1
            elif self.pos + 1 < self.length and self.text[self.pos:self.pos+2] == '\r\n':
                self.pos += 2
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
                        return None, "Unfinished escape sequence in multiline string"
                    next_ch = self.text[self.pos]
                    if next_ch in ('\r', '\n'):
                        if next_ch == '\r' and self.pos + 1 < self.length and self.text[self.pos+1] == '\n':
                            self.pos += 2
                        else:
                            self.pos += 1
                        while self.pos < self.length and self.text[self.pos] in ' \t\r\n':
                            self.pos += 1
                        continue
                    elif next_ch in ESCAPE_MAP:
                        res.append(ESCAPE_MAP[next_ch])
                        self.pos += 1
                    elif next_ch == 'u':
                        self.pos += 1
                        if self.pos + 4 > self.length:
                            return None, "Incomplete \\u unicode escape"
                        hex_str = self.text[self.pos:self.pos+4]
                        if not all(c in '0123456789abcdefABCDEF' for c in hex_str):
                            return None, f"Invalid unicode escape \\u{hex_str}"
                        self.pos += 4
                        res.append(chr(int(hex_str, 16)))
                    elif next_ch == 'U':
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
                        return None, f"Unknown escape sequence \\{next_ch} in multiline string"
                elif ch == '"':
                    qcount = 0
                    qpos = self.pos
                    while qpos < self.length and self.text[qpos] == '"':
                        qcount += 1
                        qpos += 1
                    if qcount >= 3:
                        if qcount == 3:
                            self.pos += 3
                            return "".join(res), None
                        elif qcount == 4:
                            res.append('"')
                            self.pos += 4
                            return "".join(res), None
                        elif qcount == 5:
                            res.append('""')
                            self.pos += 5
                            return "".join(res), None
                        else:
                            return None, "Invalid run of 6 or more quotation marks in multiline string"
                    else:
                        res.append(self.text[self.pos:self.pos+qcount])
                        self.pos += qcount
                else:
                    res.append(ch)
                    self.pos += 1
            return None, "Unterminated multiline string (missing \"\"\")"

        quote_char = self.text[self.pos]
        self.pos += 1
        if quote_char == "'":
            end = self.text.find("'", self.pos)
            if end == -1:
                return None, "Unterminated literal string literal"
            val = self.text[self.pos:end]
            if '\n' in val or '\r' in val:
                return None, "Newline in single-line literal string literal"
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
            if ch in ('\r', '\n'):
                return None, "Newline in single-line string literal"
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
            if self.pos >= self.length:
                return None, "Unclosed inline table: EOF reached before '}'"
            if self.text[self.pos] == ',':
                self.pos += 1
            elif self.text[self.pos] == '}':
                self.pos += 1
                return table, None
            else:
                return None, f"Expected ',' or '}}' after value in inline table, found '{self.text[self.pos]}'"
        return None, "Unclosed inline table: EOF reached before '}'"


DISALLOWED_OWNERS = {
    "none", "null", "nil", "na", "n/a", "n_a", "n / a", "unassigned", "tbd",
    "to be determined", "to be decided", "todo", "to do", "placeholder",
    "nobody", "no body", "no one", "no-one", "no_one", "not assigned",
    "not yet assigned", "unknown", "assigned", "undefined", "anyone", "someone",
    "pending", "false", "empty", "blank", "missing", "unspecified", "no owner",
    "no maintainer", "not yet", "wontfix", "upstream", "transitive", "dep"
}

DISALLOWED_UPSTREAM_WORDS = {
    # Lifecycle & action nouns
    "awaiting", "fix", "fixes", "upgrade", "upgrades", "migration", "migrations",
    "cleanup", "cleanups", "release", "releases", "patch", "patches", "update",
    "updates", "upstream", "transitive", "dep", "deps", "dependency", "dependencies",
    "crate", "crates", "package", "packages", "direct", "pinned", "transitively",
    "via", "issue", "issues", "pr", "prs", "ticket", "tickets", "repo", "repository",
    "todo", "tbd", "tba", "none", "unknown", "placeholder", "unassigned", "undefined",
    "missing", "na", "n/a", "version", "versions", "target", "targets", "series",
    # English stop words / determiners / pronouns
    "a", "an", "the", "this", "that", "these", "those", "all", "any", "some", "each",
    "every", "no", "not", "such", "other", "another", "one", "two",
    "it", "its", "our", "ours", "their", "theirs", "my", "your", "we", "us", "they",
    "them", "who", "which", "what", "someone", "anyone", "everyone", "nobody",
    # Verbs & auxiliaries
    "is", "are", "was", "were", "be", "been", "being", "has", "have", "had",
    "do", "does", "did", "done", "get", "gets", "got", "make", "makes", "made",
    "can", "could", "will", "would", "shall", "should", "may", "might", "must",
    # Prepositions & conjunctions
    "in", "on", "at", "to", "for", "of", "with", "by", "from", "into", "through",
    "about", "above", "over", "under", "between", "among", "and", "or", "but",
    "if", "then", "else", "when", "where", "why", "how", "as", "so", "because",
    "since", "while",
    # Prose adjectives
    "affected", "vulnerable", "broken", "clean", "new", "old", "current", "next",
    "latest", "sound", "unsound"
}

DISALLOWED_REVIEWS = {
    "none", "null", "nil", "na", "n/a", "n_a", "never", "no", "false",
    "unassigned", "tbd", "tba", "todo", "to do", "to be determined", "to be decided",
    "placeholder", "unknown", "undefined", "unspecified", "missing",
    "not needed", "not planned", "not required", "not applicable",
    "no review", "no review needed", "no review planned", "unnecessary",
    "wontfix", "won't fix", "n / a", "empty", "blank",
    "pending", "in progress", "in-progress", "open", "ongoing",
    "fixed", "patched", "resolved", "closed", "later", "soon", "future",
    "completed", "done", "finished", "passed", "approved", "obsolete", "retired"
}

OWNER_FIELD_PATTERN = re.compile(
    r"\b(?:owner|maintainer)\b(?:\s*[:=]\s*|\s+@)([^;,]+)",
    re.IGNORECASE
)

BARE_HANDLE_PATTERN = re.compile(
    r"(?<!\w)@([a-zA-Z0-9][a-zA-Z0-9_-]*)",
    re.IGNORECASE
)

TRACKING_PATTERN = re.compile(
    r"(?:"
    r"\btracking\b(?:\s+(?:upstream|local|repo|issue|pr|ticket))*\s*[:=]?\s*(?:#[1-9]\d*(?!\w)|https?://[a-zA-Z0-9_.-]+(?:/[a-zA-Z0-9_./#?=&%-]*)?)"
    r"|\b(?:upstream|local)\s+(?:issue\s+|pr\s+|ticket\s+)?#[1-9]\d*(?!\w)"
    r"|\b[a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+#[1-9]\d*(?!\w)"
    r")",
    re.IGNORECASE
)

def normalize_contractions(s):
    s = re.sub(r"\bwon['’]t\b", "will not", s, flags=re.IGNORECASE)
    s = re.sub(r"\bcan['’]t\b", "can not", s, flags=re.IGNORECASE)
    s = re.sub(r"\bshan['’]t\b", "shall not", s, flags=re.IGNORECASE)
    s = re.sub(r"\b(\w+)n['’]t\b", r"\1 not", s, flags=re.IGNORECASE)
    s = re.sub(r"\bcannot\b", "can not", s, flags=re.IGNORECASE)
    return s

def is_negated_prefix(prefix):
    clause_prefix = re.split(r"[;,\.\n]", prefix)[-1]
    clause_prefix = normalize_contractions(clause_prefix)
    return bool(re.search(r"\b(?:no|not|without|never|missing|unassigned|former|previous|past|prior|ex|old)\b", clause_prefix, re.IGNORECASE))

def get_owner_clause_spans(text):
    spans = []
    for m in re.finditer(r"\b(?:owner|maintainer)\b", text, re.IGNORECASE):
        start = 0
        for delim in re.finditer(r"[;,\n]", text[:m.start()]):
            start = delim.end()
        next_delim = re.search(r"[;,\n]", text[m.end():])
        if next_delim:
            end = m.end() + next_delim.start()
        else:
            end = len(text)
        spans.append((start, end))
    return spans

def has_accountable_owner(text):
    for m in OWNER_FIELD_PATTERN.finditer(text):
        if is_negated_prefix(text[:m.start()]):
            continue
        raw_val = m.group(1).strip()
        clean_val = re.sub(r"\s+", " ", raw_val.lstrip("@")).strip().lower()
        if not clean_val:
            continue
        if clean_val in DISALLOWED_OWNERS:
            continue
        if re.match(r"^(?:no|not|without|missing|unassigned|none|tbd|tba|todo|placeholder|unknown|undefined)(?:$|[-_]|\b)", clean_val):
            continue
        if " " in clean_val:
            continue
        if not re.match(r"^[a-zA-Z0-9][a-zA-Z0-9_/-]*$", clean_val):
            continue
        return True

    owner_clause_spans = get_owner_clause_spans(text)
    for m in BARE_HANDLE_PATTERN.finditer(text):
        if any(start <= m.start() < end for start, end in owner_clause_spans):
            continue
        if is_negated_prefix(text[:m.start()]):
            continue
        handle = m.group(1).lower()
        if not handle or handle in DISALLOWED_OWNERS:
            continue
        if not re.match(r"^[a-zA-Z0-9][a-zA-Z0-9_/-]*$", handle):
            continue
        if re.match(r"^(?:no|not|without|missing|unassigned|none|tbd|tba|todo|placeholder|unknown|undefined)(?:$|[-_]|\b)", handle):
            continue
        return True

    return False


EXPIRY_FIELD_PATTERN = re.compile(
    r"\b(?:expires?|expiry|expired)(?:(?:\s+(?:on|at|by|date))\b)?(?:(?:\s*[:=]\s*([^;,]*))|(?:\s+(?=(?:\d{4}-\d{2}-\d{2}|tbd|tba|todo|none|never|unknown|undefined|placeholder|no|not|without)\b)([^;,]*)))",
    re.IGNORECASE
)

REVIEW_CONDITION_PATTERN = re.compile(
    r"(?:\breview\b(?:\s+(?:by|due|before|at|on|date)\b\s*[:=]?|\s*[:=])|\brevisit\b\s+(?:when|after|on|at)\b\s+)([^;\n]+?)(?=(?:,\s*(?:owner|maintainer|tracking|scope|tool|expires?|expiry)\b|[;\n]|$))",
    re.IGNORECASE
)

LIFECYCLE_OTHER_PATTERN = re.compile(
    r"(?:"
    r"\bawaiting\b\s+(?:(?!(?:no|not|without|never)\b)[\w-]+\s+)*(?:migration|upgrade|fix|cleanup|upstream)\b"
    r"|\b(?:upstream\s+)?fix\s+pending\b"
    r"|\bno\s+compatible\s+fix\b(?:\s+in\s+[\w.-]+)?"
    r")",
    re.IGNORECASE
)

def has_lifecycle_condition(text):
    has_valid_expiry = False
    has_valid_review = False
    has_valid_milestone = False

    # 1. Validate every explicit expiry declaration.
    # Every declared deadline must be a valid, unexpired calendar date.
    for m in EXPIRY_FIELD_PATTERN.finditer(text):
        if is_negated_prefix(text[:m.start()]):
            continue
        raw_val = (m.group(1) or m.group(2) or "").strip()
        if not raw_val:
            return False, "invalid or empty expiry deadline"
        clean_val = re.sub(r"^[\s\"']+|[\s\"']+$", "", raw_val).strip()
        if re.search(r"\b(?:no|not|never|without|none|tbd|tba|todo|placeholder|unknown|undefined)\b", clean_val, re.IGNORECASE):
            return False, f"placeholder or negated expiry deadline '{raw_val}'"
        date_m = re.match(r"^(\d{4}-\d{2}-\d{2})$", clean_val)
        if not date_m:
            return False, f"invalid expiry deadline '{raw_val}'"
        date_str = date_m.group(1)
        try:
            exp_date = datetime.date.fromisoformat(date_str)
        except ValueError:
            return False, f"invalid calendar date '{date_str}' in expiry deadline"
        if exp_date < today:
            return False, f"expired on {date_str} (current date is {today.isoformat()})"
        has_valid_expiry = True

    # 2. Validate every explicit review declaration.
    # Reject placeholder, resolved-status, or expired review deadlines.
    for m in REVIEW_CONDITION_PATTERN.finditer(text):
        if is_negated_prefix(text[:m.start()]):
            continue
        raw_cond = m.group(1).strip()
        clean_cond = re.sub(r"\s+", " ", raw_cond).lower()
        if not re.search(r"[a-zA-Z0-9]", clean_cond):
            return False, f"empty review condition '{raw_cond}'"

        norm_cond = re.sub(r"^[\s\(\[\{\"\'`\-]+", "", clean_cond).strip()
        norm_cond = re.sub(r"[\s\)\]\}\"\'`\-]+$", "", norm_cond).strip()

        if norm_cond in DISALLOWED_REVIEWS or clean_cond in DISALLOWED_REVIEWS:
            return False, f"placeholder or invalid review condition '{raw_cond}'"

        norm_cond_norm = normalize_contractions(norm_cond)
        if re.search(r"\b(?:no|not|never|without|none|tbd|tba|todo|placeholder|unknown|undefined|unassigned|wontfix|obsolete|retired)\b", norm_cond_norm, re.IGNORECASE):
            return False, f"placeholder or negated review condition '{raw_cond}'"

        if re.match(r"^(?:pending|open|in\s+progress|later|soon|future|closed|completed|done|finished|passed|approved)\b", norm_cond):
            return False, f"placeholder or invalid review condition '{raw_cond}'"
        if re.search(r"#0+\b", norm_cond):
            return False, f"invalid zero-valued issue reference in review condition '{raw_cond}'"
        if re.search(r"#[0-9]+[a-zA-Z_]", norm_cond):
            return False, f"malformed issue reference in review condition '{raw_cond}'"
        dates = re.findall(r"\b(\d{4}-\d{2}-\d{2})\b", clean_cond)
        if dates:
            for date_str in dates:
                try:
                    rev_date = datetime.date.fromisoformat(date_str)
                except ValueError:
                    return False, f"invalid calendar date '{date_str}' in review condition"
                if rev_date < today:
                    return False, f"review date expired on {date_str} (current date is {today.isoformat()})"
            has_valid_review = True
            continue

        # Check if it's a version condition: must match semver/version pattern and contain digits
        is_version = False
        version_pattern = re.compile(
            r"^(?:(?:[><=^~]=?|\bversion\b|\bv\b)\s*)?v?\d+(?:\.[0-9a-zA-Z*_-]+)*(?:\s*,\s*(?:[><=^~]=?\s*)?v?\d+(?:\.[0-9a-zA-Z*_-]+)*)*$"
        )
        if version_pattern.match(norm_cond):
            if not re.search(r"\b(?:tbd|tba|todo|placeholder|unknown|none|undefined)\b", norm_cond, re.IGNORECASE):
                is_version = True

        is_cadence = bool(re.match(
            r"^(?:(?:every|each|on\s+a)\s+)?(?:quarterly|monthly|weekly|bi-weekly|semi-annually|annually|daily|quarter|month|week|year)(?:\s+(?:review|cadence|basis|cycle))?$",
            norm_cond
        ))
        is_tracker = bool(re.match(
            r"^(?:(?:on|upon|via|in|at)\s+)?(?:(?:upstream|local|repo|issue|pr|ticket)\s+)?(?:#[1-9]\d*(?!\w)|https?://[a-zA-Z0-9_.-]+(?:/[a-zA-Z0-9_./#?=&%-]*)?)$",
            norm_cond
        ))

        # Check if it's an actionable milestone condition: requires content after milestone word
        is_milestone = False
        milestone_m = re.match(r"^(?:on|at|after|when|upon|before|with|by|until)\s+(.+)$", norm_cond)
        if milestone_m:
            remainder = milestone_m.group(1).strip()
            if not re.match(r"^(?:no|not|never|without|none|tbd|tba|todo|placeholder|unknown|undefined|unassigned|fixed|patched|resolved|wontfix|completed|done|finished|passed|approved|closed|obsolete|retired)\b", remainder):
                if not re.search(r"#[0-9]+[a-zA-Z_]", remainder):
                    if re.search(r"(?:#[1-9]\d*(?!\w)|\b\d+(?:\.\d+)*\b|\b(?:release|releases|upgrade|upgrades|migration|migrations|update|updates|cleanup|cleanups|sprint|sprints|quarter|quarters|audit|audits|patch|patches|pr|prs)\b)", remainder):
                        is_milestone = True
        elif re.search(r"^\b(?:next\s+(?:release|sprint|quarter|audit|update))\b$", norm_cond):
            is_milestone = True

        if not (is_version or is_cadence or is_milestone or is_tracker):
            return False, f"unrecognized or non-actionable review condition '{raw_cond}'"

        has_valid_review = True

    # 3. Check recognized milestone / ongoing conditions
    for m in LIFECYCLE_OTHER_PATTERN.finditer(text):
        if is_negated_prefix(text[:m.start()]):
            continue
        matched_text = m.group(0).lower()
        if matched_text.startswith("awaiting") and re.search(r"\b(?:no|not|without|never)\b", matched_text):
            continue
        has_valid_milestone = True

    if has_valid_expiry or has_valid_review or has_valid_milestone:
        return True, None

    return False, "MISSING"

def validate_lifecycle(text):
    has_owner = has_accountable_owner(text)
    if re.search(r"\btracking\b[^\w#]*#0+\b", text, re.IGNORECASE):
        return has_owner, False, "invalid zero-valued tracking reference '#0'"
    if re.search(r"#[0-9]+[a-zA-Z_]", text):
        return has_owner, False, "malformed issue reference with non-numeric suffix"
    has_expiry, expiry_msg = has_lifecycle_condition(text)
    return has_owner, has_expiry, expiry_msg

# Derive baseline grandfathered advisories directly from the base branch / commit in git (single source of truth).
# If the base branch cannot be resolved, fail closed (no static fallback list) so removed advisories cannot be re-added without full review metadata.
baseline_deny_entries = {}   # adv_id -> normalized reason
baseline_audit_entries = {}  # adv_id -> normalized comment

base_deny_file = os.environ.get("BASE_DENY_TOML")
base_audit_file = os.environ.get("BASE_AUDIT_TOML")

if base_deny_file or base_audit_file:
    # Test fixture or custom baseline files
    if base_deny_file and os.path.isfile(base_deny_file):
        try:
            with open(base_deny_file, "r", encoding="utf-8") as f:
                p = TomlArrayParser(f.read())
                elems, _ = p.parse_advisories_ignore()
                if elems:
                    for item, _ in elems:
                        if isinstance(item, dict) and "id" in item:
                            adv_id = item["id"].strip()
                            reason = re.sub(r"\s+", " ", item.get("reason", "").strip())
                            baseline_deny_entries[adv_id] = reason
        except Exception:
            pass
    if base_audit_file and os.path.isfile(base_audit_file):
        try:
            with open(base_audit_file, "r", encoding="utf-8") as f:
                p = TomlArrayParser(f.read())
                elems, _ = p.parse_advisories_ignore()
                if elems:
                    for item, comment in elems:
                        if isinstance(item, str):
                            adv_id = item.strip()
                            comm = re.sub(r"\s+", " ", (comment or "").strip())
                            baseline_audit_entries[adv_id] = comm
        except Exception:
            pass
else:
    import subprocess
    base_ref = os.environ.get("BASE_REF") or os.environ.get("GITHUB_BASE_REF")
    if not base_ref and os.environ.get("GITHUB_EVENT_PATH"):
        try:
            with open(os.environ["GITHUB_EVENT_PATH"], "r", encoding="utf-8") as f:
                event_data = json.load(f)
                if "pull_request" in event_data and "base" in event_data["pull_request"]:
                    base_ref = event_data["pull_request"]["base"].get("sha")
                elif "before" in event_data and event_data["before"]:
                    before_sha = event_data["before"]
                    if not re.match(r"^0+$", before_sha):
                        base_ref = before_sha
        except Exception:
            pass

    if base_ref and re.match(r"^0+$", base_ref):
        base_ref = ""

    if not base_ref:
        for candidate in ["origin/master", "master"]:
            try:
                res = subprocess.run(
                    ["git", "rev-parse", "--verify", candidate],
                    capture_output=True, text=True, check=False
                )
                if res.returncode == 0:
                    base_ref = candidate
                    break
            except Exception:
                pass

    if base_ref:
        try:
            res = subprocess.run(["git", "show", f"{base_ref}:deny.toml"], capture_output=True, text=True, check=False)
            if res.returncode == 0:
                parser = TomlArrayParser(res.stdout)
                elems, _ = parser.parse_advisories_ignore()
                if elems:
                    for item, _ in elems:
                        if isinstance(item, dict) and "id" in item:
                            adv_id = item["id"].strip()
                            reason = re.sub(r"\s+", " ", item.get("reason", "").strip())
                            baseline_deny_entries[adv_id] = reason
            res2 = subprocess.run(["git", "show", f"{base_ref}:.cargo/audit.toml"], capture_output=True, text=True, check=False)
            if res2.returncode == 0:
                parser2 = TomlArrayParser(res2.stdout)
                elems2, _ = parser2.parse_advisories_ignore()
                if elems2:
                    for item, comment in elems2:
                        if isinstance(item, str):
                            adv_id = item.strip()
                            comm = re.sub(r"\s+", " ", (comment or "").strip())
                            baseline_audit_entries[adv_id] = comm
        except Exception:
            pass

errors = []
deny_entries = {}
audit_entries = {}

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

            if adv_id in deny_entries:
                errors.append(f"deny.toml: Duplicate advisory exception ID '{adv_id}' detected")
            else:
                deny_entries[adv_id] = reason

            if adv_id in RETIRED_ADVISORIES:
                errors.append(f"deny.toml: Retired advisory '{adv_id}' is still present in deny.toml: {RETIRED_ADVISORIES[adv_id]}")

            norm_reason = re.sub(r"\s+", " ", reason)
            is_grandfathered = (
                adv_id in baseline_deny_entries and
                norm_reason == baseline_deny_entries[adv_id]
            )
            if not is_grandfathered:
                if not reason:
                    errors.append(f"deny.toml: Advisory '{adv_id}' missing 'reason' field with owner and review/expiry condition")
                else:
                    has_owner, has_expiry, expiry_msg = validate_lifecycle(reason)
                    if not has_owner or not has_expiry:
                        errors.append(
                            f"deny.toml: New or modified advisory exception '{adv_id}' reason '{reason}' lacks required lifecycle metadata: "
                            f"owner={'ok' if has_owner else 'MISSING'}, review/expiry={'ok' if has_expiry else (expiry_msg or 'MISSING')}"
                        )
            else:
                if reason:
                    has_owner, has_expiry, expiry_msg = validate_lifecycle(reason)
                    if expiry_msg and expiry_msg != "MISSING":
                        errors.append(f"deny.toml: Advisory exception '{adv_id}' has expired or invalid review/expiry: {expiry_msg}")
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

            if adv_id in audit_entries:
                errors.append(f".cargo/audit.toml: Duplicate advisory exception ID '{adv_id}' detected")
            else:
                audit_entries[adv_id] = comment or ""

            if adv_id in RETIRED_ADVISORIES:
                errors.append(f".cargo/audit.toml: Retired advisory '{adv_id}' is still present in audit.toml: {RETIRED_ADVISORIES[adv_id]}")

            norm_comment = re.sub(r"\s+", " ", comment or "")
            is_grandfathered = (
                adv_id in baseline_audit_entries and
                norm_comment == baseline_audit_entries[adv_id]
            )
            if not is_grandfathered:
                if not comment:
                    errors.append(f".cargo/audit.toml: Advisory '{adv_id}' missing inline comment with owner and review/expiry condition")
                else:
                    has_owner, has_expiry, expiry_msg = validate_lifecycle(comment)
                    if not has_owner or not has_expiry:
                        errors.append(
                            f".cargo/audit.toml: New or modified advisory exception '{adv_id}' comment '{comment}' lacks required lifecycle metadata: "
                            f"owner={'ok' if has_owner else 'MISSING'}, review/expiry={'ok' if has_expiry else (expiry_msg or 'MISSING')}"
                        )
            else:
                if comment:
                    has_owner, has_expiry, expiry_msg = validate_lifecycle(comment)
                    if expiry_msg and expiry_msg != "MISSING":
                        errors.append(f".cargo/audit.toml: Advisory exception '{adv_id}' has expired or invalid review/expiry: {expiry_msg}")
        else:
            errors.append(f".cargo/audit.toml: Expected string advisory entry, found {type(elem).__name__}: {elem!r}")

# --- 3. Validate consistency across shared and tool-specific advisory entries ---
# When a test harness explicitly overrides only one config file in isolation,
# skip cross-file symmetric difference checks against the unrelated repository config.
is_single_file_override = (
    ("DENY_TOML" in os.environ and "AUDIT_TOML" not in os.environ) or
    ("AUDIT_TOML" in os.environ and "DENY_TOML" not in os.environ)
)

if not is_single_file_override:
    deny_set = set(deny_entries.keys())
    audit_set = set(audit_entries.keys())

    def has_explicit_tool_scope(text, expected_tool):
        # Exclusive non-negated tool scope declaration for the expected tool.
        # Must not declare or include the other tool.
        target = "deny" if "deny" in expected_tool else "audit"
        other = "audit" if target == "deny" else "deny"

        target_pattern = re.compile(rf"\b(?:cargo-)?{target}\b", re.IGNORECASE)
        other_pattern = re.compile(rf"\b(?:cargo-)?{other}\b", re.IGNORECASE)
        shorthand_pattern = re.compile(r"\b(?:cargo-)?(?:deny|audit)(?:-only|\s+only)\b", re.IGNORECASE)

        found_target_scope = False

        # 1. Inspect all explicit scope / tool field declarations
        for m in re.finditer(r"\b(?:tool|scope)\b\s*[:=]\s*([^;\n]+)", text, re.IGNORECASE):
            scope_val = m.group(1).strip().lower()
            prefix = text[:m.start()]
            clause_prefix = re.split(r"[;,\.\n]", prefix)[-1]
            if is_negated_prefix(clause_prefix):
                continue
            # If the scope declaration references the other tool, it is not exclusive to target
            if other_pattern.search(scope_val):
                return False
            # Check target in scope_val ensuring it is not negated
            for tm in target_pattern.finditer(scope_val):
                val_prefix = scope_val[:tm.start()]
                if is_negated_prefix(val_prefix):
                    continue
                found_target_scope = True

        # 2. Inspect shorthand "<tool> only" or "<tool>-only"
        for m in shorthand_pattern.finditer(text):
            clause = m.group(0).lower()
            prefix = text[:m.start()]
            clause_prefix = re.split(r"[;,\.\n]", prefix)[-1]
            if is_negated_prefix(clause_prefix):
                continue
            if other_pattern.search(clause):
                return False
            if target_pattern.search(clause):
                found_target_scope = True

        return found_target_scope

    for adv_id in sorted(deny_set - audit_set):
        reason = deny_entries[adv_id]
        norm_reason = re.sub(r"\s+", " ", reason)
        is_grandfathered = (
            adv_id in baseline_deny_entries and
            adv_id not in baseline_audit_entries and
            norm_reason == baseline_deny_entries[adv_id]
        )
        if not is_grandfathered and not has_explicit_tool_scope(reason, "cargo-deny"):
            errors.append(
                f"deny.toml: Undeclared one-sided advisory exception '{adv_id}' is missing from .cargo/audit.toml. "
                f"All exceptions must be present in both files unless declared tool-specific."
            )

    for adv_id in sorted(audit_set - deny_set):
        comment = audit_entries[adv_id]
        norm_comment = re.sub(r"\s+", " ", comment)
        is_grandfathered = (
            adv_id in baseline_audit_entries and
            adv_id not in baseline_deny_entries and
            norm_comment == baseline_audit_entries[adv_id]
        )
        if not is_grandfathered and not has_explicit_tool_scope(comment, "cargo-audit"):
            errors.append(
                f".cargo/audit.toml: Undeclared one-sided advisory exception '{adv_id}' is missing from deny.toml. "
                f"All exceptions must be present in both files unless declared tool-specific."
            )

    shared_ids = deny_set & audit_set
    for adv_id in sorted(shared_ids):
        d_reason = re.sub(r"\s+", " ", deny_entries[adv_id].strip())
        a_comment = re.sub(r"\s+", " ", audit_entries[adv_id].strip())
        if d_reason != a_comment:
            is_grandfathered_mismatch = (
                adv_id in baseline_deny_entries and
                adv_id in baseline_audit_entries and
                d_reason == baseline_deny_entries[adv_id] and
                a_comment == baseline_audit_entries[adv_id]
            )
            if not is_grandfathered_mismatch:
                errors.append(
                    f"Shared advisory '{adv_id}' metadata mismatch across configs: "
                    f"deny.toml reason '{d_reason}' != .cargo/audit.toml comment '{a_comment}'"
                )

if errors:
    print("advisory-exceptions gate: FAIL", file=sys.stderr)
    for err in errors:
        print(f"  - {err}", file=sys.stderr)
    sys.exit(1)

print("advisory-exceptions gate: clean")
sys.exit(0)
PYEOF
