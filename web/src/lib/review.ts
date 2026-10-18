import { getToken } from './auth';
import { apiOrigin, basePath } from './basePath';
export type Layer = 'identity' | 'principles' | 'tone' | 'story';
export interface Revision {
    revision: number;
    source: string;
    created_at_unix: number;
    proposal_id?: number;
    rolled_back_from?: number;
    value: unknown;
}
export interface Proposal {
    id: number;
    layer: Layer;
    proposal: string;
    rationale: string;
    trait_key?: string;
    level?: string;
    story_kind?: string;
    retire_index?: number;
    retire_target?: { kind: string; text: string };
    target_revision?: number;
    session_ref?: string;
    created_at_unix: number;
}
export interface Candidate {
    id: string;
    kind: string;
    statement: string;
    semantic_key: string;
    scope: string;
    evidence: string;
    created_at_unix: number;
}
export interface DialDrift {
    key: string;
    anchor_level: string;
    current_level: string;
    /** Signed distance from the anchor; positive means the dial moved up. */
    steps: number;
}
export interface SoulAnchor {
    /** Null for a default anchor, which the gateway derives and never stores. */
    id: number | null;
    tone_revision_id: number | null;
    story_revision_id: number | null;
    set_at_unix: number;
    source: 'default' | 'owner';
}
export interface SoulDrift {
    anchor: SoulAnchor;
    dials: DialDrift[];
    story_added: number;
    story_retired: number;
    approved_proposals_since_anchor: number;
}
export interface CandidateCursor {
    created_at_unix: number;
    id: string;
}
export interface CandidatePage {
    candidates: Candidate[];
    next_cursor: CandidateCursor | null;
}
export const CANDIDATE_PAGE_LIMIT = 50;
export function candidatePagePath(cursor: CandidateCursor | null = null): string {
    return `/api/user-model/candidates?limit=${CANDIDATE_PAGE_LIMIT}${cursor ? `&before_created_at=${cursor.created_at_unix}&before_id=${encodeURIComponent(cursor.id)}` : ''}`;
}
export function appendCandidatePage(current: CandidatePage, page: CandidatePage): CandidatePage {
    const seen = new Set(current.candidates.map(candidate => candidate.id));
    return { ...page, candidates: [...current.candidates, ...page.candidates.filter(candidate => {
        if (seen.has(candidate.id)) return false;
        seen.add(candidate.id);
        return true;
    })] };
}
export async function loadCandidatePage(cursor: CandidateCursor | null, signal: AbortSignal): Promise<CandidatePage> {
    const page = await reviewFetch<CandidatePage>(candidatePagePath(cursor), signal);
    // Old gateways ignore unknown query fields and return unbounded history.
    // Require the opted-in contract rather than rendering that legacy response.
    if (!page || !Object.prototype.hasOwnProperty.call(page, 'next_cursor')) throw new ReviewError(409, 'pagination_unavailable');
    if (!Array.isArray(page.candidates) || page.candidates.length > CANDIDATE_PAGE_LIMIT) throw new Error('Invalid candidate page');
    const next = page.next_cursor;
    if (next !== null) {
        const last = page.candidates[page.candidates.length - 1];
        if (!next || !Number.isSafeInteger(next.created_at_unix) || next.created_at_unix < 0 || typeof next.id !== 'string' || !next.id || !last || next.id !== last.id || next.created_at_unix !== last.created_at_unix
            || (cursor && (next.created_at_unix > cursor.created_at_unix || (next.created_at_unix === cursor.created_at_unix && next.id >= cursor.id)))) throw new Error('Invalid candidate cursor');
    }
    return page;
}
export interface Receipt {
    period_from_unix: number;
    ran_at_unix: number;
    proposals_created: number;
    user_model_candidates_created: number;
    outcome: string;
    drift?: SoulDrift | null;
}
export type InboxItem = {
    id: string;
    kind: 'soul_proposal';
    agent: string;
    item: Proposal;
} | {
    id: string;
    kind: 'user_model_candidate';
    item: Candidate;
} | {
    id: string;
    kind: 'reflection_receipt';
    agent: string;
    item: Receipt;
};
export interface Inbox {
    items: InboxItem[];
    total: number;
    next_offset: number | null;
}
export interface Soul {
    agent: string;
    identity?: Revision;
    principles?: Revision;
    story?: Revision;
    tone: {
        configured: unknown;
        stored?: Revision;
        effective?: Record<string, string>;
        sources?: Record<string, {
            kind: string;
            persona?: string;
            card?: string | null;
            revision?: number;
        }>;
    };
}
export interface Head {
    id: string;
    semantic_key: string;
    statement: string;
    scope: string;
    authority: string;
    source_candidate?: string;
    kind: string;
}
export interface CandidateHistory {
    candidate: Candidate;
    review_state: string;
    review_receipts: {
        id: string;
        action: string;
        note?: string;
        at_unix: number;
    }[];
}
// Only the gateway's own drift shape is trusted; anything else (an older
// gateway answering with an empty object) shows no card instead of a broken one.
export function readDrift(value: unknown): SoulDrift | null {
    if (!value || typeof value !== 'object' || !('drift' in value)) return null;
    const drift = value.drift as Partial<SoulDrift> | null;
    if (!drift || typeof drift !== 'object' || !drift.anchor || !Array.isArray(drift.dials)) return null;
    if (drift.anchor.source !== 'default' && drift.anchor.source !== 'owner') return null;
    if (typeof drift.story_added !== 'number' || typeof drift.story_retired !== 'number' || typeof drift.approved_proposals_since_anchor !== 'number') return null;
    return drift as SoulDrift;
}
export function driftIsEmpty(drift: SoulDrift): boolean {
    return drift.story_added === 0 && drift.story_retired === 0 && drift.dials.every(dial => dial.steps === 0);
}
export function signedSteps(steps: number): string {
    return steps > 0 ? `+${steps}` : steps < 0 ? `\u2212${-steps}` : '0';
}
// Keep and roll back both name the Tone and Story heads the owner is looking
// at, so a change made elsewhere since the read stops the action (409).
export function anchorRequest(soul: Soul, action: 'keep' | 'rollback'): {
    path: string;
    body: unknown;
} {
    return {
        path: action === 'keep' ? '/api/soul/anchor' : '/api/soul/anchor/rollback',
        body: { agent: soul.agent, expected_tone_revision: soul.tone.stored?.revision ?? 0, expected_story_revision: soul.story?.revision ?? 0 },
    };
}
export class ReviewError extends Error {
    constructor(public status: number, public code?: string, public field?: string, public reason?: string) { super(`Review HTTP ${status}`); }
}
// Owner-only responses have several legacy envelopes. Preserve the status,
// and keep re-pair local to this surface instead of changing anonymous routes.
export async function reviewFetch<T>(path: string, signal?: AbortSignal, body?: unknown): Promise<T> {
    const token = getToken();
    const response = await fetch(`${apiOrigin}${basePath}${path}`, {
        method: body === undefined ? 'GET' : 'POST', signal,
        headers: { ...(token ? { Authorization: `Bearer ${token}` } : {}), ...(body === undefined ? {} : { 'Content-Type': 'application/json' }) },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
        cache: 'no-store',
    });
    if (!response.ok) {
        const envelope: unknown = await response.json().catch(() => null);
        const code = envelope && typeof envelope === 'object' && 'code' in envelope && typeof envelope.code === 'string' ? envelope.code : undefined;
        const field = envelope && typeof envelope === 'object' && 'field' in envelope && typeof envelope.field === 'string' ? envelope.field : undefined;
        const reason = envelope && typeof envelope === 'object' && 'reason' in envelope && typeof envelope.reason === 'string' ? envelope.reason : undefined;
        throw new ReviewError(response.status, code, field, reason);
    }
    return response.json() as Promise<T>;
}
export type Decision = 'accept' | 'reword' | 'narrow' | 'dismiss';
export function canReword(item: InboxItem): boolean {
    return item.kind === 'user_model_candidate' || (item.kind === 'soul_proposal' && (item.item.layer === 'principles' || (item.item.layer === 'story' && item.item.retire_index === undefined)));
}
export function canNarrow(candidate: Candidate): boolean {
    return candidate.scope === 'global' || /^session:\S.*$/.test(candidate.scope) && candidate.scope.trim() === candidate.scope;
}
export function validNarrowScope(candidate: Candidate, scope: string): boolean {
    return canNarrow(candidate) && scope.startsWith('session:') && !!scope.slice(8).trim() && scope.trim() === scope && (candidate.scope === 'global' || scope === candidate.scope);
}
// Use the same canonical row for the displayed replacement and approval CAS.
export function displayedHead(candidate: Candidate, heads: readonly Head[]): Head | undefined {
    return heads.find(head => head.semantic_key === candidate.semantic_key);
}
export function decisionRequest(item: InboxItem, decision: Decision, text: string, scope: string, heads: readonly Head[]): {
    path: string;
    body: unknown;
} {
    if (item.kind === 'reflection_receipt')
        throw new Error('Receipt has no review action');
    if (decision === 'reword' && (!canReword(item) || !text.trim()))
        throw new Error('Invalid reword');
    if (decision === 'narrow' && (item.kind !== 'user_model_candidate' || !validNarrowScope(item.item, scope)))
        throw new Error('Invalid scope');
    if (item.kind === 'soul_proposal')
        return { path: `/api/soul/proposals/${item.item.id}/resolve`, body: { agent: item.agent, resolution: decision === 'dismiss' ? 'dismissed' : 'accepted', ...(decision === 'reword' ? { final_text: text.trim() } : {}) } };
    return { path: `/api/user-model/candidates/${encodeURIComponent(item.item.id)}/review`, body: { ...(decision === 'dismiss' ? {} : { expected_head: { id: displayedHead(item.item, heads)?.id ?? null } }), action: decision === 'dismiss' ? 'reject' : decision === 'reword' ? 'accept' : decision, ...(decision === 'reword' ? { final_text: text.trim() } : {}), ...(decision === 'narrow' ? { narrowed_scope: scope } : {}) } };
}

export function reflectionOutcome(outcome: string): { key: string; detail: string } {
    const separator = outcome.indexOf(':');
    const prefix = separator < 0 ? outcome : outcome.slice(0, separator);
    const known = ['clock_started', 'nothing_to_reflect_on', 'proposal_queue_full', 'ok', 'model_call_failed', 'storage_write_failed'];
    return known.includes(prefix)
        ? { key: `review.outcome_${prefix}`, detail: separator < 0 ? '' : outcome.slice(separator + 1).trim() }
        : { key: 'review.outcome_unknown', detail: outcome };
}
export function validationMessage(error: ReviewError): string {
    return error.reason === 'soul-tone-challenge-up-only' ? 'review.invalid_challenge_up_only' : error.field === 'tone' ? 'review.invalid_tone' : error.field === 'level' ? 'review.invalid_level' : 'review.invalid';
}
