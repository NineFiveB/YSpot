// §5.12's live region is normative, and its failure mode is silent: a
// screen-reader user hears nothing, and nobody looking at the screen can
// tell. So the wording gets tests.

import { describe, expect, it } from "vitest";
import { announceText, describeRow } from "./announce";
import type { Row } from "./ipc";

const app = (name: string): Row => ({
  kind: "app",
  key: `app:${name}`,
  id: name,
  name,
  subtitle: "Application",
  score: 1,
  matchRanges: [],
  appKind: "win32",
});

const file = (name: string, path: string): Row => ({
  kind: "file",
  key: `file:${path}`,
  id: path,
  name,
  subtitle: path,
  score: 0.9,
  matchRanges: [],
  path,
});

describe("announceText", () => {
  it("says nothing when there is no question", () => {
    expect(announceText("", [])).toBe("");
    expect(announceText("   ", [app("Notepad")])).toBe("");
  });

  it("reports an empty result set rather than staying silent", () => {
    expect(announceText("zzqxjv", [])).toBe("No results");
  });

  it("counts, and names the row the selection starts on", () => {
    expect(announceText("note", [app("Notepad")])).toBe("1 result. Notepad, application");
    expect(announceText("note", [app("Notepad"), app("Notes")])).toBe(
      "2 results. Notepad, application",
    );
  });

  it("pluralizes", () => {
    expect(announceText("a", [app("A")])).toContain("1 result.");
    expect(announceText("a", [app("A"), app("B")])).toContain("2 results.");
  });
});

describe("describeRow", () => {
  it("says what kind of thing every row is", () => {
    expect(describeRow(app("Notepad"))).toBe("Notepad, application");
    expect(describeRow(file("notes.txt", "C:\\notes.txt"))).toBe(
      "notes.txt, file at C:\\notes.txt",
    );
    expect(
      describeRow({
        kind: "calc",
        key: "calc:result",
        id: "calc",
        name: "42",
        subtitle: "Calculator",
        score: 2,
        matchRanges: [],
        value: "42",
      }),
    ).toBe("Calculator result 42");
    expect(
      describeRow({
        kind: "window",
        key: "window:1",
        id: "1",
        name: "Inbox",
        subtitle: "Window — outlook.exe",
        score: 1,
        matchRanges: [],
      }),
    ).toBe("Inbox, open window");
    expect(
      describeRow({
        kind: "setting",
        key: "setting:x",
        id: "x",
        name: "Display",
        subtitle: "Settings",
        score: 1,
        matchRanges: [],
      }),
    ).toBe("Display, Settings");
    expect(
      describeRow({
        kind: "command",
        key: "command:y",
        id: "y",
        name: "YSpot Settings",
        subtitle: "YSpot",
        score: 1,
        matchRanges: [],
        order: 0,
      }),
    ).toBe("YSpot Settings, YSpot command");
  });

  it("never returns an empty description for any row kind", () => {
    const rows: Row[] = [
      app("A"),
      file("b", "C:\\b"),
      { kind: "calc", key: "c", id: "c", name: "1", subtitle: "", score: 2, matchRanges: [], value: "1" },
      { kind: "setting", key: "s", id: "s", name: "S", subtitle: "Settings", score: 1, matchRanges: [] },
      { kind: "command", key: "m", id: "m", name: "M", subtitle: "YSpot", score: 1, matchRanges: [], order: 0 },
      { kind: "window", key: "w", id: "w", name: "W", subtitle: "Window", score: 1, matchRanges: [] },
    ];
    for (const r of rows) {
      expect(describeRow(r).length).toBeGreaterThan(0);
    }
  });
});
