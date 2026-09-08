# AGENTS.md — how to work in this repo

Read this before writing anything. It is shared by every AI working here (Claude
Code, Antigravity/`agy`, and whatever comes next) and it is not style advice: the
rules below were each learned by shipping the mistake they forbid.

## What this is

**YSpot** — a Windows launcher (PowerToys Run / Raycast shaped). Rust workspace
plus a Tauri v2 shell with a React + TypeScript frontend.

```
crates/yspot-proto     wire types for the shell <-> service pipe
crates/yspot-pipe      one overlapped named-pipe primitive, both sides
crates/yspot-index     the filename index: MFT enumeration, USN tailing, matching
crates/yspot-indexd    the elevated index service (console mode in M1)
crates/yspot-log       SPEC §8.5's structured log, shared by both processes
apps/shell/src-tauri   the shell (Rust): hotkey, placement, actions, IPC
apps/shell/src         the frontend (React): the launcher UI and its views
```

`SPEC.md` is normative. `docs/M1.md` is the live plan and the record of every
ruling. `docs/DOGFOOD.md` is the runbook for the two-week exclusive-use test that
M1 exits on.

## The gate

Nothing ships without all four, run by you, in this order:

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cd apps/shell && npm test && npx tsc --noEmit && npm run build
```

CI runs the same. **Read the exit status.** Do not count passing lines: a failing
binary prints *fewer* "test result: ok" lines, so a grep-and-count once dropped
15 to 1 and reported success. `gh run watch --exit-status` also returns non-zero
while a run is merely still in progress — check `status` before calling CI red.

## The rules that matter

**1. Measure before you fix.** The most recent search bug looked exactly like a
noisy settings catalog. The catalog returned four rows against a cap of four and
was innocent; the real cause was 101 directories on disk named `Settings`
outranking the command. Fixing the guess would have shipped a change that did
nothing and a commit message that was a false account of it. Establish the number
first, then aim.

**2. Nothing may claim what the code cannot do.** No button that does nothing, no
subtitle that describes a behaviour the code does not have, no comment asserting
an invariant that is not enforced. When a capability is missing, say so in the UI
and record it in `docs/M1.md` — the launcher has several honest "this cannot act
yet" screens and they are correct as written.

**3. Probe every test you write.** Break the thing the test covers, confirm the
test fails, restore. A test that cannot fail is worse than no test because it
reports success. Two real examples from this repo: a retry test that passed in
128 ms against a 5-second timer, and a test that asserted a filename it had
itself written ended in `.log`.

**4. Amend `SPEC.md` deliberately, never silently.** The convention is inline:
`(**Amended YYYY-MM-DD**, superseding "<old text>". <why, and what it costs>.)`
State the cost. If a change contradicts the spec and you do not amend it, the
spec is now wrong and nobody knows.

**5. Two processes, one log format.** `crates/yspot-log` owns the line format for
both. Do not add a second logger or a second format. Both write one JSON object
per line with a `process` field; `scripts/Read-YSpotLogs.ps1` merges them.

**6. Windows PowerShell 5.1 is the default shell on this machine.** Two fixes in
two consecutive commits were correct only on PowerShell 7, because that is where
they were tested. `&&` is a parse error on 5.1. `String.Contains(value, comparison)`
does not exist there. `ConvertFrom-Json` leaves timestamps as strings there and
returns `DateTime` on 7. Test both hosts:
`C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe` and `pwsh`.

**7. Never run anything that synthesizes input while the user is at the keyboard.**
`yspot-m0 toggle` and `yspot-m0 type` use `SendInput`. The same applies to the
product's own injection paths.

**8. Do not run release builds unasked.** They take minutes and the user asked
for that decision to stay theirs.

## Conventions

- **Comments say why, not what.** A comment that restates the line above it is
  noise; one that names the bug the line prevents is the reason the line survives
  a refactor. Cite the spec section (`§5.11`) when a rule comes from it.
- **Commit messages are prose, not bullet lists.** Say what was wrong, what the
  consequence was, and what changed. They are the design record here — several of
  this repo's hardest decisions exist only in them.
- **Tests are named as sentences** describing the property, not `test_foo_2`.
- **No new dependencies without justification.** The shell ships offline. The
  frontend has no icon library; icons are hand-authored inline SVG using
  `currentColor` so both themes and `forced-colors: active` are correct.
- **Frontend never writes files or touches the registry.** Every mutation goes
  through a Tauri command in the shell (§5.9).
- **Latency is a contract.** §2.5 budgets the service at 10 ms and the shell's
  routing at 3 ms. Nothing goes on the query hot path without a measurement.

## Working with the index and the service

The service needs an elevated prompt for the MFT/USN handle. It runs as a console
process in M1 — there is no MSI yet. `--walk <path>` is the unelevated dev mode
and needs no privileges.

`compact()` in `crates/yspot-index` renumbers about ten parallel structures **in
place** to avoid a 42 MB transient. A panic partway leaves the index internally
inconsistent, which is why the service exits rather than continuing — see
`compact_or_die`. Do not "improve" that into a log-and-continue.

## If you are `agy`

You are the executor. Claude wrote the spec and owns verification.

- Work only inside the scope you were given. Do not refactor adjacent code.
- Follow the rules above, especially 2 and 3.
- Do not modify the environment to make a check pass — no patching installed
  packages, no stubbing a missing dependency. If something does not build, say so.
- End with a fenced `===DIGEST===` block: files changed, key decisions, and one
  paragraph of context for the next step. Put bulky detail in files, not in the
  reply.
- Your "green" is a claim, not evidence. It will be re-run.
