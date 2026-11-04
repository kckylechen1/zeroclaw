# CI & Actions

Workflow entrypoints live in `.github/workflows/`. This page describes the fork's source policy and how to verify its coverage. GitHub branch protection, rulesets, Actions permissions, and environment approvals are separate cloud settings; editing this page or a workflow does not save those settings. CI policy and cloud alignment are tracked in [#552](https://github.com/kckylechen1/zeroclaw/issues/552).

## Automatic workflows

### Quality Gate (`ci.yml`)

The workflow starts on PRs targeting any branch, pushes to `master`, and `merge_group`. It stays unconditional at the workflow level so a required check is always reported. Supporting the merge-group event does not enable or imply a configured merge queue.

`Change Scope` runs first. `scripts/ci/change_scope.py` compares the checked-out result against the event's actual base:

| Event | Comparison base |
|---|---|
| Pull request | `pull_request.base.sha`, including a stacked PR's parent branch |
| Master push | The push's `before` SHA |
| Merge group | `merge_group.base_sha` |

Only ordinary, non-executable files in a narrow prose allowlist can skip the runtime matrix: root `README.md` and Markdown or MDX under `docs/book/src/`. Renames are evaluated as deletion plus addition. Mixed changes, paths outside that allowlist, symlinks, submodules, and executable-mode changes retain the full runtime lane. Runtime prompts, personality templates, fixtures, agent instructions, CI files, and build configuration do not become prose merely because their filename ends in `.md`.

An unavailable, invalid, empty, or unresolvable comparison retains full runtime validation and the real docs build. It never becomes an empty successful docs-only result.

| Selected work | Coverage |
|---|---|
| Every run | Scope-policy checks, workflow validation, `Docs Style`, and `CI Required Gate` |
| Full runtime lane | Rust formatting; workspace Clippy with `ci-all` and `-D warnings`; provider-dispatch guard; Linux build and macOS/Windows checks; no-default, minimal-companion, slim-control and i686 checks; four nextest partitions; dependency security; Web regressions/build; applicable repository, Nix, installer and targeted Windows checks |
| Docs Build | Real references, rustdoc, preprocessors, translation checks and all locales through `cargo mdbook build`, without publishing |

The four test partitions and their feature-specific runs come from `dev/ci/test-partition.json`. Formatting precedes the runtime fan-out; documentation checks depend on scope classification and run independently of Rust formatting.

Master pushes use the same classifier. A prose merge therefore skips the runtime matrix again instead of rebuilding it to warm caches. Full runtime pushes retain their selected build and cache-writing jobs; the repository-structure and Nix jobs remain PR/merge-group checks. Quality Gate and Code Analysis key push concurrency by commit so a later prose push cannot cancel or replace an earlier code push's work. Updated PR heads still cancel obsolete runs for that PR.

### Documentation coverage

`Docs Style` installs the pinned parser from `scripts/ci/docs-requirements.txt` and runs the document-gate regressions. `docs_quality_gate.sh` checks changed book/README lines for Markdown style and prose punctuation when a valid base is available. A missing or invalid base checks the full supported style scope and may expose existing issues.

`docs_links_gate.sh` checks the complete published book navigation, included sources, and local target paths, plus tracked repository Markdown outside the book. This catches old incoming links after a page or asset is renamed or deleted, including an unchanged README that links to it. Book-relative links must resolve within the published book; links to repository source outside that boundary need an explicit source URL.

The source gate reports the exact known generated pages and snippets it defers. It does not prove custom preprocessor output, rendered anchors, HTTP availability, or a successful publication build. HTTP links are collected without network requests.

`Docs Build` runs when navigation, book configuration, themes, translations, generator dependencies, or documentation CI/build inputs change. Changes to a book source containing preprocessor input in either the base or head also select the real build. The exact input list belongs to `change_scope.py`. Ordinary prose without those inputs uses the source checks and avoids workspace rustdoc.

### Aggregate and cloud protection

`CI Required Gate` uses `if: always()` and `scripts/ci/required_gate.py` to verify the complete dependency set, classifier outputs, and every expected job result. Failures, cancellations, missing results, and unexpected skips fail the gate. A skip is accepted only when the event and selected scope explicitly expect it.

`master` requires `CI Required Gate` from GitHub Actions and an up-to-date branch. This aggregate replaces the legacy leaf-check requirements; its expected-result policy still enforces every selected check. Repository merge settings allow squash merging only. Auto-merge is available, but it must be enabled separately for each PR and still waits for the applicable checks and review requirements.

Preserve linear history and the applicable review and bypass rules. Confirm the effective required checks and merge settings in GitHub before relying on this policy; settings can change independently of the source. See [Allowed actions and cloud verification](#allowed-actions) for read-only inspection.

Fresh required CI is normally the shared evidence for the Cargo surfaces it actually runs. A local rerun of the same Cargo command on the same head, target, and feature set is duplicate confidence, not a stronger proof. Before asking for extra Cargo or Clippy, compare the changed surface with the current workflow files and the actual checks on the PR. Extra validation belongs where the required gate does not prove the thing under review:

- a platform received compile checks but not tests;
- a platform, crate, or path is outside the required lint job;
- a desktop change did not trigger the desktop workflow;
- a release target is outside the PR matrix and only covered by release/manual workflows;
- stale, cancelled, skipped, or unavailable CI is not fresh evidence.

### Local pre-push checks

Follow the affected-surface commands in `AGENTS.md`: use `scripts/dev/cargo-local.sh` and the owning `scripts/dev/leg.sh <leg>` for Rust changes. Fresh hosted CI supplies the wider workspace evidence. Pure prose uses the docs gates without a local Cargo build.

For committed documentation changes, set `PR_BASE_SHA` from the verified PR metadata's `.base.sha`, including the parent branch of a stacked PR. Before opening a PR, use an explicitly chosen target commit. Refresh this value after restacking or retargeting, then pass it explicitly:

```bash
export BASE_SHA="${PR_BASE_SHA:?resolve the actual PR base SHA}"
git cat-file -e "${BASE_SHA}^{commit}"
python3 -m pip install -r scripts/ci/docs-requirements.txt
bash scripts/ci/docs_quality_gate.sh
bash scripts/ci/docs_links_gate.sh
```

The changed-line comparison uses committed `HEAD`. For uncommitted text, run Markdownlint directly on the edited files; do not present an empty committed diff as validation of those edits. The local-target pass reads the working tree. Omitting `BASE_SHA` requests the documented full-scan fallback.

The optional `.githooks/pre-push` entrypoint still contains a workspace test run and does not implement CI's classifier. Inspect its commands before enabling it; it is not the default validation prescription for a prose change. Its `ZEROCLAW_DOCS_LINT` and `ZEROCLAW_DOCS_LINKS` opt-ins inherit the explicitly exported base above.

### Dependency policy coverage

The full lane's security job runs `cargo deny check` from the root package with its default features and also runs `cargo audit`. The root manifest is a workspace manifest, but cargo-deny's command does not select every member as a graph root or enable every optional feature. Cargo Audit checks the lockfile advisory inventory; the two commands have different coverage. The exact-version skips in `deny.toml` apply within its selected graph; they do not identify a canonical newest version.

To diagnose the broader duplicate and wildcard policy without changing the lockfile, run:

```bash
cargo deny --locked --workspace --all-features check bans
```

The broader graph has known duplicate and wildcard failures and is not a passing required gate. Expanding enforcement requires a separate reviewed policy change; do not add exceptions merely to turn that diagnostic green. Daily `cargo deny check advisories` checks advisory policy on its selected graph, not duplicate-version bans. Neither its success nor a lockfile advisory audit proves that all-workspace bans pass.

Dependency-refresh PRs must disclose downgrades (or explicitly state none), newly added duplicate versions, and any deny exception changes with their reasons in the PR template. Compare package and source identities as well as versions: adding a parallel major is a new duplicate, not by itself a downgrade. Record unknown resolver causes as unknown.

### Code Analysis (`ci-code-analysis.yml`)

Uses the same diff classifier for static analysis. Semgrep runs on code/configuration PRs and master pushes; ordinary prose skips it. Semgrep remains report-only while findings are triaged. Rust CodeQL runs on full-scope master pushes and daily at 05:37 UTC, with scheduled scans retaining full coverage. The fork no longer skips CodeQL through an upstream-only repository condition.

CodeQL builds the workspace with `ci-all` and uploads successful analysis to GitHub's Security tab. It is separate from the required PR aggregate and does not add the deep build to every PR. Inspect the actual run when diagnosing analysis or upload failures; a source condition alone does not prove that analysis completed.

### Daily Advisory Scan (`daily-audit.yml`)

Runs `cargo deny check advisories` daily at 09:00 UTC against the dependency tree. Opens an issue on findings. No action unless a vulnerability is reported.

### Daily npm Audit (`daily-npm-audit.yml`)

Runs `npm audit --audit-level=high` daily at 09:23 UTC against `web/package-lock.json`. Opens one deduplicated `security` + `dependencies` issue when high-severity npm advisories affect the committed web lockfile.

### Weekly Trivy Image Scan (`trivy-scheduled.yml`)

Scans the published `dist` and `default-features` GHCR images every Saturday and uploads HIGH/CRITICAL findings to the Security tab as SARIF. The scan is report-first (`exit-code: 0` for findings), but a missing expected image fails the job before Trivy setup with the absent tag and the owning publisher workflow named in the error.

### PR Path Labeler (`pr-path-labeler.yml`)

Auto-applies path and scope labels based on changed files. It runs on PR open, reopen, and every pushed update to the PR branch. Because `sync-labels: true` is enabled, labels defined in `.github/labeler.yml` are recalculated from the current PR file set.

This workflow does not currently apply `risk:*`, `size:*`, `type:*`, contributor-tier, status, resolution, stale, or pickup labels. If a PR is missing a path/scope label, check whether the paths in `.github/labeler.yml` cover the changes.

Dependabot has separate label configuration in `.github/dependabot.yml` for its own PRs. Cargo update PRs start with `dependencies`; GitHub Actions and Docker update PRs start with `ci` and `dependencies`.

### Validate PR title (`pr-title.yml`)

Runs on every PR open/edit/synchronize. Runs the validator unit tests (`scripts/check-pr-title.test.sh`) and checks the PR title against Conventional Commits (`scripts/check-pr-title.sh`).

### Deploy mdBook docs to Pages (`docs-deploy.yml`)

Triggered on tag push, on `master` pushes that touch `docs/book/`, `xtask/`, `locales.toml`, or the deploy workflow itself, nightly (to refresh code-derived pages such as rustdoc), and on `workflow_dispatch`; builds and publishes versioned docs to the `gh-pages` branch. The PR's read-only `Docs Build` job is separate from this publisher. See [Release Runbook → Versioned documentation deployment](./release-runbook.md#step-7-versioned-documentation-deployment) for the version-floor and bootstrap rules.

### Docker Image PR Check (`docker-image-pr.yml`)

Runs only when Docker image or release-Docker context files change. It prepares a smoke `docker-ctx` with the same helper used by the stable release workflow, then builds the default prebuilt image and the Debian compatibility prebuilt image from `Dockerfile.ci` without pushing either image. This catches image dependency and `COPY` path breakage before release without giving PR runs registry write permission or running on every PR.

### Docker Publish (`docker-publish.yml`)

Builds, signs, and scans the generated four-variant matrix from `dev/ci/docker-tags.toml`: `minimal`, `default-features`, `dist`, and `all-features`. A human-created `v*` tag starts this workflow directly. A stable release started with `workflow_dispatch` creates its tag with `GITHUB_TOKEN`, which does not emit another tag-push event, so `release-stable-manual.yml` calls Docker Publish synchronously at the immutable release tag after the canonical release and Docker jobs succeed.

This matrix supplements rather than replaces the stable release's prebuilt `latest`, versioned, and `debian` images. The two paths use different build inputs and publish distinct tags.

## Manual and Advisory Workflows

### Monthly Outdated Scan (`monthly-outdated.yml`)

Scheduled monthly scan on the 1st of every month at 09:00 UTC. Runs `cargo outdated --workspace --exclude rusqlite,matrix-sdk` across all workspace members. Opens a `dependencies`-labeled issue when stale deps are found. Permissions: `contents: read` + `issues: write`. Dedup guard prevents piling up if the previous issue is still open.

The scanner remains pinned to `cargo-outdated@0.19.0`. Its hypothetical latest-version graph preserves the declared direct `rusqlite` and `matrix-sdk` constraints. `matrix-sdk-sqlite` shares the workspace `rusqlite` SQLite links graph, so bumping either crate alone can force an incompatible `libsqlite3-sys` selection; the two move as one coordinated review. This is a coverage limit: latest versions outside those two constraints are deferred, and other transitive choices obey the constrained graph. The report records this limit; no checked-in dependency or vendored security floor is changed. Exit `0` means a completed scan with no findings; `10` means a completed outdated inventory and is the only status allowed to create or reuse an issue. Every other exit status is a scanner failure. Findings and scanner failures both fail the workflow. Resolver failures are not evidence that the checked-in dependency graph is broken.

Every scan retains `outdated-output.txt` as a workflow artifact, including clean scans, scanner failures and existing-issue reuse. The report includes the exact head, runner image, toolchain and scanner versions, raw combined output and exit classification. Issue bodies include at most the first 48,000 report characters plus framing, below GitHub’s 65,536-character limit; longer inventories point back to the complete retained artifact. Run `bash scripts/ci/monthly_outdated.test.sh` for isolated command-stub coverage; it does not install a scanner or contact GitHub.

First triage step for a new issue: check if the reported outdated crates have semver-incompatible bumps and whether the consuming crate's API changed. If the bump is trivial (patch/minor), create a short dep-only PR. If the upgrade is blocked by semver breaks, close the issue with a note and the blocking crate name.

### Cross-Platform Build (`cross-platform-build-manual.yml`)

Manual trigger for building release binaries across the full target matrix: Linux x86_64/aarch64 GNU plus armv7 and arm hard-float, macOS Intel/ARM, Windows x86_64, and `aarch64-linux-android` (built with the NDK). Use this to verify a branch compiles cleanly on non-Linux targets before tagging.

### Cross-Platform Clippy (`cross-platform-clippy.yml`)

Runs on PRs changing `crates/zeroclaw-runtime/`, `crates/zeroclaw-infra/`, or this workflow, as well as manual dispatch and the weekly schedule. Each selected run checks the workspace's targets on macOS aarch64 and Windows x86_64 with `ci-all` and `-D warnings`. This catches test-only imports and conditional-compilation errors that a normal platform `cargo check` does not cover.

Ordinary prose PRs do not trigger this workflow. It remains separate advisory coverage outside `CI Required Gate`; inspect its current-head results when the changed surface needs it. The Linux i686 and minimal-companion checks remain in the main workflow.

### Release Stable (`release-stable-manual.yml`)

Manual trigger for the full release pipeline. Builds all targets, creates the GitHub Release, pushes the prebuilt `latest`, versioned, and `debian` Docker images to GHCR, calls the generated Docker variant matrix at the release tag, and triggers the website redeploy. Homebrew Core detects new releases through its own autobump service. The workflow declares `github-releases` and `docker` environments; their required reviewers and other protection settings must be verified in the target repository.

See the [Release Runbook](./release-runbook.md) for the full procedure.

Homebrew Core's
[official autobump service](https://docs.brew.sh/Autobump) discovers stable
GitHub releases and opens formula bumps independently. Do not restore a
project-owned Homebrew publisher or fork token; that duplicates Homebrew's
authoritative automation.

## Required secrets

| Secret | Used by |
|---|---|
| `WEBSITE_REPO_PAT` | `release-stable-manual.yml` (triggers the website repo redeploy) |
| `GITHUB_TOKEN` (automatic) | All workflows that push commits, open PRs, or push images to GHCR |

Docker images push to GHCR using the automatic `GITHUB_TOKEN`; there is no separate registry token. The release pipeline does not publish to crates.io, so no `CARGO_REGISTRY_TOKEN` is required.

## Build cache behavior

Rust compilation jobs in `ci.yml` use `Swatinem/rust-cache@v2`; scope checks, formatting, Nix and docs source checks do not compile the workspace. Cache behavior follows the jobs that actually run:

- **Main-gate Rust cache writes are master-only.** `save-if` is conditioned on `github.ref == 'refs/heads/master'`. PRs restore eligible caches without saving them. A prose-only master push skips the Rust jobs and does not refresh their caches.
- **Partial dependency caches may be saved.** The configured `cache-on-failure: true` preserves eligible artifacts from failed jobs when the action's save step can run; cancellation or timeout is not a promise of a saved cache.
- **Windows build cache is enabled.** The Windows build leg runs the same pinned Rust cache action as Linux and macOS. If Windows cache behavior flakes or regresses, revert the workflow change and document the failing restore/save evidence in the cache issue.
- **Incremental compilation is disabled.** `CARGO_INCREMENTAL: 0` at the workflow level. Incremental builds inflate cache size and produce non-reproducible artifacts under partial-stale conditions.
- **Tool installation downloads prebuilt binaries.** The `security` job downloads the pinned `cargo-deny` 0.19.9 Linux musl archive from its GitHub release; the `test` job downloads `cargo-nextest` from `get.nexte.st/latest/linux`. Both install steps run unconditionally, including after a Rust cache hit.

## When the gate goes red

| Symptom | First thing to check |
|---|---|
| `Release Stable` dies at `startup_failure` with zero jobs after a `uses:` ref changed | Check the run summary and the target repository's [Actions policy](#allowed-actions). A selected-actions rejection names a policy mismatch; a generic startup failure does not establish its cause |
| `CI Required Gate` red | Read `Change Scope` and the aggregate's expected-versus-actual result first, then the failing or unexpectedly skipped job |
| Prose change starts a Rust or docs build | Check both sides of the diff, file modes, actual event base, and whether the page contains preprocessor input |
| Docs source gate fails after a deletion or rename | Check unchanged incoming links, SUMMARY membership, include hosts, and repository README links |
| Release `validate` failed | `Cargo.toml` version doesn't match the workflow input, or the tag already exists |
| Release build leg failed | The specific target's job log. Android is `experimental` and runs with `continue-on-error` |
| Environment gate timed out | Re-run only the timed-out job from the workflow run page |
| Distribution publisher failed | Re-run the corresponding sub-workflow manually with `dry_run: true` first |

## Allowed actions

The source inventory below is not evidence that the target repository runs in `selected` mode or that these patterns have been saved to its allowlist. Inspect the effective cloud policy before diagnosing an action rejection or proposing a policy change.

All third-party refs are pinned to a full commit SHA with a trailing version comment; the version column below records that comment.

| Action | Used in | Purpose |
|---|---|---|
| `actions/checkout` (`v6.0.2`) | Most workflows | Repository checkout |
| `actions/cache` (`v4.2.3`, `v5.0.5`) | `docker-image-pr.yml` | Generic dependency and Trivy database caching |
| `actions/setup-node` (`v6.4.0`) | `release-stable-manual.yml`, `cross-platform-build-manual.yml` | Node toolchain for the web-dashboard build |
| `actions/upload-artifact` (`v7.0.1`) | `release-stable-manual.yml`, `cross-platform-build-manual.yml`, `docker-publish.yml`, `trivy-scheduled.yml`, `monthly-outdated.yml` | Upload build artifacts, Trivy SARIF handoff artifacts and monthly dependency scan reports |
| `actions/download-artifact` (`v8.0.1`) | `release-stable-manual.yml`, `cross-platform-build-manual.yml`, `docker-publish.yml` | Download build artifacts and Trivy SARIF handoff artifacts |
| `actions/labeler` (`v6.1.0`) | `pr-path-labeler.yml` | Apply path/scope labels from `.github/labeler.yml` |
| `dtolnay/rust-toolchain` (`stable`) | `ci.yml`, `release-stable-manual.yml`, `cross-platform-build-manual.yml`, `cross-platform-clippy.yml`, `daily-audit.yml`, `docs-deploy.yml` | Install Rust toolchain |
| `Swatinem/rust-cache` (`v2.9.1`) | `ci.yml`, `release-stable-manual.yml`, `cross-platform-build-manual.yml`, `cross-platform-clippy.yml`, `docs-deploy.yml` | Cargo build/dependency caching |
| `docker/setup-buildx-action` (`v3.11.1`, `v4.0.0`) | `release-stable-manual.yml`, `docker-publish.yml` | Docker Buildx setup |
| `docker/login-action` (`v3.4.0`, `v4.1.0`) | `release-stable-manual.yml`, `docker-publish.yml`, `trivy-scheduled.yml` | GHCR authentication |
| `docker/build-push-action` (`v6.18.0`, `v7.1.0`) | `release-stable-manual.yml`, `docker-publish.yml` | Multi-platform image build and push |
| `sigstore/cosign-installer` (`v3.8.1`) | `release-stable-manual.yml`, `docker-publish.yml` | Install cosign for keyless signing of release assets and container images |
| `anchore/sbom-action` (`v0.17.9`) | `release-stable-manual.yml` | Generate SPDX + CycloneDX SBOMs for each release |
| `slsa-framework/slsa-github-generator` (`v2.1.0`) | `release-stable-manual.yml` | Reusable workflow that produces SLSA L2 provenance for release artifacts |
| `aquasecurity/trivy-action` (`v0.36.0`) | `docker-image-pr.yml`, `docker-publish.yml`, `trivy-scheduled.yml` | Report-only container vulnerability scanning |
| `github/codeql-action/upload-sarif` (`v3.36.2`) | `docker-publish.yml`, `trivy-scheduled.yml` | Upload Trivy SARIF reports to the Security tab |
| `github/codeql-action/init` (`v3`) | `ci-code-analysis.yml` | Initialize CodeQL Rust analysis |
| `github/codeql-action/analyze` (`v3`) | `ci-code-analysis.yml` | Upload CodeQL SARIF to the Security tab |

The GitHub Release itself is created with `gh release create` inside the `publish` job, not a release action.

Candidate allowlist patterns for that source inventory, when `selected` mode is the approved policy:

```
actions/*
dtolnay/rust-toolchain@*
Swatinem/rust-cache@*
docker/*
sigstore/cosign-installer@*
anchore/sbom-action@*
slsa-framework/slsa-github-generator/.github/workflows/generator_generic_slsa3.yml@*
aquasecurity/trivy-action@*
github/codeql-action/upload-sarif@*
github/codeql-action/init@*
github/codeql-action/analyze@*
```

Bind inspection to the task's verified repository. For this fork, the read-only checks are:

```sh
TARGET_REPO=kckylechen1/zeroclaw
test "$(gh repo view "$TARGET_REPO" --json nameWithOwner --jq .nameWithOwner)" = "$TARGET_REPO" || exit 1
gh api "repos/$TARGET_REPO" \
  --jq '{allow_squash_merge, allow_merge_commit, allow_rebase_merge, allow_auto_merge}'
gh api "repos/$TARGET_REPO/branches/master/protection/required_status_checks"
gh api "repos/$TARGET_REPO/rules/branches/master"
gh api "repos/$TARGET_REPO/actions/permissions"
gh api "repos/$TARGET_REPO/actions/permissions/selected-actions"
```

A permission-denied or unavailable response leaves that setting unverified. It does not prove that a protection rule or allowlist is absent. Complete the readback with authorized repository administration access before changing the policy.

Any PR that adds or changes a `uses:` action source should state its allowlist impact. Match any authorized update to the verified missing action and read the setting back afterward. A source PR, a workflow comment, or this page does not constitute a saved cloud-policy receipt.

## Maintenance rules

- Keep `CI Required Gate` deterministic and small. A new job needs a clear quality argument and a matching expected-result policy.
- All third-party action refs must be pinned to a full commit SHA (per the allowlist policy above).
- Keep scope decisions and expected job results in `scripts/ci/`. Call shared helpers from workflows and local entrypoints instead of copying their decisions. Review changes to the classifier and aggregate together.
- Keep `scripts/ci/prepare_docker_context.sh`, `docker-image-pr.yml`, and the Docker job in `release-stable-manual.yml` aligned so PR validation exercises the same context shape the release workflow publishes.
- Run both docs gates with an explicit valid base before pushing committed documentation changes. Preserve the source gate's stated coverage limits and use the real docs build for selected rendering inputs.

## Emergency rollback

If an Actions policy blocks a critical run, identify the rejected action and verify the task repository first. Apply the smallest authorized policy correction or revert the affected workflow change. This page does not authorize switching the repository to unrestricted `all` mode. Record the incident, the exact policy delta, and the readback of the restored setting.
