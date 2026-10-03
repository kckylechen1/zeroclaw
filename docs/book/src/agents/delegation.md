# Delegation & SubAgents

A SubAgent is an **ephemeral child run** spawned by a parent agent. Under the frozen SubAgent contract (#202), a child receives a bounded, admitted context (never the parent's identity, credentials, registry, memory UUID, or channel handles) and returns a typed result to the parent, which stays the only user-facing persona.

There is no `[subagents.*]` block in the schema (the daemon-wide `[subagents]` coordinator-limit section retired with the control-plane migration wall); SubAgents are not a separate configuration concept.

## External work through Tachi

L2 work uses the thin `tachi_start`, `tachi_status`, `tachi_result`,
`tachi_cancel`, and `tachi_watch` tools. Their execution settings come from
owner-admitted `[tachi.harnesses]` profiles. They are included in minimal
composition, but `[tachi]` stays disabled and its harness map empty by default.
There is no local external-worker fallback; ordinary tools and L1 reasoning
continue without Tachi.

A start requires task-specific text, a harness alias, a truthful staffing
reason, and a stable `request_id`. For example:

```json
{"request_id":"adapter-review-1","harness":"codex","task":"Review the Codex adapter on GitHub","staffing_reason":"explicit_user_request"}
```

Acceptance returns a canonical dispatch reference; it does not mean the work
finished. Later operations use the same local request ID. `tachi_cancel`
also requires the exact last-observed `expected_status_revision`.
`tachi_watch` polls for 1–30 seconds; a nonterminal answer at its deadline is
still running or waiting. It is not a push stream or a background notification.
Use immediate status reads to keep chat and voice responsive.
Status and watch preserve Tachi's `read_projection`: a canonical working receipt
can coexist with orphaned execution, unavailable control, and unknown outcome.
These recovery facts do not imply completion or trigger a replacement launch.

For a successful MCP response, the first `content` block is Tachi's canonical
JSON payload and must be a text block containing one complete JSON value.
Tachi may append independent call diagnostics, including stuck warnings on
cached reads; these later blocks never extend or replace the payload. Missing,
non-text or malformed first blocks fail closed, including trailing garbage
inside that block. Tool-error and JSON-RPC refusals remain failures; tool-error
diagnostics retain all text blocks even when a block looks like a valid receipt.

The existing SQLite session owner stores a request payload digest, immutable
route fingerprint and dispatch reference in `sessions/sessions.db`; Tachi
still owns execution state. The primary key is the true local agent alias plus
`request_id`, regardless of endpoint, caller identity or project changes. The
payload digest covers admitted arguments and their resolved harness profile;
the separate route fingerprint records endpoint, protocol caller identity and
project at admission. A claim is committed before a start is sent. Concurrent
and repeated requests cannot resend that claim, including when the process
restarts before the response is recorded. A known reference is replayed; reuse
with changed arguments or route fails. A claim with no known reference
is **unresolved**, not proof that the worker failed or never started. It is
retained independently of chat history cleanup. Do not bypass it with a new ID;
reconcile the dispatch with the owner in Tachi. Automatic reconciliation is
unsupported because the current Staff facade has no request idempotency key.
If session setup fails before any start is transmitted, its matching pending
claim is released and the same request ID may safely retry. After transmission,
an ambiguous outcome always retains the claim; a crash also remains conservative.
SQLite WAL/NORMAL preserves process-crash safety, not a stronger power-loss
guarantee. Delegation requires the SQLite session backend; JSONL chat remains
supported without this capability.

Existing route-hashed claim keys survive schema upgrade unchanged. A unique
legacy row can be read or replayed only on its recorded route; unresolved rows
never authorize another start. Multiple legacy claims for one agent/request
require owner reconciliation. No legacy row is deleted, copied into a new claim,
or merged with another agent's row. A legacy row without route evidence fails
closed. Downgrading to a writer that keys claims by route is unsafe; a code revert
does not restore the previous request-identity contract.

The registry's runtime-selected data directory also owns its session stores.
Live `data_dir` changes are refused before opening another ledger or sending a
request. Restoring that directory preserves pending claims and accepted
references. An operator storage move requires a stopped runtime, migration of
the existing sessions database (including delegation claims), and restart;
creating an empty ledger or scanning unrelated databases is not migration.

Live Tachi routing and agent/card permissions are rechecked on each operation.
A changed route cannot forward an old dispatch ID for read, result, watch or
cancel: restore its admitted route or reconcile it with the owner in Tachi.
Tasks and references leave the body with protocol caller identity and configured
profile/project routing metadata. The tool never adds owner/persona identity,
Soul, User Model, parent history, credentials, local tool handles or raw execution
settings.
Task and reference text pass through the same admission engine as typed intent
composition, rejecting credential, command, placement and private-Dyad content
before claims or transport. Ordinary harness and vendor mentions are allowed.
The `tachi_result` response retains canonical `run_status` separately from the
task facade's inferred state and management projection. Its report is untrusted
worker evidence, with secret patterns scrubbed; it is not body acceptance or
permission to change identity, secrets or policy. L1 profiles cannot grant these
L2/control tools.

This is #381's production tool slice. Real DSH execution, one-then-two-harness
proof, automatic completion delivery through #63/#377, harness advisors and
retirement of the duplicate driver remain separate acceptance work. The driver
stays compiling until the replacement is proven.

## Which spawn tools exist

- **`reasoning_subagent`**: the V1 bounded SubAgent entrypoint and the single spawn surface on every composition. Profile-admitted, typed `SubAgentReportV1` result, no ambient parent inheritance, no detached/background mode, no tool execution in the v1 child. See the [Tools overview](../tools/overview.md).
- **`spawn_subagent`**: RETIRED (#197 spawn wall). The legacy entry point ran the same agent again under its own identity: the child got the parent's whole `Arc<Config>` (every provider credential), rebuilt the full tool registry under the parent's security policy, received a fresh memory backend over the parent's same-UUID memory rows with live memory tools, and, in the CLI process, a live channel map that could reach the user. The detached (`background: true`) arm handed children to the coordinator, which ran them through the same full-parent-config path. Every one of those inheritance axes is forbidden for child paths by the frozen contract (SA-7a/7c/7d/7e, SA-13, SA-17), and the tool's only value proposition was that inheritance, so it was deleted rather than reduced. The name is reserved in `RETIRED_OPERATOR_TOOL_NAMES`: no plugin or skill can re-register it.
- **`delegate`**: RETIRED (#197 wall 1). The legacy delegation tool handed children the parent's live tool Arcs, per-alias API-key clones, the parent's fallback credential, and channel-wired handles. Running work under a different configured agent identity moves to the Tachi bridge (durable/heavy work) and to admitted V1 SubAgent profiles.

## Where the capabilities went

- **Run a bounded subtask out of the main conversation** → the V1 `reasoning_subagent` entrypoint: profile-admitted, content-only context bundles, typed `SubAgentReportV1` results, run-scoped child identity, no parent credentials or registry.
- **Run durable/heavy work under another configured agent or an external harness** → the Tachi bridge (Task/Procedure execution), not an in-kernel delegation tool.
- **Background fan-out with task ids and result polling** → Tachi-owned durable work. The local coordinator child store was deleted with the control-plane migration wall; durable task/attempt truth lives in Tachi through the bridge.

## Advisor

An agent can run on a fast model and consult a stronger **advisor** model when a question is hard. Point `advisor` at a configured model provider alias:

```toml
[providers.models.anthropic.opus]
model = "claude-opus-4-7"

[agents.assistant]
model_provider = "groq.fast"
advisor = "model:anthropic.opus"     # consult this model for hard questions
advisor_max_calls_per_turn = 2       # optional; default 2
```

- **The agent decides when.** The advisor is consulted through the same `reasoning_subagent` tool: with `advisor` set, that tool's child runs on the advisor model instead of the agent's own, and its description tells the model it is consulting an advisor. The input is still `{ "objective": string }`, and the tool result names the advisor's `type.alias` so the UI shows who answered.
- **Only the objective is sent.** The advisor gets no transcript, tools, or memory. The agent writes the question plus a short summary of the facts needed; objectives over 8 KiB are refused with a tool error (this cap applies to every `reasoning_subagent` call).
- **Per-turn cap.** At most `advisor_max_calls_per_turn` consultations per turn (default 2, at least 1). A further call in the same turn returns a tool error saying the advisor budget is used.
- **Cost.** The advisor's token usage is recorded in the turn's cost tracker under the advisor's `type.alias` and the calling agent, so `/api/cost?agent=<alias>` includes it.
- **Privacy.** The advisor's vendor sees the objective text. Choose an advisor whose vendor you are willing to send that content to.
- **Validation.** A `model:` target must name a configured `[providers.models.<type>.<alias>]` entry. `harness:<name>` parses but is refused for now: advisors backed by an external harness come with #381.

## Recursion

Local recursion stays denied (frozen contract D1): a v1 child cannot spawn a child: the `reasoning_subagent` admission refuses any spawning lineage deeper than the parent's root. One immutable spawn lineage (`LineageRef`, SA-9) still threads every agent boundary, so no future spawn surface can reset depth by rebuilding a registry. The runtime-profile `max_delegation_depth` key retired with the spawn tools: legacy files carrying it are ignored with a load warning. Cron `JobType::Agent` runs are top-level roots, not continuations of an interactive parent's lineage.

## What a child may and may not reach

1. **No parent credentials or config tree.** Model access is an opaque host-resolved binding (SA-7d); the child never holds provider keys.
2. **No parent memory.** No `memory_store`/`memory_forget`/`memory_purge`, no live parent backend, no parent agent UUID (SA-7e/SA-17). Personal-memory changes can only return as typed candidates in the report; the parent decides disposition.
3. **No channel handles.** A child seeds zero channel handles on every spawn path; its `ask_user` fails closed. User input is requested through typed parent-request events (SA-7c/SA-25).
4. **No parent-alias identity.** The child runs under a run-scoped principal (SA-13), auditable as its own actor.
5. **One result channel.** The child returns a `SubAgentReportV1` (SA-21); there is no prose relay contract and no second durable task ledger for the v1 path (SA-26).

## What's not supported

1. **Local recursion beyond the parent.** Structural refusal at admission, not a budget.
2. **A separate long-lived identity for the child.** Children are run-scoped principals; the long-lived personal Agent identity remains the parent's.
3. **Detached/background local children.** The retired tools' detached arms died with them; durable background work belongs to the Tachi bridge.
4. **Streaming progress back to the parent.** The parent sees the structured report after the bounded run completes.
5. **A child messaging the user directly.** User input and final wording belong to the parent (SA-7c/SA-25).

## History

- #197 wall 1 removed the `delegate` tool (live parent-registry handout, credential clones, channel-wired handles; its config surface retired with it).
- The #197 spawn wall removed the `spawn_subagent` tool (same-alias full-parent-inheritance child; full config clone; detached coordinator producer). The teaching sections that used to document its gates, output strings, and verification greps were deleted with the tool; the tool's own module is gone from the tree.
- The control-plane migration wall (wall 4) deleted the durable control plane itself: the `zeroclaw-coordinator` child host, the announce chain, and the `data/control_plane.db` task ledger. Its last production writer died with the spawn wall; durable task/attempt truth is Tachi's through the bridge. Legacy `control_plane.db` files stay in place, never read or rewritten, and are reported by a once-per-boot warning.
