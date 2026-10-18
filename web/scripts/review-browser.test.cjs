const { chromium } = require(process.env.PLAYWRIGHT_MODULE || 'playwright');
const http = require('node:http');
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const dist = path.resolve(__dirname, '../dist');
const artifacts = process.env.REVIEW_SCREENSHOTS || require('node:os').tmpdir();
fs.mkdirSync(artifacts, { recursive: true });
let browser;
const server = http.createServer((req, res) => { const file = path.join(dist, req.url.split('?')[0].replace(/^\/_app/, '')); const target = fs.existsSync(file) && fs.statSync(file).isFile() ? file : path.join(dist, 'index.html'); res.setHeader('Content-Type', target.endsWith('.js') ? 'application/javascript' : target.endsWith('.css') ? 'text/css' : 'text/html'); res.end(fs.readFileSync(target)); });
(async () => {
    await new Promise(r => server.listen(0, '127.0.0.1', r));
    const origin = `http://127.0.0.1:${server.address().port}`;
    browser = await chromium.launch({ headless: true, ...(process.env.REVIEW_CHROME ? { executablePath: process.env.REVIEW_CHROME } : { channel: 'chrome' }) });
    const context = await browser.newContext({ viewport: { width: 375, height: 812 } });
    const now = 1780000000;
    let pairingEnabled = false;
    let locale = 'en';
    let supportsExpectedHead = true;
    let inboxUnavailable = false;
    let headsUnavailable = false;
    let headsFailureStatus = 503;
    let soulUnavailable = false;
    let inboxReads = 0;
    let pairCalls = 0;
    let oldHistoryRelease;
    let delayHistory = false;
    let unknownReads = 0;
    let reviewState = 'rejected';
    const actions = [];
    // ADR-018 §3 Tone exemplars: the head the gateway holds, and every PUT it receives.
    let exemplarHead = null;
    const writes = [];
    // ADR-018 §4.3 drift fixture; an older gateway answers the route with {}.
    let driftEnabled = false;
    let anchorConflictOnce = true;
    const dial = (key, anchor_level, current_level, steps) => ({ key, anchor_level, current_level, steps });
    const driftOf = (anchor, dials, extra) => ({ anchor, dials, story_added: 0, story_retired: 0, approved_proposals_since_anchor: 0, ...extra });
    const defaultAnchor = { id: null, tone_revision_id: 1, story_revision_id: null, set_at_unix: now, source: 'default' };
    const drifted = () => driftOf(defaultAnchor, [dial('warmth', 'medium', 'xhigh', 2), dial('directness', 'medium', 'medium', 0), dial('explanation_density', 'medium', 'medium', 0), dial('challenge', 'medium', 'medium', 0), dial('humor', 'medium', 'low', -1)], { story_added: 1, story_retired: 2, approved_proposals_since_anchor: 3 });
    const unmoved = (anchor) => driftOf(anchor, ['warmth', 'directness', 'explanation_density', 'challenge', 'humor'].map(key => dial(key, 'medium', 'medium', 0)));
    let driftState = drifted();
    const proposal = { id: 'soul:nova:7', kind: 'soul_proposal', agent: 'nova', item: { id: 7, layer: 'story', proposal: 'Retire the old shared shorthand', rationale: 'Synthetic stale-target fixture', retire_index: 0, retire_target: { kind: 'bond', text: 'Actual canonical shared shorthand' }, target_revision: 17, created_at_unix: now } };
    const user = { id: 'user_model:8', kind: 'user_model_candidate', item: { id: '8', kind: 'preference', statement: 'Shorter planning replies', scope: 'global', evidence: 'Synthetic owner evidence', semantic_key: 'style', created_at_unix: now } };
    const rejected = { ...user.item, id: '42', statement: 'Previously dismissed preference' };
    const scopedItems = ['agent:nova', 'channel:telegram', 'session:original'].map((scope, i) => ({ ...user, id: `user_model:${9 + i}`, item: { ...user.item, id: String(9 + i), statement: `Scoped candidate ${scope}`, scope } }));
    const addition = { ...proposal, id: 'soul:nova:12', item: { id: 12, layer: 'story', proposal: 'New story addition', rationale: 'Synthetic addition', story_kind: 'self', created_at_unix: now } };
    const toneProposal = { ...proposal, id: 'soul:nova:19', item: { id: 19, layer: 'tone', trait_key: 'warmth', level: 'xhigh', proposal: 'Warmer replies', rationale: 'Synthetic Tone limit fixture', created_at_unix: now } };
    const receipts = ['clock_started', 'nothing_to_reflect_on', 'proposal_queue_full', 'model_call_failed: synthetic timeout: retry'].map((outcome, i) => ({ id: `reflection:nova:${i}`, kind: 'reflection_receipt', agent: 'nova', item: { period_from_unix: now, ran_at_unix: now, proposals_created: 0, user_model_candidates_created: 0, outcome } }));
    let items = [proposal, user, ...scopedItems, addition, toneProposal, ...receipts];
    let currentHead = { id: 'revision-current', semantic_key: 'style', kind: 'preference', statement: 'Existing detailed planning preference', scope: 'global', authority: 'owner_authored' };
    const head = (value) => ({ revision: 2, source: 'owner', created_at_unix: now, value });
    const soul = { agent: 'nova', identity: head({ name: 'Nova' }), principles: head({ items: ['Keep private information private.'] }), story: head({ entries: [] }), tone: { configured: { warmth: 'high' }, stored: head({ heads: { warmth: 'low' } }) } };
    await context.route('**/*', async (route) => {
        const req = route.request();
        const url = new URL(req.url());
        if (url.origin !== origin)
            return route.abort();
        const send = (data, status = 200) => route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(data) });
        if (url.pathname === '/health')
            return send({ require_pairing: pairingEnabled });
        if (url.pathname === '/api/pair') {
            pairCalls++;
            return pairingEnabled ? send({ paired: true, persisted: true, token: 'synthetic-owner' }) : send({ error: 'Pairing disabled' }, 400);
        }
        if (!url.pathname.startsWith('/api/'))
            return route.continue();
        if (url.pathname === '/api/status')
            return send({ locale, version: 'synthetic', agents: [] });
        if (url.pathname === '/api/config/agent-options')
            return send({ agents: ['nova', 'atlas'] });
        if (url.pathname === '/api/config/reload-status')
            return send({ pending_reload: false });
        if (url.pathname === '/api/config/drift')
            return send({ drifted: [] });
        if (url.pathname.startsWith('/api/review') || url.pathname.startsWith('/api/soul') || url.pathname.startsWith('/api/user-model')) {
            if (req.headers().authorization !== 'Bearer synthetic-owner')
                return send({ error: 'Unauthorized' }, 401);
        }
        if (url.pathname === '/api/review/inbox') {
            inboxReads++;
            if (inboxUnavailable) return send({ code: 'store_unavailable' }, 503);
            if (url.searchParams.get('agent') === 'deleted') {
                unknownReads++;
                return send({ code: 'unknown_agent', error: 'Unknown' }, 404);
            }
            return send({ items, total: items.length, next_offset: null });
        }
        if (req.method() === 'POST') {
            const body = req.postDataJSON();
            actions.push({ path: url.pathname, body });
            if (url.pathname === '/api/soul/anchor') {
                if (anchorConflictOnce) { anchorConflictOnce = false; return send({ code: 'revision_conflict', current_revision: 3, error: 'untrusted conflict detail' }, 409); }
                driftState = unmoved({ ...defaultAnchor, id: 1, source: 'owner' });
                return send({ agent: body.agent, anchor: driftState.anchor });
            }
            if (url.pathname === '/api/soul/anchor/rollback') {
                driftState = unmoved(defaultAnchor);
                return send({ agent: body.agent, tone_revision: 3, story_revision: null });
            }
            if (url.pathname.endsWith('/19/resolve')) return send({ code: 'invalid', field: 'level', error: 'untrusted validation detail' }, 400);
            if (url.pathname.endsWith('/8/review') && body.action === 'accept') {
                assert.deepEqual(body.expected_head, { id: 'revision-current' });
                currentHead = { ...currentHead, id: 'revision-unseen-B', statement: 'Changed elsewhere before acceptance' };
                return send({ code: 'head_conflict', error: 'Synthetic CAS conflict' }, 409);
            }
            if (url.pathname.endsWith('/7/resolve')) {
                if (body.resolution === 'accepted')
                    return send({ code: 'proposal_stale', error: 'Synthetic stale target' }, 409);
                items = items.filter(i => i.id !== proposal.id);
            }
            if (url.pathname.endsWith('/42/review')) {
                assert.equal(reviewState, 'rejected');
                assert.equal(body.action, 'narrow');
                reviewState = 'narrowed';
            }
            return send({ ok: true });
        }
        if (req.method() === 'PUT' && url.pathname === '/api/soul/tone/exemplars') {
            const body = req.postDataJSON();
            writes.push({ path: url.pathname, body });
            if (body.exemplars.some(item => item.owner_line === 'SERVER-REJECTS-THIS'))
                return send({ code: 'invalid', field: 'owner_line', reason: 'soul-tone-exemplar-too-long', error: 'untrusted validation detail' }, 400);
            const current = exemplarHead?.revision ?? 0;
            if (body.expected_revision !== current)
                return send({ code: 'revision_conflict', current_revision: current, error: 'untrusted conflict detail' }, 409);
            exemplarHead = { revision: current + 1, source: 'owner', created_at_unix: now, value: { exemplars: body.exemplars } };
            return send(exemplarHead);
        }
        if (url.pathname === '/api/soul/drift')
            return driftEnabled ? send({ agent: url.searchParams.get('agent'), anchor: driftState.anchor, drift: driftState }) : send({});
        if (url.pathname === '/api/soul')
            return soulUnavailable ? send({ code: 'store_unavailable' }, 503) : send({ ...soul, tone: { ...soul.tone, exemplars: exemplarHead } });
        if (url.pathname === '/api/soul/history') {
            if (delayHistory && url.searchParams.get('layer') === 'identity') {
                oldHistoryRelease = () => send({ code: 'unauthorized', error: 'late failure' }, 401);
                return;
            }
            return send({ layer: url.searchParams.get('layer'), revisions: [{ ...head({ text: 'Fresh principles history' }), source: 'approved_proposal', proposal_id: 71 }, { ...head({ text: 'Restored principles history' }), revision: 3, source: 'owner', rolled_back_from: 1 }] });
        }
        if (url.pathname === '/api/user-model/heads')
            return headsUnavailable ? send({ code: 'heads_unavailable' }, headsFailureStatus) : send({ heads: [currentHead], ...(supportsExpectedHead ? { supports_expected_head: true } : {}) });
        if (url.pathname === '/api/user-model/candidates')
            return send({ candidates: [rejected] });
        if (url.pathname === '/api/user-model/candidates/42')
            return send({ candidate: rejected, review_state: reviewState, review_receipts: [{ id: 'r1', action: 'reject', at_unix: now }] });
        return send({});
    });
    const page = await context.newPage();
    const failures = [];
    page.on('pageerror', e => failures.push(e.message));
    await page.goto(origin + '/review?agent=deleted');
    await page.getByText('Pairing is disabled.', { exact: false }).waitFor();
    assert.equal(await page.getByLabel('Pairing code', { exact: true }).count(), 0);
    assert.equal(pairCalls, 0);
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-pair-disabled-375.png') });
    // A bridge token cannot enroll an operator while pairing remains disabled.
    await page.evaluate(() => localStorage.setItem('zeroclaw_token', 'synthetic-bridge'));
    await page.reload();
    await page.getByText('Pairing is disabled.', { exact: false }).waitFor();
    assert.equal(await page.getByLabel('Pairing code', { exact: true }).count(), 0);
    assert.equal(pairCalls, 0);
    // Explicit fixture configuration change models the documented operator recovery.
    pairingEnabled = true;
    await page.getByRole('button', { name: 'Check pairing availability' }).click();
    await page.getByLabel('Pairing code', { exact: true }).fill('123456');
    await page.getByRole('button', { name: 'Pair as owner', exact: true }).click();
    await page.getByText(user.item.statement, { exact: true }).waitFor();
    assert.equal(pairCalls, 1);
    assert.equal(await page.getByRole('combobox', { name: 'Agent', exact: true }).inputValue(), 'nova');
    assert.equal(unknownReads, 0);
    await page.getByText('Actual canonical shared shorthand', { exact: true }).waitFor();
    await page.getByText('Bound entry to retire · Revision 17', { exact: true }).waitFor();
    const addCard = page.locator('article').filter({ has: page.getByRole('heading', { name: addition.item.proposal, exact: true }) });
    assert.match(await addCard.innerText(), /Self/);
    const toneApproval = page.locator('article').filter({ has: page.getByRole('heading', { name: toneProposal.item.proposal, exact: true }) });
    const beforeInvalid = actions.length;
    const beforeInvalidReads = inboxReads;
    await toneApproval.getByRole('button', { name: 'Accept', exact: true }).click();
    await page.getByRole('status').getByText('This proposal exceeds the current Tone one-step limit', { exact: false }).waitFor();
    await toneApproval.getByRole('button', { name: 'Accept', exact: true }).waitFor();
    assert.ok(inboxReads > beforeInvalidReads);
    assert.equal(actions.length, beforeInvalid + 1);
    assert.equal(await page.getByText('Could not complete the request. Check the connection and try again.', { exact: true }).count(), 0);
    const staleCard = page.locator('article').filter({ has: page.getByRole('heading', { name: proposal.item.proposal, exact: true }) });
    await staleCard.getByRole('button', { name: 'Accept', exact: true }).click();
    await staleCard.getByText('This proposal no longer matches its target.', { exact: false }).waitFor();
    assert.equal(await staleCard.getByRole('button', { name: 'Accept', exact: true }).count(), 0);
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-stale-proposal-375.png') });
    await staleCard.getByRole('button', { name: 'Dismiss', exact: true }).click();
    await page.getByRole('heading', { name: proposal.item.proposal, exact: true }).waitFor({ state: 'detached' });
    const userCard = page.locator('article').filter({ has: page.getByRole('heading', { name: user.item.statement, exact: true }) });
    assert.match(await userCard.innerText(), /Semantic key: style/);
    assert.match(await userCard.innerText(), /Preference/);
    assert.match(await userCard.innerText(), /Existing detailed planning preference/);
    assert.match(await userCard.innerText(), /revision-current/);
    await userCard.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-approval-details-375.png') });
    const beforeConflict = actions.length;
    await userCard.getByRole('button', { name: 'Accept', exact: true }).click();
    await userCard.getByText('Changed elsewhere before acceptance', { exact: true }).waitFor();
    await page.getByText('The current entry for this key changed.', { exact: false }).waitFor();
    assert.equal(actions.length, beforeConflict + 1);
    assert.equal(actions.at(-1).body.expected_head.id, 'revision-current');
    assert.match(await userCard.innerText(), /revision-unseen-B/);
    for (const scoped of scopedItems) {
        const card = page.locator('article').filter({ has: page.getByRole('heading', { name: scoped.item.statement, exact: true }) });
        if (!scoped.item.scope.startsWith('session:')) {
            assert.equal(await card.getByRole('button', { name: 'Limit scope' }).count(), 0);
        } else {
            await card.getByRole('button', { name: 'Limit scope' }).click();
            const input = card.getByLabel('Session ID', { exact: true });
            assert.equal(await input.inputValue(), 'original');
            assert.equal(await input.evaluate(el => el.readOnly), true);
            await card.getByRole('button', { name: 'Apply reviewed change' }).click();
            await page.getByRole('status').filter({ hasText: 'Decision recorded' }).waitFor();
            assert.deepEqual(actions.at(-1).body, { action: 'narrow', narrowed_scope: 'session:original', expected_head: { id: 'revision-unseen-B' } });
        }
    }
    const disclosure = userCard.locator('summary');
    assert.ok((await disclosure.boundingBox()).height >= 44);
    await userCard.getByRole('button', { name: 'Reword', exact: true }).focus();
    await page.keyboard.press('Enter');
    assert.equal(await page.getByLabel('Your wording').evaluate(el => el === document.activeElement), true);
    await userCard.getByRole('button', { name: 'Cancel', exact: true }).click();
    await userCard.getByRole('button', { name: 'Limit scope' }).click();
    assert.equal(await page.getByLabel('Session ID', { exact: true }).evaluate(el => el === document.activeElement), true);
    await userCard.getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.getByRole('button', { name: 'My Agent', exact: true }).click();
    await page.getByRole('heading', { name: 'Identity', exact: true }).waitFor();
    const toneCard = page.locator('article').filter({ has: page.getByRole('heading', { name: 'Tone', exact: true }) });
    await toneCard.getByRole('heading', { name: 'Configured Tone', exact: true }).waitFor();
    await toneCard.getByRole('heading', { name: 'Stored Tone', exact: true }).waitFor();
    assert.match(await toneCard.innerText(), /high/);
    assert.match(await toneCard.innerText(), /low/);
    await toneCard.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-tone-fallback-375.png') });
    // Tone exemplars (ADR-018 §3): list, edit with live UTF-8 counters, CAS save, conflict and server rejection.
    const exemplarRegion = toneCard.getByRole('region', { name: 'Tone exemplars', exact: true });
    await exemplarRegion.getByText('No exemplars yet.', { exact: true }).waitFor();
    await exemplarRegion.getByRole('button', { name: 'Add exemplar', exact: true }).click();
    const exemplarEditor = toneCard.getByRole('form', { name: 'Edit exemplars', exact: true });
    const exemplarRow = (n) => exemplarEditor.getByRole('group', { name: `Exemplar ${n}`, exact: true });
    const exemplarSave = exemplarEditor.getByRole('button', { name: 'Save exemplars', exact: true });
    const first = exemplarRow(1);
    assert.equal(await first.getByLabel('Situation', { exact: true }).evaluate(el => el === document.activeElement), true);
    assert.equal(await exemplarSave.isDisabled(), true, 'empty fields cannot be saved');
    await first.getByText('0 / 80 bytes · Required.', { exact: true }).waitFor();
    // CJK counts three bytes a character: 27 characters are 81 bytes, over the cap of 80.
    await first.getByLabel('Situation', { exact: true }).fill('累'.repeat(27));
    await first.getByText('81 / 80 bytes · Over the byte limit.', { exact: true }).waitFor();
    await first.getByLabel('Owner', { exact: true }).fill('今天好累。');
    await first.getByLabel('You', { exact: true }).fill('辛苦了。先坐一会儿,剩下的我来。');
    assert.equal(await exemplarSave.isDisabled(), true, 'over the situation cap');
    await first.getByLabel('Situation', { exact: true }).fill('主人下班后很累');
    await first.getByText('21 / 80 bytes', { exact: true }).waitFor();
    await first.getByText('15 / 160 bytes', { exact: true }).waitFor();
    assert.equal(await exemplarSave.isDisabled(), false);
    // A line break is shown, flagged, and blocks saving until the owner fixes it.
    await first.getByLabel('You', { exact: true }).fill('第一行\n第二行');
    await first.getByText('Must be a single line: remove the line breaks.', { exact: false }).waitFor();
    assert.equal(await first.getByLabel('You', { exact: true }).inputValue(), '第一行\n第二行');
    assert.equal(await exemplarSave.isDisabled(), true, 'a newline blocks saving');
    await toneCard.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-exemplar-editor-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    await first.getByLabel('You', { exact: true }).fill('辛苦了。先坐一会儿,剩下的我来。');
    assert.equal(await exemplarSave.isDisabled(), false);
    // Cancel writes nothing.
    await exemplarEditor.getByRole('button', { name: 'Cancel', exact: true }).click();
    await exemplarRegion.getByText('No exemplars yet.', { exact: true }).waitFor();
    assert.equal(writes.length, 0);
    await exemplarRegion.getByRole('button', { name: 'Add exemplar', exact: true }).click();
    await first.getByLabel('Situation', { exact: true }).fill('主人下班后很累');
    await first.getByLabel('Owner', { exact: true }).fill('今天好累。');
    await first.getByLabel('You', { exact: true }).fill('辛苦了。先坐一会儿,剩下的我来。');
    await exemplarSave.click();
    await page.getByRole('status').getByText('Exemplars saved.', { exact: false }).waitFor();
    await exemplarRegion.getByText('辛苦了。先坐一会儿,剩下的我来。', { exact: true }).waitFor();
    assert.deepEqual(writes, [{ path: '/api/soul/tone/exemplars', body: { agent: 'nova', expected_revision: 0, exemplars: [{ situation: '主人下班后很累', owner_line: '今天好累。', agent_line: '辛苦了。先坐一会儿,剩下的我来。' }] } }]);
    assert.match(await exemplarRegion.innerText(), /Revision 1 · Owner/);
    assert.equal(await exemplarEditor.count(), 0, 'the editor closes after saving');
    // A change made elsewhere since the editor opened is the existing 409: nothing is overwritten.
    await exemplarRegion.getByRole('button', { name: 'Edit exemplars', exact: true }).click();
    await first.getByLabel('Owner', { exact: true }).fill('今天特别累。');
    exemplarHead = { ...exemplarHead, revision: 5, value: { exemplars: [{ situation: '别处改的', owner_line: '别处', agent_line: '别处的回复' }] } };
    await exemplarSave.click();
    await page.getByRole('status').getByText('The current revision changed.', { exact: false }).waitFor();
    await exemplarRegion.getByText('别处的回复', { exact: true }).waitFor();
    assert.equal(writes.at(-1).body.expected_revision, 1, 'the save names the revision the editor started from');
    assert.equal(exemplarHead.revision, 5, 'the conflicting save wrote nothing');
    assert.equal(await exemplarEditor.count(), 0);
    // The gateway stays the authority: a rejected save keeps the owner's text and shows the reason.
    await exemplarRegion.getByRole('button', { name: 'Edit exemplars', exact: true }).click();
    await first.getByLabel('Owner', { exact: true }).fill('SERVER-REJECTS-THIS');
    await exemplarSave.click();
    await page.getByRole('status').getByText('An exemplar field is over its byte limit; nothing was saved.', { exact: true }).waitFor();
    await first.getByLabel('Owner', { exact: true }).waitFor();
    assert.equal(await first.getByLabel('Owner', { exact: true }).inputValue(), 'SERVER-REJECTS-THIS');
    assert.equal(exemplarHead.revision, 5);
    // At most four: add up to the cap, then adding is disabled; removing frees a slot.
    for (const n of [2, 3, 4]) {
        await exemplarEditor.getByRole('button', { name: 'Add exemplar', exact: true }).click();
        const row = exemplarRow(n);
        assert.equal(await row.getByLabel('Situation', { exact: true }).evaluate(el => el === document.activeElement), true);
        await row.getByLabel('Situation', { exact: true }).fill(`s${n}`);
        await row.getByLabel('Owner', { exact: true }).fill(`o${n}`);
        await row.getByLabel('You', { exact: true }).fill(`a${n}`);
    }
    assert.equal(await exemplarEditor.getByRole('button', { name: 'Add exemplar', exact: true }).isDisabled(), true);
    await first.getByLabel('Owner', { exact: true }).fill('o1');
    await exemplarEditor.getByRole('button', { name: 'Remove exemplar 4', exact: true }).click();
    assert.equal(await exemplarEditor.getByRole('button', { name: 'Add exemplar', exact: true }).isDisabled(), false);
    assert.equal(await exemplarRow(4).count(), 0);
    await exemplarEditor.getByRole('button', { name: 'Cancel', exact: true }).click();
    assert.deepEqual(exemplarHead.value.exemplars.map(item => item.situation), ['别处改的'], 'cancelling wrote nothing');

    // Pin from chat: the reply's exact text and the owner's message before it travel as router state.
    const ownerMessage = '今天好累。\n还有很多事';
    const replyText = '辛苦了。\n先歇一会儿,剩下的我来。';
    await page.evaluate(([owner, reply]) => {
        const at = new Date().toISOString();
        localStorage.setItem('zeroclaw_session_id.nova', 'pin-session');
        localStorage.setItem('zeroclaw_chat_history_v1:pin-session', JSON.stringify({ messages: [
            { id: 'm1', role: 'user', content: 'Earlier question', local: true, timestamp: at },
            { id: 'm2', role: 'agent', content: 'Earlier answer', markdown: false, timestamp: at },
            { id: 'm3', role: 'user', content: owner, local: true, timestamp: at },
            { id: 'm4', role: 'agent', content: reply, markdown: false, timestamp: at },
        ] }));
    }, [ownerMessage, replyText]);
    const chatMessage = (text) => page.locator('div.group').filter({ hasText: text });
    const pinAction = (text) => chatMessage(text).getByRole('button', { name: 'Use as tone example', exact: true });
    await page.goto(origin + '/agent/nova');
    await pinAction('剩下的我来').waitFor();
    // Only the agent's replies offer it, never the owner's own messages.
    assert.equal(await chatMessage('Earlier question').getByRole('button', { name: 'Use as tone example' }).count(), 0);
    assert.equal(await pinAction('Earlier answer').count(), 1);
    const writesBeforePin = writes.length;
    await pinAction('剩下的我来').click();
    const pinEditor = page.getByRole('form', { name: 'Edit exemplars', exact: true });
    await pinEditor.waitFor();
    assert.equal(page.url(), `${origin}/review?agent=nova`, 'the draft is not in the URL');
    assert.equal(await page.evaluate(() => (history.state && history.state.usr) ?? null), null, 'the navigation state is dropped once the editor opens');
    assert.deepEqual(await page.evaluate(needle => Object.keys(localStorage).filter(key => !key.startsWith('zeroclaw_chat_history_v1:') && (localStorage.getItem(key) ?? '').includes(needle)), '先歇一会儿'), [], 'the draft is not in storage');
    await pinEditor.getByText('Prefilled from the chat.', { exact: false }).waitFor();
    const pinnedRow = pinEditor.getByRole('group', { name: 'Exemplar 2', exact: true });
    assert.equal(await pinEditor.getByRole('group', { name: 'Exemplar 1', exact: true }).getByLabel('Owner', { exact: true }).inputValue(), '别处');
    assert.equal(await pinnedRow.getByLabel('Situation', { exact: true }).inputValue(), '');
    assert.equal(await pinnedRow.getByLabel('Owner', { exact: true }).inputValue(), ownerMessage);
    assert.equal(await pinnedRow.getByLabel('You', { exact: true }).inputValue(), replyText);
    assert.equal(await pinnedRow.getByLabel('Situation', { exact: true }).evaluate(el => el === document.activeElement), true);
    await pinnedRow.getByText('Must be a single line: remove the line breaks.', { exact: false }).first().waitFor();
    assert.equal(await pinEditor.getByRole('button', { name: 'Save exemplars', exact: true }).isDisabled(), true);
    assert.equal(writes.length, writesBeforePin, 'nothing is saved until the owner presses save');
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-exemplar-pinned-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    await pinnedRow.getByLabel('Situation', { exact: true }).fill('owner is tired');
    await pinnedRow.getByLabel('Owner', { exact: true }).fill('今天好累。还有很多事');
    await pinnedRow.getByLabel('You', { exact: true }).fill('辛苦了。先歇一会儿,剩下的我来。');
    await pinEditor.getByRole('button', { name: 'Save exemplars', exact: true }).click();
    await page.getByRole('status').getByText('Exemplars saved.', { exact: false }).waitFor();
    assert.deepEqual(writes.at(-1), { path: '/api/soul/tone/exemplars', body: { agent: 'nova', expected_revision: 5, exemplars: [{ situation: '别处改的', owner_line: '别处', agent_line: '别处的回复' }, { situation: 'owner is tired', owner_line: '今天好累。还有很多事', agent_line: '辛苦了。先歇一会儿,剩下的我来。' }] } });
    await exemplarRegion.getByText('owner is tired', { exact: true }).waitFor();
    // With four already stored, a pinned reply makes a fifth row: it cannot be saved until one is removed.
    exemplarHead = { ...exemplarHead, revision: 9, value: { exemplars: ['a', 'b', 'c', 'd'].map(x => ({ situation: `s-${x}`, owner_line: `o-${x}`, agent_line: `a-${x}` })) } };
    await page.goto(origin + '/agent/nova');
    await pinAction('剩下的我来').click();
    await pinEditor.waitFor();
    await pinEditor.getByText('At most 4 exemplars. Remove one to save.', { exact: true }).waitFor();
    assert.equal(await pinEditor.getByRole('button', { name: 'Save exemplars', exact: true }).isDisabled(), true);
    assert.equal(await pinEditor.getByRole('button', { name: 'Add exemplar', exact: true }).isDisabled(), true);
    await pinEditor.getByRole('button', { name: 'Remove exemplar 1', exact: true }).click();
    await pinEditor.getByText('At most 4 exemplars. Remove one to save.', { exact: true }).waitFor({ state: 'detached' });
    await pinEditor.getByRole('group', { name: 'Exemplar 4', exact: true }).getByLabel('Situation', { exact: true }).fill('tired again');
    await pinEditor.getByRole('group', { name: 'Exemplar 4', exact: true }).getByLabel('Owner', { exact: true }).fill('今天好累。还有很多事');
    await pinEditor.getByRole('group', { name: 'Exemplar 4', exact: true }).getByLabel('You', { exact: true }).fill('辛苦了。');
    assert.equal(await pinEditor.getByRole('button', { name: 'Save exemplars', exact: true }).isDisabled(), false);
    await pinEditor.getByRole('button', { name: 'Cancel', exact: true }).click();
    assert.equal(exemplarHead.revision, 9, 'cancelling writes nothing');
    // Drift card: provenance, signed per-dial distance, keep and roll back by CAS.
    driftEnabled = true;
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    const driftCard = page.getByRole('region', { name: 'Drift', exact: true });
    await driftCard.waitFor();
    const driftText = await driftCard.innerText();
    assert.match(driftText, /Anchor · Default \(the first seed or owner revisions\) · /);
    assert.match(driftText, /Warmth\s+Medium → Very high\s+\+2/);
    assert.match(driftText, /Humor\s+Medium → Low\s+\u22121/);
    assert.match(driftText, /Since the anchor/);
    assert.match(driftText, /Story entries added: 1/);
    assert.match(driftText, /Story entries retired: 2/);
    assert.match(driftText, /Approved proposals: 3/);
    assert.equal(await driftCard.getByRole('button', { name: 'Roll back to anchor', exact: true }).isDisabled(), false);
    await driftCard.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-drift-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    // Keep: both revisions are named; a stale one is the existing 409 and writes nothing.
    const beforeKeep = actions.length;
    await driftCard.getByRole('button', { name: 'Keep as anchor', exact: true }).click();
    await page.getByText('The current revision changed.', { exact: false }).waitFor();
    assert.deepEqual(actions.slice(beforeKeep), [{ path: '/api/soul/anchor', body: { agent: 'nova', expected_tone_revision: 2, expected_story_revision: 2 } }]);
    assert.match(await driftCard.innerText(), /Warmth\s+Medium → Very high\s+\+2/);
    await driftCard.getByRole('button', { name: 'Keep as anchor', exact: true }).click();
    await page.getByText('Decision recorded.', { exact: false }).waitFor();
    assert.deepEqual(actions.at(-1), { path: '/api/soul/anchor', body: { agent: 'nova', expected_tone_revision: 2, expected_story_revision: 2 } });
    await driftCard.getByText('No drift from the anchor.', { exact: true }).waitFor();
    assert.match(await driftCard.innerText(), /Anchor · Set by the owner · /);
    assert.equal(await driftCard.getByRole('button', { name: 'Roll back to anchor', exact: true }).isDisabled(), true);
    // Roll back: confirm first, then the same CAS body; cancelling sends nothing.
    driftState = drifted();
    await page.getByRole('button', { name: 'Refresh', exact: true }).click();
    await driftCard.getByText('Story entries retired: 2', { exact: true }).waitFor();
    const beforeRollback = actions.length;
    await driftCard.getByRole('button', { name: 'Roll back to anchor', exact: true }).click();
    const confirm = page.getByRole('dialog');
    await confirm.getByText('Append new Tone and Story revisions equal to the anchor.', { exact: false }).waitFor();
    await confirm.getByRole('button', { name: 'Cancel', exact: true }).click();
    await confirm.waitFor({ state: 'detached' });
    assert.equal(actions.length, beforeRollback);
    await driftCard.getByRole('button', { name: 'Roll back to anchor', exact: true }).click();
    await confirm.getByRole('button', { name: 'Roll back to anchor', exact: true }).click();
    await driftCard.getByText('No drift from the anchor.', { exact: true }).waitFor();
    assert.deepEqual(actions.slice(beforeRollback), [{ path: '/api/soul/anchor/rollback', body: { agent: 'nova', expected_tone_revision: 2, expected_story_revision: 2 } }]);
    assert.match(await driftCard.innerText(), /Anchor · Default/);
    driftState = drifted();
    delayHistory = true;
    await page.getByRole('button', { name: 'History and sources', exact: true }).nth(0).click();
    await page.getByRole('button', { name: 'History and sources', exact: true }).nth(1).click();
    await page.getByText('Fresh principles history', { exact: true }).waitFor();
    await page.getByText('Proposal ID: 71', { exact: true }).waitFor();
    await page.getByText('· Restored revision: 1', { exact: true }).waitFor();
    assert.ok(oldHistoryRelease);
    const lateResponse = page.waitForResponse(r => r.url().includes('layer=identity'));
    await oldHistoryRelease();
    await lateResponse;
    await page.getByText('Fresh principles history', { exact: true }).waitFor();
    assert.equal(await page.getByLabel('Pairing code', { exact: true }).count(), 0);
    assert.equal(await page.getByRole('alert').count(), 0);
    await page.getByRole('button', { name: 'About me', exact: true }).click();
    await page.getByRole('heading', { name: currentHead.statement, exact: true }).waitFor();
    const openRejected = async () => { await page.getByRole('button', { name: 'All candidate review history' }).click(); await page.getByRole('button', { name: 'History and sources', exact: true }).click(); await page.getByText('Dismissed', { exact: true }).waitFor(); };
    await openRejected();
    await page.getByRole('button', { name: 'Limit scope' }).click();
    assert.equal(await page.getByLabel('Session ID', { exact: true }).evaluate(el => el === document.activeElement), true);
    await page.getByLabel('Session ID', { exact: true }).fill('weekend');
    await page.getByRole('button', { name: 'Apply reviewed change' }).click();
    await page.getByRole('button', { name: 'Apply reviewed change' }).waitFor({ state: 'detached' });
    assert.deepEqual(actions.at(-1).body, { action: 'narrow', narrowed_scope: 'session:weekend', expected_head: { id: 'revision-unseen-B' } });
    await page.getByRole('button', { name: 'All candidate review history' }).click();
    await page.getByRole('button', { name: 'History and sources', exact: true }).click();
    await page.getByText('Scope limited', { exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: 'Limit scope' }).count(), 0);
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-history-narrowed-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    // A server without the capability must never receive guarded approvals.
    supportsExpectedHead = false;
    await page.getByRole('button', { name: 'Inbox', exact: true }).click();
    await userCard.getByText('Upgrade the gateway', { exact: false }).waitFor();
    assert.equal(await userCard.getByRole('button', { name: 'Accept', exact: true }).count(), 0);
    assert.equal(await userCard.getByRole('button', { name: 'Reword', exact: true }).count(), 0);
    assert.equal(await userCard.getByRole('button', { name: 'Limit scope', exact: true }).count(), 0);
    const beforeDismiss = actions.length;
    await userCard.getByRole('button', { name: 'Dismiss', exact: true }).click();
    await page.getByText('Decision recorded.', { exact: false }).waitFor();
    assert.equal(actions.length, beforeDismiss + 1);
    assert.deepEqual(actions.at(-1).body, { action: 'reject' });
    reviewState = 'rejected';
    await page.getByRole('button', { name: 'About me', exact: true }).click();
    await openRejected();
    assert.equal(await page.getByRole('button', { name: 'Limit scope' }).count(), 0);
    await page.getByText('Upgrade the gateway', { exact: false }).waitFor();
    assert.equal(actions.length, beforeDismiss + 1);
    reviewState = 'narrowed';
    // The same card in Chinese, on the inbox tab the notification links to.
    driftState = drifted();
    locale = 'zh';
    await page.evaluate(() => localStorage.setItem('zeroclaw-locale', 'zh'));
    await page.reload();
    const zhDrift = page.getByRole('region', { name: '偏离', exact: true });
    await zhDrift.waitFor();
    assert.match(await zhDrift.innerText(), /锚点 · 默认/);
    await zhDrift.getByText('自锚点以来', { exact: true }).waitFor();
    await zhDrift.getByRole('button', { name: '保持为锚点', exact: true }).waitFor();
    await zhDrift.getByRole('button', { name: '回滚到锚点', exact: true }).waitFor();
    assert.match(await zhDrift.innerText(), /亲切度\s+中 → 极高\s+\+2/);
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-drift-zh-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    // Tone exemplars and the chat action in Chinese.
    await page.goto(origin + '/agent/nova');
    await chatMessage('剩下的我来').getByRole('button', { name: '用作语气示范', exact: true }).waitFor();
    await page.goto(origin + '/review?agent=nova');
    await page.getByRole('button', { name: '我的智能体', exact: true }).click();
    const zhExemplars = page.getByRole('region', { name: '示范片段', exact: true });
    await zhExemplars.getByText('s-a', { exact: true }).waitFor();
    await zhExemplars.getByRole('button', { name: '编辑示范片段', exact: true }).click();
    const zhEditor = page.getByRole('form', { name: '编辑示范片段', exact: true });
    const zhRow = zhEditor.getByRole('group', { name: '示范片段 1', exact: true });
    for (const label of ['情境', '主人', '你']) await zhRow.getByLabel(label, { exact: true }).waitFor();
    await zhRow.getByLabel('情境', { exact: true }).fill('累'.repeat(27));
    await zhRow.getByText('81 / 80 字节 · 超出字节限制。', { exact: true }).waitFor();
    await zhEditor.getByRole('button', { name: '保存示范片段', exact: true }).waitFor();
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-exemplar-editor-zh-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    await zhEditor.getByRole('button', { name: '取消', exact: true }).click();
    await page.getByRole('button', { name: '关于我', exact: true }).click();
    await page.getByRole('button', { name: '所有候选的审核历史' }).click();
    await page.getByRole('button', { name: '历史与来源', exact: true }).click();
    await page.getByText('已限定范围', { exact: true }).waitFor();
    assert.equal(await page.getByText('narrowed', { exact: true }).count(), 0);
    assert.equal(await page.getByText(/^reject ·/).count(), 0);
    await page.getByText(/^忽略 ·/).waitFor();
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-localized-history-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    await page.getByRole('button', { name: '待审核', exact: true }).click();
    for (const label of ['每周反思计时已开始', '本期没有可供反思的内容', '待审核提案已满', '反思模型调用失败: synthetic timeout: retry']) await page.getByText(label, { exact: true }).waitFor();
    assert.equal(await page.getByText('clock_started', { exact: true }).count(), 0);
    // A failure in the combined inbox or the other store cannot block a healthy tab.
    inboxUnavailable = true;
    headsUnavailable = true;
    const beforeIndependentTabs = inboxReads;
    await page.getByRole('button', { name: '我的智能体', exact: true }).click();
    await page.getByRole('heading', { name: '身份', exact: true }).waitFor();
    assert.equal(await page.getByRole('alert').count(), 0);
    assert.equal(inboxReads, beforeIndependentTabs);
    headsUnavailable = false;
    soulUnavailable = true;
    await page.getByRole('button', { name: '关于我', exact: true }).click();
    await page.getByRole('heading', { name: currentHead.statement, exact: true }).waitFor();
    assert.equal(await page.getByRole('alert').count(), 0);
    assert.equal(inboxReads, beforeIndependentTabs);
    // Healthy inbox data survives a separate heads failure, but grants no approvals.
    inboxUnavailable = false;
    headsUnavailable = true;
    supportsExpectedHead = true;
    await page.getByRole('button', { name: '待审核', exact: true }).click();
    await page.getByRole('alert').waitFor();
    await page.getByRole('heading', { name: addition.item.proposal, exact: true }).waitFor();
    await page.getByText('每周反思计时已开始', { exact: true }).waitFor();
    assert.equal(await userCard.getByRole('button', { name: '接受', exact: true }).count(), 0);
    assert.equal(await userCard.getByRole('button', { name: '改写', exact: true }).count(), 0);
    assert.equal(await userCard.getByRole('button', { name: '限定范围', exact: true }).count(), 0);
    assert.equal(await userCard.getByRole('button', { name: '忽略', exact: true }).count(), 1);
    assert.equal(await userCard.getByText('没有相同语义标识的当前条目。', { exact: true }).count(), 0);
    await userCard.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-heads-unavailable-375.png') });
    // Authentication failures still clear previously displayed owner data.
    headsFailureStatus = 401;
    await page.getByRole('button', { name: '刷新', exact: true }).click();
    await page.getByRole('button', { name: '以所有者身份配对', exact: true }).waitFor();
    assert.equal(await page.getByRole('heading', { name: addition.item.proposal, exact: true }).count(), 0);
    assert.equal(await userCard.count(), 0);
    assert.deepEqual(failures, []);
    console.log(JSON.stringify({ result: 'PASS', pairing_disabled_no_form_or_mint: true, bridge_rejected: true, pairing_enabled_recovery: true, stale_agent_recovered: true, stale_proposal_dismiss_only: true, stale_secondary_401_ignored: true, rejected_history_narrow_once: true, summary_44px: true, keyboard_editor_focus: true, bound_story_target: true, story_add_kind: true, replacement_head_visible: true, narrow_scope_valid: true, revision_provenance: true, localized_history: true, old_tone_separate: true, displayed_head_cas: true, head_conflict_refresh_only: true, old_gateway_approval_blocked: true, independent_profile_reads: true, localized_reflection_outcomes: true, tone_validation_refresh: true, heads_failure_preserves_inbox: true, heads_auth_failure_clears_data: true, drift_card_cas_keep_and_confirmed_rollback: true, drift_card_localized: true, tone_exemplars_editor_counters_cas_and_server_rejection: true, tone_exemplar_pin_from_chat_via_router_state: true, tone_exemplars_localized: true, actions }));
    await browser.close();
    server.close();
})().catch(async e => { console.error(e); server.close(); if (browser) await browser.close(); process.exitCode = 1; });
