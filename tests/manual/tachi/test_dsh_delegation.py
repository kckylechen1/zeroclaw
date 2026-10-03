#!/usr/bin/env python3
"""Operator-only production CLI -> Tachi -> real DSH proof; no Cargo test."""
import argparse
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import urllib.parse
import uuid

TOOLS = ["tachi_start", "tachi_watch", "tachi_status", "tachi_result", "tachi_cancel"]
TASK = "Compute 137 * 29. Reply with only the integer, without tools or commentary."


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


class Probe:
    def __init__(self, args, evidence):
        self.args, self.evidence = args, evidence
        self.request_id = "dsh-manual-" + uuid.uuid4().hex
        self.phase, self.calls, self.receipts, self.errors = 0, [], [], []
        self.run_dir, self.dispatch_id, self.terminal = None, None, None
        self.start = {"request_id": self.request_id, "harness": "dsh", "task": TASK,
                      "staffing_reason": "explicit_user_request"}

    def reply(self, request):
        visible = {item["function"]["name"] for item in request.get("tools", [])}
        require(set(TOOLS) <= visible, "five Tachi tools are not model-visible")
        if self.calls:
            messages = [m for m in request["messages"] if m["role"] == "tool"]
            require(messages, "production Agent returned no tool result")
            latest = messages[-1]
            require(latest["tool_call_id"] == self.calls[-1]["id"], "tool result id mismatch")
            value = json.loads(latest["content"])
            require(value.get("request_id") == self.request_id, "request id mismatch")
            self.receipts.append({"tool": self.calls[-1]["function"]["name"], "value": value})
        else:
            value = None
        if self.phase == 0:
            name, arguments, self.phase = "tachi_start", self.start, 1
        elif self.phase == 1:
            require(value.get("accepted") is True and value.get("replayed") is False,
                    "first start was not a fresh accepted dispatch")
            receipt = value["receipt"]
            self.dispatch_id = receipt["dispatch_id"]
            self.run_dir = Path(receipt["run_dir"]).resolve(strict=True)
            require(self.run_dir.is_relative_to(self.args.tachi_home), "run receipt escaped Tachi home")
            name, arguments, self.phase = "tachi_start", self.start, 2
        elif self.phase == 2:
            require(value.get("replayed") is True and value.get("accepted") is True
                    and value.get("dispatch_id") == self.dispatch_id, "start replay made another dispatch")
            name, arguments, self.phase = "tachi_watch", self.read_args(), 3
            arguments["max_wait_secs"] = 3
        elif self.phase == 3:
            require(value["status"]["dispatch_id"] == self.dispatch_id, "watch dispatch mismatch")
            if value.get("terminal") is True:
                self.terminal = value["status"]
                name, arguments, self.phase = "tachi_status", self.read_args(), 4
            else:
                name, arguments = "tachi_watch", self.read_args()
                arguments["max_wait_secs"] = 3
        elif self.phase == 4:
            require(value["status"]["dispatch_id"] == self.dispatch_id
                    and value["status"]["state"] == self.terminal["state"], "status is not terminal receipt")
            name, arguments, self.phase = "tachi_result", self.read_args(), 5
        elif self.phase == 5:
            require(value.get("trust") == "untrusted_external_report"
                    and value.get("accepted_by_body") is False, "external result lost trust boundary")
            result = value["result"]
            require(result["status"]["dispatch_id"] == self.dispatch_id
                    and result["state"] == self.terminal["state"] and not result["truncated"],
                    "result is truncated or mismatches canonical run")
            expected = "TASK_STATE_FAILED" if self.args.expect_worker_failure else "TASK_STATE_COMPLETED"
            require(result["state"] == expected, "unexpected worker outcome: " + result["state"])
            if not self.args.expect_worker_failure:
                require(result["body"] == "3973", "DSH result is not exact arithmetic answer")
            self.phase = 6
            return {"role": "assistant", "content": "EXPECTED_WORKER_FAILURE" if self.args.expect_worker_failure else "3973"}
        else:
            raise RuntimeError("unexpected extra body provider request")
        call = {"id": "probe-" + str(len(self.calls)), "type": "function",
                "function": {"name": name, "arguments": json.dumps(arguments)}}
        self.calls.append(call)
        return {"role": "assistant", "content": None, "tool_calls": [call]}

    def read_args(self):
        return {"request_id": self.request_id}

    def artifact(self, name):
        path = (self.run_dir / name).resolve(strict=True)
        require(path.is_relative_to(self.args.tachi_home), "artifact escaped Tachi home")
        require(path.stat().st_size <= 2_000_000, "artifact exceeds manual proof bound")
        return path.read_text()

    def verify_worker(self):
        status = json.loads(self.artifact("status.json"))
        require(status["dispatch_id"] == self.dispatch_id and status["state"] == self.terminal["state"],
                "disk status differs from production tool receipt")
        require(status.get("agent") == "dsh" and status.get("host_adapter") == "dsh"
                and status.get("harness_transport") == "dsh_headless", "disk status is not native DSH")
        trajectory = [json.loads(line) for line in self.artifact("trajectory.jsonl").splitlines() if line.strip()]
        starts = [e for e in trajectory if e.get("event") == "execute_started"]
        require(len(starts) == 1 and starts[0].get("dispatch_id") == self.dispatch_id
                and starts[0].get("agent") == "dsh" and starts[0].get("host_adapter") == "dsh"
                and starts[0].get("harness_transport") == "dsh_headless", "worker is not native DSH headless")
        outcomes = [e for e in trajectory if e.get("event") == "dsh_headless_result"]
        require(len(outcomes) == 1 and outcomes[0].get("dispatch_id") == self.dispatch_id,
                "missing unique DSH child outcome")
        events = [json.loads(line) for line in self.artifact("dsh-events.jsonl").splitlines() if line.strip()]
        require(events and events[0].get("type") == "session" and events[0].get("sessionId"),
                "real DSH did not open a session")
        require(sum(e.get("type") == "session" for e in events) == 1, "duplicate DSH session")
        ends = [e for e in events if e.get("type") == "status" and e.get("phase") == "turn_end"]
        require(ends and events[-1].get("type") == "final", "missing DSH terminal turn/final")
        require(not any(e.get("type") == "tool_call" for e in events), "arithmetic worker used tools")
        if self.args.expect_worker_failure:
            require(outcomes[0].get("child_exit_code") == 1 and outcomes[0].get("completed") is False
                    and ends[-1]["reason"]["kind"] == "error"
                    and ends[-1]["reason"]["error"]["code"] == "MISSING_CREDENTIAL",
                    "negative probe was not the expected real DSH authentication failure")
        else:
            require(outcomes[0].get("child_exit_code") == 0 and outcomes[0].get("completed") is True
                    and ends[-1]["reason"]["kind"] == "completed" and events[-1].get("text") == "3973"
                    and self.artifact("result.md") == "3973", "DSH child completion proof failed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True, help="dedicated loopback Tachi MCP endpoint")
    parser.add_argument("--tachi-home", required=True, type=Path)
    parser.add_argument("--zeroclaw", type=Path, default=Path.home() / ".local/bin/zeroclaw")
    parser.add_argument("--expect-worker-failure", action="store_true", help="expect MISSING_CREDENTIAL, not successful E2E")
    args = parser.parse_args()
    url = urllib.parse.urlsplit(args.endpoint)
    require(url.scheme == "http" and url.hostname in {"127.0.0.1", "localhost", "::1"}
            and not url.username and not url.password and not url.query and not url.fragment,
            "endpoint must be an uncredentialed loopback HTTP URL")
    args.tachi_home = args.tachi_home.resolve(strict=True)
    evidence = Path(tempfile.mkdtemp(prefix="zeroclaw-dsh-manual-"))
    print("Evidence: " + str(evidence), flush=True)
    before = {p.parent.resolve() for p in args.tachi_home.rglob("status.json")}
    probe = Probe(args, evidence)

    class Handler(http.server.BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def log_message(self, *_args):
            pass

        def do_POST(self):
            try:
                require(self.path == "/v1/chat/completions", "unexpected scripted provider endpoint")
                length = int(self.headers["Content-Length"])
                require(0 < length <= 1_000_000, "provider request exceeds bound")
                message = probe.reply(json.loads(self.rfile.read(length)))
            except Exception as error:
                probe.errors.append(str(error))
                message = {"role": "assistant", "content": "MANUAL_PROBE_FAILED"}
            payload = json.dumps({"id": "scripted-manual", "object": "chat.completion",
                                  "model": "synthetic", "choices": [{"index": 0, "message": message,
                                  "finish_reason": "tool_calls" if "tool_calls" in message else "stop"}],
                                  "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

    server = http.server.HTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    config_dir = evidence / "config"
    config_dir.mkdir()
    tool_list = json.dumps(TOOLS)
    (config_dir / "config.toml").write_text(f'''schema_version = 3
composition = "minimal"
[memory]
backend = "none"
auto_save = false
[providers.models.custom.probe]
uri = "http://127.0.0.1:{server.server_port}/v1/chat/completions"
api_key = "synthetic-local-dummy"
model = "synthetic"
wire_api = "chat_completions"
native_tools = true
timeout_secs = 15
[risk_profiles.probe]
level = "full"
allowed_tools = {tool_list}
auto_approve = {tool_list}
[runtime_profiles.probe]
max_tool_iterations = 128
max_actions_per_hour = 512
max_tool_result_chars = 100000
tool_call_dedup_exempt = ["tachi_start", "tachi_watch"]
[agents.probe]
model_provider = "custom.probe"
risk_profile = "probe"
runtime_profile = "probe"
[tachi]
enabled = true
endpoint = {json.dumps(args.endpoint)}
agent_identity = "zeroclaw:dsh-manual"
poll_secs = 1
[tachi.harnesses]
dsh = "dsh_executor"
''')
    started, verdict = time.monotonic(), "NOT_VERIFIED"
    try:
        with (evidence / "stdout.log").open("w") as stdout, (evidence / "stderr.log").open("w") as stderr:
            completed = subprocess.run([str(args.zeroclaw), "--config-dir", str(config_dir), "agent", "-a", "probe",
                                        "-m", "Run the authorized synthetic DSH arithmetic probe."],
                                       cwd=evidence, env={"PATH": os.defpath, "ZEROCLAW_DATA_DIR": str(evidence / "data"),
                                       "NO_PROXY": "127.0.0.1,localhost,::1"}, stdout=stdout, stderr=stderr, timeout=300)
        require(completed.returncode == 0 and not probe.errors and probe.phase == 6,
                "production CLI flow failed: " + json.dumps(probe.errors))
        probe.verify_worker()
        after = {p.parent.resolve() for p in args.tachi_home.rglob("status.json")}
        require(after - before == {probe.run_dir}, "replay/concurrent activity produced extra workers")
        expected = "EXPECTED_WORKER_FAILURE" if args.expect_worker_failure else "3973"
        require(expected in (evidence / "stdout.log").read_text().splitlines(), "CLI final reply is missing")
        verdict = "EXPECTED_WORKER_FAILURE (not successful E2E)" if args.expect_worker_failure else "SUCCESS_E2E"
        print(verdict, flush=True)
    except Exception as error:
        probe.errors.append(str(error))
        raise
    finally:
        server.shutdown()
        server.server_close()
        (evidence / "proof.json").write_text(json.dumps({"request_id": probe.request_id, "dispatch_id": probe.dispatch_id,
            "run_dir": str(probe.run_dir), "body_provider": "scripted_local_http", "dsh_service": "real",
            "verdict": verdict, "expected_worker_failure": args.expect_worker_failure,
            "elapsed_secs": time.monotonic() - started,
            "calls": probe.calls, "receipts": probe.receipts, "errors": probe.errors}, indent=2))


if __name__ == "__main__":
    main()
