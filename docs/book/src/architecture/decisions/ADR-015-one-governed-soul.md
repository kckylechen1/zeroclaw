---
id: ADR-015
title: One governed Soul with identity, principles, and voice
date: 2026-09-23
status: accepted
relates-to:
  - ADR-013
  - ADR-014
  - crates/zeroclaw-memory/src/soul.rs
  - crates/zeroclaw-memory/src/soul_candidate.rs
  - crates/zeroclaw-config/src/persona.rs
  - crates/zeroclaw-runtime/src/agent/personality.rs
  - crates/zeroclaw-runtime/src/agent/personality_templates
  - crates/zeroclaw-runtime/src/agent/system_prompt.rs
---

# ADR-015: One Governed Soul With Identity, Principles, and Voice

## Status

Accepted by the owner on 2026-09-23, including the per-key Voice layering in Decision §2 that supersedes ADR-014's whole-section precedence rule. All other ADR-014 rules remain in force. Implementation is tracked in issue #380.

## Context

[ADR-014](./ADR-014-reviewed-agentsoul-projection.md) defines a careful, closed contract for reviewed voice traits:
- five `PersonaKnobs` keys with `PersonaLevel` values;
- an operator-gated review;
- fixed repository-owned strings;
- a byte-bounded projection.

That contract is sound, but on 2026-09-23 it governs almost nothing that reaches the model.

**What actually shapes the agent's personality today:**

1. **Workspace personality files.** `SOUL.md`, `IDENTITY.md`, and `USER.md` are injected verbatim into every system prompt (`BOOTSTRAP_FILES` in `system_prompt.rs`), up to 20,000 characters each.
   - The shipped templates end with "*This file is yours to evolve. As you learn who you are, update it.*" and "*Your identity is yours to shape.*"
   - No tool policy protects these files. `is_runtime_config_path` lists only config and secret files, so the model can rewrite its own identity with `file_write`, `file_edit`, or `shell`, with no candidate, review, receipt, or bound.
   - This is exactly the "evidence becomes authority" path that ADR-014 and the Soul candidate service were built to prevent.
2. **Config persona knobs.** `[persona]` / card persona are rendered by `PersonaKnobs::to_prompt_section`. This path is owner-authored and safe.
3. **Reviewed Soul (ADR-014).** It has no production composition:
   - `IdentityRegistry` is never constructed outside tests, so no `AgentIdentityId` is ever admitted;
   - nothing calls `SoulCandidateService::submit`;
   - there is no review endpoint and no projection consumer.

**Resulting problems:**

- **Governance is inverted.** The riskiest content (free-form identity and principles, up to 40k characters, model-writable) has no governance. The least risky content (five closed dials) has the most.
- **The shipped `IDENTITY.md` / `SOUL.md` templates break the honesty floor that ADR-014 puts first:**
  - "NEVER mention OpenAI, Anthropic, DeepSeek, Google by name";
  - "You are NOT ChatGPT, Claude…";
  - "Built in Rust. 3MB binary."
- **Two parallel "souls" exist.** A reader of ADR-014 would believe Soul is governed. A reader of the prompt would see a file the model edits.
- **Voice is too thin to be a personality.** Tone dials alone do not say who the agent is or what it stands for. A personal agent needs a stable identity and a few principles that survive model and provider changes. Carrier continuity is the main user-visible benefit of having a Soul at all.
- **One dial line conflicts with a policy floor.** `challenge = minimal` renders "Do not argue… leave disagreements alone", which can suppress correcting a factual error. ADR-014 already states that `challenge` never weakens honesty; the rendered string does not.

## Decision

### 1. Soul has exactly three layers, each with one authority

| Layer | Content | Authority to change | Candidates? | Prompt section and bound |
| --- | --- | --- | --- | --- |
| **Identity** | `name`, `self_description` (≤ 280 bytes), `primary_language`, `pronouns` (optional) | Owner only (operator API or first-run setup) | No | `## Identity`, ≤ 512 bytes |
| **Principles** | Ordered list of ≤ 8 owner-worded statements, each ≤ 240 bytes | Owner only; the model may *propose* | Yes. Owner submits the final wording. | `## Principles`, ≤ 2,048 bytes |
| **Voice** | ADR-014 trait heads (5 closed keys × `PersonaLevel`) | ADR-014 review | Yes (ADR-014) | `## Voice`, ≤ 1,024 bytes (ADR-014) |

**Rules for all three layers:**
- Each layer is stored as append-only revisions under the admitted identity's `soul` namespace, with supersession links and a review receipt. This reuses the ADR-014 revision/CAS/receipt substrate; no second engine is created.
- Principles text is **owner-authored**:
  - The review request carries the exact text the owner submits. The UI may prefill it from a candidate, but the stored authority is the owner's submission, never "accept candidate text by id".
  - The ADR-014 rule that candidate prose never projects is therefore preserved.
- Relationship, mood, affect, user preferences, work policy, avatar/TTS, and Private Dyad remain excluded, exactly as in ADR-014's excluded-domains table.

### 2. One source per layer; config and legacy files become seeds

- **Voice:**
  - When a reviewed head exists for a key, it wins **for that key**.
  - Keys without a head fall back to the configured persona knob, then to `medium`.
  - This supersedes ADR-014's "configured persona suppresses Soul as a whole" rule (ADR-014 Decision, "Projection source and precedence", items 3–4). The single-owner agent has one author for both sources, so per-key layering loses nothing and removes the surprising all-or-nothing behavior. Duplicate or conflicting heads for a key still withhold the whole Voice section.
- **Identity and Principles:**
  - The first time an admitted identity starts with no Identity/Principles revisions, it seeds them:
    - Identity is seeded from config (`[agent] name` and the `{agent}` template variable).
    - Principles are seeded from a new, honest shipped default (see §5).
  - The seed revision records `source = seed`, so the owner can see it was never reviewed.
- **Legacy workspace files** (`SOUL.md`, `IDENTITY.md`; `USER.md` belongs to the User Model):
  - They stop being injected once the identity has an Identity revision whose `source` is `owner` (the owner has migrated).
  - Until then they keep being injected for compatibility, and the prompt builder logs one WARN per start naming the migration endpoint.
  - The migration UI shows the legacy file next to the structured fields so the owner can copy what they want. Nothing is parsed automatically.

### 3. The model cannot write its own Soul, but it can propose

- `SOUL.md`, `IDENTITY.md`, and `USER.md` in any agent workspace are added to the model-tool write denylist (`file_write`, `file_edit`, `personal_file`) next to `is_runtime_config_path`.
  - `shell` cannot be fully guarded by path. This residual is documented, and the approval chain for shell remains the control.
- The shipped templates drop "yours to evolve", "update it", and "yours to shape".
- A new model tool **`propose_soul_change`** is the only model path into Soul:
  - input: `layer` (`principles` | `voice`), `proposal` (≤ 240 bytes), `rationale` (≤ 480 bytes), and, for voice, the proposed `trait_key` / `level`;
  - it calls `SoulCandidateService::submit` with `CandidateOrigin::ModelSummary`, `DomainClassification::SoulDisposition`, and the current turn as the evidence ref;
  - it returns a fixed string: "Proposal recorded for owner review. Nothing about me has changed.";
  - it keeps at most 3 open proposals per identity and refuses the rest with a typed error;
  - it is absent from ReasoningSubAgent and Supervisor profiles.
- This gives the existing candidate intake its first production producer without adding any promotion path.

### 4. Identity is admitted at startup

- Each configured agent owns exactly one `AgentIdentityId`. It is minted once on first start, stored in the agent's state directory, and admitted into one `IdentityRegistry` built at the composition root.
- Missing, unreadable, or duplicated identity state fails closed: no Soul projection, one ERROR, and the agent still runs with no Soul.
- The identity id never enters the prompt.

### 5. Honesty floor for identity

- The agent may carry a name and self-description. If the user sincerely asks whether they are talking to an AI, or which model or provider is answering, the agent answers truthfully. The runtime knows the current provider and model.
- The shipped default Principles are:
  1. Be useful, not performative.
  2. Say what you actually think, including disagreement.
  3. Never invent facts or tool results; say when you are unsure.
  4. Ask before acting outside what was asked.
  5. Keep private things private.
- The `{agent}` name substitution stays. The lines denying the underlying model and the "3MB binary" claim are removed.
- `challenge = minimal` and `low` lines gain a floor clause: "Still correct factual errors and flag safety risks." `warmth` and `humor` lines never override the Principles section, which is rendered before Voice.

### 6. Projection

- Order inside the existing persona seam (after the anti-narration and tool-honesty blocks, before tools): `## Identity`, then `## Principles`, then `## Voice`. Total ≤ 3,584 bytes. Each section follows ADR-014's truncation rule: whole lines only, in fixed order, with an elision marker if it fits.
- Only three kinds of text can render:
  1. owner-submitted text (Identity, Principles);
  2. repository-owned strings (Voice, headings);
  3. the seed defaults.
- Identical revisions produce byte-identical Soul bytes across model, provider, harness, and bridge.
- Projection never changes tools, credentials, approvals, routing, or provider selection.

### 7. Owner surfaces

All of these are operator-gated gateway endpoints that reuse the ADR-014 review boundary:
- `GET /api/soul`: current Identity, Principles, and Voice, with revision, source (`seed` / `owner` / `reviewed_candidate`), and timestamp.
- `GET /api/soul/history`: revisions and receipts per layer.
- `PUT /api/soul/identity`, `PUT /api/soul/principles`: owner-authored revisions with an expected-revision CAS.
- `POST /api/soul/candidates/{id}/review`: ADR-014 voice review, plus principle review in which the owner supplies the final text.
- `POST /api/soul/{layer}/rollback`: appends a new revision equal to a named earlier one (supersession, not deletion).
- `GET /api/soul/export`: JSON of all active layers and history, without private evidence notes.

The PWA (#379) gets one "Who is my agent" page over these endpoints. Bridges never review Soul (ADR-014: channel identity is not review authority).

## Consequences

Positive consequences:

- Everything that shapes the agent's personality is governed, bounded, versioned, and inspectable. The model can no longer silently rewrite its own identity.
- The Soul survives model and provider switches byte-for-byte, which is the product reason for having one.
- The candidate service, review boundary, and projection seam built for ADR-014 get real production callers.
- The prompt shrinks from up to 40k characters of identity files to at most 3.5 KB.
- The shipped identity no longer instructs the agent to deny what it is.

Negative consequences:

- The agent can no longer "grow" its personality by itself. Growth is always a proposal the owner reviews. This is intentional.
- Owners with rich hand-written `SOUL.md` files must migrate content into ≤ 8 principles, or accept that the legacy file stops injecting after migration.
- `shell` can still write the legacy files until they stop injecting. After migration, writing them has no effect on the prompt.
- Per-key Voice layering is a semantic change from ADR-014 and must be tested against its conflicting-head rule.

## Acceptance

This ADR is implemented when:

1. an agent start admits one identity and builds `IdentityRegistry` at the composition root;
2. the three layers are stored as revisions with receipts, and the owner endpoints above work with CAS;
3. `propose_soul_change` records a candidate and cannot change any active layer (test: call it 25 times, active Soul bytes unchanged);
4. model tools cannot write `SOUL.md` / `IDENTITY.md` / `USER.md` (tests for `file_write`, `file_edit`, `personal_file`);
5. projection is byte-identical across two different providers for the same revisions, and tool, approval, and routing sets are identical with and without Soul;
6. legacy files stop injecting after an owner Identity revision exists, and a WARN is emitted before that;
7. the shipped templates and default Principles contain no false identity claims, and the `challenge` minimal/low lines carry the floor clause.

## References

- [ADR-014: Reviewed AgentSoul projection](./ADR-014-reviewed-agentsoul-projection.md)
- [ADR-013: Channels as gateway clients](./ADR-013-channels-as-gateway-clients.md)
- `crates/zeroclaw-runtime/src/agent/system_prompt.rs` (`BOOTSTRAP_FILES`, `build_system_prompt_with_persona`)
- `crates/zeroclaw-runtime/src/agent/personality_templates/SOUL.md`, `IDENTITY.md`
- `crates/zeroclaw-config/src/policy.rs` (`is_runtime_config_path`)
- `crates/zeroclaw-memory/src/soul_candidate.rs` (`CandidateOrigin::ModelSummary`)
