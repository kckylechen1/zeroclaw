# Delegation & SubAgents

A SubAgent is an **ephemeral child run** spawned by a parent agent. Under the frozen SubAgent contract (#202), a child receives a bounded, admitted context (never the parent's identity, credentials, registry, memory UUID, or channel handles) and returns a typed result to the parent, which stays the only user-facing persona.

There is no `[subagents.*]` block in the schema; SubAgents are not a separate configuration concept.

## Which spawn tools exist

- **`reasoning_subagent`**: the V1 bounded SubAgent entrypoint and the single spawn surface on every composition. Profile-admitted, typed `SubAgentReportV1` result, no ambient parent inheritance, no detached/background mode, no tool execution in the v1 child. See the [Tools overview](../tools/overview.md).
- **`spawn_subagent`**: RETIRED (#197 spawn wall). The legacy entry point ran the same agent again under its own identity: the child got the parent's whole `Arc<Config>` (every provider credential), rebuilt the full tool registry under the parent's security policy, received a fresh memory backend over the parent's same-UUID memory rows with live memory tools, and, in the CLI process, a live channel map that could reach the user. The detached (`background: true`) arm handed children to the coordinator, which ran them through the same full-parent-config path. Every one of those inheritance axes is forbidden for child paths by the frozen contract (SA-7a/7c/7d/7e, SA-13, SA-17), and the tool's only value proposition was that inheritance, so it was deleted rather than reduced. The name is reserved in `RETIRED_OPERATOR_TOOL_NAMES`: no plugin or skill can re-register it.
- **`delegate`**: RETIRED (#197 wall 1). The legacy delegation tool handed children the parent's live tool Arcs, per-alias API-key clones, the parent's fallback credential, and channel-wired handles. Running work under a different configured agent identity moves to the Tachi bridge (durable/heavy work) and to admitted V1 SubAgent profiles.

## Where the capabilities went

- **Run a bounded subtask out of the main conversation** → the V1 `reasoning_subagent` entrypoint: profile-admitted, content-only context bundles, typed `SubAgentReportV1` results, run-scoped child identity, no parent credentials or registry.
- **Run durable/heavy work under another configured agent or an external harness** → the Tachi bridge (Task/Procedure execution), not an in-kernel delegation tool.
- **Background fan-out with task ids and result polling** → Tachi-owned durable work. ZeroClaw's coordinator child store is legacy migration debt only; after the spawn wall no production writer mints new child rows, and the store's disposition belongs to the control-plane migration wall.

## Recursion

Local recursion stays denied (frozen contract D1): a v1 child cannot spawn a child: the `reasoning_subagent` admission refuses any spawning lineage deeper than the parent's root. One immutable spawn lineage (`LineageRef`, SA-9) still threads every agent boundary, so no future spawn surface can reset depth by rebuilding a registry; the coordinator's own admission cap (`max_delegation_depth`) remains the separate, coordinator-side bound. Cron `JobType::Agent` runs are top-level roots, not continuations of an interactive parent's lineage.

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
- Legacy `control_plane.db` child rows written by the retired tools stay in place, unread by anything new and readable only through the unchanged fail-closed read path, until the control-plane migration wall disposes of them.
