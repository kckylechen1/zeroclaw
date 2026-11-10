# Yonbo S3 text bridge

This is the durable home requested by [#627](https://github.com/kckylechen1/zeroclaw/issues/627)
for the restricted device dialogue brain from [#499](https://github.com/kckylechen1/zeroclaw/issues/499).
It uses Python 3.12+ and the standard library; it is not a Cargo member.
`ZeroClawBridge.reply(request)` returns text; `cancel(session_id, generation)`
requests cancellation of that exact turn. No robot, motor, audio or approval
operation is implemented here.

## Recovery boundary

The bridge is self-contained. **The vendor HTTP/TTS host is not recovered here.**
`adapter.py` is the recovered four-mode wiring for an owner-installed
`yonbo_adapter` package (`server`, `auth`, `brain_spark`, plus their dependencies).
That package is not a public pip dependency. Without it the launcher fails closed
with a prerequisite error; restoring this directory alone does not restore the
robot service. Do not substitute a newly invented vendor protocol.

Recovered evidence includes the pre-round-2 bridge and CLI, later numbered source
for `_reply_io`, shared shielded cancellation, live-before-replay buffering, and
the caller's `wait_for(reply, 10)` / `finally: wait_for(cancel, 1)` path.
The old `server.py`, Spark implementation, audio fixtures, safety filter and their
test suite were not recovered completely. The tests here are newly written from
those contracts, not recovered acceptance receipts. The provided file named
`brain_zeroclaw.final.py` also contains appended Markdown; it is not a final
executable source artifact.

Upstream comparison used local `upstream/master` at
`1c1cd118ac536a1dd1bc79b9cce69cbdc04e4f29`: its tree has no Yonbo Python
bridge equivalent. A live upstream refresh was unavailable. This adapts local S3
prior art, not an upstream PR. The gateway equivalents already live in
`crates/zeroclaw-gateway/src/ws/turn_control.rs` and its restricted device route;
no gateway changes or new dependencies are needed.

## Run

Run from the repository root. First restore the owner-local HTTP/TTS host and
make its package directory available on `PYTHONPATH`; do not point it at an
untrusted package. The following commands describe launch configuration only:

```bash
python3 -m bridges.yonbo.adapter --brain echo
python3 -m bridges.yonbo.adapter --brain fixture --wire-profile capture --tts mock
python3 -m bridges.yonbo.adapter --brain zeroclaw \
  --gateway-agent device-agent \
  --bridge-credential "$HOME/.config/yonbo/bridge-token"
python3 -m bridges.yonbo.adapter --brain spark \
  --spark-secrets "$HOME/.config/yonbo/spark-key"
```

The listener defaults to `127.0.0.1:8765`. Gateway defaults to
`ws://127.0.0.1:42617/ws/chat`; override with `--gateway-endpoint` or
`YONBO_GATEWAY_ENDPOINT`. Only numeric loopback gateway addresses are accepted.
Spark defaults to `http://127.0.0.1:8000`; override with `--spark-endpoint` or
`YONBO_SPARK_ENDPOINT`. Switching to Spark selects the existing host's Spark
brain, never an automatic fallback after a failed gateway turn. No endpoint is
contacted by importing modules or asking for `--help`.

All credentials stay **outside repositories**, in owner-owned regular files with
mode `0600`, no symlinks or hardlinks. `--bridge-credential` is a dedicated token,
not a vendor token, Spark key or owner pairing token. Spark keys are plain text;
`--secrets` points to the host's vendor JSON file. No secret paths are implicitly
loaded and no credential files or ignore rules are supplied. Configure the
gateway's dedicated device bridge with `chat` and `cancel`, the explicit agent,
and the allowed session scope; see the
[restricted device contract](../../docs/book/src/gateway/api.md#restricted-device-bridges).
The bridge cannot prove token provenance from its bytes; provisioning must never
reuse credentials. The gateway agent's risk profile must also enforce text-only
operation. Bridge authentication alone does not disable agent tools.

Gateway session IDs retain S3's mapping:
`s3-` plus the SHA-256 of compact JSON `[device_id, session_id]` (ASCII escaping).
Request UUIDs bind that identity and generation within a process namespace.
The HTTP host owns device sessions/generations and must keep session IDs unique
across devices because its cancellation seam has no device parameter. It must
apply its existing final safety filter to returned text, and call `cancel` in
its unsuccessful reply cleanup. The bridge is not a replacement for that filter.

## Bounds and recovery

- The bridge owns the 10-second reply budget, including handshake and at most
  two reconnects. Its dedicated `_reply_io` task is the only reply task it cancels;
  it never stores the HTTP handler task as cancellation authority.
- Concurrent cancel callers share one shielded cleanup task, bounded to 0.8
  seconds. Cancellation resumes the exact request to recover a lost ACK, then
  sends its bound turn ID. An ACK proves a request to cancel, not rollback.
- Once a message write is attempted, reconnect sends only `resume`. Unknown,
  gap, incomplete stream or changed epoch fails the old turn without replaying
  effects. A new generation may connect after a gateway restart.
- Each request has its own socket. Other request IDs are ignored, replay overlaps
  are deduplicated, and live events arriving before replay are bounded/buffered.
  Reply text is released only after contiguous `done` and a successful terminal.
- Limits: 16 pending turns, 4096 seen session/generation pairs per process,
  16 KiB input/reply text, 128 KiB wire messages. The seen set is not evicted to
  bypass unknown outcomes. Failed submitted turns retain only their cancellation
  binding until the host calls `cancel`; sockets close immediately.
- State is process-local, not a durable queue. Restarting this bridge cannot
  recover its old pending map; never use a restart to retry uncertain effects.

Deliberate changes from v1: split the wire codec for direct testing; incorporate
all recovered round-2 fixes; close failed sockets promptly; validate local secret
locations and hardlinks; bound identity strings; handle all RFC frame length
forms and reject malformed UTF-8/noncanonical lengths. No vendor wire behavior
or Spark implementation has been rewritten.

## Validate locally

```bash
PYTHONPYCACHEPREFIX=/tmp/yonbo-pycache python3 -m py_compile bridges/yonbo/*.py
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest bridges.yonbo.test_bridge -v
python3 -B -m bridges.yonbo.adapter --help
```

Tests use synthetic credentials in temporary directories and in-memory wire
peers. They cover masking, fragmentation, handshake bytes, credential rejection,
lost-ACK recovery, stale events, epoch changes, the measured real 10-second
budget, and Python 3.12 handler cancellation ownership. They contact no service.

The live six-scenario acceptance (normal / bad credential / timeout / cancel /
stale-generation late response / gateway restart), plus lost-ACK and reconnect
observation, must be rerun later through the restored HTTP host and real robot
once #626 connectivity permits. This port does not claim that acceptance,
independent ZCode review, deployment, or restored robot dialogue. Python is not
currently gated by repository CI; run these commands before review.
