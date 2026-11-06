// Mount the production page; only the network and polling clock are controlled.
const assert = require('node:assert/strict');
const path = require('node:path');
const { JSDOM } = require('jsdom');
const { createJiti } = require('jiti');
const React = require('react');
const { act } = React;
const dom = new JSDOM('<div id="root"></div>', { url: 'https://zeroclaw.test', pretendToBeVisual: true });
Object.assign(globalThis, {
  window: dom.window, document: dom.window.document,
  localStorage: dom.window.localStorage, IS_REACT_ACT_ENVIRONMENT: true,
});
const { createRoot } = require('react-dom/client');
const jiti = createJiti(__filename, {
  jsx: { runtime: 'automatic' }, alias: { '@': path.resolve(__dirname, '../src') },
});
const Monitor = jiti('../src/pages/Monitor.tsx').default;
const categories = ['session', 'tool', 'agent'];
const firstStart = '2026-01-01T10:00:00Z';
const nextStart = '2026-01-01T11:00:00Z';
const event = (id, timestamp) => ({ id, '@timestamp': timestamp, event: { action: id }, attributes: { request_id: `request-${id}` } });
const timers = new Map();
let timerId = 0;
window.setInterval = callback => { const id = ++timerId; timers.set(id, callback); return id; };
window.clearInterval = id => timers.delete(id);
let start = firstStart;
let failures = new Set(categories);
let events = {};
let starts = {};
let responseBarrier = null;
const reads = [];
globalThis.fetch = async raw => {
  const url = new URL(raw, window.location.origin);
  assert.equal(url.pathname, '/api/logs');
  const category = url.searchParams.get('category');
  reads.push({ category, since: url.searchParams.get('since_ts') });
  const response = new Response(JSON.stringify(failures.has(category) ? { error: 'synthetic log read failure' } : {
    events: events[category] ?? [], daemon_started_at: starts[category] ?? start,
    at_end: true, next_cursor: null, next_cursor_line_offset: null, attribution_keys: [],
  }), { status: failures.has(category) ? 500 : 200 });
  if (responseBarrier) await responseBarrier;
  return response;
};
const root = createRoot(document.getElementById('root'));
const alert = () => document.querySelector('[role="alert"]');
const has = text => document.body.textContent.includes(text);
async function update(action) {
  await act(async () => {
    action();
    await new Promise(resolve => setImmediate(resolve));
  });
}
async function click(name) {
  const button = document.querySelector(`button[aria-label="${name}"]`);
  assert.ok(button, `button ${name} exists`);
  await update(() => button.click());
}

(async () => {
  await update(() => root.render(React.createElement(Monitor)));
  await click('Pause');
  assert.ok(alert(), 'all failed reads show an error');
  assert.equal(has('No turn events yet.'), false, 'failure is not an empty timeline');

  failures = new Set(['tool']);
  events = { session: [event('current-event', '2026-01-01T10:01:00Z'), event('old-event', '2026-01-01T09:59:00Z')] };
  await click('Refresh');
  assert.ok(has('current-event'));
  assert.equal(has('old-event'), false, 'initial history is scoped to this daemon');
  assert.ok(alert(), 'partial failure is visible alongside successful rows');

  failures.clear();
  events = { session: [event('current-event', '2026-01-01T10:01:00Z')], tool: [event('recovered-tool', '2026-01-01T10:00:30Z')] };
  await click('Refresh');
  assert.ok(has('recovered-tool'));
  assert.equal(alert(), null, 'full recovery clears the warning');
  assert.equal(reads.filter(r => r.category === 'tool').at(-1).since, firstStart,
    'failed category retries from the daemon anchor, not another category cursor');
  assert.equal(document.querySelectorAll('tbody tr').length, 2, 'repeat rows deduplicate');

  failures = new Set(categories);
  await click('Refresh');
  assert.ok(alert());
  assert.ok(has('recovered-tool'), 'outage preserves prior rows with a stale warning');

  failures.clear();
  start = nextStart;
  // Restart between category responses: an old batch cannot restore rows or
  // advance the new daemon's per-category cursor.
  starts = { tool: firstStart };
  events = { session: [event('new-daemon-event', '2026-01-01T11:01:00Z')], tool: [event('late-old-daemon', '2026-01-01T10:30:00Z')] };
  await click('Refresh');
  assert.ok(has('new-daemon-event'));
  for (const old of ['current-event', 'recovered-tool', 'late-old-daemon']) {
    assert.equal(has(old), false, 'restart discards prior daemon rows');
  }
  assert.ok(alert(), 'mixed daemon batch remains incomplete until retry');
  starts = {};
  events = {};
  await click('Refresh');
  assert.equal(alert(), null);
  assert.equal(reads.filter(r => r.category === 'tool').at(-1).since, nextStart, 'restart resets the failed category cursor');
  assert.ok(has('new-daemon-event'), 'quiet refresh retains the timeline');

  let release;
  responseBarrier = new Promise(resolve => { release = resolve; });
  events = { agent: [event('in-flight-event', '2026-01-01T11:02:00Z')] };
  await click('Auto');
  await click('Pause');
  await update(() => release());
  responseBarrier = null;
  assert.ok(has('in-flight-event'), 'pausing stops future polls without discarding an in-flight read');

  Object.defineProperty(document, 'hidden', { configurable: true, value: true });
  const beforeHidden = reads.length;
  await click('Auto');
  await update(() => { for (const tick of timers.values()) tick(); });
  assert.equal(reads.length, beforeHidden, 'hidden tabs do not poll');
  failures = new Set(categories);
  Object.defineProperty(document, 'hidden', { configurable: true, value: false });
  await update(() => document.dispatchEvent(new window.Event('visibilitychange')));
  assert.ok(alert());
  assert.equal(reads.length, beforeHidden + 3, 'visible tab catches up immediately');
  await update(() => root.unmount());
  assert.equal(timers.size, 0, 'unmount cancels polling');
  console.log('Monitor component regressions passed: failures, recovery, cursors, daemon restarts, visibility.');
})().catch(error => {
  console.error(error);
  process.exitCode = 1;
}).finally(() => dom.window.close());
