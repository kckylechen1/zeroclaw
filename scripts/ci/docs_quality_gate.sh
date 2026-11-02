#!/usr/bin/env bash

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

BASE_SHA="${BASE_SHA:-}"
DOCS_FILES_RAW="${DOCS_FILES:-}"

if [ -z "$BASE_SHA" ] || ! git cat-file -e "$BASE_SHA^{commit}" 2>/dev/null; then
    echo "BASE_SHA is missing or invalid; falling back to full-file markdown lint."
    BASE_SHA=""
fi

DOCS_FILES_RAW="$(python3 scripts/ci/collect_changed_links.py \
    --base "$BASE_SHA" --docs-files "$DOCS_FILES_RAW" --list-quality)"

if [ -z "$DOCS_FILES_RAW" ]; then
    echo "No docs files detected; skipping docs quality gate."
    exit 0
fi

ALL_FILES=()
while IFS= read -r file; do
    if [ -n "$file" ]; then
        ALL_FILES+=("$file")
    fi
done < <(printf '%s\n' "$DOCS_FILES_RAW")

if [ "${#ALL_FILES[@]}" -eq 0 ]; then
    echo "No docs files detected after normalization; skipping docs quality gate."
    exit 0
fi

EXISTING_FILES=()
for file in "${ALL_FILES[@]}"; do
    if [ -f "$file" ]; then
        EXISTING_FILES+=("$file")
    fi
done

if [ "${#EXISTING_FILES[@]}" -eq 0 ]; then
    echo "No existing docs files to lint; skipping docs quality gate."
    exit 0
fi

# Em-dash gate: no new prose em-dashes (U+2014), matching the changed-line
# Markdown policy below. A missing base checks full files. Em-dashes inside
# fenced code blocks and inline `code` spans (including spans that wrap across
# lines) are allowed, since those quote literal source, CLI output, or table
# rules. Everything else uses a comma, colon, semicolon, or period instead.
python3 - "$BASE_SHA" "${EXISTING_FILES[@]}" <<'PY'
import sys

sys.path.insert(0, "scripts/ci")
from docs_lint_filter import changed_lines

EM = "\u2014"
base = sys.argv[1]
files = sys.argv[2:]
violations = []
changed = changed_lines(base, files) if base else {}

for path in files:
    try:
        with open(path, encoding="utf-8") as fh:
            lines = fh.readlines()
    except (OSError, UnicodeError) as exc:
        print(f"Cannot read {path}: {exc}", file=sys.stderr)
        sys.exit(2)

    in_fence = False
    in_span = False
    for n, line in enumerate(lines, 1):
        stripped = line.lstrip()
        if stripped.startswith("```") or stripped.startswith("~~~"):
            in_fence = not in_fence
            in_span = False
            continue
        if in_fence:
            continue
        if EM not in line and "`" not in line:
            continue
        for ch in line:
            if ch == "`":
                in_span = not in_span
            elif ch == EM and not in_span and (not base or n in changed[path]):
                violations.append((path, n, line.rstrip()))
                break

if violations:
    print("Em-dash (\u2014) found in prose. Use a comma, colon, semicolon, or")
    print("period instead. Em-dashes are only allowed inside code spans/blocks.")
    print()
    for path, n, text in violations:
        print(f"  {path}:{n}: {text}")
    print()
    print(f"Blocking prose em-dashes: {len(violations)}")
    sys.exit(1)

print("No new prose em-dashes in checked docs lines.")
PY

if command -v npx >/dev/null 2>&1; then
    MD_CMD=(npx --yes markdownlint-cli2@0.20.0)
elif command -v markdownlint-cli2 >/dev/null 2>&1; then
    MD_CMD=(markdownlint-cli2)
else
    echo "markdownlint-cli2 is required (via npx or local binary)."
    exit 1
fi

echo "Linting docs files: ${EXISTING_FILES[*]}"

# OS-tabs are an mdBook theme construct (see docs/book/theme/pc-enhance.js):
# a `<div class="os-tabs-src">` wraps one ATX heading per OS/shell that the
# preprocessor turns into tab labels at render time. Those headings are widget
# markup, not document structure, so they trip MD024/MD001/MD022. Lint a
# line-preserving mirror where headings inside those divs are rewritten to HTML
# comments; everything else (including real prose headings) is linted as-is.
MIRROR_ROOT="$(mktemp -d)"
for file in "${EXISTING_FILES[@]}"; do
    mirror_path="$MIRROR_ROOT/$file"
    mkdir -p "$(dirname "$mirror_path")"
    python3 - "$file" "$mirror_path" <<'PY'
import re
import sys

src, dst = sys.argv[1], sys.argv[2]
open_re = re.compile(r'<div\s+class="[^"]*\bos-tabs-src\b[^"]*"', re.IGNORECASE)
close_re = re.compile(r"</div>", re.IGNORECASE)
heading_re = re.compile(r"^(\s*)(#{1,6})\s+(.*\S)\s*$")

depth = 0
out = []
with open(src, encoding="utf-8") as fh:
    for line in fh:
        body = line.rstrip("\n")
        if depth == 0:
            if open_re.search(body):
                depth = 1
            out.append(body)
            continue
        # inside an os-tabs-src div
        if open_re.search(body):
            depth += 1
        m = heading_re.match(body)
        if m:
            out.append(f"{m.group(1)}<!-- os-tab: {m.group(3)} -->")
        else:
            out.append(body)
        if close_re.search(body):
            depth -= 1

with open(dst, "w", encoding="utf-8") as fh:
    fh.write("\n".join(out) + "\n")
PY
done

cp .markdownlint-cli2.yaml "$MIRROR_ROOT/.markdownlint-cli2.yaml"

LINT_OUTPUT_RAW="$(mktemp)"
LINT_OUTPUT_FILE="$(mktemp)"
set +e
( cd "$MIRROR_ROOT" && "${MD_CMD[@]}" "${EXISTING_FILES[@]}" ) >"$LINT_OUTPUT_RAW" 2>&1
LINT_EXIT=$?
set -e
# Map mirror paths back to repo-relative paths so downstream filtering and
# human output reference the real files.
sed "s#^${MIRROR_ROOT}/##" "$LINT_OUTPUT_RAW" >"$LINT_OUTPUT_FILE"
rm -rf "$MIRROR_ROOT" "$LINT_OUTPUT_RAW"

if [ "$LINT_EXIT" -eq 0 ]; then
    cat "$LINT_OUTPUT_FILE"
    rm -f "$LINT_OUTPUT_FILE"
    exit 0
fi

if [ -z "$BASE_SHA" ]; then
    cat "$LINT_OUTPUT_FILE"
    rm -f "$LINT_OUTPUT_FILE"
    exit "$LINT_EXIT"
fi

set +e
python3 scripts/ci/docs_lint_filter.py "$BASE_SHA" "$LINT_EXIT" "$LINT_OUTPUT_FILE" "${EXISTING_FILES[@]}"
SCRIPT_EXIT=$?
set -e
rm -f "$LINT_OUTPUT_FILE"
exit "$SCRIPT_EXIT"
