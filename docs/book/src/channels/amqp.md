# AMQP

The `amqp` channel consumes messages from an AMQP 0-9-1 broker (RabbitMQ and compatible). Each delivery drives the agent loop. The former `dispatch` field, which could also start a SOP run, was removed with the run side: it is a parse-time tombstone, so any value fails config parse with a migration message.

> **Formerly a SOP event source.** For trigger syntax, routing-key matching, and the historical SOP side of the wiring, see [SOP Fan-In: AMQP](../sop/fan-in/amqp.md). This page covers the broker connection; the dispatch mode was removed with the run side.

## Configuration

The full field list, derived from the live schema. For a basic consumer you set `amqp_url`, `exchange`, and `routing_keys`.

{{#config-fields channels.amqp}}

Full field reference: [config reference](../reference/config.md#channels).

## Dispatch modes

The retired `dispatch` field (removed with the run side) formerly decided what a delivery does:

- `agent_loop`: RETIRED with the run side. This value handed the delivery to the agent loop as a message; the `dispatch` key as a whole is now a parse-time tombstone, so any value (including this one) fails config parse with the migration message. Deliveries always drive the agent loop.
- `sop`: RETIRED with the run side. This value used to lift the delivery into a SOP event (routing key into the event topic, body into the payload) and dispatch it to the SOP engine; it now fails config parse with a migration message. See [SOP Fan-In: AMQP](../sop/fan-in/amqp.md) for the historical wiring.
- `sop_and_agent_loop`: RETIRED with the run side. This value used to run both of the above for each delivery; it now fails config parse with a migration message.

## TLS

For TLS transport, point `amqp_url` at an `amqps://` endpoint and supply `ca_cert`. For mutual TLS, also set `client_cert` and `client_key`. Without these, the connection is plaintext; do not expose a plaintext consumer across an untrusted network.

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| No messages consumed | exchange or routing keys do not match the publisher | Verify `exchange` and `routing_keys` against what the publisher emits |
| TLS handshake fails | `amqps://` without `ca_cert`, or a cert and key mismatch | Supply `ca_cert`; for mTLS verify `client_cert` and `client_key` pair |

## See also

- [SOP Fan-In: AMQP](../sop/fan-in/amqp.md): trigger syntax and routing-key matching
- [MQTT](./mqtt.md)
- [Channels overview](./overview.md)
