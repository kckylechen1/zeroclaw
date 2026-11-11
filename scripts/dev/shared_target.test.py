#!/usr/bin/env python3
"""Real worktrees/processes, fake Cargo: exercise target ownership without builds."""

import errno
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest


BASH = sys.argv[1] if len(sys.argv) > 1 else shutil.which("bash")
SOURCE = Path(__file__).resolve().parent
FAKE_CARGO = r'''#!/usr/bin/env python3
import json, os, signal, sys, time
from pathlib import Path

def stop(signum, _frame):
    if os.environ.get("TEST_STOPPED"):
        Path(os.environ["TEST_STOPPED"]).touch()
    sys.exit(128 + signum)

signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
if os.environ.get("TEST_TTY"):
    terminal = os.open("/dev/tty", os.O_RDWR)
    os.close(terminal)
    print("fixture: controlling terminal opened", flush=True)
metadata = sys.argv[1:2] == ["metadata"]
if os.environ.get("TEST_READY"):
    Path(os.environ["TEST_READY"]).touch()
    while not Path(os.environ["TEST_RELEASE"]).exists():
        time.sleep(0.01)
if metadata:
    if "--no-deps" not in sys.argv and os.environ.get("TEST_METADATA_FAIL"):
        sys.exit(9)
    if "--no-deps" not in sys.argv and os.environ.get("TEST_METADATA_INVALID"):
        print("{}"); sys.exit(0)
    own = {"id":"own", "name":"zeroclaw-fixture", "targets":[
        {"name":"zeroclaw_fixture"}, {"name":"component"}]}
    packages = [own]
    if "--no-deps" not in sys.argv:
        packages += [{"id":"dep", "name":"zeroclaw-vendor", "targets":[{"name":"zeroclaw_vendor"}]},
                     {"id":"collision", "name":"component", "targets":[{"name":"component"}]}]
    print(json.dumps({"packages":packages,"workspace_members":["own"]}))
else:
    Path(os.environ["TEST_OUTPUT"]).write_text(json.dumps({
        "target":os.environ["CARGO_TARGET_DIR"], "args":sys.argv[1:], "cwd":os.getcwd()}))
    sys.exit(int(os.environ.get("TEST_CARGO_EXIT", "0")))
'''


class SharedTargetTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="shared-target-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.main = self.root / "main checkout"
        self.linked = self.root / "linked checkout"
        dev = self.main / "scripts/dev"
        dev.mkdir(parents=True)
        for name in ("cargo-local.sh", "shared_target.py", "target_sweep.sh"):
            shutil.copy2(SOURCE / name, dev / name)
        ci = self.main / "scripts/ci"
        ci.mkdir()
        (ci / "toolchain_gate.sh").write_text("exit 0\n")
        (self.main / "Cargo.toml").write_text("[workspace]\nmembers=[]\n")
        for args in (["init", "-q", str(self.main)], ["-C", str(self.main), "add", "."],
                     ["-C", str(self.main), "-c", "user.name=Fixture", "-c",
                      "user.email=fixture@example.invalid", "commit", "-qm", "fixture"],
                     ["-C", str(self.main), "worktree", "add", "-q", "--detach", str(self.linked)]):
            subprocess.run(["git", *args], check=True)
        bindir = self.root / "bin"
        bindir.mkdir()
        for name, body in (("cargo", FAKE_CARGO), ("pgrep", '#!/bin/sh\nexit "${TEST_PGREP_EXIT:-1}"\n')):
            path = bindir / name
            path.write_text(body)
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{bindir}{os.pathsep}{os.environ['PATH']}",
                        HOME=str(self.root / "isolated-home"), TEST_OUTPUT=str(self.root / "output"))
        for key in ("CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR", "TEST_READY", "TEST_RELEASE",
                    "TEST_STOPPED", "TEST_CARGO_EXIT", "TEST_METADATA_FAIL", "TEST_METADATA_INVALID",
                    "TEST_PGREP_EXIT", "TEST_TTY"):
            self.env.pop(key, None)
        self.target = self.main / "target"
        self.target.mkdir()
        self.processes = []
        self.addCleanup(self.stop_processes)

    def stop_processes(self):
        for process in self.processes:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=5)

    def command(self, entry, *args, checkout=None):
        return [BASH, str((checkout or self.linked) / "scripts/dev" / entry), *args]

    def run_entry(self, entry, *args, checkout=None, **env):
        return subprocess.run(self.command(entry, *args, checkout=checkout), env=dict(self.env, **env),
                              cwd=self.root, text=True, capture_output=True, timeout=10)

    def start(self, entry, *args, **env):
        process = subprocess.Popen(self.command(entry, *args), env=dict(self.env, **env),
                                   cwd=self.root, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.processes.append(process)
        return process

    def wait_file(self, path):
        deadline = time.monotonic() + 5
        while not path.exists():
            self.assertLess(time.monotonic(), deadline, f"not reached: {path}")
            time.sleep(0.01)

    def artifacts(self, target=None):
        target = target or self.target
        own = target / "debug/deps/libzeroclaw_fixture-aaaaaaaaaaaaaaaa.rlib"
        own.parent.mkdir(parents=True, exist_ok=True)
        own.write_text("workspace")
        retained = []
        for relative in ("debug/deps/libzeroclaw_vendor-bbbbbbbbbbbbbbbb.rlib",
                         "debug/deps/component-cccccccccccccccc",
                         "debug/.fingerprint/zeroclaw-vendor-dddddddddddddddd/info",
                         "debug/build/zeroclaw-vendor-eeeeeeeeeeeeeeee/output"):
            path = target / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("dependency")
            retained.append(path)
        return own, retained

    def test_shared_defaults_dry_run_and_third_party_retention(self):
        own, retained = self.artifacts()
        for checkout in (self.main, self.linked):
            result = self.run_entry("target_sweep.sh", "--dry-run", checkout=checkout)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(str(self.target), result.stdout)
            self.assertIn(own.name, result.stdout)
            self.assertTrue(own.exists())
            self.assertTrue(all(path.exists() for path in retained))
        result = self.run_entry("target_sweep.sh")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(own.exists())
        self.assertTrue(all(path.read_text() == "dependency" for path in retained))
        self.assertTrue((self.target / ".zeroclaw-maintenance.lock").exists())

    def test_relative_cli_and_symlink_paths_share_the_same_lock(self):
        alias = self.linked / "target alias"
        alias.symlink_to(self.target, target_is_directory=True)
        for args, env in ((["check"], {"CARGO_TARGET_DIR":"target alias"}),
                          (["check", "--target-dir", "target alias"], {"CARGO_TARGET_DIR":"unused"}),
                          (["check", "--target-dir=target alias"], {}),
                          (["test", "--", "--target-dir", "ignored"], {})):
            result = self.run_entry("cargo-local.sh", *args, **env)
            self.assertEqual(result.returncode, 0, result.stderr)
            output = json.loads((self.root / "output").read_text())
            self.assertEqual(output["target"], str(self.target))
            self.assertEqual(output["args"], args)
        ready, release = self.root / "ready", self.root / "release"
        build = self.start("cargo-local.sh", "check", "--target-dir=target alias",
                           TEST_READY=str(ready), TEST_RELEASE=str(release))
        self.wait_file(ready)
        independent = self.run_entry("cargo-local.sh", "check",
                                     CARGO_TARGET_DIR=str(self.root / "independent-target"))
        self.assertEqual(independent.returncode, 0, independent.stderr)
        own, _ = self.artifacts()
        result = self.run_entry("target_sweep.sh", CARGO_TARGET_DIR="target alias")
        self.assertEqual(result.returncode, 75, result.stderr)
        self.assertTrue(own.exists())
        release.touch()
        self.assertEqual(build.wait(timeout=5), 0)

    def test_sweep_holds_lock_until_metadata_and_deletion_finish(self):
        own, _ = self.artifacts()
        ready, release = self.root / "ready", self.root / "release"
        sweep = self.start("target_sweep.sh", TEST_READY=str(ready), TEST_RELEASE=str(release))
        self.wait_file(ready)
        build_ready = self.root / "build-ready"
        build = self.start("cargo-local.sh", "check", TEST_READY=str(build_ready), TEST_RELEASE=str(release))
        time.sleep(0.15)
        self.assertIsNone(build.poll())
        self.assertFalse(build_ready.exists())
        self.assertTrue(own.exists())
        release.touch()
        self.assertEqual(sweep.wait(timeout=5), 0)
        self.assertEqual(build.wait(timeout=5), 0)
        self.assertFalse(own.exists())
        self.assertTrue(build_ready.exists())

    def test_failure_and_term_release_the_lock(self):
        result = self.run_entry("cargo-local.sh", "check", TEST_CARGO_EXIT="7")
        self.assertEqual(result.returncode, 7)
        self.assertEqual(self.run_entry("target_sweep.sh").returncode, 0)
        ready, release, stopped = (self.root / name for name in ("ready", "release", "stopped"))
        build = self.start("cargo-local.sh", "check", TEST_READY=str(ready),
                           TEST_RELEASE=str(release), TEST_STOPPED=str(stopped))
        self.wait_file(ready)
        build.send_signal(signal.SIGTERM)
        self.assertEqual(build.wait(timeout=5), 143)
        self.assertTrue(stopped.exists())
        self.assertEqual(self.run_entry("target_sweep.sh").returncode, 0)

    def test_managed_gate_can_close_fds_without_releasing_parent_lock(self):
        isolated = Path(self.env["HOME"])
        marker = isolated / ".local/state/host-build/managed.json"
        marker.parent.mkdir(parents=True)
        marker.write_text("{}")
        self.assertEqual(self.run_entry("cargo-local.sh", "check").returncode, 70)
        host = isolated / "bin/host-build"
        host.parent.mkdir()
        host.write_text('#!/usr/bin/env python3\nimport os, sys\n'
                        'assert sys.argv[1:3] == ["--", "cargo"]\n'
                        'os.closerange(3, 256)\nos.execvp("cargo", sys.argv[2:])\n')
        host.chmod(0o755)
        ready, release = self.root / "ready", self.root / "release"
        build = self.start("cargo-local.sh", "check", TEST_READY=str(ready), TEST_RELEASE=str(release))
        self.wait_file(ready)
        self.assertEqual(self.run_entry("target_sweep.sh").returncode, 75)
        release.touch()
        self.assertEqual(build.wait(timeout=5), 0)
        self.assertEqual(self.run_entry("target_sweep.sh").returncode, 0)

    def test_controlling_terminal_and_foreground_ctrl_c_are_preserved(self):
        ready, release, stopped = (self.root / name for name in ("ready", "release", "stopped"))
        pid, terminal = pty.fork()
        if pid == 0:
            env = dict(self.env, TEST_TTY="1", TEST_READY=str(ready),
                       TEST_RELEASE=str(release), TEST_STOPPED=str(stopped))
            os.execve(BASH, self.command("cargo-local.sh", "check"), env)
        os.set_blocking(terminal, False)
        output = bytearray()

        def drain():
            # macOS can wait for unread PTY output during exit after /dev/tty
            # was opened. Consume output while waiting, as a real terminal does.
            while select.select([terminal], [], [], 0)[0]:
                try:
                    chunk = os.read(terminal, 4096)
                except BlockingIOError:
                    return
                except OSError as error:
                    if error.errno == errno.EIO:  # Linux PTY EOF.
                        return
                    raise
                if not chunk:
                    return
                output.extend(chunk)

        try:
            self.wait_file(ready)  # Cargo has successfully opened /dev/tty.
            self.assertEqual(self.run_entry("target_sweep.sh").returncode, 75)
            os.write(terminal, b"\x03")
            deadline = time.monotonic() + 5
            while True:
                drain()
                exited, status = os.waitpid(pid, os.WNOHANG)
                if exited:
                    pid = None
                    self.assertEqual(os.waitstatus_to_exitcode(status), 130)
                    break
                self.assertLess(time.monotonic(), deadline,
                                f"Ctrl-C did not stop the command; stopped={stopped.exists()}; "
                                f"PTY output={bytes(output)!r}")
                time.sleep(0.01)
            self.assertIn(b"fixture: controlling terminal opened", output)
            self.assertTrue(stopped.exists())
            self.assertEqual(self.run_entry("target_sweep.sh").returncode, 0)
        finally:
            # Closing the master also releases a Darwin ttywait during failed
            # assertions; signal errors from an exiting child must not mask them.
            os.close(terminal)
            if pid is not None:
                try:
                    os.killpg(pid, signal.SIGTERM)
                except (ProcessLookupError, PermissionError):
                    pass
                cleanup_deadline = time.monotonic() + 5
                while not os.waitpid(pid, os.WNOHANG)[0]:
                    self.assertLess(time.monotonic(), cleanup_deadline,
                                    f"PTY child did not exit during cleanup; output={bytes(output)!r}")
                    time.sleep(0.01)

    def test_uncertain_output_overrides_and_missing_graph_refuse(self):
        for args in (["check", "--config", "build.target-dir='other'"],
                     ["check", "--config=alias.check='build --target-dir other'"],
                     ["check", "--config", '"build".target-dir="other"'],
                     ["check", "--config", '"bu\\u0069ld".target-dir="other"'],
                     ["check", "--config", "net.retry=3\n[build]\ntarget-dir='other'"],
                     ["check", "--config", "custom.toml"],
                     ["check", "--build-dir", "other"]):
            result = self.run_entry("cargo-local.sh", *args)
            self.assertEqual(result.returncode, 2, (args, result.stderr))
        self.assertFalse((self.root / "output").exists())
        self.assertEqual(self.run_entry("cargo-local.sh", "check", "--config", "net.offline=true",
                                        "--config=net.retry=3").returncode, 0)
        own, _ = self.artifacts()
        self.assertNotEqual(self.run_entry("target_sweep.sh", TEST_METADATA_FAIL="1").returncode, 0)
        self.assertTrue(own.exists())
        self.assertNotEqual(self.run_entry("target_sweep.sh", TEST_METADATA_INVALID="1").returncode, 0)
        self.assertTrue(own.exists())
        self.assertNotEqual(self.run_entry("target_sweep.sh", TEST_PGREP_EXIT="0").returncode, 0)
        self.assertTrue(own.exists())
        self.assertEqual(self.run_entry("cargo-local.sh", "check").returncode, 0)
        self.assertEqual(self.run_entry("target_sweep.sh", "--profile", "../outside").returncode, 2)
        outside = self.root / "outside"
        outside.mkdir()
        (self.target / "alias").symlink_to(outside, target_is_directory=True)
        self.assertEqual(self.run_entry("target_sweep.sh", "--profile", "alias").returncode, 2)


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
