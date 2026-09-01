// Hidden-WebView2 render-throttling instrument — one of the two §10 M0
// flagged open items ("hidden-WebView2 render throttling" smoke test).
//
// The question it answers: while the launcher window is hidden, does the
// WebView2 compositor keep servicing requestAnimationFrame, throttle it, or
// stop it entirely — and how long after re-show does the first frame take?
// A permanent one-callback-per-frame rAF ticker observes exactly that: rAF is
// throttled BY the compositor, so requesting frames does not prevent the
// behavior being measured, it samples it.
//
// Findings are reported through the shell's M0 ETW marker (`rafgap …`) on the
// show that follows a hidden interval, where the harness reads them alongside
// its own hotkey→visible timings.

import { m0Mark } from "./ipc";

let hiddenAt: number | null = null;
let hiddenTicks = 0;
let maxGapMs = 0;
let lastTick: number | null = null;
/** Set on show; the next tick reports first_frame_ms and clears it. */
let shownAt: number | null = null;
let pendingReport: string | null = null;

function tick(now: number): void {
  if (lastTick !== null && hiddenAt !== null) {
    hiddenTicks += 1;
    maxGapMs = Math.max(maxGapMs, now - lastTick);
  }
  if (shownAt !== null && pendingReport !== null) {
    const firstFrame = (now - shownAt).toFixed(1);
    m0Mark(`${pendingReport} first_frame_ms=${firstFrame}`);
    shownAt = null;
    pendingReport = null;
  }
  lastTick = now;
  requestAnimationFrame(tick);
}

let started = false;

/** Start the ticker. Idempotent: React StrictMode double-invokes effects in
 * dev, and two ticker chains would double every `ticks=` count. */
export function startThrottleProbe(): void {
  if (started) return;
  started = true;
  requestAnimationFrame(tick);
}

/** The shell hid the window (window:hidden event). */
export function noteHidden(): void {
  hiddenAt = performance.now();
  hiddenTicks = 0;
  maxGapMs = 0;
  // A report still pending from the previous show (compositor stalled before
  // its first frame) is stale the moment a new hidden interval starts —
  // emitting it later would carry a wildly inflated first_frame_ms.
  shownAt = null;
  pendingReport = null;
}

/** The shell showed the window (window:shown event). */
export function noteShown(): void {
  const now = performance.now();
  if (hiddenAt === null) return;
  const hiddenMs = now - hiddenAt;
  // The final gap of the hidden interval may still be open (no tick between
  // hide and show at all); fold it in so a fully-stopped compositor reports
  // its true stall rather than 0.
  const tailGap = lastTick !== null ? now - lastTick : hiddenMs;
  const gap = Math.max(maxGapMs, tailGap);
  hiddenAt = null;
  shownAt = now;
  pendingReport =
    `rafgap hidden_ms=${hiddenMs.toFixed(1)} ticks=${hiddenTicks} ` +
    `max_gap_ms=${gap.toFixed(1)}`;
}
