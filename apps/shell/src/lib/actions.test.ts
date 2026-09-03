// §5.7 makes "every action MUST be keyboard-reachable" a hard requirement,
// and §7.1 restricts one of them to Win32 apps. Both are the kind of rule a
// later refactor breaks silently, so they get tests.

import { describe, expect, it } from "vitest";
import { actionsFor, filterActions, shortcutAction } from "./actions";
import type { Row } from "./ipc";

const appRow = (appKind: "packaged" | "win32"): Row => ({
  kind: "app",
  key: "app:X",
  id: "X",
  name: "Example",
  subtitle: "Application",
  score: 1,
  matchRanges: [],
  appKind,
});

const fileRow = (): Row => ({
  kind: "file",
  key: "file:0:1",
  id: "0:1",
  name: "notes.txt",
  subtitle: "C:\\notes.txt",
  score: 1,
  matchRanges: [],
  path: "C:\\notes.txt",
});

const settingRow = (): Row => ({
  kind: "setting",
  key: "setting:ms-settings:display",
  id: "ms-settings:display",
  name: "Display",
  subtitle: "Settings",
  score: 1,
  matchRanges: [],
});

const calcRow = (): Row => ({
  kind: "calc",
  key: "calc:result",
  id: "calc",
  name: "42",
  subtitle: "Calculator — Enter copies",
  score: 2,
  matchRanges: [],
  value: "42",
});

const windowRow = (): Row => ({
  kind: "window",
  key: "window:12345",
  id: "12345",
  name: "Inbox — Outlook",
  subtitle: "Window — outlook.exe",
  score: 0.9,
  matchRanges: [],
});

const chord = (key: string, mods: Partial<{ ctrlKey: boolean; shiftKey: boolean; altKey: boolean }> = {}) => ({
  key,
  ctrlKey: false,
  shiftKey: false,
  altKey: false,
  ...mods,
});

describe("actionsFor", () => {
  it("puts the primary action first for every row kind", () => {
    expect(actionsFor(fileRow())[0].id).toBe("open");
    expect(actionsFor(appRow("win32"))[0].id).toBe("open");
    expect(actionsFor(appRow("packaged"))[0].id).toBe("open");
  });

  it("offers Run as Administrator for Win32 apps only (§7.1)", () => {
    const win32 = actionsFor(appRow("win32")).map((a) => a.id);
    const packaged = actionsFor(appRow("packaged")).map((a) => a.id);
    expect(win32).toContain("runas");
    expect(packaged).not.toContain("runas");
  });

  it("gives the calculator a copy action and settings an open action", () => {
    // §7.7: Enter copies the answer — there is nothing to open.
    expect(actionsFor(calcRow()).map((a) => a.id)).toEqual(["copy"]);
    expect(actionsFor(settingRow()).map((a) => a.id)).toEqual(["open"]);
    // Neither offers a destructive verb.
    expect(actionsFor(calcRow()).some((a) => a.destructive)).toBe(false);
    expect(actionsFor(settingRow()).some((a) => a.destructive)).toBe(false);
  });

  it("gives a window the §7.5 verbs, switching first and close destructive", () => {
    const ids = actionsFor(windowRow()).map((a) => a.id);
    expect(ids[0]).toBe("open");
    // Every layout preset §7.5 names is offered.
    for (const preset of [
      "left_half",
      "right_half",
      "top_half",
      "bottom_half",
      "maximize",
      "center",
      "left_third",
      "center_third",
      "right_third",
      "left_two_thirds",
      "right_two_thirds",
      "top_left",
      "top_right",
      "bottom_left",
      "bottom_right",
    ]) {
      expect(ids).toContain(preset);
    }
    expect(ids).toContain("topmost");
    expect(ids).toContain("minimize");
    // Closing a window is the one destructive verb here.
    const destructive = actionsFor(windowRow()).filter((a) => a.destructive);
    expect(destructive.map((a) => a.id)).toEqual(["close"]);
  });

  it("covers every §7.3 file verb", () => {
    const ids = actionsFor(fileRow()).map((a) => a.id);
    expect(ids).toEqual([
      "open",
      "reveal",
      "copy_path",
      "copy_file",
      "open_with",
      "delete",
    ]);
  });

  it("marks only the destructive action destructive", () => {
    const destructive = actionsFor(fileRow()).filter((a) => a.destructive);
    expect(destructive.map((a) => a.id)).toEqual(["delete"]);
  });
});

describe("shortcutAction", () => {
  it("maps the file chords", () => {
    const f = fileRow();
    expect(shortcutAction(f, chord("c", { ctrlKey: true }))).toBe("copy_file");
    expect(shortcutAction(f, chord("C", { ctrlKey: true, shiftKey: true }))).toBe("copy_path");
    expect(shortcutAction(f, chord("e", { ctrlKey: true, shiftKey: true }))).toBe("reveal");
  });

  it("maps Ctrl+Enter to runas for Win32 apps and nothing for packaged ones", () => {
    expect(shortcutAction(appRow("win32"), chord("Enter", { ctrlKey: true }))).toBe("runas");
    expect(shortcutAction(appRow("packaged"), chord("Enter", { ctrlKey: true }))).toBeNull();
  });

  it("ignores chords without Ctrl, and anything with Alt", () => {
    const f = fileRow();
    expect(shortcutAction(f, chord("c"))).toBeNull();
    expect(shortcutAction(f, chord("c", { ctrlKey: true, altKey: true }))).toBeNull();
    expect(shortcutAction(f, chord("q", { ctrlKey: true }))).toBeNull();
  });

  it("never maps a file chord to an app action or vice versa", () => {
    expect(shortcutAction(appRow("win32"), chord("c", { ctrlKey: true }))).toBeNull();
    expect(shortcutAction(fileRow(), chord("Enter", { ctrlKey: true }))).toBeNull();
  });

  it("gives calculator, settings and window rows no chords at all", () => {
    for (const row of [calcRow(), settingRow(), windowRow()]) {
      for (const key of ["c", "e", "Enter"]) {
        expect(shortcutAction(row, chord(key, { ctrlKey: true }))).toBeNull();
        expect(shortcutAction(row, chord(key, { ctrlKey: true, shiftKey: true }))).toBeNull();
      }
    }
  });

  it("only names actions the row actually offers", () => {
    for (const row of [
      appRow("win32"),
      appRow("packaged"),
      fileRow(),
      settingRow(),
      calcRow(),
      windowRow(),
    ]) {
      const offered = new Set(actionsFor(row).map((a) => a.id));
      for (const key of ["c", "C", "e", "E", "Enter", "q"]) {
        for (const mods of [{ ctrlKey: true }, { ctrlKey: true, shiftKey: true }]) {
          const id = shortcutAction(row, chord(key, mods));
          if (id !== null) expect(offered).toContain(id);
        }
      }
    }
  });
});

describe("filterActions", () => {
  it("is a case-insensitive substring filter, and empty means everything", () => {
    const all = actionsFor(fileRow());
    expect(filterActions(all, "")).toHaveLength(all.length);
    expect(filterActions(all, "   ")).toHaveLength(all.length);
    expect(filterActions(all, "copy").map((a) => a.id)).toEqual(["copy_path", "copy_file"]);
    expect(filterActions(all, "RECYCLE").map((a) => a.id)).toEqual(["delete"]);
    expect(filterActions(all, "zzz")).toHaveLength(0);
  });
});
