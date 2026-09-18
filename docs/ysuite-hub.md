# YSuite Settings — a hub for YKeys, YTile and YBar

A row in the launcher that opens one place to see and change the state of the
other three apps. The icon is already drawn (`yspot_suite_cube`); the row and
the view are not built.

Every claim below was read out of the three source trees and checked against
this machine. The first draft of this document was reviewed against those
trees and **nineteen of its statements were wrong** — install paths, an error
convention, a version gap, and a function that does not return what the
document said it returned. The corrections are folded in, and the ones that
changed the scope are called out where they land. If you are about to trust a
sentence here, the file and line it cites is the thing to trust.

## The shape of it

`YSuite Settings` is a built-in command (§7.6) that opens a view inside the
launcher, the way `yspot.settings` and `yspot.clipboard` already do.

One card per app, three tiers each:

1. **Status.** Installed (and where), running (and how we know), version,
   config path. Pure reads.
2. **Actions.** Things with no persistent state to corrupt.
3. **Settings.** Things written down somewhere. Deliberately thin.

A card whose app is **not installed** shows that and nothing else.

A card whose app is installed but **stopped** shows status, the actions that
make sense, and *any setting whose write is durable on its own*. Greyed out
only where the change needs a live process to take effect, and then with the
reason. Never a control that silently does nothing — **and never a disabled
control that would have worked.** YBar's theme is the worked example: it is
fully supported on a stopped bar, and the first draft's blanket "disable
settings when stopped" would have greyed out the one YBar setting that always
works.

## Detection

**Resolve the exe on `PATH`. Do not hard-code install directories.** Both
YTile and YBar ship through three channels, and the first draft named one or
two of them:

| app | channels | resolve |
|---|---|---|
| YTile | `install.ps1` per-user `%LOCALAPPDATA%\Programs\ytile` and all-user `%ProgramFiles%\ytile`; Scoop `%USERPROFILE%\scoop\shims`; winget `%LOCALAPPDATA%\Microsoft\WinGet\Links` | `ytile.exe` on PATH, those four as fallback |
| YBar | `install.ps1` `%LOCALAPPDATA%\Programs\ybar`; Scoop; winget | `ybar.exe` on PATH, that path as fallback |
| YKeys | ships *inside* YTile's release, beside `ytile.exe` | **the directory of the resolved `ytile.exe`** |

Every channel puts its directory on PATH by construction — `install.ps1` calls
`Add-ToUserPath`, Scoop uses shims, winget uses Links. `install.ps1` itself
probes for the winget copy to warn about a stale binary, which is the tree
admitting there are more channels than it installs.

Checking only the two `install.ps1` paths reports "YTile not installed" while
`\\.\pipe\ytile` is answering — a card contradicting itself — and takes the
YKeys card down with it, because YKeys is found *beside* `ytile.exe`.

Running:

| app | signal |
|---|---|
| YTile | `\\.\pipe\ytile` exists |
| YKeys | named semaphore `Local\ykeys-instance` opens |
| YBar | **connect** to `%LOCALAPPDATA%\ybar\ybar_<USERNAME>.sock` |

- **Never `WaitOne` the YKeys semaphore.** Taking the token makes the next real
  `ykeys` start print "another instance is already running" and exit 1. Open
  and close.
- **The YBar socket file existing is not proof.** A crashed daemon leaves it;
  the next start deletes and rebinds. This machine has a stale
  `ybartest_ogrus.sock` beside the live one. Attempt the connect.
- `\\.\pipe\ytile` is what YTile's own CLI checks, in six places, with the
  comment *the pipe, not the process — the pipe is what the next start
  collides with.* Same signal, same reason.

## YTile — real controls

**Config:** `%USERPROFILE%\.config\ytile\ytile.json`, JSON, camelCase,
`AllowDuplicateProperties = false`. No `--config` flag, no env override.

**YTile never writes it.** YSpot would be its sole writer.

**The file and its directory are often absent.** `YTileConfig.Load` treats
missing as a first-class silent state and runs on defaults. So "write a temp
file in the same directory and rename" throws `DirectoryNotFoundException` on
a fresh install: **create the directory first, and treat a missing file as an
empty document rather than an error.**

**Reload:** no watcher, nothing polls the mtime. A change is invisible until
`{"cmd":"reload"}`. Every write is two steps, and a hub doing only the first
appears to do nothing.

| control | mechanism | notes |
|---|---|---|
| Tiling on/off | `{"cmd":"pause"}` / `{"cmd":"resume"}` | Pipe only. Nothing persists. Reflect from `{"cmd":"state"}`. |
| Window gap | write `gap`, reload | **Clamp 0–64.** No validation in YTile. |
| Hide the taskbar | write `hideTaskbar`, reload | **Only while not paused** — see below. |
| Retile | `{"cmd":"retile"}` | Returns `ok:false, "paused — resume first"` while paused. |

**Paused is a third state, not a synonym for running.** `reload` runs
`ApplyTaskbarPolicy(managing: !_paused)` and skips `RetileAll()` when paused,
then answers `ok:true`. So with tiling paused — a switch on this same card —
writing `hideTaskbar` or `gap` and reloading reports success and changes
nothing, and the taskbar vanishes hours later when the user resumes. **Treat
paused the way "not running" is treated for these two settings**, with the
reason shown.

**`ok:false` from reload does not mean your write failed.** It is returned
whenever `configError` is non-null, including a pre-existing bad rule the user
has had for months. A hub that reports the reply verbatim tells someone their
gap change failed on every attempt, forever. Distinguish: read the config back
and check the value took.

**`ytile stop` takes YKeys down with it** — "sends stop to the daemon and
takes the bundled ykeys down with it". On this machine that unregisters all
**38** bindings, every one of them a `ytile` verb. The hub either passes
`--no-hotkeys` or says plainly what stopping will do.

**A malformed write is all-or-nothing.** Any duplicate key or bad JSON makes
`Load` discard the *entire* config and run on defaults: rules gone, gap back
to 8, and since `hideTaskbar` defaults false, the taskbar reappears. Parse,
mutate, serialise, preserve unknown keys, write atomically.

On `gap`, precisely: it is the one numeric field with no validation. A gap
over half the work area makes `Shrink` floor width and height at 0 and every
managed window collapses. A negative gap does not push windows off-screen in
the way the first draft claimed — `Shrink` *grows* the rect — but it is still
nonsense the daemon will not refuse. Clamp 0–64.

**Not in v1:** `defaultLayout` is read when a workspace is created, so writing
it changes nothing on monitors that already exist. `rules[]` deserves its own
design pass — YSpot knows the running processes and could offer a picker.

## YBar — three controls, and two traps

**Trap one: every `--bar` write is ephemeral.** On reload the daemon does
`settings = BarSettings{}` and re-runs the Lua from scratch. Height, position,
colour, padding, glass — all revert at the next reload, restart or logout. A
panel of those un-sets itself overnight. None are in v1.

**Trap two: the active config is usually not `~/.config/ybar`.** Discovery is
six steps, not four: `-c` flag, then the selected theme, then
`%XDG_CONFIG_HOME%\<name>\`, then `%USERPROFILE%\.config\<name>\`, then
`%USERPROFILE%\.ybarrc.lua`, then `%USERPROFILE%\.ybarrc`. This machine's
`current-theme` reads `sketchybar-glass`, so the live config is the theme's
`ybarrc.lua` under `%LOCALAPPDATA%\Programs\ybar\examples\`.

**And `ybar status` does not tell you the running config.** It computes that
field with `locateConfig(instance, "")` — a fresh-start *guess* — and prints a
note saying so. For a daemon started with `-c` or re-pointed by `--reload`, it
names a file the bar is not reading. **Do not wire Open-config to it while the
daemon is running.**

| control | mechanism | notes |
|---|---|---|
| Theme | `ybar theme list` / `theme use <name>` | Persistent. Writes `current-theme` **and** reloads in one verb. Works stopped. |
| Start with Windows | `ybar autostart enable\|disable\|status` | Gets the `ybarw.exe` no-console-flash logic for free. |
| Running | `ybar start` / `stop`, state from `ybar status` | |

**Error detection differs by verb.** A leading `[!]` is the *socket* reply
convention. The three local verbs the hub uses — `theme`, `autostart`,
`start/stop/status` — return before that path: judge them by **exit code**
(0 ok, 1 failed, 2 bad invocation), not by scanning for `[!]`.

**Refused in v1:** `hidden` is a one-way door with no in-bar way back.
`reserve` and `display` rewrite work-area state and break maximised windows
when wrong.

## YKeys — read-only, and the hand-off needs a version gate

No writes to `ykeys.json`. Any one of these suffices:

1. **Comments are load-bearing and stock parsers reject them.** The README
   sells commenting-out as how you park a binding, and this machine's file does
   exactly that. `JSON.parse` and `serde_json` both *throw* on it — so even the
   binding count this card is supposed to show needs a **JSONC reader**
   (comments skipped, trailing commas tolerated; `Config.cs:14-19` is the
   contract), which neither `package.json` nor `Cargo.toml` has today. Without
   one the card reports "unreadable" for a perfectly healthy 38-binding config.
2. **One duplicate key disarms the whole file.**
   `AllowDuplicateProperties = false` makes it a file-level parse failure. A
   running daemon keeps its live bindings and only logs, so the damage
   surfaces at the next reboot with nothing to explain it.
3. **It is the machine's hotkey config.** All 38 bindings here are `ytile`
   verbs — not 37, as the first draft said. The argument is stronger at 38/38.

The card shows: installed, running, config path, binding count, **Open
`ykeys.json`**, and — when the hotkey is handed to YKeys — the line to paste:

```json
"<chord>": "@signal:YSpot.Signal"
```

`ykeysChord()` produces **only the chord half**, and returns `null` when YKeys
has no spelling for that chord. The whole line exists today solely as a JSX
literal in `Settings.tsx:208`; lift it into the shared lib with a test before a
second caller renders it. A hub that pastes `ykeysChord()`'s return value alone
writes a bare token, which is not valid JSON, which is failure mode 2.

### The hand-off is inert on the released YKeys

**The scope change.** `SignalSender.cs` does not exist in **v0.1.4** — the
latest release, and the exact build YTile bundles (`release.yml` pins
`YKEYS_VERSION: '0.1.4'`). Verified on this machine: the installed
`ykeys.exe --version` reports `0.1.4` and its `--help` has no `signal` verb.

On that build `@signal:YSpot.Signal` is just a command line. `Config.Load`
accepts it, `RegisterHotKey` takes the chord, `Process.Start` throws into
`ykeys.log` — so the chord is held by a daemon that does nothing with it,
while YSpot has already unregistered its own. **Nothing summons the
launcher.**

So the one real control in this card — "Let YKeys hold the hotkey", which
already exists under that exact label in `Settings.tsx:199`, so do not invent
a second name — needs two guards the existing code does not have:

- **A version gate.** Offer it only when the resolved `ykeys.exe` reports a
  version whose signal mechanism exists. Below that, show why it is
  unavailable rather than a control that breaks the launcher.
- **A chord guard.** `registrar_action` matches `(_, Ykeys) => HandToYkeys`
  unconditionally and `save_settings` unregisters whatever we hold, with no
  check that the chord is even expressible in YKeys' vocabulary. A user on
  CapsLock or PrintScreen (`ykeysChord()` returns `null`) ticks the box,
  YSpot releases, no paste line can be offered, and nothing summons the
  launcher. That is the failure `ykeys.ts` was written to close.

`ykeys shell-hotkeys disable` is never a toggle: it writes a persistent
registry value that takes effect only when Explorer restarts, and
`--restart-shell` closes every File Explorer window. An explicit button with
that consequence spelled out, or nothing.

## Write discipline

For the one file YSpot writes (`ytile.json`) and any that follow.

- **Parse, mutate, serialise** — never template by hand.
- **Preserve unknown keys.** YSpot's own settings do this and have a test; the
  same standard applies to someone else's file.
- **Create the directory** — it may not exist.
- **Clamp every numeric** to a range the hub chooses, not the range the
  receiving app tolerates.
- **Write atomically** — temp file in the same directory, then rename.
- **Then reload, then read back.** The reply is not proof.
- **Never delete a config to reset it.**

## Failure, stated plainly

- **Not installed** — say so, offer nothing.
- **Installed, not running** — status and `start`. Settings needing a reload
  are disabled with the reason; settings whose write is self-contained (YBar's
  theme) stay enabled.
- **Running but paused** (YTile only) — `gap` and `hideTaskbar` disabled with
  the reason. Reload will claim success and do nothing.
- **Command failed** — surface the app's own text, but verify: YTile's
  `ok:false` may predate your write, and YBar's local verbs report by exit
  code rather than `[!]`.

One version note, corrected: `ybar-win` is **not** a separate repository. Both
trees are `NineFiveB/YBar.git`; the Windows port is a branch. Any update or
version check must not look for a repo that does not exist.

## v1 scope

**In:** three status cards; YTile pause/resume, gap (clamped), hideTaskbar,
retile, reload; YBar theme, autostart, start/stop; YKeys read-only; Open-config
and Open-folder on every card.

**Out:** everything marked ephemeral or dangerous; `rules[]`; `defaultLayout`;
any write to `ykeys.json`; anything needing elevation.

**Gated:** "Let YKeys hold the hotkey" ships only behind the version gate and
the chord guard. On a machine with the released YKeys — which is every machine
today — the card explains why it is unavailable. Shipping it ungated is
shipping a control whose effect is that the launcher stops opening.
