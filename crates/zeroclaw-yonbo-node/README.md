# Yonbo thin Node host

`zeroclaw-yonbo-node` is an optional, standalone device process. It owns a small
set of authenticated device-maintenance capabilities without linking the
controller, a model provider, an async runtime, or external Rust dependencies.
The binary is `yonbo-node`. Linux is the device platform; macOS supports local
process tests. Other platforms return `ERR unsupported_platform`.

This is a bounded source import of the independently exercised device prototype,
not an implementation of Gateway `zeroclaw.nodes.v2`. It does not enroll a
Gateway DeviceIdentity, grant controller tools access, or satisfy the complete
invocation lifecycle in tracker #61. Existing Gateway identity, capability
ceilings, approvals and routing remain unchanged. See the
[protocol and trust boundary](PROTOCOL.md) and the
[upstream reuse decision](../../docs/book/src/contributing/fork-notes.md#yonbo-device-host-import).

## Start closed, enable explicitly

No arguments start no service. `--serve` requires `--data-dir` and `--listen`.
`--data-dir` must be an absolute path to a private directory owned by the current
OS account, mode 0700. Its parent must already exist. No device address, home
directory, or deployment path is compiled in.

A local example, using synthetic operator state:

```sh
umask 077
node_state="$(mktemp -d)"
node_host="$(mktemp -d)"
scripts/dev/cargo-local.sh run -p zeroclaw-yonbo-node -- \
  --data-dir "$node_state" --pair operator > "$node_host/credential.json"
scripts/dev/cargo-local.sh run -p zeroclaw-yonbo-node -- \
  --data-dir "$node_state" --listen 127.0.0.1:18767 --serve
```

In another terminal, use the same private credential file:

```sh
python3 crates/zeroclaw-yonbo-node/tools/client.py \
  --address 127.0.0.1 --port 18767 \
  --credentials "$node_host/credential.json" info
```

The shell variable from the first terminal is not exported to a different
terminal. Set `node_host` there to the private directory you just created; do not
paste its credential contents. For a real separate device, run pairing on its
console or through an already verified SSH host identity, capture stdout directly
to a mode-0600 host file, and never log it. Do not disable SSH host-key checks.
The client does not provision SSH or discover devices.

`--pair operator` generates a fresh 32-byte PSK and outputs secret JSON. Repeating
it rotates that host's key and closes its existing authenticated sessions. There
is no default key or committed example secret. `--revoke operator` persists a
revoked flag and closes existing sessions before acknowledging success. These
are local administrative commands, never LAN capability verbs.

## Configuration

Options precede the mode. Duplicate, unknown, partial or irrelevant options
fail with fixed diagnostic codes before startup side effects.

| Option or mode | Meaning |
| --- | --- |
| `--data-dir /absolute/private/directory` | Device-local store and runtime owner; required except for `--info` |
| `--listen IP:PORT` | Explicit TCP binding; required only for `--serve`; port 0 selects an ephemeral port recorded in runtime.json |
| `--dbus-send /absolute/executable` | Trusted local display sender; requires `--dbus-address` |
| `--dbus-address unix:path=/absolute/socket` | Local Unix bus address; requires `--dbus-send`; no remote-bus fallback |
| `--owner-enabled /absolute/marker` | Optional owner permission marker; requires `--owner-cancel` |
| `--owner-cancel /absolute/marker` | Optional latched cancellation marker; presence or unreadability refuses work |
| `--serve` | Foreground service; no automatic restart or installer |
| `--pair HOST` | Generate or rotate a host PSK; secret stdout |
| `--revoke HOST` | Revoke a paired host and close authenticated sessions |
| `--info` | Machine-readable platform/version/boot-epoch diagnostic; no listener |

Display control is absent from the advertised capability list unless both
D-Bus options are provided by the local operator. Authentication alone does not
enable it. Enabled display commands use a fixed D-Bus interface/member and
`string, int32` signature, a fixed play count of one, and a finite face-name
allowlist. No shell parses network input. Motion, audio, microphone, arbitrary
files and generic command execution are unavailable.

Optional owner markers import only the applicable cancellation contract: a missing
or non-regular enabled marker, or a present/unreadable cancel marker, prevents
new dispatch and makes the process exit. Already dispatched display work may
finish. Deployment-specific S99, watchdog, power, boot-attempt and vendor-process
scripts are not shipped or executed by this crate.

## State ownership and recovery

The crate owns `keys.json` (PSKs, revoked flags, sequence high-water marks),
`.node.lock`, `admin.sock` and `runtime.json` inside the selected private directory.
The host client owns its private credential and lock files. None is controller
memory, a task ledger, or the Gateway DeviceIdentity store. Do not edit or restore
old copies of live keys.json: rolling its sequence state back loses replay history.

The daemon holds a process lock. Local pair/revoke requests are serialized through
its mode-0600 admin socket while it runs; offline management takes that same lock.
Commands reserve and fsync sequence state before dispatch. A crash may consume a
sequence without executing its action; neither peer automatically retries it.
Reconnect uses a new challenge and a signed sequence floor. Long-term PSKs survive
boots, but sessions bind the boot epoch and a fresh process instance.

A private `STOP epoch instance pid` request makes only that matching instance
exit; its acknowledgment precedes teardown. Supervisors must verify exit and must
not use a stale numeric PID as signal authority. Original parents may stop and
reap their own retained child. Process tests cover the instance binding, marker
exit, revocation, restart and stale-session paths. No supervisor is installed here.

## Verification and rollout

```sh
scripts/dev/cargo-local.sh check -p zeroclaw-yonbo-node
scripts/dev/cargo-local.sh clippy -p zeroclaw-yonbo-node --all-targets -- -D warnings
scripts/dev/cargo-local.sh test -p zeroclaw-yonbo-node -- --nocapture
```

The system test launches the real binary with temporary mode-0700 state and
loopback TCP/Unix sockets. A Python standard-library HMAC client is an independent
protocol oracle. A temporary display sender records fixed arguments instead of
touching a robot or a real bus. Python 3 is required; missing Python fails the test.
Owner-specific marker and instance tests replace the applicable prototype owner
fixtures at this process boundary; unrelated boot/power scripts are not imported.

This source port has host-side acceptance only. It does not deploy to a robot or
inherit the old prototype's binary hash, cross-build or reboot receipt. A later
device build must separately check architecture, dynamic interpreter, NEEDED
libraries and the GLIBC ceiling, use a generic CPU target, and preserve rescue
access. Cross-build/deployment recipes remain outside this leaf.

Rollback: stop only the optional process you own and retain its private store.
No controller setting or device boot file needs reverting because this import
changes neither. Reverting the workspace membership and crate removes the code
without deleting private state. A `READY` line is startup evidence, not ongoing
health; a display `sent` result is not a presentation acknowledgment.
