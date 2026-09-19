---
id: ADR-014
title: Reviewed AgentSoul uses a closed presentation-only projection
date: 2026-09-20
status: accepted
relates-to:
  - ADR-010
  - ADR-011
  - docs/book/src/architecture/memory-payload-lifecycle.md
  - https://github.com/kckylechen1/zeroclaw/issues/52
  - https://github.com/kckylechen1/zeroclaw/issues/188
  - https://github.com/kckylechen1/zeroclaw/issues/189
  - https://github.com/kckylechen1/zeroclaw/issues/190
  - https://github.com/kckylechen1/zeroclaw/issues/295
---

# ADR-014: Reviewed AgentSoul Uses A Closed Presentation-Only Projection

## Status

The owner selected the six-row Slice A contract in this record on 2026-09-20,
and independent review of the exact documentation head is complete. The ADR is
accepted. It does not activate candidate promotion or prompt projection, and it
does not declare issues #188, #189, #190, or #295 complete.

## Context

AgentSoul is reviewed, identity-bound stable presentation state for one admitted
agent identity across replaceable model, provider, and harness carriers. It is
not a user-preference store, relationship ledger, work-policy store, permission
system, or routing engine.

The current candidate service records free-form `disposition` and
`proposed_rule` values with supporting and countering evidence. It has no active
state. Evidence quality, recurrence, confidence, source verification, worker
receipts, model summaries, and descriptive `OwnerCorrection` labels are not
promotion authority.

ZeroClaw already has the smallest suitable presentation vocabulary:
`PersonaKnobs` defines five closed keys, `PersonaLevel` defines five closed
values, and `build_system_prompt_with_persona` owns one production `## Voice`
insertion seam. Authored config personas remain authored config. Reviewed Soul
state is stored separately and maps to that vocabulary only at projection time.

## Decision

### Closed v1 trait registry

V1 active Soul admits exactly these keys and values:

| Trait key | Value | Stable meaning | Not included |
| --- | --- | --- | --- |
| `warmth` | `minimal`, `low`, `medium`, `high`, or `xhigh` | Default social warmth in the agent's own voice. | User-specific affection, relationship status, mood, or empathy claims. |
| `directness` | Same closed `PersonaLevel` values | Default speed and firmness when stating a conclusion. | User preference, task urgency, approval bypass, or command authority. |
| `explanation_density` | Same closed `PersonaLevel` values | Default amount of supporting reasoning shown. | Work procedures, evidence standards, hidden chain-of-thought, or per-turn formatting. |
| `challenge` | Same closed `PersonaLevel` values | Default willingness to disagree or probe. | Permission to weaken honesty, correctness, privacy, evidence integrity, safety, or authority. |
| `humor` | Same closed `PersonaLevel` values | Default permission for levity. | Transient affect, relationship escalation, harassment, or permission to disregard task tone. |

No custom key or free-form active rule exists in v1. Unknown candidate
dispositions remain candidates or are rejected.

### Candidate field and authority table

| Candidate field | Review use | Active-state use | Prompt use |
| --- | --- | --- | --- |
| `candidate_id` | Required locator under the admitted AgentIdentity and exact candidate revision. | Private provenance reference. | Never projected. |
| `disposition` | Untrusted proposal that may nominate one exact v1 key. | The authenticated reviewer selects the canonical key. | Only the canonical key selects repository-owned text. |
| `proposed_rule` | Untrusted review evidence. | The reviewer selects one canonical level; free-form text remains candidate history. | Never projected. |
| `status` | Must be `candidate`; `retracted` cannot be accepted. | Candidate status is not reused as active state. | Never projected. |
| `evidence` and source revisions | The exact evidence set is covered by the reviewed candidate revision or digest. | Private provenance only. | Never projected. |
| recurrence, confidence, and outcome refs | Review context only; never authority. | Optional private receipt metadata. | Never projected. |
| `context_shapes` | Review context only. V1 active traits are global for one identity. | No new applicability engine. | Never projected. |
| `sensitivity` | V1 acceptance requires `public`. | Classification remains private metadata. | Internal and sensitive candidates never project. |
| `last_origin` | Audit context only, not authentication. | Private receipt metadata. | Never projected. |
| intake `domain` | Must have passed `SoulDisposition`; `UserPreference` is refused. | No domain inference at activation. | Never projected. |

The active value is one typed `{agent_identity_id, trait_key, persona_level}`
head plus private revision, supersession, reviewer receipt, provenance, and
behavioral-discriminator references. Candidate prose and evidence are never
active prompt fields.

### Authenticated review boundary

The first review ingress is one operator-gated Gateway endpoint that reuses the
existing operator-bearer contract. Model tools, generic memory, channel identity,
candidate origins, and worker receipts cannot review or promote Soul.

The server assigns the reviewer class `owner_operator`; request content cannot
claim reviewer authority. The bearer token is neither stored nor echoed.

A typed review request carries:

- `accept`, `reject`, or `supersede`;
- the exact expected candidate revision or canonical digest covering the full
  candidate and evidence set;
- a canonical trait key and level for accept or supersede;
- the exact expected active-head revision when replacing a head;
- a behavioral-discriminator reference for any active write; and
- an optional bounded private note.

One transaction resolves exactly one active admitted AgentIdentity, verifies the
candidate identity, status, public sensitivity, revision, evidence set, and
closed trait domain, checks the active-head revision, and appends the receipt.
Accept or supersede appends one active head and its supersession link. Reject
creates no active head. Conflict, unavailable state, or write failure returns no
partial success.

The current candidate shape has no explicit candidate revision field. An
accepted exact revision/CAS binding on the actual storage path is a prerequisite
for review ingress. Candidate id, timestamp, latest-read behavior, or a
caller-supplied evidence list is insufficient. Revision, supersession, CAS, and
receipt mechanics reuse the substrate assigned by the architecture constitution;
ZeroClaw does not create a second generic lifecycle engine or evidence store.

### Projection source and precedence

Each turn resolves exactly one active admitted AgentIdentity. Missing, revoked,
ambiguous, or unavailable identity yields no Soul projection.

An explicitly configured per-agent or card persona supplies the entire existing
`## Voice` section. Reviewed Soul supplies none in that case. There is no
per-field config/Soul merge. When no persona config exists, one active head per
canonical key maps into the existing `PersonaKnobs` rendering vocabulary and
enters the existing `build_system_prompt_with_persona` seam. Duplicate or
conflicting heads withhold the whole Soul section rather than guessing.

Precedence from highest to lowest is:

1. safety, honesty, privacy, evidence integrity, factual and task correctness,
   and tool, approval, risk, routing, credential, merge, and execution policy;
2. an explicit presentation instruction for the current turn that stays within
   those policy floors;
3. one complete explicitly configured persona;
4. reviewed active Soul, only when no configured persona exists; and
5. ordinary runtime and model defaults.

Soul text identifies itself as a default voice that yields to the current turn
and higher policy. A turn instruction creates no candidate, receipt, or active
write. Projection changes no tools, credentials, risk profile, approval,
routing, provider selection, or execution authority.

### Bounds and canonicalization

- Maximum active items: 5, one per canonical key.
- Fixed order: `warmth`, `directness`, `explanation_density`, `challenge`,
  `humor`.
- `medium` is omitted, matching the existing renderer.
- Maximum section size: 1,024 UTF-8 bytes, including heading and any marker.
- Estimated token budget: `ceil(utf8_bytes / 4) <= 256`, always labeled an
  estimate rather than an exact provider token count.
- Only fixed repository-owned strings for a key and level may render.
- Candidate text, notes, evidence, provenance, identity tokens, source ids, and
  private paths never enter the prompt.
- If a future fixed line would exceed a bound, complete lines are appended in
  fixed order until the next line would exceed the stricter bound. A bounded
  `(+N reviewed voice traits elided)` marker is appended if it fits. UTF-8
  scalars and lines are never cut.
- Identical active heads and identity produce byte-identical Soul bytes across
  model, provider, harness, and channel carriers.

The projection is a derived view, not a fact store. It creates no cache that can
resurrect revoked, superseded, erased, ambiguous, or unavailable state.

## Excluded domains and policy floors

| Domain | Examples | Owner or result |
| --- | --- | --- |
| UserModel | User preferences, values, goals, constraints, and habits. | UserModel, never global AgentSoul. |
| PrivateDyad | Shared nicknames, private shorthand, relationship conventions, and episodes. | Dyad; outside Slice A. |
| Turn/session context | Current affect, urgency, task tone, and one-turn style. | Ephemeral interaction context. |
| WorkPolicy, Skill, and Eval | Investigation, testing, preservation, completion, and repository procedure. | Existing work-policy and Tachi owners. |
| Blueprint and expression | Reusable creator defaults, lore, avatar, TTS, and multimodal expression. | Deferred design; no v1 compiler. |
| Policy floors | Honesty, privacy, evidence integrity, safety, approval, tools, risk, routing, credentials, merge, and execution authority. | Immutable higher-priority policy. |

## Acceptance gates

This ADR becomes accepted only after independent review accepts this exact
documentation head. That acceptance freezes the contract but does not activate
runtime behavior.

Production activation remains blocked until all applicable gates have exact-head
evidence:

1. Issue #188 candidate query and integrity repairs are accepted for the records
   this path consumes.
2. Issue #188 C maps and protects every actual generic and dedicated storage
   handle reachable by the review and projection consumers.
3. Issue #188 D declares and mechanically enforces the supported writer model.
4. Issue #188 E proves admitted identity, poisoned and duplicate identity
   behavior, and authenticated ingress for every enabled entry point.
5. Issue #189 implements and independently validates atomic review, exact
   candidate/evidence binding, active heads, receipts, and supersession.
6. Issue #190 independently validates one real prompt-assembly consumer,
   deterministic bounds, carrier continuity, protected access, and permission
   invariance.

No design approval, documentation merge, additive service, fixture-only test, or
issue status substitutes for those gates. #189/#190 receive no activation or
implementation authority from this ADR alone.

## Consequences

Positive consequences:

- Free-form evidence cannot become system-prompt instructions.
- Static authored persona and reviewed Soul never compete field by field.
- The first vertical reuses an existing vocabulary, review gate, and prompt
  insertion seam without a PersonaBlueprint or general compiler.
- Privacy and authority remain structurally separate from presentation.

Negative consequences:

- Internal or sensitive candidates cannot project in v1.
- Configured persona suppresses Soul as a whole, even when only one configured
  dial differs from the default.
- Context-scoped traits and multi-source composition remain unavailable.
- Activation waits for the real protected-access, writer, identity, review, and
  prompt-consumer gates rather than only the field decision.

## References

- [Issue #52: AgentSoul tracker](https://github.com/kckylechen1/zeroclaw/issues/52)
- [Issue #188: candidate repair owner](https://github.com/kckylechen1/zeroclaw/issues/188)
- [Issue #189: authorized promotion](https://github.com/kckylechen1/zeroclaw/issues/189)
- [Issue #190: bounded projection](https://github.com/kckylechen1/zeroclaw/issues/190)
- [Issue #295: presentation-plane decision](https://github.com/kckylechen1/zeroclaw/issues/295)
- [ADR-010: memory authority boundaries](./ADR-010-memory-authority-boundaries.md)
- [ADR-011: multi-agent runtime boundaries](./ADR-011-multi-agent-runtime-boundaries.md)
- [Memory and payload lifecycle](../memory-payload-lifecycle.md)
- `crates/zeroclaw-memory/src/soul.rs`
- `crates/zeroclaw-memory/src/soul_candidate.rs`
- `crates/zeroclaw-config/src/persona.rs`
- `crates/zeroclaw-gateway/src/operator_auth.rs`
- `crates/zeroclaw-gateway/src/api_user_model.rs`
- `crates/zeroclaw-runtime/src/agent/system_prompt.rs`
- `crates/zeroclaw-runtime/src/agent/prompt_helpers.rs`
