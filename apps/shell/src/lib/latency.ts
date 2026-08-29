// M0 latency measurement instrument (SPEC.md §5.10, §2.5): keydown → results
// applied (post-rAF). Ring buffer of the last 100 samples feeds the HUD.

export interface LatencySnapshot {
  last: number | null;
  p50: number | null;
  p95: number | null;
  samples: number;
}

const RING_SIZE = 100;
const MAX_PENDING = 256;

/** gen → keydown timestamp (performance.now()). */
const pending = new Map<number, number>();
const ring: number[] = [];
let ringNext = 0;
let last: number | null = null;

/** Record the keydown timestamp for a generation. */
export function markKeydown(gen: number, t0: number = performance.now()): void {
  pending.set(gen, t0);
  if (pending.size > MAX_PENDING) {
    // Drop the oldest entry (Map preserves insertion order).
    const oldest = pending.keys().next().value;
    if (oldest !== undefined) pending.delete(oldest);
  }
}

/**
 * Record that `gen`'s results were applied. Returns the keydown→applied delta
 * in ms, or null when the generation was already consumed (later batches of
 * the same generation) or superseded. Entries at or below `gen` are pruned —
 * stale generations are dropped, never rendered, so they never complete.
 */
export function markApplied(
  gen: number,
  tApplied: number = performance.now(),
): number | null {
  const t0 = pending.get(gen);
  for (const key of pending.keys()) {
    if (key <= gen) pending.delete(key);
  }
  if (t0 === undefined) return null;
  const delta = tApplied - t0;
  last = delta;
  if (ring.length < RING_SIZE) {
    ring.push(delta);
  } else {
    ring[ringNext] = delta;
  }
  ringNext = (ringNext + 1) % RING_SIZE;
  return delta;
}

function percentile(sorted: number[], q: number): number {
  const idx = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil(q * sorted.length) - 1),
  );
  return sorted[idx];
}

export function statsSnapshot(): LatencySnapshot {
  if (ring.length === 0) {
    return { last, p50: null, p95: null, samples: 0 };
  }
  const sorted = [...ring].sort((a, b) => a - b);
  return {
    last,
    p50: percentile(sorted, 0.5),
    p95: percentile(sorted, 0.95),
    samples: sorted.length,
  };
}
