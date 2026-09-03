// The §5.11 merge and selection contract is normative, and its failure mode
// is silent: rows reorder under the user's cursor, or the selection quietly
// points at a different item than the one they arrowed to. Neither shows up
// in a build or a type check, so it gets tests.

import { describe, expect, it } from "vitest";
import type { Row } from "./ipc";
import { byScore, mergeRows, selectionIndex } from "./merge";

const app = (name: string, score: number): Row => ({
  kind: "app",
  key: `app:${name}`,
  id: name,
  name,
  subtitle: "Application",
  score,
  matchRanges: [],
  appKind: "win32",
});

const file = (name: string, score: number): Row => ({
  kind: "file",
  key: `file:0:${name}`,
  id: `0:${name}`,
  name,
  subtitle: `C:\\${name}`,
  score,
  matchRanges: [],
  path: `C:\\${name}`,
});

describe("mergeRows", () => {
  it("orders both sources by global score while nothing is frozen", () => {
    const rows = mergeRows({
      apps: [app("Code", 0.9), app("Codecs", 0.55)],
      files: [file("code.rs", 1.0), file("encode.rs", 0.55)],
      frozen: null,
    });
    expect(rows.map((r) => r.name)).toEqual([
      "code.rs", // 1.0
      "Code", // 0.9
      "Codecs", // 0.55, app wins the tie
      "encode.rs",
    ]);
  });

  it("keeps a frozen order and appends late arrivals below it (§5.11 r3)", () => {
    const apps = [app("Code", 0.9)];
    const first = mergeRows({ apps, files: [], frozen: null });
    expect(first.map((r) => r.name)).toEqual(["Code"]);
    // The user has moved the selection: the order on screen is frozen. A
    // higher-scoring file now arrives and MUST NOT be spliced above.
    const second = mergeRows({
      apps,
      files: [file("code.rs", 1.0)],
      frozen: first,
    });
    expect(second.map((r) => r.name)).toEqual(["Code", "code.rs"]);
  });

  it("takes the newest copy of a frozen row without moving it", () => {
    const frozen = [app("Code", 0.9), file("code.rs", 0.55)];
    const rows = mergeRows({
      apps: [app("Code", 0.99)], // a frecency bump landed
      files: [file("code.rs", 0.55)],
      frozen,
    });
    expect(rows.map((r) => r.name)).toEqual(["Code", "code.rs"]);
    expect(rows[0].score).toBe(0.99);
  });

  it("drops nothing when a source is empty", () => {
    expect(mergeRows({ apps: [], files: [], frozen: null })).toEqual([]);
    expect(
      mergeRows({ apps: [app("A", 1)], files: [], frozen: null }).map((r) => r.name),
    ).toEqual(["A"]);
    expect(
      mergeRows({ apps: [], files: [file("b", 1)], frozen: null }).map((r) => r.name),
    ).toEqual(["b"]);
  });
});

describe("selectionIndex", () => {
  const rows = [app("Code", 0.9), file("code.rs", 0.55)];

  it("sits on row 0 when nothing is stuck", () => {
    expect(selectionIndex(rows, null)).toBe(0);
  });

  it("follows the stuck row wherever it landed", () => {
    expect(selectionIndex(rows, "file:0:code.rs")).toBe(1);
  });

  it("falls back to row 0 when the stuck row is gone", () => {
    expect(selectionIndex(rows, "app:Vanished")).toBe(0);
  });
});

describe("byScore with shell-side rows", () => {
  const calc = (): Row => ({
    kind: "calc",
    key: "calc:result",
    id: "calc",
    name: "42",
    subtitle: "Calculator",
    score: 2,
    matchRanges: [],
    value: "42",
  });
  const setting = (name: string, score: number): Row => ({
    kind: "setting",
    key: `setting:${name}`,
    id: name,
    name,
    subtitle: "Settings",
    score,
    matchRanges: [],
  });

  it("puts the calculator answer first (§7.7)", () => {
    const rows = mergeRows({
      apps: [calc(), app("Display Driver", 1.0)],
      files: [file("display.txt", 1.0)],
      frozen: null,
    });
    expect(rows[0].kind).toBe("calc");
  });

  it("breaks an exact tie for the shell side over a file", () => {
    const rows = mergeRows({
      apps: [setting("Display", 0.9)],
      files: [file("Display", 0.9)],
      frozen: null,
    });
    expect(rows[0].kind).toBe("setting");
  });
});

describe("byScore", () => {
  it("is a total order: score, then app-before-file, then name", () => {
    expect(byScore(app("A", 1), file("b", 0.5))).toBeLessThan(0);
    expect(byScore(file("b", 0.5), app("A", 1))).toBeGreaterThan(0);
    expect(byScore(app("A", 0.5), file("b", 0.5))).toBeLessThan(0);
    expect(byScore(app("A", 0.5), app("B", 0.5))).toBeLessThan(0);
    expect(byScore(app("A", 0.5), app("A", 0.5))).toBe(0);
  });
});
