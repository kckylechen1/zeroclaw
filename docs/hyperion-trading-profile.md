# Hyperion trading agent — risk profile

The trading agent runs unattended. A cron `JobType::Agent` job fires at a market
hour and a WeChat message arrives whenever the owner sends one; neither waits for
someone to be watching. On that path `ApprovalManager::for_non_interactive` sets
`non_interactive_shell_requires_approval: false`, so `shell` drops to
`NotRequired` — no approval at all. Whatever the agent can reach, it can reach at
03:00 with nobody to stop it.

So the profile below is not defence in depth for its own sake. Each layer closes
a hole the layer above cannot see.

```toml
[risk_profiles.hyperion_trading]
level = "supervised"
workspace_only = true

# Layer 1 — the tool never dispatches.
# An explicit list, so `shell` and `file_write` are absent rather than removed.
# Absent and empty mean different things here: absent (`allowed_tools` unset)
# is unrestricted, `[]` is deny-all. Naming the set is the only safe form.
allowed_tools = [
  "memory_recall",
  "memory_store",
  "hapi-edge__snapshot",
  "hapi-edge__batch_snapshot",
  "hapi-edge__history_klines",
  "hapi-memory__hapi_save",
  "hapi-memory__hapi_search",
  "hapi-memory__hapi_memory",
]

# Layer 2 — a server that grows a tool gains no reach until someone names it.
# This is the default; stated here because it is the load-bearing line.
mcp_discovered_tool_policy = "explicit_only"

# Layer 3 — backstop. If a future edit widens layer 1, these still stop at a
# prompt, and an unattended prompt is a denial.
always_ask = ["shell", "file_write", "file_edit", "delegate", "spawn_subagent"]

# Layer 4 — subtract even if something upstream re-admits them.
excluded_tools = ["shell", "file_write", "file_edit"]

auto_approve = []
block_high_risk_commands = true
require_approval_for_medium_risk = true

[risk_profiles.hyperion_trading.delegation_policy]
mode = "forbidden"
```

## Why each layer is not redundant

| Layer | Stops | Blind to |
|---|---|---|
| `allowed_tools` | tool never dispatched | tools added to the list later by a well-meaning edit |
| `mcp_discovered_tool_policy` | an MCP server offering a **new** tool | tools that are named |
| `always_ask` | anything that slipped through layers 1–2 | nothing — but only forces a *prompt* |
| `excluded_tools` | re-admission from any source | — |

Layers 1 and 2 are the ones that matter. Layers 3 and 4 exist because a
config edit is easier to get wrong than a code path, and this is the config
most likely to be edited in a hurry.

## What this profile deliberately does not contain

`shell`, `file_write`, `file_edit` — a trading agent has no business writing
files or running commands. `delegate` and `spawn_subagent` — a sub-agent that
inherits this profile is fine, but delegation to a *different* agent is a way
to escape the tool list, so it stays forbidden.

## The line this does not defend

Everything above is client-side. It stops the agent from *calling* a trading
write tool; it does not stop that tool from existing.

**A server-side read-only profile does not exist yet.** An earlier draft of this
file named `hapi-edge --profile quant_qa_read_only` as the first line of defence.
That profile is a frozen DRAFT spec in the Hyperion repo
(`docs/InProgress/runtime/TOOL_AUTHORITY_CATALOG_FROZEN_SPEC_DRAFT.md`) with no
implementation — a survey of `internal/`, `cmd/` and `deploy/` found zero
references to it, and `docs/Spec/tool_authority_catalog.yaml` does not exist.

What hapi-edge ships today is `HAPI_TOOL_PROFILE` with `standard` / `facade` /
`full|admin` (`internal/mcpserver/tools.go:25-66`). These switch which tools are
*registered*, not what they may do. Each tool carries a `ReadOnly` field, but it
is only emitted as an MCP `ReadOnlyHint` annotation for the client — it is not
enforced as an access gate anywhere in the server.

So until that catalog ships, **the layers in this file are the only enforcement
that exists**, and `facade` profile plus this allow-list is the narrowest
reachable configuration. That is a thinner defence than "server-side first,
client-side second" implies. Treat closing the catalog as a prerequisite for
letting an unattended agent anywhere near execution-tier tools.

## Enforced by

- `crates/zeroclaw-runtime/src/approval/mod.rs` — the `unattended_*` tests pin
  that `always_ask` is consulted before both the non-interactive `shell` bypass
  and the Full-autonomy short circuit.
- `crates/zeroclaw-tools/src/tool_search.rs` — the `explicit_only_*` tests pin
  that an unlisted `<server>__<tool>` name is rejected, and that the same inputs
  under `auto_admit` are not, so the gate is provably the flag.
- `crates/zeroclaw-config/src/policy.rs` —
  `from_profiles_propagates_every_risk_profile_field` pins that the policy
  actually reaches `SecurityPolicy`, using the non-default variant so a dropped
  field cannot pass.

## Still missing

An approval here is memory-only: nothing binds it to a run, a tool, and an
argument hash, and nothing expires it on restart. There is no durable audit
trail proving who approved what. Both are tracked in
[`hyperion-patch-census.md`](./hyperion-patch-census.md).
