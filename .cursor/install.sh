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
# Prefer nvm's Node over any system Node that may sit earlier on PATH.
hash -r
node --version
npm --version

# Persist PATH preference for later interactive / agent shells.
profile_snippet="$HOME/.cursor-cloud-path.sh"
cat >"$profile_snippet" <<EOF
export NVM_DIR="\$HOME/.nvm"
[ -s "\$NVM_DIR/nvm.sh" ] && . "\$NVM_DIR/nvm.sh"
nvm use --silent ${NODE_VERSION} >/dev/null 2>&1 || true
EOF
for rc in "$HOME/.bashrc" "$HOME/.profile"; do
  if [[ -f "$rc" ]] && ! grep -q 'cursor-cloud-path.sh' "$rc"; then
    printf '\n# ZeroClaw Cloud Agent PATH\n[ -f "$HOME/.cursor-cloud-path.sh" ] && . "$HOME/.cursor-cloud-path.sh"\n' >>"$rc"
  fi
done

echo "==> Web dashboard deps"
if [[ -f web/package-lock.json ]]; then
  (cd web && npm ci --ignore-scripts --no-audit --no-fund)
fi
mkdir -p web/dist
touch web/dist/.gitkeep

echo "==> Cargo registry warm"
# memcore (tachi) is a private optional git dep. Builds include it via
# repositoryDependencies; if the token still cannot see the rev, keep the
# toolchain/npm install successful and leave cargo fetch as best-effort.
if ! cargo fetch --locked; then
  echo "warning: cargo fetch --locked incomplete (private git deps may be missing)" >&2
  echo "warning: ensure github.com/kckylechen1/tachi is in repositoryDependencies and accessible" >&2
fi

echo "==> Cloud install complete"
rustc --version
node --version
command -v mold >/dev/null && mold --version | head -1
