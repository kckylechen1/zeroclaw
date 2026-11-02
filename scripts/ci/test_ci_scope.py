import contextlib
import io
from pathlib import Path
import subprocess
import tempfile
import unittest

from change_scope import classify, event_base, parse_changes, prose_path


class ScopeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.git("init", "-q")
        self.git("config", "user.name", "Fixture")
        self.git("config", "user.email", "fixture@example.invalid")
        self.write("README.md", "initial\n")
        self.write("docs/book/src/page.md", "page\n")
        self.write("src/lib.rs", "fn initial() {}\n")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.root, stderr=subprocess.DEVNULL).decode().strip()

    def write(self, path, content):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content)

    def commit(self):
        self.git("add", "-A")
        self.git("commit", "-qm", "fixture")
        return self.git("rev-parse", "HEAD")

    def scope(self, event="pull_request", base=None):
        base = self.base if base is None else base
        payload = {"pull_request": {"base": {"sha": base}}, "before": base, "merge_group": {"base_sha": base}}
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return classify(event, payload, self.root)

    def test_readme_and_adr_only_skip_code_on_all_supported_events(self):
        self.write("README.md", "updated\n")
        self.write("docs/book/src/architecture/decisions/ADR-fixture.md", "proposal\n")
        self.commit()
        for event in ("pull_request", "push", "merge_group"):
            with self.subTest(event=event):
                self.assertEqual(self.scope(event), {"full": "false", "docs_build": "false", "base_sha": self.base})

    def test_summary_requires_render_without_unrelated_runtime_matrix(self):
        self.write("docs/book/src/SUMMARY.md", "# Summary\n")
        self.commit()
        self.assertEqual(self.scope()["full"], "false")
        self.assertEqual(self.scope()["docs_build"], "true")

    def test_mixed_code_and_docs_keep_full_lane(self):
        self.write("README.md", "updated\n")
        self.write("src/lib.rs", "fn changed() {}\n")
        self.commit()
        self.assertEqual(self.scope()["full"], "true")

    def test_book_generator_inputs_require_real_render(self):
        for path in ("docs/book/peer-groups.toml", "docs/book/feature-matrix-parity.toml", "docs/book/env-vars.toml", "src/main.rs", "web/src/contexts/themes.json"):
            with self.subTest(path=path):
                base = self.git("rev-parse", "HEAD")
                self.write(path, "changed\n")
                self.commit()
                self.assertEqual(self.scope(base=base)["docs_build"], "true")

    def test_added_and_removed_custom_directives_reach_real_renderer(self):
        for directive in ("{{#peer-group definitely_missing}}", "{{ #include snippet.md}}"):
            with self.subTest(directive=directive):
                base = self.git("rev-parse", "HEAD")
                self.write("docs/book/src/page.md", directive + "\n")
                directive_commit = self.commit()
                self.assertEqual(self.scope(base=base)["full"], "false")
                self.assertEqual(self.scope(base=base)["docs_build"], "true")
                self.write("docs/book/src/page.md", "plain prose again\n")
                self.commit()
                self.assertEqual(self.scope(base=directive_commit)["docs_build"], "true")

    def test_runtime_markdown_and_unknown_paths_are_not_prose(self):
        for path in ("crates/zeroclaw-runtime/src/skills/review_prompt.md", "crates/zeroclaw-runtime/src/agent/personality_templates/SOUL.md", "AGENTS.md", ".github/CODEOWNERS", "unknown.md", "../README.md"):
            with self.subTest(path=path):
                self.assertFalse(prose_path(path))

    def test_rename_from_code_to_docs_does_not_hide_deleted_code(self):
        self.git("mv", "src/lib.rs", "docs/book/src/moved.md")
        self.commit()
        self.assertEqual(self.scope()["full"], "true")

    def test_deleted_doc_remains_in_documentation_lane(self):
        (self.root / "docs/book/src/page.md").unlink()
        self.commit()
        self.assertEqual(self.scope()["full"], "false")

    def test_stack_uses_actual_parent_instead_of_master(self):
        self.write("src/lib.rs", "fn parent_change() {}\n")
        parent = self.commit()
        self.write("README.md", "child prose\n")
        self.commit()
        self.assertEqual(self.scope(base=parent)["full"], "false")
        self.assertEqual(self.scope(base=self.base)["full"], "true")

    def test_unavailable_zero_invalid_or_empty_diff_keeps_every_check(self):
        for base in ("0" * 40, "1" * 40, "--help", "", self.base):
            with self.subTest(base=base):
                self.assertEqual(self.scope(base=base), {"full": "true", "docs_build": "true", "base_sha": ""})

    def test_symlink_or_executable_markdown_is_not_exempt(self):
        target = self.root / "README.md"
        target.unlink()
        target.symlink_to("src/lib.rs")
        self.commit()
        self.assertEqual(self.scope()["full"], "true")

    def test_nonancestor_push_falls_back_to_full(self):
        self.git("checkout", "-qb", "other")
        self.write("README.md", "other\n")
        other = self.commit()
        self.git("checkout", "--detach", self.base)
        self.write("README.md", "different\n")
        self.commit()
        self.assertEqual(self.scope("push", other)["base_sha"], "")

    def test_nul_parser_rejects_partial_or_rename_records(self):
        for raw in (b"broken", b":100644 100644 a b R100\0old\0new\0", b":100644 100644 a b M\0"):
            with self.subTest(raw=raw):
                with self.assertRaises(ValueError):
                    parse_changes(raw)

    def test_event_selection_never_uses_default_branch(self):
        payload = {"pull_request": {"base": {"sha": "parent"}}, "before": "before", "merge_group": {"base_sha": "queue-base"}}
        self.assertEqual(event_base("pull_request", payload), "parent")
        self.assertEqual(event_base("push", payload), "before")
        self.assertEqual(event_base("merge_group", payload), "queue-base")


if __name__ == "__main__":
    unittest.main()
