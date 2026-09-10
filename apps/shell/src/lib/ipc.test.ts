// The row-building half of the §7.1 frecency contract.
//
// A launch records against `row.id`; the shell adds the ranking bonus under
// the id it derives on its own side. Nothing checks the two agree at
// runtime — if they drift, launches pile up under one key while the ranker
// reads another, and the only symptom is that a file you open every day
// never rises. So both sides pin the string.

import { describe, expect, it } from "vitest";
import {
  DEPRIORITIZED_FACTOR,
  fallbackFileRow,
  fileRow,
  nextGen,
  pathWeight,
  peekNextGen,
  rowKey,
  type ResultItem,
} from "./ipc";

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

describe("the shared generation counter", () => {
  it("only goes up, and peek predicts the next value exactly", () => {
    // Two consumers issue generations — the root list and File Search — and
    // the pipe's stale-drop watermark assumes they only increase. The M0
    // self-measurement marks a keydown against the generation it is ABOUT to
    // start, so peek has to be exact, not approximate.
    const a = nextGen();
    expect(peekNextGen()).toBe(a + 1);
    const b = nextGen();
    expect(b).toBe(a + 1);
    expect(peekNextGen()).toBe(b + 1);
  });
});

describe("pathWeight", () => {
  // Built from a character code so no escape sequence can be mangled by
  // whatever writes this file, and so the separator being a backslash is
  // part of what is under test rather than an accident of quoting.
  const BS = String.fromCharCode(92);
  const win = (...parts: string[]): string => parts.join(BS);

  // The measured case: `password` put three WinSxS files above the iCloud
  // Passwords app. A prefix hit at depth 3 scores 0.9 * 0.943 = 0.849; the
  // app matches at word-start for a flat 0.800.
  it("demotes the component store below an app matching at word-start", () => {
    const p = win("C:", "Windows", "WinSxS", "amd64_x_none_8c3c", "PasswordEnrollmentManager.dll");
    expect(0.849 * pathWeight(p)).toBeLessThan(0.8);
  });

  it("leaves ordinary locations alone", () => {
    for (const p of [
      win("C:", "Users", "me", "Documents", "passwords.txt"),
      win("C:", "Windows", "System32", "kernel32.dll"),
      win("C:", "Program Files", "App", "app.exe"),
      win("D:", "projects", "src", "main.rs"),
    ]) {
      expect(pathWeight(p), p).toBe(1);
    }
  });

  it("covers every deprioritized root, case-insensitively and on any volume", () => {
    for (const p of [
      win("C:", "Windows", "WinSxS", "x", "y.dll"),
      win("c:", "windows", "winsxs", "x", "y.dll"),
      win("D:", "Windows", "WinSxS", "x", "y.dll"),
      win("C:", "Windows", "servicing", "x.dll"),
      win("C:", "Windows", "assembly", "GAC", "x.dll"),
      win("C:", "$Recycle.Bin", "S-1-5-21", "x.txt"),
      win("C:", "System Volume Information", "x.log"),
    ]) {
      expect(pathWeight(p), p).toBe(DEPRIORITIZED_FACTOR);
    }
  });

  // A folder the user happens to name WinSxS is theirs, not the component
  // store, and must not be demoted for sharing a name.
  it("only matches a real Windows root, not a lookalike deeper in the tree", () => {
    expect(pathWeight(win("C:", "Users", "me", "code", "windows", "winsxs", "notes.md"))).toBe(1);
    expect(pathWeight(win("C:", "Users", "me", "WinSxS", "notes.md"))).toBe(1);
  });

  // The promise is that the index is complete, so this demotes and never
  // excludes: a WinSxS file is still findable when nothing else matches.
  it("never zeroes a score", () => {
    expect(DEPRIORITIZED_FACTOR).toBeGreaterThan(0);
  });
});
