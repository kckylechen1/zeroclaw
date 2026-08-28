# Standard Operating Procedures (SOP)

SOP definitions are deterministic procedures: explicit trigger matching,
step contracts, and condition syntax, validated and stored in ZeroClaw. The
run side was removed: SOP runs are Tachi-side ProcedureRuns through the
procedure_v1 seam, so the former `SopEngine` runtime (approval gates, run
state, dispatch) no longer exists in ZeroClaw.

- [How SOPs run](./how-it-works.md): the pre-removal runtime contract, event flow, and getting-started walkthrough, kept as a historical reference.
- [Syntax](./syntax.md): required file layout and trigger/step syntax (the definition format that remains).
- [Cookbook](./cookbook.md): reusable SOP patterns.
- [SOP Fan-In](./fan-in/overview.md): event fan-in formats for SOP triggers. MQTT, filesystem, and AMQP were wired live sources and the daemon's periodic maintenance tick dispatched cron triggers before the run side was removed; webhook, peripheral, and calendar triggers were defined and matched but never routed to a live event source.
- [Observability](./observability.md): where run state and audit entries were stored before the run-side removal (historical reference).
- [Worked Example](./example.md): the stagehand StageX auto-update bot walkthrough, from build to draft PR, driven by a deterministic SOP over an AMQP feed (pre-removal; kept as a worked example of the format).
