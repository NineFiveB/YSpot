// Settings, as a view INSIDE the launcher (SPEC.md §5.9 as amended, §5.7's
// navigation stack).
//
// You search for it, you get it in place, and Esc takes you back to the
// query — no second window at the taskbar to change a hotkey. The launcher
// grows to fit this view and shrinks back when it closes.
//
// Every mutation goes through the shell (§5.9): this component never writes a
// file or touches the registry itself. A hotkey change is rebound before it
// is persisted, so a rejected chord surfaces here as a conflict and the old
// binding keeps working.

import { useCallback, useEffect, useState, type ReactElement } from "react";
import * as ipc from "./lib/ipc";
import { ykeysChord } from "./lib/ykeys";

/** The modifier keys, which are never the chord's own key. */
const MODIFIER_CODES = new Set([
  "ControlLeft",
  "ControlRight",
  "AltLeft",
  "AltRight",
  "ShiftLeft",
  "ShiftRight",
  "MetaLeft",
  "MetaRight",
]);

/** How a chord reads to a person: `Ctrl + Shift + K`. */
export function describeHotkey(h: ipc.Hotkey): string {
  const parts: string[] = [];
  if (h.ctrl) parts.push("Ctrl");
  if (h.alt) parts.push("Alt");
  if (h.shift) parts.push("Shift");
  if (h.win) parts.push("Win");
  parts.push(prettyCode(h.code));
  return parts.join(" + ");
}

function prettyCode(code: string): string {
  if (code.startsWith("Key")) return code.slice(3);
  if (code.startsWith("Digit")) return code.slice(5);
  if (code.startsWith("Numpad")) return `Numpad ${code.slice(6)}`;
  return code || "…";
}

/**
 * §5.1 offers these one click away when the chosen chord is taken. Exported
 * because onboarding runs the same conflict flow — §5.1 asks for the
 * alternatives wherever the conflict is shown, not only in Settings.
 */
export const ALTERNATIVES: ipc.Hotkey[] = [
  { ctrl: true, alt: false, shift: false, win: false, code: "Space" },
  { ctrl: true, alt: true, shift: false, win: false, code: "Space" },
  { ctrl: true, alt: false, shift: true, win: false, code: "Space" },
];

interface Props {
  /** Esc, or the Back control: return to the query (§5.7). */
  onClose: () => void;
}

export default function Settings({ onClose }: Props): ReactElement {
  const [view, setView] = useState<ipc.SettingsView | null>(null);
  const [capturing, setCapturing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);

  useEffect(() => {
    void ipc.getSettings().then(setView).catch((e) => setError(String(e)));
  }, []);

  // §5.1 as amended: with YKeys holding the chord, this view reports the
  // binding rather than owning it.
  const external = view?.settings.hotkey_source === "ykeys";
  // The chord in YKeys' own vocabulary, or null when it has none.
  const chordLine = view ? ykeysChord(view.settings.hotkey) : null;

  const apply = useCallback(async (next: ipc.Settings, note: string) => {
    setError(null);
    try {
      setView(await ipc.saveSettings(next));
      setSaved(note);
      window.setTimeout(() => setSaved(null), 2000);
    } catch (e) {
      // A rejected hotkey leaves the old one bound, so the view stays
      // truthful — only the message changes.
      setError(String(e));
    }
  }, []);

  // Key capture (§5.1): modifiers plus a virtual key taken from `code`, so
  // the binding does not depend on the keyboard layout. While capturing,
  // this handler owns every key — including Esc, which cancels rather than
  // leaving the view.
  useEffect(() => {
    // Not while YKeys holds the chord: the button is disabled, but a capture
    // begun before the toggle was ticked would still be armed, and the next
    // keypress would register a shell hotkey under a setting that says the
    // shell registers nothing.
    if (!capturing || !view || external) return;
    const onKey = (e: KeyboardEvent): void => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape") {
        setCapturing(false);
        return;
      }
      if (MODIFIER_CODES.has(e.code)) return; // still waiting for the key
      setCapturing(false);
      void apply(
        {
          ...view.settings,
          hotkey: {
            ctrl: e.ctrlKey,
            alt: e.altKey,
            shift: e.shiftKey,
            win: e.metaKey,
            code: e.code,
          },
        },
        "Hotkey updated",
      );
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [capturing, view, apply, external]);

  // A capture in progress when the hand-off happens is cancelled, so the
  // button does not sit reading "Press a chord…" while disabled.
  useEffect(() => {
    if (external) setCapturing(false);
  }, [external]);

  // Esc leaves the view (§5.7 pops the navigation stack), except while a
  // chord is being captured — handled above.
  useEffect(() => {
    if (capturing) return;
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [capturing, onClose]);

  return (
    <div className="settings">
      <div className="settings-header">
        <button className="settings-back" onClick={onClose} aria-label="Back to search">
          ←
        </button>
        <h1>YSpot Settings</h1>
        <span className="settings-esc">Esc to go back</span>
      </div>

      {!view ? (
        <p className="settings-note">{error ?? "Loading…"}</p>
      ) : (
        <div className="settings-body">
          <h2>General</h2>

          <div className="settings-row">
            <div className="settings-label">
              <div>Hotkey</div>
              <div className="settings-hint">
                {external
                  ? "YKeys holds this chord. Change it in ykeys.json, not here."
                  : "The chord that summons the launcher. While capturing, Esc keeps the current one."}
              </div>
            </div>
            <button
              className={capturing ? "chord chord-capturing" : "chord"}
              // Disabled rather than hidden: the chord is still what summons
              // the launcher, so it belongs on screen — it is just not ours to
              // rebind, and a button that silently did nothing would be worse
              // than one that plainly cannot be pressed.
              disabled={external}
              onClick={() => setCapturing((c) => !c)}
            >
              {capturing ? "Press a chord…" : describeHotkey(view.settings.hotkey)}
            </button>
          </div>

          <div className="settings-row">
            <div className="settings-label">
              <div>Let YKeys hold the hotkey</div>
              <div className="settings-hint">
                One daemon owns the keyboard instead of every app claiming a chord of
                its own. YKeys registers it and summons YSpot by message, which costs
                nothing extra.
                {chordLine !== null ? (
                  <>
                    {" "}
                    Add this to <code>~/.config/ykeys/ykeys.json</code>:
                    <code className="settings-code">"{chordLine}": "@signal:YSpot.Signal"</code>
                  </>
                ) : (
                  // Said rather than guessed: a line YKeys refuses leaves
                  // neither process holding the chord after a restart.
                  <>
                    {" "}
                    YKeys has no spelling for{" "}
                    <strong>{describeHotkey(view.settings.hotkey)}</strong>; pick a
                    different chord first.
                  </>
                )}
              </div>
            </div>
            <input
              type="checkbox"
              checked={external}
              disabled={chordLine === null && !external}
              onChange={(e) =>
                void apply(
                  {
                    ...view.settings,
                    hotkey_source: e.target.checked ? "ykeys" : "shell",
                  },
                  e.target.checked ? "YKeys holds the hotkey" : "YSpot holds the hotkey",
                )
              }
            />
          </div>

          {view.hotkeyWarning ? (
            <p className="settings-warning">{view.hotkeyWarning}</p>
          ) : null}
          {/*
            A rebind we just refused, or — from the view — a chord that never
            registered at startup. §5.1's conflict flow has to reach the
            second case too: the chord is in settings and looks fine, and the
            only symptom is that pressing it does nothing.
          */}
          {(error ?? view.hotkeyError) ? (
            <div className="settings-error">
              <p>{error ?? view.hotkeyError}</p>
              <div className="settings-alternatives">
                {ALTERNATIVES.map((h) => (
                  <button
                    key={describeHotkey(h)}
                    className="chord"
                    // The banner can show for any failed action; under YKeys
                    // these would register a shell hotkey the setting says
                    // does not exist.
                    disabled={external}
                    onClick={() =>
                      void apply({ ...view.settings, hotkey: h }, "Hotkey updated")
                    }
                  >
                    {describeHotkey(h)}
                  </button>
                ))}
              </div>
            </div>
          ) : null}

          <div className="settings-row">
            <div className="settings-label">
              <div>Start with Windows</div>
              <div className="settings-hint">
                Adds a per-user startup entry you can also see in Task Manager.
              </div>
            </div>
            <input
              type="checkbox"
              checked={view.autostart}
              onChange={(e) => {
                const want = e.target.checked;
                void ipc
                  .setAutostart(want)
                  .then(() => ipc.getSettings())
                  .then(setView)
                  .catch((err) => setError(String(err)));
              }}
            />
          </div>

          <div className="settings-row">
            <div className="settings-label">
              <div>Theme</div>
              <div className="settings-hint">Follows Windows unless you choose otherwise.</div>
            </div>
            <select
              value={view.settings.theme}
              onChange={(e) =>
                void apply(
                  { ...view.settings, theme: e.target.value as ipc.Settings["theme"] },
                  "Theme updated",
                )
              }
            >
              <option value="system">System</option>
              <option value="light">Light</option>
              <option value="dark">Dark</option>
            </select>
          </div>

          <h2>Diagnostics</h2>

          <div className="settings-row">
            <div className="settings-label">
              <div>Save crash reports</div>
              <div className="settings-hint">
                Lets Windows write a crash dump to your own machine if YSpot stops
                unexpectedly. Nothing is sent anywhere — off, no dump is written at all.
              </div>
            </div>
            <input
              type="checkbox"
              checked={view.settings.diagnostics?.crash_reports ?? false}
              onChange={(e) =>
                void apply(
                  {
                    ...view.settings,
                    diagnostics: { crash_reports: e.target.checked },
                  },
                  e.target.checked ? "Crash reports on" : "Crash reports off",
                )
              }
            />
          </div>

          <div className="settings-row">
            <div className="settings-label">
              <div>Logs and crash dumps</div>
              <div className="settings-hint">
                Everything YSpot records about itself, on this machine only.
              </div>
            </div>
            <button
              className="clip-clear"
              onClick={() => void ipc.openDiagnosticsFolder().catch((e) => setError(String(e)))}
            >
              Open folder
            </button>
          </div>

          <h2>Search</h2>
          <p className="settings-note">
            Indexed volumes, content-search scopes and exclusions live in the service, which
            has no configuration channel yet. They arrive with the <code>ConfigUpdate</code>{" "}
            message.
          </p>
        </div>
      )}

      {saved ? <div className="settings-saved">{saved}</div> : null}
    </div>
  );
}
