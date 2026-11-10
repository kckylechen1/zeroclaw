#!/usr/bin/env python3
"""Build the pinned eight-change source and run its bounded real-process checks.

This script is CI-only. It never reuses an existing CLI, changes tracked source,
merges a PR, or deploys. The output archive contains synthetic evidence only.
"""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import traceback
import zipfile


HEAD = "ff9233ed4d34913dd516de83ca5d3eeb047d1e98"
LOCAL_HEAD = "e02a6654ada83de3f3c6594b8d084eda8897ada6"
TREE = "d95dad3aba0b62ecbf3fc039f2587d0afc8e4ba6"
BASE = "10cdae7eec403a5bbbd68cc3066a5386b41033e3"
ORACLE_SHA = "9b9bea5093e7b7d1f5472cca5b5cd5daf3893febe5c9777e4ea169e415658599"
LEGACY_RECEIPT_SHA = "6ad5165a7e3095e95cf4404c88b0ef936b26d2c4dad2648f037018c86a6f69b9"


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def read_json(path):
    return json.loads(path.read_text())


def git(repo, *args):
    return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()


def source_identity(repo):
    require(git(repo, "rev-parse", "HEAD") == HEAD, "source HEAD differs")
    require(git(repo, "rev-parse", "HEAD^{tree}") == TREE, "source tree differs")
    require(not git(repo, "status", "--porcelain"), "source checkout is dirty")
    require(git(repo, "rev-parse", BASE + "^{commit}") == BASE, "base commit unavailable")
    return {"head": HEAD, "tree": TREE, "base": BASE,
            "local_equivalent_head": LOCAL_HEAD, "clean": True,
            "lock_blob": git(repo, "rev-parse", "HEAD:Cargo.lock")}


def run_logged(command, log_path, cwd, environment, timeout):
    started = time.monotonic()
    result = {"command": command, "started_at": now(), "timeout_seconds": timeout,
              "timed_out": False, "log": str(log_path)}
    with log_path.open("x") as log:
        process = subprocess.Popen(command, cwd=cwd, env=environment, stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            result["exit_code"] = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            result["timed_out"] = True
            os.killpg(process.pid, signal.SIGTERM)
            try:
                result["exit_code"] = process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                result["exit_code"] = process.wait(timeout=10)
    result.update(finished_at=now(), seconds=round(time.monotonic() - started, 3),
                  log_sha256=digest(log_path))
    return result


def archive_evidence(out, legacy):
    """Explicit whitelist; no executable, target, token or unrelated store."""
    paths = [(out / name, name) for name in (
        "status.json", "source-binding.json", "build-receipt.json", "build.log",
        "gateway-run.log", "migration-runner.log", "migration-run.log", "migration-run-binding.json",
        "migration-execution-receipt.json", "fixture-extraction.json", "composition.json",
        "gateway_e2e.py", "run_migration_e2e.py", "validate-kernel-cli-e2e.py", "build_and_run.py")]
    paths += [(out / "gateway-run-1" / name, "gateway-run-1/" + name) for name in (
        "result.json", "invocation.json", "persisted-before-delete.json", "provider-requests.json",
        "ws-frames.json", "gateway.log", "config/config.toml", "workspace/context.txt",
        "config/data/state/runtime-trace.jsonl", "config/data/sessions/sessions.db",
        "config/data/sessions/sessions.db-wal", "config/data/sessions/sessions.db-shm")]
    paths.append((out / "migration-e2e/result.json", "migration-e2e/result.json"))
    for role in ("memory", "companion"):
        for name in ("result.json", "invocation.json", "stdout.json", "stderr.log",
                     "migrated.db", "migrated.db-wal", "migrated.db-shm", "migrated.db.migration-marker"):
            paths.append((out / "migration-e2e" / role / name, f"migration-e2e/{role}/{name}"))
        for suffix in ("-schema-v38.json", "-v38.db", "-v38.db.migration-marker", "-v38.json"):
            name = role + suffix
            paths.append((legacy / "fixtures" / name, "legacy-fixtures/fixtures/" + name))
    paths.append((legacy / "result.json", "legacy-fixtures/result.json"))
    entries = []
    total = 0
    for path, name in paths:
        if not path.exists():
            continue
        require(path.is_file() and not path.is_symlink(), "nonregular evidence: " + name)
        before = path.stat()
        data = path.read_bytes()
        after = path.stat()
        require((before.st_size, before.st_mtime_ns) == (after.st_size, after.st_mtime_ns),
                "evidence changed during archive read: " + name)
        require(len(data) <= 20 * 1024 * 1024, "evidence file exceeded bounded size: " + name)
        total += len(data)
        require(total <= 80 * 1024 * 1024, "evidence collection exceeded bounded size")
        entries.append((name, data, hashlib.sha256(data).hexdigest()))
    manifest = {"format": "zeroclaw.eight-change-ci-evidence/v1", "created_at": now(),
                "source_head": HEAD, "source_tree": TREE, "payload_count": len(entries),
                "payload_bytes": total, "binary_included": False,
                "entries": [{"path": name, "bytes": len(data), "sha256": checksum}
                            for name, data, checksum in entries]}
    manifest_bytes = (json.dumps(manifest, indent=2) + "\n").encode()
    partial, final = out / "evidence.partial.zip", out / "evidence.zip"
    require(not partial.exists() and not final.exists(), "archive output already exists")
    with zipfile.ZipFile(partial, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
        for name, data, _ in entries:
            archive.writestr(name, data)
        archive.writestr("MANIFEST.json", manifest_bytes)
    with zipfile.ZipFile(partial) as archive:
        require(archive.testzip() is None, "archive CRC failure")
        require(len(archive.namelist()) == len(entries) + 1, "archive entry count differs")
        for name, data, checksum in entries:
            actual = archive.read(name)
            require(len(actual) == len(data) and hashlib.sha256(actual).hexdigest() == checksum,
                    "archive payload mismatch: " + name)
        require(archive.read("MANIFEST.json") == manifest_bytes, "archive manifest differs")
    partial.rename(final)
    return {"path": str(final), "sha256": digest(final), "bytes": final.stat().st_size,
            "payload_count": len(entries), "crc_and_payload_hashes_verified": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, required=True)
    args = parser.parse_args()
    out = Path(__file__).resolve().parent
    workspace = args.workspace.resolve()
    require(args.workspace.is_absolute() and out == workspace / "evidence",
            "scripts must run from the explicit workspace/evidence directory")
    require(not (out / "status.json").exists(), "fresh evidence directory required")
    repo, legacy, target = (workspace / name for name in
                            ("zeroclaw-integration-eight", "legacy-fixtures", "target"))
    state = {"format": "zeroclaw.eight-change-ci-acceptance/v1", "status": "RUNNING",
             "started_at": now(), "source_head": HEAD, "source_tree": TREE,
             "local_equivalent_head": LOCAL_HEAD, "build": {"status": "NOT_RUN"},
             "gateway": {"status": "NOT_RUN", "planned_checks": 30, "executed_checks": 0},
             "migration": {"status": "NOT_RUN", "planned_roles": 2, "executed_roles": 0},
             "limitations": ["Synthetic loopback providers; no live browser or paid-provider acceptance.",
                             "Original cancellation observation-window and vec0 oracle limits remain.",
                             "Legacy generator was not rebuilt or executed in this CI run."]}
    write_json(out / "status.json", state)
    try:
        require(os.environ.get("REVIEW_SOURCE_COMMIT") == HEAD, "pinned source env differs")
        require(os.environ.get("REVIEW_SOURCE_TREE", TREE) == TREE, "pinned tree env differs")
        identity = source_identity(repo)
        composition = read_json(out / "composition.json")
        require(composition["head"] == HEAD and composition["tree"] == TREE
                and composition["base"] == BASE and composition["local_equivalent_head"] == LOCAL_HEAD,
                "composition identity differs")
        require(len(composition["steps"]) == 8, "composition must have eight source increments")
        require(digest(out / "validate-kernel-cli-e2e.py") == ORACLE_SHA, "migration oracle changed")
        require(digest(legacy / "result.json") == LEGACY_RECEIPT_SHA, "legacy receipt differs")
        require(not (target / "debug/zeroclaw").exists() and not (out / "zeroclaw").exists(),
                "pre-existing CLI artifact is forbidden")
        require(not target.exists() or not any(target.iterdir()), "fresh build target required")
        target.mkdir(exist_ok=True)
        temp = workspace / "build-tmp"
        temp.mkdir(exist_ok=True)
        environment = dict(os.environ)
        environment.update(CARGO_TARGET_DIR=str(target), CARGO_BUILD_JOBS="1", CARGO_INCREMENTAL="0",
                           CARGO_PROFILE_DEV_OPT_LEVEL="0", CARGO_PROFILE_TEST_OPT_LEVEL="0",
                           CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0",
                           RUST_MIN_STACK="8388608", TMPDIR=str(temp), REVIEW_SOURCE_COMMIT=HEAD)
        environment.pop("CARGO_NET_OFFLINE", None)
        rustc = subprocess.check_output(["rustc", "--version"], cwd=repo, env=environment, text=True).strip()
        require(rustc.startswith("rustc 1.96.1 "), "Rust toolchain differs")
        # Matches the existing CI placeholder; does not fabricate a web bundle.
        placeholder = repo / "web/dist/.gitkeep"
        placeholder.parent.mkdir(parents=True, exist_ok=True)
        placeholder.touch(exist_ok=True)
        source_identity(repo)
        binding = dict(identity, rustc_version=rustc, recorded_at=now(),
                       validation_head=os.environ.get("REVIEW_VALIDATION_HEAD"),
                       github_run_id=os.environ.get("GITHUB_RUN_ID"),
                       github_run_attempt=os.environ.get("GITHUB_RUN_ATTEMPT"),
                       build_target_was_empty=True, web_placeholder="web/dist/.gitkeep only; no browser bundle",
                       scripts={name: digest(out / name) for name in
                                ("gateway_e2e.py", "run_migration_e2e.py", "validate-kernel-cli-e2e.py", "build_and_run.py")})
        write_json(out / "source-binding.json", binding)
        command = ["scripts/dev/cargo-local.sh", "--config", "profile.dev.package.zeroclaw-config.codegen-units=256",
                   "build", "--locked", "-p", "zeroclawlabs", "--features", "memory-tachi", "--bin", "zeroclaw"]
        receipt = dict(identity, command=command, pr_heads=composition["steps"],
                       features="default + memory-tachi", toolchain="1.96.1", rustc_version=rustc,
                       status="RUNNING", started_at=now(), binary_produced=False,
                       profile={"jobs": 1, "incremental": False, "dev_opt_level": 0,
                                "test_opt_level": 0, "debug_info": 0, "zeroclaw_config_codegen_units": 256},
                       cargo_network="online dependency resolution with --locked; no offline cache assumption")
        write_json(out / "build-receipt.json", receipt)
        build = run_logged(command, out / "build.log", repo, environment, 2700)
        receipt.update(build)
        receipt["status"] = "PASS" if build["exit_code"] == 0 and not build["timed_out"] else "FAIL"
        state["build"] = dict(build, status=receipt["status"], binary_produced=False)
        write_json(out / "build-receipt.json", receipt)
        write_json(out / "status.json", state)
        require(receipt["status"] == "PASS", "combined build failed; E2E did not execute")
        source_identity(repo)
        built = target / "debug/zeroclaw"
        require(built.is_file() and not built.is_symlink(), "successful build produced no expected CLI")
        binary = out / "zeroclaw"
        shutil.copy2(built, binary)
        require(digest(built) == digest(binary), "copied CLI differs from build output")
        receipt.update(binary=str(binary), binary_sha256=digest(binary), size=binary.stat().st_size,
                       binary_produced=True, source_clean_after_build=True)
        state["build"].update(binary_produced=True, binary_sha256=receipt["binary_sha256"])
        write_json(out / "build-receipt.json", receipt)
        write_json(out / "status.json", state)
        gateway_command = [sys.executable, str(out / "gateway_e2e.py"), "--binary", str(binary),
                           "--run-dir", str(out / "gateway-run-1"), "--source-commit", HEAD]
        gateway = run_logged(gateway_command, out / "gateway-run.log", workspace, environment, 210)
        gateway_file = out / "gateway-run-1/result.json"
        result = read_json(gateway_file) if gateway_file.exists() else {}
        checks = result.get("checks", [])
        gateway_pass = (gateway["exit_code"] == 0 and not gateway["timed_out"]
                        and result.get("result") == "PASS" and result.get("passed") == 30
                        and result.get("failed") == 0 and len(checks) == 30
                        and all(check.get("passed") is True for check in checks))
        state["gateway"] = dict(gateway, status="PASS" if gateway_pass else "FAIL",
                                planned_checks=30, executed_checks=len(checks),
                                passed=result.get("passed", 0), failed=result.get("failed", 0),
                                provider_requests=result.get("provider_requests"), result_file=str(gateway_file))
        write_json(out / "status.json", state)
        migration_command = [sys.executable, str(out / "run_migration_e2e.py")]
        migration = run_logged(migration_command, out / "migration-runner.log", workspace, environment, 480)
        migration_file = out / "migration-e2e/result.json"
        migrated = read_json(migration_file) if migration_file.exists() else {}
        roles = migrated.get("roles", {})
        migration_pass = (migration["exit_code"] == 0 and not migration["timed_out"]
                          and migrated.get("result") == "PASS" and set(roles) == {"memory", "companion"}
                          and all(value.get("result") == "PASS" for value in roles.values()))
        state["migration"] = dict(migration, status="PASS" if migration_pass else "FAIL",
                                  planned_roles=2, executed_roles=len(roles),
                                  roles={name: value.get("result") for name, value in roles.items()},
                                  result_file=str(migration_file))
        source_identity(repo)
        require(digest(binary) == receipt["binary_sha256"], "validated CLI changed")
        require(gateway_pass and migration_pass, "combined E2E failed or its required counts were not met")
        state["status"] = "PASS"
    except Exception as error:
        state.update(status="FAIL", error=f"{type(error).__name__}: {error}", traceback=traceback.format_exc())
    finally:
        state["finished_at"] = now()
        write_json(out / "status.json", state)
    try:
        archive = archive_evidence(out, legacy)
    except Exception as error:
        state.update(status="FAIL", archive_error=f"{type(error).__name__}: {error}")
        write_json(out / "status.json", state)
        print(json.dumps(state, indent=2), flush=True)
        return 1
    print(json.dumps({"status": state["status"], "build": state["build"].get("status"),
                      "source_head": HEAD, "source_tree": TREE, "local_equivalent_head": LOCAL_HEAD,
                      "binary_sha256": state["build"].get("binary_sha256"),
                      "gateway": {key: state["gateway"].get(key) for key in
                                  ("status", "executed_checks", "passed", "failed", "provider_requests")},
                      "migration": {key: state["migration"].get(key) for key in
                                    ("status", "executed_roles", "roles")},
                      "receipt_paths": ["status.json", "source-binding.json", "build-receipt.json",
                                        "gateway-run-1/result.json", "migration-e2e/result.json"],
                      "archive": archive}, indent=2), flush=True)
    return 0 if state["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())
