# Running the M1 dogfood

The last M1 exit criterion: use YSpot **exclusively** for two weeks, no daily
crash. Nothing is packaged yet, so this is how to run what is on `main`.

## 1. Build release

Debug is what `cargo test` exercises; every latency number was measured on
release, and the difference is visible. From the repo root:

```
cd apps/shell && npm run build && cd ../..
cargo build --release -p yspot-shell -p yspot-indexd
```

**Build from the current `main`, not from a binary you already have.** The
hardening review landed four in-process crashes' worth of fixes after the
first release build was made, and one of them — a multi-byte character in a
unit conversion — is reachable from a single keystroke. A stale exe would
spend the fortnight reproducing bugs that are already fixed, which is the one
outcome that wastes the two weeks.

## 2. Start the index service (elevated)

There is no MSI yet (§9.1 is later), so the service runs as a console
process. It needs an **administrator** prompt for the MFT/USN handle:

```
.\target\release\yspot-indexd.exe --mft C:
```

Leave that window open — closing it stops the service. It logs to the
console only (`RUST_LOG=debug` for more). Order does not matter: the shell
reconnects with backoff when the pipe appears.

Without it, YSpot still works — apps, settings, calculator, clipboard,
windows — and file search falls back to Windows Search (§9.5). That is a
valid mode, but it is not the one being dogfooded.

## 3. Start the shell

```
.\target\release\yspot-shell.exe
```

First run shows onboarding: hotkey confirmation, service status, autostart
consent, crash-report consent. Autostart writes an HKCU Run value pointing at
**this exe's path**, so do not move the binary afterwards — rebuild in place.

## 4. Exclusivity

Disable or uninstall whatever Alt+Space launches today (PowerToys Run,
Copilot's binding). If it stays, you will fall back to it the first time YSpot
annoys you and never file the bug. If Alt+Space cannot be taken, onboarding
names the likely owner and offers alternatives (§5.1).

Optional, if YKeys should own the chord (§5.1 amended): Settings → "Let YKeys
hold the hotkey", then add the line it shows to `~/.config/ykeys/ykeys.json`.
The installed `ykeys.exe` (0.1.4) does not know `@signal:`; use a build of
YKeys `main` (`dotnet publish src/YKeys -r win-x64 -c Release -o publish`).

## 5. Where everything lives

`%LOCALAPPDATA%\YSpot\`:

- `logs\shell.log` — structured JSON, rotated; the first thing to read.
- `settings.json` — hotkey, theme, consents, `hotkey_source`.
- `clipboard.db`, `frecency.db` — DPAPI-sealed and per-user.
- `crashes\` — WER dumps, **only** if crash-report consent was given.

## 6. When something goes wrong

A crash, a wrong result, a hotkey that stopped working — the log usually says
why. Grab the tail of `shell.log`, the service console output, and any dump in
`crashes\`, and open an issue with the exact query or action. "It felt slow"
is a bug too; say what you typed and roughly how long it took.

## 7. Stopping

Tray → Quit for the shell (the service keeps running, by design, §5.5).
Ctrl+C in the service window for the service.
