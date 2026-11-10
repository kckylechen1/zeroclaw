#!/usr/bin/env python3
"""Reject missing checks and unexpected skips, as well as failures/cancellations."""

from __future__ import annotations

import json
import os
import sys


FULL_JOBS = {
    "fmt", "repo-structure", "lint", "windows-clippy-tools-changes", "build",
    "build-macos", "check", "check-32bit", "test", "security", "nix-eval",
    "nix-hash-drift", "installer-drift", "web-permission-tests",
}
PR_ONLY_JOBS = {"repo-structure", "nix-eval", "nix-hash-drift"}
ALL_JOBS = FULL_JOBS | {"changes", "docs-style", "docs-build", "windows-clippy-tools"}


def boolean_output(job: dict, name: str) -> bool:
    value = job.get("outputs", {}).get(name)
    if value not in {"true", "false"}:
        raise ValueError(f"missing or invalid {name} output")
    return value == "true"


def validate(needs: dict, event_name: str) -> list[str]:
    if not isinstance(needs, dict) or set(needs) != ALL_JOBS:
        return ["Required gate dependency set is incomplete or unknown"]
    if event_name not in {"pull_request", "push", "merge_group"}:
        return [f"Unsupported event: {event_name}"]
    if needs["changes"].get("result") != "success":
        return ["Change classification did not succeed"]
    try:
        full = boolean_output(needs["changes"], "full")
        docs_build = boolean_output(needs["changes"], "docs_build")
        windows = full and boolean_output(needs["windows-clippy-tools-changes"], "run")
        selfhosted_ok = boolean_output(needs["changes"], "selfhosted_ok")
    except (TypeError, ValueError) as error:
        return [str(error)]
    expected = {job: "skipped" for job in ALL_JOBS}
    expected.update({"changes": "success", "docs-style": "success"})
    if full:
        for job in FULL_JOBS:
            expected[job] = "success"
        if event_name == "push":
            for job in PR_ONLY_JOBS:
                expected[job] = "skipped"
        # The self-hosted lane may be skipped only for fork PR heads; on push
        # and merge_group events the classifier output cannot legitimately be
        # false, so a skipped lane there is a workflow inconsistency.
        if event_name == "pull_request" and not selfhosted_ok:
            expected["build-macos"] = "skipped"
    if docs_build:
        expected["docs-build"] = "success"
    if windows:
        expected["windows-clippy-tools"] = "success"
    return [
        f"{job}: expected {wanted}, got {needs[job].get('result', 'missing')}"
        for job, wanted in sorted(expected.items())
        if needs[job].get("result") != wanted
    ]


def main() -> int:
    try:
        errors = validate(json.loads(os.environ["NEEDS_JSON"]), os.environ["GITHUB_EVENT_NAME"])
    except (KeyError, ValueError, TypeError, AttributeError) as error:
        errors = [f"Invalid gate inputs: {error}"]
    if errors:
        for error in errors:
            print(f"::error::{error}", file=sys.stderr)
        return 1
    print("All selected checks succeeded; every skip matches the change scope.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
