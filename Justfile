# Justfile - Convenient command runner for ZeroClaw development
# https://github.com/casey/just

# Default recipe to display help
_default:
    @just --list

# Format all code
fmt: toolchain
    cargo fmt --all

# Check formatting without making changes
fmt-check: toolchain
    cargo fmt --all -- --check

# Run clippy lints
lint: toolchain
    cargo clippy --all-targets -- -D warnings

# Run all tests
test: toolchain
    cargo test --locked

# Run only unit tests (faster)
test-lib: toolchain
    cargo test --lib

# Verify the active cargo/rustc match the rust-toolchain.toml pin
toolchain:
    ./scripts/ci/toolchain_gate.sh

# Run the full CI quality gate locally
ci: toolchain fmt-check lint test
    @echo "✅ All CI checks passed!"

# Build in release mode
build: toolchain
    cargo build --release --locked

# Build in debug mode
build-debug: toolchain
    cargo build

# Clean build artifacts
clean: toolchain
    cargo clean

# Run zeroclaw with example config (for development)
dev *ARGS: toolchain
    cargo run -- {{ARGS}}

# Check code without building
check: toolchain
    cargo check --all-targets

# Run cargo doc and open in browser
doc: toolchain
    cargo doc --no-deps --open

# Serve the docs site locally (English by default; pass LOCALE=ja for Japanese)
docs LOCALE="en": toolchain
    cargo mdbook serve --locale {{LOCALE}}

# Build the full docs site (all locales) to docs/book/book/
docs-build: toolchain
    cargo mdbook build

# Regenerate reference/cli.md, reference/config.md, and rustdoc API reference
docs-refs: toolchain
    cargo mdbook refs

# Sync .po files with English source; AI-fills delta if ANTHROPIC_API_KEY is set
docs-sync: toolchain
    cargo mdbook sync

# Sync a single locale (e.g.: just docs-sync-locale ja)
docs-sync-locale LOCALE: toolchain
    cargo mdbook sync --locale {{LOCALE}}

# Force-retranslate everything for a quality pass (costs more — use before a release)
# Optionally override model: FILL_MODEL=claude-opus-4-7 just docs-translate-force
docs-translate-force: toolchain
    cargo mdbook sync --force

# Force-retranslate a single locale
docs-translate-force-locale LOCALE: toolchain
    cargo mdbook sync --locale {{LOCALE}} --force

# Show translation status: translated/fuzzy/untranslated counts per locale
docs-translate-stats: toolchain
    cargo mdbook stats

# Validate .po format for all locales (exits non-zero on format errors)
docs-translate-check: toolchain
    cargo mdbook check

# Update dependencies
update: toolchain
    cargo update

# Run cargo audit to check for security vulnerabilities
audit: toolchain
    cargo audit

# Run cargo deny checks
deny: toolchain
    cargo deny check

# Format TOML files (requires taplo)
fmt-toml:
    taplo format

# Check TOML formatting (requires taplo)
fmt-toml-check:
    taplo format --check

# Run all formatting tools
fmt-all: fmt fmt-toml
    @echo "✅ All formatting complete!"
