# Building the web dashboard

The web dashboard at `web/` is a Vite + React + TypeScript app. Its API client (`web/src/lib/api.ts`) is hand-written against the gateway routes in `crates/zeroclaw-gateway/src/lib.rs`.

## Quickstart

<div class="os-tabs-src">

#### sh

```sh
cargo web build         # production bundle into web/dist/
cargo web dev           # vite dev server with HMR
cargo web check         # typecheck only (tsc -b)
cargo web install       # npm install in web/
```

</div>

`cargo web` is an alias for `cargo run -p xtask --bin web --` (defined in the cargo config). Every subcommand auto-runs `npm install` if `web/node_modules/` is missing.

## Editing flow

1. Change a gateway handler in `crates/zeroclaw-gateway/`.
2. Update the matching types and calls in `web/src/lib/api.ts` and `web/src/types/api.ts`.
3. Run `cargo web check` to typecheck the dashboard.
4. `cargo web build` for the final bundle into `web/dist/` (gitignored).

## CI and release builds

CI does not run `cargo web build`: the lint/build/test jobs use a `web/dist/.gitkeep` placeholder so the gateway crate compiles without the bundle. Producing a release artifact that includes the dashboard is a separate step:

<div class="os-tabs-src">

#### sh

```sh
cargo web build
cargo build --release --features gateway
```

</div>

The gateway loads `web/dist/` from the filesystem at runtime via `static_files.rs`, so the Rust compile and the web build are decoupled. Ship the populated `web/dist/` alongside the binary for installs that should serve the dashboard.

## Required tools

| Tool   | Install                                |
| ------ | -------------------------------------- |
| `npm`  | <https://nodejs.org/> or `nvm install && nvm use` from the repo root |
| `cargo`| <https://rustup.rs>                    |

The repo root `.nvmrc` pins the Node major version used by release web builds.
Use it for local dashboard work so `npm install`, `cargo web check`, and
manual release builds all run against the same Node line.

`cargo web` fails fast with an install hint if `npm` is missing.

## Supported browsers (minimum)

The dashboard targets evergreen browsers with support for both `color-mix()`
and `structuredClone()`.

- Chrome 111+
- Edge 111+
- Firefox 113+
- Safari 16.2+
