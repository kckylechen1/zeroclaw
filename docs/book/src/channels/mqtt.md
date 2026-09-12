# MQTT

The `mqtt` channel section is RETIRED with the run side. MQTT listeners were
SOP-trigger-only fan-in: they fed broker messages into the SOP engine as
events. SOP runs are Tachi-side ProcedureRuns since #243 (#197 wall 5), and the
run-side listener config went with them.

Any `[channels.mqtt]` section, even an empty header, now fails config parse
with the migration message. Remove the section; there is no replacement
`[channels.*]` key for it.

For the historical trigger syntax and topic matching that these listeners
drove, see [SOP Fan-In: MQTT](../sop/fan-in/mqtt.md). That page describes the
retired wiring only.

## See also

- [SOP Fan-In: MQTT](../sop/fan-in/mqtt.md): historical trigger syntax and topic matching
- [AMQP](./amqp.md)
- [Channels overview](./overview.md)
