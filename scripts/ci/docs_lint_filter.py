#!/usr/bin/env python3
"""Filter recognized Markdown lint diagnostics to changed lines, failing on tool errors."""

from __future__ import annotations

import argparse
from pathlib import Path
import re
import subprocess
import sys


def changed_lines(base: str, files: list[str]) -> dict[str, set[int]]:
    changed: dict[str, set[int]] = {}
    for path in files:
        proc = subprocess.run(
            ["git", "diff", "--no-ext-diff", "--unified=0", base, "HEAD", "--", path],
            check=True, capture_output=True, text=True,
        )
        lines: set[int] = set()
        for match in re.finditer(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,(\d+))? @@", proc.stdout, re.MULTILINE):
            start, count = int(match[1]), int(match[2] or "1")
            lines.update(range(start, start + count))
        changed[path] = lines
    return changed


def filter_output(output: str, exit_code: int, changed: dict[str, set[int]]) -> int:
    # markdownlint-cli2 emits both file:line and file:line:column. A column
    # must never become part of the path and turn a new issue into baseline.
    diagnostic = re.compile(r"^(.+?):(\d+)(?::\d+)?\s+error\s+(MD\d+(?:/[^\s]+)?)\s+(.*)$")
    summary = re.compile(r"^Summary: (\d+) error\(s\)$")
    banner = re.compile(r"^(?:markdownlint-cli2 v\S+ \(markdownlint v\S+\)|Finding: .*|Linting: \d+ file\(s\)|npm warn .*)$")
    blocking: list[str] = []
    baseline: list[str] = []
    unknown: list[str] = []
    summaries: list[int] = []
    for line in output.splitlines():
        match = diagnostic.match(line)
        if match:
            path, number, rule, message = match.groups()
            if path not in changed:
                unknown.append(line)
            elif int(number) in changed[path]:
                blocking.append(line)
            else:
                baseline.append(line)
        elif match := summary.match(line):
            summaries.append(int(match[1]))
        elif line.strip() and not banner.match(line):
            unknown.append(line)

    # A nonzero CLI result without exactly its expected diagnostic report is a
    # tool/configuration/read failure, even when old Markdown issues also exist.
    if exit_code != 1 or unknown or summaries != [len(blocking) + len(baseline)] or not (blocking or baseline):
        print(output)
        print("markdownlint failed with unrecognized output, file paths, counts or exit status; failing safe.")
        return 2
    if baseline:
        print("Existing markdown issues outside changed lines (non-blocking):")
        for line in baseline:
            print(f"  {line}")
    if blocking:
        print("Markdown issues introduced on changed lines (blocking):")
        for line in blocking:
            print(f"  {line}")
        return 1
    print("No blocking markdown issues on changed lines.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base")
    parser.add_argument("exit_code", type=int)
    parser.add_argument("output", type=Path)
    parser.add_argument("files", nargs="+")
    args = parser.parse_args()
    try:
        return filter_output(args.output.read_text(encoding="utf-8"), args.exit_code, changed_lines(args.base, args.files))
    except (OSError, UnicodeError, subprocess.CalledProcessError) as exc:
        print(f"Cannot classify Markdown lint output safely: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
