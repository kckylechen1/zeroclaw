# Claude Code Skills

The repo ships a set of [Claude Code skills](https://docs.claude.com/en/docs/agents/skills) under `.claude/skills/` that automate the heavier parts of the maintainer workflow: PR reviews, issue triage, squash-merging, changelog generation, and more.

Each skill lives in its own directory with a `SKILL.md` file. Claude Code loads them automatically when you open the repo; invoke them by describing what you want in plain language, or by explicit reference (e.g. `/squash-merge 1234`).

## Available skills

| Skill | Use it when |
|---|---|
| `github-pr-review-session` | Reviewing a specific PR or named queue: checks findings against source and publishes under the active account only when publication was authorized |
| `github-issue-triage` | Running a backlog sweep, closing stale/duplicate issues, applying labels, enforcing the canonical [issue stale policy](./labels.md#issue-stale-policy) |
| `github-issue` | Filing a structured issue (bug report or feature request) |
| `github-pr` | Opening or updating a PR with a fully-populated template body |
| `squash-merge` | Landing an authorized, ready PR into `master` by squash, with final-head matching and verified CI/review policy |
| `changelog-generation` | Preparing `CHANGELOG-next.md` for a release: summarises merges since the last tag |
| `skill-creator` | Creating, editing, or benchmarking the skills themselves |
| `zeroclaw` | Operating the running ZeroClaw instance (CLI + gateway API) |

## PR review workflow

The `github-pr-review-session` skill is the main tool for review days. A typical session looks like:

```
> review 1234
```

The skill reads `AGENTS.md`, the reviewer playbook, and the PR's diff + commits, then drafts a review. It uses:

- **Inline diff comments** for every 🔴 blocking, 🟡 warning, or 🔵 suggestion finding tied to a specific line
- **Review body** for overall verdict, comprehension summary, cross-references, and template-level issues not tied to a line
- **Bare commit hashes** (never wrapped in backticks: GitHub auto-links them)
- **@-prefixed usernames** in all review content

Findings follow the [feedback taxonomy](../contributing/pr-review-protocol.md#feedback-taxonomy): 🔴 [blocking] holds the PR, 🟡 [warning] should be addressed, 🔵 [suggestion] is optional, 🟢 [praise] names what works, and ✅ [resolved] acknowledges an addressed finding on re-review. The [PR Review Protocol](../contributing/pr-review-protocol.md) is canonical for the tiers and the review-body Markdown format.

The skill makes a concrete draft reviewable before publication. A review-only request stays read-only; public posting requires authorization for that action. Existing task-scoped publication authorization does not require a second yes/no exchange. The active authenticated account and reviewed head must be verified, and the review must not impersonate another person or claim human review that did not occur.

Both review and merge workflows resolve the task repository once and bind every write to it explicitly. A PR URL cannot override that binding. Upstream examples do not authorize writes to upstream, automatic milestone assignment, or messages to upstream maintainers. This fork follows the existing relationships in `#374`; it does not run a mandatory milestone-alignment phase after every review.

Re-review after changes:

```
> re-review 1234
```

Or work through the queue:

```
> go through the queue
```

## Issue triage workflow

The `github-issue-triage` skill runs autonomous backlog sweeps within defined authority bounds. With no argument it runs an **accounting** pass (backlog state, then prompts for a mode); otherwise the modes are:

- **Triage**: process issues with no triage labels: classify, apply labels, link to open PRs, flag thin bug reports, redirect security issues
- **Sweep**: full backlog pass in priority order (fixed-by-merged-PR → duplicates → `r:support` → stale candidates)
- **Stale**: apply the canonical [issue stale policy](./labels.md#issue-stale-policy) (`status:stale`, response window, exclusions, and re-engagement)
- **Won't-fix**: close issues that violate a named core engineering constraint, citing the constraint and its `AGENTS.md`/RFC reference
- **Single**: handle one issue by number or URL

Label definitions live in [Labels](./labels.md); the triage labels the skill applies (`r:needs-repro`, `r:support`, `stale-candidate`, the `status:*` lifecycle labels, and the resolution labels) are all defined there. Stale procedure lives in the issue-triage skill protocol, with reviewer-side context in [Reviewer playbook → Issue triage](./reviewer-playbook.md#issue-triage). The skill escalates ambiguity to the user before acting.

## Squash-merge strategy

Squash is this fork's landing policy. Verify that the live repository merge options match that policy; editing these documents does not change GitHub settings. The `squash-merge` skill uses GitHub's merge operation with a conventional subject, original commit provenance in the body, and a final-head precondition. A provenance list does not preserve the original commits as ancestry.

### Why the skill exists

The skill makes repository binding, authorization, final-head validation, and merge-result verification consistent. GitHub supports [configurable default squash messages](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/configuring-pull-request-merges/configuring-commit-squashing-for-pull-requests); using its defaults is acceptable when the resulting message meets project conventions. There is no blanket ban on the merge UI.

Automation uses `gh pr merge --match-head-commit --subject --body-file`, or an equivalent explicitly bound connector/API operation. Never direct-push a squash to `master` as a substitute for the PR merge process, and never assemble multiline public text as shell source.

### Format

- **Subject:** `<PR title> (#<number>)`: must be conventional commits with a scope (`feat(scope): ...`, `fix(scope): ...`).
- **Body (multi-commit PR):** bulleted list of `- <short sha> <commit subject>` from the PR branch
- **Body (single-commit PR):** full commit body, or blank if there isn't one

### Pre-flight checks

The skill verifies the bound repository/PR, final head and base, actual required checks, policy gate, mergeability, and required reviews. `REVIEW_REQUIRED` and unresolved substantive change requests are blockers, not prompts to bypass review. Pending CI is monitored while independent work continues. Routine conflict fixes are completed when authorized, followed by review and fresh checks; otherwise the concrete blocker is reported.

Validation follows `AGENTS.md`: docs gates with an explicit actual PR base for docs-only work, the shared Cargo wrapper and owning legs for code, and extra checks only for a named gap. Missing CLI tooling can use an equivalent authenticated API; it does not justify skipping safeguards. A failed or ambiguous merge call is followed by readback before any retry.

Dependent PRs may temporarily target their parent. After a parent squash, replay only the child's increment onto the updated `master`, retarget, inspect the final diff, and validate the resulting head. See [Stacked PRs](./pr-workflow.md#stacked-prs).

### Invocation

```
> squash-merge 1234
```

or explicit:

```
> /squash-merge 1234
```

The skill prepares the concrete merge result before acting. If the user already authorized the bounded review/repair/merge task, it proceeds without another ritual yes. A read-only request, new target/scope, permission change, or material risk exception needs its own decision; actual GitHub protections and substantive review requirements always remain in force.

## Changelog generation

`changelog-generation` builds `CHANGELOG-next.md` for a release by querying `gh` for merged PRs since the last tag, grouping them by conventional-commits prefix, and formatting them into the house changelog style. Use it as part of the release runbook, before dispatching `release-stable-manual.yml`.

## Editing the skills

Skills are plain Markdown with YAML frontmatter. Keep the name, description, and concrete trigger phrases (`"review 1234"`, `"triage issues"`, etc.) aligned with the workflow. An authoring guide can help with structure; edit the repository-owned files and run the relevant repository Markdown checks. A repo-policy edit does not require installing or changing a personal/plugin skill.

When a skill's behaviour diverges from the governing policy, update the skill and its references together. `AGENTS.md` and [PR Workflow](./pr-workflow.md) own this fork's repository binding, authorization, validation, and stack policy; skills implement that policy. The review protocol continues to own substantive evidence and review formatting. Repository workflow skills are versioned with this repository, not installed into a personal skill library as part of a repo-policy edit.
