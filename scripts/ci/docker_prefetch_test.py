#!/usr/bin/env python3
"""Exercise each real Docker prefetch stub layer against Cargo's target paths.

This runs only the mkdir/echo layer in a temporary manifest-only workspace.
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
