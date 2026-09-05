// The Settings hint promises its line "can be copied into ykeys.json as it
// stands". That is a contract with another program's parser, and it broke
// silently once: a lower-cased display name is not a YKeys token, and the
// refused binding left neither process holding the chord after a restart.

import { describe, expect, it } from "vitest";
import { ykeysChord, ykeysKey } from "./ykeys";

describe("ykeysKey", () => {
  it("spells letters, digits and function keys the way YKeys reads them", () => {
    expect(ykeysKey("KeyK")).toBe("k");
    expect(ykeysKey("Digit1")).toBe("1");
    expect(ykeysKey("F1")).toBe("f1");
    expect(ykeysKey("F24")).toBe("f24");
    expect(ykeysKey("F25")).toBeNull();
  });

  it("translates the codes whose YKeys names differ, not just lower-cases them", () => {
    // Each of these is a code the capture field can deliver, and each
    // lower-cased display name is one YKeys refuses with "unknown key".
    // (Whether the shell's registrar can then bind it is a separate
    // question — global-hotkey has no arm for IntlBackslash, for one — but
    // the spelling is right for a hand-edited file either way.)
    expect(ykeysKey("Numpad1")).toBe("numpad1"); // display: "Numpad 1"
    expect(ykeysKey("NumpadAdd")).toBe("numpad_add");
    expect(ykeysKey("Backquote")).toBe("grave");
    expect(ykeysKey("Equal")).toBe("plus");
    expect(ykeysKey("BracketLeft")).toBe("lbracket");
    expect(ykeysKey("ArrowLeft")).toBe("left");
    expect(ykeysKey("IntlBackslash")).toBe("oem_102");
    expect(ykeysKey("Escape")).toBe("esc");
    expect(ykeysKey("Space")).toBe("space");
    // Both Enter keys register as VK_RETURN, which is YKeys' "enter".
    expect(ykeysKey("NumpadEnter")).toBe("enter");
    expect(ykeysKey("NumpadEqual")).toBeNull();
  });

  it("says null for keys YKeys cannot bind rather than inventing a token", () => {
    for (const code of ["CapsLock", "NumLock", "ScrollLock", "PrintScreen", "Pause", "ContextMenu", ""]) {
      expect(ykeysKey(code)).toBeNull();
    }
  });
});

describe("ykeysChord", () => {
  it("joins modifiers and key in YKeys' order and spelling", () => {
    expect(ykeysChord({ ctrl: false, alt: true, shift: false, win: false, code: "Space" })).toBe("alt+space");
    expect(ykeysChord({ ctrl: true, alt: true, shift: true, win: true, code: "KeyK" })).toBe(
      "ctrl+alt+shift+win+k",
    );
    expect(ykeysChord({ ctrl: true, alt: false, shift: false, win: false, code: "Numpad1" })).toBe(
      "ctrl+numpad1",
    );
  });

  it("is null for the whole chord when the key has no spelling", () => {
    expect(ykeysChord({ ctrl: true, alt: false, shift: false, win: false, code: "CapsLock" })).toBeNull();
  });
});
