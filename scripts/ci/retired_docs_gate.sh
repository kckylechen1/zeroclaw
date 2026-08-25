#!/usr/bin/env bash

# Retired-documentation gate: fails when active documentation still TEACHES a
# retired surface (route/tool/profile/config key/env override/code path) as
# current behavior. Canonical input is scripts/ci/retired_surfaces.json, the
# single retired-term registry; retiring PRs add entries there in the same PR.
#
# Scanned surfaces (active documentation, from `git ls-files`):
#   - all tracked Markdown (.md/.mdx), i.e. READMEs, docs/, SKILL/agent/
#     operator instruction files, examples and quickstarts; excluding paths
#     with a `tests` or `fixtures` component (test data) and the
#     docs/book/po translation submodule
#   - tracked TOML under docs/ (config templates and examples)
#   - tracked English Fluent catalogues (**/locales/en/*.ftl: runtime help
#     text source of truth; non-English locales are generated and skipped)
# Not scanned: source code, CI YAML, test fixtures, book theme JS/CSS,
# binaries. Help text that lives in code is out of reach for a docs lint.
#
# Discriminator (deterministic, line-based; deliberately conservative in the
# pass direction — this is a tripwire, not a proof):
# A term hit FAILS unless historical context is found in any of
#   (a) the hit line itself,
#   (b) a 3-line window above/below the hit (sentence/table spill), or
#   (c) the enclosing Markdown section, from the nearest heading above the
#       hit down to the line before it (retirement-note sections whose intro
#       says "no longer ..." excuse the whole section body).
# Historical context = removal/deprecation vocabulary on those lines:
# remov*/deprecat*/retir*/delet*/legacy/sunset*/unsupported/no longer/
# superseded/replaced/obsolete/migrat*/historical/formerly/withdrawn/
# (not|never) enforced. Whole files whose basename starts with CHANGELOG are
# exempt (changelog context). Per-term `allowed_globs` in the registry exempt
# explicit paths; per-term `match_regex` narrows terms that collide with
# ordinary words or file basenames.
#
# Known limits, stated honestly:
#   - vocabulary on an unrelated line can excuse a nearby teaching reference
#     (false pass); sections whose heading or intro carries the vocabulary
#     excuse everything below it until the next heading (false pass);
#   - vocabulary-free teaching references to ambiguous terms outside their
#     match_regex forms are invisible (false pass);
#   - a Markdown heading resets section context, so context never leaks
#     across sections; TOML/FTL files get window context only.
# Prefering false pass over false fail is the chosen trade: legitimate
# historical mentions must not storm, and the registry keeps the drift class
# visible for the terms that matter.
#
# Override: RETIRED_SURFACES_FILE points at an alternative registry (used by
# the self-test). Exit status: 0 = clean, 1 = teaching references found,
# 2 = fatal (bad registry, Git failure, outside a work tree).

set -euo pipefail

if ! repo_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo "FATAL: retired-docs gate must run inside a Git work tree." >&2
    exit 2
fi
cd "$repo_root"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
registry="${RETIRED_SURFACES_FILE:-${script_dir}/retired_surfaces.json}"

if [ ! -r "$registry" ]; then
    echo "FATAL: retired-surface registry not readable: ${registry}" >&2
    exit 2
fi

python3 - "$registry" <<'PY'
import fnmatch
import json
import re
import subprocess
import sys

registry_path = sys.argv[1]

KINDS = {"tool", "op", "config-key", "env-prefix", "code-path"}
HISTORICAL = re.compile(
    r"remov\w*|deprecat\w*|retir\w*|delet\w*|legacy|sunset\w*|unsupported"
    r"|no longer|superseded|replaced|obsolete|migrat\w*|historical|formerly"
    r"|withdrawn|not enforced|never enforced",
    re.IGNORECASE,
)
HEADING = re.compile(r"^\s{0,3}#{1,6}\s")
FENCE = re.compile(r"^\s*(```|~~~)")
WINDOW = 3
MAX_REPORT = 50

def fatal(msg):
    print(f"FATAL: {msg}", file=sys.stderr)
    sys.exit(2)

try:
    with open(registry_path, encoding="utf-8") as fh:
        doc = json.load(fh)
except (OSError, json.JSONDecodeError) as exc:
    fatal(f"retired-surface registry is unreadable or invalid JSON: {exc}")

if not isinstance(doc, dict) or not isinstance(doc.get("retired"), list):
    fatal("registry must be an object with a 'retired' list")
if not doc["retired"]:
    fatal("registry 'retired' list must not be empty")

entries = []
seen_terms = set()
for raw in doc["retired"]:
    if not isinstance(raw, dict):
        fatal("each registry entry must be an object")
    term = raw.get("term")
    kind = raw.get("kind")
    retired_in = raw.get("retired_in")
    notes = raw.get("notes")
    if not isinstance(term, str) or not term:
        fatal(f"entry with invalid 'term': {raw!r}")
    if kind not in KINDS:
        fatal(f"entry {term!r}: 'kind' must be one of {sorted(KINDS)}, got {kind!r}")
    if not isinstance(retired_in, str) or not retired_in.strip():
        fatal(f"entry {term!r}: 'retired_in' must be a non-empty string")
    if not isinstance(notes, str) or not notes.strip():
        fatal(f"entry {term!r}: 'notes' must be a non-empty string")
    if term in seen_terms:
        fatal(f"duplicate registry term: {term!r}")
    seen_terms.add(term)

    match_regex = raw.get("match_regex")
    if match_regex is None:
        if kind == "env-prefix":
            pattern = re.escape(term)
        else:
            pattern = rf"\b{re.escape(term)}\b"
    elif isinstance(match_regex, str) and match_regex:
        pattern = match_regex
    else:
        fatal(f"entry {term!r}: 'match_regex' must be a non-empty string")
    try:
        matcher = re.compile(pattern)
    except re.error as exc:
        fatal(f"entry {term!r}: match pattern does not compile ({pattern!r}): {exc}")

    globs = raw.get("allowed_globs", [])
    if not isinstance(globs, list) or any(
        not isinstance(g, str) or not g for g in globs
    ):
        fatal(f"entry {term!r}: 'allowed_globs' must be a list of non-empty strings")

    entries.append({"term": term, "kind": kind, "matcher": matcher, "globs": globs})

proc = subprocess.run(
    ["git", "ls-files", "-z"], capture_output=True, check=False
)
if proc.returncode != 0:
    fatal(f"git ls-files failed: {proc.stderr.decode(errors='replace').strip()}")
paths = [p.decode("utf-8", errors="replace") for p in proc.stdout.split(b"\x00") if p]

def scanned(path):
    parts = path.split("/")
    if "tests" in parts or "fixtures" in parts:
        return False
    if path == "docs/book/po" or path.startswith("docs/book/po/"):
        return False
    if path.endswith((".md", ".mdx")):
        return True
    if path.startswith("docs/") and path.endswith(".toml"):
        return True
    if "/locales/en/" in f"/{path}" and path.endswith(".ftl"):
        return True
    return False

targets = [p for p in paths if scanned(p)]

violations = []
for path in targets:
    is_markdown = path.endswith((".md", ".mdx"))
    changelog = path.rsplit("/", 1)[-1].lower().startswith("changelog")
    if changelog:
        continue
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            lines = fh.read().splitlines()
    except OSError:
        continue

    active = [e for e in entries if not any(
        fnmatch.fnmatch(path, g) for g in e["globs"]
    )]
    if not active:
        continue

    in_fence = False
    heading_line = 0  # 1-based line of nearest heading; 0 = none yet
    for n, line in enumerate(lines, 1):
        stripped = line.lstrip()
        if is_markdown and FENCE.match(stripped):
            in_fence = not in_fence
            continue
        if is_markdown and not in_fence and HEADING.match(line):
            heading_line = n
            continue

        for entry in active:
            if entry["matcher"].search(line):
                window = lines[max(0, n - 1 - WINDOW): n + WINDOW]
                in_section = lines[heading_line - 1: n - 1] if heading_line else []
                historical = (
                    HISTORICAL.search(line)
                    or any(HISTORICAL.search(w) for w in window)
                    or any(HISTORICAL.search(s) for s in in_section)
                )
                if not historical:
                    text = line.strip()
                    if len(text) > 160:
                        text = text[:157] + "..."
                    violations.append(
                        (path, n, entry["kind"], entry["term"], text)
                    )

if not violations:
    print(f"retired-docs gate: {len(targets)} active doc file(s) scanned, "
          f"no teaching references to retired surfaces")
    sys.exit(0)

print("FAIL: active documentation still teaches retired surfaces:")
shown = violations[:MAX_REPORT]
for path, n, kind, term, text in shown:
    print(f"  {path}:{n}: [{kind}] {term}: {text}")
if len(violations) > MAX_REPORT:
    print(f"  ... and {len(violations) - MAX_REPORT} more")
print(f"{len(violations)} teaching reference(s) to retired surfaces. "
      "Remove them, or mark the context historical "
      "(removed/deprecated/legacy wording); registry: "
      "scripts/ci/retired_surfaces.json.")
sys.exit(1)
PY
