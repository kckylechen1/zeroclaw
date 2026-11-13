# ZeroClaw Development Environment

Build and run ZeroClaw natively using the [platform setup guides](../docs/book/src/setup/index.md). The product container development environment has been retired; see the [migration guide](../docs/book/src/setup/container.md) for preserving existing state.

## Local CI in Docker

The local CI container provides a Rust toolchain for CI-style checks without requiring one on the host. From the repository root:

```bash
./dev/ci.sh build-image
./dev/ci.sh all
```

The full pipeline runs the Rust quality gate, workspace tests, the Docker build-context safety test, a release build, license/source checks and an advisory audit. Local tests use `cargo test`; the required GitHub gate uses `cargo nextest` and differs in scheduling, isolation and reporting.

Run individual stages as needed:

```bash
./dev/ci.sh lint
./dev/ci.sh lint-strict
./dev/ci.sh test
./dev/ci.sh test-manual
./dev/ci.sh build
./dev/ci.sh deny
./dev/ci.sh audit
./dev/ci.sh security
./dev/ci.sh shell
```

`./dev/ci.sh` lists all available stages. The repository is mounted at `/workspace`; Cargo artifacts and caches use container volumes. The root `.dockerignore` protects the local CI build context. The host-side documentation gates are `./scripts/ci/docs_quality_gate.sh` and `./scripts/ci/docs_links_gate.sh`.
