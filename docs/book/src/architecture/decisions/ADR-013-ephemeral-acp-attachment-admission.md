---
id: ADR-013
title: Keep ephemeral ACP sessions free of WorkClaims until public admission is complete
date: 2026-09-20
status: proposed
relates-to:
  - docs/book/src/architecture/background-work-lifecycle.md
  - https://github.com/kckylechen1/zeroclaw/issues/266
  - https://github.com/kckylechen1/zeroclaw/issues/270
  - https://github.com/kckylechen1/tachi/issues/1678
---

# ADR-013: Keep Ephemeral ACP Sessions Free Of WorkClaims Until Public Admission Is Complete

## Status

The owner selected the direction in this record on 2026-09-20. The ADR remains
proposed until an independent reviewer accepts this exact documentation head.
It records a disabled boundary, not production activation.

## Context

ZeroClaw has a Host-owned ACP carrier that can launch and control one native
session and can send bounded session facts to Tachi's public
`tachi_agent_eval` facade. The frozen EPHEMERAL contract in issues #266 and
issue #270 forbids TaskRef, AttemptRef, and WorkClaim state for this route.

Tachi's current public attachment contract cannot satisfy that rule. At public
Tachi `main` commit
[`817a673f45c8bcdef14c5ff8bf89d84ffc05aeba`](https://github.com/kckylechen1/tachi/commit/817a673f45c8bcdef14c5ff8bf89d84ffc05aeba),
`attach_session` requires all of these objects:

| Required object | Current public behavior | Authority effect |
| --- | --- | --- |
| Current Host identity and admission | MCP initialization admits a locally asserted identity and records a generated connection/admission row. | Identifies the current Host connection. It does not launch or control the ACP process. |
| Distinct worker identity | The worker identity must differ from the Host identity. | Names the worker bound to the WorkClaim. |
| Fresh active WorkClaim at an exact transition revision | `tachi_task(action="claim")` creates the claim with scope, mode, role, expected head, lease, and holder identity. | Establishes canonical work ownership in Tachi. It does not transfer the external process handle. |
| Worker ACP capability grant | `AgentIdentity.capability_json.acp` must admit the selected tool profile and capability class. | Authorizes the closed Tachi descriptor projection only. It grants no raw shell, file, Git, credential, or process authority. |
| Host admission receipt reference | The request must name the exact current Host admission id or connection id. | Binds receipt writes to the current admitted Host connection. |
| Existing remote ACP session | The Host supplies the adapter connection identity and remote session id. | Remains launched and controlled by ZeroClaw. Tachi records the binding only. |

The WorkClaim requirement is enforced inside the attachment transaction, along
with current Host admission, exact claim revision and freshness, and the stored
ACP grant. Exact replay repeats those checks. An idempotency key is not an
admission grant.

The current public construction path has two concrete gaps:

1. Agent admission and public claim creation insert an `AgentIdentity` with
   `capability_json = NULL`. No production/public operation provisions the
   required closed ACP grant. The grant writes in the inspected sources are test
   setup only.
2. Initialization records a generated admission and connection reference in
   server state, but the public initialization result does not return the opaque
   reference required by `attach_session`.

The attachment path does not read a TaskRef, AttemptRef, assignment row, or
dispatch record. That makes a future narrow WorkClaim exception technically
possible, but it does not make the present public route usable.

## Decision

Retain the EPHEMERAL no-WorkClaim clause. Keep the LAST-A attachment route
disabled and #270 blocked.

Do not substitute a fabricated claim, invented receipt reference, fixture,
direct database write, caller-asserted grant, or second receipt ledger. A
carrier-green session is not a production-spine-green session.

ZeroClaw remains the sole lifecycle owner for an EPHEMERAL ACP session:

| Operation | ZeroClaw Host | Tachi |
| --- | --- | --- |
| Launch | Launches exactly one native session and retains its process/session handle. | Does not launch through `attach_session`. |
| Prompt or correction | Executes the bounded operation and reports the actual result. | Records admitted request/result facts. A request is not proof of execution. |
| Cancel or interrupt | Invokes the carrier and supplies authoritative confirmation. | Records typed request/result facts. Unsupported or refused remains nonterminal. |
| Reconnect or replay | Reattaches to the same remote session without another launch. | Rebinds a fresh Host admission and accepts unchanged replay only after reauthorization. |
| Redispatch or restart | Performs no implicit EPHEMERAL redispatch. | Attachment admission is not dispatch. Durable recovery requires a separately selected DURABLE route. |
| Collection and delivery | Reports actual collection; Parent owns user-facing presentation. | Does not infer collection, cleanup, semantic success, or delivery from receipt acceptance. |

## Public Tachi interfaces required before reconsideration

Reconsidering a narrow WorkClaim exception requires an exact Tachi candidate
head with both interfaces below. This ADR does not authorize or implement them.

### Authenticated ACP grant provisioning

Tachi must expose a public, authenticated operation that provisions or revises
the closed ACP grant for one admitted worker identity. The request must accept
only canonical tool-profile and capability-class names, use an exact expected
grant revision or equivalent CAS boundary, and return a durable public-safe
receipt containing the identity, resulting revision, canonical admitted set,
and policy digest. It must reject arbitrary capability JSON, unknown fields,
caller-selected issuer authority, stale revisions, and partial writes.

### Current Host admission receipt return

Tachi initialization, or one public read operation bound to that initialized
connection, must return an opaque public-safe `admission_receipt_ref` for the
exact current Host identity and connection. The client must not invent it or
discover it through database access. Reconnect must return a fresh reference;
an old connection reference must not authorize new facts.

Both interfaces need independent exact-head review, public protocol tests, and
a live admission proof before #270 can reopen the clause decision.

## Acceptance and supersession

This record becomes accepted when an independent reviewer confirms that the
document matches the pinned public contracts and does not weaken #266 D1-D7.
Acceptance keeps LAST-A disabled.

A later ADR may supersede this decision only after the two public interfaces
exist and are independently accepted. That ADR must name the exact Tachi
revision and may permit at most one WorkClaim reference solely for attachment,
receipt, replay, and intervention admission. It must continue to forbid
TaskRef/AttemptRef creation, Tachi dispatch or redispatch, lifecycle transfer,
raw-tool grants, and inferred completion, cancellation, collection, cleanup,
or delivery.

## References

- [Issue #266: frozen LAST-A contract](https://github.com/kckylechen1/zeroclaw/issues/266)
- [Issue #270: live ACP attachment gate](https://github.com/kckylechen1/zeroclaw/issues/270)
- [Tachi claim admission and WorkClaim creation](https://github.com/kckylechen1/tachi/blob/817a673f45c8bcdef14c5ff8bf89d84ffc05aeba/crates/tachi-server/src/claims_ops.rs#L27-L163)
- [Tachi public attachment handler](https://github.com/kckylechen1/tachi/blob/817a673f45c8bcdef14c5ff8bf89d84ffc05aeba/crates/tachi-server/src/agent_eval/attachment.rs#L74-L217)
- [Tachi ACP grant validation](https://github.com/kckylechen1/tachi/blob/817a673f45c8bcdef14c5ff8bf89d84ffc05aeba/crates/memcore/src/db/harness_session_attachments.rs#L314-L459)
- [Tachi Host, claim, and policy revalidation](https://github.com/kckylechen1/tachi/blob/817a673f45c8bcdef14c5ff8bf89d84ffc05aeba/crates/memcore/src/db/harness_session_attachments.rs#L753-L904)
- [Tachi atomic attachment writer](https://github.com/kckylechen1/tachi/blob/817a673f45c8bcdef14c5ff8bf89d84ffc05aeba/crates/memcore/src/db/harness_session_attachments.rs#L997-L1095)
- [Background work lifecycle](../background-work-lifecycle.md)
