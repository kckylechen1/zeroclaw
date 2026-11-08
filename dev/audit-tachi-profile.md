# Temporary F09 memory lookup profile

This isolated branch is a measurement artifact. Do not add this module to the
product PR or publish a new benchmark interface. The only production-file edit
registers an ignored test module under `cfg(test, feature = "tachi")`.

## Reproduce

This measurement branch is based on PR #604 production tree `cfe93b1a05bd26b83d7520d748c827601ae6131b` (published production commit `11651fc5ed7d232629ccc0429c15dde462430939`). It adds an ignored test module and this documentation only. Run it after any other Cargo command using the same target has completed:

```bash
# From an existing clone of kckylechen1/zeroclaw:
git fetch origin profile/audit-tachi-key-lookup-final
git switch --detach FETCH_HEAD
# Use the Rust 1.96.1 toolchain pinned by rust-toolchain.toml.
export ZEROCLAW_TACHI_PROFILE_OUTPUT="$PWD/tachi-key-lookup-profile.json"
export ZEROCLAW_TACHI_PROFILE_REVISION="$(git rev-parse HEAD)"
export ZEROCLAW_TACHI_PROFILE_TOOLCHAIN="$(rustc --version)"
CARGO_BUILD_JOBS=2 scripts/dev/cargo-local.sh test --locked -p zeroclaw-memory --features tachi --lib \
  tachi::key_lookup_profile::profile_1k_10k_50k -- \
  --ignored --exact --nocapture --test-threads=1
```

The required toolchain is the repository-pinned Rust 1.96.1. This command uses
the existing test profile, which inherits the repository's dev profile
(`opt-level = 1` for workspace code and `opt-level = 3` for dependencies), and
does not represent release-binary latency. It adds no dependency. Retain the
compiler output beside the JSON report so any profile/environment overrides
remain visible. A nonzero test exit or a JSON status other than `completed`
means the run is incomplete; do not present partial stages as a passing run.

The test prints one JSON measurement per case and checkpoints the complete
report after each corpus size. `TempDir` removes the synthetic database when
the test exits. The JSON output remains at the path selected by `ZEROCLAW_TACHI_PROFILE_OUTPUT`. The recorded audit run used harness commit `b9281a1ca25624ede383684825d61b61c779d51a`; this publication changes only these reproduction instructions, with the same Rust harness and product sources.

## Data and comparison

| Property | Definition |
|---|---|
| Current code | `74f865d900eee050f2586dd3a695d6086ec69df4` |
| Legacy point algorithm | K01 parent `0fc1e8fae36d05e0ce7efb010906ec1c7da1aa60` |
| Portable kernel | Pinned `b0c754a0cd3e4e7ffd0199691e08954942b492bd` |
| Store owner | `TachiMemory::new`, using its admitted `KernelStoreKind::Memory` handle |
| Corpus | Independent fresh stores of 1,000, 10,000 and 50,000 active rows; old/current strategies share the same admitted handle at each size |
| Row content | Exactly 1,024 synthetic ASCII text bytes, 100 summary bytes, realistic adapter metadata; no vectors |
| Seeding | Existing typed `MemoryStore::import_snapshot_batch`, one transaction for the whole fresh corpus; no repeated adapter new-key writes |
| Embedding | Existing `NoopEmbedding`; no network or embedding latency |
| Sampling | One warmup per strategy, nine measured samples per strategy, alternating old/current order |
| Cache | Warm; OS/SQLite caches are not flushed; no concurrent load is generated |

### Fixture loading and parity

The previous profile on the superseded canonical-shortcut candidate was
stopped with exit 130 before its 50k stage completed. Its partial 1k/10k
records do not describe this corrected code. This branch reruns the complete
measurement after preserving the legacy duplicate winner contract.

Ordinary typed `upsert_batch` still synchronizes each row's FTS projections,
including ordinary-column `id = ?` deletes. Combining transactions does not
remove that work. This profile therefore uses the pinned kernel's existing
portable snapshot-import API solely to load trusted synthetic fixtures. It is
not a new product write path or an untrusted import endpoint.

Every target is created by `TachiMemory::new`, with the existing
`PortableKernel` profile and `zeroclaw-memory` role. Import uses that live
handle, the same kernel path validation and reserved-write authorization;
store identity and hard state remain owned by admission. The kernel decides
inside its transaction whether the main table and both FTS projections are
empty, their schema is canonical, and their persistent triggers are admitted.
Only that proof permits its existing insert-only FTS path. No raw SQL,
connection, DDL, new index, profile override or skip-check option is used.
The entire corpus is one import batch because a second batch into a nonempty
target would use the ordinary synchronization path again.

Snapshot input retains the complete adapter metadata. Its source and time
fields use the canonical stored form of ordinary upsert. Before any timing,
eight representative fixtures pass through real `upsert_batch` and typed
`get` on a separate fresh control store under the same admission owner; every
serialized `MemoryEntry` field must equal its snapshot fixture. Any drift
fails the run rather than silently changing the input. After every import,
all N rows are read back through typed `get` and compared field for field,
including metadata. The returned receipt must also match input-derived
lifecycle/vector checksums, row counts, no vectors and no dangling
supersessions.

Synthetic `created_at` and `updated_at` are fixed to
`2026-01-01T00:00:00.000Z`; these lifecycle columns are not read by the point
algorithms. There are no supersession edges, vectors or access-history
events. Fixture creation, import, checksums and readback are outside all
operation timers. This is a synthetic point-operation comparison, not a
claim that snapshot import and ordinary application writes have the same
write latency or lifecycle behavior. Timed new writes and deletes still use
their real production methods, including the existing FTS mutation cost.

The legacy read calls the retained production `list_prefix_complete` with its
real 512-row seed and geometric growth, applies the pre-F09 logical-key filter,
then uses the same K01 `PointScope` rules. It uses the same live handle,
`spawn_blocking`, returned entry conversion and session scope as the old point
path. This compares the old query algorithm with current code in one binary;
it is not a claim about two separately compiled historical release binaries.

The current read calls the real `Memory::get_for_agent`. A case succeeds only
when both strategies return the expected row ID, key and content size, or both
return no row for the missing key. Validation happens after the timer stops.

| Case | Scope and purpose |
|---|---|
| `canonical_conversation_hit` | Matching canonical host session after the complete metadata projection |
| `canonical_global_without_host_scope` | Agent-scoped administrative call with no host session context; canonical global candidate |
| `global_in_host_scope_legacy_override_check` | Global key while a host session exists; complete candidate selection must check for a Conversation override |
| `noncanonical_legacy_conversation_hit` | Matching Conversation metadata stored under an opaque legacy path; same complete query as canonical rows |
| `new_key_miss_lookup_only` | Absent key; complete metadata projection without conflating lookup with a write |
| `forget_existing_global_in_host_scope` | Old full-prefix selection plus the real shared delete helper versus actual current `forget_for_agent`; the typed restore is outside timing |
| `new_core_write_current_only` | Actual current `store_with_agent` of an absent Core key; typed cleanup is outside timing; there is deliberately no synthetic old-write result |

The legacy and current delete cases each restore their fixture before the next
sample. The new-write case removes its fixture after every sample, so each
sample measures an absent key against exactly N background rows. The test
asserts the active corpus size before and after each stage.

## Reading the output

Each operation reports raw nanosecond samples plus minimum, median,
nearest-rank p95 and maximum milliseconds. With nine samples, this empirical
p95 equals the maximum. It is a small local profile, not a tail-latency SLO or
throughput benchmark; there is no predefined improvement threshold or ratio.

Each stage also includes:

- Actual typed-query diagnostics: every full-entry page requested by the old
  geometric scan, the returned text/metadata string bytes, the complete
  metadata projection's row count/string bytes, and actual matching-key
  full-entry hydration counts/text/metadata bytes.
- Logical seed sizes for text, summary and input metadata, with zero vector
  bytes. Input metadata size remains separate from returned-query diagnostics;
  full-field readback verifies the seed metadata was retained.
- Actual database, WAL and SHM file sizes before and after measured operations.
  These are storage sizes, not heap usage. Fresh-corpus preparation duration,
  import time and full-field verification time are separately recorded,
  never included in point-operation samples.

The diagnostic queries are untimed and use only the existing admitted typed
API. Returned row/string counts do not measure SQLite rows visited, allocation
overhead, peak RSS, or a query plan. There is no raw SQL, extra connection,
schema/DDL change, new index, cache, or store owner.

Exact metadata-key filtering reduces full-entry materialization, but the
typed query is not claimed to be an indexed O(1) lookup. Every point read and
delete, and writes missing the original exact-path overwrite probe, still
examine O(N) metadata to preserve complete legacy duplicate selection. A
sequence of distinct new-key writes can still incur O(N²) aggregate metadata
work. The profile must retain these qualifications even if the measured
latencies improve.
