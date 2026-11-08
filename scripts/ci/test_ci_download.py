import io
import os
from pathlib import Path
import random
import subprocess
import tarfile
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


DOWNLOADER = Path(__file__).with_name("download_tool.sh").resolve()
CONTENTS = random.Random(0).randbytes(32768)
archive = io.BytesIO()
with tarfile.open(fileobj=archive, mode="w:gz") as fixture_tar:
    member = tarfile.TarInfo("fixture-tool")
    member.size = len(CONTENTS)
    member.mode = 0o755
    fixture_tar.addfile(member, io.BytesIO(CONTENTS))
ARCHIVE = archive.getvalue()


class DownloadTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.output = self.root / "tool archive.tar.gz"
        self.installed = self.root / "installed"
        self.installed.mkdir()
        self.mode = "transient"
        self.requests = 0
        self.extracted_before_success = False
        self.release_stalled = threading.Event()
        fixture = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                fixture.requests += 1
                attempt = fixture.requests
                if (fixture.installed / "fixture-tool").exists():
                    fixture.extracted_before_success = True
                if fixture.mode == "permanent" or (fixture.mode == "transient" and attempt == 1):
                    self.send_response(504)
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Length", str(len(ARCHIVE)))
                self.end_headers()
                if fixture.mode == "stall" or (fixture.mode == "truncated" and attempt == 1):
                    self.wfile.write(ARCHIVE[:len(ARCHIVE) // 2])
                    self.wfile.flush()
                    if fixture.mode == "stall":
                        fixture.release_stalled.wait(15)
                    self.close_connection = True
                    return
                self.wfile.write(ARCHIVE)

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop_server)
        self.url = f"http://127.0.0.1:{self.server.server_port}/fixture.tar.gz"

    def stop_server(self):
        self.release_stalled.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def install(self, max_time="60"):
        env = dict(os.environ, CI_DOWNLOAD_MAX_TIME_SECONDS=max_time, NO_PROXY="127.0.0.1", no_proxy="127.0.0.1")
        # Exercise the real curl download followed by real tar, as in the jobs.
        # Failure must stop before tar can expose even a partial executable.
        return subprocess.run(
            ["bash", "-c", 'bash "$1" "$2" "$3" && tar -xzf "$3" -C "$4"',
             "fixture", str(DOWNLOADER), self.url, str(self.output), str(self.installed)],
            env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=15,
        )

    def assert_clean(self):
        self.assertEqual(list(self.root.glob("*.partial.*")), [])
        self.assertFalse(self.extracted_before_success)

    def assert_installed(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.output.read_bytes(), ARCHIVE)
        self.assertEqual((self.installed / "fixture-tool").read_bytes(), CONTENTS)
        self.assert_clean()

    def test_504_then_success_installs_complete_archive(self):
        self.assert_installed(self.install())
        self.assertEqual(self.requests, 2)

    def test_permanent_504_fails_and_preserves_previous_destination(self):
        self.mode = "permanent"
        self.output.write_bytes(b"previous artifact")
        result = self.install()
        self.assertEqual(result.returncode, 22, result.stderr)
        self.assertEqual(self.requests, 3)
        self.assertEqual(self.output.read_bytes(), b"previous artifact")
        self.assertEqual(list(self.installed.iterdir()), [])
        self.assert_clean()

    def test_truncated_transfer_retries_without_concatenating_or_extracting_partial_archive(self):
        self.mode = "truncated"
        self.assert_installed(self.install())
        self.assertEqual(self.requests, 2)

    def test_stalled_body_times_out_fails_and_removes_partial_files(self):
        self.mode = "stall"
        result = self.install(max_time="1")
        self.assertEqual(result.returncode, 28, result.stderr)
        self.assertEqual(self.requests, 3)
        self.assertFalse(self.output.exists())
        self.assertEqual(list(self.installed.iterdir()), [])
        self.assert_clean()

    def test_time_limit_override_cannot_disable_or_widen_the_bound(self):
        for value in ("0", "-1", "61", "invalid"):
            with self.subTest(value=value):
                result = self.install(max_time=value)
                self.assertEqual(result.returncode, 2, result.stderr)
        self.assertEqual(self.requests, 0)
        self.assertFalse(self.output.exists())
        self.assertEqual(list(self.installed.iterdir()), [])
        self.assert_clean()


if __name__ == "__main__":
    unittest.main()
