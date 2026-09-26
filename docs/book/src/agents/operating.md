# Running agents

Because there is no privileged "the agent," every command that drives an agent
names which one. Agents coexist; you address one by its alias.

## Addressing an agent

On the CLI, the agent alias is required, there is no default agent:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw agent -a <alias> -m "hello"
```

</div>

The alias is the `<alias>` half of an `[agents.<alias>]` block. For the full CLI
surface and every flag, see the generated
[CLI reference](../reference/cli.md).

## Chatting through the gateway

`zeroclaw agent -m "..."` runs one turn inside the CLI process and prints the
reply. To talk to the agent the gateway hosts (the one the web dashboard and
every other device see), use `zeroclaw chat`; `zeroclaw agent` without `-m`
opens it too:

<div class="os-tabs-src">

#### sh

```sh
zeroclaw chat -a <alias>                         # session "main" on this machine's gateway
zeroclaw chat -a <alias> -s work                 # another session
zeroclaw chat -a <alias> -m "what's on today?"   # one message, then exit
zeroclaw chat -a <alias> --gateway wss://home.example:42617
```

</div>

- Every client on the same session, including the dashboard, shares one
  conversation and sees the same replies.
- Leaving the chat (`/quit`) does not stop a running turn; `/cancel` or Ctrl+C
  does, for every client on the session.
- Tool approvals are asked in the terminal: `y` approves once, `a` always,
  anything else denies.
- The gateway must be running (`zeroclaw daemon` or `zeroclaw gateway start`).
  When pairing is on, the token comes from `ZEROCLAW_GATEWAY_TOKEN`, or from a
  plaintext `zc_*` entry in `gateway.paired_tokens`.

## Coexistence and isolation

Agents run side by side from one install. Each one keeps its own workspace,
memory, and identity (see [Filesystem components](./filesystem.md)), so by
default nothing one agent does leaks into another. They share only what their
config references share, a provider, a channel, a bundle.

One agent reaches another through **messaging** on a shared channel: two
agents can address each other only where they share a
[peer group](../channels/peer-groups.md). (The retired `delegate` tool that
used to provide a second, policy-gated hand-off path is removed; see
[Delegation & SubAgents](./delegation.md) for the retirement record.)

When an agent needs a one-off helper instead of an existing peer, it spawns an
ephemeral [SubAgent](./delegation.md) that inherits its identity and
security policy for a single task, then disappears.

## Operating multiple agents at once

`zeroclaw daemon` brings up every enabled agent together, each answering on its
own channels. Adding an agent is additive: define a new `[agents.<alias>]`
block, wire its references, and it joins the running set, the existing agents
are untouched.

For the runtime internals, the permission model, the memory model, and the
agent loop, see [Runtime internals](./internals.md).
