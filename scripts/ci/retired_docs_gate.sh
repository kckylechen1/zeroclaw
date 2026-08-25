#!/usr/bin/env bash

# Retired-documentation gate: fails when active documentation still TEACHES a
# retired surface (route/tool/profile/config key/env override/code path) as
# current behavior. Canonical input is scripts/ci/retired_surfaces.json, the
# single retired-term registry; retiring PRs add entries there in the same PR.
#
# Scanned surfaces (active documentation, from `git ls-files`):
#   - all tracked Markdown (.md/.mdx): READMEs, docs/, SKILL/agent/operator
#     instruction files, examples and quickstarts; excluding paths with a
#     `tests` or `fixtures` component (test data) and the docs/book/po
#     translation submodule
#   - tracked user-directed TOML (config templates and examples): everything
#     not under vendor/, firmware/, apps/, fuzz/, xtask/, tools/, .cargo/,
#     dev/ci/, docs/book/po/, not a tests/fixtures path, not a Cargo.toml /
#     rust-toolchain.toml / book.toml / root tool-config file (build tooling
#     is not documentation); user templates that happen to live under
#     crates/ (e.g. robot-kit) are scanned
#   - tracked English Fluent catalogues (**/locales/en/*.ftl: runtime help
#     text source of truth; non-English locales are generated and skipped)
# Not scanned: source code, CI YAML, test fixtures, book theme JS/CSS,
# binaries. Help text that lives in code is out of reach for a docs lint.
#
# Matching: each registry entry matches by kind (word-bounded identifier by
# default; env-prefix entries match as a prefix so suffixed variables hit).
# Entries may override with `match_regex` for terms that collide with
# ordinary words or file basenames. In Fluent files, every entry
# additionally matches its `tool-<hyphenated-term>` key form (the tool-
# description key convention), so `tool-proxy-config = ...` is caught even
# though the hyphenated spelling differs from the registry term; unrelated
# hyphenated keys (channel-runtime-*, app-prefixed *) do not match.
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
# explicit paths; matching is pathname-aware (directory components must be
# literal, wildcards may appear only in the basename, so "docs/*.md" exempts
# files directly under docs/ and never nested subtrees) and every path
# component must carry at least one non-wildcard character, so blanket
# exemptions ("*", "**/**", "docs/**") cannot be declared.
#
# Replacement teaching: an entry with a `replacement` string additionally
# requires that at least one scanned file teaches the replacement (contains
# the string); a rename whose replacement vanished from every active doc
# fails the gate even when the old name is fully gone. Replacement strings
# must be at least 4 characters with no whitespace, so a vacuous one-letter
# replacement cannot satisfy the contract by occurring everywhere.
#
# Known limits, stated honestly:
#   - vocabulary on an unrelated line can excuse a nearby teaching reference
#     (false pass); sections whose heading or intro carries the vocabulary
#     excuse everything below it until the next heading (false pass);
#   - vocabulary-free teaching references to ambiguous terms outside their
#     match_regex forms are invisible (false pass);
#   - bare prose variants (spelled-out or differently cased names) and
#     non-Fluent transformed spellings are invisible;
#   - the replacement check proves presence of the string somewhere in
#     active docs, not that it is taught well;
#   - a Markdown heading resets section context; TOML/FTL files get window
#     context only (comments more than 3 lines away do not excuse a hit).
# Preferring false pass over false fail is the chosen trade: legitimate
# historical mentions must not storm, and the registry keeps the drift class
# visible for the terms that matter. The registry is the declared
# contraction metadata: a contraction PR that omits its entry defeats this
# gate by review, not by mechanism.
#
# Override: RETIRED_SURFACES_FILE points at an alternative registry (used by
# the self-test). Exit status: 0 = clean, 1 = teaching references found,
# 2 = fatal (missing python3, bad registry, Git failure, outside a work
# tree).

set -euo pipefail

if ! repo_root="$(git rev-parse --show-toplevel 2>/dev/null)"; then
    echo "FATAL: retired-docs gate must run inside a Git work tree." >&2
    exit 2
fi
cd "$repo_root"

if ! command -v python3 >/dev/null 2>&1; then
    echo "FATAL: python3 is required by the retired-docs gate." >&2
    exit 2
fi

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
ROOT_TOOL_CONFIGS = {
    "Cargo.toml", "clippy.toml", "deny.toml", "locales.toml",
    "release-plz.toml", "rust-toolchain.toml", "rustfmt.toml", "taplo.toml",
}
BUILD_BASENAMES = {"Cargo.toml", "rust-toolchain.toml", "book.toml"}
NONDOC_TREES = ("vendor/", "firmware/", "apps/", "fuzz/",
                "xtask/", "tools/", ".cargo/", "dev/ci/")
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

def is_wildcard_only(glob):
    # Every path component must carry at least one non-wildcard character,
    # so a blanket "**/**" (or "*") cannot be declared as an exemption.
    return any(not re.search(r"[^*?/]", part) for part in glob.split("/"))

def wildcard_in_dir_component(glob):
    # fnmatch wildcards span "/" (they are regex-based), so wildcards are
    # only safe in the final basename component; directory components must
    # be literal to keep an exemption inside one directory.
    parts = glob.split("/")
    return any(re.search(r"[*?\[\]]", part) for part in parts[:-1])

def glob_matches(path, glob):
    # Pathname-aware match: the directory part must be an exact literal
    # prefix and only the basename may carry wildcards, so "docs/*.md"
    # exempts files directly under docs/ and never nested subtrees.
    glob_dir, _, glob_base = glob.rpartition("/")
    path_dir, _, path_base = path.rpartition("/")
    return path_dir == glob_dir and fnmatch.fnmatchcase(path_base, glob_base)

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
    if not isinstance(kind, str) or kind not in KINDS:
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
    try:
        ftl_matcher = re.compile(
            r"^tool-" + re.escape(term.replace("_", "-")) + r"\b"
        )
    except re.error as exc:
        fatal(f"entry {term!r}: Fluent key pattern does not compile: {exc}")

    globs = raw.get("allowed_globs", [])
    if not isinstance(globs, list) or any(
        not isinstance(g, str) or not g for g in globs
    ):
        fatal(f"entry {term!r}: 'allowed_globs' must be a list of non-empty strings")
    for glob in globs:
        if "/" not in glob or is_wildcard_only(glob) or wildcard_in_dir_component(glob):
            fatal(
                f"entry {term!r}: allowed_glob {glob!r} must be path-shaped "
                "(contain '/') with literal directory components and at "
                "least one non-wildcard character in every path component"
            )

    replacement = raw.get("replacement")
    if replacement is not None and (
        not isinstance(replacement, str)
        or len(replacement.strip()) < 4
        or any(ch.isspace() for ch in replacement)
    ):
        fatal(
            f"entry {term!r}: 'replacement' must be a string of at least 4 "
            "non-whitespace-only characters containing no whitespace (a "
            "trivially vacuous replacement teaches nothing)"
        )

    entries.append({
        "term": term,
        "kind": kind,
        "matcher": matcher,
        "ftl_matcher": ftl_matcher,
        "globs": globs,
        "replacement": replacement,
    })

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
    if path.endswith(".toml"):
        if parts[-1] in BUILD_BASENAMES:
            return False
        if path in ROOT_TOOL_CONFIGS:
            return False
        if any(path.startswith(t) for t in NONDOC_TREES):
            return False
        return True
    if "/locales/en/" in f"/{path}" and path.endswith(".ftl"):
        return True
    return False

targets = [p for p in paths if scanned(p)]

violations = []
file_texts = {}
for path in targets:
    is_markdown = path.endswith((".md", ".mdx"))
    is_ftl = path.endswith(".ftl")
    changelog = path.rsplit("/", 1)[-1].lower().startswith("changelog")
    if changelog:
        continue
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            content = fh.read()
    except OSError:
        continue
    file_texts[path] = content
    lines = content.splitlines()

    active = [e for e in entries if not any(
        glob_matches(path, g) for g in e["globs"]
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
            matched = entry["matcher"].search(line) or (
                is_ftl and bool(entry["ftl_matcher"].search(line))
            )
            if not matched:
                continue
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

for entry in entries:
    replacement = entry["replacement"]
    if replacement is None:
        continue
    if not any(replacement in text for text in file_texts.values()):
        violations.append(
            (
                "scripts/ci/retired_surfaces.json",
                0,
                entry["kind"],
                entry["term"],
                f"renamed surface: no active doc teaches the replacement "
                f"{replacement!r}",
            )
        )

if not violations:
    print(f"retired-docs gate: {len(targets)} active doc file(s) scanned, "
          f"no teaching references to retired surfaces")
    sys.exit(0)

print("FAIL: active documentation still teaches retired surfaces:")
shown = violations[:MAX_REPORT]
for path, n, kind, term, text in shown:
    where = f"{path}:{n}" if n else path
    print(f"  {where}: [{kind}] {term}: {text}")
if len(violations) > MAX_REPORT:
    print(f"  ... and {len(violations) - MAX_REPORT} more")
print(f"{len(violations)} teaching reference(s) to retired surfaces. "
      "Remove them, or mark the context historical "
      "(removed/deprecated/legacy wording); registry: "
      "scripts/ci/retired_surfaces.json.")
sys.exit(1)
PY
