"""Bounded RFC 6455 JSON transport for the local ZeroClaw gateway."""

import asyncio
import base64
import hashlib
import json
import secrets
import struct

MAX_MESSAGE = 131072


class BridgeError(Exception):
    """Safe to log: never attach peer payloads or credential values."""


class Wire:
    def __init__(self, reader, writer):
        self.reader = reader
        self.writer = writer

    @classmethod
    async def open(cls, host, port, path, token):
        reader, writer = await asyncio.open_connection(host, port, limit=16384)
        wire = cls(reader, writer)
        try:
            key = base64.b64encode(secrets.token_bytes(16)).decode("ascii")
            authority = f"[{host}]" if ":" in host else host
            headers = (
                f"GET {path} HTTP/1.1\r\nHost: {authority}:{port}\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
                "Sec-WebSocket-Protocol: zeroclaw.v1\r\n"
                f"Authorization: Bearer {token}\r\n\r\n"
            )
            writer.write(headers.encode("ascii"))
            await writer.drain()
            raw = await reader.readuntil(b"\r\n\r\n")
            if len(raw) > 16384:
                raise BridgeError("Invalid gateway handshake")
            lines = raw.decode("ascii").split("\r\n")
            fields = {}
            for line in filter(None, lines[1:]):
                name, value = line.split(":", 1)
                name = name.lower()
                if name in fields:
                    raise BridgeError("Duplicate handshake header")
                fields[name] = value.strip()
            digest = hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
            ).digest()
            if (
                lines[0].split()[:2] != ["HTTP/1.1", "101"]
                or fields.get("sec-websocket-accept") != base64.b64encode(digest).decode()
                or fields.get("sec-websocket-protocol") != "zeroclaw.v1"
                or fields.get("upgrade", "").lower() != "websocket"
                or "upgrade" not in [v.strip().lower() for v in fields.get("connection", "").split(",")]
                or "sec-websocket-extensions" in fields
            ):
                raise BridgeError("Gateway handshake rejected")
            return wire
        except BaseException:
            wire.close()
            raise

    def close(self):
        self.writer.transport.abort()

    async def frame(self, opcode, payload):
        size = len(payload)
        if size > MAX_MESSAGE or (opcode >= 8 and size > 125):
            raise BridgeError("Outgoing frame too large")
        if size < 126:
            length = bytes([0x80 | size])
        elif size <= 65535:
            length = b"\xfe" + struct.pack("!H", size)
        else:
            length = b"\xff" + struct.pack("!Q", size)
        mask = secrets.token_bytes(4)
        masked = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        self.writer.write(bytes([0x80 | opcode]) + length + mask + masked)
        await self.writer.drain()

    async def send(self, value):
        await self.frame(1, json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode())

    async def receive(self):
        message = bytearray()
        fragmented = False
        while True:
            flags, length = await self.reader.readexactly(2)
            opcode, final = flags & 15, bool(flags & 128)
            if flags & 112 or length & 128:
                raise BridgeError("Invalid server frame flags")
            size = length & 127
            if size == 126:
                size = struct.unpack("!H", await self.reader.readexactly(2))[0]
                if size < 126:
                    raise BridgeError("Noncanonical frame length")
            elif size == 127:
                size = struct.unpack("!Q", await self.reader.readexactly(8))[0]
                if size < 65536:
                    raise BridgeError("Noncanonical frame length")
            if size > MAX_MESSAGE or len(message) + size > MAX_MESSAGE:
                raise BridgeError("Incoming message too large")
            if opcode >= 8 and (not final or size > 125):
                raise BridgeError("Invalid control frame")
            payload = await self.reader.readexactly(size)
            if opcode == 8:
                raise ConnectionError("Gateway closed")
            if opcode == 9:
                await self.frame(10, payload)
                continue
            if opcode == 10:
                continue
            if opcode not in (0, 1) or (opcode == 0) != fragmented:
                raise BridgeError("Invalid text fragmentation")
            message.extend(payload)
            if final:
                try:
                    value = json.loads(message.decode("utf-8"))
                except (ValueError, UnicodeError, RecursionError):
                    raise BridgeError("Invalid gateway JSON") from None
                if not isinstance(value, dict):
                    raise BridgeError("Expected gateway object")
                return value
            fragmented = True
