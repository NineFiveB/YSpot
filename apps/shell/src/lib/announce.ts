// What a screen reader is told when a generation settles (SPEC.md §5.12).
//
// The announcement is deliberately about the *settled* result set, not every
// batch: §5.12 asks for debouncing to the settled set, and a reader that
// says "1 result… 8 results… 23 results" during one keystroke is worse than
// one that says nothing.
//
// Kept separate from the component so the wording is testable, because
// wording is the whole feature here.

import type { Row } from "./ipc";

/**
 * The polite-live-region text for a settled generation.
 *
 * Empty string means "say nothing" — an empty query is not a result of no
 * results, it is the absence of a question.
 */
export function announceText(query: string, rows: Row[]): string {
  if (query.trim() === "") return "";
  if (rows.length === 0) return "No results";
  const top = rows[0];
  const count = rows.length === 1 ? "1 result" : `${rows.length} results`;
  // Naming the top row is what makes the announcement useful: the selection
  // starts there, so a reader hears what pressing Enter would do.
  return `${count}. ${describeRow(top)}`;
}

/** One row, as a sentence a screen reader can read out. */
export function describeRow(row: Row): string {
  switch (row.kind) {
    case "calc":
      return `Calculator result ${row.name}`;
    case "app":
      return `${row.name}, application`;
    case "setting":
      return `${row.name}, ${row.subtitle}`;
    case "command":
      return `${row.name}, YSpot command`;
    case "window":
      return `${row.name}, open window`;
    case "file":
      return `${row.name}, file at ${row.subtitle}`;
  }
}
