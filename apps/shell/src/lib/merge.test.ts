// The §5.11 merge and selection contract is normative, and its failure mode
// is silent: rows reorder under the user's cursor, or the selection quietly
// points at a different item than the one they arrowed to. Neither shows up
// in a build or a type check, so it gets tests.

import { describe, expect, it } from "vitest";
import { fileRow, type Row } from "./ipc";
import { KIND_CAPS, byScore, collapseSameName, mergeRows, selectionIndex } from "./merge";

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
  it("is a total order: score, then kind rank, then name", () => {
    expect(byScore(app("A", 1), file("b", 0.5))).toBeLessThan(0);
    expect(byScore(file("b", 0.5), app("A", 1))).toBeGreaterThan(0);
    expect(byScore(app("A", 0.5), file("b", 0.5))).toBeLessThan(0);
    expect(byScore(app("A", 0.5), app("B", 0.5))).toBeLessThan(0);
    expect(byScore(app("A", 0.5), app("A", 0.5))).toBe(0);
  });

  // Ranking settings pages over apps on a tie was tried for `windows` and
  // reviewed out: it put Taskbar above Task Manager, Notifications above
  // Notepad and Camera privacy above the Camera app on a fresh profile —
  // the most common launcher queries there are. These pin the old rule so
  // it cannot come back quietly: on a tie between an app and a page, the
  // name decides, and the app the user was typing towards wins.
  it.each([
    ["task", "Task Manager", "Taskbar"],
    ["not", "Notepad", "Notifications"],
    ["cam", "Camera", "Camera privacy"],
  ])("%s: keeps the app above the same-scored settings page", (_q, appName, pageName) => {
    const page: Row = {
      kind: "setting",
      key: `setting:${pageName}`,
      id: pageName,
      name: pageName,
      subtitle: "Settings",
      score: 0.9,
      matchRanges: [],
    };
    const rows = mergeRows({ apps: [page, app(appName, 0.9)], files: [], frozen: null });
    expect(rows.map((r) => r.name)).toEqual([appName, pageName]);
  });
});

const command = (name: string, score: number, order = 0): Row => ({
  kind: "command",
  key: `command:${name}`,
  id: name,
  name,
  subtitle: "YSpot",
  score,
  matchRanges: [],
  order,
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

const win = (name: string, score: number): Row => ({
  kind: "window",
  key: `window:${name}`,
  id: name,
  name,
  subtitle: "Window",
  score,
  matchRanges: [],
});

describe("KIND_CAPS", () => {
  // Twenty-six rows could reach a viewport that shows between eight and nine,
  // so a query matching five kinds pushed the answer the user typed for off
  // the visible page. The caps are a display budget applied after the sort.
  it("trims after the score sort, keeping the best of each kind", () => {
    const files = Array.from({ length: 12 }, (_, i) => file(`f${i}.txt`, 0.99 - i * 0.01));
    const rows = mergeRows({ apps: [], files, frozen: null });
    expect(rows).toHaveLength(KIND_CAPS.file);
    expect(rows.map((r) => r.score)).toEqual([0.99, 0.98, 0.97]);
  });

  it("never reorders what survives", () => {
    const input = {
      apps: [
        command("Windows Settings", 1.3),
        command("YSpot Settings", 1.3),
        command("File Search", 1.3),
        app("Settings Sync", 0.95),
        app("SettingsGuru", 0.94),
        app("Setup", 0.93),
        app("Settle", 0.92),
        setting("Display", 0.9),
        setting("Bluetooth", 0.89),
        setting("Network", 0.88),
        setting("Storage", 0.87),
        win("Settings — Chrome", 0.86),
        win("Settings — Code", 0.85),
        win("Settings — Slack", 0.84),
      ],
      files: Array.from({ length: 6 }, (_, i) => file(`s${i}.txt`, 0.8 - i * 0.01)),
      frozen: null,
    };
    const capped = mergeRows(input);
    const uncapped = [...input.apps, ...input.files].sort(byScore);
    // Every surviving row appears in the uncapped order, in the same relative
    // order: a subsequence walk, which a cap that reordered would fail.
    let j = 0;
    for (const r of capped) {
      while (j < uncapped.length && uncapped[j].key !== r.key) j += 1;
      expect(j, `${r.name} is out of order or missing`).toBeLessThan(uncapped.length);
      j += 1;
    }
  });

  // The sharp edge of the whole change. If a cap evicts the selected row,
  // selectionIndex silently returns 0 and Enter runs something the user
  // never chose — invisible in a build, a type check and a screenshot.
  it("never drops the selected row (§5.11 r2)", () => {
    const files = Array.from({ length: 12 }, (_, i) => file(`f${i}.txt`, 0.99 - i * 0.01));
    const sel = files[7].key;
    const rows = mergeRows({ apps: [], files, frozen: null, selectedKey: sel });
    expect(rows.some((r) => r.key === sel)).toBe(true);
    // The exempt row is kept AND counts, so the budget stays a real bound:
    // three under the cap plus the one being stood on.
    expect(rows).toHaveLength(KIND_CAPS.file + 1);
    expect(selectionIndex(rows, sel)).toBe(KIND_CAPS.file);
  });

  it("keeps every frozen row, and counts them against the budget", () => {
    const frozen = Array.from({ length: 5 }, (_, i) => file(`frozen${i}.txt`, 0.9 - i * 0.01));
    const arriving = Array.from({ length: 5 }, (_, i) => file(`new${i}.txt`, 0.99 - i * 0.01));
    const rows = mergeRows({ apps: [], files: [...frozen, ...arriving], frozen });
    expect(rows.map((r) => r.key)).toEqual(frozen.map((r) => r.key));
  });
});

describe("collapseSameName", () => {
  // 101 directories under one user profile are named exactly "Settings".
  // They are one answer, not five rows.
  it("collapses same-named rows to the best-scoring one", () => {
    const rows = collapseSameName(
      [
        { ...file("Settings", 0.8929), key: "file:0:a", subtitle: "C:\a\Settings" },
        { ...file("Settings", 0.9091), key: "file:0:b", subtitle: "C:\b\Settings" },
        { ...file("Settings", 0.8929), key: "file:0:c", subtitle: "C:\c\Settings" },
        { ...file("Settings", 0.8929), key: "file:0:d", subtitle: "C:\d\Settings" },
        file("settings.json", 0.8491),
      ],
      5,
    );
    expect(rows).toHaveLength(2);
    expect(rows[0].subtitle).toBe("C:\b\Settings");
    expect(rows[1].name).toBe("settings.json");
  });

  it("keeps distinct names, in score order", () => {
    const rows = collapseSameName(
      [file("e.rs", 0.5), file("a.rs", 0.9), file("c.rs", 0.7), file("b.rs", 0.8), file("d.rs", 0.6)],
      5,
    );
    expect(rows.map((r) => r.name)).toEqual(["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]);
  });

  // Case-insensitive, because the filesystem is.
  it("treats names as case-insensitive", () => {
    const rows = collapseSameName([file("Settings", 0.9), { ...file("SETTINGS", 0.8), key: "file:0:z" }], 5);
    expect(rows).toHaveLength(1);
    expect(rows[0].score).toBe(0.9);
  });
});

// The query `windows`, as the sources actually score it on the author's
// machine — every number below is the real one, not a stand-in.
//
// The Windows directory is an index entry like any other: EXACT at depth 1
// is 1.0 / (1 + 0.02) = 0.980, above every app and page that merely start
// with the word. Twelve apps and four catalog pages tie at the 0.9 prefix
// score, where the frontend's only remaining order is the name — and
// "Windows App Cert Kit" sorts before "Windows backup". So Windows Backup was
// at rows 4 AND 5, once as the page and once as the app, under a folder and
// a dev kit, and nothing on the shared scale could move it.
//
// What did: it is a built-in now, in the band with Windows Settings, and the
// two are declared in the order they should show. Its app row is suppressed
// as the command's duplicate; the `ms-settings:sync` page stays, on the
// shared scale, where it was.
describe("the query `windows`", () => {
  const BS = String.fromCharCode(92);
  const folder = fileRow({
    // FRNs travel as decimal strings: a u64 exceeds the JS safe-integer range.
    id: { volumeIdx: 0, frn: "5" },
    path: `C:${BS}Windows`,
    name: "Windows",
    score: 1 / 1.02,
    matchRanges: [[0, 7]],
  });
  const pages = ["Windows Update", "Windows backup", "Windows Security", "Windows Defender Firewall"];
  // What AppsFolder returns on this machine, less the suppressed Backup row,
  // in the shell's own length-then-name order and cut to its pool of eight.
  const apps8 = [
    "Windows Tools",
    "Windows Security",
    "Windows PowerShell",
    "Windows App Cert Kit",
    "Windows PowerShell ISE",
    "Windows PowerShell (x86)",
    "Windows Memory Diagnostic",
    "Windows Media Player Legacy",
  ];
  const generation = () =>
    mergeRows({
      apps: [
        // Both prefix-match at 0.9 and take the band: an exact tie at 1.4.
        command("Windows Settings", 0.9 + 0.5, 1),
        command("Windows Backup", 0.9 + 0.5, 2),
        ...pages.map((n) => setting(n, 0.9)),
        ...apps8.map((n) => app(n, 0.9)),
      ],
      files: [folder],
      frozen: null,
    });

  it("puts Windows Backup directly under Windows Settings", () => {
    const rows = generation();
    expect(rows.slice(0, 2).map((r) => [r.name, r.kind])).toEqual([
      ["Windows Settings", "command"],
      ["Windows Backup", "command"],
    ]);
  });

  it("breaks that tie by declaration, not by the alphabet", () => {
    // By name, "Windows Backup" leads "Windows Settings" on the strength of
    // a B. The order field is what says otherwise.
    expect(byScore(command("Windows Settings", 1.4, 1), command("Windows Backup", 1.4, 2))).toBeLessThan(0);
    expect(byScore(command("Windows Backup", 1.4, 2), command("Windows Settings", 1.4, 1))).toBeGreaterThan(0);
    // And only a tie: a better score still wins outright, whatever the order.
    expect(byScore(command("Windows Backup", 1.5, 2), command("Windows Settings", 1.4, 1))).toBeLessThan(0);
  });

  it("leaves the directory and the shared scale exactly where they were", () => {
    const rows = generation();
    // The folder is not demoted: demoting one directory named Windows only
    // promotes the next one, and there are several under any profile.
    expect(rows[2]).toEqual(folder);
    expect(rows[2].score).toBeCloseTo(0.98, 3);
    // Below it, the 0.9 tier in the old name order — app before page only
    // when the alphabet says so, never by kind.
    expect(rows[3].name).toBe("Windows App Cert Kit");
  });

  it("shows Windows Backup once as the command; the page keeps its own row", () => {
    const rows = generation();
    const backups = rows.filter((r) => r.name.toLowerCase() === "windows backup");
    expect(backups.map((r) => r.kind)).toEqual(["command", "setting"]);
  });

  it("puts YSpot Settings first for `settings`, as asked, by the same rule", () => {
    // Both are WORD_START 0.8 plus the band. YSpot Settings is declared
    // first; a name sort had Windows first on the strength of a W.
    const rows = mergeRows({
      apps: [command("Windows Settings", 1.3, 1), command("YSpot Settings", 1.3, 0)],
      files: [],
      frozen: null,
    });
    expect(rows.map((r) => r.name)).toEqual(["YSpot Settings", "Windows Settings"]);
  });
});
