# Summoning YSpot with one key

The ask was the bare **Windows** key. This records why that one is not
available, what is, and exactly what a user has to do to get it — because the
answer is a registry edit on their side and a one-rule change on ours, and
neither is guessable from the other.

## Why not the Windows key

`RegisterHotKey` binds *modifier + key*. There is no call that binds a lone
modifier, so no amount of work inside YSpot reaches `Win` on its own. The same
is true of YKeys: its whole keyboard surface is `RegisterHotKey`
(`HotkeyListener.cs`), and a grep of its source for `WH_KEYBOARD_LL`,
`VK_APPS`, `LWIN` or `RWIN` returns nothing. So "let YKeys hold it" does not
reach it either — that was the plan of record and it does not work.

What *would* reach it is a low-level keyboard hook (`WH_KEYBOARD_LL`) that
watches for `Win` down followed by `Win` up with no other key between, swallows
that release so the Start menu does not open, and passes everything else
through. That is how the tools that offer it do it, and it is genuinely
unpleasant:

- The hook sees **every keystroke on the machine**, including passwords. It has
  to be in a process the user already trusts with that, and it has to be fast:
  a slow hook is silently evicted by Windows and the binding stops working with
  no error.
- Swallowing the release breaks `Win` as a *modifier* unless the pass-through is
  exactly right — `Win+X`, `Win+L`, `Win+Shift+S`, `Win+V` all have to keep
  working, and `Win+L` cannot be intercepted at all.
- Getting it wrong takes away the Start menu, with nothing on screen to explain
  why, on a machine whose owner then cannot use the launcher to fix it.

That is a real feature with a real failure mode, and it belongs in YKeys — the
process that already owns the keyboard by design — not in the launcher. It is
not scheduled here.

## What is available: one key, via F13–F24

Keyboards do not have F13 through F24. Windows has virtual key codes for them,
and nothing emits those codes unless someone deliberately arranges it. That
makes them the one safe bare binding: there is no keystroke to hijack.

YKeys already reasons exactly this way, and had the carve-out first:

```csharp
// A bare key would hijack normal typing system-wide; F13-F24 are the
// exception since keyboards only emit them as deliberate macro keys.
bool macroKey = vk is >= VK_F13 and <= VK_F24;
if (modifiers == 0 && !macroKey)
    error = "needs a modifier (alt/ctrl/shift/win); only f13-f24 may bind bare";
```

YSpot's `Hotkey::rejection` had the rule without the exception, so a bare F24
was refused — which is why the remap plan could not be finished. The exception
is now in both, worded the same way on purpose.

**YKeys is not required for this.** With the Menu key remapped, YSpot binds F24
through its ordinary `RegisterHotKey` path. Hand-off to YKeys stays an option
(§5.1 as amended) for people who want one daemon holding the whole keyboard,
and `ykeysChord` already spells `f24` correctly for that file.

## The remap, which is the user's half

The Menu key — the one between right `Alt` and right `Ctrl`, `VK_APPS`, the one
that opens a context menu — is the best donor: it is a key almost nobody uses,
and unlike `Win` it is not load-bearing for the OS.

It is remapped with the `Scancode Map` value, a kernel-level scancode
translation. It is machine-wide, not per-user, and it needs a **reboot**.

```powershell
# Menu (E0 5D) -> F24 (0x0076). Run elevated; reboot to take effect.
$map = [byte[]]@(
  0,0,0,0, 0,0,0,0,         # header: version, flags
  2,0,0,0,                  # 2 entries (1 mapping + terminator)
  0x76,0x00, 0x5D,0xE0,     # F24  <-  Menu
  0,0,0,0                   # terminator
)
Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Keyboard Layout' `
  -Name 'Scancode Map' -Type Binary -Value $map
```

To undo it, delete the value and reboot:

```powershell
Remove-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Keyboard Layout' -Name 'Scancode Map'
```

**YSpot does not write this.** It is `HKLM`, it needs elevation, it affects
every user of the machine, and a malformed value can leave a keyboard in a
state its owner cannot type their way out of. Settings shows the recipe and
copies it; the user runs it knowingly. That is the same principle as the
`ykeys.json` line: we show exactly what to paste, we do not reach into
someone else's file.

## What the code does

- `Hotkey::rejection` accepts a modifier-less chord when the key is F13–F24,
  and refuses every other bare key with the message it always had.
- `Hotkey::accelerator` needed no change: it already emits a bare `"F24"` when
  no modifier is set.
- `Hotkey::warning` gains a note for a bare macro key, because a binding that
  depends on a remap the user might later remove should say so where it is set
  rather than fail silently afterwards.

## What is deliberately not done

- No `WH_KEYBOARD_LL` hook anywhere in YSpot.
- No writing to `HKLM`, and no offer to.
- No attempt to detect whether the remap is in place. It is possible — read the
  value back and parse it — but a wrong answer is worse than none, the value is
  machine-wide and may be someone else's, and the honest signal is simply
  whether pressing the key summons the launcher.
