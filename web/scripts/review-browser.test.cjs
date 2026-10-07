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
    let pairCalls = 0;
    let oldHistoryRelease;
    let delayHistory = false;
    let unknownReads = 0;
    let reviewState = 'rejected';
    const actions = [];
    const proposal = { id: 'soul:nova:7', kind: 'soul_proposal', agent: 'nova', item: { id: 7, layer: 'growth', proposal: 'Retire the old shared shorthand', rationale: 'Synthetic stale-target fixture', retire_index: 0, retire_target: { kind: 'bond', text: 'Actual canonical shared shorthand' }, target_revision: 17, created_at_unix: now } };
    const user = { id: 'user_model:8', kind: 'user_model_candidate', item: { id: '8', kind: 'preference', statement: 'Shorter planning replies', scope: 'global', evidence: 'Synthetic owner evidence', semantic_key: 'style', created_at_unix: now } };
    const rejected = { ...user.item, id: '42', statement: 'Previously dismissed preference' };
    const scopedItems = ['agent:nova', 'channel:telegram', 'session:original'].map((scope, i) => ({ ...user, id: `user_model:${9 + i}`, item: { ...user.item, id: String(9 + i), statement: `Scoped candidate ${scope}`, scope } }));
    const addition = { ...proposal, id: 'soul:nova:12', item: { id: 12, layer: 'growth', proposal: 'New growth addition', rationale: 'Synthetic addition', growth_kind: 'self', created_at_unix: now } };
    let items = [proposal, user, ...scopedItems, addition];
    const currentHead = { id: 'revision-current', semantic_key: 'style', kind: 'preference', statement: 'Existing detailed planning preference', scope: 'global', authority: 'owner_authored' };
    const head = (value) => ({ revision: 2, source: 'owner', created_at_unix: now, value });
    const soul = { agent: 'nova', identity: head({ name: 'Nova' }), principles: head({ items: ['Keep private information private.'] }), growth: head({ entries: [] }), voice: { configured: { warmth: 'high' }, stored: head({ heads: { warmth: 'low' } }) } };
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
            if (url.searchParams.get('agent') === 'deleted') {
                unknownReads++;
                return send({ code: 'unknown_agent', error: 'Unknown' }, 404);
            }
            return send({ items, total: items.length, next_offset: null });
        }
        if (req.method() === 'POST') {
            const body = req.postDataJSON();
            actions.push({ path: url.pathname, body });
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
        if (url.pathname === '/api/soul')
            return send(soul);
        if (url.pathname === '/api/soul/history') {
            if (delayHistory && url.searchParams.get('layer') === 'identity') {
                oldHistoryRelease = () => send({ code: 'unauthorized', error: 'late failure' }, 401);
                return;
            }
            return send({ layer: url.searchParams.get('layer'), revisions: [{ ...head({ text: 'Fresh principles history' }), source: 'approved_proposal', proposal_id: 71 }, { ...head({ text: 'Restored principles history' }), revision: 3, source: 'owner', rolled_back_from: 1 }] });
        }
        if (url.pathname === '/api/user-model/heads')
            return send({ heads: [currentHead] });
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
            assert.deepEqual(actions.at(-1).body, { action: 'narrow', narrowed_scope: 'session:original' });
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
    const voiceCard = page.locator('article').filter({ has: page.getByRole('heading', { name: 'Effective Voice', exact: true }) });
    await voiceCard.getByRole('heading', { name: 'Configured Voice', exact: true }).waitFor();
    await voiceCard.getByRole('heading', { name: 'Stored Voice', exact: true }).waitFor();
    assert.match(await voiceCard.innerText(), /high/);
    assert.match(await voiceCard.innerText(), /low/);
    await voiceCard.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-voice-fallback-375.png') });
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
    assert.deepEqual(actions.at(-1).body, { action: 'narrow', narrowed_scope: 'session:weekend' });
    await page.getByRole('button', { name: 'All candidate review history' }).click();
    await page.getByRole('button', { name: 'History and sources', exact: true }).click();
    await page.getByText('Scope limited', { exact: true }).waitFor();
    assert.equal(await page.getByRole('button', { name: 'Limit scope' }).count(), 0);
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-history-narrowed-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    locale = 'zh';
    await page.evaluate(() => localStorage.setItem('zeroclaw-locale', 'zh'));
    await page.reload();
    await page.getByRole('button', { name: '关于我', exact: true }).click();
    await page.getByRole('button', { name: '所有候选的审核历史' }).click();
    await page.getByRole('button', { name: '历史与来源', exact: true }).click();
    await page.getByText('已限定范围', { exact: true }).waitFor();
    assert.equal(await page.getByText('narrowed', { exact: true }).count(), 0);
    assert.equal(await page.getByText(/^reject ·/).count(), 0);
    await page.getByText(/^忽略 ·/).waitFor();
    await page.screenshot({ path: path.join(artifacts, 'zeroclaw-phone-localized-history-375.png') });
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
    assert.deepEqual(failures, []);
    console.log(JSON.stringify({ result: 'PASS', pairing_disabled_no_form_or_mint: true, bridge_rejected: true, pairing_enabled_recovery: true, stale_agent_recovered: true, stale_proposal_dismiss_only: true, stale_secondary_401_ignored: true, rejected_history_narrow_once: true, summary_44px: true, keyboard_editor_focus: true, bound_growth_target: true, growth_add_kind: true, replacement_head_visible: true, narrow_scope_valid: true, revision_provenance: true, localized_history: true, old_voice_separate: true, actions }));
    await browser.close();
    server.close();
})().catch(async e => { console.error(e); server.close(); if (browser) await browser.close(); process.exitCode = 1; });
