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

The required web job runs `npm ci`, permission and review regressions, and
`npm run build` (typecheck plus Vite). Rust lint/build/test jobs use a
`web/dist/.gitkeep` placeholder so the gateway crate compiles without the
bundle. Producing a release artifact that includes the dashboard is a
separate step:

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

## Owner review on a phone-sized screen

The `/review` page uses the existing operator-only review, Soul and User Model
APIs. A token's presence does not grant review authority; an owner-only read
verifies access. The local recovery view checks `/health`: it offers code
entry only when pairing is enabled. With pairing disabled, the gateway cannot
issue a new operator token. The owner must enable `gateway.require_pairing`
in the gateway host configuration, apply it through the normal lifecycle,
then run `zeroclaw gateway get-paircode --new` on that host. The page provides
a check-again action; it does not change configuration or start services.
Existing authorized operator tokens still work with pairing disabled.

The inbox separates unapplied candidates from reflection receipts. Accept and
reword use the candidate's existing authority path; limiting a User Model
candidate binds it to the entered session. Dismissal applies no candidate.
A rejected User Model candidate can be narrowed once from its review history.
Soul proposals have no generic scope-narrowing action. Stale Soul proposals
show the backend reason and offer dismissal instead of another acceptance. Agent selection filters
Soul and reflection records, while the shared User Model retains its recorded
scopes. Evidence is displayed as text, never executed as HTML.

My Agent shows the four Soul layers, provenance and history. Restoring an older
Soul revision appends a new revision and includes the current revision for
conflict detection. About me shows active owner-authorized entries and the
candidate review history; there is no User Model rollback button. Effective
Voice and per-key sources are read from the gateway when available, without
recalculating precedence in the browser.

These pages add no background notification channel or offline record cache.
They do not establish PWA installability or closed-app delivery. Deploy the web
bundle alongside a gateway with the review APIs. Rolling back the web bundle
leaves canonical Soul and User Model records intact.

Run `npm run test:review`, the existing permission tests and `npm run build`
from `web/`. Validate the rendered owner entry, pending/empty/error states,
review actions and revision conflicts at 375px and desktop widths with
synthetic records before shipping. Desktop viewport tests do not prove a
physical phone or installed-service flow.

The browser regression fixture is `web/scripts/review-browser.test.cjs`. After
building, run it with an already-installed Playwright module selected by
`PLAYWRIGHT_MODULE` and optional Chrome executable selected by `REVIEW_CHROME`
(otherwise the installed Chrome channel is used). `REVIEW_SCREENSHOTS` chooses
the screenshot directory; it defaults to the system temporary directory. The
fixture launches an isolated context and synthetic localhost APIs, asserts
that pairing-disabled gateways never mint tokens, and tests stale proposals,
late history failures, rejected-candidate narrowing, focus and touch targets.
It installs no browser package and requires no production credentials.
