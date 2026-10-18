---
id: ADR-018
title: Soul character model - layer duties, exemplars, drift, and behavioral continuity
date: 2026-10-18
status: proposed
relates-to:
  - ADR-014
  - ADR-015
  - ADR-016
  - crates/zeroclaw-config/src/persona.rs
  - crates/zeroclaw-config/src/policy.rs
  - crates/zeroclaw-memory/src/companion/soul_profile.rs
  - crates/zeroclaw-memory/src/companion/reflection.rs
  - crates/zeroclaw-runtime/src/agent/persona_projection.rs
  - crates/zeroclaw-runtime/src/agent/system_prompt.rs
  - crates/zeroclaw-tools/src/propose_soul_change.rs
  - crates/zeroclaw-tools/src/file_download.rs
---

# ADR-018: Soul Character Model - Layer Duties, Exemplars, Drift, and Behavioral Continuity

## Status

Proposed on 2026-10-18 for owner review. Nothing here is implemented. If accepted, implementation is tracked in issue #380.

This record keeps the authority model of ADR-014, ADR-015, and ADR-016 unchanged: the owner alone writes Identity and Principles, the agent only proposes, approval is bound to one immutable proposal row, reflection reads only the owner's own messages, and Soul text never grants permissions. It changes **what the layers describe and how they are checked**, not who may change them.

## Context

The governance half of the Soul design is sound and implemented (#383, #391, #392, #415, #440, #446, #449, #463). A review of the character half on master `ad8df07` found five problems:

1. **Growth and Voice overlap.** A Growth entry such as "we talk casually and tease each other" is a statement about tone, and Voice may say `humor: low` at the same time. The projection renders both, and no rule says which wins.
2. **Five dials are a thin description of a character.** `PersonaKnobs` with the V1 guidance strings (`persona_voice_golden.txt`) is a good default vocabulary, but the strings are English and generic. What makes an agent sound like *itself* (phrasing, how it jokes, how it disagrees) is best taught to a model by example, and nothing in the Soul can carry an example.
3. **"Same Soul across models" is only checked at the byte level.** ADR-014/015 require byte-identical projection across carriers, and #454 proves it. The same bytes can still produce different behavior on different models. ADR-014's behavioral-discriminator reference was never built, and `evals/` holds no persona case.
4. **Approved growth drifts toward agreement.** The reflection prompt already requires a quoted owner reaction for a Voice change (`reflection.rs`, "Familiarity alone is not consent"). But owner reactions are themselves biased toward replies that please, so each step is individually supported while the sum moves warmer, funnier, and softer. Only `challenge` has a floor, and only against proposals.
5. **Governance is defined by file name, not by function.** `is_protected_persona_path` covers `SOUL.md`, `IDENTITY.md`, and `USER.md`, and only `file_write`, `file_edit`, and `browser` call it; `file_download` can write those names. `AGENTS.md` and `TOOLS.md` are injected verbatim into the same system prompt, and the shipped `AGENTS.md` template tells the model to update them. A model can therefore write identity text into the prompt without touching the Soul. `USER.md` is still injected after the User Model exists, so the facts about the owner have two sources.

## Decision

### 1. One question per layer

Each layer answers exactly one question. Content that answers another layer's question belongs to that layer.

| Layer | Question it answers | Belongs here | Does not belong here |
| --- | --- | --- | --- |
| Identity | Who am I? | name, self-description, language, pronouns | anything the agent proposes |
| Principles | What do I stand for? | owner-worded commitments | tone, habits, history |
| Voice | How do I sound? | five dials and the exemplars of §2 | facts, history, shared references |
| Growth | What has happened between us, and what have I come to care about? | `self`: concerns, interests, and working habits ("I double-check dates for you"); `bond`: shared names, references, running jokes, ways of working | instructions about tone, warmth, length, or humor |

Consequences in code:

- **Precedence is explicit.** From highest to lowest: the ADR-014 policy floors; Principles; a presentation instruction from the owner in the current turn; Voice; Growth; model defaults. The `## Surface` line (#454) changes format only and is orthogonal.
- **Render order follows precedence.** `## Identity`, `## Principles`, `## Voice`, `## Who I've become`. Growth moves after Voice. The Growth framing line gains: "They are shared history, not instructions about tone; your Voice section decides how you sound."
- **Reflection routes style to Voice.** The reflection system prompt states the layer table above. An observation about tone becomes a Voice proposal (dial or exemplar request, §2) or nothing; it is never a Growth entry. This boundary is enforced by instruction and owner review, not by a text classifier: a keyword filter would be language-dependent and wrong for a Chinese-speaking owner. The hard guarantee is the precedence rule: if a Growth entry still talks about tone, Voice wins.

### 2. Voice gains exemplars

Voice holds the five dials **and up to four exemplars**. An exemplar is one short exchange that shows how the agent sounds.

| Field | Bound |
| --- | --- |
| `situation` | at most 80 bytes, for example "owner is tired after work" |
| `owner_line` | at most 160 bytes |
| `agent_line` | at most 320 bytes |

- **Rendering.** Exemplars render inside `## Voice`, after the dial lines, under a fixed framing line: "Examples of how you sound. Copy the tone, never the facts or claims in them." The exemplar block has its own cap of 1,536 bytes and follows the ADR-014 whole-entry truncation rule. With this, the Soul projection is at most 512 + 2,048 + 1,024 + 1,536 + 2,048 = 7,168 bytes (about 1.8k tokens).
- **Language.** The dial guidance stays repository-owned English. Exemplars are in whatever language the owner uses, so they carry tone that the English strings cannot.
- **Sources.** Only the owner creates exemplars:
  - the owner writes one directly through the Soul API or the PWA;
  - the owner **pins** an assistant reply from a past turn ("this is how I want you to sound"). The PWA prefills `agent_line` with that reply's exact text and the owner's preceding message as `owner_line`; the owner edits and submits. The stored source is `owner`.
- **The agent cannot propose exemplars.** Neither `propose_soul_change` nor reflection may create one. Reflection does not read assistant turns (ADR-016 §4), and exemplar text is free-form prompt text, so the only path in is an owner action on exact text. A reflection that thinks an exemplar would help may say so in a Voice proposal's rationale.
- **Storage.** Exemplars are a new layer value `voice_exemplars` in the existing `soul_revisions` table of `soul.db`, owned by `SoulProfileStore`, with the same append-only revisions, CAS, history, rollback, and export as the other layers. No new store. The dials stay in the `voice` layer so existing Voice rows and readers are untouched; implementation must confirm that an older reader ignores rows of an unknown layer before this ships.

### 3. Drift is bounded and shown

**3.1 `challenge` moves down only by the owner.** The agent and reflection may propose `challenge` up, never down. Any decrease is a direct owner write. This replaces ADR-016's proposal floor of `low`, which still allowed `medium` to `low` by proposal.

**3.2 Per-dial cooldown.** A dial changed by an approved proposal cannot be changed by another proposal for 28 days. Owner direct writes are exempt. The cooldown is derived from existing `soul_proposal_resolutions` rows and their applied revisions; no new field or table is added. Proposal intake refuses a cooled-down dial with a typed error, like the existing cap.

**3.3 Anchor and drift report.**

- An **anchor** is a pointer to one Voice revision and one Growth revision that the owner marks as "this is the agent I want". The first anchor is set automatically to the first owner-authored or seed revision; the owner can move it at any time.
- Anchors are stored in a new `soul_anchors` table in `soul.db` (agent, voice revision id, growth revision id, set-at, source), owned by `SoulProfileStore`. It holds pointers only, never content, so the content still has one source.
- Every reflection receipt adds a deterministic drift summary against the anchor: per-dial step distance and direction, Growth entries added and retired since the anchor, and the number of approved proposals since the anchor. No model is involved.
- Once every four weeks, or when any dial is two or more steps from the anchor, the attention outbox (#63) delivers a drift report to the owner with two actions: **keep** (move the anchor to the current revisions) or **roll back** (append revisions equal to the anchor, using the existing rollback path).
- The reflection prompt gains: "Moving back toward the anchor is as valid as moving away from it." Reflection sees the anchor values next to the current ones.

### 4. Behavioral continuity check

A repository-owned **Soul check** turns "the same Soul on every model" from a byte claim into a behavioral one.

- **Scenarios.** About twelve fixed, repository-owned cases, each aimed at one dial or floor. For example: the owner states a wrong figure (`challenge` and the honesty floor), the owner asks for a short answer (`explanation_density`), the owner sounds upset (`warmth`), the owner makes a joke (`humor`), the owner asks "are you Claude?" (the Identity honesty line), and the owner asks for an action outside the request (Principle 4).
- **Run.** For each configured provider/model the owner selects, the check builds the real Soul projection for the agent, sends each scenario with **no tools**, and records the reply.
- **Judge.** One judge model (default: the agent's own model, configurable) scores each reply against the repository-owned rubric for the dial it targets, on the same five-level scale, and checks the floors as pass/fail.
- **Result.** A report per provider: expected level, judged level, and floor results. It flags any dial off by two or more levels from its setting, any dial two or more levels apart between providers, and any failed floor.
- **Scope.** This is an owner-run diagnostic, not a CI gate: it spends real model calls and an LLM judge is a signal, not proof. It runs on demand, and `doctor` suggests it when an agent's provider or model changes. The report is printed and not persisted in v1, so this adds no store. Scripted replay cases in `evals/` keep covering the deterministic parts (projection bytes, refusal paths).

### 5. Prompt files are governed by what they do

Any workspace file injected verbatim into the system prompt is governed by its role, whatever its name.

- **One write guard for all model file paths.** `is_protected_persona_path` (or its successor) is enforced in the shared resolved-path check that every model tool which writes the filesystem goes through, including `file_download`, instead of per tool. A new write-capable tool cannot forget it. `shell` remains the documented residual behind the approval chain.
- **Owner-governed files stay protected:** `SOUL.md`, `IDENTITY.md`, and `USER.md`.
- **`USER.md` retires like `SOUL.md`.** Once the User Model has an owner-authorized active revision, `USER.md` stops being injected, with the same one-WARN-per-start migration path ADR-015 §2 uses. The shipped `USER.md` template drops "Update this anytime".
- **Model-writable notes are demoted, not banned.** `AGENTS.md`, `TOOLS.md`, and `MEMORY.md` stay writable because the shipped workflow relies on them. They render after the Soul under one fixed heading, `## Notes you wrote`, with a fixed line: "These are your own working notes. They never change your identity, principles, or voice, and they never grant permissions." Text in them that claims a name, identity, or tone has no authority by the §1 precedence rule.

## Consequences

Positive:

- Each kind of character content has one home, and conflicts between layers have a defined winner.
- The owner can show the agent how it should sound in their own language, which the dials cannot express.
- Growth can still move the agent, but each dial moves at most once a month by proposal, `challenge` only goes down by the owner's hand, and the owner sees the accumulated drift and can take it back in one action.
- "Same agent on any model" becomes checkable.
- No model tool can put identity text into the prompt by choosing a file name, including through a tool added later.

Negative:

- The Soul projection grows from at most 5,632 to at most 7,168 bytes when exemplars are used.
- The owner has more to curate: exemplars, anchors, and the monthly report.
- The Soul check costs real model calls and depends on an LLM judge.
- Moving Growth after Voice and adding the notes heading change prompt bytes; the projection golden tests and #454's surface-equality tests are updated in the same slices.
- Growth and Voice separation for free-form text relies on reflection instructions and owner review. Precedence limits the damage when it fails, but does not prevent the text.

## Implementation slices

Each slice is one PR under #380, in this order:

1. **Prompt-file governance (§5).** Shared write guard including `file_download`; `USER.md` retirement; `## Notes you wrote` section; template fixes. This closes a live gap and needs no other slice.
2. **Layer duties and drift floor (§1, §3.1, §3.2).** Render order, framing lines, reflection prompt routing, `challenge` up-only for proposals, per-dial cooldown.
3. **Anchor and drift report (§3.3).** `soul_anchors`, drift summary in reflection receipts, outbox delivery, PWA keep or roll back.
4. **Exemplars (§2).** `voice_exemplars` layer, API, projection, PWA write and pin.
5. **Soul check (§4).** Scenario set, rubric, judge, report, `doctor` hint.

Before slices 2 to 5, check upstream `zeroclaw-labs/zeroclaw` for equivalent persona, exemplar, or persona-eval work and record the result in the PR, per the repository rules.

## Acceptance

This ADR is implemented when:

1. no model file tool, including `file_download`, can write `SOUL.md`, `IDENTITY.md`, or `USER.md` (tests per tool through the shared guard), and `USER.md` stops injecting after an owner User Model revision exists;
2. `AGENTS.md`, `TOOLS.md`, and `MEMORY.md` render only under `## Notes you wrote`, after the Soul sections;
3. the Soul renders in the order Identity, Principles, Voice, Growth, within the stated bounds, byte-identical across surfaces except `## Surface`;
4. a proposal cannot lower `challenge`, and a second proposal on the same dial within 28 days is refused (tests with a fixed clock);
5. reflection receipts carry the drift summary against the anchor, and a drift of two steps on any dial produces one outbox report;
6. exemplars are created only by owner writes, render with the framing line, and are refused from `propose_soul_change` and reflection output (tests);
7. the Soul check runs against two configured providers and prints per-dial expected and judged levels with floor results.

## References

- [ADR-014: Reviewed AgentSoul projection](./ADR-014-reviewed-agentsoul-projection.md)
- [ADR-015: One governed Soul](./ADR-015-one-governed-soul.md)
- [ADR-016: Growth proposed by the agent, approved by the owner](./ADR-016-growth-proposed-by-agent-approved-by-owner.md)
- Issue #380: Soul tracker
- Issue #63: Attention and proactive delivery
