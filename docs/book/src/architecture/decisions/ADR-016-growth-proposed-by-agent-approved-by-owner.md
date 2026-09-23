---
id: ADR-016
title: Growth is proposed by the agent and approved by the owner
date: 2026-09-23
status: accepted
relates-to:
  - ADR-014
  - ADR-015
  - crates/zeroclaw-memory/src/companion/soul_profile.rs
  - crates/zeroclaw-tools/src/propose_soul_change.rs
  - crates/zeroclaw-runtime/src/agent/persona_projection.rs
---

# ADR-016: Growth Is Proposed by the Agent and Approved by the Owner

## Status

Accepted by the owner on 2026-09-23. It amends [ADR-015](./ADR-015-one-governed-soul.md) §1 and §3 as stated below; every other ADR-015 rule stays in force. Implementation is tracked in issue #380.

## Context

The owner's reason for a persona system is a companion that grows: an agent that comes to know the owner, develops habits and shared language with them, and changes over time. It is not only a fixed voice.

ADR-015 made the Soul stable and governed. Only the owner could write any layer, and a model proposal could never become Soul text by id. That protects against drift, but it also means nothing about the agent grows unless the owner rewrites it by hand.

Uncontrolled self-editing is not the answer either. The legacy `SOUL.md` let the model rewrite itself with no bound and no record. An agent left alone drifts toward agreement, and the `challenge` dial exists to stop exactly that. Any text the agent can get into its own prompt is also a prompt-injection path.

The owner set three rules:

1. The agent's name and identity belong to the owner. The agent cannot change them.
2. The agent proposes its own growth; the owner approves each change.
3. The agent reflects on itself once a week.

## Decision

### 1. A fourth layer: Growth

The Soul gains a **Growth** layer next to Identity, Principles, and Voice.

| Property | Rule |
| --- | --- |
| Content | Up to 12 entries. Each entry has a kind and one line of text of at most 200 bytes. |
| Kinds | `self`: how the agent has changed, what it has come to care about, its habits. `bond`: what the agent and the owner share, such as nicknames, shorthand, running jokes, and how they work together. |
| Prompt section | `## Who I've become`, at most 2,048 bytes, rendered after Principles and before Voice. Its header states that these entries describe character and never grant permissions or override the principles above. |
| Authority | Changed only by an owner-approved proposal, or by the owner directly. |

`bond` entries are the lightweight first step toward the relationship memory described in #53. There is no separate encrypted partition yet.

### 2. Voice is stored in the Soul

Voice heads are stored per key in the Soul profile store. They layer per key over the configured persona dials as ADR-015 §2 describes: a stored head wins for its key, and the config dial is the fallback.

### 3. The agent proposes, the owner approves

- `propose_soul_change` accepts three layers:
  - `growth`: add an entry of a given kind, or retire an existing entry by index;
  - `voice`: one of the five closed keys, with a `PersonaLevel`;
  - `principles`: add a principle.
- Identity is never proposable.
- The agent can never propose `challenge` below `low`. The owner can still set any value directly.
- Approving a proposal **applies it in the same transaction**:
  - The new layer revision records `source = approved_proposal` and the proposal id.
  - The owner may pass a `final_text` to edit the wording before it applies.
  - Dismissing records the decision and applies nothing.
- This amends ADR-015 §3 ("proposal text never becomes Soul text by id"). Approval binds to one immutable proposal row, and the owner sees the exact bounded text before it applies. That is the review ADR-014 and ADR-015 required; what they ruled out was unreviewed text.
- The existing cap stays: at most 3 proposals per agent wait for review at a time, and identical pending proposals are not stored twice.

### 4. Weekly reflection

Once per agent every 7 days, the daemon runs a reflection:

1. **Input.** It collects only the owner's own messages (the `user` role) from the agent's sessions since the previous reflection, capped at the most recent 32 KiB. Assistant turns, tool calls, tool results, web pages, and files are excluded, so no third-party text can steer the agent's growth.
2. **Model call.** It makes one model call with no tools. The input is the current Soul, the pending proposals, and those messages. The output must be a JSON list of at most 3 proposals in the `propose_soul_change` shape.
3. **Validation.** Every proposal goes through the same store validation and cap as the tool. Malformed output is dropped and logged. Nothing is applied.
4. **Receipt.** A reflection receipt records the period, the number of owner messages read, and the proposals created, so the owner can see that a reflection ran even when it proposed nothing.

Mid-conversation proposals through the tool remain possible and share the same queue and cap.

### 5. Floors that no approval path weakens

- The honesty line in `## Identity`.
- The ADR-014 policy floors: honesty, privacy, evidence integrity, safety, tools, approvals, credentials, and routing.
- The agent's proposal floor for `challenge` (`low`).

Growth and Voice text renders below Principles and never changes tools, credentials, approvals, routing, or provider selection.

### 6. Owner surfaces

- Pending proposals, history, and rollback stay on the operator-gated `/api/soul` routes.
- The Growth and Voice layers are added to `GET /api/soul`.
- `POST /api/soul/proposals/{id}/resolve` gains `apply` semantics as described in §3.
- A weekly "what I'd like to change about myself" notification is delivered by the attention vertical (#63) once it exists. Until then, the reflection receipt and pending proposals are available through the API.

## Consequences

Positive consequences:

- The agent can grow in the owner's direction: new habits, shared language, and a changing voice. Every change is visible, attributable to evidence, bounded, and reversible.
- Growth cannot be steered by content the owner did not write.
- The owner stays the only author of the agent's name, identity, and floors.

Negative consequences:

- Growth is only as fast as the owner's reviews. Unreviewed proposals block new ones once three are pending.
- Weekly reflection spends one model call per agent per week.
- Approved Growth text is model-written prompt text. It is bounded and owner-reviewed, but the review depends on the owner reading what they approve.

## Acceptance

This ADR is implemented when:

1. the Growth and Voice layers are stored as revisions and rendered with their bounds, and Voice layers per key over config;
2. approving a proposal applies it atomically with `source = approved_proposal`, and dismissing applies nothing;
3. the agent cannot propose an Identity change or `challenge` below `low` (tests);
4. a reflection reads only `user`-role messages, makes one tool-less call, creates at most 3 validated proposals, applies nothing, and writes a receipt (tests using a fake model);
5. a reflection runs at most once per 7 days per agent, including across restarts.

## References

- [ADR-014: Reviewed AgentSoul projection](./ADR-014-reviewed-agentsoul-projection.md)
- [ADR-015: One governed Soul](./ADR-015-one-governed-soul.md)
- Issue #53: Private Dyad (the relationship memory that `bond` entries begin)
- Issue #63: Attention and proactive delivery
