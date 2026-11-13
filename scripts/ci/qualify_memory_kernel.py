#!/usr/bin/env python3
"""Qualify one immutable Tachi pin in a disposable committed-source worktree.

This never updates the caller's manifest/lock, switches a service, or opens a
user store. Only Cargo generates the candidate lock. A resolved lock is not an
accepted upgrade: the receipt remains failed until every listed command passes.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib

MANIFEST = Path("crates/zeroclaw-memory/Cargo.toml")
KERNEL_REPOSITORY = "https://github.com/kckylechen1/Tachi.git"
PIN_RE = re.compile(r"^[0-9a-f]{40}$")


def replace_pin(text: str, revision: str) -> tuple[str, str]:
    if not PIN_RE.fullmatch(revision):
        raise ValueError("kernel revision must be a complete lowercase commit SHA")
    dependency = tomllib.loads(text)["dependencies"]["portable_kernel"]
    if dependency.get("git") != KERNEL_REPOSITORY or dependency.get("package") != "portable-kernel":
        raise ValueError("unexpected portable-kernel source; refusing to rewrite it")
    previous = dependency.get("rev", "")
    if not PIN_RE.fullmatch(previous):
        raise ValueError("existing kernel dependency is not commit-pinned")
    pattern = re.compile(r'(?m)^(portable_kernel\s*=\s*\{[^\n]*\brev\s*=\s*")' + previous + r'("[^\n]*\})$')
    updated, count = pattern.subn(lambda match: match[1] + revision + match[2], text)
    if count != 1:
        raise ValueError("expected exactly one unchanged inline kernel declaration")
    original = tomllib.loads(text)
    expected = tomllib.loads(text)
    expected["dependencies"]["portable_kernel"]["rev"] = revision
    if tomllib.loads(updated) != expected or original["dependencies"]["portable_kernel"]["rev"] != previous:
        raise ValueError("kernel pin rewrite changed an unrelated manifest field")
    return updated, previous


def check_graph(metadata: dict, revision: str) -> dict:
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = metadata.get("resolve", {}).get("nodes", [])
    node_features = {node["id"]: set(node["features"]) for node in nodes}
    sources = [package for package in packages.values() if package.get("source", "") and "github.com/kckylechen1/Tachi" in package["source"]]
    names = {package["name"] for package in sources}
    if not {"portable-kernel", "memcore", "vault-kit"}.issubset(names):
        raise ValueError("candidate graph does not contain the expected shared-kernel packages")
    if any(package["source"].split("#")[-1] != revision for package in sources):
        raise ValueError("candidate graph contains mixed Tachi revisions")
    if any(package["name"] == "memcore" and "admin" in node_features.get(package["id"], set()) for package in sources):
        raise ValueError("Tachi admin feature entered the portable consumer graph")
    sqlite = [package for package in packages.values() if package.get("links") == "sqlite3"]
    if len(sqlite) != 1:
        raise ValueError("candidate graph must contain exactly one sqlite3 links provider")
    return {
        "kernel_packages": sorted(({"name": package["name"], "version": package["version"], "source": package["source"]} for package in sources), key=lambda package: package["name"]),
        "sqlite3": {"name": sqlite[0]["name"], "version": sqlite[0]["version"]},
    }


def command(args: list[str], cwd: Path, log: Path, receipt: dict, env: dict) -> None:
    with log.open("wb") as stream:
        result = subprocess.run(args, cwd=cwd, env=env, stdout=stream, stderr=subprocess.STDOUT, check=False)
    receipt["commands"].append({"argv": args, "exit_code": result.returncode, "log": log.name})
    if result.returncode:
        raise RuntimeError(f"qualification command failed (exit {result.returncode}); see {log.name}")


def qualify(repo: Path, revision: str, output: Path) -> int:
    # Resolve everything before creating the output. No caller-selected branch is
    # changed. Worktree removal is scoped to the one directory created below.
    source = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
    if not PIN_RE.fullmatch(source):
        raise ValueError("source is not a commit object")
    if output.exists() or output.is_symlink():
        raise ValueError("output directory must not exist")
    output.mkdir(parents=True, mode=0o700)
    receipt = {"source_sha": source, "kernel_sha": revision, "status": "INCOMPLETE", "commands": []}
    created = False
    work = Path(tempfile.mkdtemp(prefix="zeroclaw-kernel-qualification-")) / "consumer"
    env = os.environ.copy()
    # The target belongs to this qualification and is removed with its scratch
    # parent; this does not clean or reuse another worktree's build artifacts.
    env["CARGO_TARGET_DIR"] = str(work.parent / "target")
    env["CARGO_INCREMENTAL"] = "0"
    try:
        subprocess.run(["git", "-C", str(repo), "worktree", "add", "--detach", str(work), source], check=True)
        created = True
        manifest = work / MANIFEST
        replacement, previous = replace_pin(manifest.read_text(), revision)
        receipt["previous_kernel_sha"] = previous
        manifest.write_text(replacement)
        command(["cargo", "update", "-p", "portable-kernel"], work, output / "resolve.log", receipt, env)
        shutil.copyfile(work / "Cargo.lock", output / "candidate.Cargo.lock")
        shutil.copyfile(manifest, output / "candidate.Cargo.toml")
        receipt["lock_sha256"] = hashlib.sha256((work / "Cargo.lock").read_bytes()).hexdigest()
        metadata_file = output / "metadata.json"
        with metadata_file.open("wb") as stream, (output / "metadata.stderr.log").open("wb") as errors:
            metadata_result = subprocess.run(["cargo", "metadata", "--locked", "--format-version", "1", "--features", "memory-tachi"], cwd=work, env=env, stdout=stream, stderr=errors, check=False)
        receipt["commands"].append({"argv": ["cargo", "metadata", "--locked", "--format-version", "1", "--features", "memory-tachi"], "exit_code": metadata_result.returncode, "log": "metadata.stderr.log"})
        if metadata_result.returncode:
            raise RuntimeError("candidate metadata resolution failed")
        receipt["graph"] = check_graph(json.loads(metadata_file.read_text()), revision)
        commands = [
            ("portable-tests.log", ["cargo", "test", "--locked", "-p", "zeroclaw-memory", "--features", "tachi"]),
            ("consumer-check.log", ["cargo", "check", "--locked", "--workspace", "--all-targets", "--features", "memory-tachi"]),
        ]
        for filename, args in commands:
            command(args, work, output / filename, receipt, env)
        receipt["status"] = "PASS"
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.CalledProcessError) as error:
        receipt["status"] = "FAIL"
        receipt["error"] = str(error)
    finally:
        if created:
            cleanup = subprocess.run(["git", "-C", str(repo), "worktree", "remove", "--force", str(work)], check=False)
            receipt["cleanup_exit_code"] = cleanup.returncode
            if cleanup.returncode:
                receipt["status"] = "FAIL"
        if not work.exists():
            shutil.rmtree(work.parent)
        (output / "qualification.json").write_text(json.dumps(receipt, indent=2) + "\n")
    return 0 if receipt["status"] == "PASS" else 1


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel-rev", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not PIN_RE.fullmatch(args.kernel_rev):
        parser.error("--kernel-rev requires a full lowercase SHA")
    repo = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    return qualify(repo, args.kernel_rev, args.output.resolve())


if __name__ == "__main__":
    raise SystemExit(main())
