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

This record keeps ADR-017 and ADR-019 unchanged. It names a small set of durability and attention rules for the body, taken as design references from Pi Durable, adds an owner rule for acknowledging and confirming before acting (§8), and records which Pi Durable ideas are deliberately not adopted.

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

### 2. Every submission carries a caller request identity

Every way into a conversation (bridge intake, `/ws/chat`, CLI, cron, and scheduled reminders) carries a request identity chosen by the caller. A resubmission with the same identity returns the original turn and its outcome instead of starting a new one. Bridge intake already has a source message identity (#438); this extends the same guarantee to the other surfaces through the one conversation owner (#376). Where a surface has no natural identity, the adapter mints one and persists it before sending.

### 3. Every tool and Node capability declares a replay class

Each tool, MCP integration, and Node capability declares one of two replay classes:

| Class | Meaning | Examples |
| --- | --- | --- |
| `safe` | Rerunning after a crash cannot cause a second effect: it only reads, or it passes an idempotency key the receiving side honors | memory search, weather, `tachi_staff` `status` and `result`, Node `info` |
| `never` (default) | An interrupted call is never rerun. The model is told it was interrupted, with any stored output, and the outcome is reported as unknown | sending a message for the owner, Tachi `start` and `cancel`, any Node action that moves or plays |

The default is `never`. A capability becomes `safe` only by an explicit declaration that a reviewer checks. This turns the existing `SubmissionUnknown` and unknown-device-outcome rules into one property of every capability rather than special cases. Tachi `start` stays `never` until Tachi offers a start idempotency key (#2003).

### 4. Approval decisions are stored with the call, first write wins

An approval decision is stored with the tool call it belongs to before the call runs. After a restart the stored decision is read, not asked again, and a denied call is never re-asked automatically. A second write for the same call is refused. This sits inside the existing approval chain and audit trail; it gives no extension a way to approve (ADR-019 §1).

### 5. Foreground and background work are separate

- **Foreground** work belongs to the current turn. Cancelling the turn stops it and everything it started.
- **Background** work belongs to the conversation, not to the turn: watching a delegated Tachi run for completion, an event-triggered reminder, a scheduled delivery. Cancelling the turn leaves it alone. It survives restarts through the scheduler and is stopped only by an explicit cancel of that work.

This makes the separation in ADR-017 §7 concrete for #381 completion delivery and #63 proactive delivery. Background work records its owner (the conversation) and an expiry; reminders also carry a fire limit and owner scope, as the PRD requires.

### 6. Memory is prepared in the background; history is never deleted

- During a conversation, the body records **observations** (concrete events or decisions in the owner's own messages) as background work at turn end. They are session-scoped and are not long-term memory.
- Promoting an observation to long-term memory follows ADR-010 and ADR-016 unchanged: only owner- or agent-originated content qualifies, and anything that would change the Soul is a proposal the owner approves. This ADR widens nothing.
- Pre-compaction saving uses the prepared observations, so compaction does not need an extra model call to decide what to keep.
- Compaction starts in the background before the context is full and never deletes stored messages. A handoff may start a new context from a note; older messages stay searchable through a read-only history tool with replay class `safe`.

### 7. One attention state, published to every surface

The body publishes one committed attention projection per conversation: idle, listening, thinking, working (with counts of running delegated jobs), needs-you (with the pending question or approval), done, and error. Clients receive the current value when they attach and then only changes, through the gateway contracts in ADR-019 §4.

- The Yonbo pet-state mapping (ADR-019 §6, S8) is driven by this projection, not by separate event wiring.
- **Quiet by default.** The body interrupts the owner only for: a decision it needs, a finished job, a stuck job, an unknown outcome, or a budget limit. Everything else is visible in the attention state without a notification.
- A question to the owner is structured: what must be decided, the options, the recommended option, and the evidence. Surfaces without buttons render it as numbered replies (ADR-019 §6).
- Body language and the attention state never carry anything that needs trust. Approval still happens only on an authenticated surface.

### 8. Say something before acting

When the owner will have to wait, the body first tells the owner what it understood and what it is about to do. This matters most on voice, where the owner cannot see the attention state.

There are two forms:

| Form | When | Does the work wait? | Example |
| --- | --- | --- | --- |
| Acknowledge | the intent is clear and the next step takes noticeable time (a slow tool call, a search, a delegation that is already confirmed) | no: the work starts right after | "好的，我去查一下。" |
| Confirm | the intent is ambiguous, or the next step is costly, slow, or hard to undo | yes: nothing starts until the owner answers | "你是想让 Codex 把登录页的 bug 修掉，对吗？" |

Rules:

- **Confirm is required by default** before starting a Tachi job (L2) and before any tool that acts for the owner (ADR-019 §5). For other requests, the model confirms when the intent is unclear. The owner may relax the default per surface.
- **No filler for quick answers.** An instant answer gets no acknowledgement first.
- **Spoken first, committed first.** The acknowledgement or confirmation is sent to the surface as soon as it is produced, before any tool call runs, so on voice it plays while the work starts. It is committed to the transcript before the tool call, so after a restart the body knows what it told the owner.
- **The words come from the Soul.** The model writes the acknowledgement in the Soul's Tone. If the model gives none and the first step turns out slow, the surface shows the attention state instead (the robot's pose, a short earcon, a typing indicator), or uses a short phrase from the owner-made Tone exemplars (ADR-018). The body never makes up other words on the agent's behalf.
- **Confirming intent is not approval.** A spoken "yes" settles what the owner meant. It never approves an action that needs approval, which still happens on an authenticated surface (ADR-019 §6). A delegation may therefore have a voice confirmation followed by an approval card on the phone.
- **Short and closed.** A confirmation restates the task in one sentence and asks one yes-or-no question. If two readings are plausible, it offers at most those two.
- **Silence is never yes.** On voice, an unanswered confirmation is asked once more; then the conversation waits in needs-you and nothing starts.
- **Interrupting after an acknowledgement** ("不是，我是说……") cancels the foreground work it started (§5). Background work that already started, such as a Tachi run, keeps going until it is cancelled explicitly. That is why delegation is confirmed before it starts, rather than cancelled after.
- **What is said matches the state.** An acknowledgement goes with thinking or working, a confirmation with needs-you, the result with done. What the owner hears and what the robot shows never disagree.
- **Delegation says who and when.** The acknowledgement after a confirmed delegation names who does the work and that the result comes back later: "好的，我交给 Codex，做完告诉你。"

### 9. Not adopted

- **Runtime replacement of extensions or tools inside the body.** It conflicts with ADR-019 §1. Code changes reach the body by a normal restart.
- **Switching models when a provider's quota runs out** for delegated work. That belongs to Tachi profiles, not to the body.
- **Multi-agent graphs and durable subagent orchestration inside the body.** L2 work stays in Tachi (ADR-017 §3). The body's L1 subagent stays bounded and side-effect free.
- **Pi Durable's storage layer.** The body keeps its existing SQLite owners; no new durable store without a named owner.

## Consequences

Positive:

- One rule, the replay class, replaces several special cases and is checked per capability in review.
- Restarts no longer risk a second approval prompt or a second effect from an interrupted call.
- Completion delivery, reminders, and the duck's body language share one background-work model and one attention state.
- Memory preparation moves off the critical path of compaction.
- On voice the owner always knows whether the agent heard, is working, or is waiting for an answer, and a misheard request is caught before it becomes a Tachi job.

Negative:

- Every existing tool and Node capability needs a replay class; until declared, all are `never`, which may make some reads report "interrupted" after a crash.
- Request identities on `/ws/chat` and the CLI may need a contract change, which touches a protected area (`AGENTS.md`).
- Background work adds scheduler state that must be inspected, expired, and cancelled explicitly.
- Observation capture costs a background model call per turn unless batched.
- Confirmations add a round trip before delegation and acting for the owner; a poorly tuned "noticeable time" makes the agent either chatty or silent.

## Implementation slices

Each slice is one PR:

1. **Replay class.** Add the class to tool and Node capability metadata with default `never`; mark the clearly read-only tools and Tachi `status`/`result` as `safe`; interrupted `never` calls are reported to the model as interrupted.
2. **Request identity.** Check which surfaces already carry a request identity; add one where missing through the conversation owner (#376), as a versioned contract change if needed.
3. **Approval memo.** Persist the approval decision with the call before execution; refuse a second write; restart tests.
4. **Background work.** Foreground versus background ownership; Tachi completion watching as the first background work (#381), then proactive delivery (#63).
5. **Attention projection.** Publish the projection; wire the Yonbo pet-state mapping to it in S8; structured questions with numbered fallback.
6. **Memory pipeline.** Observations at turn end, pre-compaction save from observations, background compaction, history search tool, event-triggered reminders as background work.
7. **Acknowledge and confirm.** Send text produced before tool calls to surfaces immediately (voice first) and commit it before the call; require confirmation before Tachi `start` and acting-for-owner tools; silence starts nothing; interrupting cancels foreground work; Tone-exemplar or non-verbal fallback when the first step is slow. On the Yonbo adapter this lands with S5.

Before each slice, check upstream and Pi Durable for equivalent work and record the result, per `AGENTS.md`.

## Acceptance

This ADR is implemented when:

1. every tool and Node capability has a declared replay class, and a test kills the body during a `never` call and shows it is not rerun and is reported as interrupted;
2. resubmitting a request with the same identity on each surface returns the original turn, with tests;
3. a restart between an approval decision and execution neither asks again nor runs twice;
4. cancelling a turn leaves background work running; a background Tachi watch and a reminder each survive a restart and fire once;
5. a client that attaches late receives the current attention state, and the robot shows the mapped pose for done and needs-you during an acceptance run;
6. compaction deletes no stored messages, and a message from before a handoff can be found through the history tool;
7. on voice, a request that needs a slow step is heard acknowledged before any tool result while an instant answer gets no filler; no Tachi job or acting-for-owner call starts without a confirmed intent; an unanswered confirmation starts nothing; a spoken "yes" never satisfies an approval; and interrupting after an acknowledgement cancels the foreground work, with tests.

## Open questions

- Do `/ws/chat`, the CLI, and cron already carry a request identity that the conversation owner deduplicates? (Slice 2.)
- Where do observations live: the existing session store, or memory with a session scope (ADR-010)?
- Should observation capture run every turn, or batch at idle to limit cost?
- Which attention states should notify the owner on the phone, and which only change the robot's pose?
- What expiry and fire limit should reminders default to?
- What counts as "noticeable time" on voice before an acknowledgement is needed (for example, about one second)?
- May the owner skip confirmation for delegations phrased precisely, or for a trusted surface such as the CLI?

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
