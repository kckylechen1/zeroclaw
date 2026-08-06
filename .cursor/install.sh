#!/usr/bin/env bash
# Idempotent Cloud Agent install for ZeroClaw Builds.
# Keeps toolchain pins aligned with CI (.github/workflows/ci.yml, .nvmrc).
set -euo pipefail

cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)"

RUST_TOOLCHAIN="${ZEROCLAW_RUST_TOOLCHAIN:-1.96.1}"
NODE_VERSION="${ZEROCLAW_NODE_VERSION:-24}"

echo "==> ZeroClaw cloud install (rust=${RUST_TOOLCHAIN}, node=${NODE_VERSION})"

echo "==> System packages"
sudo apt-get update -qq
sudo apt-get install -y --no-install-recommends \
  build-essential \
  ca-certificates \
  clang \
  curl \
  g++ \
  gettext \
  git \
  libssl-dev \
  libudev-dev \
  mold \
  pkg-config \
  ripgrep

echo "==> Rust toolchain ${RUST_TOOLCHAIN}"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain "${RUST_TOOLCHAIN}"
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

rustup toolchain install "${RUST_TOOLCHAIN}" --profile default \
  --component rustfmt --component clippy
rustup default "${RUST_TOOLCHAIN}"
rustc --version
cargo --version

echo "==> Node ${NODE_VERSION}"
export NVM_DIR="${NVM_DIR:-$HOME/.nvm}"
if [[ ! -s "$NVM_DIR/nvm.sh" ]]; then
  curl -fsSL https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.3/install.sh | bash
fi
# shellcheck disable=SC1091
source "$NVM_DIR/nvm.sh"
nvm install "${NODE_VERSION}"
nvm alias default "${NODE_VERSION}"
node --version
npm --version

echo "==> Web dashboard deps"
if [[ -f web/package-lock.json ]]; then
  (cd web && npm ci --ignore-scripts --no-audit --no-fund)
fi
mkdir -p web/dist
touch web/dist/.gitkeep

echo "==> Cargo registry warm"
cargo fetch --locked

echo "==> Cloud install complete"
rustc --version
node --version
