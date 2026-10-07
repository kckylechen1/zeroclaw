import assert from 'node:assert/strict';
// Browser globals for the existing base-path and auth modules; no real secrets.
Object.assign(globalThis, { window: { __ZEROCLAW_BASE__: '/controller' }, localStorage: { getItem: () => 'synthetic-owner' } });
const { decisionRequest, canReword, reviewFetch, ReviewError } = await import('./review');
const user = { id: 'user_model:x', kind: 'user_model_candidate' as const, item: { id: 'x/y', kind: 'preference', statement: 'Short answers', semantic_key: 'style', scope: 'global', evidence: 'source', created_at_unix: 1 } };
assert.deepEqual(decisionRequest(user, 'dismiss', '', ''), { path: '/api/user-model/candidates/x%2Fy/review', body: { action: 'reject' } });
assert.deepEqual(decisionRequest(user, 'reword', '  Brief when practical  ', ''), { path: '/api/user-model/candidates/x%2Fy/review', body: { action: 'accept', final_text: 'Brief when practical' } });
assert.deepEqual(decisionRequest(user, 'narrow', '', 'session:A').body, { action: 'narrow', narrowed_scope: 'session:A' });
assert.throws(() => decisionRequest(user, 'narrow', '', 'global'));
const soul = { id: 'soul:nova:7', kind: 'soul_proposal' as const, agent: 'nova', item: { id: 7, layer: 'growth' as const, proposal: 'Shared shorthand', rationale: 'Owner reaction', created_at_unix: 1 } };
assert.equal(canReword(soul), true);
assert.deepEqual(decisionRequest(soul, 'accept', '', '').body, { agent: 'nova', resolution: 'accepted' });
assert.deepEqual(decisionRequest(soul, 'dismiss', '', '').body, { agent: 'nova', resolution: 'dismissed' });
assert.throws(() => decisionRequest(soul, 'narrow', '', 'session:A'));
assert.equal(canReword({ ...soul, item: { ...soul.item, retire_index: 0 } }), false);
assert.throws(() => decisionRequest({ ...soul, item: { ...soul.item, layer: 'voice' } }, 'reword', 'high', ''));
let calls = 0;
globalThis.fetch = async (input, options) => {
    calls++;
    assert.equal(input, '/controller/api/review/inbox');
    assert.equal((options?.headers as Record<string, string>).Authorization, 'Bearer synthetic-owner');
    assert.equal(options?.cache, 'no-store');
    return new Response(JSON.stringify({ code: 'store_unavailable', error: 'synthetic' }), { status: 503 });
};
await assert.rejects(reviewFetch('/api/review/inbox'), (error: unknown) => error instanceof ReviewError && error.status === 503);
globalThis.fetch = async () => new Response('', { status: 401 });
await assert.rejects(reviewFetch('/api/review/inbox'), (error: unknown) => error instanceof ReviewError && error.status === 401);
assert.equal(calls, 1);
console.log('review action binding and owner HTTP status boundaries passed');
