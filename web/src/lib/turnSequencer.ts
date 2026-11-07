/**
 * Seq-ordered consumption of S2 turn events (#549 / #563 review).
 *
 * The live ring and a resume replay can interleave: live seq N+1 may arrive
 * before the replay carrying N. Applying events in ARRIVAL order (dedup only)
 * corrupts the stream — chunks render out of order ("CAB") and a `done` /
 * `tool_result` / terminal applied early clears buffers the missing chunks
 * still belong to. This module is the pure ordering core: it holds every
 * sequenced event until all lower seqs have been applied (or declared
 * unrecoverable) and releases them strictly in seq order.
 *
 * Gap policy (what happens to a hole that can never be filled):
 * - A hole at or below a replay's `last_seq` is unrecoverable: the replay
 *   returned everything the server still caches after our anchor, so a seq
 *   it did not carry is gone (cache bound / `gap`). `settleThrough(last_seq)`
 *   skips those holes and releases the buffered events above them in order.
 * - Any other hole is first given a chance to fill (a replay may be in
 *   flight); the caller arms a bounded grace timer and, on expiry, calls
 *   `flush()`, which skips every remaining hole and releases everything
 *   buffered in order. Skipped seqs are reported so the UI can show a gap
 *   notice; the final `done.full_response` (when present) remains the
 *   authoritative transcript text, but ordering never depends on it.
 * - The buffer is bounded (`maxBuffered`); overflowing it forces a flush so
 *   a permanently missing seq cannot grow memory without bound.
 *
 * Events without a numeric `seq` (legacy frames) bypass ordering entirely.
 */

export interface SequencedRelease<E> {
  /** Events now applicable, strictly ascending by seq. */
  ready: { seq: number; event: E }[];
  /** Seqs declared unrecoverable by this call (ascending). */
  skipped: number[];
}

export class TurnSequencer<E> {
  /** Next seq to apply; everything below it is applied or skipped. */
  private next = 1;
  private readonly pending = new Map<number, E>();

  constructor(private readonly maxBuffered = 512) {}

  /** Highest seq with every lower seq applied or skipped — the resume anchor. */
  get resumeSeq(): number {
    return this.next - 1;
  }

  /** True while at least one received event waits for a lower seq. */
  get hasHole(): boolean {
    return this.pending.size > 0;
  }

  get bufferedCount(): number {
    return this.pending.size;
  }

  /** Lowest missing seq while a hole exists, else null. */
  get firstMissing(): number | null {
    return this.pending.size > 0 ? this.next : null;
  }

  /** Offer one sequenced event; returns whatever became contiguous. */
  offer(seq: number, event: E): SequencedRelease<E> {
    if (!Number.isInteger(seq) || seq < this.next || this.pending.has(seq)) {
      return { ready: [], skipped: [] }; // duplicate / already applied
    }
    this.pending.set(seq, event);
    const ready = this.drain();
    if (this.pending.size > this.maxBuffered) {
      const forced = this.flush();
      return { ready: [...ready, ...forced.ready], skipped: forced.skipped };
    }
    return { ready, skipped: [] };
  }

  /**
   * Declare every seq `<= through` that has not arrived unrecoverable, then
   * release buffered events in order (including any contiguous run above
   * `through`). Used after a replay answer, whose `last_seq` bounds what the
   * server could still send for the missing range.
   */
  settleThrough(through: number): SequencedRelease<E> {
    const skipped: number[] = [];
    const ready: { seq: number; event: E }[] = [];
    while (this.next <= through) {
      const event = this.pending.get(this.next);
      if (event === undefined) {
        skipped.push(this.next);
      } else {
        this.pending.delete(this.next);
        ready.push({ seq: this.next, event });
      }
      this.next += 1;
    }
    ready.push(...this.drain());
    return { ready, skipped };
  }

  /** Skip every remaining hole and release all buffered events in order. */
  flush(): SequencedRelease<E> {
    if (this.pending.size === 0) return { ready: [], skipped: [] };
    const highest = Math.max(...this.pending.keys());
    return this.settleThrough(highest);
  }

  private drain(): { seq: number; event: E }[] {
    const ready: { seq: number; event: E }[] = [];
    for (;;) {
      const event = this.pending.get(this.next);
      if (event === undefined) break;
      this.pending.delete(this.next);
      ready.push({ seq: this.next, event });
      this.next += 1;
    }
    return ready;
  }
}
