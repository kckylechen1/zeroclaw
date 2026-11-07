import assert from 'node:assert/strict';
import { TurnSequencer, type SequencedRelease } from './turnSequencer';

type Ev = { type: 'chunk'; text: string } | { type: 'done' } | { type: 'tool_result' } | { type: 'terminal' };

/** Minimal stand-in for the AgentContext stream state: chunks append to the
 *  streaming buffer, `done` commits it as a message and clears the buffer. */
class Stream {
  buffer = '';
  messages: string[] = [];
  terminal = false;
  skipped: number[] = [];
  apply(release: SequencedRelease<Ev>) {
    this.skipped.push(...release.skipped);
    for (const { event } of release.ready) {
      assert.equal(this.terminal, false, 'nothing applies after the terminal');
      if (event.type === 'chunk') this.buffer += event.text;
      else if (event.type === 'done') {
        this.messages.push(this.buffer);
        this.buffer = '';
      } else if (event.type === 'terminal') this.terminal = true;
    }
  }
}

const chunk = (text: string): Ev => ({ type: 'chunk', text });

// 1. Live/replay interleave: live seq 3 arrives before the replay carrying
//    1..2. Arrival-order apply would render "CAB"; ordered apply renders ABC.
{
  const seq = new TurnSequencer<Ev>();
  const s = new Stream();
  s.apply(seq.offer(3, chunk('C'))); // live, ahead of replay
  assert.equal(s.buffer, '', 'held behind the hole');
  assert.equal(seq.hasHole, true);
  assert.equal(seq.resumeSeq, 0, 'resume anchor stays contiguous');
  // Replay batch (1, 2, and a re-delivered 3) then settle on last_seq.
  s.apply(seq.offer(1, chunk('A')));
  s.apply(seq.offer(2, chunk('B')));
  s.apply(seq.offer(3, chunk('C'))); // duplicate of the buffered live frame
  s.apply(seq.settleThrough(3));
  assert.equal(s.buffer, 'ABC');
  assert.equal(seq.hasHole, false);
  assert.equal(seq.resumeSeq, 3);
  assert.deepEqual(s.skipped, []);
  // Late duplicates of already-applied seqs are ignored.
  s.apply(seq.offer(2, chunk('B')));
  assert.equal(s.buffer, 'ABC');
}

// 2. done / tool_result / terminal before the chunks: they must not commit or
//    clear the stream before the earlier chunks are applied.
{
  const seq = new TurnSequencer<Ev>();
  const s = new Stream();
  s.apply(seq.offer(4, { type: 'done' }));
  s.apply(seq.offer(5, { type: 'terminal' }));
  s.apply(seq.offer(3, { type: 'tool_result' }));
  assert.deepEqual(s.messages, []);
  assert.equal(s.terminal, false, 'terminal held behind missing chunks');
  s.apply(seq.offer(2, chunk('lo')));
  s.apply(seq.offer(1, chunk('hel')));
  assert.deepEqual(s.messages, ['hello'], 'done committed the FULL ordered text');
  assert.equal(s.buffer, '');
  assert.equal(s.terminal, true);
  assert.equal(seq.resumeSeq, 5);
}

// 3a. Gap declared by a replay: seq 2 is beyond the server cache and never
//     comes. settleThrough(last_seq) skips it and releases the rest in order.
{
  const seq = new TurnSequencer<Ev>();
  const s = new Stream();
  s.apply(seq.offer(1, chunk('A')));
  s.apply(seq.offer(3, chunk('C')));
  s.apply(seq.offer(4, { type: 'terminal' }));
  assert.equal(s.buffer, 'A');
  s.apply(seq.settleThrough(4));
  assert.deepEqual(s.skipped, [2]);
  assert.equal(s.buffer, 'AC');
  assert.equal(s.terminal, true);
  assert.equal(seq.resumeSeq, 4);
}

// 3b. settleThrough only declares seqs <= last_seq lost; live events beyond
//     the replay snapshot keep their ordering contract.
{
  const seq = new TurnSequencer<Ev>();
  const s = new Stream();
  s.apply(seq.offer(5, chunk('E'))); // live, after the replay snapshot
  s.apply(seq.offer(1, chunk('A')));
  s.apply(seq.settleThrough(2)); // replay knew only up to 2; 2 is lost
  assert.deepEqual(s.skipped, [2]);
  assert.equal(s.buffer, 'A');
  assert.equal(seq.hasHole, true, '3 and 4 may still arrive live');
  s.apply(seq.offer(4, chunk('D')));
  s.apply(seq.offer(3, chunk('C')));
  assert.equal(s.buffer, 'ACDE');
}

// 3c. Grace expiry: flush() skips every hole and applies in order.
{
  const seq = new TurnSequencer<Ev>();
  const s = new Stream();
  s.apply(seq.offer(4, chunk('D')));
  s.apply(seq.offer(2, chunk('B')));
  s.apply(seq.flush());
  assert.deepEqual(s.skipped, [1, 3]);
  assert.equal(s.buffer, 'BD');
  assert.equal(seq.hasHole, false);
  assert.deepEqual(seq.flush(), { ready: [], skipped: [] });
  // A hole that later "fills" after being skipped is a stale duplicate.
  s.apply(seq.offer(3, chunk('C')));
  assert.equal(s.buffer, 'BD');
}

// 3d. Bounded buffer: overflow forces a flush instead of unbounded growth.
{
  const seq = new TurnSequencer<Ev>(2);
  const s = new Stream();
  s.apply(seq.offer(2, chunk('b')));
  s.apply(seq.offer(3, chunk('c')));
  assert.equal(s.buffer, '');
  s.apply(seq.offer(4, chunk('d')));
  assert.deepEqual(s.skipped, [1]);
  assert.equal(s.buffer, 'bcd');
}

// Invalid seqs are ignored.
{
  const seq = new TurnSequencer<Ev>();
  assert.deepEqual(seq.offer(0, chunk('x')), { ready: [], skipped: [] });
  assert.deepEqual(seq.offer(1.5, chunk('x')), { ready: [], skipped: [] });
}

console.log('turnSequencer logic tests passed');
