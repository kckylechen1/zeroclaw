# Hyperion lean coding profile

A Pi-class token diet for the Hyperion **coding** agent. It is not the
unattended trading profile in `docs/hyperion-trading-profile.md`: that
profile has no `shell` / `file_write`. This one keeps a small coding
core and names the hapi read facades the agent is allowed to call.

No new config schema. Every knob below already exists.

## Three-way split

| Knob | Lives on | Why it cannot move |
|---|---|---|
| `allowed_tools`, `excluded_tools`, `mcp_discovered_tool_policy` | `[risk_profiles.hyperion_lean]` (a card supersedes the agent's `risk_profile`) | Authorization. `RuntimeProfileConfig` is operational, not a grant list. |
| `prompt_injection_mode = "compact"` | `[runtime_profiles.hyperion_lean]` | Registers `read_skill` and keeps skill bodies out of the system prompt. |
| `deferred_loading = true` | global `[mcp]` | Process-wide. One daemon cannot defer MCP for Hyperion and keep eager MCP for another alias. |
| Agent alias | `[agents.hyperion]` | Must set **both** `risk_profile` and `runtime_profile`. Setting only one leaves the other on global defaults (`Full` skills, unrestricted tools). |

Copy [`hyperion-lean-profile.toml`](./hyperion-lean-profile.toml) into the
operator `config.toml` and point `[[mcp.servers]]` at the live hapi-edge
endpoints.

## ExplicitOnly — name every facade

Default `mcp_discovered_tool_policy` is `explicit_only`. Unlisted
`<server>__<tool>` names are not auto-admitted. If the allow-list omits
the hapi facades, the filtered deferred set is empty and `tool_search`
is not registered.

The example names:

- `hapi-edge__snapshot`
- `hapi-edge__batch_snapshot`
- `hapi-edge__history_klines`
- `hapi-memory__hapi_memory`
- `tool_search`

Do not relist withdrawn `hapi_save` / `hapi_search` as independent tools.
`portfolio_*` write facades stay on `hyperion_trading` unless an operator
explicitly adds them here. A server-side read-only hapi-edge profile
remains the first line of defence for trading writes.

`mcp_discovered_tool_policy = "auto_admit"` restores the old `__` bypass
(whatever the server offers next). Do not set it on this profile.

## Known boundary: skill tools

Shell/http tools declared inside a skill are registered **after** the
built-in `allowed_tools` filter. Only `excluded_tools` subtracts them.
A workspace skill can therefore put `skillname__tool` onto a lean agent
even when that name is not in `allowed_tools`. Do not treat Leaf 1 as
closing that hole.

## Provider-wire budget

The regression gate reuses the production path: `build_iteration_tool_specs`
then `OpenAiModelProvider::chat_tools_wire` (the same `NativeToolSpec`
array `chat()` puts on the wire). Tokens are `ceil(len/4)` of that
**complete** `tools[]` JSON once, not a sum of per-tool ceils.

This repo has no Hyperion trading skill bundle. Compact-skill goldens
copy two contributor `SKILL.md` files from `.claude/skills/`
(`zeroclaw`, `changelog-generation`) into
`crates/zeroclaw-runtime/tests/fixtures/lean-skill-bundle/`. They are
instruction-only (no `SKILL.toml` tools). Measured on this branch: **2
skills loaded, 0 skill tools, 12 registry tools** on the worst-case
assembly (11 lean built-ins + `tool_search`). Skill-tool bypass remains
a Leaf 3 boundary; if a future fixture declares shell/http tools, report
the measured registry size instead of assuming 11.

Default assembly counts are informational (upstream adding a built-in
must not fail CI). The freeze hangs on the worst-case lean assembly
only: copied skills + ExplicitOnly MCP + WeChat `inject_memory=false`.

| Assembly | Tools | params tok | native `tools[]` | system prompt | whole turn |
|---|---:|---:|---:|---:|---:|
| `Config::default()` (informational) | 48 | 9,768 | 13,749 | 2,287 | **16,036** |
| `hyperion_lean` (WeChat, no skills, no MCP) | 11 | 1,176 | 1,949 | 2,093 | **4,042** |
| `hyperion_lean` + copied `SKILL.md` fixture | 11 | 1,176 | 1,949 | 2,583 | **4,532** |
| `hyperion_lean` + ExplicitOnly MCP (`tool_search`) | 12 | 1,253 | 2,082 | 2,193 | **4,275** |
| `hyperion_lean` + copied skills + ExplicitOnly MCP (**gate**) | 12 | 1,253 | 2,082 | 2,683 | **4,765** |

Default offenders on the wire: `cron_add` 1,161, `cron_update` 1,015, `git_forge` 678, `browser` 622.

The frozen ceiling is **5000 tokens** (measured peak 4,765 on the gate
row; slack rather than a tight false green). Re-run the harness after
changing the allow-list or default tool schemas:

```sh
cargo test -p zeroclaw-runtime --lib provider_wire_budget -- --nocapture
```

Do not measure Rust `json!` source length of `parameters_schema`. That
counts source characters, not wire JSON.
