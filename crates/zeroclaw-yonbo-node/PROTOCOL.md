# N3B1 device-maintenance protocol

This protocol is retained from the bounded Yonbo device prototype. It is not the
Gateway Node v2 wire format, DeviceIdentity pairing or a controller approval
mechanism. A future adapter must reconcile those contracts through tracker #61;
PSK possession must never substitute for the Gateway's capability-policy
intersection or high-risk grants.

## Threat model

An untrusted network peer may connect, replay, modify or stall frames. A revoked
host may retain its old PSK. Device root and the host OS account holding credentials
are trusted; compromise of either is outside this protocol. Pairing needs an
already authenticated private console/SSH channel. HMAC authenticates messages
but does not encrypt them. There is no confidentiality or DoS-availability claim.
No new cryptographic construction is introduced by the source port; the existing
SHA-256/HMAC implementation and RFC 4231 known-answer tests are retained. They do
not establish certification, compiler-level constant time or memory zeroization.

## Frames and MACs

Frames are ASCII plus LF, at most 512 bytes including LF, with single spaces and
no NUL. There is no pipelining or offline command queue. Each connection holds at
most one bounded input/output frame. At most four clients are admitted; auth has
a three-second deadline, each command turn ten seconds, and the session at most
300 seconds. Slow clients close; they do not gain an unbounded queue.

```text
Node -> host: N3B1 epoch instance servernonce
Host -> node: AUTH host clientnonce proof
Node -> host: READY last_seq turnnonce proof
Host -> node: CMD seq verb arg proof
Node -> host: RESULT seq OK|ERR payload nextturn proof
```

`epoch` is SHA256 of the trimmed operating-system boot ID. `instance` is fresh
for each process; servernonce, clientnonce and each turnnonce are fresh 32-byte
values represented as 64 lowercase hex characters. Host IDs use 1-32 lowercase
ASCII letters, digits, underscores or hyphens. PSKs contain 32 random bytes.

Each MAC field is encoded as ASCII followed by NUL. Define session fields as
`epoch, instance, servernonce, host, clientnonce`, in that order.

| Message | HMAC-SHA256 input fields |
| --- | --- |
| AUTH | `node3b/auth/v1`, session fields |
| READY | `node3b/ready/v1`, session fields, last_seq, turnnonce |
| CMD | `node3b/cmd/v1`, session fields, turnnonce, seq, verb, arg |
| RESULT | `node3b/result/v1`, session fields, old turnnonce, seq, status, payload, nextturn |

The domain strings stay byte-compatible with the prototype. The node verifies
host/key/revocation, current turn MAC, deadline and strict monotonic sequence
before reserving the sequence and dispatching. Sequence values are canonical
unsigned decimal u64; command sequence zero, leading zeros, duplicates and
out-of-order values are rejected. The client verifies both READY and RESULT
proofs. A new connection never inherits an old pending command.

## Capabilities and lifecycle

`info -` returns a bounded machine diagnostic. `capabilities -` returns only
currently enabled capabilities. `face NAME` is unavailable until explicitly
configured by the local operator. Accepted names are `Happy`/`KaiXin`,
`GetAngry`/`ShengQi`, `Worry`/`DanYou`, `Default`, `Listen`, `ThinkDeep`, `Sad`,
and `Like`. Alias mapping cannot supply a path or executable. Play count is one.
The sender has a two-second deadline plus bounded cleanup of its own child.
`sent:NAME` reports D-Bus submission, not completion or visual proof.

Every accepted sequence is atomically committed and synced before execution.
Storage failure closes the daemon rather than continuing with an uncertain
sequence floor. A crash between reservation and execution may lose an action.
An unknown side effect is never automatically replayed. Per-command sync is
appropriate for low-frequency control, not an unreviewed high-rate motion stream.
Normal process-restart tests do not prove physical power-loss durability.

Pair/revoke are local CLI operations routed through the private admin socket
when the service runs. Revoke persists first, closes all matching authenticated
connections, then acknowledges. An in-progress serialized action may finish
before revoke is processed; after acknowledgment no old-key dispatch is admitted.
Pairing an existing host rotates its key, resets the new key's sequence and closes
old sessions. Management is not available as a network capability.

Private admin PING returns `PONG epoch instance pid`. Private
`STOP epoch instance pid` only accepts the exact current instance, replies
`STOPPING`, then exits before further network dispatch. The caller must verify
exit separately. This is not a numeric-PID signaling API. Cancellation markers
stop new dispatch but do not roll back a display event already sent to the vendor.

## Private files

The selected data directory is mode 0700 and belongs to the process account;
regular state files and the admin socket are 0600. Symlink/file-identity and
canonical store checks fail closed. The maximum store is 16 KiB and 32 hosts,
including revoked records. The binary serializes state changes with a process
lock; external edits to a live keys.json are unsupported. State is written through
create-new temporary files, file sync, rename and directory sync.

Generated credentials are never committed. The host credential schema is
`version`, `host`, `key`, `last_seq`; the key is generated during pairing, not
filled from a sample. Keep raw credentials and authentication transcripts out of
logs, issue bodies and PRs. Only operator-approved private backups may contain
state, and restoring an older sequence high-water mark requires explicit recovery
analysis rather than an automatic startup fallback.
