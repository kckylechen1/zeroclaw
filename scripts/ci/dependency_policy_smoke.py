#!/usr/bin/env python3
"""Exercise dependency-policy boundaries with cargo-deny, without compiling crates.

Run after the real dependency checks have populated Cargo's cache. Fixtures are
local path dependencies with generated lockfiles; their names and versions test
the reviewed exceptions, not substitute implementations of those libraries.
The ordinary CI deny/audit commands still check real sources and advisories.
"""

import argparse
from collections import Counter
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile


REVIEWED_LICENSES = (
    ("memcore", "2.0.0", "AGPL-3.0-only"),
    ("portable-kernel", "1.9.0", "AGPL-3.0-only"),
    ("vault-kit", "0.1.0", "AGPL-3.0-only"),
    ("borrow-or-share", "0.2.4", "MIT-0"),
)
AGPL_PACKAGES = {name for name, _, license_id in REVIEWED_LICENSES
                 if license_id == "AGPL-3.0-only"}
OPTIONAL_RNG_VERSIONS = (("rand", "0.8.6"), ("rand_chacha", "0.3.1"))


def run(cargo, args, repo):
    command = [cargo, *args]
    result = subprocess.run(
        command, cwd=repo, text=True, capture_output=True,
        env={**os.environ, "CARGO_TERM_COLOR": "never", "CARGO_NET_OFFLINE": "true"},
    )
    return command, result


def failure_context(message, command, result):
    return (
        f"{message}\n$ {shlex.join(command)}\nexit: {result.returncode}\n"
        f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
    )


def fail(message, command, result):
    raise RuntimeError(failure_context(message, command, result))


class DefaultGraphViolation(RuntimeError):
    def __init__(self, message, command, result, agpl, rng):
        super().__init__(failure_context(message, command, result))
        self.agpl = set(agpl)
        self.rng = set(rng)


def check_default_graph(cargo, repo, manifest=None):
    manifest = manifest or repo / "Cargo.toml"
    lock = manifest.with_name("Cargo.lock")
    before = lock.read_bytes()
    command, result = run(cargo, [
        "tree", "--locked", "--offline", "--manifest-path", str(manifest),
        "-p", "zeroclawlabs", "--target", "all", "--edges", "normal,build",
        "--prefix", "none",
        "--format", "{p}",
    ], repo)
    if result.returncode != 0:
        fail("Could not inspect the default production dependency graph", command, result)
    if lock.read_bytes() != before:
        fail("Default graph inspection changed Cargo.lock", command, result)
    packages = set()
    for line in result.stdout.splitlines():
        if not line.strip():
            continue
        fields = line.split()
        if len(fields) < 2 or not fields[1].startswith("v") or len(fields[1]) == 1:
            fail(f"Unrecognized default dependency entry: {line!r}", command, result)
        packages.add((fields[0], fields[1][1:]))
    names = {name for name, _ in packages}
    if "zeroclawlabs" not in names:
        fail("Default graph did not contain the selected root package", command, result)
    violations = []
    forbidden_agpl = sorted((name, version) for name, version in packages
                            if name in AGPL_PACKAGES)
    forbidden_rng = sorted(packages.intersection(OPTIONAL_RNG_VERSIONS))
    if forbidden_agpl:
        violations.append(f"Optional AGPL packages entered the default graph: {forbidden_agpl}")
    if forbidden_rng:
        violations.append(f"Optional Tachi RNG versions entered the default graph: {forbidden_rng}")
    if violations:
        raise DefaultGraphViolation("; ".join(violations), command, result,
                                    forbidden_agpl, forbidden_rng)
    print("PASS default production graph excludes optional AGPL/RNG packages on all targets")


def write_package(path, name, version, license_id, dependencies="", workspace=False,
                  table="dependencies"):
    path.mkdir(parents=True)
    (path / "Cargo.toml").write_text(
        "[package]\n"
        f"name = {json.dumps(name)}\nversion = {json.dumps(version)}\n"
        f"license = {json.dumps(license_id)}\nedition = \"2021\"\n"
        "[lib]\npath = \"lib.rs\"\n"
        + ("[workspace]\nresolver = \"2\"\n" if workspace else "")
        + (f"[{table}]\n" + dependencies if dependencies else ""),
        encoding="utf-8",
    )
    (path / "lib.rs").write_text("// Metadata-only dependency-policy fixture.\n",
                                 encoding="utf-8")


def write_fixture(directory, name, packages, wildcard=False,
                  root_name="zeroclaw-policy-fixture", table="dependencies"):
    root = directory / name / "root"
    dependencies = []
    for index, (package, version, license_id) in enumerate(packages):
        dependency = root.parent / "deps" / str(index)
        write_package(dependency, package, version, license_id)
        requirement = "" if wildcard else f", version = {json.dumps('=' + version)}"
        dependencies.append(
            f"dep_{index} = {{ package = {json.dumps(package)}, "
            f"path = {json.dumps(str(dependency))}{requirement} }}\n"
        )
    write_package(root, root_name, "0.0.0", "MIT",
                  "".join(dependencies), workspace=True, table=table)
    return root


def check_default_rejection(cargo, repo, directory, name, packages, *,
                            expected_agpl=(), expected_rng=(), windows_only=False):
    root = write_fixture(directory, name, packages, root_name="zeroclawlabs",
                         table="target.'cfg(windows)'.dependencies" if windows_only
                         else "dependencies")
    manifest = root / "Cargo.toml"
    command, result = run(cargo, [
        "generate-lockfile", "--offline", "--manifest-path", str(manifest),
    ], repo)
    if result.returncode != 0:
        fail(f"{name}: fixture resolution failed", command, result)
    try:
        check_default_graph(cargo, repo, manifest)
    except DefaultGraphViolation as error:
        if error.agpl != set(expected_agpl) or error.rng != set(expected_rng):
            raise RuntimeError(f"{name}: unexpected default-graph refusal\n{error}") from error
    else:
        raise RuntimeError(f"{name}: default graph accepted optional-only dependencies")
    print(f"PASS {name}")


def check_case(cargo, config, repo, directory, name, packages, check,
               expected_errors=None, wildcard=False):
    expected = Counter(expected_errors or {})
    root = write_fixture(directory, name, packages, wildcard=wildcard)
    manifest = str(root / "Cargo.toml")
    command, result = run(cargo, [
        "generate-lockfile", "--offline", "--manifest-path", manifest,
    ], repo)
    if result.returncode != 0:
        fail(f"{name}: fixture resolution failed", command, result)
    lock = root / "Cargo.lock"
    before = lock.read_bytes()
    # The short -c belongs to cargo-deny. The local Cargo launcher reserves
    # --config for Cargo's one-line configuration overrides.
    command, result = run(cargo, [
        "deny", "--offline", "--locked", "--format", "json",
        "--manifest-path", manifest, "check", "-c", str(config),
        "--disable-fetch", check,
    ], repo)
    records = []
    for line in result.stderr.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue  # The host Cargo wrapper may print its toolchain receipt.
        if isinstance(record, dict):
            records.append(record)
    summaries = [record["fields"] for record in records
                 if record.get("type") == "summary" and "fields" in record]
    diagnostics = [record.get("fields", {}) for record in records
                   if record.get("type") == "diagnostic"]
    errors = Counter(item.get("code") for item in diagnostics
                     if item.get("severity") in ("error", "bug"))
    tool_errors = [record for record in records if record.get("type") == "log"
                   and record.get("fields", {}).get("level") == "ERROR"]
    # cargo-deny 0.19.9 uses distinct check bits: bans=2 and licenses=4.
    # A nonzero exit alone is not evidence that the intended policy refused it.
    expected_status = {"bans": 2, "licenses": 4}[check] if expected else 0
    valid_summary = (len(summaries) == 1 and set(summaries[0]) == {check}
                     and summaries[0][check].get("errors") == sum(expected.values()))
    if (result.returncode != expected_status or not valid_summary
            or errors != expected or tool_errors):
        fail(f"{name}: expected {dict(expected)} policy errors; observed {dict(errors)}",
             command, result)
    if lock.read_bytes() != before:
        fail(f"{name}: cargo-deny changed the fixture lockfile", command, result)
    print(f"PASS {name}")


def next_patch(version):
    major, minor, patch = version.split(".")
    return f"{major}.{minor}.{int(patch) + 1}"


def main():
    repo = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cargo", default="cargo",
                        help="Cargo executable, or scripts/dev/cargo-local.sh on managed hosts")
    parser.add_argument("--config", type=Path, default=repo / "deny.toml")
    args = parser.parse_args()
    config = args.config.resolve(strict=True)
    cargo = str(Path(args.cargo).resolve()) if os.sep in args.cargo else args.cargo
    check_default_graph(cargo, repo)
    legacy_rng = tuple((name, version, "MIT") for name, version in OPTIONAL_RNG_VERSIONS)
    duplicates = legacy_rng + (
        ("rand", "0.9.4", "MIT"), ("rand", "0.10.3", "MIT"),
        ("rand_chacha", "0.9.0", "MIT"),
    )
    with tempfile.TemporaryDirectory(prefix="zeroclaw-dependency-policy-") as temporary:
        directory = Path(temporary)
        memcore = next(package for package in REVIEWED_LICENSES if package[0] == "memcore")
        check_default_rejection(cargo, repo, directory, "windows-only-agpl-is-denied",
                                (memcore,), expected_agpl=(memcore[:2],), windows_only=True)
        check_default_rejection(cargo, repo, directory, "default-legacy-rng-is-denied",
                                legacy_rng, expected_rng=OPTIONAL_RNG_VERSIONS)
        cases = (
            ("reviewed-licenses", REVIEWED_LICENSES, "licenses", {}),
            ("unreviewed-license-packages", (
                ("zeroclaw-unreviewed-agpl", "1.0.0", "AGPL-3.0-only"),
                ("zeroclaw-unreviewed-mit0", "1.0.0", "MIT-0"),
            ), "licenses", {"rejected": 2}),
            ("license-exceptions-do-not-cover-new-versions", tuple(
                (name, next_patch(version), license_id)
                for name, version, license_id in REVIEWED_LICENSES
            ), "licenses", {"rejected": 4}),
            ("license-exceptions-do-not-cover-other-licenses", tuple(
                (name, version, "GPL-3.0-only") for name, version, _ in REVIEWED_LICENSES
            ), "licenses", {"rejected": 4}),
            ("reviewed-duplicate-versions", duplicates, "bans", {}),
            ("duplicate-skips-do-not-cover-new-versions", tuple(
                (name, next_patch(version) if (name, version) in OPTIONAL_RNG_VERSIONS else version,
                 license_id) for name, version, license_id in duplicates
            ), "bans", {"duplicate": 2}),
        )
        for name, packages, check, expected in cases:
            check_case(cargo, config, repo, directory, name, packages, check, expected)
        check_case(cargo, config, repo, directory, "unversioned-path-is-denied",
                   (("zeroclaw-unversioned-path", "1.0.0", "MIT"),),
                   "bans", {"wildcard": 1}, wildcard=True)
    print("Dependency policy: real default graph, 2 default graph rejection probes, "
          "and 7 cargo-deny boundary cases passed")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, ValueError) as error:
        print(f"dependency policy smoke failed: {error}", file=sys.stderr)
        sys.exit(1)
