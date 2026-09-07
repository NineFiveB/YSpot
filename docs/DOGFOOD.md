# Running the M1 dogfood

The last M1 exit criterion: use YSpot **exclusively** for two weeks, no daily
crash. Nothing is packaged yet, so this is how to run what is on `main`.

## 1. Build release

Debug is what `cargo test` exercises; every latency number was measured on
release, and the difference is visible. From the repo root:

```
cd apps/shell; npm run build; cd ../..
cargo build --release -p yspot-shell -p yspot-indexd
```

Semicolons, not `&&`: Windows PowerShell 5.1 rejects `&&` outright with "the
token '&&' is not a valid statement separator in this version", and 5.1 is
what a plain **Windows PowerShell** shortcut still opens. Everything else here
is PowerShell-flavoured, so nothing else would have warned you.

**Build from the current `main`, not from a binary you already have.** The
hardening review fixed a crash reachable from a single keystroke — a
multi-byte character in a unit conversion — along with the crash handler that
was supposed to have recorded it, after the first release build was made. A
stale exe would spend the fortnight reproducing bugs that are already fixed,
which is the one outcome that wastes the two weeks.

## 2. Start the index service (elevated)

There is no MSI yet (§9.1 is later), so the service runs as a console
process. It needs an **administrator** prompt for the MFT/USN handle:

```
cd C:\Users\ogrus\Documents\Development\YSpot
.\target\release\yspot-indexd.exe --mft C:
```

The `cd` is not decoration: an elevated prompt opens in `C:\Windows\System32`,
not the repo, so the relative path fails without it. This is the one shell
whose working directory you do not choose.

Leave that window open — closing it stops the service. Order does not
matter: the shell reconnects with backoff when the pipe appears.

It logs to `%ProgramData%\YSpot\logs\indexd.log` as well as the console. For
more, set the level first, as its own statement — a bash-style `RUST_LOG=debug
.\yspot-indexd.exe` is not a thing PowerShell understands, and it will tell you
the term is not recognized rather than starting anything:

```
$env:RUST_LOG = 'debug'
.\target\release\yspot-indexd.exe --mft C:
```

A bare level only, not `env_logger`'s `module=level` form; a value it cannot
read is warned about in the log rather than ignored. Same
one-JSON-object-per-line format as the shell.

The file half matters more than it looks. A console is a buffer: it scrolls
away, it dies with its window, and it is empty by the time you notice anything
at 09:00. The file is what you actually read after a bad night.

Without it, YSpot still works — apps, settings, calculator, clipboard,
windows — and file search falls back to Windows Search (§9.5). That is a
valid mode, but it is not the one being dogfooded.

## 3. Start the shell

```
.\target\release\yspot-shell.exe
```

First run shows onboarding, five steps: Hotkey, Fast search, What's indexed,
Start with Windows, Diagnostics. The middle two are report-only — index scope
selection needs a protocol message that is M2 — so the decisions are the
hotkey, autostart, and crash reporting. Esc defers the wizard rather than
completing it, so it returns on the next summon.

**Say yes to crash reporting.** It is opt-in and defaults to off, and with it
off no dump is ever written: `crashes\` stays empty for the whole fortnight,
including for the crash you most want to explain.

Autostart writes an HKCU Run value pointing at **this exe's path**, so do not
move the binary afterwards — rebuild in place.

## 4. Exclusivity

Disable or uninstall whatever Alt+Space launches today (PowerToys Run,
Copilot's binding). If it stays, you will fall back to it the first time YSpot
annoys you and never file the bug. If Alt+Space cannot be taken, onboarding
names the likely owner and offers alternatives (§5.1).

Note that the chord being free *right now* does not settle it. Copilot
autostarts, and its Alt+Space binding lives in app state rather than a
registry key anyone can read, so the collision — if it comes — arrives at a
reboot or an app update, not today. If Alt+Space stops summoning YSpot
mid-fortnight, read the log before suspecting YSpot.

Optional, if YKeys should own the chord (§5.1 amended): Settings → "Let YKeys
hold the hotkey", then add the line it shows to `~/.config/ykeys/ykeys.json`.

Order matters here, and getting it wrong leaves the chord dead. The **running**
ykeys is the installed 0.1.4, started at logon by the YTile scheduled task, and
0.1.4 has no `@signal:` verb at all — but it reads the same config file. Add
the signal line while it is running and it takes Alt+Space and then does
nothing with it. So: stop the running one first, then start a build of `main`,
which understands the verb. That build already exists; no `dotnet publish`
needed unless you want a fresher one.

The simpler answer is to skip YKeys for the fortnight. Leave `hotkey_source`
at its default and let YSpot hold the chord itself — one less moving part in
the two weeks whose whole purpose is finding YSpot's own bugs.

## 4b. Start from a clean slate

Nothing here is required, but decide deliberately rather than by omission —
the fortnight's output is "did it crash and what did the log say", and old
data makes that harder to read.

The one that actually matters: `%LOCALAPPDATA%\YSpot\logs\shell.log` may hold
lines from earlier development builds, written before the `run-start` marker
existed. They will sit at the top of every merged read for two weeks, and they
carry warnings for bugs that are already fixed.

```
Remove-Item "$env:LOCALAPPDATA\YSpot\logs\*.log"
```

The databases are worth checking rather than assuming: dev sessions may leave
`clipboard.db` and `frecency.db` behind with no rows in them, in which case
they skew nothing and can stay. The icon cache and the WebView2 profile under
`%LOCALAPPDATA%\com.yspot.shell` are pure caches; deleting them costs one
slower first frame.

Do this with the shell **not** running.

## 5. Where everything lives

`%LOCALAPPDATA%\YSpot\`:

- `logs\shell.log` — structured JSON, rotated; the first thing to read.
  The service writes the matching `%ProgramData%\YSpot\logs\indexd.log`;
  both carry a `process` field, so concatenating them and sorting by `ts`
  gives one timeline across the pair.
- `settings.json` — hotkey, theme, consents, `hotkey_source`.
- `clipboard.db` — a plain SQLite file. Every clipboard **value** is sealed
  with DPAPI; the row metadata around it is not. Kind, source application,
  timestamp and a hash of the content are stored in the clear, so the file
  still reveals what you copied from and when, just not what.
  `frecency.db` is not encrypted at all: launch counts against app ids and
  `volume:frn` pairs. Both carry `-wal` and `-shm` sidecars.
- `icons\` — the extracted app-icon cache, a hundred or so PNGs. Rebuildable;
  delete it freely.
- `crashes\` — minidumps the shell writes itself, **only** if crash-report
  consent was given. Not WER: Windows reads the LocalDumps key only from
  `HKLM`, which the shell has no rights to write.

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
.\scripts\Read-YSpotLogs.ps1 -Simple -Pattern 'C:\Users\me\Documents'
.\scripts\Read-YSpotLogs.ps1 -Raw > logs.txt     # to attach to an issue
```

`-Pattern` is a regular expression; `-Simple` makes it literal text, which is
what you want for any path, because a Windows path is not a valid regex.

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
exact query or action. Note that only the shell writes dumps: a service crash
leaves its log line and nothing in `crashes\`, because §8.5 gives the service
WER LocalDumps and that is the MSI's job, which is later. "It felt slow" is a
bug too; say what you typed and roughly how long it took.

## 7. Stopping

Tray → Quit for the shell (the service keeps running, by design, §5.5).
Ctrl+C in the service window for the service.
