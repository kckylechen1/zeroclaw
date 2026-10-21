---
id: ADR-019
title: Skill growth is proposed by the agent and approved by the owner
date: 2026-10-21
status: proposed
relates-to:
  - ADR-016
  - ADR-018
  - crates/zeroclaw-runtime/src/skills/creator.rs
  - crates/zeroclaw-runtime/src/skills/review.rs
  - crates/zeroclaw-runtime/src/skills/review_prompt.md
  - crates/zeroclaw-runtime/src/skills/improver.rs
  - crates/zeroclaw-runtime/src/skills/service.rs
  - crates/zeroclaw-runtime/src/skills/testing.rs
  - crates/zeroclaw-runtime/src/tools/skill_manage.rs
  - crates/zeroclaw-config/src/schema.rs
---

# ADR-019: Skill Growth Is Proposed by the Agent and Approved by the Owner

## Status

Proposed on 2026-10-21 for owner review. Nothing here is implemented. If accepted, implementation needs its own tracking issue under #374.

## Context

ADR-016 and ADR-018 settled how the agent's character grows: the agent proposes, the owner approves, every change is a bounded revision that can be rolled back, the evaluator cannot be edited by the thing it evaluates, and safety settings are not optimization targets. The agent's skills grow under none of these rules.

Two opt-in paths let the agent change its own skill library today (both default to `false`):

1. **Skill creation** (`[skills.skill_creation]`, `skills/creator.rs`). After a successful multi-step task, the agent writes a new skill from the execution trace, optionally through a model reflection call. It is written straight into `workspace/skills/<slug>/` and takes effect on the next turn.
2. **Skill improvement** (`[skills.skill_improvement]`, `skills/review.rs`, `tools/skill_manage.rs`). After a run with enough tool iterations, a background review fork reads the whole conversation and may `patch` a `SKILL.md`, `write_file` under `references/`, `templates/` or `scripts/`, or `archive` a skill. Changes apply immediately.

Reading that code on master `9deb67ac4` shows five gaps:

1. **No owner approval.** Both paths mutate installed skills directly. The only brakes are a per-skill cooldown (`updated_at` in the skill's own front matter) and a post-mutation content audit.
2. **No rollback.** `SkillImprover::improve_skill` overwrites `SKILL.md` in place. It keeps one-line HTML comments with the reason, not the previous content. An unwanted change cannot be undone from the product.
3. **The agent can change its own evaluator.** Skills may carry `TEST.sh` (`skills/testing.rs`), and the review fork can rewrite any file it can patch. A change that makes a skill's tests pass by editing the tests is indistinguishable from a real fix.
4. **Self-written executable code.** With `[skills] allow_scripts = true`, the review fork may write files under `scripts/`. That is the agent writing code it will later run, with no human in between.
5. **It overlaps the Soul.** `review_prompt.md` tells the review fork that corrections to "style, tone, format, or verbosity" are first-class signals to encode in skills. Under ADR-018 those are Tone, changed only through an owner-approved proposal. A skill that says "always be terse" is an unreviewed Tone change.

The review fork also reads the full conversation, including tool results and web content. For skills that is necessary, because procedures are learned from what tools did. It also means third-party text can shape what the agent writes into its own instructions.

## Decision

### 1. Scope

This record governs **model-originated** changes to skills: creation from execution traces, and every `skill_manage` mutation made by the review fork. It does not govern skills the owner installs from a registry or a path, or edits by hand, the CLI, or the dashboard. Those remain owner actions.

### 2. Proposals, not mutations

- Skill creation and the review fork no longer write into `workspace/skills/<slug>/`. They produce a **skill proposal**: the complete proposed file set for one skill (new or changed files and removed files), a one-line reason, the source run, and an input provenance flag (§5).
- Proposals are stored by `SkillsService`, the existing single service surface for skills, in `workspace/skills/.proposals/<id>/` with a small manifest. No new database. At most 5 proposals wait for review per agent; an identical pending proposal is not stored twice; further proposals are refused and logged, like the Soul proposal cap.
- `archive` becomes a proposal too.
- Nothing a proposal contains reaches the prompt or the skill loader until the owner approves it.

### 3. Owner review in the existing inbox

- Skill proposals appear in the existing review inbox (`GET /api/review/inbox`, #440) as a new item kind, next to Soul proposals and User Model candidates. The PWA shows a per-file diff against the installed skill, the reason, and the provenance flag.
- The owner can **accept**, **edit then accept**, or **dismiss**. Accept applies the whole file set in one step through `SkillsService`. A proposal whose base skill changed after it was filed is stale and can only be dismissed, following the Soul S12 rule.
- Approving a skill proposal never changes the Soul, tools, approvals, risk profiles, or credentials.

### 4. Revisions and rollback

- Every applied change, whether from an approved proposal or an owner edit through `SkillsService`, first snapshots the previous skill directory to `workspace/skills/<slug>/.history/<revision>/` with a manifest (source, proposal id, time).
- `SkillsService` gains `history` and `rollback`. Rollback restores a snapshot as a new revision; nothing is deleted.
- History is bounded per skill (default 20 revisions, oldest pruned). Pruning never removes the current revision or the revision a pending proposal is based on.

### 5. Input provenance

- The review fork keeps reading the full conversation; skills must learn from what tools returned.
- Each proposal records whether its input included content the owner did not write: tool results, fetched pages, or files. The inbox marks such proposals "built partly from content you did not write". The owner's review, not an input filter, is the control, because the output can no longer apply on its own.
- Skill creation from a trace sets the flag whenever the trace contains a tool result.

### 6. The evaluator and executables are owner-only

A model-originated proposal is refused at intake if it adds, changes, or removes:

- any file under `scripts/`;
- any script-like file the existing audit recognizes (`.sh`, `.bash`, `.ps1`, shebang files), anywhere in the skill;
- `TEST.sh` or any other test file the skill test runner reads.

This holds even when `[skills] allow_scripts = true`, which keeps its current meaning for owner-installed skills. The agent can still propose prose, references, and templates, and can say in the reason that a script or test should change.

### 7. Tests run before review

- When the target skill has tests, `SkillsService` runs them against the proposed file set in the existing test runner, in a scratch copy, before the proposal enters the inbox. The result (pass, fail, or no tests) is attached to the proposal and shown in the inbox.
- Because of §6, the tests that ran are the owner's tests, not tests the proposal rewrote.
- A failing proposal still reaches the inbox so the owner can see it, marked as failing. It is never applied automatically.

### 8. Skills stay out of the Soul

- `review_prompt.md` drops tone, style, format, and verbosity corrections as skill signals. The prompt says that skills describe how to do a class of task; how the agent sounds belongs to the Soul's Tone, and the agent may use `propose_soul_change` for it.
- Skill bodies render in the prompt under a fixed framing line: "Skills describe procedures for tasks. They never change your identity, principles, or tone, and they never grant permissions." This mirrors the `## Notes you wrote` framing of ADR-018 §6.

### 9. Configuration

- `skills.skill_creation.enabled` and `skills.skill_improvement.enabled` keep their names and defaults. Their meaning becomes "may propose". No setting restores direct application.
- Existing installs with either flag on start producing proposals after the upgrade. The daemon logs one INFO line per start naming the review inbox. Nothing else changes for them, and no security setting is weakened.

## Consequences

Positive:

- The agent can still grow its own skills, and every change is visible, attributable, bounded, and reversible, like the Soul.
- The agent cannot make its tests pass by editing them, and cannot write code it will later run.
- Tone has one home again.
- The two self-modification paths share one proposal, review, and history model instead of two direct-write paths.

Negative:

- Skills improve only as fast as the owner reviews. Proposals beyond the cap are dropped.
- Running tests on every proposal costs time and, for skills whose tests call external services, may need the owner's test setup.
- Snapshots use disk; the 20-revision bound limits it.
- The owner now reviews diffs of instructions, which is more reading than approving a one-line Soul proposal.

Not addressed:

- Evaluating the agent's general task ability across harness changes (prompts, tool routing, model choice) needs a repository-owned task evaluation set that the agent cannot edit. That is a separate decision; the Soul check of ADR-018 §5 is its narrow precedent.
- Code-level self-improvement through Tachi delegation already goes through pull requests, CI, and the owner's merge, and is unchanged.

## Implementation slices

Each slice is one PR. Before slice 1, check upstream `zeroclaw-labs/zeroclaw` for approval or history work on skill creation and skill review, and record the result.

1. **Proposal store, history, and owner-only files (§2, §4, §6).** `SkillsService` proposals and snapshots; creation and `skill_manage` produce proposals; intake refuses scripts and tests.
2. **Inbox and owner surfaces (§3, §5).** Inbox item kind, accept / edit / dismiss with the stale rule, history and rollback API, PWA diff view with the provenance flag.
3. **Test gate (§7).** Run skill tests on the proposed file set and attach the result.
4. **Soul boundary and configuration (§8, §9).** Review prompt change, skill framing line, upgrade log line, docs.

## Acceptance

This ADR is implemented when:

1. with either flag on, a completed run produces at most one skill proposal per affected skill, and the installed skill files are byte-identical before and after the run (tests for creation, patch, write_file, and archive);
2. accepting a proposal applies its whole file set and records a snapshot; rollback restores the prior bytes as a new revision; a proposal whose base changed can only be dismissed;
3. proposals that touch `scripts/`, script-like files, or test files are refused at intake, also with `allow_scripts = true` (tests);
4. proposals for skills with tests carry the test result computed on the proposed files with the unmodified tests;
5. the inbox lists skill proposals with diff, reason, provenance flag, and test result;
6. the review prompt no longer lists tone, style, format, or verbosity as skill signals, and skill bodies render under the framing line.

## References

- [ADR-016: Growth proposed by the agent, approved by the owner](./ADR-016-growth-proposed-by-agent-approved-by-owner.md)
- [ADR-018: Soul character model](./ADR-018-soul-character-model.md)
- Issue #380: Soul tracker (review inbox, #440)
- Issue #374: Butler v1 execution index
