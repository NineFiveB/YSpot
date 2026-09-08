// The two drawn glyphs have to survive next to the hundred vendored ones.
//
// Both maps get merged at render time and drawn through one <path>, so the
// things that would break quietly are: a key that shadows a vendored key, a
// viewBox that disagrees, a command the renderer does not apply, or a
// coordinate outside the box that gets clipped on one side only. None of
// those show up as an error — they show up as a wrong picture, which is
// exactly the kind of bug a preview at 16px is bad at catching.

import { describe, expect, it } from "vitest";

import { FLUENT_20, FLUENT_VIEWBOX } from "./fluentGlyphs";
import { YSPOT_20, YSPOT_VIEWBOX } from "./yspotGlyphs";

const DRAWN = Object.entries(YSPOT_20);

describe("YSpot's drawn glyphs", () => {
  it("has the two the launcher is missing", () => {
    expect(Object.keys(YSPOT_20).sort()).toEqual([
      "yspot_backup_restore",
      "yspot_windows_shield",
    ]);
  });

  it("shares the vendored viewBox", () => {
    // Different viewBoxes would scale one set against the other, and the
    // difference is invisible until two icons sit in the same list.
    expect(YSPOT_VIEWBOX).toBe(FLUENT_VIEWBOX);
  });

  it("claims no key the vendored set already uses", () => {
    // A collision silently shadows one drawing with the other, depending on
    // which way round the merge spreads them.
    const clash = Object.keys(YSPOT_20).filter((k) => k in FLUENT_20);
    expect(clash).toEqual([]);
  });

  it.each(DRAWN)("%s uses only the commands the renderer applies", (_key, d) => {
    // The renderer sets `fill` and nothing else: no stroke, no fill-rule, no
    // arcs to flatten. vendor-fluent-icons.py asserts the same of every
    // vendored path, so this keeps the drawn ones to the same contract.
    expect(d).toMatch(/^[MLCZ0-9 .,-]+$/);
    expect(d.startsWith("M")).toBe(true);
    expect(d.endsWith("Z")).toBe(true);
  });

  it.each(DRAWN)("%s stays inside the 20px box", (_key, d) => {
    const nums = (d.match(/-?\d+(?:\.\d+)?/g) ?? []).map(Number);
    expect(nums.length).toBeGreaterThan(0);
    expect(nums.every(Number.isFinite)).toBe(true);
    expect(Math.min(...nums)).toBeGreaterThanOrEqual(0);
    expect(Math.max(...nums)).toBeLessThanOrEqual(20);
  });

  it.each(DRAWN)("%s carries the holes it is drawn with", (_key, d) => {
    // Both drawings are a solid with something knocked out of it, and a hole
    // is a second subpath. One subpath means the knockout was lost — which is
    // how the icon would look if a boolean silently collapsed.
    expect(d.split("M").length - 1).toBeGreaterThan(1);
  });
});
