# Testing

ZeroClaw uses a five-level testing taxonomy backed by filesystem layout. Each level has a different boundary and a different cost; pick the lowest level that proves what you need to prove.

When a PR claims behavior that a user directly runs, clicks, sends, installs, or observes, use [User-boundary proof](./user-boundary-proof.md) to identify the smallest test or manual check that reaches that boundary.

## The five levels

| Level | What it tests | Boundary | Where it lives |
|---|---|---|---|
| **Unit** | A single function or struct | Everything mocked | `#[cfg(test)]` blocks in `src/**` or co-located `tests.rs` |
| **Component** | One subsystem inside its own boundary | Subsystem real, everything else mocked | `tests/component/` |
| **Integration** | Multiple internal components wired together | Real internals, external APIs mocked | `tests/integration/` |
| **System** | Full request → response across all internal boundaries | Only external APIs mocked | `tests/system/` |

Plus two non-test directories:

| Directory | Purpose |
|---|---|
| `tests/manual/` | Human-driven test scripts (shell, Python), run directly, not via cargo |
| `tests/support/` | Shared mock infrastructure, not a test binary, included as `mod support;` from each level |

## Running tests

<div class="os-tabs-src">

#### sh

```sh
scripts/dev/cargo-local.sh test -p zeroclaw-runtime --lib
scripts/dev/cargo-local.sh test -p zeroclaw-runtime --lib --features heavy-tests
scripts/dev/cargo-local.sh test -p zeroclaw-gateway --lib --features nodes
scripts/dev/cargo-local.sh test -p zeroclawlabs --test component
scripts/dev/cargo-local.sh test -p zeroclawlabs --test integration agent
scripts/dev/cargo-local.sh test -p zeroclawlabs --test system
scripts/dev/leg.sh --list                   # CI's package/feature partitions
scripts/dev/leg.sh app                      # before push: only affected legs
./dev/ci.sh test-component                  # component tests in Docker
```

</div>

Use a package and `--lib` or `--test <target>` during iteration. A test-name
filter alone does not narrow the binaries Cargo compiles. Before pushing, run
the affected legs from `dev/ci/test-partition.json`; CI runs every leg. Do not
substitute a full-workspace host build for that selection.

## Local build storage

Use `scripts/dev/cargo-local.sh` instead of bare `cargo` for host-side checks
across linked worktrees. The existing `scripts/dev/leg.sh` runner also uses this
entrypoint for its metadata, runner detection, main test and extra runs. Both
the launcher and `scripts/dev/target_sweep.sh` use the main checkout's `target`
directory by default. They resolve relative `CARGO_TARGET_DIR` paths from the
checkout root and canonicalize symlink aliases. The launcher's `--target-dir`
option takes precedence over the environment; arguments after `--` belong to
the test or program and do not select a target. Checkouts with a separate Git
directory must provide an explicit target path.

The launcher checks the pinned toolchain, preserves managed host admission,
and defaults to non-incremental compilation. `CARGO_INCREMENTAL` overrides are
respected. These entrypoints require Python 3 with Unix file locking on macOS
or Linux. The sweep also requires Bash 4 or newer; macOS's bundled Bash 3 is
insufficient.

```sh
scripts/dev/cargo-local.sh test -p zeroclaw-memory --lib
scripts/dev/cargo-local.sh clippy -p zeroclaw-memory --all-targets -- -D warnings
```

Repository Cargo config also disables incremental compilation for bare Cargo.
The local launcher defaults `CARGO_PROFILE_DEV_DEBUG` to `0`, retaining symbols
but omitting debug information. The test profile inherits this setting unless
overridden separately. Debug assertions, overflow checks, and release profiles
are unchanged. Explicit dev/test environment overrides are preserved. Restore
file/line backtraces for a diagnostic run with:

```sh
CARGO_PROFILE_DEV_DEBUG=line-tables-only scripts/dev/cargo-local.sh test -p zeroclaw-config --lib
```

Use `CARGO_INCREMENTAL=1` only for a deliberate short edit loop. Turning it off
does not delete existing incremental artifacts. Keep feature/profile choices
stable; different worktree paths and compilation modes can still leave multiple
workspace artifacts in a shared target. Serialize checks sharing that target.

The two entrypoints hold the same target-specific maintenance lock for the
entire foreground command. A build waits if another wrapper command holds it;
a sweep, including `--dry-run`, refuses immediately when it is busy. The parent
retains the lock while `host-build -- cargo` runs, even if the host launcher
closes inherited file descriptors. With a controlling terminal, commands keep
the existing session and foreground group: terminal Ctrl-C already reaches
them, while the supervisor forwards TERM/HUP to its direct command and waits.
Without a terminal, it forwards INT/TERM/HUP to a separate command group.
Keep `.zeroclaw-maintenance.lock` in place; deleting it can create two
independent locks.

This lock covers these entrypoints, not bare Cargo, direct test executables or
background processes launched outside them. The sweep retains its process
check as a best-effort guard, but that check alone cannot close a startup race.
Killing only a supervisor or leaving detached descendants is outside this
coordination guarantee; confirm remaining children have stopped before sweeping.
Custom aliases that redirect build output and
separate build-directory layouts are unsupported. Simple one-line overrides
such as `--config net.offline=true` work; config files, quoted/escaped keys, and
`build`/`alias` overrides are refused. Select output with `CARGO_TARGET_DIR` or
`--target-dir` instead.

At delivery-batch closeout or when disk pressure requires it, inspect the
retained targets and preview a sweep. Keep the active batch's workspace cache
between edit/test cycles; sweeping after every command forces it to rebuild.

```sh
scripts/dev/target_sweep.sh --dry-run
scripts/dev/target_sweep.sh
# For a target explicitly selected during the build, use that same path:
CARGO_TARGET_DIR=../batch-target scripts/dev/target_sweep.sh --dry-run
```

Run the mutating sweep only when no build or executable uses the target. It
removes workspace artifacts and retains third-party dependencies, including
dependency names containing `zeroclaw` and names colliding with test targets.
Cleanup stays an explicit maintenance operation, never a build exit hook.
Inventory independent targets separately; do not assume the robot or another
toolchain shares this cache. Keep third-party dependencies even in retired
targets until an explicit cache retirement policy permits their removal; do
not use `cargo clean` or bypass Cargo cache-tag guards. Retain non-build receipts
outside the target.

## Picking a level for a new test

1. Testing one subsystem in isolation? → `tests/component/`
2. Testing multiple components wired together? → `tests/integration/`
3. Testing full message flow end to end? → `tests/system/`
4. Needs a real provider or API key? → don't add it to the suite. Mock the provider with `tests/support/`, and note any manual check against the real service in the PR's testing section.

After creating the file, add it to the level's `mod.rs` and use shared infrastructure from `tests/support/`.

## Shared infrastructure

Every test binary includes `mod support;`, making the shared mocks available as `crate::support::*`.

| Module | Contents |
|---|---|
| `mock_model_provider.rs` | `MockModelProvider` (FIFO scripted), `RecordingModelProvider` (captures requests), `TraceLlmModelProvider` (JSON fixture replay) |
| `mock_tools.rs` | `EchoTool`, `CountingTool`, `FailingTool`, `RecordingTool` |
| `mock_channel.rs` | `TestChannel` (captures sends, records typing events) |
| `helpers.rs` | `make_memory()`, `make_observer()`, `build_agent()`, `text_response()`, `tool_response()`, `StaticRecallMemory` |
| `trace.rs` | `LlmTrace`, `TraceTurn`, `TraceStep` types + `LlmTrace::from_file()` |
| `assertions.rs` | `verify_expects()` for declarative trace assertion |

Typical usage:

```rust
use crate::support::{MockModelProvider, EchoTool, CountingTool};
use crate::support::helpers::{build_agent, text_response, tool_response};
```

## JSON trace fixtures

Trace fixtures are canned LLM response scripts stored as JSON files in `tests/fixtures/traces/`. They replace inline mock setup with declarative conversation scripts, much easier to read and edit than `mockall` chains.

How it works:

1. `TraceLlmModelProvider` loads a fixture and implements the `ModelProvider` trait.
2. Each `provider.chat()` call returns the next step from the fixture in FIFO order.
3. Real tools execute normally (`EchoTool` actually processes its arguments).
4. After all turns, `verify_expects()` checks declarative assertions.
5. If the agent calls the provider more times than there are steps, the test fails.

Fixture format:

```json
{
  "model_name": "test-name",
  "turns": [
    {
      "user_input": "User message",
      "steps": [
        {
          "response": {
            "type": "text",
            "content": "LLM response",
            "input_tokens": 20,
            "output_tokens": 10
          }
        }
      ]
    }
  ],
  "expects": {
    "response_contains": ["expected text"],
    "tools_used": ["echo"],
    "max_tool_calls": 1
  }
}
```

Response types: `"text"` (plain text) or `"tool_calls"` (LLM requests tool execution).

Expects fields: `response_contains`, `response_not_contains`, `tools_used`, `tools_not_used`, `max_tool_calls`, `all_tools_succeeded`, `response_matches` (regex).

## Database tests are integration tests

Don't mock SQLite for tests that exercise schema or SQL; integration tests must hit a real database. The mock-passes-but-prod-fails class of bug is real and we've eaten it before.

## Manual tests

`tests/manual/` holds scripts for human-driven testing that can't be automated via `cargo test`. Run them directly. Channel-specific manual smoke tests live under `tests/manual/<channel>/`.
