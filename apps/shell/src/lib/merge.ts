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
  /** The row the selection is stuck to, if any — never trimmed by a cap. */
  selectedKey?: string | null;
}

/**
 * Root-list display budget, per kind (§5.11 r4).
 *
 * Every source contributes its quota unconditionally — one calculator answer,
 * eight apps, four settings pages, three commands, five windows and five files
 * — twenty-six rows into a viewport that shows between eight and nine. So a
 * query that legitimately matches five kinds pushed the answer the user
 * actually typed for off the visible page.
 *
 * The per-source caps upstream are unchanged and are now candidate pools.
 * This is the display budget, applied AFTER the global sort, so the survivors
 * are the best of each kind and the relative order is untouched.
 *
 * The numbers: `calc` one, because there is only ever one answer. `command`
 * two, because there are five built-ins and no query reaches three of them
 * meaningfully. `setting` and `app` three, because a fourth was never the
 * answer when the first three were not. `window` two, because a matching
 * window is a shortcut rather than a search result. `file` three, tightening
 * §7.3's cap, with the File Search view holding the full list. Fourteen rows
 * worst case, down from twenty-six.
 */
export const KIND_CAPS: Record<Row["kind"], number> = {
  calc: 1,
  command: 2,
  setting: 3,
  app: 3,
  window: 2,
  file: 3,
};

/**
 * Trim to the budget, keeping input order.
 *
 * Exempt rows are always kept and still COUNT against their kind, so the
 * budget stays a real bound. Nothing a cap does may evict the row the user is
 * standing on: if it did, `selectionIndex` would silently fall back to row 0
 * and Enter would run something the user never selected (§5.11 r2).
 */
function capByKind(rows: Row[], exempt: ReadonlySet<string>): Row[] {
  const used = new Map<Row["kind"], number>();
  const out: Row[] = [];
  for (const r of rows) {
    const n = (used.get(r.kind) ?? 0) + 1;
    used.set(r.kind, n);
    if (n > KIND_CAPS[r.kind] && !exempt.has(r.key)) continue;
    out.push(r);
  }
  return out;
}

function exemptSet(frozen: Row[] | null, selectedKey: string | null): ReadonlySet<string> {
  const s = new Set<string>(frozen ? frozen.map((r) => r.key) : []);
  if (selectedKey !== null) s.add(selectedKey);
  return s;
}

/**
 * Collapse same-named file rows to the best-scoring one (§7.3).
 *
 * The root list is for scanning. On the author's machine 101 directories
 * under one user profile are named exactly `Settings`; they are one answer,
 * not five rows.
 *
 * Root list ONLY. The File Search view must never call this: five files named
 * `main.rs` in five projects are five different answers, and that view is
 * where the full list lives.
 */
export function collapseSameName(files: Row[], cap: number): Row[] {
  const best = new Map<string, Row>();
  for (const f of files) {
    const k = f.name.toLowerCase();
    const cur = best.get(k);
    if (!cur || f.score > cur.score) best.set(k, f);
  }
  return [...best.values()].sort(byScore).slice(0, cap);
}

/**
 * The display list for a generation.
 *
 * With no frozen order this is a global-score merge. With one, the frozen
 * rows keep their positions and anything new is appended in score order
 * (§5.11 rule 3).
 */
export function mergeRows({ apps, files, frozen, selectedKey = null }: MergeInput): Row[] {
  const all = [...apps, ...files].sort(byScore);
  if (!frozen || frozen.length === 0) {
    return capByKind(all, exemptSet(null, selectedKey));
  }
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
  return capByKind(kept, exemptSet(frozen, selectedKey));
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
