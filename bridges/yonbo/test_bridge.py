"""Offline contract tests; no robot, owner service or real credential access."""

import asyncio
import base64
from contextlib import suppress
from dataclasses import replace
import hashlib
import json
from pathlib import Path
import struct
import tempfile
import time
import unittest
from unittest.mock import patch
import uuid

from bridges.yonbo import brain_zeroclaw as bridge
from bridges.yonbo.brain_zeroclaw import BrainRequest, Pending, ZeroClawBridge, credential
from bridges.yonbo.wire import BridgeError, Wire

TOKEN = "synthetic_test_token_0123456789abcdef"
REQUEST = BrainRequest("test-device", "test-session", 1, "normal", "Hello", True)
TURN = str(uuid.UUID(int=1))


class MemoryWriter:
    def __init__(self):
        self.data = bytearray()
        self.transport = self
        self.closed = False

    def write(self, data):
        self.data.extend(data)

    async def drain(self):
        pass

    def abort(self):
        self.closed = True


def server_frame(payload, opcode=1, final=True):
    size = len(payload)
    header = bytes([(128 if final else 0) | opcode])
    if size < 126:
        return header + bytes([size]) + payload
    if size <= 65535:
        return header + b"\x7e" + struct.pack("!H", size) + payload
    return header + b"\x7f" + struct.pack("!Q", size) + payload


class ScriptWire:
    def __init__(self, frames=()):
        self.frames = list(frames)
        self.sent = []
        self.closed = False

    async def receive(self):
        if not self.frames:
            raise ConnectionError("Synthetic disconnect")
        frame = self.frames.pop(0)
        if isinstance(frame, Exception):
            raise frame
        return frame

    async def send(self, value):
        self.sent.append(value)

    def close(self):
        self.closed = True


def envelope(pending, kind, **fields):
    return dict(type=kind, version=1, epoch=pending.epoch,
                request_id=pending.request_id, turn_id=TURN, **fields)


def completed(pending):
    return [
        envelope(pending, "turn_event", seq=1, event={"type": "chunk", "content": "Hello"}),
        envelope(pending, "turn_event", seq=2, event={"type": "done", "full_response": "Hello"}),
        envelope(pending, "turn_event", seq=3, event={"type": "turn_terminal", "outcome": "done"},
                 terminal=True, stream_complete=True),
    ]


class CredentialTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "token"
        self.path.write_text(TOKEN)
        self.path.chmod(0o600)

    def test_owner_file(self):
        self.assertEqual(credential(self.path), TOKEN)

    def test_wrong_mode_symlink_hardlink_and_directory(self):
        self.path.chmod(0o644)
        with self.assertRaises(BridgeError):
            credential(self.path)
        self.path.chmod(0o600)
        alias = self.path.with_name("alias")
        alias.symlink_to(self.path)
        with self.assertRaises(BridgeError):
            credential(alias)
        alias.unlink()
        alias.hardlink_to(self.path)
        with self.assertRaises(BridgeError):
            credential(self.path)
        with self.assertRaises(BridgeError):
            credential(self.path.parent)

    def test_repository_file_and_invalid_token(self):
        (self.path.parent / ".git").mkdir()
        with self.assertRaises(BridgeError):
            credential(self.path)
        (self.path.parent / ".git").rmdir()
        for value in ("short", "a" * 257, "a" * 32 + "\r\nInjected: value", "非ASCII"):
            self.path.write_text(value)
            with self.assertRaises(BridgeError):
                credential(self.path)

    def test_owner_mismatch(self):
        with patch.object(bridge.os, "getuid", return_value=-1):
            with self.assertRaises(BridgeError):
                credential(self.path)

    def test_loopback_only(self):
        for endpoint in ("ws://example.invalid:1234/ws/chat", "wss://127.0.0.1:1234/ws/chat",
                         "ws://127.0.0.1:1234/ws/chat?token=ignored", "ws://user@127.0.0.1:1234/ws/chat"):
            with self.assertRaises(BridgeError):
                ZeroClawBridge(endpoint, "test", self.path)


class WireTests(unittest.IsolatedAsyncioTestCase):
    def wire(self, data):
        reader = asyncio.StreamReader()
        reader.feed_data(data)
        reader.feed_eof()
        return Wire(reader, MemoryWriter())

    async def test_fragmented_unicode_with_ping(self):
        payload = json.dumps({"text": "中文 🦆"}, ensure_ascii=False).encode()
        wire = self.wire(server_frame(payload[:12], final=False)
                         + server_frame(b"ping", opcode=9)
                         + server_frame(payload[12:], opcode=0))
        self.assertEqual(await wire.receive(), {"text": "中文 🦆"})
        self.assertEqual(wire.writer.data[0], 0x8A)
        self.assertTrue(wire.writer.data[1] & 128)

    async def test_masked_output_all_length_encodings(self):
        for size in (5, 126, 65536):
            wire = self.wire(b"")
            payload = b"x" * size
            await wire.frame(1, payload)
            raw = wire.writer.data
            marker = raw[1] & 127
            offset = 2 if marker < 126 else 4 if marker == 126 else 10
            mask = raw[offset:offset + 4]
            decoded = bytes(c ^ mask[i % 4] for i, c in enumerate(raw[offset + 4:]))
            self.assertEqual(decoded, payload)
            self.assertTrue(raw[1] & 128)

    async def test_reject_invalid_frames(self):
        cases = [b"\x81\x80", b"\xc1\x00", server_frame(b"[]"),
                 server_frame(b"\xff"), server_frame(b"{}", opcode=0),
                 server_frame(b"x", opcode=2), server_frame(b"x", opcode=9, final=False),
                 b"\x81\x7e\x00\x01x", b"\x81\x7f" + struct.pack("!Q", 131073)]
        for data in cases:
            with self.subTest(data=data):
                with self.assertRaises(BridgeError):
                    await self.wire(data).receive()

    async def test_handshake_and_bad_credential_response(self):
        writers = []

        async def open_connection(host, port, limit):
            reader = asyncio.StreamReader()

            class HandshakeWriter(MemoryWriter):
                async def drain(self):
                    lines = dict(line.split(b": ", 1) for line in bytes(self.data).split(b"\r\n")[1:] if line)
                    if lines[b"Authorization"] != b"Bearer " + TOKEN.encode():
                        reader.feed_data(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                    else:
                        digest = hashlib.sha1(lines[b"Sec-WebSocket-Key"] + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest()
                        reader.feed_data(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                                         b"Connection: keep-alive,Upgrade\r\nSec-WebSocket-Protocol: zeroclaw.v1\r\n"
                                         b"Sec-WebSocket-Accept: " + base64.b64encode(digest) + b"\r\n\r\n")
                    reader.feed_eof()

            writer = HandshakeWriter()
            writers.append(writer)
            return reader, writer

        with patch("bridges.yonbo.wire.asyncio.open_connection", side_effect=open_connection):
            wire = await Wire.open("127.0.0.1", 12345, "/ws/chat", TOKEN)
            wire.close()
            with self.assertRaises(BridgeError):
                await Wire.open("127.0.0.1", 12345, "/ws/chat", "wrong")
        self.assertTrue(all(writer.closed for writer in writers))


class BridgeTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        path = Path(self.temp.name) / "token"
        path.write_text(TOKEN)
        path.chmod(0o600)
        self.brain = ZeroClawBridge("ws://127.0.0.1:12345/ws/chat", "test", path)

    def pending(self):
        return Pending("test", str(uuid.uuid4()), epoch="epoch")

    async def test_contiguous_terminal_required_and_stale_generation_ignored(self):
        pending = self.pending()
        events = completed(pending)
        old = dict(events[0], request_id=str(uuid.uuid4()))
        pending.wire = ScriptWire([old, events[0], events[0], *events[1:]])
        self.assertEqual(await self.brain.consume(pending), "Hello")
        for frames in ([events[2]], events[:2], [dict(events[0], epoch="other")],
                       [events[0], events[1], dict(events[2], stream_complete=False)]):
            p = Pending("test", pending.request_id, epoch="epoch", wire=ScriptWire(frames))
            with self.assertRaises((BridgeError, ConnectionError)):
                await self.brain.consume(p)

    async def test_live_before_replay_and_overlap(self):
        pending = self.pending()
        events = completed(pending)
        replay = envelope(pending, "turn_replay", gap=False, events=events[:2])
        pending.wire = ScriptWire([events[1], events[2], replay])
        self.assertEqual(await self.brain.consume(pending, recovering=True), "Hello")
        for bad in (dict(replay, gap=True), dict(replay, events=[events[1]]),
                    envelope(pending, "turn_ack", status="unknown")):
            p = Pending("test", pending.request_id, epoch="epoch", wire=ScriptWire([bad]))
            with self.assertRaises(BridgeError):
                await self.brain.consume(p, recovering=True)

    async def test_lost_ack_resumes_without_resubmission(self):
        wires = []

        async def connect(pending):
            pending.epoch = "epoch"
            frames = [ConnectionError()] if not wires else [
                envelope(pending, "turn_replay", gap=False, events=completed(pending))]
            pending.wire = ScriptWire(frames)
            wires.append(pending.wire)

        self.brain.connect = connect
        self.assertEqual(await self.brain.reply(REQUEST), "Hello")
        self.assertEqual([w.sent[0]["op"] for w in wires], ["message", "resume"])
        self.assertEqual(wires[0].sent[0]["request_id"], wires[1].sent[0]["request_id"])
        self.assertTrue(all(w.closed for w in wires))
        self.assertFalse(self.brain.pending)
        with self.assertRaises(BridgeError):
            await self.brain.reply(REQUEST)

    async def test_restart_refuses_old_epoch_but_new_generation_can_connect(self):
        calls = []

        async def open_wire(host, port, path, token):
            from urllib.parse import parse_qs, urlsplit
            session = parse_qs(urlsplit(path).query)["session_id"][0]
            epoch = "old" if not calls else "new"
            wire = ScriptWire([{"type": "session_start", "session_id": session},
                               {"type": "connected"}, {"type": "turn_ready", "version": 1, "epoch": epoch}])
            calls.append(wire)
            return wire

        with patch.object(Wire, "open", side_effect=open_wire):
            with self.assertRaisesRegex(BridgeError, "epoch changed"):
                await self.brain.reply(REQUEST)
            self.assertEqual(sum(f["op"] == "message" for w in calls for f in w.sent if "op" in f), 1)
            with self.assertRaises(BridgeError):
                await self.brain.cancel(REQUEST.session_id, 1)
            fresh = self.pending()
            fresh.epoch = None
            await self.brain.connect(fresh)
            self.assertEqual(fresh.epoch, "new")
            fresh.close()
        self.assertFalse(self.brain.pending)

    async def test_text_admission(self):
        for request in (replace(REQUEST, image="image"), replace(REQUEST, mode="video"),
                        replace(REQUEST, book_type="book"), replace(REQUEST, text=" "),
                        replace(REQUEST, text="x" * 16385), replace(REQUEST, generation=True)):
            with self.assertRaises(BridgeError):
                await self.brain.reply(request)
        self.assertFalse(self.brain.pending)
        self.assertFalse(self.brain.seen)

    def install_waiting_transport(self):
        entered = asyncio.Event()
        wires = []
        binding = []

        async def connect(pending):
            pending.epoch = "epoch"
            frames = [] if not wires else [
                envelope(pending, "turn_replay", gap=False, events=[]),
                envelope(pending, "turn_cancel_ack", status="cancelled")]
            pending.wire = ScriptWire(frames)
            wires.append(pending.wire)
            binding.append(pending)

        async def consume(pending, recovering=False):
            entered.set()
            await asyncio.Event().wait()

        self.brain.connect = connect
        self.brain.consume = consume
        return entered, wires, binding

    async def test_python312_wait_for_timeout_preserves_http_handler(self):
        entered, wires, binding = self.install_waiting_transport()

        async def handler():
            try:
                await asyncio.wait_for(self.brain.reply(REQUEST), 0.03)
            except TimeoutError:
                pass
            finally:
                await asyncio.wait_for(self.brain.cancel(REQUEST.session_id, 1), 1)
            return "handler survived"

        caller = asyncio.create_task(handler())
        await entered.wait()
        self.assertIsNot(binding[0].task, caller)
        self.assertEqual(await caller, "handler survived")
        self.assertFalse(caller.cancelled())
        self.assertEqual([f["op"] for f in wires[-1].sent], ["resume", "cancel"])
        self.assertTrue(all(w.closed for w in wires))
        self.assertFalse(self.brain.pending)

    async def test_bridge_owns_real_ten_second_budget(self):
        _, wires, _ = self.install_waiting_transport()
        started = time.monotonic()
        with self.assertRaises(TimeoutError):
            await self.brain.reply(REQUEST)
        elapsed = time.monotonic() - started
        self.assertGreaterEqual(elapsed, 10)
        self.assertLess(elapsed, 11)
        await self.brain.cancel(REQUEST.session_id, 1)
        self.assertTrue(all(w.closed for w in wires))

    async def test_concurrent_cancel_has_one_shielded_owned_cleanup(self):
        entered, wires, binding = self.install_waiting_transport()
        caller = asyncio.create_task(self.brain.reply(REQUEST))
        await entered.wait()
        cleanup_entered = asyncio.Event()
        cleanup_release = asyncio.Event()
        original_connect = self.brain.connect

        async def blocked_connect(pending):
            cleanup_entered.set()
            await cleanup_release.wait()
            await original_connect(pending)

        self.brain.connect = blocked_connect
        first = asyncio.create_task(self.brain.cancel(REQUEST.session_id, 1))
        await cleanup_entered.wait()
        first.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await first
        cleanup = binding[0].cleanup
        self.assertFalse(cleanup.cancelled())
        cleanup_release.set()
        await self.brain.cancel(REQUEST.session_id, 1)
        with suppress(asyncio.CancelledError):
            await caller
        self.assertIs(binding[0].cleanup, cleanup)
        self.assertEqual(len(wires), 2)
        self.assertTrue(all(w.closed for w in wires))
        self.assertFalse(self.brain.pending)


if __name__ == "__main__":
    unittest.main()
