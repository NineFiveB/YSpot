// Cross-source merge and selection (SPEC.md §5.11), kept out of the component
// so it can be reasoned about — and tested — on its own.
//
// One keystroke is one generation, and its rows arrive from two sources at
// different times: the shell's app catalog in ~1 ms, the service's file batch
// at ~10 ms. §5.11's three rules:
//
//   1. Service batches are append-only within a generation (they are already
//      globally rank-descending, so a later batch never splices above an
//      earlier one).
//   2. Merge never moves the selection. Selection is sticky to its stable ID
//      and resets to row 0 only on generation change.
//   3. Late results stay out of the way — they never reorder what the user is
//      looking at.
//
// Rules 2 and 3 read together decide what happens when a late source outranks
// what is on screen. Until the user has moved the selection, the list is a
// plain global-score merge and the selection sits on the best row — that is
// the ordering the ranker exists to produce, and nothing the user is aiming at
// can move, because they have not aimed yet. Once the user HAS moved the
// selection, the displayed order is frozen and later arrivals append below it,
// which is rule 3 stated literally.

import type { Row } from "./ipc";

/**
 * Global-score order. On an exact tie a shell-side row (calculator, app,
 * settings page) leads a file: the shell knows what those rows ARE, while a
 * file that merely scores the same is a weaker claim on the top slot.
 */
export function byScore(a: Row, b: Row): number {
  if (b.score !== a.score) return b.score - a.score;
  const shellSide = (r: Row): number => (r.kind === "file" ? 1 : 0);
  if (shellSide(a) !== shellSide(b)) return shellSide(a) - shellSide(b);
  return a.name.localeCompare(b.name);
}

export interface MergeInput {
  /** Everything this generation has produced, per source. */
  apps: Row[];
  files: Row[];
  /** The order currently on screen, if the user has moved the selection. */
  frozen: Row[] | null;
}

/**
 * The display list for a generation.
 *
 * With no frozen order this is a global-score merge. With one, the frozen
 * rows keep their positions and anything new is appended in score order
 * (§5.11 rule 3).
 */
export function mergeRows({ apps, files, frozen }: MergeInput): Row[] {
  const all = [...apps, ...files].sort(byScore);
  if (!frozen || frozen.length === 0) return all;
  const seen = new Set(frozen.map((r) => r.key));
  const kept: Row[] = [];
  const byKey = new Map(all.map((r) => [r.key, r]));
  for (const row of frozen) {
    // Keep the frozen position, but take the newest copy of the row (a later
    // batch may carry a better score for the same id).
    kept.push(byKey.get(row.key) ?? row);
  }
  for (const row of all) {
    if (!seen.has(row.key)) kept.push(row);
  }
  return kept;
}

/**
 * Where the selection lands after a merge: the index of `selectedKey`, or 0
 * when nothing is stuck (a fresh generation) or the stuck row is gone.
 */
export function selectionIndex(rows: Row[], selectedKey: string | null): number {
  if (selectedKey === null) return 0;
  const i = rows.findIndex((r) => r.key === selectedKey);
  return i >= 0 ? i : 0;
}
