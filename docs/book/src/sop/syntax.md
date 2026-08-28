# SOP Syntax Reference

SOP definitions are loaded from subdirectories under `sops_dir`. When `sops_dir` is omitted from config, CLI commands fall back to `<workspace>/sops` for offline inspection, but runtime SOP execution is disabled.

## 1. Directory Layout

```text
<workspace>/sops/
  deploy-prod/
    SOP.toml
    SOP.md
```

Each SOP must have `SOP.toml`. `SOP.md` is optional, but runs with no parsed steps will fail validation.

## 2. Authoring Boundary

The file-backed representation still contains a manifest file plus `SOP.md`.
This page intentionally does not enumerate manifest fields or provide
hand-authored manifest examples.

Use this page for the syntax that remains visible when reviewing, validating, or
debugging SOPs: `SOP.md` step bullets, trigger field summaries generated from
the runtime schema, and `condition` expressions. Before running a generated or
checked-in SOP, validate it with `zeroclaw sop validate <name>`.

`SOP.toml` carries the SOP's identity (`name`, `description`, `version`), its
`triggers`, and its execution knobs. The concurrency-admission fields govern what
happens when a trigger arrives while this SOP's execution slots are full:

| Field | Default | Effect |
|---|---:|---|
| `max_concurrent` | `1` | Maximum runs of this SOP *executing* at once. A run parked at a HITL approval or a deterministic checkpoint releases its slot, so it does not count against this. |
| `admission_policy` | `parallel` | How a trigger that cannot admit right now is handled (see below). |
| `max_pending_approvals` | `0` (unlimited) | Upper bound on runs of this SOP parked at a HITL approval simultaneously. Past the bound, further triggers are deferred (backpressure), never silently dropped (except under `drop`). |

`admission_policy` values (`SopAdmissionPolicy`, snake_case):

- `parallel` (default) - admit up to `max_concurrent`; a trigger that cannot admit
  now is **deferred** (surfaced for backpressure/redelivery on the trigger's
  transport), never silently dropped. Best for independent work (e.g.
  PR-approval SOPs).
- `hold` - serialize: admit only when no run of this SOP is active or parked;
  other triggers are deferred. For pipelines whose pre-approval steps must not
  overlap.
- `coalesce` - collapse a concurrent trigger onto the already-in-flight run (the
  in-flight run's latest state already covers it).
- `drop` - legacy fire-and-forget: a trigger that cannot admit is dropped.
  Explicit opt-in only; never the default.

A deferred trigger's recovery is transport-dependent - there is no in-engine
durable pending-trigger queue in this version (that is a separate follow-up):

- **AMQP** (`durable_ack = true`, SOP-only dispatch): the delivery is nacked
  (`requeue = true`) so the broker retries it once there is room.
- **AMQP combined `sop_and_agent_loop`**: the agent side already consumed the
  delivery, so a backpressured SOP overflow is logged loudly and ACKed (not
  redelivered), to avoid double-running the agent side.
- **MQTT / cron / filesystem / channel-router** (and any other headless source
  that only logs its dispatch results): no per-message redelivery, so a
  deferred trigger is dropped after a loud log (the next
  scheduled/published/observed trigger is the only recovery).

```toml
[sop]
name = "deploy-prod"
description = "Production deploy with approval"
version = "1.0.0"
max_concurrent = 1
admission_policy = "hold"
max_pending_approvals = 8

[[triggers]]
type = "manual"
```

Approval broker groups and policies were removed with the SOP run side. The
`[sop.approval]` section of the main config is a retired retention knob: it
still parses so existing installs boot, but its values are no longer read, and
approval authority lives Tachi-side with ProcedureRun gates through the
procedure_v1 seam. The paired-token subject material that used to feed broker
group membership is unchanged gateway knowledge; it no longer has a
SOP-consumption path in ZeroClaw.

## 3. `SOP.md` Step Format

Steps are parsed from the `## Steps` section.

```md
## Steps

1. **Preflight** — Check service health and release window.
   - tools: http_request

2. **Deploy** — Run deployment command.
   - tools: shell
   - requires_confirmation: true
   - policy: prod
   - input: {"type":"object","required":["version"],"properties":{"version":{"type":"string"}}}
   - output: {"type":"object","required":["digest"],"properties":{"digest":{"type":"string"}}}
   - next: 3
```

Routing and approval bullets can be combined in the same `SOP.md` steps:

```md
## Steps

1. **Classify event** — Inspect the incoming payload.
   - output: {"type":"object","required":["severity"],"properties":{"severity":{"type":"string"}}}
   - when: $.steps.1.severity == "critical"
   - next: 2

2. **Prepare summary** — Build the operator-facing remediation plan.
   - depends_on: 1
   - on_failure: retry:2
   - next: 3

3. **Approval gate** — Require explicit approval before changing state.
   - kind: checkpoint
   - requires_confirmation: true
   - next: 4

4. **Apply remediation** — Execute the approved action.
   - tools: shell
   - allow-tools: shell
   - on_failure: goto:5

5. **Notify operator** — Send a failure notice for follow-up.
   - tools: http_request
```

Parser behavior:

- Numbered items (`1.`, `2.`, ...) define step order.
- Leading bold text (`**Title**`) becomes step title.
- `- tools:` maps to `suggested_tools`.
- `- requires_confirmation: true` enforces approval for that step.
- `- kind:` accepts `execute` (default) or `checkpoint`. A checkpoint step
  pauses deterministic execution at that step. Use `requires_confirmation: true`
  when a step must require approval in any execution mode.
- `- allow-tools:` and `- deny-tools:` define an explicit per-step tool scope.
- `- input:` and `- output:` attach JSON Schema-like step boundary contracts.
- `- when:` is a routing guard evaluated against accumulated completed-step
  outputs after the current step finishes. When it does not match, the run
  completes instead of dispatching another step.
- `- next:` and `- depends_on:` route non-linear runs. Ineligible routed steps
  are marked `skipped` and leave the run `pending` instead of dispatching.
- `- when:` guards an explicit `- next:` jump; when the condition is false, the
  run advances to the next linear step (`current_step + 1`) instead of completing.
- `- on_failure:` accepts `fail`, `retry:<count>`, or `goto:<step>` and is
  enforced for reported step failures and output schema failures.
- `- mode:` overrides the SOP execution mode for that step.
- `- policy:` names an approval-broker policy (a key in `[sop.approval].policies`).
  Retired with the run side: the bullet is still parsed and round-tripped as
  `SOP.md` format, but no broker consumes it, so it no longer gates anything.
  Approval authority lives Tachi-side through the procedure_v1 seam.

### Retired approval-broker route delivery

Removed with the run side: the `[sop.approval]` policies, their
`request_route`/`escalation_route` out-of-band delivery, and the broker that
consumed them no longer exist in ZeroClaw. Gate/resume authority lives
Tachi-side with ProcedureRuns through the procedure_v1 seam.

### Deterministic checkpoints: approval and resume

Retired with the run side: nothing executes SOP steps in ZeroClaw any more, so
`kind: checkpoint` gates, their approve/deny/edit/revise resolutions, and the
approval ledger that recorded them no longer exist here. The bullet is still
parsed and round-tripped as `SOP.md` format. Gate, resume, and review-draft
semantics live Tachi-side with ProcedureRuns through the procedure_v1 seam.

### Injected-adapter capabilities

Retired with the run side: the `llm.generate` and `forge.comment` capability
adapters (and the capability registry that injected them) were removed.
`kind: capability` / `capability:` bullets are still parsed and round-tripped
as `SOP.md` format, but no engine executes them. The headless review-pipeline
pattern they enabled now belongs to Tachi-side ProcedureRuns through the
procedure_v1 seam.

### Step Contract Enforcement

Step contracts are optional. When present, `input` and `output` accept a compact
JSON object with `type`, `required`, `properties`, and `items` fields. The
supported primitive types are `object`, `array`, `string`, `number`, `integer`,
`boolean`, and `null`.

The `[sop]` config controls enforcement:

| Field | Default | Effect |
|---|---:|---|
| `step_schema_enforce` | `true` | Retired with the run side: no engine executes steps, so step schema enforcement no longer applies. The key still parses but is no longer read. |
| `step_scope_enforce` | `false` | Retired with the run side: no live step turn exists to scope, so per-step tool scopes are format-only. The key still parses but is no longer read. |
| `step_mandatory_tools` | - | Retired with the run side: the lifecycle tools it listed were removed, so this key no longer has an effect. |
| `max_step_visits` | `256` | Retired with the run side: no routed runs exist to bound. The key still parses but is no longer read. |
| `max_step_retries` | `2` | Retired with the run side: no step failure policy executes. The key still parses but is no longer read. |
| `untrusted_payload_max_bytes` | `8192` | Cap untrusted trigger topic/payload text at a UTF-8 character boundary; `0` disables the cap. |
| `untrusted_input_guard` | `"warn"` | Prompt-guard action for untrusted trigger input: `warn`, `block`, or `sanitize`. |
| `untrusted_guard_sensitivity` | `0.7` | Sensitivity used by prompt-guard screening and outbound redaction. |
| `untrusted_frame_warning` | `true` | Include explanatory warning text in the untrusted-content frame. Frame boundaries remain enabled. |
| `untrusted_outbound_redact` | `true` | Enable shared outbound redaction for SOP content-safety consumers. |
| `procedural_memory_enabled` | - | Retired with the run side: the `sop_workshop` proposal pipeline it gated was removed, so this key no longer has an effect. |

Step-engine enforcement was removed with the run side: schema enforcement,
routing enforcement, and tool-scope enforcement described here applied to
engine-driven steps that no longer exist. The `[sop]` keys in the table above
still parse but are no longer read.

The untrusted-content screening keys (`untrusted_*`) are retained with the
surviving `security/external_content` screening surface, which currently has no
live SOP trigger feeder in ZeroClaw after the run-side removal.

Procedural memory was removed with the run side: `sop_workshop`, proposal
capture, and proposal write-back no longer exist. Proposal-style learning
belongs to the Tachi-side procedure_v1 seam.

### Run Durability

Removed with the run side: `persist_runs`, `run_store_backend`, and
`run_state_dir` no longer configure anything, and the runs.db store they
described was demolished. Existing `sop/runs.db` files on disk are left in
place untouched.

## 4. Trigger Types

{{#sop-trigger-index}}

For the live-versus-unwired status of each source and the transport details, see [SOP Fan-In](./fan-in/overview.md).

## 5. Condition Syntax

Trigger `condition` fields and step `when:` guards use the same expression
grammar. Trigger conditions evaluate against the event payload. Step `when:`
guards evaluate against accumulated completed-step outputs in this shape:

```json
{
  "steps": {
    "1": {
      "severity": "critical"
    }
  }
}
```

Evaluation is fail-closed for invalid conditions, missing payloads, unresolved
JSON paths, and direct numeric comparisons whose payload or comparand is not a
number. An empty condition matches unconditionally.

### JSON Path Form

A condition beginning with `$` compares a value inside a JSON payload:
`$.path.to.field <op> <value>`.

| Expression | Payload | Matches |
|---|---|---|
| `$.value > 85` | `{"value":90}` | yes |
| `$.value >= 85` | `{"value":85}` | yes |
| `$.temp < 25` | `{"temp":20}` | yes |
| `$.temp <= 25` | `{"temp":25}` | yes |
| `$.status == "critical"` | `{"status":"critical"}` | yes |
| `$.status != "error"` | `{"status":"ok"}` | yes |
| `$.count == 42` | `{"count":42}` | yes |
| `$.data.sensor.value > 85` | `{"data":{"sensor":{"value":87.3}}}` | yes |
| `$.readings.1 == 20` | `{"readings":[10,20,30]}` | yes |
| `$.active == "true"` | `{"active":true}` | yes |
| `$.nonexistent > 0` | `{"value":90}` | no |

Path rules:

- Use dot-separated segments. Array elements use a numeric segment such as
  `$.readings.1`; bracket syntax is not supported.
- Missing keys, out-of-range array indexes, invalid JSON, and empty payloads
  fail closed.
- There are no wildcards, filters, recursive descent, or built-in variables.

### Direct Numeric Form

A condition with no leading `$` compares the whole payload as a number. This is
useful for scalar event payloads.

| Expression | Payload | Matches |
|---|---|---|
| `> 0` | `1` | yes |
| `> 0` | `0` | no |
| `>= 5` | `6` | yes |
| `< 100` | `50` | yes |
| `== 42` | `42` | yes |
| `!= 0` | `1` | yes |
| `> 3.14` | `3.15` | yes |
| `> 0` | `not a number` | no |

### Operators

A comparison uses one operator, matched longest-first: `>=`, `<=`, `!=`, `==`,
`>`, `<`. JSON-path comparisons try numeric comparison first. If both sides
parse as numbers, they compare numerically; otherwise values compare as strings.
Surrounding double quotes on the comparand are stripped, so quote string
literals: `$.status == "critical"`. Direct numeric conditions are numeric-only:
if either side does not parse as a number, there is no match.

The condition evaluator converts JSON booleans to the strings `true` and
`false`, so compare them as quoted strings, for example `$.active == "true"`.

A condition is a single comparison. Logical combinators such as `AND`, `OR`,
and `NOT` are not supported.

## 6. Validation

Use:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw sop validate
zeroclaw sop validate <name>
```

</div>

Validation warns on empty names/descriptions, missing triggers, missing steps, and step numbering gaps.
