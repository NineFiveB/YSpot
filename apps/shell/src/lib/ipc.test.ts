// The row-building half of the §7.1 frecency contract.
//
// A launch records against `row.id`; the shell adds the ranking bonus under
// the id it derives on its own side. Nothing checks the two agree at
// runtime — if they drift, launches pile up under one key while the ranker
// reads another, and the only symptom is that a file you open every day
// never rises. So both sides pin the string.

import { describe, expect, it } from "vitest";
import { fallbackFileRow, fileRow, rowKey, type ResultItem } from "./ipc";

const item = (over: Partial<ResultItem> = {}): ResultItem => ({
  id: { volumeIdx: 2, frn: "9007199254740993" },
  path: "C:\notes\todo.md",
  name: "todo.md",
  score: 0.8,
  matchRanges: [],
  ...over,
});

describe("file row identity", () => {
  it("keys a service file on volumeIdx:frn", () => {
    // Matches JsResultId::frecency_id in pipe_client.rs. The FRN is past
    // 2^53, which is why it crosses the wire as a string.
    expect(rowKey({ volumeIdx: 2, frn: "9007199254740993" })).toBe(
      "2:9007199254740993",
    );
  });

  it("gives executeAction the same id the ranker scored", () => {
    const row = fileRow(item());
    expect(row.id).toBe(rowKey(item().id));
  });

  it("keys a fallback hit on its path, which is all that provider mints", () => {
    const row = fallbackFileRow({
      path: "C:\notes\todo.md",
      name: "todo.md",
      score: 0.5,
    });
    expect(row.id).toBe("C:\notes\todo.md");
  });

  it("takes the fallback score from the shell rather than inventing one", () => {
    // The shell adds the frecency bonus to its flat base before emitting, so
    // a hardcoded constant here would silently discard the ranking.
    const row = fallbackFileRow({ path: "C:\a.txt", name: "a.txt", score: 0.57 });
    expect(row.score).toBe(0.57);
  });
});
