#!/usr/bin/env python3
"""Run two new integration-CLI migrations with unchanged, previously built fixtures.

This runner never builds a binary, regenerates fixtures, or modifies old evidence.
Invoke only after the parent has confirmed the combined CLI build is complete.
"""

import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

OUT = Path(__file__).resolve().parent
REPO = OUT.parent / "zeroclaw-integration-eight"
LEGACY = OUT.parent / "legacy-fixtures"
HEAD = os.environ["REVIEW_SOURCE_COMMIT"]
EXPECTED_HEAD = "ff9233ed4d34913dd516de83ca5d3eeb047d1e98"
LOCAL_EQUIVALENT_HEAD = "e02a6654ada83de3f3c6594b8d084eda8897ada6"
LEGACY_RECEIPT_SHA = "6ad5165a7e3095e95cf4404c88b0ef936b26d2c4dad2648f037018c86a6f69b9"
TREE = "d95dad3aba0b62ecbf3fc039f2587d0afc8e4ba6"
BASE = "10cdae7eec403a5bbbd68cc3066a5386b41033e3"
HARNESS_SHA = "9b9bea5093e7b7d1f5472cca5b5cd5daf3893febe5c9777e4ea169e415658599"


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def git(*args):
    return subprocess.check_output(["git", "-C", str(REPO), *args], text=True).strip()


def write_new_json(path, data):
    with path.open("x") as stream:
        stream.write(json.dumps(data, indent=2) + "\n")


def fixture_census():
    return {
        path.name: {
            "sha256": digest(path), "size": path.stat().st_size,
            "inode": path.stat().st_ino, "mtime_ns": path.stat().st_mtime_ns,
        }
        for path in sorted((LEGACY / "fixtures").iterdir()) if path.is_file()
    }


def main():
    require(HEAD == EXPECTED_HEAD, "REVIEW_SOURCE_COMMIT differs from pinned published source")
    build_path = OUT / "build-receipt.json"
    build = json.loads(build_path.read_text())
    composition = json.loads((OUT / "composition.json").read_text())
    binary = OUT / "zeroclaw"
    harness = OUT / "validate-kernel-cli-e2e.py"
    result_dir = OUT / "migration-e2e"
    require(not result_dir.exists(), "fresh migration output is required")
    require(build["exit_code"] == 0, "combined binary build did not pass")
    require(build["head"] == HEAD and build["tree"] == TREE and build["base"] == BASE,
            "build source identity differs")
    require(composition["head"] == HEAD and composition["local_equivalent_head"] == LOCAL_EQUIVALENT_HEAD
            and composition["tree"] == TREE,
            "integration source composition differs")
    require(build["pr_heads"] == composition["steps"], "build does not bind all eight PR heads")
    require(build["features"] == "default + memory-tachi", "memory feature build differs")
    require(git("rev-parse", "HEAD") == HEAD and git("rev-parse", "HEAD^{tree}") == TREE,
            "current integration source differs")
    require(not git("status", "--porcelain"), "integration source is dirty")
    require(git("rev-parse", "HEAD:Cargo.lock") == build["lock_blob"], "build lockfile differs")
    require(Path(build["binary"]).resolve() == binary.resolve(), "unexpected build artifact")
    require(digest(binary) == build["binary_sha256"], "combined binary changed")
    require(digest(harness) == HARNESS_SHA, "reviewed migration oracle changed")

    old_receipt_path = LEGACY / "result.json"
    old_receipt_hash = digest(old_receipt_path)
    require(old_receipt_hash == LEGACY_RECEIPT_SHA, "pinned legacy generator receipt differs")
    old_receipt = json.loads(old_receipt_path.read_text())
    require(old_receipt["result"] == "PASS" and old_receipt["generator_exit_code"] == 0
            and old_receipt["build_exit_code"] == 0, "legacy generator provenance is incomplete")
    require(old_receipt["generator_revision"] == "eba0c48617e8820831bfd22f91d269c6d70f409f",
            "legacy generator revision differs")
    # The original generator executable was verified when preparing the bundle.
    # CI authenticates the unchanged receipt and every original fixture below;
    # it does not claim to rebuild or execute the legacy generator.
    before = fixture_census()
    require(set(before) == {item["name"] for item in old_receipt["files"]},
            "legacy fixture inventory changed")
    for item in old_receipt["files"]:
        found = before[item["name"]]
        require(found["sha256"] == item["sha256"] and found["size"] == item["size"],
                "legacy fixture bytes differ: " + item["name"])

    command = [sys.executable, str(harness), "--binary", str(binary),
               "--fixtures", str(LEGACY / "fixtures"), "--repo", str(REPO),
               "--source-sha", HEAD, "--binary-sha256", build["binary_sha256"],
               "--output", str(result_dir)]
    binding = {
        "description": "New two-role CLI migration execution against the combined eight-PR binary",
        "head": HEAD, "tree": TREE, "base": BASE,
        "build_receipt": str(build_path), "build_receipt_sha256": digest(build_path),
        "binary_sha256": build["binary_sha256"], "harness_sha256": HARNESS_SHA,
        "legacy_fixture_provenance": {
            "reused_without_regeneration": True, "receipt": str(old_receipt_path),
            "receipt_sha256": old_receipt_hash, "original_finished_at": old_receipt["finished_at"],
            "generator_revision": old_receipt["generator_revision"],
            "generator_binary_sha256": old_receipt["binary_sha256"],
            "generator_executable_checked_in_this_run": False,
            "pinned_original_receipt_verified": True,
        },
        "command": command, "source_files_before": before,
        "started_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    }
    write_new_json(OUT / "migration-run-binding.json", binding)
    started = time.monotonic()
    with (OUT / "migration-run.log").open("x") as log:
        completed = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                   stdin=subprocess.DEVNULL, timeout=420)
    after = fixture_census()
    result = json.loads((result_dir / "result.json").read_text())
    summary = {
        "exit_code": completed.returncode, "result": result["result"],
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "head": HEAD, "tree": TREE, "binary_sha256": build["binary_sha256"],
        "source_files_unchanged": before == after,
        "legacy_receipt_unchanged": digest(old_receipt_path) == old_receipt_hash,
        "result_file": str(result_dir / "result.json"),
        "finished_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    }
    write_new_json(OUT / "migration-execution-receipt.json", summary)
    print(json.dumps(summary, indent=2), flush=True)
    require(summary["source_files_unchanged"] and summary["legacy_receipt_unchanged"],
            "preserved legacy inputs changed")
    require(completed.returncode == 0 and result["result"] == "PASS", "migration E2E failed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
