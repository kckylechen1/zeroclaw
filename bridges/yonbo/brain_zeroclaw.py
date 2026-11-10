"""S3 restricted text bridge. No vendor credentials, device I/O or effect replay."""

import asyncio
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import uuid
from urllib.parse import urlencode, urlsplit

from .wire import BridgeError, Wire

TURN_SECONDS = 10
CANCEL_SECONDS = 0.8
MAX_TEXT_BYTES = 16384


def read_local_file(path, max_bytes=65536):
    """Read an owner-local regular 0600 file, outside this checkout and any repo."""
    try:
        resolved = Path(path).expanduser().resolve(strict=True)
        checkout = Path(__file__).resolve().parents[2]
        if resolved.is_relative_to(checkout) or any((p / ".git").exists() for p in resolved.parents):
            raise BridgeError("Credential files must be outside repositories")
        fd = os.open(Path(path).expanduser(), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        try:
            info = os.fstat(fd)
            if (not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) != 0o600
                    or info.st_uid != os.getuid() or info.st_nlink != 1 or info.st_size > max_bytes):
                raise BridgeError("Credential file must be owner-only regular mode 0600")
            data = os.read(fd, max_bytes + 1)
            if len(data) > max_bytes:
                raise BridgeError("Credential file too large")
            return data
        finally:
            os.close(fd)
    except (OSError, ValueError, UnicodeError, TypeError):
        raise BridgeError("Cannot read local credential file") from None


def read_secret(path):
    try:
        value = read_local_file(path, 256).decode("ascii").strip()
    except UnicodeError:
        raise BridgeError("Invalid credential format") from None
    if not value or any(ord(c) < 33 or ord(c) > 126 for c in value):
        raise BridgeError("Invalid credential format")
    return value


def credential(path):
    token = read_secret(path)
    if not re.fullmatch(r"[A-Za-z0-9_-]{32,128}", token):
        raise BridgeError("Invalid bridge credential format")
    return token


@dataclass(frozen=True)
class BrainRequest:
    device_id: str
    session_id: str
    generation: int
    mode: str
    text: str
    is_new: bool
    image: str | None = None
    book_type: str | None = None


@dataclass
class Pending:
    session: str
    request_id: str
    task: asyncio.Task | None = None
    cleanup: asyncio.Task | None = None
    wire: Wire | None = None
    epoch: str | None = None
    turn: str | None = None
    seq: int = 0
    delta: str = ""
    answer: str | None = None
    submitted: bool = False
    cancelled: bool = False

    def close(self):
        if self.wire is not None:
            self.wire.close()
            self.wire = None


class ZeroClawBridge:
    def __init__(self, endpoint, agent, credential_file):
        try:
            url = urlsplit(endpoint)
            valid = (url.scheme == "ws" and url.hostname in ("127.0.0.1", "::1")
                     and url.port and url.path == "/ws/chat" and not url.query
                     and not url.fragment and not url.username and not url.password
                     and isinstance(agent, str) and re.fullmatch(r"[A-Za-z0-9_-]{1,64}", agent))
            if not valid:
                raise ValueError
        except (ValueError, TypeError, AttributeError):
            raise BridgeError("Expected numeric loopback gateway and explicit agent alias") from None
        self.host, self.port, self.agent = url.hostname, url.port, agent
        self.token = credential(credential_file)
        self.namespace = uuid.uuid4()
        self.pending = {}
        self.seen = set()

    async def connect(self, pending):
        path = "/ws/chat?" + urlencode({"agent": self.agent, "session_id": pending.session})
        pending.wire = await Wire.open(self.host, self.port, path, self.token)

        async def expect(kind):
            for _ in range(16):
                frame = await pending.wire.receive()
                if frame.get("type") in ("error", "turn_error"):
                    raise BridgeError("Gateway negotiation rejected")
                if frame.get("type") == kind:
                    return frame
            raise BridgeError("Gateway negotiation incomplete")

        start = await expect("session_start")
        if start.get("session_id") != pending.session:
            raise BridgeError("Session mismatch")
        await pending.wire.send({"type": "connect"})
        await expect("connected")
        await pending.wire.send({"type": "turn_control", "version": 1, "op": "negotiate"})
        ready = await expect("turn_ready")
        epoch = ready.get("epoch")
        if type(ready.get("version")) is not int or ready["version"] != 1 or not isinstance(epoch, str) or not epoch:
            raise BridgeError("Invalid turn negotiation")
        if pending.epoch is not None and pending.epoch != epoch:
            raise BridgeError("Gateway epoch changed; outcome unknown")
        pending.epoch = epoch

    @staticmethod
    def control(pending, operation, **fields):
        return dict(type="turn_control", version=1, epoch=pending.epoch,
                    request_id=pending.request_id, op=operation, **fields)

    @staticmethod
    def bound(pending, frame):
        if (not isinstance(frame, dict) or type(frame.get("version")) is not int
                or frame["version"] != 1 or frame.get("epoch") != pending.epoch
                or frame.get("request_id") != pending.request_id):
            raise BridgeError("Turn identity mismatch")
        turn = frame.get("turn_id")
        try:
            if not isinstance(turn, str):
                raise ValueError
            uuid.UUID(turn)
        except ValueError:
            raise BridgeError("Invalid turn binding") from None
        if pending.turn is not None and pending.turn != turn:
            raise BridgeError("Turn binding changed")
        pending.turn = turn

    def event(self, pending, frame):
        self.bound(pending, frame)
        seq = frame.get("seq")
        if type(seq) is not int or seq < 1:
            raise BridgeError("Invalid event sequence")
        if seq <= pending.seq:
            return False
        if seq != pending.seq + 1:
            raise BridgeError("Incomplete event sequence")
        event = frame.get("event")
        if not isinstance(event, dict):
            raise BridgeError("Invalid turn event")
        kind = event.get("type")
        if kind in ("error", "turn_gap", "approval_request", "question"):
            raise BridgeError("Turn cannot complete as text")
        if kind in ("chunk", "done"):
            text = event.get("content" if kind == "chunk" else "full_response")
            if not isinstance(text, str) or len(text.encode()) > MAX_TEXT_BYTES:
                raise BridgeError("Invalid reply text")
            if kind == "chunk":
                pending.delta += text
                if len(pending.delta.encode()) > MAX_TEXT_BYTES:
                    raise BridgeError("Reply too large")
            else:
                if pending.answer is not None:
                    raise BridgeError("Duplicate done event")
                pending.answer = text
        pending.seq = seq
        if frame.get("terminal") is True:
            if (kind != "turn_terminal" or event.get("outcome") != "done"
                    or frame.get("stream_complete") is not True or pending.answer is None):
                raise BridgeError("Turn did not complete")
            return True
        if kind == "turn_terminal":
            raise BridgeError("Invalid terminal event")
        return False

    async def consume(self, pending, recovering=False):
        buffered = []
        buffered_bytes = 0
        for _ in range(512):
            frame = await pending.wire.receive()
            kind = frame.get("type")
            if kind in ("turn_ack", "turn_event", "turn_replay") and frame.get("request_id") != pending.request_id:
                continue  # Same-session subscription can carry an older generation.
            if recovering and kind == "turn_event":
                buffered_bytes += len(json.dumps(frame).encode())
                if len(buffered) >= 128 or buffered_bytes > 131072:
                    raise BridgeError("Recovery buffer full")
                buffered.append(frame)
                continue
            if kind == "turn_ack":
                self.bound(pending, frame)
                if frame.get("status") not in ("accepted", "duplicate"):
                    raise BridgeError("Request outcome unknown")
            elif kind == "turn_event":
                if self.event(pending, frame):
                    return pending.answer
            elif kind == "turn_replay":
                self.bound(pending, frame)
                events = frame.get("events")
                if not recovering or frame.get("gap") is not False or not isinstance(events, list) or len(events) > 129:
                    raise BridgeError("Replay unavailable or incomplete")
                for event in events + buffered:
                    if self.event(pending, event):
                        return pending.answer
                recovering = False
                buffered.clear()
            elif kind in ("error", "turn_error", "turn_gap"):
                raise BridgeError("Gateway turn failed")
        raise BridgeError("Turn frame limit reached")

    async def reply(self, request):
        if (not isinstance(request.device_id, str) or not 1 <= len(request.device_id) <= 256
                or not isinstance(request.session_id, str) or not 1 <= len(request.session_id) <= 256
                or type(request.generation) is not int or request.generation < 1
                or request.mode != "normal" or request.image is not None or request.book_type is not None
                or not isinstance(request.text, str) or not request.text.strip()
                or len(request.text.encode()) > MAX_TEXT_BYTES):
            raise BridgeError("Expected bounded text-only request")
        key = (request.session_id, request.generation)
        if key in self.seen or len(self.seen) >= 4096 or len(self.pending) >= 16:
            raise BridgeError("Duplicate request or bridge capacity reached")
        identity = json.dumps([request.device_id, request.session_id], separators=(",", ":"))
        pending = Pending("s3-" + hashlib.sha256(identity.encode()).hexdigest(),
                          str(uuid.uuid5(self.namespace, identity + ":" + str(request.generation))))
        self.seen.add(key)
        self.pending[key] = pending
        # Round-2 invariant: only this dedicated child is cancellation authority.
        # Python 3.12 wait_for may run reply in the HTTP handler's own task.
        pending.task = asyncio.create_task(self._reply_io(key, pending, request.text))
        return await pending.task

    async def _reply_io(self, key, pending, text):
        success = False
        try:
            async with asyncio.timeout(TURN_SECONDS):
                for attempt in range(3):
                    try:
                        await self.connect(pending)
                        recovering = pending.submitted
                        if recovering:
                            await pending.wire.send(self.control(pending, "resume", after_seq=pending.seq))
                        else:
                            # A partial write is uncertain: never send message again.
                            pending.submitted = True
                            await pending.wire.send(self.control(pending, "message", content=text))
                        answer = await self.consume(pending, recovering)
                        if pending.cancelled:
                            raise BridgeError("Turn cancelled")
                        success = True
                        return answer
                    except (ConnectionError, asyncio.IncompleteReadError, OSError):
                        pending.close()
                        if attempt == 2:
                            raise BridgeError("Gateway unavailable; outcome unknown") from None
                        await asyncio.sleep(0.05)
        finally:
            pending.close()
            if success or not pending.submitted:
                self.pending.pop(key, None)
            # Retain uncertain bindings for the HTTP caller's finally/cancel path.

    async def cancel(self, session_id, generation):
        key = (session_id, generation)
        pending = self.pending.get(key)
        if pending is None:
            return
        if pending.cleanup is None:
            pending.cancelled = True
            pending.cleanup = asyncio.create_task(self._cancel_owned(key, pending))
            pending.cleanup.add_done_callback(lambda task: None if task.cancelled() else task.exception())
        await asyncio.shield(pending.cleanup)

    async def _cancel_owned(self, key, pending):
        try:
            async with asyncio.timeout(CANCEL_SECONDS):
                if pending.task is not None and not pending.task.done():
                    pending.task.cancel()
                    await asyncio.gather(pending.task, return_exceptions=True)
                pending.close()
                if not pending.submitted:
                    return
                await self.connect(pending)
                await pending.wire.send(self.control(pending, "resume", after_seq=pending.seq))
                for _ in range(32):
                    frame = await pending.wire.receive()
                    if frame.get("type") == "turn_replay" and frame.get("request_id") == pending.request_id:
                        self.bound(pending, frame)
                        break
                    if frame.get("type") in ("error", "turn_error", "turn_ack"):
                        raise BridgeError("Cancellation binding unavailable")
                else:
                    raise BridgeError("Cancellation binding unavailable")
                await pending.wire.send(self.control(pending, "cancel", turn_id=pending.turn))
                for _ in range(32):
                    frame = await pending.wire.receive()
                    if frame.get("type") == "turn_cancel_ack" and frame.get("request_id") == pending.request_id:
                        self.bound(pending, frame)
                        return  # Requested only; this is not rollback/completion proof.
                    if frame.get("type") in ("error", "turn_error"):
                        raise BridgeError("Cancellation rejected")
                raise BridgeError("Cancellation acknowledgement missing")
        finally:
            pending.close()
            self.pending.pop(key, None)
