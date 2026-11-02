import unittest

from required_gate import ALL_JOBS, FULL_JOBS, PR_ONLY_JOBS, validate


def result_set(full=False, docs_build=False, event="pull_request", windows=False):
    needs = {job: {"result": "skipped", "outputs": {}} for job in ALL_JOBS}
    needs["changes"] = {"result": "success", "outputs": {"full": str(full).lower(), "docs_build": str(docs_build).lower()}}
    needs["docs-style"]["result"] = "success"
    if full:
        for job in FULL_JOBS:
            needs[job]["result"] = "success"
        if event == "push":
            for job in PR_ONLY_JOBS:
                needs[job]["result"] = "skipped"
        needs["windows-clippy-tools-changes"]["outputs"]["run"] = str(windows).lower()
    if docs_build:
        needs["docs-build"]["result"] = "success"
    if windows:
        needs["windows-clippy-tools"]["result"] = "success"
    return needs


class GateTests(unittest.TestCase):
    def test_accepts_prose_and_code_expected_results_on_each_event(self):
        for event in ("pull_request", "push", "merge_group"):
            for full, build in ((False, False), (False, True), (True, False), (True, True)):
                with self.subTest(event=event, full=full, build=build):
                    self.assertEqual(validate(result_set(full, build, event), event), [])

    def test_accepts_selected_windows_checks(self):
        self.assertEqual(validate(result_set(full=True, windows=True), "pull_request"), [])

    def test_failure_cancellation_or_unexpected_skip_cannot_pass(self):
        for job in ("docs-style", "lint", "test", "web-permission-tests", "check-32bit", "build"):
            for result in ("failure", "cancelled", "skipped", "neutral", None):
                with self.subTest(job=job, result=result):
                    needs = result_set(full=True)
                    needs[job]["result"] = result
                    self.assertTrue(validate(needs, "pull_request"))

    def test_docs_build_is_required_when_selected(self):
        needs = result_set(docs_build=True)
        needs["docs-build"]["result"] = "skipped"
        self.assertTrue(validate(needs, "pull_request"))

    def test_classifier_failure_or_missing_output_is_fatal(self):
        for status in ("failure", "cancelled", "skipped"):
            needs = result_set()
            needs["changes"]["result"] = status
            self.assertTrue(validate(needs, "pull_request"))
        for output in ("full", "docs_build"):
            for value in (None, "", "True", True):
                needs = result_set()
                needs["changes"]["outputs"][output] = value
                self.assertTrue(validate(needs, "pull_request"))

    def test_missing_or_added_dependencies_cannot_silently_pass(self):
        for job in ALL_JOBS:
            needs = result_set()
            del needs[job]
            self.assertTrue(validate(needs, "pull_request"))
        needs = result_set()
        needs["unclassified"] = {"result": "success"}
        self.assertTrue(validate(needs, "pull_request"))

    def test_windows_detector_output_is_required_for_full_lane(self):
        needs = result_set(full=True)
        needs["windows-clippy-tools-changes"]["outputs"] = {}
        self.assertTrue(validate(needs, "pull_request"))

    def test_unselected_failure_is_not_ignored(self):
        needs = result_set()
        needs["lint"]["result"] = "failure"
        self.assertTrue(validate(needs, "pull_request"))


if __name__ == "__main__":
    unittest.main()
