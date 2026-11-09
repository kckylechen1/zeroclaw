// Exercise the production AgentProvider and WebSocketClient together. Only the
// network and timer clock are controlled; React hooks and event handlers run as-is.
const assert = require('node:assert/strict');
const { test, after } = require('node:test');
const path = require('node:path');
const { JSDOM } = require('jsdom');
const { createJiti } = require('jiti');
const React = require('react');
const { act } = React;
const dom = new JSDOM('<body></body>', { url: 'https://zeroclaw.test', pretendToBeVisual: true });
Object.assign(globalThis, {
  window: dom.window, document: dom.window.document,
  localStorage: dom.window.localStorage, IS_REACT_ACT_ENVIRONMENT: true,
});
const { createRoot } = require('react-dom/client');
const jiti = createJiti(__filename, {
  jsx: { runtime: 'automatic' }, alias: { '@': path.resolve(__dirname, '../src') },
});
const { AgentProvider, useAgent } = jiti('../src/contexts/AgentContext.tsx');
const { AgentChatInner } = jiti('../src/pages/AgentChat.tsx');
const { MemoryRouter } = require('react-router-dom');
dom.window.HTMLElement.prototype.scrollIntoView = () => {};

const sockets = [];
class Socket {
  static OPEN = 1;
  readyState = 0;
  sent = [];
  constructor(url) { this.url = url; sockets.push(this); }
  send(raw) { assert.equal(this.readyState, Socket.OPEN); this.sent.push(JSON.parse(raw)); }
  open() { this.readyState = Socket.OPEN; this.onopen?.(); }
  receive(frame) { this.onmessage?.({ data: JSON.stringify(frame) }); }
  close(code = 1000) { this.readyState = 3; this.onclose?.({ code }); }
}
globalThis.WebSocket = Socket;
const realSetTimeout = globalThis.setTimeout;
const realClearTimeout = globalThis.clearTimeout;
const timers = new Map();
let timerId = 0;
globalThis.setTimeout = (callback, delay) => {
  const id = ++timerId;
  timers.set(id, { callback, delay });
  return id;
};
globalThis.clearTimeout = id => timers.delete(id);
function tick(delay) {
  for (const [id, timer] of [...timers]) {
    if (timer.delay === delay) { timers.delete(id); timer.callback(); }
  }
}
const requests = [];
let configuredModel = 'synthetic.model';
globalThis.fetch = async (raw, options = {}) => {
  const url = new URL(raw, window.location.origin);
  const method = options.method ?? 'GET';
  requests.push({ method, path: url.pathname });
  let data;
  if (url.pathname.endsWith('/messages')) data = { session_persistence: false, messages: [] };
  else if (url.pathname.endsWith('/abort')) data = { status: 'abort_requested' };
  else if (method === 'DELETE' && url.pathname.startsWith('/api/sessions/')) data = { deleted: true };
  else if (url.pathname === '/api/status') data = { model: configuredModel };
  else if (url.pathname === '/api/config/catalog') data = { providers: [] };
  else if (url.pathname === '/api/config/list') data = { entries: [] };
  else if (url.pathname === '/api/attachments' && method === 'POST') data = { id: 'synthetic-file', file_name: 'note.txt' };
  else if (url.pathname === '/api/config/prop') {
    if (method === 'PUT') configuredModel = JSON.parse(options.body).value;
    data = { value: configuredModel };
  } else throw new Error(`Unexpected API request ${method} ${url.pathname}`);
  return new Response(JSON.stringify(data), { status: 200 });
};
async function update(action) {
  await act(async () => { await action(); await new Promise(resolve => setImmediate(resolve)); });
}
async function withAgent(token, run, renderChat = false) {
  localStorage.clear();
  if (token) localStorage.setItem('zeroclaw_token', 'synthetic-owner');
  sockets.length = 0;
  requests.length = 0;
  const container = document.createElement('div');
  document.body.append(container);
  const root = createRoot(container);
  let value;
  function Probe() {
    value = useAgent();
    return React.createElement('pre', null, JSON.stringify({
      messages: value.messages, typing: value.typing, error: value.error,
      streamingContent: value.streamingContent, stopRequested: value.stopRequested,
    }));
  }
  try {
    await update(() => root.render(React.createElement(AgentProvider, { agentAlias: 'test-agent' },
      React.createElement(MemoryRouter, null,
        React.createElement(Probe),
        renderChat && React.createElement(AgentChatInner, { agentAlias: 'test-agent' })))));
    await update(() => sockets[0].open());
    await run({ get value() { return value; }, wire: sockets[0], container });
  } finally {
    await update(() => root.unmount());
    container.remove();
    timers.clear();
  }
}
const turnId = 'f1111111-1111-4111-8111-111111111111';
const otherTurnId = 'f2222222-2222-4222-8222-222222222222';
const ready = (epoch = 'epoch-1') => ({ type: 'turn_ready', epoch, limits: { replay_events: 128 } });
const event = (request, seq, inner, turn = turnId) => ({ type: 'turn_event', request_id: request, turn_id: turn, seq, event: inner });
const sentMessages = () => sockets.flatMap(socket => socket.sent).filter(frame => frame.op === 'message' || frame.type === 'message');
const restAborts = () => requests.filter(request => request.path.endsWith('/abort'));
const sideRequests = () => sockets.flatMap(socket => socket.sent).filter(frame =>
  frame.type === 'btw' || (frame.op === 'message' && frame.content.startsWith('/btw ')));
const sideAnswers = ctx => ctx.value.messages.filter(message => message.sideQuery && message.role === 'agent');
const persistedMessages = () => JSON.parse(localStorage.getItem(
  `zeroclaw_chat_history_v1:${localStorage.getItem('zeroclaw_session_id.test-agent')}`)).messages;
async function typeChat(ctx, text) {
  const input = ctx.container.querySelector('textarea');
  assert.equal(input.disabled, false, 'the composer stays available for side queries');
  await update(() => {
    Object.getOwnPropertyDescriptor(dom.window.HTMLTextAreaElement.prototype, 'value').set.call(input, text);
    input.dispatchEvent(new dom.window.Event('input', { bubbles: true }));
  });
  return input;
}

test('idle /btw uses the side-query contract and renders a temporary answer without a main turn', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendMessage('/btw summarize the context'));
  const query = sideRequests()[0];
  assert.ok(query.request_id.startsWith('btw-'));
  assert.equal(query.epoch, 'epoch-1');
  assert.equal(ctx.value.typing, false);
  await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: query.request_id, status: 'btw', turn_id: null }));
  await update(() => ctx.wire.receive({ type: 'btw_response', id: query.request_id, content: 'Synthetic side answer.' }));
  assert.deepEqual(sideAnswers(ctx).map(message => message.content), ['Synthetic side answer.']);
  assert.ok(ctx.value.messages.every(message => message.ephemeral));
  assert.deepEqual(persistedMessages(), []);
  assert.equal(ctx.value.typing, false);
  assert.equal(ctx.value.error, null);
  await update(() => ctx.value.sendMessage('next ordinary task'));
  assert.equal(ctx.value.typing, true, 'a side query never occupies main-turn admission');
}));

test('busy /btw receipts, results and failures preserve streaming, approval and Stop correlation', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive({ ...ready(), capabilities: ['steer'] }));
  await update(() => ctx.value.sendMessage('main task'));
  const parent = sentMessages()[0].request_id;
  await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: parent, turn_id: turnId, status: 'accepted' }));
  await update(() => ctx.wire.receive(event(parent, 1, { type: 'chunk', content: 'main output' })));
  await update(() => ctx.wire.receive(event(parent, 2, { type: 'approval_request', request_id: 'synthetic-approval', tool: 'shell', timeout_secs: 60 })));
  await update(() => assert.equal(ctx.value.steerMessage('/btw side question'), true));
  const query = sideRequests()[0];
  assert.equal(ctx.wire.sent.some(frame => frame.op === 'steer'), false, '/btw cannot become a correction');
  await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: query.request_id, turn_id: turnId, status: 'btw' }));
  assert.equal(ctx.value.pendingApproval.requestId, 'synthetic-approval');
  assert.equal(ctx.value.canSteer, true);
  await update(() => ctx.value.abortSession());
  assert.deepEqual(ctx.wire.sent.at(-1), {
    type: 'turn_control', version: 1, op: 'cancel', epoch: 'epoch-1', request_id: parent, turn_id: turnId,
  });
  for (const receipt of [
    { type: 'btw_error', id: query.request_id, message: 'synthetic provider failure' },
    { type: 'turn_error', op: 'message', request_id: query.request_id, code: 'BTW_BUSY' },
    { type: 'btw_response', id: query.request_id, content: 'late duplicate answer' },
  ]) await update(() => ctx.wire.receive(receipt));
  assert.ok(sideAnswers(ctx)[0].content.includes('synthetic provider failure'));
  assert.equal(ctx.value.typing, true);
  assert.equal(ctx.value.stopRequested, true);
  assert.equal(ctx.value.streamingContent, 'main output');
  assert.deepEqual(persistedMessages().map(message => message.content), ['main task']);
  await update(() => ctx.wire.receive(event(parent, 3, { type: 'turn_terminal', outcome: 'aborted' })));
  assert.equal(ctx.value.typing, false);
  assert.equal(ctx.value.stopRequested, false);
}));

test('side queries correlate out of order, bound concurrency and ignore duplicates or other clients', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  for (let i = 0; i < 4; i++) await update(() => assert.equal(ctx.value.sendSideQuery(`question ${i}`), true));
  await update(() => assert.equal(ctx.value.sendSideQuery('over capacity'), false));
  assert.equal(sideRequests().length, 4);
  const queries = sideRequests();
  const ids = sideAnswers(ctx).map(message => message.id);
  await update(() => ctx.wire.receive({ type: 'btw_response', id: queries[3].request_id, full_response: 'answer 3' }));
  await update(() => ctx.wire.receive({ type: 'turn_error', request_id: queries[1].request_id, op: 'message', code: 'BTW_BUSY' }));
  await update(() => ctx.wire.receive({ type: 'btw_response', id: queries[0].request_id, content: 'answer 0' }));
  await update(() => ctx.wire.receive({ type: 'btw_response', id: queries[3].request_id, content: 'duplicate answer' }));
  await update(() => ctx.wire.receive({ type: 'btw_response', id: 'btw-other-client', content: 'unrelated answer' }));
  assert.deepEqual(sideAnswers(ctx).map(message => message.id), ids, 'results replace their own pending bubble');
  assert.equal(sideAnswers(ctx)[0].content, 'answer 0');
  assert.ok(sideAnswers(ctx)[1].content.includes('BTW_BUSY'));
  assert.equal(sideAnswers(ctx)[3].content, 'answer 3');
  assert.equal(ctx.value.messages.some(message => message.content === 'unrelated answer'), false);
  await update(() => assert.equal(ctx.value.sendSideQuery('a freed slot'), true));
  assert.equal(ctx.value.typing, false);
}));

test('side-query validation rejects attachments and invalid UTF-8 byte lengths without losing main state', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => assert.equal(ctx.value.sendSideQuery('with a file', ['synthetic-handle']), false));
  await update(() => assert.equal(ctx.value.sendSideQuery('  '), false));
  await update(() => assert.equal(ctx.value.sendSideQuery('界'.repeat(5462)), false));
  assert.equal(sideRequests().length, 0);
  assert.ok(ctx.value.messages.some(message => message.content.includes('attachments')));
  assert.equal(ctx.value.typing, false);
  await update(() => assert.equal(ctx.value.sendSideQuery('x'.repeat(16 * 1024)), true));
  assert.equal(sideRequests().length, 1, 'the exact byte limit remains usable');
}));

test('side queries never queue before ready or replay after disconnect while the main turn resumes', async () => withAgent(true, async ctx => {
  await update(() => assert.equal(ctx.value.sendSideQuery('before ready'), false));
  await update(() => ctx.wire.receive(ready()));
  assert.equal(sideRequests().length, 0);
  await update(() => ctx.value.sendMessage('main task'));
  const parent = sentMessages()[0].request_id;
  await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: parent, turn_id: turnId, status: 'accepted' }));
  await update(() => ctx.value.sendSideQuery('lost on disconnect'));
  const query = sideRequests()[0];
  await update(() => ctx.wire.close(1006));
  assert.ok(sideAnswers(ctx)[0].content.includes('unknown'));
  await update(() => assert.equal(ctx.value.sendSideQuery('while disconnected'), false));
  await update(() => tick(1000));
  const replacement = sockets.at(-1);
  await update(() => replacement.open());
  await update(() => replacement.receive(ready()));
  assert.equal(sideRequests().length, 1);
  assert.deepEqual(replacement.sent.filter(frame => frame.op === 'resume').map(frame => frame.request_id), [parent]);
  await update(() => replacement.receive({ type: 'btw_response', id: query.request_id, content: 'late answer' }));
  await update(() => replacement.receive({ type: 'turn_error', request_id: query.request_id, code: 'BTW_UNAVAILABLE', op: 'message' }));
  assert.equal(ctx.value.typing, true);
  assert.ok(sideAnswers(ctx)[0].content.includes('unknown'));
}));

test('a timed-out side query stays retired and its late error cannot stop a new main turn', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendSideQuery('timeout question'));
  const query = sideRequests()[0];
  await update(() => tick(65_000));
  assert.ok(sideAnswers(ctx)[0].content.includes('unknown'));
  await update(() => ctx.value.sendMessage('successor main task'));
  await update(() => ctx.wire.receive({ type: 'error', id: query.request_id, code: 'BTW_FAILED', message: 'late error' }));
  await update(() => ctx.wire.receive({ type: 'btw_response', id: query.request_id, content: 'late answer' }));
  assert.equal(ctx.value.typing, true);
  assert.ok(sideAnswers(ctx)[0].content.includes('unknown'));
  assert.equal(sideRequests().length, 1);
}));

for (const retirement of ['clear', 'model switch']) {
  test(`${retirement} retires side queries before replacing the attachment`, async () => withAgent(true, async ctx => {
    await update(() => ctx.wire.receive(ready()));
    await update(() => ctx.value.sendSideQuery('old attachment question'));
    const query = sideRequests()[0];
    await update(() => retirement === 'clear' ? ctx.value.clearAllMessages() : ctx.value.switchModel('synthetic.next-model'));
    const replacement = sockets.at(-1);
    assert.notEqual(replacement, ctx.wire);
    await update(() => replacement.open());
    await update(() => replacement.receive(ready()));
    await update(() => ctx.wire.receive({ type: 'btw_response', id: query.request_id, content: 'old socket answer' }));
    await update(() => replacement.receive({ type: 'btw_response', id: query.request_id, content: 'old broadcast answer' }));
    assert.equal(sideRequests().length, 1);
    assert.equal(sideAnswers(ctx).some(message => message.content.endsWith('answer')), false);
    if (retirement === 'clear') assert.equal(ctx.value.messages.length, 0);
    else assert.ok(sideAnswers(ctx)[0].content.includes('unknown'));
    await update(() => assert.equal(ctx.value.sendSideQuery('new attachment question'), true));
  }));
}

test('legacy side queries use id and keep both answers and admission errors out of the main stream', async () => withAgent(false, async ctx => {
  await update(() => ctx.value.sendMessage('legacy main task'));
  await update(() => ctx.wire.receive({ type: 'chunk', content: 'legacy output' }));
  await update(() => ctx.value.sendSideQuery('legacy side question'));
  const query = sideRequests()[0];
  assert.equal(query.type, 'btw');
  assert.ok(query.id);
  assert.equal(query.request_id, undefined);
  assert.equal(query.content, '/btw legacy side question');
  await update(() => ctx.wire.receive({ type: 'ack', id: query.id, status: 'accepted', turn: 'btw', durable: false }));
  await update(() => ctx.wire.receive({ type: 'btw_response', id: query.id, content: 'legacy side answer' }));
  await update(() => ctx.value.sendSideQuery('legacy refused question'));
  await update(() => ctx.wire.receive({ type: 'error', id: sideRequests()[1].id, code: 'BTW_UNAVAILABLE', message: 'unavailable' }));
  assert.equal(sideAnswers(ctx)[0].content, 'legacy side answer');
  assert.ok(sideAnswers(ctx)[1].content.includes('unavailable'));
  assert.equal(ctx.value.streamingContent, 'legacy output');
  assert.equal(ctx.value.typing, true);
  await update(() => ctx.value.abortSession());
  assert.equal(restAborts().length, 1, 'side requests cannot change legacy Stop routing');
}));

test('chat composer recognizes idle /btw and its slash escape, and renders the temporary answer', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  const input = await typeChat(ctx, '/btw idle question');
  await update(() => input.dispatchEvent(new dom.window.KeyboardEvent('keydown', { key: 'Enter', bubbles: true })));
  assert.equal(sideRequests().length, 1, '/btw reaches the gateway instead of an unknown-command notice');
  assert.equal(input.value, '');
  await update(() => ctx.wire.receive({ type: 'btw_response', id: sideRequests()[0].request_id, content: 'Visible side answer' }));
  assert.ok([...ctx.container.querySelectorAll('.chat-markdown')].some(node => node.textContent === 'Visible side answer'));
  assert.ok(ctx.container.textContent.includes('Side query · not saved in chat history'));
  await typeChat(ctx, '//btw escaped question');
  await update(() => ctx.container.querySelector('button[aria-label="Ask aside"]').click());
  assert.equal(sideRequests()[1].content, '/btw escaped question');
  assert.equal(ctx.value.typing, false);
}, true));

for (const steering of [false, true]) {
  test(`chat composer keeps busy /btw separate from steering (capability=${steering})`, async () => withAgent(true, async ctx => {
    await update(() => ctx.wire.receive({ ...ready(), capabilities: steering ? ['steer'] : [] }));
    await update(() => ctx.value.sendMessage('main task'));
    const parent = sentMessages().at(-1).request_id;
    await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: parent, turn_id: turnId, status: 'accepted' }));
    if (steering) await update(() => assert.equal(ctx.value.steerMessage('pending correction'), true));
    const input = await typeChat(ctx, '/btw busy question');
    await update(() => input.dispatchEvent(new dom.window.KeyboardEvent('keydown', { key: 'Enter', bubbles: true })));
    assert.equal(sideRequests().length, 1);
    const query = sideRequests()[0];
    await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: query.request_id, turn_id: turnId, status: 'btw' }));
    await update(() => ctx.wire.receive({ type: 'btw_response', id: query.request_id, content: 'Busy side answer' }));
    assert.ok([...ctx.container.querySelectorAll('.chat-markdown')].some(node => node.textContent === 'Busy side answer'));
    assert.equal(ctx.value.steeringPending, steering, 'side-query completion cannot settle a pending correction');
    assert.equal(ctx.value.typing, true);
    assert.ok(ctx.container.querySelector('button[aria-label="Stop"]'));
    assert.equal(ctx.wire.sent.filter(frame => frame.op === 'steer').length, steering ? 1 : 0);
    await update(() => ctx.container.querySelector('button[aria-label="Stop"]').click());
    assert.equal(ctx.wire.sent.at(-1).request_id, parent);
    assert.equal(ctx.wire.sent.at(-1).op, 'cancel');
  }, true));
}

test('chat composer keeps attachments and text when /btw rejects them, then sends after removal', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  const fileInput = ctx.container.querySelector('input[type="file"]');
  await update(() => {
    Object.defineProperty(fileInput, 'files', { configurable: true, value: [new File(['synthetic'], 'note.txt', { type: 'text/plain' })] });
    fileInput.dispatchEvent(new dom.window.Event('change', { bubbles: true }));
  });
  assert.ok(ctx.container.textContent.includes('note.txt'));
  await typeChat(ctx, '/btw with attachment');
  await update(() => ctx.container.querySelector('button[aria-label="Ask aside"]').click());
  assert.equal(sideRequests().length, 0);
  assert.equal(ctx.container.querySelector('textarea').value, '/btw with attachment');
  assert.ok(ctx.container.textContent.includes('Side queries do not accept attachments'));
  assert.ok(ctx.container.textContent.includes('note.txt'));
  await update(() => ctx.container.querySelector('button[aria-label="Remove attachment"]').click());
  await update(() => ctx.container.querySelector('button[aria-label="Ask aside"]').click());
  assert.equal(sideRequests().length, 1);
  assert.equal(sideRequests()[0].attachments, undefined);
}, true));

test('explicit steering is negotiated, scoped and never replaces ordinary turn correlation', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive({ ...ready(), capabilities: ['steer'] }));
  await update(() => ctx.value.sendMessage('original task'));
  const parent = sentMessages().at(-1).request_id;
  assert.equal(ctx.value.canSteer, false, 'wait for authoritative turn identity');
  await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: parent, turn_id: turnId, status: 'accepted' }));
  assert.equal(ctx.value.canSteer, true);
  await update(() => assert.equal(ctx.value.steerMessage('correct destination', ['scoped-file']), true));
  const correction = ctx.wire.sent.at(-1);
  assert.equal(correction.op, 'steer');
  assert.equal(correction.parent_request_id, parent);
  assert.equal(correction.turn_id, turnId);
  assert.equal(correction.epoch, 'epoch-1');
  assert.deepEqual(correction.attachments, ['scoped-file']);
  assert.notEqual(correction.request_id, parent);
  assert.equal(ctx.value.steeringPending, true);
  await update(() => ctx.value.sendMessage('ordinary busy input'));
  assert.equal(sentMessages().length, 1);
  await update(() => ctx.wire.receive({ ...correction, type: 'turn_steer_ack', turn_id: otherTurnId, status: 'accepted' }));
  assert.equal(ctx.value.steeringPending, true, 'wrong target ACK is inert');
  await update(() => ctx.wire.receive({ ...correction, type: 'turn_steer_ack', status: 'accepted' }));
  assert.equal(ctx.value.steeringPending, false);
  assert.equal(ctx.value.typing, true, 'steer ACK cannot settle the task');
  await update(() => ctx.value.abortSession());
  assert.equal(ctx.wire.sent.at(-1).request_id, parent, 'Stop keeps the original admission identity');
  assert.equal(ctx.value.canSteer, false);
  await update(() => assert.equal(ctx.value.steerMessage('after cancel'), false));
}));

test('steering is unavailable without server opt-in and late receipts cannot affect a successor', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendMessage('original task'));
  const parent = sentMessages().at(-1).request_id;
  await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: parent, turn_id: turnId, status: 'accepted' }));
  assert.equal(ctx.value.canSteer, false);
  await update(() => assert.equal(ctx.value.steerMessage('unsupported'), false));
  await update(() => ctx.wire.close());
  await update(() => tick(1_000));
  const reconnected = sockets.at(-1);
  await update(() => reconnected.open());
  await update(() => reconnected.receive({ ...ready(), capabilities: ['steer'] }));
  await update(() => assert.equal(ctx.value.steerMessage('receipt lost'), true));
  const correction = reconnected.sent.at(-1);
  await update(() => tick(5_000));
  assert.equal(ctx.value.steeringPending, false);
  assert.equal(sockets.flatMap(socket => socket.sent).filter(frame => frame.op === 'steer').length, 1, 'unknown receipt is never resent');
  await update(() => reconnected.receive(event(parent, 1, { type: 'turn_terminal', outcome: 'done' })));
  await update(() => ctx.value.sendMessage('successor'));
  const successor = sentMessages().at(-1).request_id;
  await update(() => reconnected.receive({ type: 'turn_ack', request_id: successor, turn_id: otherTurnId, status: 'accepted' }));
  await update(() => assert.equal(ctx.value.steerMessage('new correction'), true));
  await update(() => reconnected.receive({ ...correction, type: 'turn_steer_ack', status: 'accepted' }));
  assert.equal(ctx.value.steeringPending, true, 'old receipt cannot clear a new pending correction');
  await update(() => reconnected.close());
  assert.equal(ctx.value.steeringPending, false, 'disconnect settles as unknown without queueing');
}));

test('Clear chat retires pending steering before a new attachment can send', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive({ ...ready(), capabilities: ['steer'] }));
  await update(() => ctx.value.sendMessage('old task'));
  const parent = sentMessages().at(-1).request_id;
  await update(() => ctx.wire.receive({ type: 'turn_ack', request_id: parent, turn_id: turnId, status: 'accepted' }));
  await update(() => assert.equal(ctx.value.steerMessage('pending old correction'), true));
  const oldCorrection = ctx.wire.sent.at(-1);
  await update(() => ctx.value.clearAllMessages());
  assert.equal(ctx.value.steeringPending, false);
  assert.equal(ctx.value.canSteer, false);
  const replacement = sockets.at(-1);
  await update(() => replacement.open());
  await update(() => replacement.receive({ ...ready(), capabilities: ['steer'] }));
  await update(() => ctx.value.sendMessage('new task'));
  assert.equal(sentMessages().at(-1).content, 'new task');
  const successor = sentMessages().at(-1).request_id;
  await update(() => replacement.receive({ type: 'turn_ack', request_id: successor, turn_id: otherTurnId, status: 'accepted' }));
  await update(() => assert.equal(ctx.value.steerMessage('new correction'), true));
  await update(() => ctx.wire.receive({ ...oldCorrection, type: 'turn_steer_ack', status: 'accepted' }));
  assert.equal(ctx.value.steeringPending, true, 'retired attachment cannot settle a replacement control');
}));

test('anonymous pairing-disabled chat uses its supported legacy contract', async () => withAgent(false, async ({ value, wire }) => {
  assert.equal(wire.sent.length, 0, 'S2 cannot negotiate without an authenticated subject');
  await update(() => value.sendMessage('anonymous chat', ['scoped-handle']));
  const sent = wire.sent[0];
  assert.equal(sent.type, 'message');
  assert.equal(sent.content, 'anonymous chat');
  assert.deepEqual(sent.attachments, ['scoped-handle']);
  assert.ok(sent.request_id);
  await update(() => value.abortSession());
  assert.equal(restAborts().length, 1, 'legacy Stop retains its existing endpoint');
  await update(() => wire.receive({ type: 'aborted' }));
}));

test('Stop withdraws an unsent request before turn_ready without a session abort', async () => withAgent(true, async ctx => {
  assert.equal(ctx.wire.sent[0].op, 'negotiate');
  await update(() => ctx.value.sendMessage('must not run'));
  assert.equal(sentMessages().length, 0);
  assert.equal(ctx.value.typing, true);
  await update(() => ctx.value.abortSession());
  assert.equal(ctx.value.typing, false);
  assert.equal(restAborts().length, 0);
  await update(() => ctx.wire.receive(ready()));
  assert.equal(sentMessages().length, 0, 'ready cannot resurrect the cancelled queue entry');
  await update(() => ctx.value.sendMessage('explicit next request'));
  assert.deepEqual(sentMessages().map(frame => frame.content), ['explicit next request']);
}));

for (const refusal of [
  { type: 'turn_error', op: 'negotiate', code: 'AUTHENTICATION_REQUIRED' },
  { type: 'error', code: 'BRIDGE_OPERATION_DENIED', message: 'Access denied' },
]) {
  test(`negotiation refusal ${refusal.code} settles queued requests and blocks late flush`, async () => withAgent(true, async ctx => {
    await update(() => ctx.value.sendMessage('queued before refusal'));
    await update(() => ctx.wire.receive(refusal));
    assert.equal(ctx.value.typing, false);
    assert.equal(ctx.value.connected, false);
    assert.ok(ctx.value.error.includes(refusal.code));
    await update(() => ctx.wire.receive(ready()));
    await update(() => ctx.value.sendMessage('cannot send on refused transport'));
    assert.equal(sentMessages().length, 0);
    assert.equal(restAborts().length, 0);
  }));
}

test('Stop waits for an ACK-bound turn id and ignores stale cancellation acknowledgements', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendMessage('sent but not acknowledged'));
  const request = sentMessages()[0].request_id;
  await update(() => ctx.value.abortSession());
  assert.equal(ctx.value.typing, true, 'sent work has not been confirmed stopped');
  assert.equal(ctx.value.stopRequested, true);
  assert.equal(restAborts().length, 0);
  assert.equal(ctx.wire.sent.filter(frame => frame.op === 'cancel').length, 0);
  await update(() => ctx.wire.receive({ type: 'turn_ack', status: 'accepted', request_id: request, turn_id: turnId }));
  assert.deepEqual(ctx.wire.sent.find(frame => frame.op === 'cancel'), {
    type: 'turn_control', version: 1, op: 'cancel', epoch: 'epoch-1', request_id: request, turn_id: turnId,
  });
  await update(() => ctx.wire.receive(event(request, 1, { type: 'turn_terminal', outcome: 'aborted' })));
  assert.equal(ctx.value.typing, false);
  assert.equal(ctx.value.stopRequested, false);
  const oldAck = { type: 'turn_cancel_ack', request_id: request, turn_id: turnId, status: 'requested' };
  await update(() => ctx.wire.receive(oldAck));
  assert.equal(ctx.value.stopRequested, false, 'late acknowledgement cannot revive a settled Stop badge');
  await update(() => ctx.value.sendMessage('next turn'));
  const nextRequest = sentMessages()[1].request_id;
  await update(() => ctx.wire.receive({ type: 'turn_ack', status: 'accepted', request_id: nextRequest, turn_id: otherTurnId }));
  await update(() => ctx.wire.receive(oldAck));
  assert.equal(ctx.value.stopRequested, false, 'old cancellation cannot affect a later turn');
  assert.equal(ctx.value.typing, true);
}));

test('Stop survives reconnect and binds to the replayed request without resending it', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendMessage('unknown admission'));
  const request = sentMessages()[0].request_id;
  await update(() => ctx.wire.close(1006));
  await update(() => ctx.value.abortSession());
  await update(() => tick(1000));
  const reconnect = sockets.at(-1);
  await update(() => reconnect.open());
  await update(() => reconnect.receive(ready()));
  assert.ok(reconnect.sent.some(frame => frame.op === 'resume' && frame.request_id === request));
  assert.equal(sentMessages().length, 1);
  await update(() => reconnect.receive({ type: 'turn_replay', request_id: request, turn_id: turnId, events: [], last_seq: 0, terminal: false }));
  assert.ok(reconnect.sent.some(frame => frame.op === 'cancel' && frame.request_id === request && frame.turn_id === turnId));
  assert.equal(restAborts().length, 0);
  assert.equal(ctx.value.typing, true);
  await update(() => reconnect.receive(event(request, 1, { type: 'turn_terminal', outcome: 'aborted' })));
  assert.equal(ctx.value.typing, false);
}));

test('losing authentication on reconnect never downgrades a sent S2 request', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendMessage('possibly admitted'));
  await update(() => ctx.wire.close(1006));
  localStorage.removeItem('zeroclaw_token');
  await update(() => tick(1000));
  const reconnect = sockets.at(-1);
  await update(() => reconnect.open());
  assert.equal(reconnect.sent[0].op, 'negotiate');
  await update(() => ctx.value.abortSession());
  await update(() => reconnect.receive({ type: 'turn_error', op: 'negotiate', code: 'AUTHENTICATION_REQUIRED' }));
  assert.equal(sentMessages().length, 1);
  assert.equal(restAborts().length, 0);
  assert.ok(ctx.value.messages.some(message => message.notice && message.content.includes('outcome')));
  assert.equal(ctx.value.typing, false);
}));

for (const refused of [false, true]) {
  test(`S2 stays required after ${refused ? 'refused' : 'interrupted'} initial negotiation and token loss`, async () => withAgent(true, async ctx => {
    await update(() => ctx.value.sendMessage('initially queued request'));
    if (refused) {
      await update(() => ctx.wire.receive({ type: 'turn_error', op: 'negotiate', code: 'AUTHENTICATION_REQUIRED' }));
    }
    await update(() => ctx.wire.close(1006));
    localStorage.removeItem('zeroclaw_token');
    await update(() => tick(1000));
    const reconnect = sockets.at(-1);
    await update(() => reconnect.open());
    assert.equal(reconnect.sent[0].op, 'negotiate');
    await update(() => ctx.value.abortSession());
    await update(() => reconnect.receive(ready()));
    assert.equal(sentMessages().length, 0, 'neither refused nor locally stopped work can execute later');
    assert.equal(restAborts().length, 0);
  }));
}

test('a model-switch client preserves the previous attachment S2 requirement', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  localStorage.removeItem('zeroclaw_token');
  await update(() => ctx.value.switchModel('synthetic.next'));
  const replacement = sockets.at(-1);
  assert.notEqual(replacement, ctx.wire);
  await update(() => replacement.open());
  assert.equal(replacement.sent[0].op, 'negotiate');
  assert.equal(restAborts().length, 0);
}));

for (const negotiated of [false, true]) {
  test(`Clear chat preserves S2 after token loss ${negotiated ? 'after' : 'before'} initial ready`, async () => withAgent(true, async ctx => {
    if (negotiated) await update(() => ctx.wire.receive(ready()));
    localStorage.removeItem('zeroclaw_token');
    await update(() => ctx.value.clearAllMessages());
    assert.ok(requests.some(request => request.method === 'DELETE' && request.path.startsWith('/api/sessions/')));
    const replacement = sockets.at(-1);
    assert.notEqual(replacement, ctx.wire);
    await update(() => replacement.open());
    assert.equal(replacement.sent[0]?.op, 'negotiate', 'history reset cannot downgrade the selected contract');
    await update(() => ctx.value.sendMessage('queued on the replacement'));
    assert.equal(sentMessages().length, 0);
    await update(() => ctx.wire.receive({ type: 'turn_error', op: 'negotiate', code: 'AUTHENTICATION_REQUIRED' }));
    assert.equal(ctx.value.connected, true, 'a stale callback cannot reject the replacement');
    assert.equal(ctx.value.typing, true);
    await update(() => ctx.value.abortSession());
    assert.equal(restAborts().length, 0, 'Stop must not fall back to session REST abort');
    assert.equal(ctx.value.typing, false);
    await update(() => replacement.receive(ready()));
    assert.equal(sentMessages().length, 0, 'ready cannot send the withdrawn request');
  }));
}

test('a changed process epoch settles sent uncertainty without replaying the message', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendMessage('old epoch request'));
  await update(() => ctx.wire.close(1006));
  await update(() => tick(1000));
  const reconnect = sockets.at(-1);
  await update(() => reconnect.open());
  await update(() => reconnect.receive(ready('epoch-2')));
  assert.equal(ctx.value.typing, false);
  assert.equal(sentMessages().length, 1);
  assert.ok(ctx.value.messages.some(message => message.notice && message.content.includes('outcome')));
  assert.equal(reconnect.sent.filter(frame => frame.op !== 'negotiate').length, 0);
}));

test('an unsent request queued on reconnect belongs to the new epoch when ready', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.wire.close(1006));
  await update(() => tick(1000));
  const reconnect = sockets.at(-1);
  await update(() => reconnect.open());
  await update(() => ctx.value.sendMessage('new epoch request'));
  await update(() => reconnect.receive(ready('epoch-2')));
  const sent = sentMessages()[0];
  assert.equal(sent.epoch, 'epoch-2');
  assert.equal(ctx.value.typing, true);
  assert.equal(ctx.value.messages.some(message => message.notice && message.content.includes('outcome')), false);
  await update(() => reconnect.receive(event(sent.request_id, 1, { type: 'done', full_response: 'new epoch response' })));
  await update(() => reconnect.receive(event(sent.request_id, 2, { type: 'turn_terminal', outcome: 'done' })));
  assert.ok(ctx.value.messages.some(message => message.content === 'new epoch response'));
}));

test('a gap replay recovers the entire cached prefix before applying an early live terminal', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendMessage('replay the result'));
  const request = sentMessages()[0].request_id;
  const terminal = event(request, 131, { type: 'turn_terminal', outcome: 'done' });
  await update(() => ctx.wire.receive(terminal));
  assert.equal(ctx.value.typing, true, 'terminal waits for earlier events');
  const events = [
    event(request, 1, { type: 'tool_call', name: 'lookup', id: 'call-1', args: {} }),
    event(request, 2, { type: 'tool_result', name: 'lookup', id: 'call-1', output: 'recovered result' }),
    ...Array.from({ length: 126 }, (_, index) => event(request, index + 3, { type: 'chunk', content: 'x' })),
    terminal,
  ];
  await update(() => ctx.wire.receive({ type: 'turn_replay', request_id: request, turn_id: turnId, events, gap: true, terminal: true, last_seq: 131 }));
  assert.equal(ctx.value.messages.find(message => message.toolCall)?.toolCall.output, 'recovered result');
  assert.equal(ctx.value.streamingContent, 'x'.repeat(126));
  assert.equal(ctx.value.typing, false);
}));

test('an earlier replay gap cannot discard recoverable events beyond that snapshot', async () => withAgent(true, async ctx => {
  await update(() => ctx.wire.receive(ready()));
  await update(() => ctx.value.sendMessage('two replay batches'));
  const request = sentMessages()[0].request_id;
  await update(() => ctx.wire.receive({ type: 'turn_replay', request_id: request, turn_id: turnId,
    events: [event(request, 1, { type: 'thinking', content: 'trace' }), event(request, 3, { type: 'chunk', content: 'A' })],
    gap: true, last_seq: 3, terminal: false,
  }));
  const terminal = event(request, 6, { type: 'turn_terminal', outcome: 'done' });
  await update(() => ctx.wire.receive(terminal));
  assert.equal(ctx.value.typing, true, 'the old gap only covered seq <= 3');
  await update(() => tick(3000));
  assert.ok(ctx.wire.sent.some(frame => frame.op === 'resume' && frame.after_seq === 3));
  await update(() => ctx.wire.receive({ type: 'turn_replay', request_id: request, turn_id: turnId,
    events: [event(request, 4, { type: 'chunk', content: 'B' }), event(request, 5, { type: 'done' }), terminal],
    gap: true, last_seq: 6, terminal: true,
  }));
  assert.ok(ctx.value.messages.some(message => message.content === 'AB'));
  assert.equal(ctx.value.typing, false);
}));

after(() => {
  globalThis.setTimeout = realSetTimeout;
  globalThis.clearTimeout = realClearTimeout;
  dom.window.close();
});
