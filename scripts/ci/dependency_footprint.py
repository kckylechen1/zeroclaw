#!/usr/bin/env python3

"""Measure target-specific normal/build dependency closures for root profiles."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path
from typing import Any


CONFIG_PATH = Path("dev/ci/dependency-footprint.json")
PACKAGE_LINE = re.compile(r"^(?P<name>\S+) v(?P<version>\S+)(?: \((?P<detail>.*)\))?$")


class FootprintError(RuntimeError):
    pass


def load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise FootprintError(f"cannot read {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise FootprintError(f"{path} must contain a JSON object")
    return value


def profile_command(profile: dict[str, Any], target: str) -> list[str]:
    command = [
        "cargo",
        "metadata",
        "--locked",
        "--offline",
        "--format-version",
        "1",
        "--filter-platform",
        target,
    ]
    if profile.get("no_default_features"):
        command.append("--no-default-features")
    features = profile.get("features", [])
    if features:
        command.extend(["--features", ",".join(features)])
    return command


def tree_command(profile: dict[str, Any], target: str, root_name: str) -> list[str]:
    command = [
        "cargo",
        "tree",
        "--locked",
        "--offline",
        "--target",
        target,
        "--package",
        root_name,
        "--edges",
        "normal,build",
        "--prefix",
        "none",
        "--format",
        "{p}|{f}",
    ]
    if profile.get("no_default_features"):
        command.append("--no-default-features")
    features = profile.get("features", [])
    if features:
        command.extend(["--features", ",".join(features)])
    return command


def run_metadata(profile: dict[str, Any], target: str, repo: Path) -> dict[str, Any]:
    command = profile_command(profile, target)
    completed = subprocess.run(
        command,
        cwd=repo,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise FootprintError(
            f"{' '.join(command)} failed with exit {completed.returncode}: {detail}"
        )
    try:
        value = json.loads(completed.stdout)
    except json.JSONDecodeError as exc:
        raise FootprintError(f"cargo metadata returned invalid JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise FootprintError("cargo metadata did not return a JSON object")
    return value


def run_tree(
    profile: dict[str, Any], target: str, root_name: str, repo: Path
) -> str:
    command = tree_command(profile, target, root_name)
    completed = subprocess.run(
        command,
        cwd=repo,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise FootprintError(
            f"{' '.join(command)} failed with exit {completed.returncode}: {detail}"
        )
    return completed.stdout


def normalized_source(package: dict[str, Any], repo: Path) -> str:
    manifest = Path(package["manifest_path"]).resolve()
    try:
        relative_manifest = manifest.relative_to(repo).as_posix()
        return f"workspace:{relative_manifest}"
    except ValueError:
        source = package.get("source")
        if not source:
            raise FootprintError(
                "path dependency outside the workspace has no stable source identity: "
                f"{package['name']} {package['version']}"
            )
        return source


def tree_closure(
    metadata: dict[str, Any], tree_output: str, root_name: str
) -> dict[str, Any]:
    resolve = metadata.get("resolve")
    if not isinstance(resolve, dict):
        raise FootprintError("cargo metadata omitted the resolve graph")
    packages = metadata.get("packages", [])
    packages_by_id = {package["id"]: package for package in packages}
    root_id = resolve.get("root")
    if root_id not in packages_by_id or packages_by_id[root_id].get("name") != root_name:
        raise FootprintError(
            f"resolve root is not the configured package {root_name!r}: {root_id!r}"
        )
    default_members = metadata.get("workspace_default_members", [])
    if default_members != [root_id]:
        raise FootprintError(
            "workspace default members must contain only the measured root package; "
            f"got {default_members!r}"
        )

    repo = Path(metadata["workspace_root"]).resolve()
    candidates: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    for package in packages:
        candidates[(package["name"], package["version"])].append(package)

    selected: dict[str, dict[str, Any]] = {}
    row_count = 0
    for raw_line in tree_output.splitlines():
        if not raw_line.strip():
            continue
        row_count += 1
        package_text, separator, feature_text = raw_line.partition("|")
        if not separator:
            raise FootprintError(f"cargo tree row omits feature separator: {raw_line!r}")
        package_text = package_text.removesuffix(" (proc-macro)")
        match = PACKAGE_LINE.fullmatch(package_text)
        if not match:
            raise FootprintError(f"cannot parse cargo tree package row: {raw_line!r}")
        key = (match.group("name"), match.group("version"))
        choices = candidates.get(key, [])
        detail = match.group("detail")
        if len(choices) > 1 and detail:
            detail_path = Path(detail)
            if detail_path.is_absolute():
                choices = [
                    package
                    for package in choices
                    if Path(package["manifest_path"]).parent.resolve()
                    == detail_path.resolve()
                ]
            else:
                choices = [
                    package
                    for package in choices
                    if detail in (package.get("source") or "")
                ]
        if len(choices) != 1:
            identities = [package["id"] for package in choices]
            raise FootprintError(
                f"cargo tree identity {package_text!r} maps to {len(choices)} "
                f"metadata packages: {identities}"
            )
        package = choices[0]
        features = feature_text.removesuffix(" (*)").strip()
        selected_row = selected.setdefault(
            package["id"],
            {
                "name": package["name"],
                "version": package["version"],
                "source": normalized_source(package, repo),
                "features": set(),
            },
        )
        selected_row["features"].update(
            feature for feature in features.split(",") if feature
        )

    if root_id not in selected:
        raise FootprintError("cargo tree output does not contain the resolve root")
    package_rows = []
    for row in selected.values():
        package_rows.append({**row, "features": sorted(row["features"])})
    package_rows.sort(key=lambda row: (row["name"], row["version"], row["source"]))
    closure_digest = hashlib.sha256(
        json.dumps(package_rows, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()
    return {
        "package_count_including_root": len(package_rows),
        "cargo_tree_row_count": row_count,
        "closure_sha256": closure_digest,
        "packages": package_rows,
    }


def package_index(closure: dict[str, Any]) -> dict[str, list[dict[str, Any]]]:
    result: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for package in closure["packages"]:
        result[package["name"]].append(package)
    return result


def check_expectations(
    profile_name: str, closure: dict[str, Any], expectation: dict[str, Any]
) -> list[str]:
    errors = []
    packages = package_index(closure)
    for name in expectation.get("present_packages", []):
        if name not in packages:
            errors.append(f"{profile_name}: required package {name!r} is absent")
    for name in expectation.get("absent_packages", []):
        if name in packages:
            errors.append(f"{profile_name}: forbidden package {name!r} is present")
    for field, should_exist in (
        ("package_features_present", True),
        ("package_features_absent", False),
    ):
        for name, features in expectation.get(field, {}).items():
            rows = packages.get(name, [])
            if len(rows) != 1:
                errors.append(
                    f"{profile_name}: expected one resolved {name!r} package, got {len(rows)}"
                )
                continue
            actual = set(rows[0]["features"])
            for feature in features:
                if (feature in actual) != should_exist:
                    state = "absent" if should_exist else "present"
                    errors.append(
                        f"{profile_name}: {name!r} feature {feature!r} is {state}"
                    )
    return errors


def package_differences(
    profiles: list[dict[str, Any]], comparisons: list[dict[str, str]]
) -> list[dict[str, Any]]:
    by_name = {profile["name"]: profile for profile in profiles}
    differences = []
    for comparison in comparisons:
        name = comparison["profile"]
        reference_name = comparison["reference"]
        if name not in by_name or reference_name not in by_name:
            continue

        def identities(profile_name: str) -> dict[tuple[str, str, str], dict[str, str]]:
            return {
                (package["name"], package["version"], package["source"]): {
                    "name": package["name"],
                    "version": package["version"],
                    "source": package["source"],
                }
                for package in by_name[profile_name]["packages"]
            }

        actual = identities(name)
        reference = identities(reference_name)
        added = [actual[key] for key in sorted(actual.keys() - reference.keys())]
        removed = [reference[key] for key in sorted(reference.keys() - actual.keys())]
        differences.append(
            {
                "profile": name,
                "reference": reference_name,
                "added_count": len(added),
                "removed_count": len(removed),
                "added": added,
                "removed": removed,
            }
        )
    return differences


def configured_profiles(config: dict[str, Any]) -> dict[str, dict[str, Any]]:
    profiles = config.get("profiles")
    if not isinstance(profiles, list) or not profiles:
        raise FootprintError("profile config must define at least one profile")
    configured: dict[str, dict[str, Any]] = {}
    for profile in profiles:
        if not isinstance(profile, dict) or not isinstance(profile.get("name"), str):
            raise FootprintError("every configured profile must have a string name")
        name = profile["name"]
        if not name or name in configured:
            raise FootprintError(f"profile name is empty or duplicated: {name!r}")
        configured[name] = profile
    return configured


def synthetic_metadata() -> dict[str, Any]:
    def package(name: str) -> dict[str, Any]:
        return {
            "id": name,
            "name": name,
            "version": "1.0.0",
            "source": None,
            "manifest_path": f"/repo/{name}/Cargo.toml",
        }

    return {
        "workspace_root": "/repo",
        "workspace_default_members": ["root"],
        "packages": [package(name) for name in ("root", "normal", "build", "dev", "leaf")],
        "resolve": {
            "root": "root",
            "nodes": [
                {
                    "id": "root",
                    "features": ["selected", "polluted-by-workspace-dev-edge"],
                    "deps": [],
                },
                {"id": "normal", "features": [], "deps": []},
                {"id": "build", "features": [], "deps": []},
                {"id": "dev", "features": [], "deps": []},
                {"id": "leaf", "features": [], "deps": []},
            ],
        },
    }


def command_output(command: list[str], repo: Path) -> str:
    completed = subprocess.run(
        command,
        cwd=repo,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise FootprintError(
            f"{' '.join(command)} failed with exit {completed.returncode}: {detail}"
        )
    return completed.stdout.strip()


def provenance(repo: Path) -> dict[str, Any]:
    tracked_status = command_output(
        ["git", "status", "--porcelain", "--untracked-files=no"], repo
    )
    tracked_files = command_output(["git", "ls-files"], repo).splitlines()
    dependency_inputs = [
        path
        for path in tracked_files
        if path == "Cargo.lock" or Path(path).name == "Cargo.toml"
    ]
    changed_inputs = command_output(
        ["git", "diff", "--name-only", "HEAD", "--", *dependency_inputs], repo
    )
    return {
        "source_git_head": command_output(["git", "rev-parse", "HEAD"], repo),
        "tracked_worktree_dirty": bool(tracked_status),
        "dependency_inputs_dirty": bool(changed_inputs),
        "cargo_version": command_output(["cargo", "--version"], repo),
        "rustc_version": command_output(["rustc", "--version"], repo),
    }


def self_test() -> None:
    tree_output = """\
root v1.0.0 (/repo/root)|selected
normal v1.0.0 (/repo/normal)|default,std
leaf v1.0.0 (/repo/leaf)|
build v1.0.0 (/repo/build)|build-feature
leaf v1.0.0 (/repo/leaf)| (*)
"""
    closure = tree_closure(synthetic_metadata(), tree_output, "root")
    names = {package["name"] for package in closure["packages"]}
    if names != {"root", "normal", "build", "leaf"}:
        raise FootprintError(f"self-test closure mismatch: {sorted(names)}")
    errors = check_expectations(
        "fixture",
        closure,
        {
            "present_packages": ["normal", "build", "leaf"],
            "absent_packages": ["dev"],
            "package_features_present": {"root": ["selected"]},
            "package_features_absent": {
                "root": ["not-selected", "polluted-by-workspace-dev-edge"]
            },
        },
    )
    if errors:
        raise FootprintError("self-test expectations failed: " + "; ".join(errors))
    mirror = {**closure, "name": "mirror"}
    fixture = {**closure, "name": "fixture"}
    differences = package_differences(
        [fixture, mirror], [{"profile": "mirror", "reference": "fixture"}]
    )
    if len(differences) != 1 or differences[0]["added_count"] != 0:
        raise FootprintError(f"self-test package comparison failed: {differences}")
    polluted = {
        **mirror,
        "packages": mirror["packages"]
        + [
            {
                "name": "dev-only",
                "version": "1.0.0",
                "source": "workspace:dev-only/Cargo.toml",
                "features": [],
            }
        ],
    }
    differences = package_differences(
        [fixture, polluted], [{"profile": "mirror", "reference": "fixture"}]
    )
    if differences[0]["added_count"] != 1 or differences[0]["removed_count"] != 0:
        raise FootprintError(f"self-test package delta mismatch: {differences}")
    try:
        configured_profiles({"profiles": []})
    except FootprintError as exc:
        if "at least one profile" not in str(exc):
            raise
    else:
        raise FootprintError("self-test accepted an empty profile config")
    outside = synthetic_metadata()
    outside_package = outside["packages"][1]
    outside_package["manifest_path"] = "/Users/example/private/normal/Cargo.toml"
    try:
        tree_closure(outside, tree_output, "root")
    except FootprintError as exc:
        if "no stable source identity" not in str(exc):
            raise
    else:
        raise FootprintError("self-test accepted an unstable external path identity")
    print("dependency footprint self-test: PASS")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, default=CONFIG_PATH)
    parser.add_argument("--target", help="required Cargo target triple")
    parser.add_argument("--output", type=Path, help="write normalized JSON report")
    parser.add_argument(
        "--profiles",
        help="comma-separated profile names; defaults to every configured profile",
    )
    parser.add_argument("--self-test", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.self_test:
        self_test()
        return 0
    if not args.target:
        raise FootprintError("--target is required so target-gated edges are explicit")
    if not args.output:
        raise FootprintError("--output is required")

    repo = Path.cwd().resolve()
    config = load_json(args.config)
    configured = configured_profiles(config)
    names = (
        args.profiles.split(",") if args.profiles else list(configured)
    )
    unknown = sorted(set(names) - set(configured))
    if unknown:
        raise FootprintError(f"unknown profiles: {', '.join(unknown)}")

    report_profiles = []
    failures = []
    for name in names:
        profile = configured[name]
        metadata = run_metadata(profile, args.target, repo)
        tree_output = run_tree(profile, args.target, config["root_package"], repo)
        closure = tree_closure(metadata, tree_output, config["root_package"])
        failures.extend(
            check_expectations(
                name, closure, config.get("expectations", {}).get(name, {})
            )
        )
        report_profiles.append(
            {
                "name": name,
                "description": profile["description"],
                "cargo_metadata_args": profile_command(profile, args.target)[2:],
                "cargo_tree_args": tree_command(
                    profile, args.target, config["root_package"]
                )[2:],
                **closure,
            }
        )

    report = {
        "schema_version": 1,
        "target": args.target,
        "edge_kinds": ["normal", "build"],
        "root_package": config["root_package"],
        "cargo_lock_sha256": hashlib.sha256(
            (repo / "Cargo.lock").read_bytes()
        ).hexdigest(),
        "profile_config_sha256": hashlib.sha256(
            args.config.read_bytes()
        ).hexdigest(),
        "provenance": provenance(repo),
        "package_differences": package_differences(
            report_profiles, config.get("comparisons", [])
        ),
        "profiles": report_profiles,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    if failures:
        for failure in failures:
            print(f"dependency footprint: FAIL: {failure}", file=sys.stderr)
        return 1
    for profile in report_profiles:
        print(
            f"{profile['name']}: {profile['package_count_including_root']} packages, "
            f"{profile['cargo_tree_row_count']} cargo tree rows, "
            f"sha256={profile['closure_sha256']}"
        )
    print(f"dependency footprint: PASS ({args.output})")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except FootprintError as exc:
        print(f"dependency footprint: ERROR: {exc}", file=sys.stderr)
        raise SystemExit(2)
