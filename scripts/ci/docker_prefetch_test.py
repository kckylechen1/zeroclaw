#!/usr/bin/env python3
"""Exercise each real Docker prefetch stub layer against Cargo's target paths.

This checks the manifest COPY declarations, then runs only the mkdir/echo layer
in a temporary manifest-only workspace.
The source-image matrix remains responsible for compiling the actual images.
"""

import shutil
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
DOCKERFILES = ("Dockerfile", "Dockerfile.debian")


def missing_manifest_copies(dockerfile_text):
    """Check the three manifest COPY forms used by these source Dockerfiles."""
    section = dockerfile_text.split("# 1. Copy manifests to cache dependencies", 1)[1]
    section = section.split("# Create dummy targets", 1)[0]
    lines = {line.strip() for line in section.splitlines()}
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
    missing = []
    for member in workspace["members"]:
        path = Path(member) / "Cargo.toml"
        if member == ".":
            expected = "COPY Cargo.toml Cargo.lock ./"
        elif path.parts[0] == "crates" and len(path.parts) == 3:
            expected = "COPY --parents crates/*/Cargo.toml ./"
        else:
            expected = f"COPY {path.as_posix()} {path.as_posix()}"
        if expected not in lines:
            missing.append(path.as_posix())
    return sorted(missing)


def prefetch_layer(dockerfile):
    lines = (ROOT / dockerfile).read_text().splitlines()
    start = lines.index(
        "# Create dummy targets for all workspace members so manifest parsing succeeds."
    ) + 1
    command = []
    for line in lines[start:]:
        command.append(line.removeprefix("RUN ").removesuffix("\\"))
        if not line.endswith("\\"):
            break
    return " ".join(command)


def copy_manifests(destination):
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
    for member in workspace["members"]:
        path = Path(member) / "Cargo.toml"
        target = destination / path
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / path, target)
    # The default Dockerfile copies the real proc macro before stubbing.
    # Preserve that same source so its explicit lib target can be checked.
    shutil.copytree(
        ROOT / "crates/zeroclaw-macros/src",
        destination / "crates/zeroclaw-macros/src",
    )


def missing_targets(workspace):
    missing = []
    for manifest in workspace.rglob("Cargo.toml"):
        data = tomllib.loads(manifest.read_text())
        targets = []
        if "lib" in data:
            targets.append(data["lib"].get("path", "src/lib.rs"))
        for kind, directory in (
            ("bin", "src/bin"),
            ("test", "tests"),
            ("bench", "benches"),
            ("example", "examples"),
        ):
            for target in data.get(kind, []):
                targets.append(
                    target.get("path", f"{directory}/{target['name']}.rs")
                )
        for path in targets:
            target = manifest.parent / path
            if not target.is_file():
                missing.append(str(target.relative_to(workspace)))
    return sorted(missing)


class DockerPrefetchTargets(unittest.TestCase):
    def test_prefetch_copies_every_workspace_manifest(self):
        for dockerfile in DOCKERFILES:
            with self.subTest(dockerfile=dockerfile):
                self.assertEqual(
                    missing_manifest_copies((ROOT / dockerfile).read_text()), []
                )

    def test_missing_bridge_manifest_copy_is_detected(self):
        workspace = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]
        member = next(
            member for member in workspace["members"] if member.startswith("bridges/")
        )
        path = f"{member}/Cargo.toml"
        copy = f"COPY {path} {path}\n"
        for dockerfile in DOCKERFILES:
            with self.subTest(dockerfile=dockerfile):
                source = (ROOT / dockerfile).read_text()
                self.assertIn(copy, source)
                self.assertEqual(
                    missing_manifest_copies(source.replace(copy, "")), [path]
                )

    def test_real_prefetch_layers_supply_every_declared_target(self):
        for dockerfile in DOCKERFILES:
            with self.subTest(dockerfile=dockerfile), tempfile.TemporaryDirectory() as tmp:
                workspace = Path(tmp)
                copy_manifests(workspace)
                subprocess.run(
                    ["sh", "-eu", "-c", prefetch_layer(dockerfile)],
                    cwd=workspace,
                    check=True,
                )
                self.assertEqual(missing_targets(workspace), [])

    def test_a_new_feature_gated_binary_cannot_escape_the_contract(self):
        for dockerfile in DOCKERFILES:
            with self.subTest(dockerfile=dockerfile), tempfile.TemporaryDirectory() as tmp:
                workspace = Path(tmp)
                copy_manifests(workspace)
                with (workspace / "Cargo.toml").open("a") as manifest:
                    manifest.write(
                        '\n[[bin]]\nname = "fixture-new-bin"\n'
                        'path = "src/bin/fixture-new-bin.rs"\n'
                        'required-features = ["nodes"]\n'
                    )
                subprocess.run(
                    ["sh", "-eu", "-c", prefetch_layer(dockerfile)],
                    cwd=workspace,
                    check=True,
                )
                self.assertEqual(
                    missing_targets(workspace), ["src/bin/fixture-new-bin.rs"]
                )


if __name__ == "__main__":
    unittest.main(verbosity=2)
