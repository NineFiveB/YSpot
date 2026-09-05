// The chord in YKeys' spelling (SPEC.md §5.1 as amended).
//
// Settings shows the exact line to paste into `ykeys.json` when YKeys is to
// hold the hotkey, and that line is only worth showing if YKeys' parser will
// accept it. The shell captures chords by `KeyboardEvent.code`, which is a
// different vocabulary from the one YKeys reads — `Backquote` versus `grave`,
// `ArrowLeft` versus `left`, `NumpadAdd` versus `numpad_add` — and a few codes
// the shell can bind (CapsLock, PrintScreen) have no YKeys spelling at all.
// Lower-casing the display name looked right and produced lines YKeys refused
// with "unknown key", which after a restart left NEITHER process holding the
// chord. Hence an explicit map, mirroring `HotkeyParser.TryParseKey` in the
// YKeys repo, and `null` for anything it cannot honestly express.

import type { Hotkey } from "./ipc";

/** `KeyboardEvent.code` → YKeys key token, or null when YKeys has none. */
export function ykeysKey(code: string): string | null {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3).toLowerCase();
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code.toLowerCase();
  if (/^Numpad[0-9]$/.test(code)) return `numpad${code.slice(6)}`;
  return NAMED[code] ?? null;
}

const NAMED: Record<string, string> = {
  Space: "space",
  Enter: "enter",
  // The registrar binds both Enter keys to VK_RETURN, so YKeys' "enter" is
  // the identical RegisterHotKey call and a chord captured on the numpad
  // hands over verbatim. (NumpadEqual stays null: it maps to VK_E, which
  // nothing in YKeys names.)
  NumpadEnter: "enter",
  Tab: "tab",
  Escape: "esc",
  Backspace: "backspace",
  Delete: "delete",
  Insert: "insert",
  Home: "home",
  End: "end",
  PageUp: "pageup",
  PageDown: "pagedown",
  ArrowLeft: "left",
  ArrowRight: "right",
  ArrowUp: "up",
  ArrowDown: "down",
  // Punctuation: YKeys names US-layout positions by what the keycap says.
  Minus: "minus",
  Equal: "plus",
  Comma: "comma",
  Period: "period",
  Semicolon: "semicolon",
  Slash: "slash",
  Backquote: "grave",
  BracketLeft: "lbracket",
  BracketRight: "rbracket",
  Backslash: "backslash",
  Quote: "quote",
  IntlBackslash: "oem_102",
  NumpadAdd: "numpad_add",
  NumpadSubtract: "numpad_subtract",
  NumpadMultiply: "numpad_multiply",
  NumpadDivide: "numpad_divide",
  NumpadDecimal: "numpad_decimal",
};

/**
 * The whole chord as a `ykeys.json` key — lowercase, `+`-joined, no spaces —
 * or null when the key has no YKeys spelling, in which case the UI must say
 * so rather than show a line that will be refused.
 */
export function ykeysChord(h: Hotkey): string | null {
  const key = ykeysKey(h.code);
  if (key === null) return null;
  const parts: string[] = [];
  if (h.ctrl) parts.push("ctrl");
  if (h.alt) parts.push("alt");
  if (h.shift) parts.push("shift");
  if (h.win) parts.push("win");
  parts.push(key);
  return parts.join("+");
}
