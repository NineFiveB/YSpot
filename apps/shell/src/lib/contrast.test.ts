// §5.12: "Both themes MUST meet WCAG 2.1 AA contrast."
//
// A normative requirement that nothing enforces is a requirement that dies
// the first time someone nudges a hex value to taste. This reads the real
// token values out of styles.css and computes the ratios, so the palette
// cannot drift below AA without a test going red.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const cssPath = resolve(dirname(fileURLToPath(import.meta.url)), "..", "styles.css");
const css = readFileSync(cssPath, "utf8");

/** The `:root` block's tokens, and the dark-scheme block's overrides. */
function tokensOf(block: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of block.matchAll(/--([a-z-]+):\s*([^;]+);/g)) {
    out[m[1]] = m[2].trim();
  }
  return out;
}

/** The first `:root { … }` block, which carries the light palette. */
function lightBlock(): string {
  const i = css.indexOf(":root {");
  return css.slice(i, css.indexOf("}", i));
}

/** The `prefers-color-scheme: dark` block's `:root` overrides. */
function darkBlock(): string {
  const i = css.indexOf("@media (prefers-color-scheme: dark)");
  const j = css.indexOf(":root {", i);
  return css.slice(j, css.indexOf("}", j));
}

function channel(c: number): number {
  const s = c / 255;
  return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
}

function luminance(hex: string): number {
  const h = hex.replace("#", "").trim();
  const r = parseInt(h.slice(0, 2), 16);
  const g = parseInt(h.slice(2, 4), 16);
  const b = parseInt(h.slice(4, 6), 16);
  return 0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b);
}

function ratio(a: string, b: string): number {
  const la = luminance(a);
  const lb = luminance(b);
  const [hi, lo] = la > lb ? [la, lb] : [lb, la];
  return (hi + 0.05) / (lo + 0.05);
}

describe("WCAG 2.1 AA contrast (§5.12)", () => {
  const themes = {
    light: tokensOf(lightBlock()),
    dark: { ...tokensOf(lightBlock()), ...tokensOf(darkBlock()) },
  };

  it("finds the palettes it is meant to check", () => {
    for (const [name, t] of Object.entries(themes)) {
      for (const token of ["bg", "fg", "muted", "accent"]) {
        expect(t[token], `${name} --${token}`).toMatch(/^#[0-9a-f]{6}$/i);
      }
    }
    // The two themes must actually differ, or one of them is not being read.
    expect(themes.light.bg).not.toBe(themes.dark.bg);
  });

  it("meets 4.5:1 for body text in both themes", () => {
    for (const [name, t] of Object.entries(themes)) {
      expect(ratio(t.fg, t.bg), `${name} fg on bg`).toBeGreaterThanOrEqual(4.5);
      // Row subtitles and hints use --muted at small sizes, so they are
      // body text for the purposes of AA, not "large text".
      expect(ratio(t.muted, t.bg), `${name} muted on bg`).toBeGreaterThanOrEqual(4.5);
    }
  });

  it("meets at least 3:1 for the accent, which is only ever UI or large", () => {
    for (const [name, t] of Object.entries(themes)) {
      expect(ratio(t.accent, t.bg), `${name} accent on bg`).toBeGreaterThanOrEqual(3);
    }
  });
});
