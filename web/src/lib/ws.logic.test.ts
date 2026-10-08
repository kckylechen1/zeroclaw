import assert from 'node:assert/strict';
const values = new Map([['zeroclaw_session_id.web', 'shared']]);
const urls: string[] = [];
class Socket {
  static OPEN = 1;
  readyState = 0;
  constructor(url: string) { urls.push(url); }
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
