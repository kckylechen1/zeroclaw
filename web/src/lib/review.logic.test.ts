import assert from 'node:assert/strict';
// Browser globals for the existing base-path and auth modules; no real secrets.
Object.assign(globalThis, { window: { __ZEROCLAW_BASE__: '/controller' }, localStorage: { getItem: () => 'synthetic-owner' } });
const { decisionRequest, canReword, reviewFetch, ReviewError } = await import('./review');
const user = { id: 'user_model:x', kind: 'user_model_candidate' as const, item: { id: 'x/y', kind: 'preference', statement: 'Short answers', semantic_key: 'style', scope: 'global', evidence: 'source', created_at_unix: 1 } };
assert.deepEqual(decisionRequest(user, 'dismiss', '', '', []), { path: '/api/user-model/candidates/x%2Fy/review', body: { action: 'reject' } });
assert.deepEqual(decisionRequest(user, 'reword', '  Brief when practical  ', '', []), { path: '/api/user-model/candidates/x%2Fy/review', body: { action: 'accept', final_text: 'Brief when practical', expected_head: { id: null } } });
assert.deepEqual(decisionRequest(user, 'narrow', '', 'session:A', []).body, { action: 'narrow', narrowed_scope: 'session:A', expected_head: { id: null } });
assert.throws(() => decisionRequest(user, 'narrow', '', 'global', []));
const soul = { id: 'soul:nova:7', kind: 'soul_proposal' as const, agent: 'nova', item: { id: 7, layer: 'story' as const, proposal: 'Shared shorthand', rationale: 'Owner reaction', created_at_unix: 1 } };
assert.equal(canReword(soul), true);
assert.deepEqual(decisionRequest(soul, 'accept', '', '', []).body, { agent: 'nova', resolution: 'accepted' });
assert.deepEqual(decisionRequest(soul, 'dismiss', '', '', []).body, { agent: 'nova', resolution: 'dismissed' });
assert.throws(() => decisionRequest(soul, 'narrow', '', 'session:A', []));
assert.equal(canReword({ ...soul, item: { ...soul.item, retire_index: 0 } }), false);
assert.throws(() => decisionRequest({ ...soul, item: { ...soul.item, layer: 'tone' } }, 'reword', 'high', '', []));
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
for (const field of ['tone', 'level', 'unexpected_field']) {
    globalThis.fetch = async () => new Response(JSON.stringify({ code: 'invalid', field, error: 'untrusted prose' }), { status: 400 });
    await assert.rejects(reviewFetch('/api/soul/proposals/19/resolve'), (error: unknown) => error instanceof ReviewError && error.field === field && validationMessage(error) === (field === 'unexpected_field' ? 'review.invalid' : `review.invalid_${field}`));
}
globalThis.fetch = async () => new Response(JSON.stringify({ code: 'invalid', field: 'level', reason: 'soul-tone-challenge-up-only', error: 'untrusted prose' }), { status: 400 });
await assert.rejects(reviewFetch('/api/soul/proposals/19/resolve'), (error: unknown) => error instanceof ReviewError && error.reason === 'soul-tone-challenge-up-only' && validationMessage(error) === 'review.invalid_challenge_up_only');
globalThis.fetch = async () => new Response(JSON.stringify({ code: 'tone_dial_cooldown', dial: 'warmth', until_unix: 1, error: 'untrusted prose' }), { status: 429 });
await assert.rejects(reviewFetch('/api/soul/proposals/19/resolve'), (error: unknown) => error instanceof ReviewError && error.status === 429 && error.code === 'tone_dial_cooldown');
assert.equal(canReword({ ...soul, item: { ...soul.item, layer: 'tone' } }), false);
console.log('tone and story layer names, challenge rule and dial cooldown boundaries passed');

// ADR-018 §4.3 anchor and drift: shape guard, signed steps, and CAS binding.
const { anchorRequest, driftIsEmpty, readDrift, signedSteps } = await import('./review');
const dial = (key: string, anchor_level: string, current_level: string, steps: number) => ({ key, anchor_level, current_level, steps });
const drift = { anchor: { id: null, tone_revision_id: 1, story_revision_id: null, set_at_unix: 10, source: 'default' as const }, dials: [dial('warmth', 'medium', 'xhigh', 2), dial('humor', 'medium', 'low', -1)], story_added: 1, story_retired: 0, approved_proposals_since_anchor: 3 };
assert.deepEqual(readDrift({ agent: 'nova', anchor: drift.anchor, drift }), drift);
for (const unreadable of [null, {}, { drift: null }, { drift: { anchor: drift.anchor } }, { drift: { ...drift, anchor: { ...drift.anchor, source: 'model' } } }, { drift: { ...drift, story_added: '1' } }]) assert.equal(readDrift(unreadable), null);
assert.equal(driftIsEmpty(drift), false);
assert.equal(driftIsEmpty({ ...drift, dials: [dial('warmth', 'high', 'high', 0)], story_added: 0 }), true);
assert.equal(driftIsEmpty({ ...drift, dials: [dial('warmth', 'high', 'high', 0)], story_added: 0, story_retired: 2 }), false);
assert.deepEqual([2, -1, 0].map(signedSteps), ['+2', '\u22121', '0']);
const head = (revision: number) => ({ revision, source: 'owner', created_at_unix: 1, value: {} });
const withRevisions = { agent: 'nova', tone: { configured: {}, stored: head(4) }, story: head(9) };
assert.deepEqual(anchorRequest(withRevisions, 'keep'), { path: '/api/soul/anchor', body: { agent: 'nova', expected_tone_revision: 4, expected_story_revision: 9 } });
assert.deepEqual(anchorRequest(withRevisions, 'rollback'), { path: '/api/soul/anchor/rollback', body: { agent: 'nova', expected_tone_revision: 4, expected_story_revision: 9 } });
// A layer with no revision yet is expected as 0, including a null stored head.
assert.deepEqual(anchorRequest({ agent: 'nova', tone: { configured: {}, stored: null as never } }, 'keep').body, { agent: 'nova', expected_tone_revision: 0, expected_story_revision: 0 });
globalThis.fetch = async (input, options) => {
    assert.equal(input, '/controller/api/soul/anchor');
    assert.equal(options?.method, 'POST');
    return new Response(JSON.stringify({ code: 'revision_conflict', current_revision: 5, error: 'untrusted prose' }), { status: 409 });
};
await assert.rejects(reviewFetch('/api/soul/anchor', undefined, anchorRequest(withRevisions, 'keep').body), (error: unknown) => error instanceof ReviewError && error.status === 409 && error.code === 'revision_conflict');
console.log('anchor and drift boundaries passed');

// ADR-018 §3 Tone exemplars: byte counting, single-line and bound checks that
// mirror the gateway, the CAS-bound PUT body, the pinned-reply draft, and the
// router state that carries it.
const { EXEMPLAR_MAX_BYTES, MAX_EXEMPLARS, exemplarBytes, exemplarProblem, exemplarsRequest, exemplarsSavable, pinnedExemplarDraft, readExemplarDraftState, readExemplars } = await import('./review');
assert.deepEqual(EXEMPLAR_MAX_BYTES, { situation: 80, owner_line: 160, agent_line: 320 });
assert.equal(MAX_EXEMPLARS, 4);
// UTF-8 bytes of the trimmed text: ASCII 1, CJK 3, emoji 4; the ideographic space trims.
assert.equal(exemplarBytes('abc'), 3);
assert.equal(exemplarBytes('  累  '), 3);
assert.equal(exemplarBytes('　累　'), 3);
assert.equal(exemplarBytes('累累累'), 9);
assert.equal(exemplarBytes('😀'), 4);
assert.equal(exemplarBytes(''), 0);
// 26 CJK characters are 78 bytes and fit the situation cap of 80; 27 are 81 and do not.
assert.equal(exemplarProblem('situation', '累'.repeat(26)), null);
assert.equal(exemplarProblem('situation', '累'.repeat(27)), 'too_long');
assert.equal(exemplarProblem('situation', 'a'.repeat(80)), null);
assert.equal(exemplarProblem('situation', 'a'.repeat(81)), 'too_long');
assert.equal(exemplarProblem('owner_line', 'a'.repeat(160)), null);
assert.equal(exemplarProblem('owner_line', 'a'.repeat(161)), 'too_long');
assert.equal(exemplarProblem('agent_line', '累'.repeat(106)), null);
assert.equal(exemplarProblem('agent_line', '累'.repeat(107)), 'too_long');
// The cap counts the trimmed text, as on the gateway.
assert.equal(exemplarProblem('situation', `  ${'a'.repeat(80)}  `), null);
assert.equal(exemplarProblem('situation', ''), 'empty');
assert.equal(exemplarProblem('situation', ' 　 '), 'empty');
// Any line break or control character inside the text blocks saving (a heading cannot be injected).
for (const bad of ['a\nb', 'a\r\nb', 'a\rb', 'a\n## Our story so far', 'a\tb', 'a\u0085b', 'a b', 'a b', 'a\u0000b'])
    assert.equal(exemplarProblem('agent_line', bad), 'single_line', JSON.stringify(bad));
// Surrounding line breaks are trimmed, as on the gateway, so they do not block saving.
assert.equal(exemplarProblem('agent_line', '\nhello\n'), null);
// A long text with a newline reports the newline first, as the gateway does.
assert.equal(exemplarProblem('situation', `${'a'.repeat(100)}\nb`), 'single_line');

const exemplar = { situation: '主人下班后很累', owner_line: '今天好累。', agent_line: '辛苦了,先歇一会儿。' };
assert.equal(exemplarsSavable([]), true);
assert.equal(exemplarsSavable([exemplar]), true);
assert.equal(exemplarsSavable([exemplar, exemplar, exemplar, exemplar]), true);
assert.equal(exemplarsSavable([exemplar, exemplar, exemplar, exemplar, exemplar]), false);
assert.equal(exemplarsSavable([{ ...exemplar, owner_line: '' }]), false);
assert.equal(exemplarsSavable([{ ...exemplar, agent_line: 'one\ntwo' }]), false);
assert.equal(exemplarsSavable([{ ...exemplar, situation: 'a'.repeat(81) }]), false);

// Save names the head the editor started from and sends the trimmed text over PUT.
assert.deepEqual(exemplarsRequest('nova', 3, [{ situation: ' tired ', owner_line: ' hi', agent_line: 'hello ' }]), {
    path: '/api/soul/tone/exemplars', method: 'PUT',
    body: { agent: 'nova', expected_revision: 3, exemplars: [{ situation: 'tired', owner_line: 'hi', agent_line: 'hello' }] },
});
assert.deepEqual(exemplarsRequest('nova', 0, []).body, { agent: 'nova', expected_revision: 0, exemplars: [] });
assert.throws(() => exemplarsRequest('nova', 0, [{ ...exemplar, owner_line: 'a\nb' }]));
assert.throws(() => exemplarsRequest('nova', 0, [exemplar, exemplar, exemplar, exemplar, exemplar]));
globalThis.fetch = async (input, options) => {
    assert.equal(input, '/controller/api/soul/tone/exemplars');
    assert.equal(options?.method, 'PUT');
    assert.equal((options?.headers as Record<string, string>)['Content-Type'], 'application/json');
    return new Response(JSON.stringify({ code: 'revision_conflict', current_revision: 4, error: 'untrusted prose' }), { status: 409 });
};
{
    const request = exemplarsRequest('nova', 3, [exemplar]);
    await assert.rejects(reviewFetch(request.path, undefined, request.body, request.method), (error: unknown) => error instanceof ReviewError && error.status === 409 && error.code === 'revision_conflict');
}
// POST stays the default for every other owner action.
globalThis.fetch = async (_input, options) => { assert.equal(options?.method, 'POST'); return new Response('{}'); };
await reviewFetch('/api/soul/anchor', undefined, {});

// The gateway's validation reasons pick the message; an unknown one stays generic.
for (const [reason, key] of [['soul-tone-exemplars-too-many', 'review.invalid_exemplars_too_many'], ['soul-tone-exemplar-empty', 'review.invalid_exemplar_empty'], ['soul-tone-exemplar-single-line', 'review.invalid_exemplar_single_line'], ['soul-tone-exemplar-too-long', 'review.invalid_exemplar_too_long']]) {
    globalThis.fetch = async () => new Response(JSON.stringify({ code: 'invalid', field: 'agent_line', reason, error: 'untrusted prose' }), { status: 400 });
    await assert.rejects(reviewFetch('/api/soul/tone/exemplars', undefined, {}, 'PUT'), (error: unknown) => error instanceof ReviewError && validationMessage(error) === key);
}
assert.equal(validationMessage(new ReviewError(400, 'invalid', 'agent_line', 'something-new')), 'review.invalid');

// Reading the head tolerates an older gateway and malformed rows.
const withExemplars = (value: unknown) => ({ agent: 'nova', tone: { configured: {}, exemplars: { revision: 2, source: 'owner', created_at_unix: 1, value } } });
assert.deepEqual(readExemplars(withExemplars({ exemplars: [exemplar] })), [exemplar]);
assert.deepEqual(readExemplars({ agent: 'nova', tone: { configured: {} } }), []);
assert.deepEqual(readExemplars({ agent: 'nova', tone: { configured: {}, exemplars: null } }), []);
assert.deepEqual(readExemplars(withExemplars({ exemplars: [exemplar, { situation: 'x' }, null, 7] })), [exemplar]);
assert.deepEqual(readExemplars(withExemplars({ exemplars: 'nope' })), []);
assert.deepEqual(readExemplars(withExemplars({ exemplars: [{ ...exemplar, extra: 'dropped' }] })), [exemplar]);

// Pin from chat: the reply's exact text, the owner's latest message before it, nothing trimmed.
const row = (id: string, role: 'user' | 'agent', text: string, plain = true) => ({ id, role, text, plain });
const transcript = [row('u1', 'user', 'first question'), row('a1', 'agent', 'first answer'), row('u2', 'user', '今天好累。\n还有很多事'), row('t1', 'agent', 'tool output', false), row('a2', 'agent', '辛苦了。\n\n先歇一会儿。  ')];
assert.deepEqual(pinnedExemplarDraft(transcript, 'a2'), { situation: '', owner_line: '今天好累。\n还有很多事', agent_line: '辛苦了。\n\n先歇一会儿。  ' });
assert.deepEqual(pinnedExemplarDraft(transcript, 'a1'), { situation: '', owner_line: 'first question', agent_line: 'first answer' });
// No owner message before the reply: empty owner line. Command output and notices are not the owner's words.
assert.deepEqual(pinnedExemplarDraft([row('a0', 'agent', 'hello')], 'a0'), { situation: '', owner_line: '', agent_line: 'hello' });
assert.equal(pinnedExemplarDraft([row('u0', 'user', '/help', false), row('a0', 'agent', 'hello')], 'a0')?.owner_line, '');
// Only the agent's ordinary replies can be pinned.
assert.equal(pinnedExemplarDraft(transcript, 'u1'), null);
assert.equal(pinnedExemplarDraft(transcript, 't1'), null);
assert.equal(pinnedExemplarDraft(transcript, 'missing'), null);
assert.equal(pinnedExemplarDraft([row('a0', 'agent', '  \n ')], 'a0'), null);

// Router state: only the pinned shape is read; the URL never carries the text.
assert.deepEqual(readExemplarDraftState({ toneExemplarDraft: { agent: 'nova', ...exemplar } }), { agent: 'nova', ...exemplar });
for (const state of [null, undefined, {}, 'x', { toneExemplarDraft: null }, { toneExemplarDraft: { agent: '', ...exemplar } }, { toneExemplarDraft: { agent: 'nova', situation: 'x' } }, { toneExemplarDraft: { agent: 'nova', ...exemplar, owner_line: 7 } }])
    assert.equal(readExemplarDraftState(state), null);
console.log('tone exemplar bounds, request binding, pinned draft and router state passed');
