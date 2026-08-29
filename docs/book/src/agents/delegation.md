# Delegation & SubAgents

A SubAgent is an **ephemeral child run** spawned by a parent agent that inherits the parent's identity by default: same agent alias, same `SecurityPolicy`, same memory allowlist, same configured model provider, same tool registry. Auditable as a child via a tracing span `agent.<alias>.subagent.<run_id>`.

SubAgents are not a separate configuration concept. There is no `[subagents.*]` block in the schema. Every SubAgent's identity is whichever parent's agent loop spawned it.

## Which spawn tools exist

- **`spawn_subagent`**: runs the SAME agent again under its own identity for a focused subtask. The child sees the parent's full permissions envelope minus any narrowing. Use when the parent wants to scope an internal subtask out of its main conversation history without changing identity. This page documents `spawn_subagent` end to end; it lives at `crates/zeroclaw-runtime/src/tools/spawn_subagent.rs`.
- **`reasoning_subagent`**: the V1 bounded SubAgent entrypoint (profile-admitted, typed `SubAgentReportV1` result, no ambient parent inheritance). It fronts the minimal composition and is the replacement-first surface for bounded SubAgent work.
- **`delegate`**: RETIRED. The legacy delegation tool was removed in #197 wall 1: it was constructed from full parent inheritance (per-alias API-key clones, the parent's fallback credential, a full `Arc<Config>` snapshot, a live pre-filter parent-registry snapshot, and channel-wired user-reaching handles handed to children), all of which the frozen SubAgent contract forbids on child paths. Running work under a different configured agent identity moves to the Tachi bridge (durable/heavy work) and to admitted V1 SubAgent profiles; the name is reserved and no plugin or skill can re-register it.

`spawn_subagent` is a full/legacy-composition surface: it is available whenever `composition = "full"` (the default for pre-existing configs) and remains available as legacy migration debt, tracked by the SubAgent migration epic. Fresh installs on `composition = "minimal"` front the V1 `reasoning_subagent` tool as the SubAgent entry point instead (see the [Tools overview](../tools/overview.md)).

## How a SubAgent is instantiated

Two spawn sites converge on `SubAgentSpawn` (`crates/zeroclaw-runtime/src/subagent/mod.rs`):

1. **From an agent loop**: the model calls the `spawn_subagent` tool with a `prompt` string. The tool is registered like any other in the registry (`crates/zeroclaw-runtime/src/tools/mod.rs`, `SpawnSubagentTool::new`).
2. **From cron**: `JobType::Agent` jobs run through `run_agent_job` (`crates/zeroclaw-runtime/src/cron/scheduler.rs`) which builds the same `SubAgentContext` but flags the child as a top-level run (not a SubAgent) so it can itself spawn one level of subagent.

Both paths invoke:

```rust
SubAgentSpawn::for_agent(config, parent_alias)?     // resolve parent identity
    .build(SubAgentOverrides::default())?           // validate any narrowing
```

`for_agent` reads the parent's `risk_profile` and `[agents.<alias>.workspace.read_memory_from]` to build the inherited allowlist; the parent's own alias is always added so a SubAgent always sees its parent's own memory rows. `build` applies optional narrowing (see [Permission inheritance](#permission-inheritance) below) and returns a validated `SubAgentContext`.

## Lifecycle

Synchronous, in-process, single tokio runtime. Nothing crosses the process boundary.

1. Parent's tool loop dispatches `spawn_subagent`. The tool reads its `prompt` argument, refuses if empty.
2. The tool checks two guards in order:
   - **Depth-1 cap.** If the calling run was itself a SubAgent (`AgentRunOverrides.is_subagent == true`), refuse with `"spawn_subagent: a subagent may not spawn its own subagents (depth-1 cap)"`. SubAgents cannot recurse.
   - **Unified lineage cap.** Independently of the depth-1 flag, the run's spawn lineage (one ledger across every local spawn, SA-9 of the frozen SubAgent contract) is checked against the runtime profile's `max_delegation_depth` (default 3); at or past the cap the tool refuses with the lineage depth-limit message. A registry rebuild inside a child cannot reset this counter.
   - **Risk-profile tool gate.** If the parent's `[risk_profiles.<alias>].allowed_tools` is non-empty and does not list `spawn_subagent`, or `excluded_tools` lists it, refuse with a message naming the parent alias.
3. The tool calls `SubAgentSpawn::for_agent` + `build`. Failures (unknown parent alias, escalating override) surface as `ToolResult { success: false, error: "subagent spawn failed: ..." }`.
4. The tool constructs `AgentRunOverrides { security, memory: None, is_subagent: true, suppress_memory_inject: true }` (the child's `SubTurn` origin already skips engine memory injection; the flag makes the opt-out explicit) and awaits `crate::agent::run` (`crates/zeroclaw-runtime/src/agent/loop_.rs`, `pub async fn run`) inside a tracing scope keyed `subagent-<uuid>`. The parent's `tool` execution **blocks** until the child returns.
5. The child agent loop runs to completion. Its tool registry is built fresh, with `is_subagent_caller: true` flowing into its own `SpawnSubagentTool` so any attempt to recurse is rejected at the depth gates. The child run also seeds NO channel handles: its `ask_user` and other channel tools fail closed, because a child must not reach the user directly (it reports through the parent).
6. The child returns `Result<String>`. The parent's `spawn_subagent` tool wraps it:
   - Success: `ToolResult { success: true, output: <child's final response>, error: None }`. Empty output is replaced with the literal `"subagent completed without output"`.
   - Failure: `ToolResult { success: false, error: Some("subagent run failed: ...") }`.
7. The parent's tool loop continues with that `ToolResult` in its conversation context. The child's intermediate turns and tool calls are NOT replayed into the parent's history; only the final response surfaces.

## What gets delivered back upstream

One thing: the child's **final assistant message**, as a string, wrapped in `ToolResult.output`.

- The child's tool calls, intermediate reasoning turns, and any memory writes the child performed are observable in the structured logs under the child's tracing span but do not enter the parent's conversation history.
- The child's session lives under the path `subagent-<uuid>` (or `cron-<uuid>` for cron-spawned runs). This is the conversation-history key, not a filesystem location, it isolates the child's history from the parent's.
- Memory writes performed by the child are written to the parent's identity (same agent UUID at the SQL/Postgres backends; same workspace dir for Markdown). Cron-spawned runs disable `memory.auto_save` so opt-in writes still work but routine recall doesn't accumulate.

There is no streaming or partial-progress channel back to the parent. Long-running SubAgents stall the parent's tool execution for their full duration; there is no per-call timeout knob.

### Multiple calls in one turn

The agent loop applies a per-turn duplicate-call guard: a tool called twice with identical arguments in the same turn normally has the second call skipped. `spawn_subagent` is **exempt** from that guard. Launching several with the same prompt (redundancy, sampling, fan-out) is an intentional pattern, not an accidental repeat, so each identical call runs and each result is returned. Without the exemption only the first identical call would execute and only its output would reach the model.

When parallel tool execution is enabled (`parallel_tools = true` in the runtime profile), multiple `spawn_subagent` calls in one turn run concurrently and every child's final response is returned to the parent, keyed to its own tool call.

## Permission inheritance

A SubAgent inherits the parent's permissions verbatim unless the spawn site supplies a narrowing `SubAgentOverrides`. Today both in-tree spawn sites pass `SubAgentOverrides::default()` (inherit everything). The override surface is shipped and validated; a future caller-supplied narrowing path drops in without runtime changes.

Inheritance axis by axis:

1. **`SecurityPolicy`**: inherited by `Arc<SecurityPolicy>` cloning. Override path (`SubAgentOverrides::policy = Some(policy)`) runs `SecurityPolicy::ensure_no_escalation_beyond` (`crates/zeroclaw-config/src/policy.rs`) and rejects any field that adds privilege the parent doesn't have. Validated axes include autonomy level, allowed_roots (rw + ro + write-only), allowed_commands, workspace_only, forbidden_paths in the parent ⊆ child direction, shell_env_passthrough, `max_actions_per_hour`, `max_cost_per_day_cents`, `shell_timeout_secs`, `block_high_risk_commands`, and `require_approval_for_medium_risk`. Rejections chain a precise `EscalationViolation` so diagnostics name the offending field.
2. **Action / cost budgets**: `PerSenderTracker` is shared between parent and child by `Arc` clone. Inherit-verbatim path: the child holds the same `Arc<SecurityPolicy>` so writes to `record_action()` / `record_cost()` hit the same bucket. Override path: `SubAgentSpawn::build` copies the parent's `tracker` field into the narrowed child policy explicitly. **A SubAgent cannot bypass `max_actions_per_hour` or `max_cost_per_day_cents` by spawning**, the limit is shared.
3. **Tool registry**: the child's registry is built fresh by `tools::all_tools_with_runtime` under the inherited policy. The registry then passes through `apply_policy_tool_filter` (`crates/zeroclaw-runtime/src/agent/loop_.rs`), which drops any tool whose name fails either gate:
   - The policy's `allowed_tools` / `excluded_tools` (sourced from the parent's `risk_profile`).
   - The caller-supplied `allowed_tools` argument to `agent::run`.
   `spawn_subagent` is in the registry but its `is_subagent_caller` flag is set to `true` for the child, so the recursion refusal fires before any spawn work. The `model_switch` tool is retired from the model surface entirely, so it is absent from the child's registry for the same reason it is absent from the parent's: a SubAgent inherits the parent's model verbatim (see axis 5) and no agent, parent or child, can switch the active model out from under a session through an ordinary tool. The retired `delegate` tool is likewise absent everywhere, by deletion rather than filtering.
4. **Memory allowlist**: a `HashSet<String>` of sibling agent **aliases** (the `[agents.<alias>]` config keys). Inherited from the parent's `workspace.read_memory_from` plus the parent's own alias. Override path (`SubAgentOverrides::allowed_agent_aliases`) is validated as a subset; any alias not on the parent's list is rejected by name. The parent's own alias is always re-added so a SubAgent always sees its parent's rows.
5. **Model provider**: inherited from the parent's `[agents.<alias>] model_provider` resolution. Temperature comes from the parent's provider entry (`config.model_provider_for_agent(parent_alias).and_then(|e| e.temperature)`). This inheritance is enforced, not merely a default: `model_switch` is retired from the model-visible registry (see axis 3), so neither a SubAgent nor its parent can switch models through an ordinary tool. To run a subtask on a different model, admit a V1 SubAgent profile whose `model_policy` names that model, or hand the work to a configured sibling agent through the Tachi bridge.
6. **Identity at the data layer**: same UUID in the `agents` table (SQL backends), same workspace dir for Markdown, same secret store. The parent-vs-child distinction is purely observability: a separate tracing span and a separate conversation-history session key.

## How a user makes one fire

You don't call these tools yourself; the bot does, from inside its turn. As a user, you influence the bot's choice with how you phrase the request. There is no special command, no slash-syntax, and no JSON the user types. Whether the model picks `spawn_subagent` depends on its system prompt, the tool's `description` text (visible to the model), and the user's wording. **Phrasing influences; it does not force.**

What CAN be made deterministic is **availability**: tools that aren't in the parent agent's registry can't be picked. The risk-profile gate lives in `[risk_profiles.<alias>].allowed_tools` and `[risk_profiles.<alias>].excluded_tools`. A non-empty `allowed_tools` list must include `spawn_subagent` for the model to see that tool; an empty `allowed_tools` list leaves tool availability unrestricted unless `excluded_tools` names the tool. Restart the daemon after editing the config.

What's verifiable end-to-end:

1. The literal output strings the tool returns to the model on each path (success, refusal, failure). Quoted verbatim below, sourced from `tools/spawn_subagent.rs`.
2. The literal config knobs that change behavior (`allowed_tools`, `max_delegation_depth`, etc.).
3. The structured tracing span shape that scopes everything emitted during the child run.

What's NOT verifiable from these docs:

1. Whether your specific bot, on your specific model, on your specific system prompt, will pick the tool when asked "Spawn a subagent to ..." Wording moves the needle; outcomes vary. If the bot doesn't pick the tool, the most reliable lever is to extend the bot's system prompt with explicit instructions ("When asked for a focused subtask, use the `spawn_subagent` tool").
2. The exact text the bot writes to you in its final reply. The bot reads the tool's output and **generates its own** reply on top. The tool's output text may be quoted, paraphrased, or summarized.

### `spawn_subagent`: refusal strings the model sees

These are exact, sourced from `crates/zeroclaw-runtime/src/tools/spawn_subagent.rs`. The model receives them as the tool's error string and reacts. The user-visible bot reply is whatever the model writes next; it commonly references or echoes the refusal.

1. Empty/missing `prompt` argument: `Missing or empty 'prompt' parameter`
2. Caller is itself a SubAgent (depth-1 cap): `spawn_subagent: a subagent may not spawn its own subagents (depth-1 cap)`
3. Lineage at the unified cap: `spawn_subagent: lineage depth limit reached (<depth>/<cap>). ...`
4. Parent's risk-profile tool gate excludes `spawn_subagent`: `spawn_subagent: refused — agent '<parent_alias>' risk_profile does not list spawn_subagent in allowed_tools`
5. Unknown parent alias / spawn build error: `subagent spawn failed: <wrapped error>`
6. Child run returned an error: `subagent run failed: <wrapped error>`

On success, the tool's output IS the child's final response text. If the child returned an empty string, the output is the literal placeholder: `subagent completed without output`. There is no fixed prefix to grep for in the success case.

### `spawn_subagent`: how to verify it actually fired

Tail your log. The tool-spawned child runs inside a `scope!` that emits a tracing span named `zeroclaw_scope` (with target `zeroclaw_log_internal_scope`) carrying `agent_alias=<parent>` and `session_key=<uuid>`. Every log line emitted during the child run carries those fields. The parent's own turn has its own `session_key`; a NEW `session_key` value appearing mid-turn for the same `agent_alias` is the signal that a SubAgent ran. The child's conversation-history session path is `subagent-<uuid>` (filesystem-ish identifier, distinct from the tracing field).

Cron-launched agent jobs use a different, more explicit span name: `subagent` (literal) with fields `category="cron"`, `agent_alias=<owning agent>`, `cron_job_id=<id>`, `run_id=<uuid>`, `spawn_site="cron"`. Cron paths are trivially greppable: `grep 'spawn_site="cron"' zeroclaw.log`. Note that cron-launched runs are top-level (`is_subagent=false`); they may themselves call `spawn_subagent` once.

This is a thin signal for the agent-loop spawn path. A dedicated "subagent started / completed" record routed through `attribution_span!(tool)` is tracked as a code-side follow-up, once the agent loop wraps tool execution in an attribution span, every `record!` inside the tool will carry `tool=spawn_subagent` automatically and the question becomes a trivial grep.

## Retired: the `delegate` tool (historical)

The `delegate` tool was removed in #197 wall 1. It is documented here only as a retirement record; no current build registers it, the name is reserved against plugins and skills, and the teaching sections that used to live here (delegation gating, delegate output strings, verification, and the `spawn_subagent` vs `delegate` comparison table) were deleted with it. Summary of where its capabilities went:

- **Run a bounded subtask under a controlled profile** → the V1 `reasoning_subagent` entrypoint: profile-admitted, content-only context bundles, typed `SubAgentReportV1` results, no ambient parent inheritance.
- **Run durable/heavy work under another configured agent or an external harness** → the Tachi bridge (Task/Procedure execution), not an in-kernel delegation tool.
- **Background fan-out with task ids and result polling** → Tachi-owned durable work; ZeroClaw's coordinator child store remains legacy migration debt only, and no new writer mints `Delegate` rows.
- **Config that only the old tool consumed** (`[risk_profiles.*].delegation_policy`, `[agents.*].delegates`, `delegate_same_risk_profile`, the `[delegate]` timeout section) is retired with it; leftover sections are ignored with a loud warning.

## What's not supported

1. **Recursion beyond depth 1.** A SubAgent cannot spawn its own SubAgent. The cap is a hard refusal at the tool, not a budget. Cron-launched runs start at depth 0 and may spawn one level; agent-loop-launched SubAgents are at depth 1 and refuse further spawning. The unified spawn lineage additionally counts every local spawn across one run and refuses at the profile's `max_delegation_depth`.
2. **A separate identity for the child.** SubAgents share the parent's agent UUID. Running work under a different configured identity is the Tachi bridge's job now that the legacy hand-off tool is retired.
3. **Per-spawn time budget.** There is no `timeout_secs` argument. The parent blocks for the full duration of the child run; cancellation has to flow through the broader interruption scope.
4. **Streaming progress back to the parent.** The parent sees the child's final response as a single string after completion.
5. **A `[agents.<alias>].subagent_*` config block.** The validator and override type ship today; the operator-facing config surface that plumbs caller-defined narrowing is not in this release. Both spawn sites pass `SubAgentOverrides::default()` until that surface lands.
6. **A child messaging the user directly.** A child run seeds no channel handles; its `ask_user` and channel tools fail closed. User input and final wording belong to the parent (the frozen SubAgent contract, SA-7c/SA-25).
