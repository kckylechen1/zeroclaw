---
id: ADR-020
title: Durable turns, replay classes, background work, and attention - patterns adopted from Pi Durable
date: 2026-10-30
status: proposed
relates-to:
  - ADR-010
  - ADR-013
  - ADR-016
  - ADR-017
  - ADR-018
  - ADR-019
  - crates/zeroclaw-gateway/src/nodes.rs
  - crates/zeroclaw-tools/src/tachi_delegation.rs
  - crates/zeroclaw-tools/src/tachi_staff.rs
---

# ADR-020: Durable Turns, Replay Classes, Background Work, and Attention - Patterns Adopted from Pi Durable

## Status

Proposed on 2026-10-30. Not accepted until the owner approves it.

Merging this proposal does not activate its slices or change existing approval,
replay, privacy, or delivery contracts. #374 remains the execution index.

This record keeps the authority boundaries in ADR-017 and the ADR-019 proposal unchanged. It names durability and attention patterns drawn from Pi Durable, distinguishes acknowledgement from necessary intent clarification (§8), and records which patterns are not adopted. The Decision section describes proposed work, not delivered capabilities.

## Context

- The body already follows several durability rules, but each one lives in its own place: a transmitted Tachi start with no trustworthy receipt is `SubmissionUnknown` and is never replayed (ADR-017 §8); a lost Node result is an unknown outcome and is never resent with a new device sequence (#514, ADR-019 Acceptance); bridges carry a durable intake identity (#377, #438); disconnecting a client, cancelling the current turn, and cancelling a delegated job are separate actions (ADR-017 §7).
- Open work needs the same rules again: automatic background completion delivery (#381, #63), pre-compaction note saving and event-triggered reminders (PRD, Memory), and the Yonbo X1 pet-state mapping (ADR-019 §6, S8).
- Earendil shipped Pi Durable (experimental, TypeScript) on 2026-10-01 together with Pi 1.0. It is a harness for long-running agents. Its relevant design choices:
  - every step of a run is a task that stores a checkpoint before moving on;
  - a submission carries a `requestId`, so a client that retries after a crash gets the original submission back;
  - every tool declares whether a rerun after a crash is safe (`replay: "safe"`); otherwise the model is told the call was interrupted, with the output stored so far;
  - a hook that makes a decision, such as an approval, stores it in a memo where the first write wins, so a restart does not ask again;
  - tasks are foreground (an ordinary abort such as Esc stops them) or background (owned by the conversation, survive restarts, and an ordinary abort leaves them alone; the example given is a reminder that fires tomorrow);
  - compaction runs in the background before the context is full, never deletes older messages, and a handoff starts a new context while older messages stay searchable;
  - application state lives in typed documents committed atomically with the transcript, and clients subscribe to a current view and then receive only changes.
- Community Pi packages add ideas that fit a companion agent: observation and reflection memory prepared in the background so compaction is a cheap render step (`pi-observational-memory`); model memory proposals that stay provisional until the user confirms them (`pi-compact`); and subagents that publish a live count of running, waiting-for-you, and queued work, interrupting the user only when a decision is needed (`pi-durable-subagents`).
- On voice there is no screen. When the owner asks for something that takes time, silence is indistinguishable from "did not hear me", and a misheard request may be delegated to Tachi before anyone notices. The owner wants the agent to say something first, either confirming what it understood or saying what it is about to do, so the owner always knows what state it is in.
- Pi Durable cannot be used directly: it is TypeScript, its API is marked experimental, ADR-009 retired the plugin host, and third-party Pi packages execute code. Its runtime registry replacement ("malleable" extensions) conflicts with ADR-019 §1, which keeps the body's authority non-replaceable.

## Decision

### 1. Pi Durable is a design reference, not a dependency

No Pi Durable or Pi package code is linked into the body or shipped with it. Ideas are reimplemented in Rust inside the existing crates. A PR that adopts one of the patterns below names the Pi Durable source (commit or document) it follows, per `AGENTS.md`, the same way upstream ports are recorded.

### 2. Every submission carries a scoped caller request identity

Each ingress carries a stable caller request identity scoped to its authenticated principal, agent, and logical conversation or admitted source. The identity is not authorization. Its immutable payload includes admitted attachment references and routing provenance where applicable. Reuse with changed payload is a conflict; a different principal or scope cannot take over the receipt.

A duplicate returns the existing reference and recorded state, with an outcome only when known. An interrupted, redacted, unavailable, or unresolved receipt is not proof that work never started and never permits a fresh identity to bypass reconciliation. Deduplication does not establish exactly-once external effects. Existing authorization and routing checks still apply to reads, continuation, cancellation, and execution.

Reuse the existing conversation/session intake owner and request bindings. Bridge intake already persists source identity, payload and cursor (#438). Negotiated WebSocket turn controls bind request, payload, authenticated subject and process epoch; their bounded `process_epoch` replay is not durable execution recovery after restart. Inventory these contracts and add only the missing guarantee through the one conversation owner (#376). An adapter without a natural identity persists one before first transmission.

### 3. Every tool and Node capability declares a replay class

Each tool, MCP integration, and Node capability declares one of two replay classes:

| Class | Meaning | Examples |
| --- | --- | --- |
| `safe` | Rerunning after a crash cannot cause a second effect: it only reads, or it passes an idempotency key the receiving side honors | memory search, weather, `tachi_staff` `status` and `result`, Node `info` |
| `never` (default) | An interrupted call is never rerun. The model is told it was interrupted, with any stored output, and the outcome is reported as unknown | sending a message for the owner, Tachi `start` and `cancel`, any Node action that moves or plays |

The default is `never`. A capability becomes `safe` only by an explicit declaration that a reviewer checks. For a Node capability that declaration lives in the kernel's capability catalog (#630), beside its schema and risk class. A device advertises names only and cannot declare its own capability replayable. This turns the existing `SubmissionUnknown` and unknown-device-outcome rules into one property of every capability rather than special cases. Tachi `start` stays `never` until Tachi offers a start idempotency key (#2003).

A replay label alone never overrides a stricter existing admission, approval,
or unknown-outcome rule. In particular, Node metadata does not authorize
resending an uncertain device action under a new invocation or sequence.

### 4. Persisted approval evidence does not renew execution authority

Use the existing ApprovalStore and audit path, binding each decision to the authenticated approver, exact run/call, tool and argument digest. A first-write decision record is evidence, not a reusable execution grant. Preserve boot binding, expiry, one-shot consumption, revocation and current-policy checks: a grant from an earlier boot cannot authorize execution after restart.

A denied or uncertain call is not automatically re-prompted or replayed. If the owner explicitly resumes blocked work, obtain any fresh authenticated approval required by the existing gate. Do not create a second approval store or give an extension approval authority. Persisted decisions and replay classes alone do not establish exactly-once effects.

### 5. Foreground and background work are separate

- **Foreground** work belongs to the current turn. Cancellation stops local generation/waiting and requests cancellation only where supported. It does not retract external effects or prove a Tachi/Node action stopped; unsupported and unknown outcomes remain explicit.
- **Background** work has a declared owner beyond the current turn: a delegated-completion watch, a reminder, or a scheduled delivery. Cancelling a turn leaves it alone. Restart recovery reuses the existing scheduler and source receipts; cancellation or expiry of a watch does not cancel its worker.

The existing session store owns request-to-dispatch bindings, Tachi owns run state, the existing scheduler owns scheduled watch/reminder intent, and BridgeOutbox owns delivery. Reuse those owners for #381/#63 rather than creating another task ledger or notifier. Background work records its conversation/owner scope and expiry; reminders also carry a bounded fire limit. These are proposed integration guarantees, not a claim that unattended completion is already implemented.

### 6. Memory preserves provenance; compaction preserves retained history

- Background observations are bounded, session-scoped derivatives of eligible source records. Preserve the original source reference, authenticated attribution, agent/session/principal scope and source revision. A role label, model summary or quoted material does not establish owner authorship. Owner reflection reuses original-ingress and owner-source checks; tool output, retrieved pages, attachments and third-party text do not gain authority when a model repeats them.
- Observations remain provisional. Curated-memory writes follow the existing bounded lifecycle policy; User Model activation and Soul changes retain their owner-review, provenance, revision and rollback boundaries. Preparing observations grants no new promotion authority or transcript owner.
- Pre-compaction saving may reuse prepared observations. Preparation and compaction are bounded and reconciled against source revisions before use; a failure leaves authoritative session history intact.
- Compaction and handoff change working context, not the canonical retained transcript. Read-only history search is scoped to the authorized agent/session/principal. Compaction does not rewrite tool outcomes, approvals or a surface's playback receipts.
- History preservation does not mean permanent retention. Explicit owner deletion, privacy erasure and existing retention rules apply to canonical content and observation, summary, search and cache derivatives. Deleted or redacted text must not reappear through a handoff. Minimal replay-prevention metadata follows its existing retention contract; erasure never authorizes rerunning uncertain work.

### 7. One attention state, published to every surface

The body derives an attention projection from existing conversation, approval and canonical task records: idle, listening, thinking, working, needs-you, done, and error. It is not another execution ledger. Authorized Clients receive a privacy-appropriate current view and changes through the contracts in ADR-019 §4; shared devices do not receive private question or result content merely to display a state.

- The Yonbo pet-state mapping (ADR-019 §6, S8) is driven by this projection, not by separate event wiring.
- **Quiet by default.** A needed decision, finished/stuck job, unknown outcome or budget limit can create a notification candidate. Delivery still follows #63's explicit timezone, quiet hours, expiry, snooze/mute/dismiss, owner-authorized bypass, recipient and privacy policy. Reuse BridgeOutbox and deduplicate by canonical result or occurrence identity. Execution state stays separate from delivery `accepted` / `sent` / `confirmed` / `unknown`; an unknown send is not blindly replayed.
- A question to the owner is structured: what must be decided, the options, the recommended option, and the evidence. Surfaces without buttons render it as numbered replies (ADR-019 §6).
- Body language and the attention state never carry anything that needs trust. Approval still happens only on an authenticated surface.

### 8. Say something before acting

When authorized work takes noticeable time, the body briefly states what it understood and will do. An explicit, sufficiently precise owner request already confirms intent; being slow or delegated does not by itself require another question. This matters most on voice, where the owner cannot see the attention state.

There are two forms:

| Form | When | Does the work wait? | Example |
| --- | --- | --- | --- |
| Acknowledge | intent and authorization are already sufficiently clear; the next step takes noticeable time | no additional intent-confirmation round; existing action gates still apply | "好的，我去查一下。" |
| Clarify | meaning, scope, budget or required authority remains unresolved | affected work waits for the missing decision | "要修改登录页，还是只检查问题？" |

Rules:

- **Reuse established intent and authorization.** A clear authorized L2 or acting-for-owner request gets an acknowledgement, not a repeated confirmation. Ask only for unresolved meaning, scope or required authority. Shared-device speech is still unverified speaker input and never supplies authenticated owner approval.
- **No filler for quick answers.** An instant answer gets no acknowledgement first.
- **Commit before execution.** Record the acknowledgement before the tool call and publish it promptly. A voice adapter can then play it while work proceeds; publication alone proves neither playback nor that a person heard it. A missing playback receipt must not cause replay of the action.
- **The words come from the Soul.** The body writes its acknowledgement in the Soul's Tone. A surface fallback is a nonverbal state signal or a separately owner-approved neutral phrase. Tone exemplars guide style; their contextual facts and owner lines are not a reusable phrasebook (ADR-018).
- **Confirming intent is not approval.** A spoken "yes" settles what the owner meant. It never approves an action that needs approval, which still happens on an authenticated surface (ADR-019 §6). A delegation may therefore have a voice confirmation followed by an approval card on the phone.
- **Short and closed.** A confirmation restates the task in one sentence and asks one yes-or-no question. If two readings are plausible, it offers at most those two.
- **Silence is never yes.** On voice, an unanswered confirmation is asked once more; then the conversation waits in needs-you and nothing starts.
- **Interrupting after an acknowledgement** ("不是，我是说……") requests scoped foreground cancellation (§5). Existing external effects and background Tachi runs retain their own supported control rules; the body reports requested, unsupported or unknown truthfully and never promises rollback from an interruption.
- **What is said matches the state.** An acknowledgement goes with thinking or working, a confirmation with needs-you, the result with done. What the owner hears and what the robot shows never disagree.
- **Delegation wording follows receipts.** Before acceptance, say "我准备交给 Codex"; only a canonical accepted dispatch supports "已交给 Codex". Promise later notification only when the background delivery path and authorized destination are actually configured; otherwise explain the available status/result path.

### 9. Not adopted

- **Runtime replacement of extensions or tools inside the body.** It conflicts with ADR-019 §1. Code changes reach the body by a normal restart.
- **Switching models when a provider's quota runs out** for delegated work. That belongs to Tachi profiles, not to the body.
- **Multi-agent graphs and durable subagent orchestration inside the body.** L2 work stays in Tachi (ADR-017 §3). The body's L1 subagent stays bounded and side-effect free.
- **Pi Durable's storage layer.** The body keeps its existing SQLite owners; no new durable store without a named owner.

## Consequences

Positive:

- One rule, the replay class, replaces several special cases and is checked per capability in review.
- Restarts preserve decision evidence while refusing stale grants and blind replay of uncertain effects.
- Completion delivery, reminders, and the duck's body language share one background-work model and one attention state.
- Memory preparation moves off the critical path of compaction.
- On voice the owner always knows whether the agent heard, is working, or is waiting for an answer, and a misheard request is caught before it becomes a Tachi job.

Negative:

- Every existing tool and Node capability needs a replay class; until declared, all are `never`, which may make some reads report "interrupted" after a crash.
- Request identities on `/ws/chat` and the CLI may need a contract change, which touches a protected area (`AGENTS.md`).
- Background work adds scheduler state that must be inspected, expired, and cancelled explicitly.
- Observation capture costs a background model call per turn unless batched.
- Necessary clarifications add a round trip; a poorly tuned "noticeable time" makes acknowledgements either chatty or late.

## Implementation slices

Each slice is one PR:

1. **Replay class.** Add the class to tool and Node capability metadata with default `never`; mark the clearly read-only tools and Tachi `status`/`result` as `safe`; interrupted `never` calls are reported to the model as interrupted.
2. **Request identity.** Inventory existing scoped identities and recovery limits; add only missing guarantees with payload-conflict, cross-scope and restart tests through the existing conversation owner (#376).
3. **Approval evidence.** Reuse the existing store; bind immutable decisions to exact calls without renewing grants. Test prior-boot refusal, expiry, one-shot use and explicit resumed-work approval.
4. **Background work.** Reuse existing scheduler, session bindings, Tachi receipts and outbox for #381/#63. Test independent watch/worker cancellation, occurrence deduplication and unknown delivery.
5. **Attention projection.** Publish the projection; wire the Yonbo pet-state mapping to it in S8; structured questions with numbered fallback.
6. **Memory pipeline.** Prepare bounded, provenance-preserving observations; keep promotion with existing owners; reconcile scoped history search and all derivatives with deletion and retention.
7. **Acknowledge and clarify.** Commit and publish a brief acknowledgement before slow authorized work; ask only for unresolved intent/scope/authority. Silence never satisfies a required decision. Preserve protected approvals, scoped cancellation and truthful playback/delivery state. Use a neutral approved phrase or nonverbal fallback; coordinate the voice change with its separately reviewed surface contract.

Before each slice, check upstream and Pi Durable for equivalent work and record the result, per `AGENTS.md`.

## Acceptance

This ADR is implemented when:

1. every tool and Node capability has a declared replay class, and a test kills the body during a `never` call and shows it is not rerun and is reported as interrupted;
2. an identical authorized resubmission returns the existing reference/state; changed payloads conflict, cross-scope access fails, and unknown/redacted receipts never create replacement work;
3. a restart preserves approval evidence but refuses prior-boot execution grants, does not automatically re-prompt or replay an uncertain call, and requires any fresh approval when the owner explicitly resumes it;
4. cancelling local foreground work leaves independent background work alone; restart recovery creates one canonical notification candidate per occurrence, while lost sends stay unknown without blind retry and watch cancellation does not imply worker cancellation;
5. a client that attaches late receives the current attention state, and the robot shows the mapped pose for done and needs-you during an acceptance run;
6. compaction preserves retained canonical messages; authorized history search finds pre-handoff content, while deleted or out-of-scope content and its derivatives remain unavailable;
7. a controlled voice run records acknowledgement playback before the first slow tool result while instant answers get no filler; a clear authorized request does not trigger redundant intent confirmation; unanswered required clarifications start no affected work; spoken "yes" never grants approval; and cancellation reports supported, requested or unknown outcomes without claiming external rollback.

## Open questions

- Which gaps remain after comparing durable bridge intake, process-epoch WebSocket replay, CLI and cron identities? (Slice 2.)
- Where do observations live: the existing session store, or memory with a session scope (ADR-010)?
- Should observation capture run every turn, or batch at idle to limit cost?
- Which attention states should notify the owner on the phone, and which only change the robot's pose?
- What expiry and fire limit should reminders default to?
- What counts as "noticeable time" on voice before an acknowledgement is needed (for example, about one second)?
- Which ambiguous voice cases need clarification beyond the established intent and authorization, without asking again for an already explicit request?

## References

- [Pi Durable announcement](https://earendil.com/posts/pi-durable/), Earendil, 2026-10-01
- Pi Durable source: `earendil-works/pi` `packages/durable` (README and examples)
- Pi packages used as references: `pi-observational-memory`, `pi-compact`, `pi-durable-subagents`
- [ADR-010: Memory authority boundaries](./ADR-010-memory-authority-boundaries.md)
- [ADR-016: Growth proposed by the agent, approved by the owner](./ADR-016-growth-proposed-by-agent-approved-by-owner.md)
- [ADR-017: One personal agent body, edge devices, and delegation through Tachi](./ADR-017-personal-agent-body-edges-and-delegation.md)
- [ADR-018: Soul character model](./ADR-018-soul-character-model.md)
- [ADR-019: One body, many hands](./ADR-019-one-body-many-hands.md)
- Issues #63, #376, #377, #381, #438, #514, #535; [kckylechen1/Tachi#2003](https://github.com/kckylechen1/Tachi/issues/2003)
