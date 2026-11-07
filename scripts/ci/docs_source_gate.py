#!/usr/bin/env python3
"""Check authored mdBook navigation, includes and publication-relative link targets.

This is deliberately a source check, not a substitute for `cargo mdbook build`.
Generated inputs have named owners below; anchors, remote availability and custom
preprocessor output are outside this gate's coverage.
"""

from __future__ import annotations

import argparse
from html.parser import HTMLParser
from pathlib import Path
import re
import sys
import tomllib
from urllib.parse import unquote, urlsplit

try:
    from markdown_it import MarkdownIt
except ImportError as exc:
    raise SystemExit(
        "Docs checks require: python3 -m pip install -r scripts/ci/docs-requirements.txt"
    ) from exc


# Exact generated inputs, not a wildcard exemption for missing Markdown files.
GENERATED_DOC_TARGETS = {
    "docs/book/src/reference/cli.md": "xtask/src/cmd/mdbook/refs.rs",
    "docs/book/src/reference/config.md": "xtask/src/cmd/mdbook/refs.rs",
    "docs/book/src/_snippets/feature-matrix-channels.md": "xtask/src/cmd/mdbook/feature_matrix.rs",
    "docs/book/src/_snippets/feature-matrix-providers.md": "xtask/src/cmd/mdbook/feature_matrix.rs",
    "docs/book/src/_snippets/feature-matrix-tools.md": "xtask/src/cmd/mdbook/feature_matrix.rs",
}
# These source-authored links are owned by the full rustdoc build. Other absolute
# site paths are not silently exempted. Add an entry with its real generator.
GENERATED_SITE_TARGETS = {
    f"/api/{crate}/index.html": "xtask/src/cmd/mdbook/refs.rs::build_api"
    for crate in (
        "zeroclaw", "zeroclaw_api", "zeroclaw_config", "zeroclaw_runtime",
        "zeroclaw_providers", "zeroclaw_gateway_client", "zeroclaw_gateway",
        "zeroclaw_tools", "zeroclaw_memory", "zeroclaw_infra",
    )
}
INCLUDE_RE = re.compile(r"(?<!\\)\{\{\s*#(include|rustdoc_include|playground)\s+([^}]+)\}\}")
MARKDOWN = MarkdownIt("commonmark")


class DocsError(ValueError):
    pass


class HtmlLinks(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.links: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        for name, value in attrs:
            if value is not None and ((tag == "a" and name == "href") or name == "src"):
                self.links.append(value)


def markdown_links(text: str) -> list[str]:
    """Use CommonMark tokens so examples, escaped syntax and references stay distinct."""
    links: list[str] = []

    def visit(tokens) -> None:
        for token in tokens:
            if token.type == "link_open":
                links.append(token.attrGet("href") or "")
            elif token.type == "image":
                links.append(token.attrGet("src") or "")
            elif token.type in {"html_inline", "html_block"}:
                html = HtmlLinks()
                html.feed(token.content)
                html.close()
                links.extend(html.links)
            if token.children:
                visit(token.children)

    visit(MARKDOWN.parse(text))
    return links


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as exc:
        raise DocsError(f"Cannot read {path}: {exc}") from exc


def remote_link(target: str) -> bool:
    return target.startswith("//") or urlsplit(target).scheme.lower() in {"http", "https", "mailto", "tel"}


class BookSources:
    def __init__(self, repo: Path) -> None:
        self.repo = repo.resolve()
        self.book = self.repo / "docs/book"
        try:
            config = tomllib.loads(read_text(self.book / "book.toml"))
            source = config.get("book", {}).get("src", "src")
            if not isinstance(source, str):
                raise DocsError("book.src must be a string")
            self.source = (self.book / source).resolve()
        except tomllib.TOMLDecodeError as exc:
            raise DocsError(f"Invalid book.toml: {exc}") from exc
        if not self.source.is_relative_to(self.repo) or not self.source.is_dir():
            raise DocsError(f"Book source must be an existing directory in the repository: {self.source}")
        self.generated = {self.repo / path: owner for path, owner in GENERATED_DOC_TARGETS.items()}
        self.deferred: set[str] = set()
        self.published: set[Path] = set()
        self.outputs: dict[Path, Path] = {}
        self.checked_links = 0

    def known_generated(self, path: Path) -> bool:
        if path in self.generated:
            self.deferred.add(f"{path.relative_to(self.repo)} ({self.generated[path]})")
            return True
        return False

    def load_summary(self) -> None:
        summary = self.source / "SUMMARY.md"
        for target in markdown_links(read_text(summary)):
            if not target:  # mdBook permits label-only/draft navigation entries.
                continue
            url = urlsplit(target)
            if url.scheme or url.netloc or url.query or url.fragment or url.path.startswith("/"):
                raise DocsError(f"SUMMARY.md chapter must be a source-relative Markdown path: {target}")
            path = (self.source / unquote(url.path)).resolve()
            if not path.is_relative_to(self.source) or path.suffix not in {".md", ".mdx"}:
                raise DocsError(f"SUMMARY.md chapter escapes the book or is not Markdown: {target}")
            if path in self.published:
                raise DocsError(f"SUMMARY.md lists a chapter twice: {target}")
            if not path.is_file() and not self.known_generated(path):
                raise DocsError(f"SUMMARY.md chapter is missing: {target}")
            self.published.add(path)
            output = path.with_name("index.html") if path.name == "README.md" else path.with_suffix(".html")
            if output in self.outputs:
                raise DocsError(f"SUMMARY.md chapters produce the same HTML path: {target}")
            self.outputs[output] = path
        if not self.published:
            raise DocsError("SUMMARY.md has no publishable chapters")

    def expand_includes(self, text: str, source: Path, stack: tuple[Path, ...] = ()) -> str:
        if source in stack or len(stack) >= 32:
            raise DocsError(f"Recursive include: {' -> '.join(str(p) for p in (*stack, source))}")
        def expand(match: re.Match[str]) -> str:
            kind = match.group(1)
            # Match mdBook 0.5's links preprocessor: first token is the path;
            # remaining tokens are playground attributes (ignored by include).
            specification, *attributes = match.group(2).split()
            raw, *selectors = specification.split(":")
            target = (source.parent / raw).resolve()
            if not target.is_relative_to(self.repo):
                raise DocsError(f"Include outside the repository: {source} -> {raw}")
            if not target.is_file():
                if self.known_generated(target):
                    return match.group()  # Deferred source, never a fabricated generated page.
                raise DocsError(f"Missing include: {source} -> {raw}")
            lines = read_text(target).splitlines(keepends=True)
            selected = set(range(len(lines)))
            anchor_selection = False
            if selectors and kind != "playground":
                selection = ":".join(selectors)
                if re.fullmatch(r"\d*(?::\d*)?", selection):
                    bounds = selection.split(":")
                    start = int(bounds[0] or "1")
                    end = int(bounds[1]) if len(bounds) == 2 and bounds[1] else (len(lines) if len(bounds) == 2 or not selection else start)
                    if start < 1 or end < start or end > len(lines):
                        raise DocsError(f"Invalid include range: {source} -> {specification}")
                    selected = set(range(start - 1, end))
                else:
                    anchor_selection = True
                    begin_re = re.compile(r"\bANCHOR:\s*" + re.escape(selection) + r"(?![\w-])")
                    end_re = re.compile(r"\bANCHOR_END:\s*" + re.escape(selection) + r"(?![\w-])")
                    start = next((i for i, line in enumerate(lines) if begin_re.search(line)), None)
                    end = next((i for i, line in enumerate(lines) if end_re.search(line)), None)
                    if start is None or end is None or end <= start:
                        raise DocsError(f"Missing or invalid include anchor: {source} -> {specification}")
                    selected = set(range(start + 1, end))
            content = "".join(
                line if i in selected else "# " + line
                for i, line in enumerate(lines)
                if (kind in {"rustdoc_include", "playground"} or i in selected)
                and (not anchor_selection or not re.search(r"\bANCHOR(?:_END)?:\s*[\w-]+", line))
            )
            content = self.expand_includes(content, target, (*stack, source))
            if kind == "playground":
                fence = "`" * max(3, max((len(run) + 1 for run in re.findall(r"`+", content)), default=3))
                content = f"{fence}rust,{','.join(attributes)}\n{content}\n{fence}\n"
            return content

        return INCLUDE_RE.sub(expand, text)

    def check_target(self, source: Path, target: str, *, in_book: bool) -> None:
        if not target or remote_link(target):
            return
        url = urlsplit(target)
        if url.scheme or url.netloc:
            raise DocsError(f"Unsupported local link: {source} -> {target}")
        if not url.path:  # Anchor existence needs rendered output, not a source-path guess.
            return
        if in_book and url.path in GENERATED_SITE_TARGETS:
            self.deferred.add(f"{url.path} ({GENERATED_SITE_TARGETS[url.path]})")
            return
        if in_book and url.path.startswith("/"):
            raise DocsError(f"Unknown site-absolute link: {source} -> {target}")
        path = ((self.repo if url.path.startswith("/") else source.parent) / unquote(url.path).lstrip("/")).resolve()
        boundary = self.source if in_book else self.repo
        if not path.is_relative_to(boundary):
            # Adapted from upstream #9581 (49ef92a6): a source-tree file outside
            # mdBook is not a page that the deployed book can link to.
            raise DocsError(f"Relative link escapes {'published book' if in_book else 'repository'}: {source} -> {target}")
        if in_book and (url.path.endswith("/") or path.is_dir()):
            path = path / "index.html"
        if in_book and path.suffix in {".md", ".mdx"}:
            if path not in self.published:
                raise DocsError(f"Linked Markdown is not a SUMMARY.md chapter: {source} -> {target}")
            if self.known_generated(path):
                return
        elif in_book and path.suffix == ".html" and path in self.outputs:
            path = self.outputs[path]
            if self.known_generated(path):
                return
        if not path.is_file() and not (not in_book and path.is_dir()):
            raise DocsError(f"Missing local link target: {source} -> {target}")
        self.checked_links += 1

    def check_document(self, path: Path, *, in_book: bool) -> list[str]:
        text = read_text(path)
        if in_book:
            text = self.expand_includes(text, path)
        links = markdown_links(text)
        for target in links:
            self.check_target(path, target, in_book=in_book)
        return links

    def check_book(self) -> None:
        self.load_summary()
        for path in sorted(self.published):
            if not path.is_file() and self.known_generated(path):
                continue
            self.check_document(path, in_book=True)

    def report(self) -> None:
        print(f"Docs source gate: {len(self.published)} chapters; {self.checked_links} local targets checked.")
        if self.deferred:
            print("Generated targets deferred to the real docs build:")
            for target in sorted(self.deferred):
                print(f"  {target}")
        print("Coverage: authored navigation, includes and local target paths; no rendered anchors, custom preprocessor output or HTTP availability claim.")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path.cwd())
    args = parser.parse_args()
    try:
        book = BookSources(args.repo)
        book.check_book()
        book.report()
        return 0
    except (DocsError, OSError, ValueError) as exc:
        print(f"Docs source gate failed: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
