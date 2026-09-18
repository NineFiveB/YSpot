// The mapping is a hand-maintained table of 93 rows, and its failure mode is
// a wrong picture rather than an error — which is the bug it exists to fix, so
// it gets tests that read the shipping catalog rather than a copy of it.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

import type { Row } from "./ipc";
import {
  COMMAND_GLYPH,
  GLYPHS,
  GLYPH_VIEWBOX,
  KIND_GLYPH_KEY,
  SETTING_GLYPH,
  glyphKey,
  glyphPath,
} from "./rowGlyph";
import { FLUENT_20, FLUENT_VIEWBOX } from "./fluentGlyphs";
import { YSPOT_20, YSPOT_VIEWBOX } from "./yspotGlyphs";

const here = dirname(fileURLToPath(import.meta.url));
const catalogPath = resolve(here, "..", "..", "src-tauri", "resources", "settings-catalog.json");
const catalog = JSON.parse(readFileSync(catalogPath, "utf8")) as {
  settings: { id: string; name: string }[];
  control_panel: { id: string; name: string }[];
};
const catalogRows = [...catalog.settings, ...catalog.control_panel];

const row = (kind: Row["kind"], id: string): Row =>
  ({ kind, key: `${kind}:${id}`, id, name: id, subtitle: "", score: 1, matchRanges: [] }) as Row;

describe("the settings glyph table", () => {
  // The catalog is data, shipped beside the executable and editable without a
  // code release (§7.2). Adding a page there and forgetting this table gives
  // the new row a gear — indistinguishable from the other gears, which is the
  // original complaint. Reading the real file is what makes that impossible.
  it("covers every row in the shipping catalog", () => {
    const missing = catalogRows.filter((e) => !(e.id in SETTING_GLYPH)).map((e) => `${e.id} (${e.name})`);
    expect(missing).toEqual([]);
  });

  it("names no row the catalog does not have", () => {
    const known = new Set(catalogRows.map((e) => e.id));
    expect(Object.keys(SETTING_GLYPH).filter((id) => !known.has(id))).toEqual([]);
  });

  it("uses only keys that exist", () => {
    const bad = Object.entries(SETTING_GLYPH)
      .filter(([, k]) => !(k in GLYPHS))
      .map(([id, k]) => `${id} -> ${k}`);
    expect(bad).toEqual([]);
  });

  // The property the whole change exists for. Control Panel items are exempt:
  // each carries its own CLSID bitmap, so its key is a fallback that is only
  // visible on a machine that does not register the canonical name.
  it("gives every ms-settings: page a key no other page uses", () => {
    const seen = new Map<string, string>();
    const clashes: string[] = [];
    for (const e of catalog.settings) {
      const k = SETTING_GLYPH[e.id];
      const first = seen.get(k);
      if (first) clashes.push(`${k}: ${first} and ${e.id}`);
      else seen.set(k, e.id);
    }
    expect(clashes).toEqual([]);
  });

  // The two drawn icons were made for these two rows and nothing else; the
  // third is reserved for a YSuite row that does not exist yet.
  it("puts the drawn icons on the pages they were drawn for", () => {
    expect(SETTING_GLYPH["ms-settings:windowsdefender"]).toBe("yspot_windows_shield");
    expect(SETTING_GLYPH["ms-settings:sync"]).toBe("yspot_backup_restore");
    expect(Object.values(SETTING_GLYPH)).not.toContain("yspot_suite_cube");
  });
});

describe("the command glyph table", () => {
  it("uses only keys that exist", () => {
    const bad = Object.entries(COMMAND_GLYPH).filter(([, k]) => !(k in GLYPHS));
    expect(bad).toEqual([]);
  });

  // A built-in added without an entry here falls back to a console mark,
  // which says "shell command" and not what the command does.
  it("covers every built-in the Rust side declares", () => {
    const rs = readFileSync(resolve(here, "..", "..", "src-tauri", "src", "commands.rs"), "utf8");
    // Only `all()`: the tests below it name real ids too, but a string in a
    // test is not a declaration and should not be able to add one here.
    const start = rs.indexOf("pub fn all()");
    const end = rs.indexOf("\n}", start);
    expect(start, "commands.rs no longer has an all()").toBeGreaterThan(-1);
    const ids = [...new Set([...rs.slice(start, end).matchAll(/"((?:yspot|windows)\.[a-z]+)"/g)].map((m) => m[1]))];
    expect(ids.length).toBeGreaterThanOrEqual(6);
    expect(ids.filter((id) => !(id in COMMAND_GLYPH))).toEqual([]);
  });
});

describe("glyphKey", () => {
  it("resolves a settings page to its own key", () => {
    expect(glyphKey(row("setting", "ms-settings:bluetooth"))).toBe("bluetooth");
  });

  it("resolves a built-in to its own key", () => {
    expect(glyphKey(row("command", "yspot.clipboard"))).toBe("clipboard_paste");
  });

  it("falls back to the kind for an id it does not know", () => {
    expect(glyphKey(row("setting", "ms-settings:invented"))).toBe(KIND_GLYPH_KEY.setting);
    expect(glyphKey(row("command", "yspot.invented"))).toBe(KIND_GLYPH_KEY.command);
  });

  it("gives every kind a key that exists, so no row is ever blank", () => {
    for (const kind of Object.keys(KIND_GLYPH_KEY) as Row["kind"][]) {
      expect(GLYPHS[KIND_GLYPH_KEY[kind]], kind).toBeTruthy();
      expect(glyphPath(row(kind, "anything"))).toBeTruthy();
    }
  });
});

describe("the two glyph sets", () => {
  // Spread-merged into one record: a shared key would silently shadow one of
  // them, and which one depends on spread order.
  it("share no key", () => {
    expect(Object.keys(YSPOT_20).filter((k) => k in FLUENT_20)).toEqual([]);
    expect(Object.keys(GLYPHS)).toHaveLength(
      Object.keys(FLUENT_20).length + Object.keys(YSPOT_20).length,
    );
  });

  it("share one viewBox, so one <svg> serves every row", () => {
    expect(YSPOT_VIEWBOX).toBe(FLUENT_VIEWBOX);
    expect(GLYPH_VIEWBOX).toBe(FLUENT_VIEWBOX);
  });
});
