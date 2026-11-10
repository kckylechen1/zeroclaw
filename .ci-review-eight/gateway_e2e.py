#!/usr/bin/env python3
"""Bounded real-process gateway integration probe using only synthetic local data.

Exercises the production wire contract documented by web/src/lib/ws.ts, not a
browser or React renderer. Run against an explicitly supplied built CLI. All
configuration, databases, responses, and credentials belong to a fresh run dir.
"""

import argparse
import asyncio
import base64
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import time
import traceback
import uuid

import aiohttp
from aiohttp import web


LIMIT = 25
OUT = Path(__file__).resolve().parent
REPO = OUT.parent / "zeroclaw-integration-eight"
HEAD = os.environ["REVIEW_SOURCE_COMMIT"]
EXPECTED_HEAD = "ff9233ed4d34913dd516de83ca5d3eeb047d1e98"
LOCAL_EQUIVALENT_HEAD = "e02a6654ada83de3f3c6594b8d084eda8897ada6"
TREE = "d95dad3aba0b62ecbf3fc039f2587d0afc8e4ba6"
BASE = "10cdae7eec403a5bbbd68cc3066a5386b41033e3"
PR_HEADS = {
    615: "dadf8e4c8560d976cecb367c3ff6670370c2b4b8",
    617: "49d0ebc2178195afb4fe358418336f878825abc3",
    619: "cfe2a9fbd1a7f0c1816dbd109bbc91b251408880",
    621: "56a8820c3f914adcbdc7fe6c827c5dcb5dd638c6",
    622: "5ba730c5bc0fedd77e28797143b65f256961f191",
    623: "0bbbf793aaf1613f1fcf03c4d7b4de582ccaff66",
    629: "244c4e06ac7ade65ec9e5ee92a17e55bb75d8935",
}
TOOL_CONTEXT = "SEVEN_PR_LOCAL_TOOL_CONTEXT"
MAIN_PROMPT = "SEVEN_PR_MAIN_IMAGE"
MAIN_ANSWER = "SEVEN_PR_MAIN_COMPLETED"
SIDE_MARKERS = ("SEVEN_PR_IDLE", "SEVEN_PR_BUSY", "SEVEN_PR_HOLD_DELETE", "SEVEN_PR_FRESH")
PNG = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a6ioAAAAASUVORK5CYII=")


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def preflight(args):
    """Bind the original seven PRs plus the independent completion attribution fix."""
    def require(condition, message):
        if not condition:
            raise AssertionError(message)

    def git(*parts):
        return subprocess.check_output(["git", "-C", str(REPO), *parts], text=True).strip()

    build_path = OUT / "build-receipt.json"
    composition_path = OUT / "composition.json"
    build = json.loads(build_path.read_text())
    composition = json.loads(composition_path.read_text())
    require(HEAD == EXPECTED_HEAD, "REVIEW_SOURCE_COMMIT differs from pinned published source")
    require(args.source_commit == HEAD, "supplied source commit differs from reviewed integration")
    require(build["exit_code"] == 0, "combined binary build did not pass")
    require(build["head"] == HEAD and build["tree"] == TREE and build["base"] == BASE,
            "build source identity differs")
    require(composition["head"] == HEAD and composition["local_equivalent_head"] == LOCAL_EQUIVALENT_HEAD
            and composition["tree"] == TREE and composition["base"] == BASE,
            "integration source composition differs")
    require(len(composition["steps"]) == 8 and
            {step["pr"]: step["source_head"] for step in composition["steps"][:7]} == PR_HEADS and
            composition["steps"][7]["label"] == "final-main-model-attribution" and
            composition["steps"][7]["source_head"] == "98a28f44ce48f1e753f6b908fc83582bd10d2051" and
            composition["parent_seven_head"] == "96f11044abb1c39379ac1b82103de1fddfeed111",
            "composition does not contain the exact seven PRs and attribution fix")
    require(build["pr_heads"] == composition["steps"], "build does not bind the seven PR heads plus attribution fix")
    require(build["features"] == "default + memory-tachi", "memory feature build differs")
    require(build["toolchain"] == "1.96.1", "build toolchain differs")
    require(git("rev-parse", "HEAD") == HEAD and git("rev-parse", "HEAD^{tree}") == TREE,
            "current integration source differs")
    require(not git("status", "--porcelain"), "integration source is dirty")
    require(git("rev-parse", "HEAD:Cargo.lock") == build["lock_blob"], "build lockfile differs")
    require(Path(build["binary"]).resolve() == args.binary.resolve() == (OUT / "zeroclaw").resolve(),
            "unexpected supplied build artifact")
    binary_hash = digest(args.binary)
    require(binary_hash == build["binary_sha256"], "combined binary changed")
    require(args.binary.stat().st_size == build["size"], "combined binary size differs")
    return {
        "head": HEAD, "tree": TREE, "base": BASE, "pr_heads": build["pr_heads"],
        "features": build["features"], "toolchain": build["toolchain"], "lock_blob": build["lock_blob"],
        "binary_sha256": binary_hash, "source_clean_at_preflight": True,
        "build_receipt": str(build_path), "build_receipt_sha256": digest(build_path),
        "composition_sha256": digest(composition_path), "harness_sha256": digest(Path(__file__)),
    }


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def content_text(value):
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(part.get("text", "") for part in value if isinstance(part, dict))
    return ""


class Probe:
    def __init__(self, args, source_binding):
        self.args = args
        self.source_binding = source_binding
        self.run_dir = args.run_dir.resolve()
        self.run_dir.mkdir(parents=True, exist_ok=False)
        self.checks = []
        self.requests = []
        self.frames = []
        self.main_entered = asyncio.Event()
        self.main_release = asyncio.Event()
        self.side_entered = asyncio.Event()
        self.side_release = asyncio.Event()
        self.side_closed = asyncio.Event()
        self.main_calls = 0
        self.runners = []
        self.process = None
        self.log = None
        self.ws = None
        self.started = time.monotonic()
        self.session_id = str(uuid.uuid4())
        self.token = "synthetic-integration-" + uuid.uuid4().hex
        self.base = "http://127.0.0.1:" + str(free_port())

    def check(self, name, condition, evidence=None):
        row = {"check": name, "passed": bool(condition)}
        if evidence is not None:
            row["evidence"] = evidence
        self.checks.append(row)
        if not condition:
            raise AssertionError(name + ": " + json.dumps(evidence, ensure_ascii=False))

    async def provider(self, request, endpoint):
        body = await request.json()
        last = content_text(body.get("messages", [{}])[-1].get("content"))
        is_side = "[SIDE QUERY (/btw)" in last
        item = {"endpoint": endpoint, "side_query": is_side, "request": body}
        self.requests.append(item)
        message = {"role": "assistant", "content": "SEVEN_PR_SIDE_ANSWER_" + endpoint}
        finish = "stop"
        if is_side and SIDE_MARKERS[2] in last:
            self.side_entered.set()
            transport = request.transport
            while not self.side_release.is_set():
                if transport is None or transport.is_closing():
                    item["client_closed_before_release"] = True
                    self.side_closed.set()
                    return web.Response(status=499)
                try:
                    await asyncio.wait_for(self.side_release.wait(), 0.05)
                except TimeoutError:
                    pass
        elif not is_side:
            self.main_calls += 1
            if self.main_calls == 1:
                message = {
                    "role": "assistant", "content": None,
                    "tool_calls": [{"index": 0, "id": "integration-file-read", "type": "function",
                                    "function": {"name": "file_read", "arguments": '{"path":"context.txt"}'}}],
                }
                finish = "tool_calls"
            else:
                self.main_entered.set()
                await asyncio.wait_for(self.main_release.wait(), LIMIT)
                message = {"role": "assistant", "content": MAIN_ANSWER}
        if body.get("stream"):
            packet = {"id": "synthetic-response", "choices": [{"index": 0, "delta": message, "finish_reason": finish}]}
            return web.Response(text="data: " + json.dumps(packet) + "\n\ndata: [DONE]\n\n", content_type="text/event-stream")
        return web.json_response({"id": "synthetic-response", "choices": [{"index": 0, "message": message, "finish_reason": finish}]})

    async def start_provider(self, name):
        app = web.Application()
        async def handler(request):
            return await self.provider(request, name)
        app.router.add_post("/v1/chat/completions", handler)
        runner = web.AppRunner(app, access_log=None, shutdown_timeout=2)
        await runner.setup()
        port = free_port()
        await web.TCPSite(runner, "127.0.0.1", port).start()
        self.runners.append(runner)
        return f"http://127.0.0.1:{port}/v1"

    async def start_gateway(self, client):
        text_url = await self.start_provider("text")
        vision_url = await self.start_provider("vision")
        config_dir = self.run_dir / "config"
        workspace = self.run_dir / "workspace"
        config_dir.mkdir()
        workspace.mkdir()
        (workspace / "context.txt").write_text(TOOL_CONTEXT + "\n")
        config = f'''schema_version = 3
locale = "en"
[providers.models.custom.text]
uri = "{text_url}"
model = "text-model"
vision = false
native_tools = true
timeout_secs = 40
[providers.models.custom.vision]
uri = "{vision_url}"
model = "vision-model"
vision = true
native_tools = true
timeout_secs = 40
[multimodal]
vision_model_provider = "custom.vision"
[agents.review]
model_provider = "custom.text"
risk_profile = "review"
runtime_profile = "review"
[agents.review.workspace]
path = {json.dumps(str(workspace))}
[agents.review.memory]
backend = "none"
[risk_profiles.review]
workspace_only = true
allowed_tools = ["file_read"]
auto_approve = ["file_read"]
always_ask = []
[runtime_profiles.review]
agentic = true
max_tool_iterations = 4
[reliability]
provider_retries = 0
[gateway]
host = "127.0.0.1"
port = {self.base.rsplit(':', 1)[1]}
require_pairing = true
paired_tokens = ["{self.token}"]
session_persistence = true
check_updates = false
[channels]
session_backend = "sqlite"
'''
        (config_dir / "config.toml").write_text(config)
        (config_dir / "config.toml").chmod(0o600)
        self.db_path = config_dir / "data" / "sessions" / "sessions.db"
        env = {k: v for k, v in os.environ.items() if not k.startswith("ZEROCLAW_") and k.lower() not in {"http_proxy", "https_proxy", "all_proxy"}}
        env["NO_PROXY"] = "127.0.0.1,localhost"
        argv = [str(self.args.binary.resolve()), "--config-dir", str(config_dir), "gateway", "start"]
        write_json(self.run_dir / "invocation.json", {"argv": argv, "source_binding": self.source_binding})
        self.log = (self.run_dir / "gateway.log").open("wb")
        self.process = await asyncio.create_subprocess_exec(*argv, cwd=workspace, env=env, stdout=self.log, stderr=asyncio.subprocess.STDOUT, start_new_session=True)
        deadline = time.monotonic() + LIMIT
        while time.monotonic() < deadline:
            if self.process.returncode is not None:
                raise RuntimeError("Gateway exited before readiness; inspect gateway.log")
            try:
                async with client.get(self.base + "/health", timeout=aiohttp.ClientTimeout(total=1)) as response:
                    if response.status == 200:
                        self.check("gateway_health", True)
                        return
            except (aiohttp.ClientError, TimeoutError):
                pass
            await asyncio.sleep(0.1)
        raise TimeoutError("Gateway health readiness exceeded limit")

    async def frame(self, predicate, timeout=LIMIT):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            message = await self.ws.receive(timeout=deadline - time.monotonic())
            if message.type != aiohttp.WSMsgType.TEXT:
                raise RuntimeError("Unexpected WS terminal/control message: " + str(message.type))
            frame = json.loads(message.data)
            self.frames.append(frame)
            if predicate(frame):
                return frame
            if frame.get("type") in {"error", "btw_error", "turn_error", "approval_request"}:
                raise AssertionError("Unexpected refusal or approval: " + json.dumps(frame))
        raise TimeoutError("Expected WebSocket frame not received")

    async def connect(self, client):
        self.ws = await client.ws_connect(self.base + f"/ws/chat?agent=review&session_id={self.session_id}&surface=web", headers={"Authorization": "Bearer " + self.token}, protocols=["zeroclaw.v1"], max_msg_size=4 * 1024 * 1024)
        start = await self.frame(lambda f: f.get("type") == "session_start")
        await self.ws.send_json({"type": "turn_control", "version": 1, "op": "negotiate", "capabilities": ["steer"]})
        ready = await self.frame(lambda f: f.get("type") == "turn_ready")
        self.epoch = ready["epoch"]
        return start

    async def side(self, marker, wait=True):
        request_id = "btw-" + str(uuid.uuid4())
        await self.ws.send_json({"type": "btw", "id": request_id, "content": "/btw " + marker})
        ack = await self.frame(lambda f: f.get("type") == "ack" and f.get("id") == request_id)
        self.check(marker + "_admission", ack.get("status") == "accepted" and ack.get("turn") == "btw" and ack.get("durable") is False, ack)
        if not wait:
            return request_id
        return await self.frame(lambda f: f.get("type") == "btw_response" and f.get("id") == request_id)

    def rows(self):
        with sqlite3.connect(self.db_path.as_uri() + "?mode=ro", uri=True, timeout=5) as connection:
            return connection.execute("SELECT role, content FROM sessions WHERE session_key=? ORDER BY id", ("gw_" + self.session_id,)).fetchall()

    async def exercise(self):
        async with aiohttp.ClientSession(timeout=aiohttp.ClientTimeout(total=LIMIT)) as client:
            await self.start_gateway(client)
            try:
                bad = await client.ws_connect(self.base + f"/ws/chat?agent=review&session_id={self.session_id}&surface=web", protocols=["zeroclaw.v1"])
                await bad.close()
                self.check("unauthenticated_websocket_denied", False, "unexpected upgrade")
            except aiohttp.WSServerHandshakeError as error:
                self.check("unauthenticated_websocket_denied", error.status == 401, error.status)
            self.check("auth_denial_has_no_provider_request", len(self.requests) == 0)
            start = await self.connect(client)
            self.check("fresh_session_empty", start.get("message_count") == 0, start)
            idle = await self.side(SIDE_MARKERS[0])
            # Agent::from_config names the default provider by its family
            # (construction.rs), while an active vision route uses its ref.
            self.check("idle_default_binding", idle.get("provider") == "custom" and idle.get("model") == "text-model" and idle.get("content") == "SEVEN_PR_SIDE_ANSWER_text", idle)
            self.check("idle_side_not_persisted", self.rows() == [])

            headers = {"Authorization": "Bearer " + self.token, "Content-Type": "image/png"}
            async with client.post(self.base + "/api/attachments", params={"agent": "review", "session_id": self.session_id, "file_name": "fixture.png"}, headers=headers, data=PNG) as response:
                uploaded = await response.json()
                self.check("image_attachment_admitted", response.status == 201, uploaded)
            main_id = str(uuid.uuid4())
            await self.ws.send_json({"type": "turn_control", "version": 1, "op": "message", "epoch": self.epoch, "request_id": main_id, "content": MAIN_PROMPT, "attachments": [uploaded["id"]]})
            await asyncio.wait_for(self.main_entered.wait(), LIMIT)
            busy = await self.side(SIDE_MARKERS[1])
            self.check("busy_vision_binding", busy.get("provider") == "custom.vision" and busy.get("model") == "vision-model" and busy.get("content") == "SEVEN_PR_SIDE_ANSWER_vision" and bool(busy.get("turn_id")), busy)
            busy_request = self.requests[-1]
            self.check("busy_actual_endpoint_and_model", busy_request["endpoint"] == "vision" and busy_request["request"]["model"] == "vision-model")
            self.check("busy_snapshot_contains_actual_tool_result", TOOL_CONTEXT in json.dumps(busy_request["request"]["messages"]))
            self.check("side_returns_while_main_still_blocked", not self.main_release.is_set())
            self.main_release.set()
            terminal = await self.frame(lambda f: f.get("type") == "turn_event" and f.get("request_id") == main_id and f.get("terminal") is True)
            self.check("main_turn_completes", terminal.get("event", {}).get("outcome") == "done", terminal)
            completion = next((frame.get("event", {}) for frame in self.frames
                               if frame.get("type") == "turn_event"
                               and frame.get("request_id") == main_id
                               and frame.get("event", {}).get("type") == "done"), {})
            self.check("main_completion_model_matches_actual_route",
                       completion.get("model") == "vision-model", completion)
            self.check("main_completion_provider_ref_is_additive",
                       completion.get("provider_ref") == "custom.vision"
                       and completion.get("provider") == "custom", completion)
            rows = self.rows()
            write_json(self.run_dir / "persisted-before-delete.json", rows)
            row_text = json.dumps(rows)
            self.check("main_transcript_persisted", MAIN_PROMPT in row_text and MAIN_ANSWER in row_text, {"rows": len(rows)})
            self.check("side_exchanges_absent_from_sqlite", all(marker not in row_text for marker in SIDE_MARKERS) and "SEVEN_PR_SIDE_ANSWER" not in row_text)

            blocked_id = await self.side(SIDE_MARKERS[2], wait=False)
            await asyncio.wait_for(self.side_entered.wait(), LIMIT)
            await self.ws.close()
            self.ws = None
            # No provider response is released until DELETE has completed.
            async with client.delete(self.base + f"/api/sessions/{self.session_id}", headers={"Authorization": "Bearer " + self.token}) as response:
                deleted = await response.json()
                self.check("sqlite_session_delete_after_last_socket", response.status == 200 and deleted.get("deleted") is True, deleted)
            await asyncio.wait_for(self.side_closed.wait(), 8)
            self.check("blocked_provider_cancelled_after_delete_completed", self.side_closed.is_set() and not self.side_release.is_set())
            self.check("deleted_sqlite_history_empty", self.rows() == [])
            self.side_release.set()
            start = await self.connect(client)
            self.check("reconnect_has_no_deleted_history", start.get("message_count") == 0 and not start.get("resumed"), start)
            fresh = await self.side(SIDE_MARKERS[3])
            self.check("fresh_default_provider_restored", fresh.get("provider") == "custom" and fresh.get("model") == "text-model" and fresh.get("content") == "SEVEN_PR_SIDE_ANSWER_text", fresh)
            fresh_request = self.requests[-1]
            self.check("fresh_actual_endpoint_and_context", fresh_request["endpoint"] == "text" and fresh_request["request"]["model"] == "text-model" and MAIN_PROMPT not in json.dumps(fresh_request["request"]["messages"]) and TOOL_CONTEXT not in json.dumps(fresh_request["request"]["messages"]))
            self.check("no_stale_frame_observed_through_fresh_completion", not any(f.get("id") == blocked_id and f.get("type") in {"btw_response", "btw_error"} for f in self.frames))
            self.check("all_side_requests_have_no_tools", all(not item["request"].get("tools") for item in self.requests if item["side_query"]))
            self.check("main_requests_use_vision", all(item["endpoint"] == "vision" and item["request"]["model"] == "vision-model" for item in self.requests if not item["side_query"]))
            self.check("fresh_side_remains_unpersisted", self.rows() == [])
            await self.ws.close()
            self.ws = None

    async def run(self):
        result = {
            "result": "FAIL", "source_binding": self.source_binding,
            "scope": "Original 28 gateway lifecycle checks plus final main model and additive provider_ref on the combined eight-change binary; synthetic loopback providers, production S2/btw wire contract and active SQLite.",
            "limitations": [
                "This is not browser UI or external provider acceptance.",
                "The blocked provider returns HTTP 499 after detecting client closure; it does not produce a delayed successful response. Provider-completion/publication races require the separate deterministic Rust regression.",
                "No stale-frame claim extends beyond the frames consumed through the fresh-query completion.",
                "Connection closure is observed after the last WebSocket closes and DELETE completes; the deterministic lifecycle test separately isolates DELETE-triggered invalidation.",
            ],
        }
        try:
            await asyncio.wait_for(self.exercise(), 150)
            result["result"] = "PASS"
        except BaseException as error:
            result["error"] = repr(error)
            result["traceback"] = traceback.format_exc()
        finally:
            self.main_release.set()
            self.side_release.set()
            if self.ws is not None:
                await self.ws.close()
            if self.process is not None and self.process.returncode is None:
                os.killpg(self.process.pid, signal.SIGTERM)
                try:
                    await asyncio.wait_for(self.process.wait(), 5)
                except TimeoutError:
                    os.killpg(self.process.pid, signal.SIGKILL)
                    await self.process.wait()
            if self.log is not None:
                self.log.close()
            for runner in self.runners:
                await runner.cleanup()
            result.update({"checks": self.checks, "passed": sum(c["passed"] for c in self.checks), "failed": sum(not c["passed"] for c in self.checks), "provider_requests": len(self.requests), "seconds": round(time.monotonic() - self.started, 3)})
            write_json(self.run_dir / "provider-requests.json", self.requests)
            write_json(self.run_dir / "ws-frames.json", self.frames)
            write_json(self.run_dir / "result.json", result)
        print(json.dumps({k: result[k] for k in ("result", "passed", "failed", "provider_requests", "seconds")}, ensure_ascii=False))
        return 0 if result["result"] == "PASS" else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--run-dir", type=Path, required=True, help="Must not exist; stores synthetic config and raw evidence")
    parser.add_argument("--source-commit", required=True)
    args = parser.parse_args()
    if not args.binary.is_file():
        parser.error("--binary must be the already-built actual CLI")
    binding = preflight(args)
    return asyncio.run(Probe(args, binding).run())


if __name__ == "__main__":
    raise SystemExit(main())
