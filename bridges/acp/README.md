# ACP gateway bridge

`zeroclaw-bridge-acp` builds the `zeroclaw-acp-bridge` binary, a bounded ACP v1
stdio projection of the gateway conversation. The gateway owns agent execution,
session history, approvals and cancellation. This package contains the transport
adapter and its process-local session bindings.

## Build and test

From the repository root:

```bash
scripts/dev/cargo-local.sh build --release --locked -p zeroclaw-bridge-acp --bin zeroclaw-acp-bridge
scripts/dev/cargo-local.sh test -p zeroclaw-bridge-acp
```

The bridge needs no root package feature. Source installation and Nix builds
include it when their selected root feature closure contains `agent-runtime`.
Minimal installations without that feature continue to install only `zeroclaw`.
Source Docker images build and ship only the main `zeroclaw` executable.

## Configuration and protocol

Run `zeroclaw-acp-bridge --config /absolute/adapter.toml`. The configuration,
dedicated owner credential and bounded protocol are documented in
[Owner ACP bridge](../../docs/book/src/gateway/api.md#bounded-owner-bridges-and-acp).

The bridge accepts only numeric loopback WebSocket endpoints. Its credential
file must remain owned by the invoking user with mode `0600`. It retains the
gateway owner grant, once-only permission choices, exact-turn cancellation and
bounded S2 reconnect behavior. It does not execute client filesystem, terminal
or MCP commands. Protocol output goes to stdout; diagnostics go to stderr.

Permission labels use `zeroclaw-config`'s optional `i18n` module, sharing the
runtime's locale detection, embedded English and Chinese catalogs, disk
overrides and fallback behavior. The bridge does not depend on the runtime,
providers, memory, tools or gateway server crates.
