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

A legitimate exception to the 5,000-token provider-wire ceiling or minimal membership table must be recorded in `scripts/ci/wire_budget_exceptions.json` with all 8 required fields:

- `owner`: GitHub handle of the authorizing maintainer.
- `tool_name`: Exact tool identifier receiving the exception.
- `rationale`: Verifiable proof why the capability must reside in the kernel.
- `wire_tokens`: Measured delta in provider-wire token cost.
- `sunset_decision`: Permanent retention or scheduled deprecation date.
- `security_privacy_impact`: Security and privacy boundary evaluation.
- `dependency_cost_rationale`: Analysis of dependency overhead and build footprint impact.
- `pr`: Associated pull request number establishing the exception.
