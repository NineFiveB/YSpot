// First-run onboarding (SPEC.md §5.9), as a view inside the launcher —
// the same shape Settings and clipboard history took.
//
// Five steps, in §5.9's order: hotkey, service consent, scope, autostart,
// diagnostics consent. Two of them can only report rather than act, and say
// so plainly instead of offering a button that does nothing:
//
// - The **service** step's Install action launches the elevated service MSI,
//   which does not exist until packaging (§9.1). The step reports whether
//   the service is answering and always offers the skip path §5.9 requires.
// - **Scope selection** needs the service's `ConfigUpdate` message, which is
//   not implemented. The step states the default rather than pretending to
//   offer a choice.
//
// Both are recorded in docs/M1.md. The three steps that DO act — hotkey,
// autostart, diagnostics — are where consent actually lives, which is why
// an existing install sees this once too.

import { useCallback, useEffect, useState, type ReactElement } from "react";
import { describeHotkey } from "./Settings";
import * as ipc from "./lib/ipc";

interface Props {
  /** Finished or skipped: back to the query. */
  onClose: () => void;
}

const STEPS = ["Hotkey", "Fast search", "What's indexed", "Start with Windows", "Diagnostics"];

export default function Onboarding({ onClose }: Props): ReactElement {
  const [step, setStep] = useState(0);
  const [state, setState] = useState<ipc.OnboardingState | null>(null);
  const [settings, setSettings] = useState<ipc.Settings | null>(null);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(() => {
    void ipc.onboardingState().then(setState).catch((e) => setError(String(e)));
    void ipc
      .getSettings()
      .then((v) => setSettings(v.settings))
      .catch((e) => setError(String(e)));
  }, []);

  useEffect(refresh, [refresh]);

  const finish = useCallback(() => {
    void ipc
      .finishOnboarding()
      .then(onClose)
      .catch((e) => setError(String(e)));
  }, [onClose]);

  // Esc leaves the wizard without marking it done, so it comes back next
  // time — skipping is not the same as deciding.
  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (e.key === "Escape") {
        e.preventDefault();
        e.stopPropagation();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const last = step === STEPS.length - 1;

  return (
    <div className="settings">
      <div className="settings-header">
        <h1>Welcome to YSpot</h1>
        <span className="settings-esc">
          Step {step + 1} of {STEPS.length} · Esc to finish later
        </span>
      </div>

      <div className="settings-body">
        <h2>{STEPS[step]}</h2>

        {step === 0 ? (
          <>
            <p className="settings-note">
              {state?.hotkeyError
                ? `That chord could not be registered: ${state.hotkeyError}`
                : `Press ${state ? describeHotkey(state.hotkey) : "the hotkey"} anywhere to summon YSpot.`}
            </p>
            <p className="settings-note">
              If something else already owns it — PowerToys Run and Copilot both like
              Alt+Space — you can change it in Settings at any time.
            </p>
          </>
        ) : null}

        {step === 1 ? (
          <>
            <p className="settings-note">
              Instant file search needs the YSpot index service, which installs separately
              and asks for administrator rights once.
            </p>
            <p className="settings-note">
              {state?.serviceConnected
                ? "It is installed and running, so file search is already instant."
                : "It is not running. YSpot works without it — apps, settings, the calculator, clipboard history and window management are unaffected — and file search falls back to Windows Search, which is slower."}
            </p>
            <p className="settings-note">
              The installer ships with the packaged build; until then this step is
              informational, and skipping costs you nothing you can see today.
            </p>
          </>
        ) : null}

        {step === 2 ? (
          <>
            <p className="settings-note">
              By default the service indexes file <em>names</em> on every fixed NTFS drive.
              Full-text content indexing is off, and is opted into per folder.
            </p>
            <p className="settings-note">
              Choosing scopes needs a configuration channel to the service that is not built
              yet, so for now this is the default and Settings will grow the controls.
            </p>
          </>
        ) : null}

        {step === 3 ? (
          <>
            <p className="settings-note">
              YSpot can start with Windows so the hotkey works from the moment you log in.
              It appears in Task Manager's Startup tab, where you can also turn it off.
            </p>
            <label className="clip-toggle">
              <input
                type="checkbox"
                checked={state?.autostart ?? false}
                onChange={(e) => {
                  const want = e.target.checked;
                  void ipc
                    .setAutostart(want)
                    .then(refresh)
                    .catch((err) => setError(String(err)));
                }}
              />
              Start YSpot when I sign in
            </label>
          </>
        ) : null}

        {step === 4 ? (
          <>
            <p className="settings-note">
              If YSpot stops unexpectedly, Windows can write a crash dump to your own
              machine so the problem can be diagnosed. Nothing is sent anywhere.
            </p>
            <label className="clip-toggle">
              <input
                type="checkbox"
                checked={state?.crashReports ?? false}
                onChange={(e) => {
                  const want = e.target.checked;
                  if (!settings) return;
                  void ipc
                    .saveSettings({ ...settings, diagnostics: { crash_reports: want } })
                    .then(refresh)
                    .catch((err) => setError(String(err)));
                }}
              />
              Save crash dumps on this machine
            </label>
          </>
        ) : null}

        {error ? <div className="settings-error">{error}</div> : null}
      </div>

      <div className="clip-footer">
        <button
          className="clip-clear"
          disabled={step === 0}
          onClick={() => setStep((s) => Math.max(0, s - 1))}
        >
          Back
        </button>
        <button className="clip-clear" onClick={last ? finish : () => setStep((s) => s + 1)}>
          {last ? "Start using YSpot" : "Next"}
        </button>
      </div>
    </div>
  );
}
