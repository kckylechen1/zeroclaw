# Core-Residency Rationale

This document defines the architectural policy and review checklist for adding or widening default tools, kernel primitives, or built-in integrations in ZeroClaw.

## Principle: Kernel Primitives Default-Closed

The ZeroClaw minimal companion profile (`composition = "minimal"`) is frozen around an explicit, bounded set of core primitives (15 tools, ≤5,000 provider-wire tokens). New tools or integrations do **not** join the default/kernel composition by convenience or default.

Before proposing a new default tool or kernel integration, the PR author must provide an explicit machine/checklist-visible **Core-Residency Rationale** answering why the capability cannot live on an existing extension plane.

---

## Extension Plane Evaluation Checklist

Every new default capability must evaluate all 6 extension planes:

1. **Skill**: Can this capability be authored as a prompt/instruction skill with local workspace scripts or tools?
   - *If yes*: Deliver as a Skill bundle, not a Rust kernel tool.
2. **MCP / Optional Integration**: Can this capability run as an MCP server or optional first-party crate/feature (`integrations-saas`, `hardware-tools`, etc.)?
   - *If yes*: Deliver as an external MCP server or feature-gated optional adapter.
3. **Node Capability (#55)**: Does this capability interact with physical device sensors, local hardware, or peripheral peripherals?
   - *If yes*: Route to the Node capability fabric via `/ws/nodes`.
4. **Surface / Channel Integration**: Does this capability represent inbound/outbound messaging or UI presentation?
   - *If yes*: Implement as a Channel adapter in `zeroclaw-channels` or a frontend client.
5. **Provider Adapter**: Does this capability interact with model inference, embeddings, or voice/transcription APIs?
   - *If yes*: Implement as a typed Provider in `zeroclaw-providers`.
6. **Tachi Worker / Harness Adapter (#200)**: Does this capability execute complex, long-running, or repo-mutating workflows (e.g., git commits, test runs, repository refactoring)?
   - *If yes*: Route through `TaskIntentV1` to the Tachi task execution bridge.

---

## Minimal Profile Exception Invariants

A legitimate exception to the 5,000-token provider-wire ceiling or minimal membership table needs an explicit owner decision recorded in the PR that raises it, naming the owner, the tool, why it must be in the kernel, its measured provider-wire token cost, and whether the exception is permanent or has a sunset date. The ceiling constant in `provider_wire_budget.rs` changes only with that decision.

## Email tool compile boundary

The existing root `channel-email` feature selects the optional email tool and
channel implementations. The crate-local `email-tools` features forward that
selection; they add no runtime configuration or admission authority.

| Integration | Current module / feature | Default build | Default tool-visible | Heavy dependencies | Extension plane | Compatibility | Source deletion now? |
|---|---|---|---|---|---|---|---|
| Email search/read | `zeroclaw-tools/src/email_{imap,read,search}.rs`; tools/runtime `email-tools`; root/channels `channel-email` | Yes for root defaults and standalone tools/runtime defaults; no for root `--no-default-features --features agent-runtime` | No with default disabled email config; full composition registers both tools only with the compile feature and an enabled email channel | `async-imap`, `mail-parser` | Optional first-party tools shared with the Email Channel | Config, environment, credentials, permissions and data formats unchanged; custom no-default builds must opt in to compile email tools | No; retained for explicit email builds |

Root `channel-email` conditionally forwards to an already selected runtime.
The channel feature also forwards to the shared IMAP utility in the tools crate.
Minimal runtime composition still applies its existing membership policy even
when email support is compiled. SaaS and hardware feature selection is separate.
This is the email compile-graph slice of [issue #211](https://github.com/kckylechen1/zeroclaw/issues/211), not completion of its remaining integration census.
