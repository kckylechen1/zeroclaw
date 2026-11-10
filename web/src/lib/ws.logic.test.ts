import assert from 'node:assert/strict';
const values = new Map([['zeroclaw_session_id.web', 'shared']]);
const urls: string[] = [];
type SentFrame = Record<string, unknown>;
const sent: SentFrame[] = [];
let socket: { readyState: number; onopen: (() => void) | null; onmessage: ((ev: { data: string }) => void) | null; send: (data: string) => void; close: () => void } | null = null;
class Socket {
  static OPEN = 1;
  readyState = 0;
  onopen: (() => void) | null = null;
  onmessage: ((ev: { data: string }) => void) | null = null;
  constructor(url: string) {
    urls.push(url);
    socket = this as never;
  }
  send(data: string) {
    sent.push(JSON.parse(data));
  }
  close() {}
}
Object.assign(globalThis, {
  window: { location: { protocol: 'https:', host: 'gateway.invalid' }, __ZEROCLAW_BASE__: '/controller' },
  localStorage: { getItem: (key: string) => values.get(key) ?? null, setItem: (key: string, value: string) => values.set(key, value) },
  WebSocket: Socket,
});
const { WebSocketClient } = await import('./ws');
const client = new WebSocketClient({ agentAlias: 'web', autoReconnect: false });
client.connect();
client.disconnect();
client.connect();
client.disconnect();
assert.equal(urls.length, 2);
for (const url of urls) {
  const parsed = new URL(url);
  assert.equal(parsed.protocol, 'wss:');
  assert.equal(parsed.pathname, '/controller/ws/chat');
  assert.equal(parsed.searchParams.get('surface'), 'web');
  assert.equal(parsed.searchParams.get('agent'), 'web');
  assert.equal(parsed.searchParams.get('session_id'), 'shared');
}
console.log('web chat advertises its surface on each attachment');

// ---------------------------------------------------------------------------
// S2 (#549): negotiation frames, pre-negotiation queueing, epoch-bound flush,
// and scoped cancel/resume framing.
// ---------------------------------------------------------------------------
values.set('zeroclaw_token', 'synthetic-owner');
const s2 = new WebSocketClient({ agentAlias: 'web', autoReconnect: false });
s2.onMessage = () => {
  // no-op: the assertions below inspect the transport's outbound frames.
};
s2.connect();
const wire = socket!;
assert.equal(wire.readyState, 0);
// The socket becoming OPEN triggers negotiation before anything else.
wire.readyState = 1;
wire.onopen?.();
assert.equal(sent.length, 1, 'negotiation is the first frame on open');
assert.deepEqual(sent[0], { type: 'turn_control', version: 1, op: 'negotiate', capabilities: ['steer'] });
// sendTurnMessage before turn_ready must queue, not send.
const queuedId = s2.sendTurnMessage('queued hello');
assert.equal(s2.epoch, null);
assert.equal(sent.length, 1, 'only negotiate is on the wire pre-ready');
assert.equal(s2.sendSideQuery('not negotiated yet'), null, 'side queries never bypass S2 readiness');
// turn_ready flushes the queue with the negotiated epoch.
wire.onmessage?.({ data: JSON.stringify({ type: 'turn_ready', version: 1, epoch: 'epoch-1' }) });
assert.equal(s2.epoch, 'epoch-1');
assert.equal(sent.length, 2, 'queued message flushed on ready');
const flushed = sent[1]!;
assert.equal(flushed.type, 'turn_control');
assert.equal(flushed.op, 'message');
assert.equal(flushed.epoch, 'epoch-1');
assert.equal(flushed.request_id, queuedId);
assert.equal(flushed.content, 'queued hello');
assert.equal('attachments' in flushed, false, 'no attachments key for plain text');
// Post-negotiation sends go straight out with the epoch.
const liveId = s2.sendTurnMessage('with file', ['h1', 'h2']);
const live = sent[2]!;
assert.equal(live.epoch, 'epoch-1');
assert.equal(live.request_id, liveId);
assert.deepEqual(live.attachments, ['h1', 'h2']);
// Scoped cancel reuses the MESSAGE's request id: the server journals and
// looks the control up under the original admission's identity, so a fresh
// id would always resolve to `unknown` and never cancel anything.
assert.equal(s2.cancelTurn(liveId, 'turn-abc'), true);
const cancel = sent[3]!;
assert.deepEqual(cancel, {
  type: 'turn_control',
  version: 1,
  op: 'cancel',
  epoch: 'epoch-1',
  request_id: liveId,
  turn_id: 'turn-abc',
});
// Resume addresses an admitted request after a sequence number.
s2.resumeTurn(liveId, 7);
assert.deepEqual(
  { ...sent[4]!, request_id: '' },
  { type: 'turn_control', version: 1, op: 'resume', epoch: 'epoch-1', request_id: '', after_seq: 7 },
);
console.log('s2 transport negotiates, queues until ready, and frames scoped controls');

// Dedicated read-only frames remain valid after S2 negotiation. Using an S2
// message with the side-query id would fail the gateway's UUID check, and its
// message-admission errors do not echo request_id for safe UI correlation.
const sideId = s2.sendSideQuery('question while a main turn is active');
assert.ok(sideId);
assert.ok(sideId.startsWith('btw-'));
assert.deepEqual(sent[sent.length - 1], {
  type: 'btw', id: sideId, content: '/btw question while a main turn is active',
});
assert.equal(s2.isMessageQueued(sideId), false, 'side queries have no admission or replay queue');
assert.equal(s2.epoch, 'epoch-1', 'the main turn keeps its negotiated S2 contract');
console.log('s2 attachments use the dedicated correlated side-query frame');
