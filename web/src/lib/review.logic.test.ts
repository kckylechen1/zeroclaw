import assert from 'node:assert/strict';
// Browser globals for the existing base-path and auth modules; no real secrets.
Object.assign(globalThis, { window: { __ZEROCLAW_BASE__: '/controller' }, localStorage: { getItem: () => 'synthetic-owner' } });
const { decisionRequest, canReword, reviewFetch, ReviewError } = await import('./review');
const user = { id: 'user_model:x', kind: 'user_model_candidate' as const, item: { id: 'x/y', kind: 'preference', statement: 'Short answers', semantic_key: 'style', scope: 'global', evidence: 'source', created_at_unix: 1 } };
assert.deepEqual(decisionRequest(user, 'dismiss', '', '', []), { path: '/api/user-model/candidates/x%2Fy/review', body: { action: 'reject' } });
assert.deepEqual(decisionRequest(user, 'reword', '  Brief when practical  ', '', []), { path: '/api/user-model/candidates/x%2Fy/review', body: { action: 'accept', final_text: 'Brief when practical', expected_head: { id: null } } });
assert.deepEqual(decisionRequest(user, 'narrow', '', 'session:A', []).body, { action: 'narrow', narrowed_scope: 'session:A', expected_head: { id: null } });
assert.throws(() => decisionRequest(user, 'narrow', '', 'global', []));
const soul = { id: 'soul:nova:7', kind: 'soul_proposal' as const, agent: 'nova', item: { id: 7, layer: 'growth' as const, proposal: 'Shared shorthand', rationale: 'Owner reaction', created_at_unix: 1 } };
assert.equal(canReword(soul), true);
assert.deepEqual(decisionRequest(soul, 'accept', '', '', []).body, { agent: 'nova', resolution: 'accepted' });
assert.deepEqual(decisionRequest(soul, 'dismiss', '', '', []).body, { agent: 'nova', resolution: 'dismissed' });
assert.throws(() => decisionRequest(soul, 'narrow', '', 'session:A', []));
assert.equal(canReword({ ...soul, item: { ...soul.item, retire_index: 0 } }), false);
assert.throws(() => decisionRequest({ ...soul, item: { ...soul.item, layer: 'voice' } }, 'reword', 'high', '', []));
let calls = 0;
globalThis.fetch = async (input, options) => {
    calls++;
    assert.equal(input, '/controller/api/review/inbox');
    assert.equal((options?.headers as Record<string, string>).Authorization, 'Bearer synthetic-owner');
    assert.equal(options?.cache, 'no-store');
    return new Response(JSON.stringify({ code: 'store_unavailable', error: 'synthetic' }), { status: 503 });
};
await assert.rejects(reviewFetch('/api/review/inbox'), (error: unknown) => error instanceof ReviewError && error.status === 503 && error.code === 'store_unavailable');
globalThis.fetch = async () => new Response('', { status: 401 });
await assert.rejects(reviewFetch('/api/review/inbox'), (error: unknown) => error instanceof ReviewError && error.status === 401);
assert.equal(calls, 1);
console.log('review action binding and owner HTTP status boundaries passed');

for (const code of ['proposal_stale', 'revision_conflict', 'proposal_already_resolved', 'candidate_already_reviewed', 'head_conflict', 'unknown_agent']) {
    globalThis.fetch = async () => new Response(JSON.stringify({ code, error: 'untrusted prose' }), { status: code === 'unknown_agent' ? 404 : 409 });
    await assert.rejects(reviewFetch('/api/review/inbox'), (error: unknown) => error instanceof ReviewError && error.code === code);
}

for (const scope of ['agent:nova', 'channel:telegram', 'session:A']) {
    const scoped = { ...user, item: { ...user.item, scope } };
    assert.throws(() => decisionRequest(scoped, 'narrow', '', 'session:B', []));
    if (scope === 'session:A') assert.deepEqual(decisionRequest(scoped, 'narrow', '', scope, []).body, { action: 'narrow', narrowed_scope: scope, expected_head: { id: null } });
    else assert.throws(() => decisionRequest(scoped, 'narrow', '', scope, []));
}
assert.throws(() => decisionRequest(user, 'narrow', '', 'session:  ', []));

const displayed = { id: 'shown-A', semantic_key: 'style', kind: 'preference', statement: 'Currently shown', scope: 'global', authority: 'owner_authored' };
for (const decision of ['accept', 'reword', 'narrow'] as const) {
    const payload = decisionRequest(user, decision, 'Reviewed wording', 'session:A', [displayed]).body as { expected_head: { id: string | null } };
    assert.deepEqual(payload.expected_head, { id: 'shown-A' });
    const noMatch = decisionRequest(user, decision, 'Reviewed wording', 'session:A', [{ ...displayed, semantic_key: 'other' }]).body as { expected_head: { id: string | null } };
    assert.deepEqual(noMatch.expected_head, { id: null });
}
assert.deepEqual(decisionRequest(user, 'dismiss', '', '', [displayed]).body, { action: 'reject' });

const { reflectionOutcome, validationMessage } = await import('./review');
for (const outcome of ['clock_started', 'nothing_to_reflect_on', 'proposal_queue_full', 'ok', 'storage_write_failed']) {
    assert.deepEqual(reflectionOutcome(outcome), { key: `review.outcome_${outcome}`, detail: '' });
}
assert.deepEqual(reflectionOutcome('model_call_failed: timeout: synthetic'), { key: 'review.outcome_model_call_failed', detail: 'timeout: synthetic' });
assert.deepEqual(reflectionOutcome('future_status'), { key: 'review.outcome_unknown', detail: 'future_status' });
for (const field of ['voice', 'level', 'unexpected_field']) {
    globalThis.fetch = async () => new Response(JSON.stringify({ code: 'invalid', field, error: 'untrusted prose' }), { status: 400 });
    await assert.rejects(reviewFetch('/api/soul/proposals/19/resolve'), (error: unknown) => error instanceof ReviewError && error.field === field && validationMessage(error) === (field === 'unexpected_field' ? 'review.invalid' : `review.invalid_${field}`));
}
