# Command-policy fuzz harness

`fuzz_command_validation` passes UTF-8 input to
`zeroclaw_config::policy::SecurityPolicy::validate_command_execution` with the
default policy and `approved = false`. Shell and cron execution use this same
policy implementation. The harness checks panic robustness of command parsing,
allowlisting, and risk validation; it does not execute commands or establish
correctness of the full approval and tool-dispatch path.

The former config, provider, webhook, and tool-parameter targets only parsed
generic TOML or JSON values. They did not exercise those application boundaries
and have been removed. Existing application regression tests remain necessary.

This is a standalone Cargo workspace. Root workspace checks do not include it.
It uses the existing `libfuzzer-sys` dependency and links the policy's owning
crate directly with default features disabled. Generated locks, corpora,
artifacts, coverage output, and build output stay local; record the resolved
dependency versions alongside any fuzzing results.

## Stable build and seed smoke

From the repository root, use the pinned toolchain and serialize Cargo work with
other checks in this worktree:

```sh
scripts/ci/toolchain_gate.sh
cargo fmt --manifest-path fuzz/Cargo.toml --all -- --check
cargo build --manifest-path fuzz/Cargo.toml --bin fuzz_command_validation
mkdir -p fuzz/corpus/fuzz_command_validation
printf '%s' 'ls' > fuzz/corpus/fuzz_command_validation/allowed
printf '%s' 'ls; rm -rf /' > fuzz/corpus/fuzz_command_validation/denied
fuzz/target/debug/fuzz_command_validation fuzz/corpus/fuzz_command_validation -runs=1000 -max_len=4096
```

These commands assume Cargo's default target directory; adjust the binary path
if `CARGO_TARGET_DIR` is set. A stable build checks compilation and linking. A
seed smoke run exercises the harness without sanitizer coverage instrumentation;
it is not evidence of coverage-guided fuzzing. No shell seed is executed.

## Instrumented fuzzing

On a prepared runner with `cargo-fuzz` and a suitable nightly toolchain already
installed, run from the repository root:

```sh
cargo +nightly fuzz build fuzz_command_validation
cargo +nightly fuzz run fuzz_command_validation -- -runs=1000 -max_len=4096
```

Add synthetic seeds containing quoted commands, separators, environment
assignments, and medium/high-risk commands. Record the exact nightly version,
target triple, resolved dependencies, command, exit status, and run count.
A bounded run is a smoke check, not a completed fuzzing campaign. These are
reproduction instructions, not a claim that either validation mode has passed.
