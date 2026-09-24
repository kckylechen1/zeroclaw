# syntax=docker/dockerfile:1.7-labs

# >>> generated:base-arg-node from dev/ci/container-base-images.toml by `cargo generate installers` - do not edit <<<
ARG ZEROCLAW_BASE_NODE=node:24-bookworm-slim@sha256:3638d9a6fe4030bd716be989438248074489337ba3275657f93595428be4fc03
# >>> end generated:base-arg-node <<<
# >>> generated:base-arg-rust-slim from dev/ci/container-base-images.toml by `cargo generate installers` - do not edit <<<
ARG ZEROCLAW_BASE_RUST_SLIM=rust:1.96-slim@sha256:31ee7fc65186be7e0e0ccb3f2ca305f14e4739e7642a1ae65753aa5d7b874523
# >>> end generated:base-arg-rust-slim <<<
# >>> generated:base-arg-debian from dev/ci/container-base-images.toml by `cargo generate installers` - do not edit <<<
ARG ZEROCLAW_BASE_DEBIAN=debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258
# >>> end generated:base-arg-debian <<<
# >>> generated:base-arg-distroless from dev/ci/container-base-images.toml by `cargo generate installers` - do not edit <<<
ARG ZEROCLAW_BASE_DISTROLESS=gcr.io/distroless/cc-debian13:nonroot@sha256:d97bc0a941b8d4be647dc0ee75b264ddbb772f1ac5ba690a4309c00723b23775
# >>> end generated:base-arg-distroless <<<

# ── Stage 0: Frontend build ─────────────────────────────────────
# The web dashboard bundle is architecture-independent (JS/WASM), so the
# frontend tooling stages are pinned to the native build platform. On a
# multi-platform (`--platform linux/amd64,linux/arm64`) build this keeps node
# and `cargo web build` running natively instead of under QEMU emulation.
FROM --platform=$BUILDPLATFORM ${ZEROCLAW_BASE_NODE} AS web-node

FROM --platform=$BUILDPLATFORM ${ZEROCLAW_BASE_RUST_SLIM} AS web-builder
WORKDIR /app
COPY --from=web-node /usr/local/bin/node /usr/local/bin/node
COPY --from=web-node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y \
        g++ \
        pkg-config \
    && ln -s /usr/local/lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s /usr/local/lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx \
    && rm -rf /var/lib/apt/lists/*
COPY web/package.json web/package-lock.json web/
RUN cd web && npm ci --ignore-scripts
COPY . .
RUN --mount=type=cache,id=zeroclaw-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=zeroclaw-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=zeroclaw-web-target,target=/app/target,sharing=locked \
    cargo web build

# ── Stage 1: Build ────────────────────────────────────────────
# Pinned to the native build platform; the Rust toolchain runs on the host arch
# (amd64 on the GitHub-hosted runners) and cross-compiles to $TARGETARCH so
# rustc never runs under QEMU. TARGETARCH is injected by BuildKit per target
# platform (`amd64`/`arm64`).
FROM --platform=$BUILDPLATFORM ${ZEROCLAW_BASE_RUST_SLIM} AS builder

WORKDIR /app
ARG TARGETARCH
# >>> generated:docker-features-arg by `cargo generate installers` - do not edit <<<
ARG ZEROCLAW_CARGO_FLAGS="--no-default-features --features agent-runtime,channel-acp-server,channel-discord,channel-email,channel-lark,channel-matrix,channel-telegram,channel-webhook,gateway,hardware-tools,integrations-saas,observability-prometheus,schema-export,whatsapp-web"
# >>> end generated:docker-features-arg <<<

# Install build dependencies. The slim base ships cc but not a C++ compiler;
# g++ covers cc-crate-built C++ deps. For arm64 cross-builds, also install the
# aarch64 GNU cross toolchain (C and C++), the arm64 libc dev files, and the
# Rust target.
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update && apt-get install -y \
        pkg-config \
        g++ \
    && if [ "$TARGETARCH" = "arm64" ]; then \
        dpkg --add-architecture arm64 && apt-get update && apt-get install -y \
            gcc-aarch64-linux-gnu \
            g++-aarch64-linux-gnu \
            libc6-dev-arm64-cross \
        && rustup target add aarch64-unknown-linux-gnu; \
    fi \
    && rm -rf /var/lib/apt/lists/*

# 1. Copy manifests to cache dependencies
COPY Cargo.toml Cargo.lock ./
# Copy every workspace-member manifest in one glob — adding or removing a crate
# no longer requires editing this file.  --parents preserves the
# crates/<name>/Cargo.toml directory structure.
COPY --parents crates/*/Cargo.toml ./
# The plugin test fixture is a nested workspace member the glob above misses.
COPY --parents crates/zeroclaw-plugins/tests/fixtures/channel-fixture/Cargo.toml ./
# zeroclaw-macros is a proc-macro crate, compiled for the host even on a cross
# build. If only a stub lib.rs is present during the pre-fetch, its host-cached
# artifact is reused in the real build under the target-triple dir, leaving
# `zeroclaw_macros::Configurable` unresolved. Copy its real source now so the
# proc-macro is built from the genuine implementation during the pre-fetch.
COPY --parents crates/zeroclaw-macros/src/ ./
# tools/fill-translations and xtask are dev/build tools; copy manifests only so
# Cargo can resolve the workspace, then stub their entry points so the
# dependency pre-fetch step succeeds without building them into the image.
COPY tools/fill-translations/Cargo.toml tools/fill-translations/Cargo.toml
COPY xtask/Cargo.toml xtask/Cargo.toml
# Create dummy targets for all workspace members so manifest parsing succeeds.
RUN mkdir -p src benches tools/fill-translations/src xtask/src/bin \
    && echo "fn main() {}" > src/main.rs \
    && echo "" > src/lib.rs \
    && echo "fn main() {}" > benches/agent_benchmarks.rs \
    && echo "fn main() {}" > tools/fill-translations/src/main.rs \
    && echo "" > xtask/src/lib.rs \
    && echo "fn main() {}" > xtask/src/bin/mdbook.rs \
    && echo "fn main() {}" > xtask/src/bin/fluent.rs \
    && echo "fn main() {}" > xtask/src/bin/web.rs \
    && mkdir -p crates/zeroclaw-hardware/examples \
    && echo "fn main() {}" > crates/zeroclaw-hardware/examples/esp32_sim.rs \
    && for d in crates/*/; do [ "$d" = "crates/zeroclaw-macros/" ] && continue; mkdir -p "${d}src" && printf '' > "${d}src/lib.rs"; done \
    && mkdir -p crates/zeroclaw-plugins/tests/fixtures/channel-fixture/src \
    && printf '' > crates/zeroclaw-plugins/tests/fixtures/channel-fixture/src/lib.rs \
    && mkdir -p crates/zeroclaw-gateway/tests \
    && printf '' > crates/zeroclaw-gateway/tests/nodes_mdns.rs
RUN --mount=type=cache,id=zeroclaw-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=zeroclaw-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=zeroclaw-target,target=/app/target,sharing=locked \
    if [ "$TARGETARCH" = "arm64" ]; then \
      export RUST_TARGET=aarch64-unknown-linux-gnu \
             CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
             CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
             CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
             PKG_CONFIG_ALLOW_CROSS=1 \
             PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig; \
    else \
      export RUST_TARGET=x86_64-unknown-linux-gnu; \
    fi && \
    if [ -n "$ZEROCLAW_CARGO_FLAGS" ]; then \
      cargo build --release --locked --target "$RUST_TARGET" -p zeroclawlabs $ZEROCLAW_CARGO_FLAGS; \
    else \
      cargo build --release --locked --target "$RUST_TARGET" -p zeroclawlabs; \
    fi
RUN rm -rf src benches crates xtask tools/fill-translations

# 2. Copy only build-relevant source paths (avoid cache-busting on docs/tests/scripts)
COPY src/ src/
COPY benches/ benches/
COPY crates/ crates/
COPY xtask/ xtask/
COPY tools/fill-translations/ tools/fill-translations/
# locales.toml lives at repo root and is embedded by zeroclaw-runtime via
# include_str!("../../../locales.toml"); the real build needs it present.
COPY locales.toml .
COPY *.rs .
RUN touch src/main.rs
# Bust the stubbed workspace crates so the real sources rebuild.
RUN --mount=type=cache,id=zeroclaw-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=zeroclaw-cargo-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=zeroclaw-target,target=/app/target,sharing=locked \
    if [ "$TARGETARCH" = "arm64" ]; then \
      export RUST_TARGET=aarch64-unknown-linux-gnu STRIP=aarch64-linux-gnu-strip \
             CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
             CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
             CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
             PKG_CONFIG_ALLOW_CROSS=1 \
             PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig; \
    else \
      export RUST_TARGET=x86_64-unknown-linux-gnu STRIP=strip; \
    fi && \
    rm -rf target/"$RUST_TARGET"/release/.fingerprint/zeroclawlabs-* \
           target/"$RUST_TARGET"/release/deps/zeroclawlabs-* \
           target/"$RUST_TARGET"/release/incremental/zeroclawlabs-* \
           target/"$RUST_TARGET"/release/.fingerprint/zeroclaw-* \
           target/"$RUST_TARGET"/release/deps/zeroclaw_* \
           target/"$RUST_TARGET"/release/incremental/zeroclaw_* \
           target/"$RUST_TARGET"/release/.fingerprint/xtask-* \
           target/"$RUST_TARGET"/release/deps/xtask-* \
           target/"$RUST_TARGET"/release/.fingerprint/fill-translations-* \
           target/"$RUST_TARGET"/release/deps/fill_translations-* && \
    if [ -n "$ZEROCLAW_CARGO_FLAGS" ]; then \
      cargo build --release --locked --target "$RUST_TARGET" -p zeroclawlabs $ZEROCLAW_CARGO_FLAGS; \
    else \
      cargo build --release --locked --target "$RUST_TARGET" -p zeroclawlabs; \
    fi && \
    cp target/"$RUST_TARGET"/release/zeroclaw /app/zeroclaw && \
    "$STRIP" /app/zeroclaw
RUN size=$(stat -c%s /app/zeroclaw) && \
    if [ "$size" -lt 1000000 ]; then echo "ERROR: zeroclaw too small (${size} bytes), likely dummy build artifact" && exit 1; fi

# Prepare runtime directory structure and default config inline (no extra stage).
# Dashboard assets live at /usr/share/zeroclawlabs/web/dist (outside the documented
# /zeroclaw-data mount point) so a bind mount on /zeroclaw-data cannot shadow them.
RUN mkdir -p /zeroclaw-data/.zeroclaw /zeroclaw-data/data && \
    printf '%s\n' \
        'api_key = ""' \
        'default_provider = "openrouter"' \
        'default_model = "anthropic/claude-sonnet-4-20250514"' \
        'default_temperature = 0.7' \
        'composition = "minimal"' \
        '' \
        '[gateway]' \
        'port = 42617' \
        'host = "[::]"' \
        'allow_public_bind = true' \
        'require_pairing = false' \
        'web_dist_dir = "/usr/share/zeroclawlabs/web/dist"' \
        '' \
        '[risk_profiles.default]' \
        'level = "supervised"' \
        'auto_approve = ["file_read", "file_write", "file_edit", "memory_recall", "memory_store", "web_search_tool", "web_fetch", "calculator", "glob_search", "content_search", "image_info", "weather", "git_operations"]' \
        > /zeroclaw-data/.zeroclaw/config.toml && \
    chown -R 65534:65534 /zeroclaw-data

# ── Stage 2: Development Runtime (Debian) ────────────────────
FROM ${ZEROCLAW_BASE_DEBIAN} AS dev

# Install essential runtime dependencies only (use docker-compose.override.yml for dev tools)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    vim-tiny \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /zeroclaw-data /zeroclaw-data
COPY --from=builder /app/zeroclaw /usr/local/bin/zeroclaw
# Install the dashboard at /usr/share/zeroclawlabs/web/dist (outside the
# documented /zeroclaw-data mount) so user volumes do not shadow it (#6400).
COPY --from=web-builder /app/web/dist /usr/share/zeroclawlabs/web/dist

# Overwrite minimal config with DEV template (Ollama defaults)
COPY dev/config.template.toml /zeroclaw-data/.zeroclaw/config.toml
RUN chown 65534:65534 /zeroclaw-data/.zeroclaw/config.toml

# Environment setup
# Ensure UTF-8 locale so CJK / multibyte input is handled correctly
ENV LANG=C.UTF-8
# Bootstrap (uppercase tail) — pre-load: decides where the config file lives.
ENV ZEROCLAW_DATA_DIR=/zeroclaw-data/data
ENV HOME=/zeroclaw-data
# V0.8.0 env-var grammar: `ZEROCLAW_<dotted_path_with_double_underscores>=<value>`
# mirrors the TOML config 1:1; `__` is the path separator. Operators inject
# credentials and runtime knobs at `docker run -e ...` (or via docker-compose
# `environment:`). Legacy `PROVIDER`, `ZEROCLAW_MODEL`, `ANTHROPIC_API_KEY`,
# `API_KEY`, etc. fallbacks were eradicated. Example:
#   docker run -e ZEROCLAW_providers__models__anthropic__default__api_key=sk-ant-... ...
ENV ZEROCLAW_gateway__port=42617

WORKDIR /zeroclaw-data
USER 65534:65534
EXPOSE 42617
HEALTHCHECK --interval=60s --timeout=10s --retries=3 --start-period=10s \
    CMD ["zeroclaw", "status", "--format=exit-code"]
ENTRYPOINT ["zeroclaw"]
CMD ["daemon"]

# ── Stage 3: Production Runtime (Distroless) ─────────────────
FROM ${ZEROCLAW_BASE_DISTROLESS} AS release

COPY --from=builder /app/zeroclaw /usr/local/bin/zeroclaw
COPY --from=builder /zeroclaw-data /zeroclaw-data
# Install the dashboard at /usr/share/zeroclawlabs/web/dist (outside the
# documented /zeroclaw-data mount) so user volumes do not shadow it (#6400).
COPY --from=web-builder /app/web/dist /usr/share/zeroclawlabs/web/dist

# Environment setup
# Ensure UTF-8 locale so CJK / multibyte input is handled correctly
ENV LANG=C.UTF-8
ENV ZEROCLAW_DATA_DIR=/zeroclaw-data/data
ENV HOME=/zeroclaw-data
# Default provider and model are set in config.toml, not here,
# so config file edits are not silently overridden
#ENV PROVIDER=

# API_KEY must be provided at runtime!

WORKDIR /zeroclaw-data
USER 65534:65534
EXPOSE 42617
HEALTHCHECK --interval=60s --timeout=10s --retries=3 --start-period=10s \
    CMD ["zeroclaw", "status", "--format=exit-code"]
ENTRYPOINT ["zeroclaw"]
CMD ["daemon"]
