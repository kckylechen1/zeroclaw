# Filesystem

The `filesystem` channel section is RETIRED with the run side. Filesystem
listeners were SOP-trigger-only fan-in: they fed watched-path changes into the
SOP engine as events. SOP runs are Tachi-side ProcedureRuns since #243
(#197 wall 5), and the run-side listener config went with them.

Any `[channels.filesystem]` section, even an empty header, now fails config
parse with the migration message. Remove the section; there is no replacement
`[channels.*]` key for it.

For the historical trigger syntax and path matching that these listeners
drove, see [SOP Fan-In: Filesystem](../sop/fan-in/filesystem.md). That page
describes the retired wiring only.

## See also

- [SOP Fan-In: Filesystem](../sop/fan-in/filesystem.md): historical trigger syntax and path matching
- [Channels overview](./overview.md)
