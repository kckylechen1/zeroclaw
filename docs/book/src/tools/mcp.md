# MCP

ZeroClaw is an MCP client: it connects to external [Model Context Protocol](https://modelcontextprotocol.io) servers and exposes their tools to the agent. Each MCP tool is namespaced as `<server>__<tool>` (for example `filesystem__read_file`), so tools from different servers never collide.

## Configure MCP

MCP support is enabled by default, but no external MCP tools are exposed until at least one server is configured under `mcp.servers` and an agent is granted that server through its `mcp_bundles` (see Per-agent server scoping below). Configure through the gateway or `zeroclaw config set`:

```sh
zeroclaw config set mcp.servers.filesystem.command npx
```

Set `mcp.enabled = false` to disable MCP tool loading without removing server definitions.

## Per-agent server scoping (`mcp_bundles`)

A `[[mcp.servers]]` entry only *defines* a server. Which servers an agent actually connects to is decided by that agent's `agents.<alias>.mcp_bundles`, and the model is secure by default: omission is not a grant.

- An agent with no `mcp_bundles` connects to **no** MCP servers, even when `mcp.servers` is non-empty.
- A bundle `[mcp_bundles.<alias>]` names the servers it grants by their `mcp.servers` `name`, with an optional `exclude` list. An agent's grant is the union of the servers across every bundle it references, minus any name excluded by any of those bundles (deny wins).
- An unknown bundle alias, or a bundle server name that matches no configured server, grants nothing. Both fail closed and are reported as non-fatal warnings by config validation, so a typo narrows an agent's access instead of widening it.

```toml
[[mcp.servers]]
name = "filesystem"
command = "npx"

[mcp_bundles.files]
servers = ["filesystem"]

[agents.assistant]
mcp_bundles = ["files"]   # connects to `filesystem`; an agent without this gets no MCP servers
```

- Bundle changes take effect on **session restart**. The resolver (`Config::mcp_servers_for_agent`) runs at session/agent construction time; editing `[mcp_bundles.*]` or `agents.<alias>.mcp_bundles` while a session is live does not change that session's connected servers. End and restart the affected sessions to pick up new grants.

This is the *connection* boundary (which servers an agent talks to at all). The `allowed_tools` / `excluded_tools` controls below are the per-tool *capability* boundary applied on top of whatever a granted server exposes.

## Transports

A server is reached over one of three transports (the `transport` field):

| Transport | When to use | Required fields |
|---|---|---|
| `stdio` (default) | A local process you spawn (a Node.js or Python MCP server) | `command`, optional `args`, `env` |
| `http` | A remote server speaking MCP over HTTP POST | `url`, optional `headers` |
| `sse` | A remote server speaking MCP over HTTP + Server-Sent Events | `url`, optional `headers` |

`env` (stdio) and `headers` (http/sse) are stored as secrets; `headers` commonly carries the `Authorization: Bearer …` token for the upstream server.

Add a server through the gateway or `zeroclaw config set` (for example `zeroclaw config set mcp.servers.filesystem.command npx`). A stdio server needs `command` plus optional `args`/`env`; an http/sse server needs `url` plus optional `headers`. The per-field commands are in the field table below.

## Editing servers

Two surfaces edit the same `[[mcp.servers]]` table:

- **`config.toml`**: hand-edit the keys documented below. The full table is round-tripped on save.
- **Web dashboard**: currently renders `mcp.servers` through a JSON-array editor.

## Server fields

Per-server fields (`[[mcp.servers]]`), generated from the schema:

{{#config-fields mcp.servers}}

`tool_timeout_secs` is an optional per-call timeout; it must be greater than 0 and is capped at 600 seconds.

## Top-level fields

{{#config-fields mcp}}

## Deferred loading

`mcp.deferred_loading` is `false` by default, so configured MCP tools are included in the model context eagerly. Set it to `true` to place only MCP tool **names**, each with a one-line summary of its description, in the system prompt. The agent can use `tool_search` to fetch a tool's full schema when it needs the parameter contract. This keeps the initial context window small when a server exposes many tools.

If the agent already knows the arguments, a call to an exact, authorized
`<server>__<tool>` name also activates that tool. Native calls and text-protocol
fallback use the same conversation catalog and policy. Unknown names, suffixes
of unactivated tools and case variants do not activate tools. Discovery does not
grant access: risk-profile and caller allowlists, deny rules and current-turn
exclusions still apply. The original hooks and approval gate run before automatic
activation and execution. A cancelled or unapproved call does not activate a tool.
Activated schemas remain available for later iterations of the conversation.
The streaming text filter uses the same authorized names as fallback parsing,
so deferred JSON tool calls are withheld from ordinary text chunks before execution.

### Recovering from argument errors

MCP tools return a compact recovery hint for clearly missing required fields or
incorrect top-level JSON types, and for a server's JSON-RPC `-32602` rejection.
Hints show field names and types, with at most eight fields and 48 characters per
field name. They omit argument values, schema descriptions, defaults and enum
values. The agent can fetch the full schema with `tool_search` and submit a
corrected call through the normal policy and approval path; no call is replayed
automatically.

This is a conservative shape check, not a complete JSON Schema validator.
References, composition schemas, nested constraints, undeclared types and
business rules remain the MCP server's responsibility. Nullable inputs are
accepted when the schema declares them. Task continuations keep their existing
handle validation. Transport failures and `isError` execution failures retain
their normal error handling and are not guessed to be argument errors.

## Binary tool results

When a tool result contains an `image` or `audio` item, or an embedded `resource` with a base64 `blob`, anywhere in the result (including nested inside `structuredContent`), ZeroClaw replaces the base64 payload with a one-line note such as `[image attachment omitted: image/png, about 18 bytes]` before the result reaches the model. Text content and other fields are kept. The model does not see the binary data itself; images from MCP tools are not yet passed to vision-capable models.

## Security and approval

MCP tool calls go through the same approval gate as every other tool, governed by the agent's risk profile (`risk_profiles.<alias>`). The `tool_search` discovery step is auto-approved so deferred MCP loading can work in non-interactive sessions, but tools discovered from MCP servers still follow the normal approval policy:

- At autonomy `level = full`, calls do not prompt unless their name matches `always_ask` (MCP tools included).
- Otherwise, an MCP tool call prompts for approval unless its **prefixed** name (`<server>__<tool>`) is in the profile's `auto_approve` list. `auto_approve = ["*"]` approves everything; an exact entry like `auto_approve = ["filesystem__read_file"]` approves just that tool.
- `always_ask` is the inverse: a name (or `"*"`) there always prompts, overriding `auto_approve`.

An already activated tool's unique suffix alias is resolved to its prefixed
name after `before_tool_call` hooks and before approval. The same canonical
approval rule therefore covers direct aliases and hook rewrites, including at
full autonomy. This read-only lookup does not activate a deferred tool.

### Authorization: `allowed_tools` / `excluded_tools`

Approval gates *when* a tool call needs a human green-light. Authorization gates *whether* the agent can call a tool at all. The two are independent.

Keep the three MCP tool controls on their own axes:

| Control | Scope | Use it for |
|---|---|---|
| `tool_filter_groups` | Prompt/context exposure | Decide which MCP tool schemas are visible to the model for a turn. |
| `auto_approve` / `always_ask` | Approval policy | Decide whether a selected MCP tool call requires operator approval. |
| `allowed_tools` / `excluded_tools` | Capability policy | Decide which prefixed tool names the risk profile may use at all. |

Runtime-discovered MCP tools use the same explicit capability policy:

- Omitted `allowed_tools` imposes no allowlist constraint. An explicit
  `allowed_tools = []` denies every tool.
- With a non-empty allowlist, the default `mcp_discovered_tool_policy =
  "explicit_only"` admits only listed MCP names. Discovery and automatic schema
  activation cannot add permissions.
- An operator can explicitly select `mcp_discovered_tool_policy = "auto_admit"`
  to admit unlisted `<server>__<tool>` names under a non-empty risk-profile
  allowlist. An empty allowlist still denies all tools.
- `excluded_tools` always wins. Caller-supplied per-run allowlists, including
  cron scopes, are strict intersections and never inherit the auto-admit
  exception.

For example, this profile admits a read tool without admitting its write sibling:

```toml
[risk_profiles.assistant]
allowed_tools = ["tool_search", "filesystem__read_file"]
mcp_discovered_tool_policy = "explicit_only"
excluded_tools = ["filesystem__write_file"]
auto_approve = ["filesystem__read_file"]
```

`auto_approve` alone does not hide a tool from the model; it only answers the approval question after the model selects that tool. Use `tool_filter_groups` to reduce prompt noise and `allowed_tools` / `excluded_tools` to enforce a capability boundary.

See [Autonomy levels](../security/autonomy.md) for the full per-profile field surface, and the [Config reference](../reference/config.md#mcp) for every MCP field and default.

## MCP Resources and Prompts

In addition to MCP **tools**, ZeroClaw exposes MCP **resources** and **prompts**
from connected servers.

### Tools

Two built-in tools are available (subject to your agent's tool access policy):

- `mcp_resources`: `action: "list"` (optional `server`, `cursor`) lists
  resources; `action: "read"` with `uri` (prefixed `<server>__<uri>`) returns
  contents.
- `mcp_prompts`: `action: "list"` lists prompts; `action: "get"` with `name`
  (prefixed `<server>__<name>`) and optional `arguments` returns the rendered
  prompt messages.

Servers that do not advertise resource/prompt capabilities are skipped, and calls
against them return a clear "does not support" error.

### Pinning resources into context

Each MCP server entry accepts an optional `pinned_resources` field: a list of
resource URIs to read once at startup and inject into the system prompt. Set it
through the same config surfaces used to define the server (the gateway,
or `zeroclaw config set`, as shown under [Configure MCP](#configure-mcp)), naming
the resources you want the agent to always have on hand. The field defaults to
empty, so servers without it are unaffected.

Pinned content is read once per run (no live refresh) and is labeled
`trust="untrusted-external"` so the model treats it as data, not instructions.

### Security

Resource and prompt content originates from the configured MCP server and is
treated as **untrusted**: it is provenance-wrapped, secret-scrubbed, and
length-bounded before entering context. Access to `mcp_resources` / `mcp_prompts`
and to specific servers is governed by your agent's tool access policy (risk
profile), and narrows correctly when delegating to subagents.
