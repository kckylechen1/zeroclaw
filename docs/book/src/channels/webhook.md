# Webhooks

The `webhook` channel is a generic inbound/outbound HTTP adapter. It runs its own embedded HTTP server on a port you choose, accepts JSON-shaped messages, hands them to the agent, and (optionally) POSTs the agent's replies to a URL you specify. Use it as the universal adapter for any system that can produce an HTTP POST.

> **Not the same as the gateway's `/webhook` endpoint.** The gateway service has its own `POST /webhook` for paired clients hitting the agent over HTTP, that lives under `[gateway]` and is described in [Operations → Network deployment](../ops/network-deployment.md). This page documents the `[channels.webhook]` channel only.

## Configuration

{{#config-fields channels.webhook}}

Full field reference: [config reference](../reference/config.md#channels).

## Inbound

The channel binds `0.0.0.0:{port}` and routes `POST {listen_path}`.

Request body (JSON):

```json
{
  "sender": "alice",
  "content": "Hello, agent.",
  "thread_id": "optional-conversation-id"
}
```

- `sender`: required, used as the message's sender identity.
- `content`: required, the user message handed to the agent. Empty content returns `400`.
- `thread_id`: optional. If set, the agent's reply targets the same thread; otherwise replies target `sender`.

Success returns `200 OK`. Malformed JSON or empty `content` returns `400`. Backpressure (channel queue full) returns `503`.

## Signature verification and replay protection

The listener requires a configured `secret` and refuses to start without one.
By default, every inbound request must have exactly one of each header:

```text
X-Webhook-Timestamp: <Unix time in whole seconds>
X-Webhook-Id: <unique event ID or nonce>
X-Webhook-Signature: sha256=<hex-encoded HMAC-SHA256>
```

The event ID must be 1 to 128 printable ASCII characters without spaces. Generate
it once per event, for example a UUID, and reuse it when retrying that event.
The timestamp is decimal Unix seconds. The default acceptance window is
inclusive, within 300 seconds before or after the receiver's clock.

Sign these exact bytes (the separators are single LF bytes, not literal `\n`):

```text
v1\n<timestamp header>\n<event ID header>\n<raw request body>
```

Compute `HMAC-SHA256(secret, signing_bytes)`. Hex encoding is case-insensitive;
the `sha256=` prefix is optional. Send exactly the body bytes that were signed.
Changing the timestamp, ID, or body invalidates the signature.

Example producer code (Python standard library):

```python
import hashlib
import hmac
import json
import time
import uuid

body = json.dumps({"sender": "alice", "content": "Hello, agent."}).encode()
timestamp = str(int(time.time()))
event_id = str(uuid.uuid4())
signing_bytes = b"v1\n" + timestamp.encode() + b"\n" + event_id.encode() + b"\n" + body
signature = hmac.new(secret.encode(), signing_bytes, hashlib.sha256).hexdigest()
headers = {
    "Content-Type": "application/json",
    "X-Webhook-Timestamp": timestamp,
    "X-Webhook-Id": event_id,
    "X-Webhook-Signature": "sha256=" + signature,
}
# POST body unchanged using these headers; secret comes from your secret manager.
```

### Configuration and migration

```toml
[channels.webhook.my_hook]
enabled = true
port = 8090
# Supply secret through your existing secret configuration.
replay_window_secs = 300
replay_cache_capacity = 4096
dangerously_disable_replay_protection = false
```

Both limits are optional. `replay_window_secs` accepts 1 through 86400 seconds;
`replay_cache_capacity` accepts 1 through 65536 entries. Zero or out-of-range
values refuse startup, including when the dangerous switch is enabled.

**Existing body-only producers must migrate.** Add the timestamp and event ID
headers and sign the complete v1 envelope above. Missing timestamp or ID returns
`401` with JSON error `webhook_replay_migration_required`, required header names,
and the signing format. Missing/invalid signatures, malformed headers, and
out-of-window timestamps also return `401`. A repeated accepted event ID returns
`409` and is not delivered again, including simultaneous requests.

For a deliberate temporary legacy migration only, set
`dangerously_disable_replay_protection = true` in that alias's config. This restores
body-only HMAC verification, logs a warning at listener startup, and permits
replays. A valid HMAC and configured secret are still required. Restore `false`
after updating the producer; there is no automatic fallback to the old format.

### Cache scope and delivery limits

The channel instance owns an in-memory bounded LRU of accepted event IDs. A marker
lasts through `signed timestamp + replay_window_secs`, including future-dated
requests. Expired entries can be removed. If the cache is full of live markers,
new events receive `503` (`webhook_replay_cache_full`); live markers are never
evicted to make room. Increase capacity within the supported bound or wait for
expiry. Authentication and payload validation happen before reserving a marker.

The reservation is atomic and occurs before enqueueing the message. A queue
failure or lost HTTP response does not remove it: `409` means this event was
already reserved, not that the agent completed its work. Do not generate a new
ID automatically to retry an uncertain outcome.

State survives listener restarts on the same channel instance, but process
restarts or replacement instances clear it. Multiple processes have independent
caches; use a single receiver for a given producer stream and distinct secrets
per alias. This is bounded process-local replay protection, not durable
exactly-once delivery. Keep the receiver's clock synchronized.

## Outbound

When `send_url` is set, every agent reply is delivered as an HTTP request to that URL:

```
{send_method} {send_url}
Authorization: {auth_header}    # only if auth_header is set
Content-Type: application/json

{
  "content": "agent reply text",
  "thread_id": "optional thread id",
  "recipient": "optional recipient id"
}
```

- `send_method` is `POST` (default) or `PUT`. Any other value falls back to `POST`.
- `auth_header` is sent verbatim as the `Authorization` header value, include the scheme yourself (e.g. `Bearer xyz`, `Basic dXNlcjpwYXNz`).
- `recipient` is omitted when empty.
- Non-2xx responses raise an error in logs; the agent reply is considered failed.

When `send_url` is unset, agent replies are dropped silently (logged at `debug`). This is the right configuration for fire-and-forget inbound flows where the response is delivered through some other channel.

## Public exposure

The channel binds to `0.0.0.0` directly. To expose it on the public internet:

1. **Reverse proxy**: terminate TLS at nginx / Caddy / Traefik and proxy to the channel's port. See [Operations → Network deployment](../ops/network-deployment.md).
2. **Tunnel**: configure `[tunnel]` (`ngrok`, `cloudflare`, or `tailscale`) and the daemon brings up the tunnel alongside the channel.
3. **Local-only**: run inside a private network and have your producer hit the LAN/loopback address directly.

Always pair public exposure with `secret`. An unauthenticated webhook listener is an open ingress to the agent.

## Outbound retries

When `send_url` is set, outbound delivery retries transient failures, network errors, HTTP `429`, and HTTP `5xx`, with exponential backoff (±25% jitter) capped by `retry_max_delay_ms`. Non-`429` `4xx` responses fail immediately without retrying. When the server returns a `Retry-After` header on `429` or `503`, that value is honored and also clamped by `retry_max_delay_ms`. Setting `max_retries = 0` is fire-and-forget.

## See also

- [Operations → Network deployment](../ops/network-deployment.md): TLS termination, tunnels, the gateway's separate `/webhook`
- [Channels → Overview](./overview.md)
