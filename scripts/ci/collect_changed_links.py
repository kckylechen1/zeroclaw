#!/usr/bin/env python3
"""Collect links from changed Markdown files, or all tracked docs without a valid base.

The full-book source gate handles old inbound links after deletes/renames. This
command also checks source-relative links in repository Markdown outside mdBook.
Whole changed documents are parsed so reference definitions and code fences have
their real CommonMark context. HTTP URLs are collected, never fetched here.
"""

from __future__ import annotations

import argparse
from pathlib import Path
import subprocess
import sys

from docs_source_gate import BookSources, DocsError, markdown_links, read_text


def run_git(args: list[str], *, check: bool = True) -> subprocess.CompletedProcess[str]:
    proc = subprocess.run(["git", *args], check=False, capture_output=True, text=True)
    if check and proc.returncode:
        raise DocsError(f"git {' '.join(args)} failed: {proc.stderr.strip()}")
    return proc


def valid_base(base: str) -> bool:
    return bool(base) and run_git(["cat-file", "-e", f"{base}^{{commit}}"], check=False).returncode == 0


def supported_doc(path: str) -> bool:
    return Path(path).suffix in {".md", ".mdx"} or path == "LICENSE"


def quality_doc(path: str) -> bool:
    return path == "README.md" or (
        path.startswith("docs/book/src/") and Path(path).suffix in {".md", ".mdx"}
    )


def select_docs_files(base: str, provided: str = "", *, quality_only: bool = False) -> list[str]:
    explicit = provided.splitlines()
    if valid_base(base):
        paths = explicit or run_git(["diff", "--no-renames", "--name-only", "-z", base, "HEAD"]).stdout.split("\0")
    else:
        print("BASE_SHA missing or invalid; checking all tracked documentation.", file=sys.stderr)
        paths = [*run_git(["ls-files", "-z"]).stdout.split("\0"), *explicit]
    supported = quality_doc if quality_only else supported_doc
    selected = sorted({path for path in paths if path and supported(path)})
    if any("\n" in path or "\r" in path for path in selected):
        raise DocsError("Documentation paths containing newlines are unsupported")
    return selected


def deleted_files(base: str) -> set[str]:
    if not valid_base(base):
        return set()
    return set(run_git(["diff", "--no-renames", "--diff-filter=D", "--name-only", "-z", base, "HEAD"]).stdout.split("\0"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="")
    parser.add_argument("--docs-files", default="", help="Explicit newline-separated files (unioned with full scan if base is unavailable)")
    parser.add_argument("--list-quality", action="store_true", help="List the book/README style scope instead of checking links")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--http-output", type=Path)
    parser.add_argument("--check-local-targets", action="store_true")
    args = parser.parse_args()
    try:
        files = select_docs_files(args.base, args.docs_files, quality_only=args.list_quality)
        deleted = deleted_files(args.base)
        if args.list_quality:
            for path in files:
                if path not in deleted and not Path(path).is_file():
                    raise DocsError(f"Selected documentation file is missing: {path}")
            files = [path for path in files if path not in deleted]
            if files:
                print("\n".join(files))
            return 0
        if args.output is None:
            parser.error("--output is required unless --list-quality is used")
        repo = Path(run_git(["rev-parse", "--show-toplevel"]).stdout.strip()).resolve()
        book = BookSources(repo)
        if args.check_local_targets:
            book.check_book()
            book.report()
        selected = set(files)
        candidates = set(files)
        if args.check_local_targets:
            # Source READMEs and contribution guides may link to a book page or
            # asset that was deleted without changing the referring Markdown.
            candidates.update(
                name for name in run_git(["ls-files", "-z"]).stdout.split("\0")
                if supported_doc(name) and not (repo / name).resolve().is_relative_to(book.source)
            )
        links: set[str] = set()
        for name in sorted(candidates):
            path = (repo / name).resolve()
            if not path.is_relative_to(repo):
                raise DocsError(f"Documentation path escapes the repository: {name}")
            if name in deleted and not path.exists():
                continue  # Full-book gate checks the unchanged incoming links.
            text = read_text(path)  # Unknown missing files and invalid UTF-8 fail.
            parsed = markdown_links(text)
            if args.check_local_targets and not path.is_relative_to(book.source):
                for target in parsed:
                    book.check_target(path, target, in_book=False)
            # The full-book pass checked every published host context already,
            # including links inside shared snippets.
            if name in selected:
                links.update(target for target in parsed if target)
        args.output.write_text("".join(f"{link}\n" for link in sorted(links)), encoding="utf-8")
        http = sorted(link for link in links if link.startswith(("http://", "https://")))
        if args.http_output:
            args.http_output.write_text("".join(f"{link}\n" for link in http), encoding="utf-8")
        print(f"Collected {len(links)} link(s) from {len(files)} selected documentation file(s); {len(http)} HTTP(S) URL(s), availability not checked.")
        return 0
    except (DocsError, OSError, ValueError) as exc:
        print(f"Docs link collection failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
