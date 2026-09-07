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

Leave that window open — closing it stops the service. Order does not
matter: the shell reconnects with backoff when the pipe appears.

It logs to `%ProgramData%\YSpot\logs\indexd.log` as well as the console
(`RUST_LOG=debug` for more), in the same one-JSON-object-per-line format the
shell uses. That matters here more than it looks: a console is a buffer that
scrolls away, dies with its window, and is empty by the time you notice
anything at 09:00. The file is what you actually read after a bad night.

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
  The service writes the matching `%ProgramData%\YSpot\logs\indexd.log`;
  both carry a `process` field, so concatenating them and sorting by `ts`
  gives one timeline across the pair.
- `settings.json` — hotkey, theme, consents, `hotkey_source`.
- `clipboard.db`, `frecency.db` — DPAPI-sealed and per-user.
- `crashes\` — WER dumps, **only** if crash-report consent was given.

## 6. When something goes wrong

A crash, a wrong result, a hotkey that stopped working — the log usually says
why. Both logs are JSON lines, and the two live in different directories
because §8.5 puts each process's log where its privilege allows. Reading them
as one timeline is what answers "the launcher showed nothing, what did the
service think?", so there is a script for it:

```
.\scripts\Read-YSpotLogs.ps1                     # last 200 lines, merged
.\scripts\Read-YSpotLogs.ps1 -Level error,warn -Tail 0
.\scripts\Read-YSpotLogs.ps1 -Pattern 'pipe|reconnect'
.\scripts\Read-YSpotLogs.ps1 -Raw > logs.txt     # to attach to an issue
```

It merges at millisecond resolution, because a query and its answer are tens
of milliseconds apart and a coarser sort would put them in the wrong order.

**If file results stop reflecting reality** — a file you just created never
shows up, but everything else still works — check for a dead maintenance
thread before assuming a search bug:

```
.\scripts\Read-YSpotLogs.ps1 -Level error -Pattern 'thread' -Tail 0
```

A service whose USN tailer has died keeps answering searches perfectly well,
from an index frozen at the moment it stopped. It looks healthy. The log line
is the only thing that says otherwise, and it names the restart as the fix.

**To find where a run began**, which over two weeks of logons and restarts is
the first thing you need:

```
.\scripts\Read-YSpotLogs.ps1 -Pattern run-start -Tail 0
```

Each process writes one such line as it starts, carrying its version, its pid
and the log file it resolved. That last part answers "am I even reading the
right file" without leaving the log.

Attach `-Raw` output and any dump in `crashes\`, and open an issue with the
exact query or action. "It felt slow"
is a bug too; say what you typed and roughly how long it took.

## 7. Stopping

Tray → Quit for the shell (the service keeps running, by design, §5.5).
Ctrl+C in the service window for the service.
