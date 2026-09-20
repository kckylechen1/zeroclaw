# Dependency Footprint Measurement

ZeroClaw measures a package dependency footprint as the target-specific,
feature-resolved closure of Cargo **normal and build dependencies** for the root
package. Dev dependencies and unrelated workspace members are outside this
metric. The metric says which packages a profile resolves; it does not measure
compile time, peak memory, target-directory size, binary size, provider-wire
tokens, or runtime resource use.

The profile registry is
`dev/ci/dependency-footprint.json`. It defines these root selections:

| Profile | Root selection | Purpose |
|---|---|---|
| `kernel` | `--no-default-features` | Canonical root with every default feature disabled |
| `minimal` | `--no-default-features --features agent-runtime` | Lean companion agent without default channels or optional integration families |
| `default` | Cargo defaults | Standard compatibility profile |
| `full` | Cargo defaults plus `--features ci-all` | The feature profile exercised by repository-wide CI |
| `*-explicit` | Minimal plus one existing family feature | Positive controls for email, SaaS, hardware-tool, and Discord feature propagation |

Run the measurement from the repository root with an explicit target triple:

```bash
python3 scripts/ci/dependency_footprint.py \
  --target x86_64-unknown-linux-gnu \
  --output /tmp/zeroclaw-dependency-footprint.json
```

The command invokes locked, offline `cargo metadata` and `cargo tree` queries.
The report records every selected package's name, version, source or workspace
manifest, and resolved features, plus a normalized closure digest. The whole
`Cargo.lock` SHA-256 binds registry checksums and the complete lock selection.
Source HEAD, tracked dirty state, dependency-input dirty state, and pinned
Cargo/rustc versions record the measurement provenance.
Run `scripts/ci/dependency_footprint.test.sh` to exercise the parser without
invoking Cargo.

`cargo metadata` describes all workspace packages and can show features unified
through a workspace member's dev dependency even when that dev edge is not in
the root build. The measurement therefore takes package membership and resolved
features from root-scoped `cargo tree --edges normal,build`. Metadata supplies
the exact package source only. The self-test includes a deliberately
polluted metadata feature to keep this separation enforceable.

## Integration census

This census separates compile selection from runtime admission. A feature can
remove code without removing a package when the implementation uses dependencies
that the core already needs.

| Family | Owning module and feature edge | Default build | Default tool-visible | Distinct heavy dependency closure | Extension plane | Compatibility |
|---|---|---|---|---|---|---|
| Email search/read and Email Channel | `zeroclaw-tools::email_{imap,read,search}`; tools/runtime `email-tools`; root/channels `channel-email` | Yes | No with no enabled Email channel; full composition can register the tools when configured | `async-imap`, `lettre`, `mail-parser`, and their transitive closure | Channel plus optional first-party tools | Defaults retain the existing surface; minimal omits it; `channel-email` explicitly restores it |
| Jira, Notion, Google Workspace, Microsoft 365, LinkedIn, Composio, Pushover | Tool modules and runtime registration under `integrations-saas` | Yes | Pushover is assembled in full composition; the other tools require their existing config or credentials | None unique: this feature owns no dependency edge and uses already-required HTTP/runtime crates | Optional first-party tools; MCP or skills remain available for external integrations | Config still parses when omitted and enabled-but-uncompiled families log a skip; defaults retain the family |
| Hardware board info and memory tools | Tool modules under `hardware-tools`; construction belongs to `zeroclaw-hardware` under `hardware` or `probe` | Types compile by default; hardware backend does not | No without explicit hardware backend/tool assembly | None from `hardware-tools`; `probe-rs` remains separately optional under `probe` | Hardware/Node capability plane | Default type availability remains; backend activation is explicit |
| Discord Channel and `discord_search` | Channel modules under `channel-discord`; archive search is a shared-memory tool registered only for an archive-enabled Discord config | Channel yes; search implementation compiles with the agent runtime | No with default config; minimal composition excludes `discord_search` | None unique: `channel-discord` owns no dependency edge and uses shared HTTP/WebSocket/memory crates | Channel | Defaults retain Discord; custom no-default builds opt into `channel-discord` |

Email is the useful package-graph discriminator: minimal must exclude its three
direct heavy packages, while both the default and explicit Email profiles must
restore them and the matching crate-local features. The other current families
are already compile-gated but do not own a removable package family. Moving
their code behind another feature would not reduce this dependency metric.

## Reference snapshot

With `Cargo.lock` SHA-256
`d8eb6032a5b7f054a1f4dd269c00f548f7649307797c588390000f4593853afc`,
the locked/offline measurement produced this target-specific snapshot:

| Profile | Linux x86_64 packages | macOS arm64 packages | Package delta from target's minimal |
|---|---:|---:|---:|
| `kernel` | 255 | 256 | Not compared; this profile omits the agent runtime |
| `minimal` | 291 | 294 | baseline |
| `default` | 330 | 333 | +39 |
| `full` | 743 | 743 | +452 Linux / +449 macOS |
| `email-explicit` | 318 | 321 | +27 |
| `saas-explicit` | 291 | 294 | 0 |
| `hardware-tools-explicit` | 291 | 294 | 0 |
| `discord-explicit` | 291 | 294 | 0 |

The report's package count includes the root package. Differences between the
two targets are expected because Cargo target predicates select different
normal/build edges. A zero package delta does not mean identical compiled code:
the resolved feature sets and closure digests still differ.
