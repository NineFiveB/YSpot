# YSpot — Engineering Specification

**Version 0.1 · 2026-08-29 · Status: draft, pre-implementation**

YSpot is a keyboard-first launcher and command palette for Windows 10 (1809+) and Windows 11. This document is the normative engineering specification: architecture, indexer service, IPC protocol, shell/frontend, extension platform, built-in commands, security model, packaging, and milestones. Normative keywords (MUST/SHOULD/MAY) follow RFC 2119 usage.

This spec was produced from verified research into Raycast's architecture (including its Windows 2.0 release), PowerToys Run/Command Palette internals, and the proven NTFS MFT + USN journal indexing techniques (Everything, ultrasearch), followed by an adversarial multi-reviewer consistency pass.

## Table of contents

1. [Vision, Goals, and Positioning](#1-vision-goals-and-positioning)
2. [System Architecture](#2-system-architecture)
3. [Indexer Service (yspot-indexd)](#3-indexer-service-yspot-indexd)
4. [IPC Protocol](#4-ipc-protocol)
5. [Shell and Frontend](#5-shell-and-frontend)
6. [Extension Platform](#6-extension-platform)
7. [Built-in Commands and Windows Integration](#7-built-in-commands-and-windows-integration)
8. [Security Model](#8-security-model)
9. [Packaging, Installation, and Updates](#9-packaging-installation-and-updates)
10. [Milestones](#10-milestones)
11. [Risks and Open Questions](#11-risks-and-open-questions)

---
## 1. Vision, Goals, and Positioning

### 1.1 Product statement

YSpot is a keyboard-first launcher and command palette for Windows 10 (1809+) and Windows 11 that combines Raycast-class UX and an isolated TypeScript/React extension platform with search infrastructure Windows itself does not provide: a first-party Rust indexing service that enumerates the NTFS Master File Table and tails the USN journal for instant filename search across every local NTFS volume, plus an opt-in Tantivy full-text content index. It is built Windows-first — five cooperating processes (elevated Rust index service, sandboxed `yspot-extract` content-extraction worker, unelevated Tauri shell, React frontend in WebView2, Node.js extension host) — not a port, not Electron, and fully functional without a cloud account.

### 1.2 Goals

1. **Instant, always-ready UI.** Global hotkey to visible window in < 50 ms; results data available to the frontend ≤ 20 ms (p95) after keydown, applied in the next rAF after arrival (budget decomposition owned by §2.5; end-to-end pixels add up to two vsync periods on top — ~33 ms at 60 Hz, ~16 ms at 120 Hz). The popup takes focus only in direct response to user invocation and MUST restore the previously focused window on dismissal (§5.2).
2. **Complete local file search.** Initial filename index of a 1M-file NTFS volume in < 15 s on the SSD reference machine with quiescent disk (relaxed HDD target: 90 s; reference machines defined in §10 M0); file-system change (USN event) searchable in < 1 s; substring, fuzzy, and camel-case matching with frecency ranking — not prefix-only.
3. **First-party content search.** Full-text search over user-selected scopes via an embedded Tantivy index, independent of the Windows Search service.
4. **Deep Windows coverage.** Launch UWP and Win32 apps, every `ms-settings:` page in the catalog, Control Panel tasks; window management; clipboard history.
5. **Safe extensibility.** TypeScript/React extensions in an out-of-process Node host, one worker thread per extension with memory limits; an extension crash MUST render an error card and MUST NOT crash or stall the shell.
6. **Frugal residency.** Idle RAM: shell + frontend < 150 MB. Index service budget lines: filename index ≤ 200 MB per 1M files; content pipeline steady-state ≤ 150 MB with merge transients ≤ 300 MB; caches ≤ 50 MB (LRU-evicted). The `yspot-extract` content-extraction worker is a separate process outside the service budget, RSS ≤ 200 MB.
7. **Minimal elevated surface.** Only `yspot-indexd` runs elevated; it parses raw volume structures and MUST be treated as the security-critical component (least privilege, ACL'd named pipes, no script execution). Hostile file-format parsing (PDF, OOXML, …) runs in the sandboxed, write-restricted `yspot-extract` worker process (§2.2, §8.1), never in the elevated service.

### 1.3 Non-goals

- **No macOS or Linux support.** Windows-first is the product, not a phase.
- **No Electron.** The shell is Tauri v2 over the system WebView2; the launcher window, hotkeys, and tray are native Rust/Win32.
- **No arbitrary HTML/CSS/DOM from extensions.** Extensions declare UI from a fixed component library (List, Detail, Form, Action, …); the frontend renders the serialized tree. Ever.
- **No cloud account requirement for core features.** Launcher, file search, content search, settings/app search, clipboard history, and local extensions work fully offline. Accounts MAY gate optional sync/store features only.
- **No content indexing by default.** Full-text scopes are strictly opt-in (privacy and disk/RAM cost).
- **No replacing Windows Search system-wide.** YSpot does not register as a shell search provider or modify Explorer; non-NTFS/network volumes fall back to Windows Search via OleDB — implemented in the unelevated shell, in the correct user context — rather than getting a custom crawler (in v1).
- **No general-purpose automation runtime.** YSpot is a launcher with extensions, not a scripting/macro platform.

### 1.4 Competitive positioning

| | **YSpot** | Raycast for Windows | PowerToys Run | PowerToys Command Palette | Flow Launcher | Everything | Listary |
|---|---|---|---|---|---|---|---|
| **Indexing approach** | Own elevated Rust service: NTFS MFT enumeration + USN tailing, all NTFS volumes; shell-side Windows Search (OleDB) fallback for non-NTFS/network | Own Rust MFT indexer, filename/metadata only | Windows Search SystemIndex via OleDB (`Search.CollatorDSO`); completeness hostage to Classic vs Enhanced mode | No own file index; Windows Search / app catalog; Everything via extension | No own index; delegates to Windows Index or Everything | Own MFT + USN index; NTFS-focused, instant | Own index reading NTFS metadata directly; real-time updates; non-NTFS added manually |
| **Matching quality** | Substring + fuzzy + camel-case, frecency ranking | Fuzzy + frecency for commands/apps; file matching on own index | Prefix-oriented; no good substring/fuzzy | Fuzzy for commands; file results inherit Windows Search behavior | Good fuzzy for commands/apps; files depend on backend | Best-in-class filename substring/wildcard/regex; not a frecency launcher | Fuzzy + substring; learns frequent items |
| **Content search** | **First-party Tantivy full-text, opt-in scopes, USN-incremental** | None of its own; delegates to Windows Search | Only what Windows Search has content-indexed | Same — Windows Search | Via Windows Index backend only | `content:` is unindexed/slow; optional 1.5 content index is in-RAM, meant for ~≤1 GB of text | None (filename/metadata only) |
| **Extension model + isolation** | TS/React declarative components; out-of-proc Node host, one worker per extension, memory limits, JSON-RPC; crash-isolated | Same model (TS/React, isolated Node workers) — YSpot matches it | In-process .NET plugin assemblies; no isolation | Out-of-proc WinRT/COM extensions (MSIX, .NET SDK); process-isolated but native-code, no resource limits, no web-tech UI | .NET plugins in-process; Python/Node via JSON-RPC out-of-proc; mixed isolation | No extension platform (external query SDK over `WM_COPYDATA` only) | No extension platform; user keywords/custom commands |
| **OS integration depth** | Apps (UWP+Win32), full `ms-settings:` catalog, Control Panel tasks, window management, clipboard history, every NTFS file | Growing port of macOS feature set; Windows coverage incomplete in beta | Apps, shell/system commands, plugin set | PT Run successor: apps, files, calculator, settings; WinGet extension gallery | Apps, web search, large plugin ecosystem | File search only (opens files; ETP/HTTP servers) | Deep Explorer + open/save file-dialog integration (its signature); launcher basics |

### 1.5 Differentiators as testable claims

- **D1 — First-party full-text content search.** With the Windows Search service (`WSearch`) stopped or disabled, a query in any opt-in content scope MUST still return matching documents, ranked, from YSpot's own Tantivy index; a small text document saved into an indexed scope MUST be findable by its new content within 10 s. No competitor in §1.4 passes this test with its own index at normal working-set sizes.
- **D2 — Windows-first depth.** On a clean SSD reference machine (§10 M0) with a 1M-file NTFS volume and quiescent disk: initial filename index completes in < 15 s (relaxed HDD target: 90 s); any file is findable by mid-string substring; a rename is searchable under its new name in < 1 s (USN); every page in the shipped `ms-settings:` catalog and both UWP and Win32 apps launch from the palette. PowerToys Run fails the substring and completeness tests (Windows Search dependency); Everything and Listary pass file tests but have no settings/window/clipboard surface.
- **D3 — Process-isolated web-tech extensions on Windows.** A hostile test extension (infinite loop, unbounded allocation, or thrown exception) MUST produce only an error card: shell input latency stays within the §1.2 budgets, other extensions keep running, and no extension can inject HTML/CSS/DOM — only declarative component trees. PowerToys Run cannot pass (in-process assemblies); Command Palette isolates processes but offers neither TS/React + npm authoring nor per-extension memory limits.

Competitor characterizations above are sourced from vendor docs and project documentation, including [voidtools index docs](https://www.voidtools.com/en-us/support/everything/indexes/), [Listary docs](https://help.listary.com/options-index), [Flow Launcher JSON-RPC docs](https://github.com/Flow-Launcher/docs/blob/main/json-rpc.md), and [PowerToys Command Palette extension docs](https://deepwiki.com/microsoft/PowerToys/7.5-creating-command-palette-extensions).

## 2. System Architecture

### 2.1 Process model

YSpot is five cooperating processes split across one trust boundary. Nothing elevated ever renders UI or runs third-party code; nothing that runs third-party code can touch a raw volume handle; and the code that parses hostile document formats holds neither privilege — it runs write-restricted, forbidden from creating child processes.

```
            SESSION 0 — SERVICE SIDE OF THE TRUST BOUNDARY
┌────────────────────────────────────────────────────────────┐
│ yspot-indexd        Rust Windows service (LocalSystem)     │
│  MFT enumeration (FSCTL_ENUM_USN_DATA) per NTFS volume     │
│  USN journal tailing (FSCTL_READ_USN_JOURNAL)              │
│  In-memory filename index (substring/fuzzy/camel)          │
│  Tantivy full-text content index (opt-in scopes)           │
└──────────▲──────────────────────────┬──────────────────────┘
           │                          │ spawns: write-restricted
           │                          │ token + job object,
           │                          │ no child processes
           │                          ▼
           │              ┌──────────────────────────────────┐
           │              │ yspot-extract  content extractor │
           │              │  hostile-format parsers          │
           │              │  (PDF/OOXML/…)                   │
           │              │  input: duplicated read-only     │
           │              │  handles (never paths)           │
           │              │  output: anonymous pipe,         │
           │              │  frame-capped per §4.2           │
           │              └──────────────────────────────────┘
           │  \\.\pipe\yspot.indexd.v1 — ACL'd named pipe,
           │  JSON-RPC, streamed result batches
═══════════╪═════════════════ trust boundary ════════════════
           │  USER SESSION — UNELEVATED (medium IL)
┌──────────┴─────────────────────────────────┐
│ yspot (shell)           Tauri v2, Rust     │
│  global hotkey · WS_POPUP + WS_EX_TOPMOST  │
│   + WS_EX_TOOLWINDOW popup (focus §5.2)    │
│  tray · autostart · monitor placement      │
│  app/settings/command catalogs             │
│  per-user frecency store · global ranker   │
│  Windows Search OleDB fallback (non-NTFS)  │
│  IPC broker · sole permission enforcement  │
└────▲───────────────────────────▲───────────┘
     │ Tauri IPC bridge          │ JSON-RPC over stdio/pipe
┌────┴────────────────┐   ┌──────┴──────────────────────────┐
│ Frontend (WebView2) │   │ yspot-exthost   bundled Node.js │
│  React + TS UI      │   │  1 worker thread (v8 isolate)   │
│  virtualized list   │   │  per extension, memory-capped   │
│  renders extension  │◄──┼─ declarative component trees    │
│  component trees    │   │  (JSON render tree + patches)   │
└─────────────────────┘   └─────────────────────────────────┘
```

### 2.2 Responsibilities and trust levels

| Process | Runs as | Trust | Responsibilities |
|---|---|---|---|
| `yspot-indexd` | LocalSystem service, Session 0 | Highest privilege, most hardened | Open `\\.\X:` volume handles; enumerate the MFT via `FSCTL_ENUM_USN_DATA`; tail change journals via `FSCTL_READ_USN_JOURNAL`; own the filename and Tantivy content indexes; spawn and supervise the `yspot-extract` worker; answer queries over one named pipe. Answers non-NTFS/network scopes with error `107 SCOPE_UNSUPPORTED` (§4.4) — the shell routes those scopes to its own OleDB provider. |
| `yspot-extract` | Spawned by the service via `CreateProcessAsUser` with a write-restricted token, inside a job object, Session 0 | Sandboxed hostile-input parser | Parse content-extraction formats (PDF, OOXML, …) for the content pipeline. Receives files only as duplicated read-only handles (never by path); returns extracted text over an anonymous pipe subject to the same frame-cap rules as the main pipe (§4.2). `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY` (no child processes) is applied by the service at creation. RSS budget ≤ 200 MB — a separate process, outside the service's memory budget. Its format parsers are mandatory cargo-fuzz targets (§8). |
| `yspot` (shell) | Interactive user, medium IL | Trusted first-party, unelevated | Global hotkey; show/hide the popup (activate-on-summon, restore-previous-foreground-on-dismiss, §5.2); tray, autostart, monitor placement; host WebView2 warm from login; own app/settings/Control Panel catalogs; own the per-user frecency store and the global ranker (§3.4, §5.11); run the Windows Search OleDB fallback (`Search.CollatorDSO`) for non-NTFS/network scopes in the correct user context; broker all IPC and act as the sole authoritative permission-enforcement point for extension `api.invoke` (§4.7, §8.2). Never parses volume structures. |
| Frontend | WebView2 renderer processes | Sandboxed renderer | Launcher UI, keyboard handling, virtualized results, extension component-tree rendering. No Node, no filesystem — only the Tauri IPC bridge. |
| `yspot-exthost` | Child of shell, same user | Least trusted — runs third-party code | One worker thread per extension with a memory limit; extension lifecycle; serializes declarative UI to the frontend via the shell. No direct pipe to the service — every capability is brokered by the shell, which validates each `api.invoke` against the per-extension grant table (§4.7); exthost-side checks are fast-fail UX only and carry no security weight. |

The service MUST expose no network endpoints, MUST deny the NETWORK SID on its pipe ACL (SYSTEM, Administrators, and INTERACTIVE-logon users only — the `(A;;0x12019B;;;IU)` ACE in §4.1), and MUST treat every pipe message as untrusted input (strict schema validation, bounded allocations).

### 2.3 Why each boundary exists

- **Service / everything else — elevation isolation.** Raw volume handles require administrator rights; parsing attacker-influencible on-disk structures as SYSTEM is the riskiest code in the product. It is confined to one Rust process with no UI, no scripting, no network, one pipe. A shell or extension compromise yields no elevation: the pipe speaks only a query protocol.
- **Service / extractor — hostile-format isolation.** Document formats (PDF, OOXML) are attacker-supplied input; their parsers never run inside the SYSTEM process. `yspot-extract` runs under a write-restricted token in a job object, cannot create child processes, and cannot open files by path — a parser exploit lands in a process with nothing worth having.
- **Shell / exthost — crash and resource isolation.** Third-party JS cannot take down the launcher. A leaking or crashing extension kills its worker (or the whole exthost) — the hotkey, window, and first-party results survive. This is the differentiator over PowerToys' in-process .NET plugins. Worker isolation is a robustness boundary, not a security boundary — the security boundary is the shell's `api.invoke` validation (§8.2).
- **Shell / frontend — keep-warm and sandboxing.** WebView2 is initialized at login and kept alive hidden, so the hotkey path is show-window-only (< 50 ms budget) with zero browser startup cost. Chromium's renderer sandbox also contains any UI-layer exploit.
- **Service / sessions.** One machine-wide index serves every user session (fast user switching); shells are per-session clients, so the expensive index is built once. Content scopes, exclusions, frecency, and security trimming are per-user (§3 multi-user semantics).

### 2.4 Process lifecycle

1. **Boot:** SCM starts `yspot-indexd` (auto-start). Warm start: load the persisted index snapshot, then replay USN records from the persisted per-volume USN cursor. Cold start (no snapshot, or journal wrap/`UsnJournalID` change): full MFT re-enumeration at **normal I/O priority** (only the VeryLow per-handle hint is applied); background I/O priority applies only to rebuilds performed behind an existing serving index (§3.6). The cold scan MUST complete within 15 s per 1M files on the SSD reference machine with quiescent disk; relaxed HDD target 90 s (reference machines defined in §10 M0).
2. **Login:** the shell starts via per-user autostart (Run key written by the shell on first run, §9), registers the global hotkey, creates the hidden `WS_POPUP + WS_EX_TOPMOST + WS_EX_TOOLWINDOW` popup window (focus model per §5.2), initializes WebView2 and loads the frontend immediately, and connects to the service pipe. Ready state (hotkey→visible < 50 ms) MUST be reached within 3 s of process start.
3. **Exthost:** `yspot-exthost` is spawned on the first extension invocation and stays resident afterward; individual extension workers start on demand. When any enabled extension declares `rootSearch` in its manifest (§6.1), spawning at shell idle is **mandatory** — the exthost MUST be resident before the first keystroke can fan out to it (§4.7).
4. **Content extraction:** the service spawns `yspot-extract` when the extraction queue is non-empty — via `CreateProcessAsUser` with a write-restricted token, inside a job object, with `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY` applied at creation. The worker stays resident while the backlog drains and is torn down when idle; crash/hang handling per §2.6.

### 2.5 Data flow: one keystroke

The normative latency budget: **results data available to the frontend ≤ 20 ms (p95) after keydown; applied in the next rAF after arrival.** This table is the single normative decomposition; §5.7 references it and MUST NOT restate different numbers:

| Stage | Budget (p95) |
|---|---|
| Input + frontend dispatch (keydown → `query` on the Tauri bridge) | ≤ 2 ms |
| Shell routing (fan-out, generation bookkeeping) | ≤ 3 ms |
| Service first batch (in-memory match + rank + trimming) | ≤ 10 ms |
| — of which security trimming: AccessCheck bounded to the returned page (≤ 32 rows) against the per-SID directory-grant cache (§3.8) | ≤ 2 ms |
| Pipe transfer + deserialize | ≤ 5 ms |
| **Total — results data available to the frontend** | **≤ 20 ms** |

End-to-end *pixels* are the budget plus up to two vsync periods (rAF coalescing + compositor present): ≈ 33 ms at 60 Hz, ≈ 16 ms at 120 Hz — stated here so no section claims keydown→pixels < 20 ms. Measurement endpoints for the CI harness: injected keydown timestamp → DWM present of the updated frame (ETW `Microsoft-Windows-DWM` present events); the harness runs on reference Machine B when that hardware exists (deferred by the §10 Machine B amendment — the M0 numbers were taken on Machine A).

1. Keydown lands in the WebView2 input field; the React handler fires and sends `query{text, generation}` over the Tauri IPC bridge; `generation` is a monotonic counter.
2. Shell fans the query out: to `yspot-indexd` over the named pipe for files; to its own in-process catalogs for apps/settings/commands; to its own OleDB provider for any scope the service answers with `107 SCOPE_UNSUPPORTED`; and to the exthost for extensions that declare `rootSearch` (§4.7 — 150 ms hard deadline; late results append per the §5.11 merge contract or are dropped).
3. Service cancels any in-flight query with an older generation for that client, runs the in-memory match and ranks by pure `match_quality` plus depth penalty (frecency is applied shell-side, §3.4), trims the returned page via AccessCheck (§3.8), and writes the first batch (top ~32 hits) to the pipe within 10 ms. Further batches stream strictly rank-descending, so cross-batch application is append-only (§5.11).
4. Shell applies per-user frecency in its global ranker, merges sources under the §5.11 merge/selection contract (the selected row never moves on seq append), and forwards batches to the frontend as events tagged with `generation`.
5. Frontend discards batches whose generation is stale and applies the surviving batch in the next rAF, patching the virtualized list.

Only the newest generation per client is ever serviced; superseded queries MUST be cancelled, not queued.

### 2.6 Failure and recovery

| Dies | Blast radius | Recovery |
|---|---|---|
| `yspot-indexd` | File search only | SCM recovery actions restart it (1 s / 5 s / 30 s). Warm restore = snapshot + USN replay from cursor; worst case = cold rebuild within the §2.4 budget. The shell reconnects with exponential backoff and meanwhile serves apps/settings/commands normally, showing an inline "file index restarting" notice. |
| `yspot-extract` | Content extraction only | Hang or crash → the service kills the worker after the 10 s timeout, **quarantines the offending file (never retried)**, and restarts the worker. Filename search and already-committed content results are unaffected. |
| `yspot` (shell) | Everything user-visible (frontend and exthost die with it) | The shell registers with `RegisterApplicationRestart` so WER relaunches it after a crash (note: Windows only restarts apps that ran ≥ 60 s); autostart covers the next login. The per-user frecency store and settings are persisted on write, so nothing is lost but the in-progress query. |
| WebView2 renderer | UI goes blank; shell survives | Shell handles `CoreWebView2.ProcessFailed`: on render-process exit it calls Reload; on browser-process exit it recreates the controller, then re-pushes UI state. The popup shows a minimal native "reloading" state during the gap. |
| One extension worker | That extension only | Worker thread dies at its isolate; frontend shows an error card in place of the extension's UI (retry action). Other extensions and all first-party results are unaffected. Per-extension crash-loop breaker: 5 crashes / 10 min → extension disabled (thresholds normative in §6.4). |
| `yspot-exthost` | All extensions | Shell detects child exit, marks extension results unavailable, and respawns the exthost lazily on the next extension invocation (eagerly at idle if any enabled extension declares `rootSearch`, §2.4), with a process crash-loop breaker: 3 crashes / 60 s → all extensions disabled until manual re-enable (thresholds normative in §6.4). |

### 2.7 Technology choices

- **Rust (service + shell core + extractor):** memory-safe parsing of raw NTFS/USN structures in the one elevated process — and of hostile document formats in the write-restricted `yspot-extract` worker — with the throughput to meet the §2.4 cold-scan budget.
- **Tauri v2 (shell):** native Rust window/hotkey/tray control (including raw Win32 styles like `WS_EX_TOPMOST` and `WS_EX_TOOLWINDOW` via the `windows` crate), sidecar management, and system-WebView2 rendering — Raycast-class footprint that Electron cannot hit.
- **WebView2 (frontend host):** the evergreen Chromium runtime — preinstalled on Windows 11 only; on Windows 10 it was rolled out to consumer devices but is absent from enterprise/LTSC/clean 1809 images. The installer MUST detect a missing or too-old Evergreen Runtime and run the bundled Evergreen Bootstrapper (§9.1); portable mode performs the same check at first run (§9.5). In exchange: React/TS UI velocity and renderer sandboxing for free.
- **Bundled Node.js (exthost):** the npm ecosystem for extension authors, `worker_threads` for per-extension v8 isolates with memory limits, and a pinned runtime version independent of whatever is on the user's machine.
- **Named pipes + JSON-RPC:** pipes are the native Windows local-IPC primitive with real ACLs for the trust boundary; JSON-RPC gives request/response, cancellation, and streamed notifications over any duplex byte channel with one framing.

## 3. Indexer Service (yspot-indexd)

`yspot-indexd` is a Rust Windows service running as `LocalSystem`, installed by the elevated service MSI (§9.1) and started at boot (`SERVICE_AUTO_START`, delayed). It owns all index state and answers queries from per-session shells over an ACL'd named pipe (§4.1). Because it parses raw volume structures with SYSTEM rights, it MUST contain no networking code, MUST NOT load extension code, and MUST treat all pipe input as untrusted (length-checked, versioned frames).

### 3.1 Volume discovery and per-volume strategy

- Enumerate volumes with `FindFirstVolumeW`/`FindNextVolumeW`; resolve mount points via `GetVolumePathNamesForVolumeNameW`; classify with `GetDriveTypeW` and `GetVolumeInformationW` (file-system name).
- **NTFS fixed volumes** (`DRIVE_FIXED`, fs == `NTFS`): full custom index — MFT enumeration + USN tailing (§3.2–3.3). ReFS is out of scope for v1 and falls through to the passthrough path below (error `107` → shell-side Windows Search).
- **NTFS removable volumes**: indexed on arrival, index discarded on removal (no snapshot). Off by default; per-volume opt-in.
- **exFAT/FAT32/network volumes** (`DRIVE_REMOTE` or non-NTFS): no custom index, and **no Windows Search passthrough in the service**. The service answers queries scoped to such volumes with error `107 SCOPE_UNSUPPORTED` (§4.4), and the **unelevated shell** routes those scopes to its own Windows Search `SystemIndex` passthrough (OLE DB provider `Search.CollatorDSO`, `ISearchCatalogManager("SystemIndex")` → `ISearchQueryHelper::GenerateSQLFromUserQuery`), running in the user's own context — correct per-logon drive-letter mappings, user-identity authentication to SMB, and Windows Search's own per-caller security trimming, none of which work from a `LocalSystem` Session-0 process. Portable mode (§9.5) uses the identical shell-side code path. If the `WSearch` service is disabled, the shell MUST degrade passthrough scopes to "not searchable" with a visible hint, never to a silent empty result. Keeping the OleDB/COM query stack out of the SYSTEM process also shrinks the elevated attack surface (§8.1).
- Volume arrival/removal: the service registers `RegisterDeviceNotificationW(..., DEVICE_NOTIFY_SERVICE_HANDLE)` and handles `SERVICE_CONTROL_DEVICEEVENT` (`DBT_DEVICEARRIVAL`/`DBT_DEVICEREMOVECOMPLETE`, `DBT_DEVTYP_VOLUME`).

### 3.2 Initial filename enumeration (NTFS)

The initial scan MUST use `DeviceIoControl(FSCTL_ENUM_USN_DATA)` — the documented, filesystem-maintained way to walk all in-use MFT records — not a hand-rolled `$MFT` parser. Direct DASD parsing of `$MFT` is faster in theory but undocumented, fragile across NTFS revisions, and a needless attack surface in an elevated process; it MAY be revisited behind a build flag only if the enumeration budget (§1.2, §10 M0: **15 s per 1M files on the SSD reference machine (Machine A), quiescent disk; relaxed HDD target 90 s (Machine B)**) is missed.

- Open the volume handle with `CreateFileW("\\\\.\\X:", GENERIC_READ, FILE_SHARE_READ|FILE_SHARE_WRITE, ..., FILE_FLAG_BACKUP_SEMANTICS, ...)`. Opening a volume handle for read is denied to standard users; in practice it requires membership in Administrators or Backup Operators. Running as `LocalSystem` satisfies this and provides `SeBackupPrivilege` (the service SHOULD explicitly enable it via `AdjustTokenPrivileges` before volume opens). This privilege requirement is the reason the indexer is a service at all.
- Loop `FSCTL_ENUM_USN_DATA` with `MFT_ENUM_DATA_V0` (`StartFileReferenceNumber = 0` on the first call; each call's output begins with the next start FRN), a ≥1 MiB output buffer, parsing `USN_RECORD_V2` entries: FRN (64-bit), parent FRN, `FileAttributes`, `FileName`. NTFS 64-bit FRNs suffice; `MFT_ENUM_DATA_V1`/`USN_RECORD_V3` (128-bit IDs) is only needed for ReFS and is not used in v1.
- Before enumerating, call `FSCTL_QUERY_USN_JOURNAL` to capture `UsnJournalID` and `NextUsn`; if it fails with `ERROR_JOURNAL_NOT_ACTIVE` (1179), create the journal with `FSCTL_CREATE_USN_JOURNAL` (`MaximumSize` ≥ 64 MiB, `AllocationDelta` 8 MiB SHOULD be requested). The captured `NextUsn` is the tail-start cursor, so changes during the scan are not lost.

### 3.3 USN journal tailing

- Tail with `DeviceIoControl(FSCTL_READ_USN_JOURNAL)` using `READ_USN_JOURNAL_DATA_V0` (`StartUsn` = cursor, `ReasonMask` = create/delete/rename/close/basic-info/hard-link reasons **plus `USN_REASON_SECURITY_CHANGE`** (feeds the §3.8 cache invalidation), `BytesToWaitFor` = 1 and `Timeout` = 0: the call blocks until at least `BytesToWaitFor` bytes of unfiltered journal data are added — no polling. `BytesToWaitFor` counts *bytes*, so keep it small (1); both fields are ignored on asynchronously opened handles, so the tailing handle is opened synchronously on a dedicated thread). Persist the cursor (`UsnJournalID`, last USN) after each applied batch.
- Apply `USN_REASON_FILE_CREATE`, `USN_REASON_FILE_DELETE`, `USN_REASON_RENAME_OLD_NAME`/`USN_REASON_RENAME_NEW_NAME` (paired by FRN), and attribute/basic-info changes. Because paths are derived from the parent-FRN chain (§3.4), a directory rename updates every descendant's path implicitly — no subtree rewrite. `USN_REASON_SECURITY_CHANGE` records modify no index entry; they invalidate the affected directory subtree's entries in the per-SID directory-grant cache (§3.8), so a tightened ACL cannot keep serving stale cached grants.
- **Wrap/truncation recovery (mandatory):** if the stored `UsnJournalID` differs from the current one, or `FSCTL_READ_USN_JOURNAL` fails with `ERROR_JOURNAL_ENTRY_DELETED` (1181) (cursor older than `FirstUsn` — records overwritten), the volume index is stale and MUST be rebuilt by full re-enumeration (§3.2). On `ERROR_JOURNAL_DELETE_IN_PROGRESS` (1178), wait for deletion to finish, recreate the journal, then re-enumerate. USN event → searchable MUST meet the < 1 s budget at p95.

### 3.4 In-memory filename index

- **Layout:** one entry per file/dir: `{ frn: u64, parent_frn: u64, name_off: u32, folded_off: u32, name_len: u16, folded_len: u16, flags: u16 }` (32 B with padding — no usage data lives in the service; see Ranking below). Slots are STABLE: a delete tombstones its entry (`DEAD` flag) and returns the slot to a freelist rather than swapping another entry into it, because every derived column is keyed by slot and a permutation would invalidate all of them at once. Ranking depth and the segment initials live in their own per-slot columns rather than in the entry, so the ranking loop streams a 1 B/entry array instead of missing into a 32 MB table, names in a contiguous arena (original-case UTF-8 plus a case-folded shadow arena for matching). Full paths are never stored; they are reconstructed by walking `parent_frn` to the volume root on demand (path compression via the FRN tree). An FRN→entry hashmap supports USN application. Entries store **no size or mtime** — `USN_RECORD_V2` provides neither; `size`/`mtime` in `SearchResults` items are populated by lazily stat-ing only the returned page (≤ 32 files) before send, and v1 `SearchQuery` carries no size/mtime filters (§4.3). Every result item carries the opaque stable id `{volume_idx: u32, frn: u64}` (§4.3), which the entry provides for free.
- **Unicode:** arena names are normalized to **NFC**; the shadow arena and the incoming query both additionally get Unicode **simple case folding** (ICU4X `icu_normalizer` + `icu_casemap`; the Unicode version — 16.0 at time of writing — is pinned as a build constant and bumped deliberately), so NFC/NFD spellings of the same name match and folding is consistent between arena and query. CJK and other unsegmented scripts match via the substring tier — that is the documented, supported path; the case/camel/word-boundary tiers do not apply to them.
- **Memory target:** ≤ 120 MB per 1M entries typical, 200 MB hard cap (per budget) — the cap **includes** the candidate-generation structures below (presence sets, the bit-sliced class index, the initials column, the head column, the arena record table and its block index). Measured at 1M **synthetic** entries (`bench`, mean name length ≈ 19 B): 133.7 B/entry cold, 139.1 B/entry warm — inside the hard cap, above the 120 B/entry typical target. **Real volumes run longer names and therefore heavier: mean name length L measures 25–35 B** (25 on the 1.09M MFT enumeration in `docs/M0.md`, 30.0 on a 756k walk, 34.6 on a 554k user-file-heavy walk — issue #10), and every arena-sized structure scales with it at ~2.06 B/entry per byte of L, giving **143.0 B/entry at 1.09M (MFT), 148.7 at 756k, 165.4 at 554k**, all inside the cap; `docs/design/accel-redesign.md` §F restates the byte accounting at the measured range, where the steady state holds but the compaction transient on a name-heavy volume makes the open-addressed FRN table (its Step 12) the required lever rather than an optional one. The cap holds across corpus sizes — 142.6 B/entry at 20k, 132.0 at 50k, 131.8 at 100k (synthetic) — under one rule: **a fixed-size structure is admissible exactly when it is smaller than the data it accelerates**, and everything else grows in steps proportional to what it already holds rather than in constants sized for a 1M-entry index. The rule is what decides whether a set is allocated unconditionally or earns its place: the arena's 2 MiB trigram set only becomes smaller than the arena above ~109k entries, so below that it is not allocated at all and the 8 KiB bigram set carries the Pass A gate alone; the 8 KiB pair over the initials column passes the same test above ~1k entries, two orders of magnitude lower, so it is simply always held. Service-wide budget lines (normative, restated from §1.2 Goal 6): filename index ≤ 200 MB per 1M files; content pipeline steady-state ≤ 150 MB with merge transients ≤ 300 MB (enforced via Tantivy writer heap settings, §3.5); service caches (per-SID directory-grant cache and friends, §3.8) ≤ 50 MB with LRU eviction; the extractor is a **separate process** (`yspot-extract`, §2.2) with its own RSS budget ≤ 200 MB, outside the service's. `IndexStatus.ram_bytes` reports `{filename, content, caches}` separately (§4.3) so each line is CI-enforceable on its own.
- **Matching**, in ranked tiers: exact name > prefix > word-boundary (segments split on `-_. ` and space) > camel-case initials (`fbar` → `FooBar.txt`) > contiguous substring > fuzzy subsequence (bounded edit tolerance). Query cancellation on new keystroke is mandatory.
- **Candidate generation (prefilter):** each expensive tier scores only a prefiltered candidate set, never the full entry table.
  - **Contiguous tiers** (exact / prefix / word-boundary / substring) come from one SIMD `memmem` scan of the case-folded arena. The arena is **NUL-fenced** (`0x00 rec 0x00 rec 0x00`), which both makes a cross-record match structurally impossible and lets a hit's tier be classified from the two bytes either side of it. The scan is skipped outright when the **presence sets** — exact membership over the arena's byte trigrams (2^24 bits), bigrams and single bytes — prove the query cannot appear contiguously anywhere. A hit maps back to its entry in O(1) through an append-only record table plus a 64-byte-block index, never a binary search. **Single-byte queries** (the first keystroke of every search) take their exact and prefix tiers from a **2-byte-per-slot head column** — the first two folded bytes of each record, the same two bytes the classifier reads either side of a hit at a record's start — in one streaming pass, and run the arena scan only while the resulting floor still admits a word-boundary hit (issue #11: at 1M entries such a query hits nearly every record, and the scan's per-hit loop, not the scan, cost 7–17 ms).
  - **Camel-case initials** come from a **fixed-stride 8-byte column**, one lane per slot, so a hit maps to its slot by a shift. Queries longer than the lane skip the tier. Names with more segments than fit in 8 folded bytes lose their trailing initials — see the note below. This column has **presence sets of its own** — bigrams and single bytes over the lanes — and the scan is skipped outright when they prove no lane can carry the query. They must be separate sets: the arena's describe which bytes are adjacent inside whole names, which says nothing about which letters are adjacent as segment initials, so gating this tier on them would drop genuine results. Sound for the same reason the arena's are, because a match here is required to lie wholly inside one lane.
  - **Fuzzy subsequence** candidates come from a **bit-sliced character-class index**: 64 slices, one bit per slot per class, and a candidate must have a class set that is a **superset of the query's**. Queries shorter than 3 characters do not run the fuzzy tier at all.

    This supersedes the trigram-intersection rule the earlier draft of this section specified (*candidates = names containing all query trigrams*). Trigram intersection has **false negatives** on exactly the queries the tier exists for: a gapped subsequence carries none of its own trigrams, so `abcd` could never find `a_b_c_d`, and `dtldr` could never find `dataLoader`. The class-set rule is a strict superset of the trigram rule, so nothing that matched before stops matching, and every survivor is still verified exactly by the density scorer — the widening admits no false positives. It also costs 8 B/entry against the posting lists' ~91 B/entry measured at 1M, which is what brought the index inside its memory cap.

    **Bounded by the candidate cap — normatively approximate (issue #7 ruling, M1).** Because the prefilter is deliberately wider, a query whose character classes are common can produce more survivors than the fuzzy cap will verify. Exact top-K under overload was measured unreachable within the §2.5 budget on real corpora, by three independent walls: verifying every survivor is over budget; the scalar scorer's throughput ceiling is 1.39× (per-query oracle over the byte-level and ASCII-tight-loop variants); and even the tightest bound any class-positional data can give — the exact minimal window containing every query class — still requires more verifications than the cap allows (mean 21,988, worst 104,697 across 53 real skeleton queries), because it is order-blind and because sparse-genuine queries never fill the page, which makes verify-all the only exact answer. The tier is therefore **normatively approximate when the cap binds**, with two mitigations: (1) the retained set is the shallowest survivors, which resolves the massive equal-density tie plateaus real corpora produce (~90k names tied at the page floor for `mcrsft`-shaped queries) in the same order the ranker would; (2) **budget-adaptive continuation** — when the page still has room after the retained set is verified, the drain keeps verifying tail candidates until a wall-clock deadline (~the §2.5 service allotment) measured from the start of the search. A query that fills its page never continues, so the latency gates are unaffected by construction; the sparse-genuine regime, where the cap previously cost essentially all results (e.g. 1,242 of 1,276 genuine matches dropped on a measured query), recovers them for free because rejection-heavy tails are the cheapest to verify. The residual approximation is the rare strictly-above-plateau candidate on dense queries. The benchmark harness reports survivors against the cap per query class as the standing tripwire, and on the synthetic 1M corpus the widest class admits 572k candidates against a cap of 20k. Measurement history in issue #7.
  - Fuzzy DP scoring runs only on the union of these candidate sets. All of these structures are budgeted inside the 200 MB / 1M-entry cap above, and all are maintained **incrementally** at the index's two mutation choke points: none is rebuilt on the query path, so a USN event costs the next query nothing.
  - §10 M0 exit criteria include worst-case 2-char and 3-char fuzzy query measurements, not just substring.
- **Filtering:** a caller-supplied predicate over the entry NAME (an extension test, a prefix) is applied while candidates are being ranked, not after, so a filtered page fills with `max_results` accepted rows. Post-filtering cannot do this: it must guess an over-fetch multiple and returns short whenever the filter is more selective than the guess. A predicate needing the full path stays a post-filter — path reconstruction walks the parent chain per candidate — and is therefore still subject to that shortfall.
- **Tie ordering:** rows with *exactly* equal scores are ordered by entry index, which is an implementation artifact rather than a stable key: slots are recycled from a freelist and renumbered by compaction (§3.7), so the relative order of exactly-tied rows may differ across churn. Deterministic for a given event history; not stable across it.
- **Ranking:** the service returns pure `score = match_quality × depth_penalty`. **No frecency exists in the service**: frecency is owned by the shell's per-user store (`%LOCALAPPDATA%\YSpot\frecency.db`, SQLite, keyed for files by the stable `{volume GUID + FRN}` id) and applied uniformly by the shell's global ranker across all sources (§7.3), which is also what keeps one user's usage history out of another user's ordering (§3.10). `depth_penalty` mildly favors shallow paths; hidden/system-attribute files rank below normal files but are not excluded.

### 3.5 Content index (Tantivy)

- **Opt-in scopes**, defaulting (when a user enables content search) to **the enabling user's profile** minus noise directories: `AppData`, `node_modules`, `.git`, `target`, `build`, `dist`, `venv`/`.venv`, `__pycache__`, browser caches. Scopes and exclusions are **per-user configuration keyed by the impersonated pipe-client SID** (§3.10) and are user-editable.
- **Extractor pipeline** — runs in `yspot-extract`, the dedicated fifth process (§2.1/§2.2), never in the service: extractors parse hostile file formats (PDF, OOXML). The service spawns it via `CreateProcessAsUser` with a **write-restricted token + job object**, with `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY` (no child processes) applied at creation; files are passed by **duplicated read-only handle** (never by path — the worker needs no filesystem rights of its own), and extracted text returns over an anonymous pipe under the same frame-cap rules as the main pipe (§4.2/§8.1). Formats: plain text/code/`.md` read directly with encoding detection; `.pdf` via a Rust PDF text extractor (pdfium behind a sandbox or a pure-Rust crate); `.docx`/`.xlsx`/`.pptx` via ZIP + XML part extraction. Caps: skip files > 50 MB; index at most the first 2 MB of extracted text per file; per-file extraction timeout 10 s — on timeout or worker crash, the worker is killed and restarted and the offending file is **quarantined, never retried** (§2.6). The format parsers are mandatory cargo-fuzz targets (§8.1).
- **Incremental updates** are driven by the same USN stream, debounced 2 s per path; deletes/renames update the Tantivy doc by stored FRN key. **Freshness:** after any extraction completes, the changed document MUST be committed and the NRT reader reloaded within 3 s — so a small text document saved into an indexed scope is findable by its new content within 10 s (§1.5 D1; measured in §10 M2 exit criteria). Cloud placeholder files (`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` / `FILE_ATTRIBUTE_OFFLINE`, e.g. OneDrive) MUST NOT be opened for extraction — reading them triggers mass hydration; they are filename-indexed only.
- **Storage:** `%ProgramData%\YSpot\index\content\` (ACL: SYSTEM + Administrators full, Users none). Each content document is tagged with the owning scope's SID and served only to that SID (§3.10). The ≥ 30 s batch-commit cadence applies **only to bulk backlog** (initial content indexing, large drops); fresh single-file changes take the 3 s commit + NRT-reload path above. Rely on Tantivy segment merging with a background merge policy (writer heap settings sized to the §3.4 budget lines: steady-state ≤ 150 MB, merge transients ≤ 300 MB), plus a weekly forced compaction in idle windows. Size budget: ≤ 10% of scoped corpus size, absolute default cap 2 GB (user-raisable); on cap, oldest-modified files are evicted, not newest.

### 3.6 Scheduling and throttling

- **Initial/cold enumeration** (the scan that gates first use) runs at **normal I/O priority**, with only the per-handle `SetFileInformationByHandle(FileIoPriorityHintInfo, IoPriorityHintVeryLow)` hint applied — thread-level background priority is starvable without bound under foreground disk load and would make the 15 s budget unenforceable. **Background priority** (`THREAD_MODE_BACKGROUND_BEGIN` on worker threads, `PROCESS_MODE_BACKGROUND_BEGIN` semantics, plus the VeryLow handle hint) applies to everything else: rebuilds performed behind an existing serving index, Tantivy merges, compaction, and backlog extraction. §2.4 states the same rule.
- **Battery:** subscribe via `RegisterPowerSettingNotification(GUID_ACDC_POWER_SOURCE, DEVICE_NOTIFY_SERVICE_HANDLE)`; on DC power, content extraction pauses and USN batches apply at reduced cadence (filename tailing never stops — it is cheap). 
- **User-idle:** `GetLastInputInfo` is session-scoped and reflects no interactive input in Session 0, so the shell reports interactive/idle state over the pipe (`SessionState {state: active|idle}`, §4.3). The service treats the machine as idle only when **every** connected session reports idle; heavy work (merges, compaction, backlog extraction) runs only in machine-idle windows.
- **Pause/resume:** any interactive user may pause indexing machine-wide via `PauseIndexing`/`ResumeIndexing` (§4.3); the action is logged and reflected as `IndexStatus.state = paused`. Per-volume disable and journal recreation are admin-only (§3.10).

### 3.7 Persistence and restart

- Per NTFS volume, snapshot the filename index (entries + name arenas + the §3.4 prefilter structures — **no usage data**; frecency is shell-owned per §3.4, so nothing ranking-related is lost on rebuild) to `%ProgramData%\YSpot\index\fs\<volume-guid>.snap` as a versioned, checksummed (xxhash64) flat binary, written atomically (temp file + `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`), alongside `{UsnJournalID, LastUsn}`. Snapshot on clean shutdown and every 15 min of accumulated change.
- On start: load snapshot (mmap-friendly layout; target < 1 s per 1M entries), then replay USN from `LastUsn`. Any failure — bad checksum, version mismatch, journal ID change, `ERROR_JOURNAL_ENTRY_DELETED` — discards the snapshot and re-enumerates; the service answers queries from partial data during rebuild, flagged `rebuilding` in responses.
- **Compaction.** Mutation is append-only — a delete tombstones its slot and erases its folded record, a rename appends the new name and abandons the old — so dead slots and dead arena bytes accumulate with churn even though the live count does not. Compaction reclaims them: it renumbers slots, compacts both arenas, and rebuilds every derived column and the presence sets, which is also the point at which the presence sets stop being a superset of the live arena and initials column and become exact again.

  Compaction MUST be performed **in place**. Assembling fresh arenas beside the live ones and swapping would double the two largest structures at peak — roughly 42 MB at 1M entries — i.e. it would breach the §3.4 cap precisely when the index is already carrying enough garbage to want compacting. This is possible because live records ascend in record-table order in both arenas and compaction only ever drops records, so the write cursor never passes the read cursor. The FRN map keeps its keys and has only its values rewritten; rebuilding it would rehash every key to no purpose.

  Triggers: dead arena bytes > 25% of live bytes, superseded records > 25% of the record table, or dead slots > 12.5% of the entry table. Compaction takes the write lock for its whole run — every structure is renumbered at once, so unlike the depth repair it cannot be sliced — and is therefore gated on machine idle (§3.6). Past 40% dead bytes it runs regardless, since the waste then exceeds what waiting for a quiet window is worth. The volume reports `rebuilding` (§4.3) while it runs.

  Compaction renumbers slots, so it can reorder rows whose scores are *exactly* equal (§3.4, Tie ordering). It changes no scores and no membership.

- **Service configuration** (content scopes, exclusions, per-volume toggles — including the per-SID user config of §3.10) persists as versioned JSON at `%ProgramData%\YSpot\config.json` (ACL: SYSTEM + Administrators write), applied and written atomically with the same temp-file + `MoveFileExW` pattern. This file is the durable form of `ConfigUpdate` patches (§4.3).

### 3.8 Exclusion rules

Always excluded from both indexes: NTFS metafiles (`$MFT`, `$Extend`, `$RECYCLE.BIN` contents surfaced as such), `System Volume Information`, pagefile/hiberfil/swapfile. Filename index is otherwise complete ("every file on every NTFS volume" is the product promise); user exclusions and the content-scope rules of §3.5 apply on top. Reparse points are indexed as entries but never traversed for content. Per-volume disable is available (admin-only, §3.10). Results are filtered at query time against the *requesting user's* access rights before leaving the service — the service reads as SYSTEM and MUST NOT leak names of files the shell user cannot access:

- The service impersonates the pipe client via `ImpersonateNamedPipeClient` and runs `AccessCheck` against each candidate's parent-directory security descriptor.
- Grants are held in a **per-SID directory-grant cache** (one cache per client SID — required by §2.3's multi-session model; LRU-evicted inside the ≤ 50 MB service-cache budget of §3.4).
- `USN_REASON_SECURITY_CHANGE` records in the journal stream (§3.3) invalidate the affected directory subtree's cache entries, so an ACL tightened after caching cannot keep leaking names.
- Hot-path trimming is bounded to the returned page: ≤ `max_results` checks per batch, with checks for deeper batches amortized as they stream. Trimming cost has its own line in §2.5's latency table.

### 3.9 ultrasearch license — verified finding: re-implement, do not fork

Checked 2026-08-28 at `github.com/Dicklesworthstone/ultrasearch`: the repo's licensing is internally contradictory. The top-level `LICENSE` file is titled **"MIT License (with OpenAI/Anthropic Rider)", Copyright (c) 2026 Jeffrey Emanuel**, adding a rider that forbids providing or distributing the software to "Restricted Parties" (OpenAI, Anthropic, and affiliates) and auto-terminates on breach — a field-of-use restriction that makes it **not an OSI open-source license**. A second file `LICENSE-MIT` is plain MIT (c) 2025, and the README badge claims "MIT/Apache-2.0" while the README text says MIT. Which grant governs is legally ambiguous, and the rider taints any conservative reading. **Decision: YSpot MUST NOT fork or copy ultrasearch code.** yspot-indexd re-implements the architectural pattern (MFT enumeration + USN tailing + Tantivy + service/worker split — ideas, not expression) as a cleanroom original implementation, using only the public Microsoft documentation cited above.

### 3.10 Multi-user semantics

One machine-wide filename index serves every interactive session (§2.3). Everything user-specific is partitioned by the **impersonated pipe-client SID** (the mechanism §3.8 already requires):

- **Per-user configuration:** content scopes and user exclusions are per-user config keyed by the pipe-client SID, persisted in `%ProgramData%\YSpot\config.json` (§3.7). A `ConfigUpdate` from a non-admin client can mutate only that client SID's own scopes and exclusions — user A can never add, remove, or widen scopes that cover user B's directories.
- **Content ownership:** every content document is tagged at index time with the owning scope's SID and is served **only** to that SID, *in addition to* the per-result `AccessCheck` of §3.8 — which applies equally to `ContentSearchQuery` results and their snippets. User A never receives content hits or snippets from scopes user B enabled, even for files A could read on disk.
- **Machine-wide operations** (per-volume disable, USN journal recreation) are **admin-only**: the impersonated client token MUST be checked for Administrators membership; otherwise the request fails with error `103` (§4.3). Pause/resume of indexing (§3.6) is deliberately *not* admin-gated — any interactive user may pause machine-wide, and the action is logged.
- **No cross-user ranking state:** the service holds no frecency or usage data (§3.4); each shell applies its own user's frecency store, so one user's file-open patterns can neither pollute nor leak into another user's result ordering.
- **Idle aggregation:** the machine counts as idle only when every connected session reports idle via `SessionState` (§3.6, §4.3).

## 4. IPC Protocol

Two transports: a **framed binary protocol over a named pipe** between `yspot-indexd` and its clients (hot search path), and **JSON-RPC 2.0** for shell↔frontend (Tauri commands/events) and shell↔exthost (stdio). Extensions never talk to the service directly; the shell proxies.

### 4.1 Service named pipe

- **Pipe name:** `\\.\pipe\yspot.indexd.v1`. The `v1` suffix is the *transport* major version; it changes only on an incompatible framing change (never for message additions — see §4.5).
- **Creation:** `CreateNamedPipeW` with `PIPE_TYPE_BYTE | PIPE_READMODE_BYTE`, `PIPE_REJECT_REMOTE_CLIENTS` (blocks SMB/remote clients at the kernel), `FILE_FLAG_OVERLAPPED`, and `nMaxInstances = PIPE_UNLIMITED_INSTANCES`. `FILE_FLAG_FIRST_PIPE_INSTANCE` is passed on the **first instance only** — that is the squat check: if that first creation fails with `ERROR_ACCESS_DENIED`, another process has squatted the name, and the service MUST log a security event and refuse to start degraded. Subsequent instances are created with the same security descriptor but **without** the flag (with it, every later `CreateNamedPipeW` call would itself fail with `ERROR_ACCESS_DENIED`); they need no squat check because the DACL below withholds `FILE_CREATE_PIPE_INSTANCE` from non-SYSTEM/non-admin principals. The service re-arms a fresh listening instance on every accepted connection. If all instances are momentarily busy, a connecting client sees `ERROR_PIPE_BUSY` and MUST retry via `WaitNamedPipe` (100 ms timeout, up to 5 attempts) before reporting the service unavailable.
- **Security descriptor (SDDL):** the service (SYSTEM) passes an explicit descriptor via `SECURITY_ATTRIBUTES`, built with `ConvertStringSecurityDescriptorToSecurityDescriptorW`:

  ```
  O:SYG:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019B;;;IU)S:(ML;;NW;;;ME)
  ```

  - `SY`/`BA` (SYSTEM, Administrators): full control, for the service and admin tooling.
  - `IU` (S-1-5-4, interactive-logon users — exactly the unelevated shell/exthost in the interactive session): mask `0x12019B` = `FILE_GENERIC_READ | FILE_GENERIC_WRITE` **minus** `FILE_CREATE_PIPE_INSTANCE` (0x4). `FILE_CREATE_PIPE_INSTANCE` shares its value with `FILE_APPEND_DATA`, so granting plain `FILE_GENERIC_WRITE` would let any client create rogue server instances of our pipe; the reduced mask forbids that.
  - `D:P` blocks ACL inheritance; `S:(ML;;NW;;;ME)` sets a Medium integrity label with no-write-up, so low-IL (sandboxed) processes cannot write.
  - No ACE for Everyone, `NU` (network), or Anonymous — they get no access at all.
- **Client hardening:** clients MUST open with explicit access rights `dwDesiredAccess = 0x12019B` (`FILE_GENERIC_READ | (FILE_GENERIC_WRITE & ~FILE_APPEND_DATA)`) — never `GENERIC_WRITE` (alone or as `GENERIC_READ | GENERIC_WRITE`), because `GENERIC_WRITE` maps to `FILE_GENERIC_WRITE` (0x120116), which includes `FILE_APPEND_DATA` (0x4) — exactly the bit the DACL withholds — so a generic-write open fails with `ERROR_ACCESS_DENIED` against our own descriptor. Clients MUST also pass `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` (server cannot fully impersonate the client; identification-level tokens still suffice for the service's `AccessCheck` trimming and Administrators membership checks) and MUST verify the server before the first write: `GetNamedPipeServerProcessId` + confirm the pipe object's owner SID is `S-1-5-18` (SYSTEM) via `GetSecurityInfo`. Servers MAY use `GetNamedPipeClientProcessId` for logging but MUST NOT use PID for authorization (the DACL is the authorization).

### 4.2 Framing

Length-prefixed **MessagePack**: `u32` little-endian payload length, then one MessagePack-encoded map. Rationale: the results-to-frontend budget (≤ 20 ms p95 after keydown, decomposed in the §2.5 table) is spent streaming batches of hundreds of result rows with paths, scores, and match ranges; MessagePack (`rmp-serde`) is ~25–40% smaller than JSON on this shape, parses without float/UTF-8 escaping overhead, and carries binary cleanly — while remaining schemaless maps with string keys, which is what makes the additive versioning rules in §4.5 work. JSON stays where humans debug (§4.6–4.7). Limits (stated identically in §8.1): **client→service frames are capped at 1 MiB** (queries are small; large inbound frames are attack surface), **service→client frames at 16 MiB** (result batches); an oversized or undecodable inbound frame is a protocol error and the connection is dropped. The anonymous pipe between the service and the `yspot-extract` worker (§2.2) reuses this framing with the same cap rules (service→extractor 1 MiB, extractor→service 16 MiB). Paths are transmitted as UTF-8, with unpaired UTF-16 surrogates replaced by U+FFFD and a `raw_path` binary field added only in that rare case.

### 4.3 Messages

Every frame is a map with `t` (type tag). Request/response pairs correlate on `id` (u64, per-connection, client-assigned); streams correlate on `gen`.

| `t` | Dir | Fields | Notes |
|---|---|---|---|
| `Hello` | C→S | `proto_min:u32, proto_max:u32, client:str, pid:u32` | First frame after connect. |
| `HelloAck` | S→C | `proto:u32, service_version:str, index_epoch:u64` | `proto = min(server_max, client_max)`; below `proto_min` of either side ⇒ `Error` 100 + close. `index_epoch` bumps on full rebuild. |
| `SearchQuery` | C→S | `id, gen:u64, text:str, scopes:[str], filters{ext:[str], kind, path_substr:str}, max_results:u32` | Filename index. Supersedes all lower `gen` (§4.4). `path_substr` is a case-folded substring match on the parent path (backs §7.3's `path:`). Size/mtime filters are deliberately absent in v1 — the §3.4 entry stores neither. |
| `ContentSearchQuery` | C→S | `id, gen, query:str, scopes:[str], max_results, snippets:bool` | Tantivy query string; only opt-in content scopes. Shares the `gen` space with `SearchQuery`. |
| `SearchResults` | S→C | `gen, seq:u32, is_final:bool, items:[{id:{volume_idx:u32, frn:u64}, path, name, score:f32, size, mtime, match_ranges:[[u32,u32]], snippet?}]` | Streamed batches, `seq` starts at 0, contiguous; `is_final` on last (possibly empty) batch. Serves both query types. Batches are strictly rank-descending per `gen` — batch 0 is the global top-K after full match+rank, so cross-batch application is append-only (merge contract, §5.11). `id` is the opaque stable key (volume-GUID index + FRN) used by `executeAction`, §5.6 row keys, and the shell's frecency store. `size`/`mtime` are populated by lazily stat-ing only the returned page (≤ 32 files) just before send. `match_ranges` offsets are **UTF-16 code-unit indexes** into the transmitted `name` string. |
| `Cancel` | C→S | `gen` | Explicit cancel (e.g. window hidden) with no replacement query. |
| `IndexStatusReq` / `IndexStatus` | C→S / S→C | resp: `id, volumes:[{volume, fs, state, files_indexed:u64, usn_lag_ms:u32, content_docs:u64, ram_bytes:{filename:u64, content:u64, caches:u64}}]` | `state ∈ enumerating\|tailing\|rebuilding\|paused\|unsupported\|offline`. `paused` (machine-wide, set via `PauseIndexing`) is reported on every volume; `unsupported` marks non-NTFS volumes the service does not index — queries scoped there get error 107 and the shell routes them to its own Windows Search OleDB provider (§3.1). `ram_bytes` splits out the §1.2 Goal 6 budget lines so CI can enforce each separately. |
| `ConfigUpdate` | C→S | `id, patch:{content_scopes?, excluded_paths?, ...}` → `Ack{id}` | Partial update; unknown keys rejected with code 106. `content_scopes`/`excluded_paths` are **per-user** state keyed by the impersonated pipe-client SID (multi-user semantics, §3). Machine-wide keys (per-volume disable, journal recreation) are **admin-only**: the service impersonates the client and checks the token for Administrators membership, else error 103. Durable form: `%ProgramData%\YSpot\config.json` (§3). |
| `SessionState` | C→S | `{state: active\|idle}` | Sent by each session's shell on interactive/idle transitions (`GetLastInputInfo` is session-scoped and reflects no interactive input in Session 0, hence shell-reported). The service treats the machine as idle only when **every** connected session reports idle (§3.6). Fire-and-forget. |
| `PauseIndexing` / `ResumeIndexing` | C→S | `id, {}` → `Ack{id}` | Any interactive user may pause indexing machine-wide (the event is logged); reflected as `IndexStatus.state = paused`. Backs the §5.5 tray toggle and §5.9 service controls. |
| `Subscribe` | C→S | `id, topics:[str]` → `Ack{id}` | Topics: `index.progress`, `index.state`, `config.changed`. |
| `Event` | S→C | `topic:str, payload:map` | Push after `Subscribe`; e.g. progress `{volume, phase, percent, files_seen}`. |
| `Error` | S→C | `id?, gen?, code:u32, message:str, retryable:bool, data?` | See §4.8. |

There is deliberately **no launch/open-report message** in v1: frecency is owned entirely by the shell (per-user store at `%LOCALAPPDATA%\YSpot\frecency.db`, keyed by stable IDs derived from the result `id` — volume GUID + FRN), so the service never sees launches and ranks on pure match quality alone (§3.4).

### 4.4 Cancellation

`gen` is a per-connection monotonically increasing counter assigned by the client (typically one per keystroke). Receiving any query with a higher `gen` implicitly cancels all in-flight work on that connection: the service MUST stop matching/scoring for stale generations promptly (checked at batch boundaries, SHOULD be < 5 ms) and MUST NOT start new batches for them. In-flight stale batches MAY still arrive; clients MUST drop any `SearchResults`/`Error` whose `gen` is less than the client's current generation, without warning. A stale generation therefore never needs an explicit `Cancel`; `Cancel` exists only for "stop entirely". `gen` never resets while a connection lives; reconnect resets it.

### 4.5 Versioning

- Protocol version is a single `u32` negotiated in `Hello`/`HelloAck`. The negotiated version governs which message types and semantics are active.
- **Additive-only:** within a transport major (`v1` pipe name), changes MUST be limited to new message types, new optional fields, and new enum values. Fields are never removed, renamed, or re-typed. Receivers MUST ignore unknown map keys and unknown `t` values they did not negotiate (unknown *requests* get `Error` 105, not a disconnect).
- Anything that can't be expressed additively ⇒ new pipe name `yspot.indexd.v2`; the service serves both during a deprecation window of at least two releases.

### 4.6 JSON-RPC 2.0 — shell ↔ frontend

Carried over Tauri v2 IPC: JSON-RPC requests map to Tauri commands (`invoke`), notifications to Tauri events (`emit`/`listen`). Same envelope semantics (`id`, `error{code,message,data}`) so the error model in §4.8 is uniform.

| Method / Event | Dir | Params → Result |
|---|---|---|
| `search` | F→S | `{gen, text, scopes?, filters?}` → `{accepted:true}`; results arrive via event |
| `contentSearch` | F→S | `{gen, query, scopes?, snippets?}` → `{accepted:true}` |
| `cancel` | F→S | `{gen}` → `{}` |
| `executeAction` | F→S | `{resultId, actionId, modifiers}` → `{ok}` (launch, open folder, copy path, window mgmt…); `resultId` is the opaque `id` from §4.3 for index results (also the shell's frecency key), or a namespaced stable ID for apps/commands/extensions |
| `getIndexStatus` / `getConfig` / `setConfig` | F→S | `{}` / `{}` / `{patch}` → status / config / `{ok}` |
| `extEvent` | F→S | `{instanceId, handlerId, payload}` → `{}` (forwarded to exthost `ui.event`) |
| `ui.resyncRequest` | F→S | `{instanceId}` → `{}` — sent when a received patch's `baseSeq` does not match the frontend mirror's current `seq`; the shell forwards it to the exthost as `ui.resync` (§4.7). After sending, the frontend discards further patches for that instance until a fresh full `ext:render` arrives |
| `hideWindow` / `frontendReady` | F→S | `{}` → `{}` |
| `search:results` | S→F (event) | `{gen, seq, isFinal, items[]}` — relayed pipe batches (items carry the §4.3 `id`); shell drops stale gens before relay |
| `index:progress`, `index:state` | S→F (event) | relayed `Event` payloads |
| `ext:render` / `ext:patch` | S→F (event) | `{instanceId, seq, tree}` / `{instanceId, seq, baseSeq, ops[]}` (RFC 6902 JSON Patch); the frontend applies a patch only if `baseSeq` equals its mirror's current `seq`, else it sends `ui.resyncRequest` |
| `config:changed`, `window:shown`, `window:hidden` | S→F (event) | `{…}` — `window:shown` MUST precede first paint focus handling |

### 4.7 JSON-RPC 2.0 — shell ↔ exthost

Transport: exthost stdio, newline-delimited JSON-RPC 2.0 (one UTF-8 JSON object per `\n`-terminated line; no `Content-Length` headers). `stderr` is reserved for crash diagnostics.

| Method | Dir | Params → Result |
|---|---|---|
| `initialize` | S→E | `{protocolVersion, hostVersion, extensionsDir}` → `{protocolVersion, capabilities}` |
| `extension.load` / `extension.unload` | S→E | `{id, path, manifest, memoryLimitMb}` / `{id}` → `{ok}` |
| `command.launch` | S→E | `{extId, commandId, instanceId, args}` → `{ok}`; UI arrives via `ui.render` |
| `ui.event` | S→E | `{instanceId, handlerId, payload}` → `{}` (user pressed Enter, selected item, typed in search bar…) |
| `ui.resync` | S→E | `{instanceId}` → `{}` — forwarded `ui.resyncRequest` (§4.6); the extension worker MUST answer with a fresh full `ui.render` carrying its current `seq` |
| `search.query` | S→E | `{gen, text}` → `{accepted:true}` — root-search fan-out, sent only to extensions whose manifest declares `rootSearch` (§6.1); results stream back via `search.results` |
| `shutdown` | S→E | `{}` → `{}`; exthost exits within 2 s or is killed |
| `ui.render` (notif) | E→S | `{instanceId, seq, tree}` — full declarative component tree (fixed component set only); `seq` is a per-instance monotonic counter |
| `ui.patch` (notif) | E→S | `{instanceId, seq, baseSeq, ops[]}` — RFC 6902 JSON Patch; the worker reconciler diffs against its own last-committed tree; `baseSeq` names the tree the ops apply to, `seq` the tree that results |
| `search.results` (notif) | E→S | `{gen, items[], isFinal}` — hard deadline **150 ms** after `search.query`; later results are appended per the merge contract (§5.11) or dropped; the shell enforces per-extension result caps and drops stale `gen`s |
| `api.invoke` | E→S | `{extId, capability, method, args}` → result — brokered host APIs (storage, oauth, notifications, `system.processes.list/kill` for the process killer (§7); `clipboard.*`/`window.*` are deferred past v1 per §7.8). The **shell is the sole authoritative enforcement point**: it MUST validate every call against the per-extension grant table before execution and MUST rate-limit `api.invoke` per extension. The exthost broker MAY pre-filter for fast-fail UX; its checks carry no security weight (§8.2) |
| `extension.crashed` (notif) | E→S | `{extId, instanceId?, error}` — shell shows error card; host survives |
| `log` (notif) | E→S | `{extId, level, message}` |

### 4.8 Error model

JSON-RPC surfaces use standard codes (`-32700` parse, `-32600` invalid request, `-32601` method not found, `-32602` invalid params, `-32603` internal); application codes below are shared across all three transports (pipe `Error.code` uses them directly; JSON-RPC carries them in `error.data.appCode` with a `-32000` range code). Errors are per-request/per-gen, never connection-fatal, except framing violations (oversized/undecodable frame), which close the pipe.

| Code | Name | Retryable | Meaning |
|---|---|---|---|
| 100 | `UNSUPPORTED_VERSION` | no | Handshake version ranges disjoint; connection closed after send |
| 101 | `VOLUME_OFFLINE` | yes | Scope references a dismounted/unknown volume |
| 102 | `INDEX_REBUILDING` | yes | Query arrived during rebuild; partial results flagged via `data.partial` |
| 103 | `SCOPE_DENIED` | no | Content query outside the caller's opt-in content scopes, or an admin-only machine-wide operation (per-volume disable, journal recreation) attempted by a client whose token is not in Administrators |
| 104 | `OVERLOADED` | yes | Service shed load; client SHOULD debounce and resend latest gen |
| 105 | `UNKNOWN_MESSAGE` | no | `t`/method not in negotiated version |
| 106 | `INVALID_CONFIG` | no | `ConfigUpdate`/`setConfig` patch rejected; `data.key` names the field |
| 107 | `SCOPE_UNSUPPORTED` | no | Scope lies on a volume the service does not index (non-NTFS — remote, FAT/exFAT, …); the shell routes such scopes to its own Windows Search OleDB provider (§3.1), which is also the §9.5 portable-mode code path |
| 200–299 | extension errors | varies | `EXT_TIMEOUT`, `EXT_OOM_KILLED`, `EXT_THREW`, `CAPABILITY_DENIED` — surfaced as error cards, never crash the host |

## 5. Shell and Frontend

The shell (`yspot`) is a Tauri v2 Rust process running unelevated in the user session. It owns every OS integration point: global hotkey, launcher window lifecycle, tray icon, autostart (the shell writes its own HKCU Run value, §5.4), single-instance enforcement, spawning/supervising `yspot-exthost`, and brokering IPC between the frontend, `yspot-indexd`, and the exthost. Beyond OS integration, the shell owns four cross-cutting responsibilities:

- **Windows Search OleDB fallback.** The `Search.CollatorDSO` passthrough lives only in the unelevated shell — the correct process for user identity, per-logon-session drive mappings, and Windows Search's caller-scoped security trimming. When the service answers a scope with error `107 SCOPE_UNSUPPORTED` (non-NTFS scopes, §4.3), the shell routes that scope to its own OleDB provider. Portable/degraded mode (§9.5, §5.9) uses this identical shell-side code path.
- **Frecency and global ranking.** The shell owns the per-user frecency store at `%LOCALAPPDATA%\YSpot\frecency.db` (SQLite, persisted on write), keyed by stable IDs: `{volume GUID + FRN}` for files, AUMID or resolved shortcut path for apps, namespaced IDs for commands and extensions. The service returns pure `match_quality` (plus depth penalty); the shell's global ranker applies frecency uniformly across all sources (§5.11). There is no `ReportOpen` pipe message in v1 — the shell records launches/opens locally.
- **Capability enforcement.** The shell is the sole authoritative permission-enforcement point: every extension `api.invoke` is validated in the shell against the per-extension grant table before execution, and rate-limited per extension (§4.7, §6.5). The exthost broker's pre-filtering is fast-fail UX only and carries no security weight (§8.2).
- **Session state reporting.** The shell reports its session's interactive/idle state to the service via `SessionState` (§4.3); the service treats the machine as idle only when every connected session reports idle.

The frontend is React + TypeScript loaded in the shell's WebView2. No business logic lives in the frontend; it renders state and forwards intents over the Tauri IPC bridge.

### 5.1 Global hotkey

- Default binding: **Alt+Space**, registered via `RegisterHotKey(hwnd, id, MOD_ALT | MOD_NOREPEAT, VK_SPACE)` (`MOD_ALT` = 0x0001, `MOD_NOREPEAT` = 0x4000, `VK_SPACE` = 0x20). This matches the legacy PowerToys Run default and Raycast muscle memory. Note: `RegisterHotKey` globally overrides the classic Alt+Space window system-menu accelerator — the same tradeoff PowerToys Run made; the binding is rebindable, so this is acceptable.
- Registration MUST happen in the shell (Rust), never in the frontend, and MUST use `RegisterHotKey` — not a low-level keyboard hook (`WH_KEYBOARD_LL` adds latency to every keystroke system-wide and trips AV heuristics). Tauri's global-shortcut plugin is acceptable iff it compiles down to `RegisterHotKey` on Windows.
- **Conflict detection**: `RegisterHotKey` returns FALSE when the chord is already registered; check `GetLastError() == ERROR_HOTKEY_ALREADY_REGISTERED` (1409). On failure the shell MUST NOT silently degrade: onboarding/settings MUST show a conflict dialog that (a) names the likely owner from a built-in table — PowerToys Run (Alt+Space), PowerToys Command Palette (Win+Alt+Space), Windows input-language switcher (Win+Space), Copilot (Alt+Space on some Windows 11 builds) — and (b) offers one-click alternatives (Ctrl+Space, Ctrl+Alt+Space, Ctrl+Shift+Space). The chord picker MUST reject F12 (reserved for debuggers per `RegisterHotKey` docs) and SHOULD warn on `MOD_WIN` chords (Win-key shortcuts are documented as reserved for the OS).
- **Rebinding**: settings captures modifiers + virtual-key via a key-capture field, then atomically `UnregisterHotKey` old → `RegisterHotKey` new; on failure it re-registers the old chord and surfaces the conflict dialog. The binding persists in settings and re-registers on every shell start.
- The `WM_HOTKEY` handler toggles the launcher (show if hidden, dismiss if visible). Budget: `WM_HOTKEY` receipt → window visible with query field focused in **< 50 ms**. Nothing on this path may perform IPC, disk I/O, or WebView2 navigation.
- **The shell MAY instead be summoned by an external hotkey daemon.** (**Amended 2026-09-03**, relaxing "Registration MUST happen in the shell" above to "MUST happen in the shell *when the shell owns the chord*". The MUST against `WH_KEYBOARD_LL` is unchanged and applies to the daemon too.) `RegisterHotKey` offers no way to ask who owns a chord — the first process to claim one wins and every other gets a bare `FALSE` — so a machine running several tools that each grab a piece of the keyboard has no coherent owner and no way to report a conflict properly. One daemon holding the whole keyboard is the arrangement that can be reasoned about. Rules:
  - The choice is a setting (`hotkey_source`: `shell` | `ykeys`), never inferred. From inside the process "nobody registered the chord" and "something else registers it for us" are indistinguishable, and one of them is the fault this section requires be reported.
  - **`shell` remains the default**, so an install that has never heard of a daemon behaves exactly as specified above.
  - The summons MUST NOT be a process spawn. Starting a process costs ~8 ms median and ~20 ms p95 on a warm machine even for a program that does nothing, against a 50 ms budget that is an M0 exit criterion (§10). The shell exposes a message-only window (class `YSpot.Signal`) and one `RegisterWindowMessage("YKeysSignal")`; `wParam` selects the verb (0 toggle, 1 show, 2 settings, 3 clipboard — append only, never renumber).
  - The daemon MUST call `AllowSetForegroundWindow` on the shell's process before posting. Pressing the chord makes the *daemon* the process Windows permits to take the foreground; without the hand-off the launcher appears and does not hold the keyboard, which is §5.2's failure mode. The shell logs a refused `SetForegroundWindow` rather than leaving it to be found by typing into nothing.
  - Conflict reporting still applies to whichever side registers. Under `ykeys` the shell registers nothing, so it reports nothing — no chord was ever its to lose — and the daemon names the chord it could not take.

### 5.2 Launcher window and focus model

This subsection is the **single normative statement of the focus model** — *activate-on-summon, restore-previous-foreground-on-dismiss*. §1.2 (Goal 1), §2.1, §2.4, §10 (M0), and §11 (Risk 3) defer to it.

The launcher is a borderless, undecorated, always-on-top popup: `WS_POPUP` with `WS_EX_TOPMOST` (0x00000008) and `WS_EX_TOOLWINDOW` (0x00000080). `WS_EX_TOOLWINDOW` keeps it out of the taskbar and the Alt+Tab switcher (documented behavior).

**`WS_EX_NOACTIVATE` MUST NOT be set on the launcher window.** Verified semantics: `WS_EX_NOACTIVATE` (0x08000000) prevents the window from becoming foreground when clicked and keeps it off the taskbar, and the docs state such a window should not be activated except explicitly via `SetActiveWindow`/`SetForegroundWindow`. But WebView2 (and IME composition) only receives typed input when its host window has keyboard focus, and focus follows activation — a genuinely non-activated window cannot host a text field. The pattern real launchers use, which YSpot MUST implement, achieves "never steals focus" through lifecycle instead:

1. On `WM_HOTKEY`: record `GetForegroundWindow()` as `prev_hwnd`, position the window (§5.3), show it, and call `SetForegroundWindow(own_hwnd)`. This succeeds despite the foreground lock because the hotkey press makes the shell the last-input process; the shell MUST log and fall back gracefully (flash-free retry) if it is ever refused.
2. Focus the WebView2 controller so the query field receives keystrokes, including IME composition, immediately.
3. On dismissal (Esc, action executed, hotkey toggle, or focus loss via `WM_ACTIVATE`/`WA_INACTIVE`): hide the window and call `SetForegroundWindow(prev_hwnd)` to hand focus back exactly where it was.
4. Because the window only ever shows in direct response to user invocation, it never steals focus from foreground work.

`WS_EX_NOACTIVATE` MAY be used for auxiliary non-interactive surfaces (HUD confirmations, toasts) that must never take focus at all.

### 5.3 Placement

On every show: `GetCursorPos` → `MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)` → `GetMonitorInfoW` → position within `rcWork` (work area, excludes taskbar). Layout: horizontally centered; top edge at 20% of work-area height; width 680 logical px clamped to 90% of work-area width. The process MUST declare Per-Monitor v2 DPI awareness in its manifest and handle `WM_DPICHANGED`; placement is recomputed on every show, so monitor hot-plug and DPI changes need no persistent state.

### 5.4 Warm start and autostart

- **Autostart mechanism: HKCU Run key** — `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, value `YSpot` = `"<install>\yspot.exe" --hidden`. The **shell itself** writes this value on first run for each user, gated by the onboarding autostart toggle (§5.9) — never the installer (a per-user value written by an installer would exist only for the installing user, §9.1). Uninstall removes the current user's value; other users' stale entries self-heal (exe gone ⇒ dead Run entry). Rationale for the Run key: writable unelevated; surfaced to the user with an enable/disable toggle in Task Manager → Startup apps and Settings → Apps → Startup (honest, auditable); trivially removed on uninstall. Task Scheduler is rejected: its chief advantages (elevated launch, delay/trigger control) don't apply — the shell runs unelevated and `yspot-indexd` autostarts independently via the Service Control Manager — and scheduled tasks are opaque to most users. The MSIX/Store build (M4) MUST switch to the `windows.startupTask` appx extension, since packaged apps virtualize registry writes.
- **Warm from login**: with `--hidden`, the shell creates the launcher window, loads the frontend, waits for a "first frame rendered" signal from JS (forcing WebView2's first composition while still invisible), then idles. The shell MUST NOT call `CoreWebView2.TrySuspend` — the whole point is a hot renderer. Whether WebView2 throttles rendering of a hidden (`SW_HIDE`) window is one of the two remaining open items — **verify in M0 smoke tests**. If measurement shows hidden-window resume misses the 50 ms budget because of such throttling, the shell MUST instead hide by cloaking: `DwmSetWindowAttribute(hwnd, DWMWA_CLOAK /* 13, the set attribute */, &TRUE, sizeof(BOOL))`, which removes the window from screen while DWM keeps composing it. Idle budget: shell + frontend + WebView2 processes **< 150 MB** RAM.
- **Renderer recycle (memory-creep mitigation, §11 Risk 4)**: the shell MAY recycle the WebView2 renderer only when its private working set exceeds **250 MB** AND at least **10 minutes** have passed without a summon, and MUST immediately re-warm (reload + wait for the frontend-ready signal) so the hot-path guarantee holds. A summon arriving during re-warm shows the §2.6 native placeholder and is counted as a budget miss in telemetry.

### 5.5 Tray icon and single instance

- Tray icon is always present while the shell runs. Left-click toggles the launcher. Context menu: **Open YSpot**, **Settings…**, **Pause indexing** (sends `PauseIndexing`/`ResumeIndexing` over the pipe, §4.3; any interactive user may pause machine-wide, and the service logs the pause; reflected in `IndexStatus.state = paused`), **Quit**. Quit exits shell, frontend, and exthost; it MUST NOT stop the indexing service (service lifecycle is managed in Settings, §5.9).
- Single instance: at startup, `CreateMutexW(NULL, TRUE, L"Local\\YSpot.Shell.SingleInstance")` (session-local namespace — one shell per logged-in session). If `GetLastError() == ERROR_ALREADY_EXISTS`, the new process forwards a `show` command to the running instance over the shell IPC pipe and exits 0. Launching the exe again therefore acts as "summon the launcher" (Tauri's single-instance plugin provides this pattern).

### 5.6 Frontend: results list

- The results list MUST be virtualized with fixed row height (48 logical px). Target: **~20 visible rows**; render visible rows + 5 overscan above/below, so ≤ ~30 mounted DOM rows whether the result set holds 10 items or 1,000,000. Scroll offsets are computed from index math, never from DOM measurement.
- Rows are keyed by **stable, provider-scoped IDs** (files: the opaque result `id` — `{volume_idx: u32, frn: u64}` — carried on every `SearchResults` item, §4.3; apps: AUMID or resolved shortcut path; commands: namespaced command ID). These are the same stable IDs `executeAction` and the shell's frecency store use (§5 preamble). Stable keys make React reconciliation cheap when successive keystrokes reorder mostly-identical result sets, and preserve selection identity across updates (§5.11).
- Each row component MUST be memoized (`React.memo` with shallow props); a query update re-renders only rows whose backing item actually changed.

### 5.7 Frontend: keyboard model

- **Typing always goes to the query field.** A single input element holds focus whenever the window is visible; a capture-phase keydown handler refocuses it if anything else has focus. IME composition events pass through untouched. There is no click-to-focus dance and no focusable result rows.
- Semantics: **Up/Down** move selection (clamped at ends, no wrap; selection is sticky to the selected row's stable ID and resets to row 0 **only on generation change** — a new keystroke — never on a batch append or cross-source merge; §5.11). **PgUp/PgDn** move by one visible page. **Tab** accepts the inline autocompletion or drills into the selected item's argument mode (e.g. a command that takes a parameter). **Enter** runs the selected item's primary action. **Esc**: clears the query if non-empty; else pops the navigation stack (if inside an extension view); else dismisses the window. **Backspace** on an empty query pops the navigation stack.
- **Ctrl+K** opens the action panel (Raycast parity): a secondary palette listing every action available on the selected item with its shortcut; the panel is itself type-to-filter and follows the same Up/Down/Enter/Esc rules.
- Budget: **results data available to the frontend ≤ 20 ms (p95) after keydown; applied in the next rAF after arrival** (§5.10). The decomposition (input + frontend dispatch ≤ 2 ms, shell routing ≤ 3 ms, service first batch ≤ 10 ms, pipe + deserialize ≤ 5 ms) is owned by the single budget table in §2.5 — this section does not restate it normatively. End-to-end pixel latency is the budget plus up to two vsync periods (~33 ms at 60 Hz, ~16 ms at 120 Hz): vsync quantization is display physics, not slack to spend.

### 5.8 Theming and window materials

- Light/dark follows the system: the frontend uses `prefers-color-scheme` (WebView2 reflects the Windows app mode) plus Tauri theme-changed events, over a design-token layer (CSS custom properties). A settings override (light/dark/system) is provided.
- **Real Mica cannot be produced inside WebView2** — Mica/Acrylic are DWM-composed materials sampling content behind the native window; page CSS (`backdrop-filter`) can only sample the page itself. Two-tier approach:
  - **Windows 11 22H2+ (build 22621)**: make the Tauri window and WebView2 background transparent (`DefaultBackgroundColor` = transparent) and apply `DwmSetWindowAttribute(hwnd, DWMWA_SYSTEMBACKDROP_TYPE /* 38 */, &DWMSBT_TRANSIENTWINDOW /* 3, Desktop Acrylic */, sizeof(int))`. CSS paints translucent surfaces over the system acrylic. (`DWMWA_SYSTEMBACKDROP_TYPE` is documented as supported starting build 22621; `DWMSBT_MAINWINDOW` = 2 is Mica, `DWMSBT_TABBEDWINDOW` = 4 is Mica Alt — the launcher uses the transient/acrylic material by design.)
  - **Windows 10 and Windows 11 pre-22621**: opaque acrylic-*look* fallback — solid theme surface with layered translucent internal panels, subtle noise texture, and 1 px luminous border. No desktop sampling; undocumented `SetWindowCompositionAttribute` acrylic MUST NOT be used (unstable across builds, drag-lag history).
  - Auxiliary framed windows (Settings) additionally set `DWMWA_USE_IMMERSIVE_DARK_MODE` (20, documented from build 22000) so their title bars match dark mode.

### 5.9 Settings UI and first-run onboarding

- **Settings opens INSIDE the launcher** — a view the search window grows to fit, reached by searching for it (`settings`, `preferences`, `hotkey`) or from the tray, and left with Esc, which is exactly the navigation stack §5.7 already defines. (**Amended 2026-09-03**, superseding "a separate, normal framed window (taskbar-visible, resizable)". A launcher the user already has open, with their hands on the keys, should not throw a second window at the taskbar to change a hotkey; searching for the thing and getting it in place is the whole premise of the product. The launcher recomputes its §5.3 placement for the taller view, keeping its top edge fixed and clamping the height to the work area. Revisit only if a section arrives that genuinely cannot fit — the extension list, say.) Sections: **General** (hotkey rebind with live conflict check, autostart toggle, theme), **Search** (indexed volumes, content-search scopes, exclusions, service status/start/stop), **Extensions** (installed list, per-extension enable/permissions), **Advanced** (index rebuild, diagnostics, log export — a zip of the log directories defined in the Diagnostics & logging subsection, §8.5). All mutations go through the shell; settings persist as JSON at `%LOCALAPPDATA%\YSpot\settings.json` (machine-local state — hotkey bindings and index configuration MUST NOT roam).
- First-run onboarding (launcher-styled wizard): (1) hotkey confirmation — attempts registration immediately and runs the conflict flow of §5.1 if it fails; (2) **service consent** — plain-language explanation that fast search requires installing the `yspot-indexd` Windows service; the Install button launches the **separate elevated service MSI** (UAC prompt, §9.1), with a visible "skip" path; (3) **index scope selection** — default: filename index on all fixed NTFS volumes; content (full-text) indexing default **off**, enabled per-folder (content scopes are per-user, §3's multi-user semantics); (4) autostart opt-out (default on; the shell writes the HKCU Run value per §5.4); (5) **diagnostics consent** — opt-in crash reporting (§8.5), default off. Declining the service leaves YSpot in degraded mode — apps, settings, commands, plus file search via the shell's own Windows Search OleDB provider (§5 preamble), the same code path as portable mode (§9.5) — with a non-nagging upsell in Settings.

### 5.10 Frontend performance rules (normative)

- **No layout thrash**: never read layout (`offsetHeight`, `getBoundingClientRect`) after a same-frame write; all list geometry derives from constants. Rows use `contain: strict` and translate-only positioning; animations are `transform`/`opacity` only.
- **Results diffing**: every query dispatch carries the monotonically increasing generation (`gen`, §4.3); responses for stale generations are dropped, never rendered. Result arrays are diffed by the stable IDs of §5.6; identical prefixes must not remount. Merge and selection behavior across batches and sources follows the contract in §5.11.
- **Render coalescing**: result updates are applied at most once per animation frame (rAF batching); rapid keystrokes coalesce to the latest query.
- **Icons/thumbnails** load asynchronously off the critical path via an LRU cache (shell-side extraction → data URI); a missing icon renders a placeholder, never delays the row.
- CI MUST run an automated latency harness (synthesized keystrokes against a 1M-item corpus) on reference **Machine B** — an obligation deferred with Machine B itself (§10 amendment); until that hardware exists the shared-runner benchmark stands in, advisory. Measurement endpoints: **injected-keydown timestamp → DWM present of the updated frame**, captured via ETW `Microsoft-Windows-DWM` present events. The gate derives from §2.5's normative budget — results data available to the frontend ≤ 20 ms p95, applied in the next rAF after arrival — so the keydown→present pass threshold is that budget plus up to two vsync periods of the harness display (~33 ms at Machine B's 60 Hz). The build fails if keydown→present p95 exceeds that threshold or if hotkey→visible exceeds 50 ms.

### 5.11 Merge and selection contract (normative)

One keystroke — one generation (`gen`) — produces results from multiple sources at different times: shell-side catalogs (apps, commands, quicklinks) in ~1 ms, the service's batch `seq 0` at ~10 ms, later service batches after that, and extension root-search results up to their 150 ms deadline (§4.7). The frontend merges them under three rules:

1. **Service batches are append-only.** Within a generation, service batches are strictly rank-descending: batch `seq 0` is the global top-K after full match + rank, and every later batch is strictly worse than everything already delivered. Cross-batch application is therefore pure append — a later batch never splices rows above earlier ones.
2. **Merge never moves the selection.** Cross-source merge inserts rows by global score — the shell's ranker, which applies frecency from the per-user store uniformly across all sources (§5 preamble) — but MUST NOT move or re-index the currently selected row. Selection is sticky to its stable ID (§5.6) and resets to row 0 **only on generation change** (a new keystroke), never on a `seq` append or a late-arriving source.
3. **Late extension results stay out of the way.** Extension results arriving within the 150 ms deadline merge by score subject to rule 2; late results append below the fold or in their own section — never reordering what the user is looking at. Results arriving after the deadline are appended under this same rule or dropped (§4.7); per-extension result caps apply (§4.7).

### 5.12 Accessibility (normative)

- The query field and results list implement the WAI-ARIA **combobox/listbox pattern with `aria-activedescendant`**: DOM focus stays in the input at all times — exactly matching §5.7's keyboard model — while the visually selected row is exposed as the active-descendant `option`, so screen readers track Up/Down selection without focus ever leaving the query field.
- Result-count changes are announced via a **polite live region**; announcements SHOULD be debounced to the settled result set of a generation, not fired per streamed batch.
- **Every action MUST be keyboard-reachable**: primary action, action panel (Ctrl+K), navigation stack, settings. §5.7's model already guarantees this structurally; it is restated here as a hard requirement so no future surface regresses it.
- **Forced-colors / high contrast**: the design-token layer (§5.8) MUST resolve to system colors under `forced-colors: active`; no state may be conveyed by color alone.
- Both themes MUST meet **WCAG 2.1 AA contrast**.
- **M1 exit criterion**: the launcher is operable end-to-end (summon → type → navigate → execute → dismiss) with **Narrator** and with **NVDA** (§10).

### 5.13 Unicode, IME, and language

- **IME**: the §5.2 focus model exists precisely so IME works — WebView2 receives composition input only when its host window has keyboard focus. Composition events pass through to the query field untouched (§5.7), and M0 proves summon-and-type including IME input (§10).
- **Highlight offsets**: `match_ranges` in `SearchResults` are **UTF-16 code-unit indexes** into the transmitted `name` string (§4.3), so the frontend slices JavaScript strings directly — no byte-to-code-unit conversion and no corrupt highlights on non-ASCII names.
- **Matching**: NFC normalization and Unicode simple case folding are defined in §3.4 and apply identically to arena and query. CJK queries (typed via IME or otherwise) match via the **substring tier** — the documented supported path; the case-variant and camel-case tiers do not apply to unsegmented scripts.
- **Language**: v1 ships an **English-only** UI, with all user-facing strings externalized from day one (a string-catalog module; no literals in components) so later localization requires no restructuring. The ms-settings catalog's display names and synonyms are likewise English-only in v1 (§7.2).

## 6. Extension Platform

Extensions are TypeScript + React programs executed inside `yspot-exthost` (the bundled Node.js sidecar, §2) and rendered by the shared React frontend. Extensions never touch the DOM, never ship HTML/CSS, and never run in the shell or frontend process. Everything an extension can do flows through `@yspot/api` and is mediated by the host.

### 6.1 Extension anatomy

An extension is a directory (packaged as a zip for distribution) with a `package.json` manifest and one entry module per command. The manifest reuses standard npm fields plus a `yspot` block:

```jsonc
{
  "name": "github-repos",            // unique id: lowercase, [a-z0-9-], npm-compatible
  "version": "1.2.0",                // semver, MUST bump on every published change
  "main": "dist/index.js",
  "yspot": {
    "title": "GitHub Repos",         // human-readable, shown in root search
    "description": "Search and open your GitHub repositories",
    "icon": "icon.png",              // 512x512 PNG, bundled
    "min-api-version": "1.0.0",      // host refuses to load if its API version is older
    "commands": [
      {
        "name": "search-repos",      // unique within extension
        "title": "Search Repositories",
        "mode": "view",              // "view" (renders UI) | "no-view" (runs headless, may toast)
        "entry": "dist/search-repos.js",
        "keywords": ["gh", "repo"]   // extra root-search aliases
      }
    ],
    "rootSearch": {                  // optional: registers one command as a live root-search provider
      "command": "search-repos",     // MUST name a command declared above
      "minQueryLength": 2            // shell dispatches no query below this length
    },
    "permissions": ["network", "clipboard-write"],
    "preferences": [                 // rendered in YSpot settings, injected read-only at runtime
      { "name": "token", "title": "API Token", "type": "password", "required": true }
    ]
  }
}
```

Rules:
- The host MUST reject manifests with unknown permission strings, missing `min-api-version`, or commands whose entry file is absent.
- Source is TypeScript + React compiled against the published `@yspot/api` types (§6.7). Extensions MAY use npm dependencies; native addons (`.node` files) MUST be rejected at install time — all workers share the exthost process, and a native crash would defeat isolation.
- Command names, not extension names, appear in root search; the shell's per-user frecency store (§5) ranks them like any other result, keyed by namespaced command IDs (`<extension-id>/<command-name>`).
- `rootSearch` (optional) makes the named command a live root-search provider: once the root query reaches `minQueryLength`, the shell dispatches `search.query{gen, text}` and receives `search.results{gen, items[], isFinal}` (§4.7). Each generation has a hard 150 ms deadline — results arriving later are appended per the merge contract (§5.11) or dropped — and results are capped per extension. Declaring `rootSearch` is surfaced in store review, and when any enabled extension declares it, eager exthost spawn at shell idle is mandatory (§2.4) so the provider exists before the first keystroke.

### 6.2 Declarative component library v1

Extensions build UI from a fixed component set exported by `@yspot/api`. The frontend maps each node type to a real React component with YSpot's styling; unknown node types MUST be rejected (error card), never rendered as raw markup. Key props only (all components also accept `key`):

| Component | Key props |
|---|---|
| `List` | `isLoading`, `searchBarPlaceholder`, `onSearchTextChange(text)`, `throttle` (debounce callback ~200 ms), `filtering` (host-side fuzzy filter on/off) |
| `List.Section` | `title`, `subtitle` |
| `List.Item` | `title`, `subtitle`, `icon`, `accessories[]` (right-aligned text/icon badges), `keywords[]`, `actions` (an `ActionPanel`) |
| `Detail` | `markdown` (CommonMark + GFM tables, sanitized by the frontend; raw HTML blocks stripped), `isLoading`, `actions` |
| `Form` | `actions` (MUST contain an `Action.SubmitForm`), `isLoading` |
| `Form.TextField` / `Form.PasswordField` | `id`, `title`, `placeholder`, `defaultValue` |
| `Form.TextArea` | `id`, `title`, `placeholder`, `defaultValue` |
| `Form.Checkbox` | `id`, `label`, `defaultValue` |
| `Form.Dropdown` (+ `Form.Dropdown.Item`) | `id`, `title`, `defaultValue`; item: `value`, `title`, `icon` |
| `ActionPanel` (+ `ActionPanel.Section`) | `title` |
| `Action` | `title`, `icon`, `shortcut` (e.g. `{ modifiers: ["ctrl"], key: "enter" }`), `onAction()` |
| `Action.SubmitForm` | `title`, `onSubmit(values)` |
| `Action.OpenInBrowser` | `url`, `title?` — opens via the shell's default-browser handler; no permission needed (user-invoked) |
| `Action.CopyToClipboard` | `content`, `title?` — requires `clipboard-write` |

`Grid` (image-first tile layout) is deferred to component library v2; the node-type namespace reserves it. Icons are bundled asset paths, built-in icon names, or file paths the frontend resolves — never remote URLs fetched by the frontend.

### 6.3 Render pipeline

1. The command's React tree runs against a custom renderer (built on `react-reconciler`) inside the extension's worker. Host objects are plain JS nodes: `{ type, props, children }`.
2. On first commit the reconciler serializes the full tree as a JSON render tree. Subsequent commits emit RFC 6902 JSON Patch diffs against the reconciler's own last-committed tree.
3. Trees/patches travel as JSON-RPC notifications: worker → exthost main thread → shell (stdio/pipe) → frontend (WebView2 IPC). Patches for one commit are batched into a single message; the exthost coalesces commits faster than one frame (~16 ms) into the latest state.
4. The frontend applies patches to a mirror tree and maps node types to real React components. Functions cannot be serialized: callback props (`onAction`, `onSearchTextChange`, `onSubmit`) are replaced with stable handler ids; the frontend fires `invokeHandler(id, args)` back down the same pipe, and the reconciler dispatches to the original closure.
5. Ordering MUST be preserved per extension. Every `ui.render`/`ui.patch` carries `{seq, baseSeq}`: `seq` numbers the tree state the message produces, `baseSeq` names the state it applies on top of (a full `ui.render` carries `baseSeq: null`). The frontend applies a patch only if `baseSeq` matches its mirror tree's current `seq`; on mismatch it sends `ui.resyncRequest{instanceId}` (§4.6) and discards further patches until a fresh full `ui.render` arrives. The shell forwards the request to the exthost as `ui.resync{instanceId}` (§4.7), and the exthost responds with a full `ui.render` carrying the current `seq`. A single serialized tree or patch batch MUST NOT exceed 1 MiB; oversized commits fail the commit with a logged error, not a truncated render.

Latency budget: extension `setState` → visible pixel change SHOULD be < 50 ms for trees under 500 nodes.

### 6.4 Runtime isolation

- One `worker_threads` Worker per running extension — its own v8 isolate, module registry, and microtask queue. Extensions never share globals.
- Resource limits via Worker `resourceLimits`: `maxOldGenerationSizeMb: 128` by default (a manifest MAY request up to 512; the store flags it at review), `stackSizeMb: 4`. Exceeding heap kills only that worker. This paragraph is the single source of truth for the extension heap default; §8.2 references it without restating the number.
- CPU watchdog: the exthost main thread pings each worker's event loop every 1 s. A worker that fails to respond for > 5 s is presumed blocked and is terminated via `worker.terminate()`.
- Crash handling: on worker exit/termination the frontend swaps the extension's view for an error card (extension name, error summary, "Reload" action). The exthost auto-restarts the crashed worker with exponential backoff (1 s, 2 s, 4 s … capped at 60 s). Two circuit breakers apply, both normatively defined here (§2.6 cross-references them): the **per-extension worker breaker** — 5 crashes of one extension's worker within 10 minutes disables that extension until the user re-enables it in settings; and the **exthost-process breaker** — 3 crashes of the exthost process itself within 60 s disables all extensions until the user intervenes. The exthost, shell, and all other extensions MUST survive any single extension crash.
- Workers get no ambient capabilities: `process.env` is scrubbed, `child_process`/`fs`/raw `net` access from extension code is denied by module policy in the worker loader; all I/O goes through `@yspot/api` brokered calls (§6.5, §6.6).

### 6.5 Permission model

Permissions are declared in the manifest and granted by the user. The **shell is the sole authoritative enforcement point**: every `api.invoke` is validated in the shell against the per-extension grant table before execution (§4.7's wording is normative), and the shell rate-limits `api.invoke` per extension. The exthost broker (main thread) MAY pre-filter calls for fast-fail UX, but its checks carry no security weight — the entire exthost process runs third-party code and is untrusted (§2.2). Worker isolation is a robustness boundary (crash/memory containment), not a security boundary; the security boundary is the shell's JSON-RPC validation (§8.2).

| Permission | Grants |
|---|---|
| `network` | `fetch` via the broker. Manifest MAY narrow to a host allowlist (`"network": ["api.github.com"]`); narrowed manifests get a gentler prompt. |
| `clipboard-read` | Read clipboard text via API. |
| `clipboard-write` | Write clipboard / `Action.CopyToClipboard`. |
| `filesystem` | Read (or `read-write`) within user-approved directory scopes chosen at prompt time; paths outside granted scopes are rejected. |
| `shell-exec` | Launch processes via a brokered API. Highest-risk: the grant prompt shows the exact command line the first time each distinct program is invoked. |
| `system-processes` | Enumerate and terminate processes via the brokered `system.processes.list/kill` capability (the shell performs the Win32 mechanics). Pre-granted for the built-in Process Killer (§6.9, §7); store-gated (manual review) for third-party extensions. |

Rules: an API call whose permission is undeclared MUST fail immediately (no prompt). Declared permissions are prompted on first use — not at install — with extension name and requested capability. Every grant is listed and revocable per-extension in YSpot settings; revocation takes effect on the next API call without restarting the extension.

### 6.6 API surface v1 (`@yspot/api`)

- **Components** — everything in §6.2.
- **Search** — view commands receive the panel's search text via `List.onSearchTextChange`; `useSearch()` hook wraps it with debouncing and stale-response discarding. `popToRoot()`, `closeMainWindow()` for post-action navigation.
- **Storage** — `LocalStorage.getItem/setItem/removeItem/allItems`: per-extension namespaced KV persisted by the host, 5 MB cap per extension, no permission required (it is the extension's own data). `LocalStorage.setSecret/getSecret/deleteSecret`: secret values stored via the DPAPI mechanism of §8.4 (encrypted at rest with per-extension entropy), kept out of the plain KV; the entropy is an isolation convenience, not a secrecy boundary — enforcement is the shell broker refusing cross-extension secret reads.
- **Network** — `fetch(url, init)`: WHATWG-fetch-shaped, brokered, gated on `network`.
- **Clipboard** — `Clipboard.copy(text)`, `Clipboard.readText()`, gated on the two clipboard permissions.
- **Feedback** — `showToast({ style, title, message })` (in-panel, non-blocking) and `showHUD(text)` (transient overlay after the panel closes).
- **Preferences** — `getPreferenceValues()` returns manifest-declared preferences; `password` fields are persisted through the same DPAPI secret store as `setSecret` (§8.4), never on disk in plaintext.
- **Environment** — API version, extension id, theme (light/dark), and paths to the extension's own asset/support directories.

Additions to this surface are minor API versions; removals or behavior breaks are major versions checked against `min-api-version`.

### 6.7 Developer experience

- `yspot new` scaffolds a TypeScript extension (manifest, tsconfig, sample command, bundler config).
- `yspot dev` in an extension directory: builds, side-loads the extension into the running YSpot instance in development mode, watches sources, and hot-reloads the worker on change (< 2 s from save to updated UI). Extension `console.*` output and uncaught errors stream to the terminal.
- Types ship as `@yspot/api` on npm; the package contains type declarations and the worker-side runtime stubs, versioned in lockstep with the host API version.
- Dev-mode extensions are visibly badged in root search and are exempt from signature checks but NOT from the permission model or resource limits.

### 6.8 Distribution

- **M3**: local install (`yspot install <dir|zip>`) and install-from-git-URL (`yspot install https://github.com/...`). Both display the full manifest — including every declared permission — before completing.
- **M4**: curated store. Submissions are reviewed; accepted packages are zips signed by the YSpot store key (detached signature verified by the shell at install and at load). Non-store installs remain possible but are labeled "unreviewed" in settings. Updates are pulled by the shell, verified, and staged; a running extension is swapped on next launch of one of its commands.

### 6.9 Dogfooding rule

Five built-in features MUST be implemented as extensions running purely on the public `@yspot/api` before the API is declared stable (end of M3): the **Settings & Control Panel catalog** (§7.2), **Quicklinks** (§7.7), **Web Search** (§7.7), the **Emoji Picker** (§7), and the **Process Killer** (§7). All five use the same manifest format, component library, render pipeline, permission model, and isolation as third-party extensions. Dogfooded built-ins MAY have their declared permissions pre-granted at install; the Process Killer's `system-processes` capability (§6.5) is pre-granted for the built-in and store-gated for third parties.

Clipboard history, window management, system commands, and the calculator engine are explicitly NOT covered by this rule: per §7.8's placement table they remain native (in-shell) in v1, and the brokered privileged capabilities they would require (`clipboard.*`, `window.*`) are deferred past v1. Within the dogfooded five, any capability the API lacks is an API gap to be fixed, not a private backdoor to be added: if the public API cannot express these features, it is not ready for third parties.

## 7. Built-in Commands and Windows Integration

All built-ins in this section are implemented natively (Rust in the shell, or the indexer service where noted) — never as exthost extensions — unless explicitly marked otherwise in §7.8. Rationale: they need Win32 window/session context, or they sit on the hot path of the keystroke latency budget (§2.5).

### 7.1 App Launching

- **Enumeration.** The shell MUST enumerate the `AppsFolder` virtual shell folder (`SHGetKnownFolderItem(FOLDERID_AppsFolder)` → `IShellItem` → `IEnumShellItems`; equivalently parse `shell:AppsFolder`). This one namespace uniformly yields Win32 apps (Start Menu links) and UWP/packaged apps. For each item, read the display name (`SIGDN_NORMALDISPLAY`) and the AppUserModelID via `IShellItem2::GetString(PKEY_AppUserModel_ID)`. Re-enumerate on a debounced schedule (on popup show if > 5 min stale, and every 30 min in the background); a full re-enumeration MUST NOT block the results pipeline.
- **Launching.** Packaged apps: `IApplicationActivationManager::ActivateApplication(aumid, args, AO_NONE, &pid)` (CLSID `ApplicationActivationManager`), with `ShellExecuteEx` on `shell:AppsFolder\<AUMID>` as fallback. Win32 apps: `ShellExecuteEx` on the shell item (invokes the `.lnk`, preserving its arguments/working dir). "Run as administrator" secondary action uses the `runas` verb (Win32 only; hidden for packaged apps).
- **Icons.** Extract via `IShellItemImageFactory::GetImage`, requested at the logical size (32/48 px) multiplied by the target monitor's scale factor (Per-Monitor v2 DPI, §5.3), encode to PNG, cache on disk keyed by AUMID + physical pixel size. (Shipped deviation: the key carries no source mtime and the disk entry expires after seven days instead. `AppsFolder` items have no single source file whose mtime is meaningful — a packaged app's icon comes from its manifest and its assets, a Win32 app's from whatever binary the `.lnk` points at — so an mtime key would be either wrong or a per-item resolve on every extraction. The cost of the TTL is that an icon changed by an app update is stale for at most a week.) The frontend receives `data:` URIs or a local asset path; icon extraction runs on a worker thread pool, never on the query path.
- **Ranking.** App results are frecency-ranked (launch count decayed by recency) by the shell's per-user frecency store (`%LOCALAPPDATA%\YSpot\frecency.db`, SQLite — the same shell-owned store that ranks files, commands, and extensions; apps are keyed by AUMID or shortcut path). Exact-prefix name matches MUST outrank frecency.

### 7.2 Settings and Control Panel

- **Settings pages.** YSpot ships a static catalog of `ms-settings:` URIs (name, synonyms, keywords, URI) derived from Microsoft's published reference ("Launch the Windows Settings app", learn.microsoft.com). Launch is `ShellExecuteEx` on the URI (e.g. `ms-settings:display`, `ms-settings:privacy-microphone`). The catalog format MUST be data-driven (JSON shipped with the app) so it can be updated without a code release. Catalog display names and synonyms are English-only in v1 — an explicit decision; localization of the catalog is deferred.
- **Capability filtering.** Page availability varies by Windows version, SKU, and hardware (e.g. battery-saver pages on battery-powered devices only, cellular pages only with a WWAN adapter). Each catalog entry MAY declare a gate (`min_build`, `requires: battery|cellular|pen|touchpad|bluetooth`); the shell evaluates gates at startup (e.g. `GetSystemPowerStatus` for battery, radio/adapter presence for cellular/Bluetooth) and hides ungated-out entries rather than launching URIs that no-op to the Settings home page.
- **Control Panel.** Index classic Control Panel items by their documented canonical names and launch via `control.exe /name Microsoft.<CanonicalName>` (with `/page` where a sub-page is catalogued). Items exposed only as shell namespace objects launch via `ShellExecuteEx` on their `shell:::{CLSID}` parsing path.

### 7.3 File Search UX

- **Root query.** Top-level queries include inline fuzzy filename matches from yspot-indexd (substring/fuzzy/camel-case match quality), interleaved with apps/settings/commands under the shell's global ranker, which applies per-user frecency uniformly across all sources (§7.1's store). Inline file results are capped (default 5) to keep the root list scannable.
- **File Search command.** A dedicated command with the full result list and query filters: `kind:` (document/image/audio/video/folder/archive — mapped to extension sets), `ext:`, `path:` (substring on parent path — carried as the `path_substr` filter in §4.3's `SearchQuery`), and `content:` which routes the remaining terms to the Tantivy full-text index (opt-in scopes, §3.5). Filters combine with AND semantics. Size and date filters (`size:`, `modified:`) are deferred past v1 — the pipe protocol carries no size/mtime filters (§4.3); the `size`/`mtime` values shown in result rows are display metadata only, lazily stat-ed by the service for the returned page (≤ 32 files) before send.
- **Preview pane.** Toggleable pane (default on in File Search): text/code files render in the frontend with syntax highlighting, capped at 256 KB read; images, PDFs, and other rich types render thumbnails via `IShellItemImageFactory::GetImage` (which delegates to registered `IThumbnailProvider` handlers, so any format with an installed thumbnail handler works). Thumbnail extraction runs out of the query path and results are cached.
- **Actions.** Open (`ShellExecuteEx`, default verb); Open With (`SHOpenWithDialog`); Reveal in Explorer (`SHOpenFolderAndSelectItems`); Copy Path / Copy File (`CF_HDROP`); Delete to Recycle Bin (`IFileOperation::DeleteItem` with `FOF_ALLOWUNDO`; MUST NOT use permanent-delete APIs, and MUST show its own confirm for multi-item deletes). All file actions run in the unelevated shell — never in the elevated service.

### 7.4 Clipboard History

- **Capture.** The shell registers a hidden message-only (`HWND_MESSAGE`) window with `AddClipboardFormatListener` and handles `WM_CLIPBOARDUPDATE`. (Open item: `WM_CLIPBOARDUPDATE` is posted directly to registered listeners rather than broadcast, so a message-only window is expected to receive it, but no authoritative doc states it — verify in the M0 smoke tests; the fallback is a hidden top-level window.) On update it snapshots text (`CF_UNICODETEXT`), images (`CF_DIB`/`CF_DIBV5`, re-encoded to PNG), and file lists (`CF_HDROP`, stored as paths). Source app is resolved via `GetClipboardOwner` → `GetWindowThreadProcessId` → process image name, falling back to the current foreground window's process.
- **Exclusions.** Entries advertising the `ExcludeClipboardContentFromMonitorProcessing` registered clipboard format (the documented format name — the convention password managers use) MUST NOT be stored; entries carrying the documented companion formats `CanIncludeInClipboardHistory` or `CanUploadToCloudClipboard` with a DWORD value of 0 MUST likewise be excluded. Per-app exclusion list is user-configurable.
- **Storage.** Encrypted at rest, per-user. (**Amended 2026-09-03**, superseding "SQLite (SQLCipher); the database key is generated per-user and protected with DPAPI". Shipped instead: a plain SQLite file in which **every value a row carries — the content AND the preview — is encrypted individually with DPAPI** (`CryptProtectData`, `CRYPTPROTECT_UI_FORBIDDEN`). The guarantee is the same, the ciphertext is bound to the user's login credentials, and there is no key for YSpot to generate, store or leak; SQLCipher would have added a key-management layer and dragged OpenSSL into every build and CI run for it. Encrypting the preview is not optional: a clipboard entry is usually shorter than the preview length, so a plaintext preview column would hand over exactly what the encryption is for. What this leaves in the clear is row metadata — timestamp, source app, kind — the accepted trade. Because each preview is a DPAPI call, the **searchable** window is the most recent 1,000 entries, decrypted into memory on a background thread at startup; retention on disk keeps the full set.) Retention is configurable (default 30 days / 10,000 entries; images capped at 500 MB total, oldest-first eviction).
- **Paste.** Selecting an entry hides the popup, restores focus to the previous foreground window, writes the entry to the clipboard, and injects Ctrl+V via `SendInput`. ("Paste as plain text" is **not a separate action while only text is captured**: the history stores `CF_UNICODETEXT` and file paths and nothing else, so every paste already writes plain text. It becomes a real distinction the day rich formats are captured.) This history is deliberately independent of Windows' Win+V cloud clipboard: no `Windows.ApplicationModel.DataTransfer` history APIs, no cloud sync, and it works with Win+V disabled.

### 7.5 Window Management

- **Enumeration.** `EnumWindows`, filtered to alt-tab-eligible windows: `IsWindowVisible`; the window is its root owner's last active popup (`GetAncestor(GA_ROOTOWNER)` + `GetLastActivePopup` walk); `WS_EX_TOOLWINDOW` not set (unless `WS_EX_APPWINDOW` is); and — required to exclude ghost windows of suspended/background UWP apps — `DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, …)` returns 0 (`DWMWA_CLOAKED` = 14, the query attribute; `DWMWA_CLOAK` = 13 is the corresponding set attribute). Titles via `GetWindowText`; icons via `WM_GETICON`/`GCLP_HICON` with the owning process's exe icon as fallback.
- **Switching.** `ShowWindow(SW_RESTORE)` if minimized (`IsIconic`), then `SetForegroundWindow`. Windows' foreground-lock rules mean a background process may be denied and the taskbar button flashes instead; the shell MUST apply the accepted workaround — synthesize a no-op Alt key press/release via `SendInput` (making YSpot's thread the last-input thread) immediately before `SetForegroundWindow`, with `AttachThreadInput` to the current foreground thread as a fallback path.
- **Move/resize.** `SetWindowPos` against the target monitor's work area (`MonitorFromWindow` + `GetMonitorInfo`, per-monitor-DPI aware). Preset commands: left/right halves, thirds (left/center/right, and two-thirds variants), quarters, maximize, center, and move-to-next-monitor. Windows with fixed-size constraints keep their size and are positioned only.
- **Other actions.** Always-on-top toggle: `SetWindowPos` with `HWND_TOPMOST`/`HWND_NOTOPMOST`. Minimize: `ShowWindow(SW_MINIMIZE)`. Close: `PostMessage(WM_CLOSE)` — never `TerminateProcess`.

### 7.6 System Commands

| Command | Mechanism |
|---|---|
| Lock | `LockWorkStation()` |
| Sleep | `SetSuspendState(FALSE, FALSE, FALSE)` (powrprof.dll); hibernate variant passes `TRUE` for the first arg if hibernation is enabled |
| Shutdown / Restart | Enable `SE_SHUTDOWN_NAME` via `OpenProcessToken` + `AdjustTokenPrivileges`, then `ExitWindowsEx(EWX_SHUTDOWN \| EWX_HYBRID_SHUTDOWN, …)` / `ExitWindowsEx(EWX_REBOOT, …)` (`EWX_HYBRID_SHUTDOWN` = 0x00400000, Win8+, valid only in combination with `EWX_SHUTDOWN`); MUST confirm before executing (no accidental Enter) |
| Empty Recycle Bin | `SHEmptyRecycleBin(NULL, NULL, SHERB_NOCONFIRMATION \| SHERB_NOPROGRESSUI \| SHERB_NOSOUND)` after YSpot's own confirm showing item count via `SHQueryRecycleBin` |
| Volume set/mute | Core Audio: `IMMDeviceEnumerator::GetDefaultAudioEndpoint(eRender, eConsole)` → `IAudioEndpointVolume::SetMasterVolumeLevelScalar` / `SetMute` |
| Brightness | Internal panels: WMI `WmiMonitorBrightnessMethods::WmiSetBrightness` (`root\wmi`). External monitors: DDC/CI via `GetPhysicalMonitorsFromHMONITOR` + `SetMonitorBrightness` (dxva2.dll). Hide the command if neither path reports a controllable display |

**Wi-Fi / Bluetooth toggles.** The WinRT `Windows.Devices.Radios.Radio` API (`GetRadiosAsync`, filter by `RadioKind`, `SetStateAsync`) requires package identity: `radios` is a package-manifest device capability, and unpackaged callers fail with `APPMODEL_ERROR_NO_PACKAGE`-class errors. This is verified against the platform documentation, not an inference. Therefore: toggles are enabled only in MSIX-packaged builds (M4), where `RequestAccessAsync` user consent is still required; unpackaged builds ship the same commands as deep links to `ms-settings:network-wifi` / `ms-settings:bluetooth` instead. Airplane mode has no supported programmatic toggle and is a deep link (`ms-settings:network-airplanemode`) only.

### 7.7 Calculator, Web Search, Quicklinks, Snippets

- **Calculator.** The root query is continuously parsed by a deterministic expression engine in the shell (Rust; no JS `eval`, no external process): arithmetic, percentages, bit/hex, unit conversions (`12 mi in km`, `72 f in c`), date math (`days until dec 25`), and currency (`100 eur in usd`) using rates fetched at most every 12 h and cached; currency silently degrades to "rates unavailable" offline. The result renders as the first row; Enter copies it. Parse+eval runs on the shell's routing hot path and MUST fit inside its share of the §2.5 latency budget (shell routing ≤ 3 ms).
- **Web search fallback.** When nothing scores above threshold, show "Search the web for …" rows for the user's configured engines (default set: Google, Bing, DuckDuckGo, Wikipedia; each an editable URL template). Opens via `ShellExecuteEx` on the URL → default browser.
- **Quicklinks.** User-defined entries: name, icon, URL or file/folder path template containing `{query}`; the argument is percent-encoded (RFC 3986) before substitution for URLs. A Quicklink with `{query}` accepts an inline argument (Tab to fill).
- **Snippets.** Named text snippets with placeholders (`{clipboard}`, `{date:FORMAT}`, `{cursor}` — cursor supported only for the final caret position). Inserted via the clipboard paste path of §7.4 (restore focus, `SendInput` Ctrl+V), then the previous clipboard contents are restored. v1 explicitly does NOT do global abbreviation expansion — no low-level keyboard hook (`SetWindowsHookEx(WH_KEYBOARD_LL)`) is installed; that is deferred and will be opt-in if ever shipped.

### 7.8 Placement: shell-native vs. extension-eligible

| Built-in | v1 home | Extension later? |
|---|---|---|
| App launching, file search UX | Shell (+ indexd) | No — hot path, latency budget |
| Clipboard history | Shell | No — needs clipboard listener window, `SendInput`, foreground restore |
| Window management | Shell | No — needs `EnumWindows`/`SetForegroundWindow` foreground-lock workarounds in-process |
| System commands, volume/brightness | Shell | No — token privileges, COM/WMI in trusted process |
| Snippets, calculator | Shell | Engine stays native; snippet *management UI* MAY move to an extension |
| Settings/Control Panel catalog, Quicklinks, web search | Shell (data-driven) | Yes — pure catalog + `ShellExecuteEx`; together with Emoji Picker and Process Killer these are the five dogfooded M3 extensions (§6.9), migrated onto the public `@yspot/api` |
| Emoji Picker | Extension (exthost) | Ships as an extension from day one — dogfooded in M3 (§6.9) |
| Process Killer | Extension (exthost) | Ships as an extension from day one via the brokered `system.processes.list/kill` capability — dogfooded in M3 (§6.9) |

Two v1 built-ins are extension-first — they run purely on the public `@yspot/api` and are two of the five dogfooded extensions of §6.9/M3:

- **Emoji Picker** — searches the Unicode emoji set by name/keyword/shortcode and copies the selection to the clipboard; pure data + List UI, no privileged capabilities.
- **Process Killer** — lists running processes (name, PID, memory) filtered by the query and terminates the selection; uses the brokered `system.processes.list` / `system.processes.kill` capability, executed by the unelevated shell (so it can only terminate processes the user's token can open with `PROCESS_TERMINATE`). This capability is pre-granted for the built-in and store-gated for third parties.

If the "No" domains above are ever opened to extensions, it will be through host-brokered privileged capabilities (`clipboard.*`, `window.*`) where the shell performs the Win32 mechanics in-process — never raw Win32: extension workers have no window handles and no input injection rights. These privileged brokered capabilities are explicitly deferred past v1; the "No" rows above are the v1 shipping truth. The only brokered privileged capability in v1 is `system.processes.list/kill` (Process Killer, above).

## 8. Security Model

### 8.1 Threat model: the elevated service (`yspot-indexd`)

`yspot-indexd` runs elevated and parses attacker-influenceable data. Everything it consumes is untrusted:

| Untrusted input | Threat | Required mitigation |
|---|---|---|
| Raw MFT records (`FSCTL_ENUM_USN_DATA` output) | Malformed/hostile on-disk structures (crafted USB volume, disk corruption) cause OOB reads, integer overflow, allocation bombs in an elevated process | Memory-safe Rust parser; every length/count field validated against hard caps before use; no allocation sized from an on-disk length field without a cap (MFT record ≤ 4 KiB, attribute name ≤ 255 UTF-16 units, path depth ≤ 512); parser is `#![forbid(unsafe_code)]` |
| USN journal records (`FSCTL_READ_USN_JOURNAL`) | Same class; additionally journal-wrap/reset confusion causing stale index state | Same parsing rules; on `ERROR_JOURNAL_ENTRY_DELETED` / journal ID change, MUST discard and re-enumerate the volume rather than guess |
| Named-pipe messages | Any local process can connect and send arbitrary bytes; elevation-of-privilege target | Length-prefixed frames with the asymmetric caps of §4.2, stated here identically: **client→service 1 MiB, service→client 16 MiB**; an oversized inbound frame ⇒ disconnect that client; strict schema validation before dispatch; malformed frame ⇒ disconnect that client only; per-client rate limiting; no client-supplied strings interpreted as paths for writes |
| Its own index files (`%ProgramData%\YSpot\index`) | Tampering by non-admin users to poison the elevated process | Directory ACL: writable by the service account and Administrators only; index load treats file contents as untrusted (same cap rules); corrupt index ⇒ rebuild, never crash-loop |
| File contents extracted for the content index (PDF, OOXML, …) | Crafted documents exploit format-parser bugs — the most exploit-prone code after the MFT parser | All format parsing runs in `yspot-extract` (§2.2), a separate worker spawned via `CreateProcessAsUser` with a write-restricted token + job object and `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY` (no child processes) applied at creation; files passed by duplicated read-only handle (never by path — the worker needs no filesystem rights); results returned over an anonymous pipe with the same frame-cap rules as the main pipe; hang/crash ⇒ kill after 10 s, quarantine the offending file (never retry), restart worker (§2.6) |

Normative requirements:

- The service MUST be written in Rust. `unsafe` is permitted only in small audited modules wrapping Win32 calls (`DeviceIoControl`, pipe APIs); all parsing crates MUST compile with `#![forbid(unsafe_code)]`.
- CI MUST run `cargo-fuzz` targets for (a) the MFT/attribute parser, (b) the USN record parser, (c) the pipe wire protocol, and (d) every `yspot-extract` format parser (PDF, OOXML, etc.), on every merge to main, with a persisted corpus. A reproducible fuzz crash is a release blocker.
- The pipe MUST be created with `PIPE_REJECT_REMOTE_CLIENTS` and a DACL granting access only to SYSTEM, Administrators, and INTERACTIVE users — no remote, no anonymous, no service-to-service surprises.
- The service performs zero network I/O. It MUST NOT link a TLS/HTTP stack; update checks live in the unelevated shell.
- **Service account: LocalSystem, with startup privilege stripping.** Opening raw volume handles (`\\.\C:`) and issuing `FSCTL_ENUM_USN_DATA` requires administrative access to the volume device object. A virtual service account (`NT SERVICE\yspot-indexd`) only gains that access by joining the Administrators group — at which point it is effectively admin anyway, with added installer fragility (group membership surviving upgrades, per-volume DACL surgery for hot-plugged drives). We therefore run as LocalSystem and reduce it: at startup, before touching any untrusted input, the service MUST call `AdjustTokenPrivileges` to remove every token privilege except `SeChangeNotifyPrivilege`, `SeBackupPrivilege`, and `SeManageVolumePrivilege`, and SHOULD enable process mitigation policies via `SetProcessMitigationPolicy`: mandatory ASLR (`ProcessASLRPolicy` — note it affects only images loaded after the policy is set) and CIG (`ProcessSignaturePolicy`). A no-child-processes policy is deliberately NOT on this list: it is not a policy `SetProcessMitigationPolicy` accepts at runtime, and the service must spawn `yspot-extract` anyway. The child-process restriction applies to the **extractor**, set by the service at spawn via `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY` (see the extraction row above). If later testing proves a virtual service account with an explicit volume-device DACL grant works reliably across hot-plug, we downgrade to it in a point release.

### 8.2 Extension sandboxing (recap of §6)

- One worker thread (v8 isolate) per extension inside `yspot-exthost`, with a per-extension heap cap (default and maximum per §6.4, the single source for those numbers) and cooperative CPU watchdog; a misbehaving extension is killed and shown as an error card — the host survives. Crash-loop breakers (per-extension and exthost-process) are defined in §6.4.
- Extensions declare permissions in their manifest (`clipboard-read`, `network`, `filesystem` with explicit path scopes). The **shell is the sole authoritative enforcement point**: every `api.invoke` is validated in the shell against the per-extension grant table before execution (§4.7 wording is normative), and the shell rate-limits `api.invoke` per extension. The exthost broker MAY pre-filter undeclared calls for fast-fail UX, but its checks carry no security weight.
- Worker isolation is a **robustness** boundary (crash containment, memory caps), not a security boundary. `worker_threads` share a process; the security boundary is the shell's JSON-RPC validation of every brokered call.
- Extensions never emit HTML/CSS/DOM. The frontend renders only the fixed component library from a validated JSON render tree; unknown component types or props are dropped.
- Extensions talk only to `yspot-exthost`; they have no handle to the service pipe and cannot issue index queries except through the audited host API.

### 8.3 What YSpot never does

- **No keylogging.** Global input is limited to `RegisterHotKey` for the summon hotkey. YSpot MUST NOT install low-level keyboard hooks (`WH_KEYBOARD_LL`). Clipboard history uses `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE` (event-driven, no polling, no input interception), is off until enabled in **Settings**, and honors the documented clipboard-privacy formats — `ExcludeClipboardContentFromMonitorProcessing`, `CanIncludeInClipboardHistory`, and `CanUploadToCloudClipboard` — never capturing content flagged for exclusion.
- **No network from core** except the update check and opt-in crash reports (§8.5). Telemetry is opt-in, default off, and its full payload schema is published.
- **No content upload.** Index data, previews, and search queries never leave the machine.

### 8.4 Secrets handling

Secrets have exactly **one** mechanism in the product, used for both extension secret storage (`LocalStorage.setSecret/getSecret/deleteSecret`, §6.6) and extension password-type preference fields: DPAPI — `CryptProtectData`/`CryptUnprotectData` (`dpapi.h`) — which is the correct primitive: per-user by default (only the same Windows account can decrypt, keys managed by the OS). Windows Credential Manager is NOT used. We MUST NOT pass `CRYPTPROTECT_LOCAL_MACHINE` (that would let any local user decrypt). The extension ID is mixed into the `pOptionalEntropy` parameter, but this is an isolation convenience, not the enforcement: the actual enforcement is the shell broker refusing cross-extension secret reads (§8.2's grant-table validation). Encrypted blobs live under `%LOCALAPPDATA%\YSpot\secrets\<ext-id>`. The elevated service stores no secrets at all.

### 8.5 Crash reporting and diagnostics logging

**Crash reporting.** Crash capture is per-process, matching each process's privilege level:

- **Service (`yspot-indexd`)**: WER LocalDumps registry configuration (`HKLM\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\yspot-indexd.exe`), written by the service MSI; minidumps land in `%ProgramData%\YSpot\crashes`.
- **Shell, exthost, extractor**: in-process minidump handler (or WER LocalDumps for the per-user executables); dumps land in `%LOCALAPPDATA%\YSpot\crashes`.
- **Consent**: crash-report upload is opt-in, with consent captured in onboarding (§5.9). No consent ⇒ dumps stay local and are rotated away.
- **Upload**: performed only by the unelevated shell — the service never talks to the network (§8.1). Uploaded dumps are PII-scrubbed: file paths in dump metadata and attached logs are hashed before transmission.
- **Metric**: a *session* is one shell process lifetime with at least one summon; a session is *crash-free* if no YSpot process (service, shell, exthost, extractor, frontend renderer) wrote a crash dump during it. "Crash-free sessions > 99%" in §10 M4 is computed from opted-in telemetry over this definition.

**Diagnostics and logging.** Every process logs to a fixed location matching its privilege level:

- Service → `%ProgramData%\YSpot\logs`. Security events (pipe squatting per §4.1, admin-only op rejections) are additionally written to the Windows Application Event Log under a registered `YSpot` event source (registered by the service MSI, removed on uninstall per §9.4).
- Shell, exthost, frontend, extractor → `%LOCALAPPDATA%\YSpot\logs`.
- Format: structured line format (one JSON object per line: timestamp, level, process, component, message, fields). Rotation: size-based, 5 files × 10 MB per process. Default level: Info.
- §5.9's "log export" is defined as a zip of these two directories (the service directory is world-readable; no elevation needed to export).

## 9. Packaging, Installation, and Updates

### 9.1 Install model: per-user shell + separate service MSI

The product installs as **two artifacts** with different elevation requirements:

1. **Shell package (per-user, unelevated).** Shell + frontend + exthost runtime install to `%LOCALAPPDATA%\Programs\YSpot` via an unelevated per-user installer. Because the binaries live in a user-writable location, the Tauri v2 updater (§9.3) can update them without elevation. **Autostart:** the shell writes its own `HKCU` Run value on first run for each user, gated by the onboarding autostart toggle (§5.9) — the installer does not write it. Uninstall removes the current user's value; other users' stale entries self-heal (exe gone ⇒ dead entry).
2. **Service MSI (per-machine, elevated).** A separate **MSI built with WiX** (v4+), using `ServiceInstall`/`ServiceControl` for `yspot-indexd`. It is launched by onboarding step 2 on user consent (one UAC prompt); skipping the step leaves the product in degraded/portable mode — §5.9's skip path and §9.5's portable mode are the same code path. MSIX is rejected for the service: MSIX-packaged services require Windows 10 2004+ (we support 1809+), require the `packagedServices` restricted capability (plus `localSystemServices` for LocalSystem services), and Microsoft states this capability is generally not approved for Store submissions. One UAC elevation for the service install is acceptable and expected for a product whose value proposition includes an MFT-reading service.

**WebView2 Runtime bootstrap.** The Evergreen WebView2 Runtime is preinstalled on Windows 11 only; on Windows 10 — including enterprise/LTSC and clean 1809 images — it may be absent. The shell installer MUST detect a missing or too-old Evergreen Runtime and run the **Evergreen Bootstrapper (bundled)** before first launch. Portable mode (§9.5) performs the same check at first run.

**CPU architecture.** v1 ships **x64 only**; ARM64 is explicitly deferred. The installer warns on ARM64 hosts, and the performance budgets (§1.2, §10) are unvalidated under x64 emulation on ARM devices.

A Store-distributed MSIX of the *no-service* portable mode (§9.5) MAY ship in M4.

### 9.2 Code signing

All shipped PE binaries (service, shell, exthost Node runtime, updater) and both installers — the per-user shell installer and the service MSI (§9.1) — MUST be Authenticode-signed with a timestamp. Reality check on SmartScreen: since 2024, EV certificates no longer grant immediate SmartScreen reputation — EV-signed files build reputation the same way OV-signed ones do. Therefore: use an OV certificate or Azure Trusted Signing (cheaper, HSM-backed, Microsoft-managed identity), keep the publisher identity absolutely stable across releases so reputation accrues, and expect SmartScreen warnings for early downloads regardless of certificate class. Budget for this in beta comms ("More info → Run anyway" screenshot in docs).

### 9.3 Auto-update

Two independent channels, because elevation requirements differ:

- **Shell + frontend + exthost** (unelevated): Tauri v2 updater, updating the per-user install under `%LOCALAPPDATA%\Programs\YSpot` (§9.1) — no elevation ever required. Signed update manifest (updater's own Ed25519 signature *in addition to* Authenticode), background download, apply on next launch. Delta packages SHOULD be used once size warrants it.
- **Service** (elevated): updated only via minor upgrade of the service MSI (§9.1). Flow: shell's update check detects a service-version bump → downloads the full MSI to `%LOCALAPPDATA%\YSpot\updates` → verifies Authenticode publisher before anything else → notifies the user ("Restart to update — needs administrator approval") → on consent, launches `msiexec /i` which triggers the standard UAC prompt → MSI stops, replaces, restarts the service. The service update is never silent and never auto-elevates.
- **Version skew**: the pipe protocol carries a version handshake; service N MUST serve clients N-1, because the shell updates without elevation and the service may lag until the user consents to UAC.

### 9.4 Uninstall

Uninstall is two-part, matching the install model (§9.1). The service MSI uninstall MUST: stop the service and delete its SCM service entry; delete `%ProgramData%\YSpot` (indexes, config, logs — indexes can exceed hundreds of MB and users notice leftovers); and remove the `YSpot` Event Log source registration (§8.5). The per-user shell uninstaller MUST: remove the shell binaries, Start Menu entries, tray registration, and the current user's Run-key value; other users' stale Run values self-heal (exe gone ⇒ dead entry). There are no pipe or firewall artifacts to remove by design — named pipes are ephemeral kernel objects that vanish with the server's last handle, and the service performs zero network I/O (§8.1); the automated uninstall test verifies none exist. Per-user data (`%LOCALAPPDATA%\YSpot`: settings, clipboard history, extension secrets, logs, crash dumps) is removed after an explicit checkbox, default checked. `sc query yspot-indexd` after uninstall MUST report the service does not exist — this is an automated release test.

### 9.5 Portable / no-admin mode

A per-user build (plain ZIP + the same per-user installer as §9.1) MUST work with zero elevation: no service, no MFT index. File search degrades to the **shell's own** Windows Search OleDB provider (`Search.CollatorDSO` over SystemIndex) — the identical shell-side code path that full installs use when the service answers a scope with error `107 SCOPE_UNSUPPORTED` (§3.1); portable mode is not a second implementation. App launching, settings search, calculator, clipboard history, window management, and extensions all work unchanged. Portable mode performs the WebView2 Evergreen Runtime check of §9.1 at first run. The UI MUST show an unobtrusive "fast indexing off — install the full version" hint, not a nag.

## 10. Milestones

### M0 — Latency-proving skeleton (2–3 weeks)
**Goal:** prove the performance budgets on real hardware before building anything else. **Scope:** service skeleton with MFT enumeration + USN tailing on one NTFS volume; query over the pipe covering the substring tier plus the §3.4 prefilters for fuzzy (bit-sliced character classes) and camel-case (fixed-stride initials column); bare Tauri window with hotkey summon using the **activate-and-restore focus model (§5.2), including IME input** — M0 proves the SetForegroundWindow-on-hotkey + focus-restore path; a results list wired to the pipe; benchmark harness with automated measurement (endpoints: injected keydown timestamp → DWM present of the updated frame, via ETW `Microsoft-Windows-DWM` present events); smoke tests for the two flagged open items (AddClipboardFormatListener on a message-only window; hidden-WebView2 render throttling).

**Reference machines** (every performance budget in this spec is measured on these):
- **Machine A**: 8-core ≥ 3.5 GHz, 32 GB RAM, NVMe SSD, 120 Hz display, current Windows 11.
- **Machine B** *(deferred)*: ~2015 dual-core (i5-5200U class), 8 GB RAM, SATA SSD, 60 Hz display, Windows 10 1809.

**Machine B amendment (M0 sign-off decision):** no such hardware is currently available. M0 signs off on the Machine A column alone; the Machine B column of `docs/M0.md` remains open as a standing post-M0 obligation to be filled when low-end hardware is acquired, and until then the shared-runner CI benchmark (advisory, 300k corpus) stands in as the only low-end tripwire. Any M1+ feature whose budget is plausibly CPU- or refresh-rate-bound on 2015-class hardware must state so in review rather than assume the missing column. The CI latency harness assignment moves with the deferral: it runs on Machine B when Machine B exists.

M0 was measured on Machine-A-class hardware with one recorded variance: 16 GB RAM rather than 32 (immaterial to the gates — RSS passed at 163 MB against the 200 MB cap; see `docs/M0.md`).

**Exit criteria (measured, not eyeballed):** hotkey→visible < 50 ms p95; **results data available to the frontend ≤ 20 ms p95 after keydown, applied in the next rAF after arrival** (per §2.5's decomposition; end-to-end pixels ≈ budget + up to 2 vsync periods — ~33 ms at 60 Hz, ~16 ms at 120 Hz) on a ≥1M-file volume — measured for substring queries **and** worst-case 2-char and 3-char fuzzy queries; initial index of 1M files < 15 s on the SSD reference machine with quiescent disk (relaxed HDD target: 90 s); USN event→searchable < 1 s; service RSS < 200 MB at 1M files (filename-index budget, §1.2). Numbers recorded on Machine A (Machine B deferred per the amendment above). **If any budget fails, M1 does not start** — architecture is revisited instead. One correctness item is explicitly ruled OUT of this gate: issue #7 (fuzzy-tier truncation returns provably wrong pages on real corpora when `FUZZY_CAP` binds) carries into M1 as entry work, with the bench's fuzzy-survivor table (its `capped` column) as the standing tripwire — recorded here so the ruling was deliberate, not a default.

### M1 — Core launcher, daily-drivable (4–6 weeks)
**Goal:** replace PowerToys Run for the author full-time. **Scope:** Win32 + UWP app launching (Start Menu, `AppsFolder`), ms-settings catalog + Control Panel tasks, file search UI with actions (open, reveal, copy path, copy file), calculator, clipboard history and window management (native, §7.4/§7.5), tray icon, autostart, settings window, multi-monitor placement, onboarding, shell-side OleDB fallback for non-NTFS scopes (§3.1, error 107 routing), crash reporting (opt-in, §8.5).
**Exit criteria:** author dogfoods exclusively for 2 weeks with no daily crash; all M0 budgets still green under CI benchmark; portable mode functional; launcher operable end-to-end with Narrator and NVDA (§5 accessibility NFR).

### M2 — Content search (3–4 weeks)
**Goal:** ship the differentiator. **Scope:** Tantivy full-text index with opt-in scopes UI, incremental reindex from USN events, text/PDF/Office extractors running in the sandboxed `yspot-extract` worker (§2.2, §8.1), `content:` query prefix + ranking merge, preview pane with hit highlighting.
**Exit criteria:** content query over a 50k-document scope returns < 100 ms p95; content freshness measured: a small text document saved into an indexed scope is findable by its new content within 10 s (D1, §1.5 — fresh-change commit + NRT reader reload within 3 s of extraction per §3.5); indexer stays within idle-priority I/O; scope add/remove reflects within 60 s; RAM budgets still met.

### M3 — Extension platform (5–6 weeks)
**Goal:** third-party-ready extension runtime. **Scope:** `yspot-exthost` with per-extension isolates, memory caps and permission enforcement (shell-authoritative, §8.2); v1 component library (List, Detail, Form, Action — Grid stays v2 per §6.2); JSON render tree + JSON Patch diffing with the seq/baseSeq resync protocol (§4.6/§4.7); `yspot dev` CLI (scaffold, hot reload, package); five dogfooded extensions built purely on the public `@yspot/api`: **Settings & Control Panel catalog, Quicklinks, Web Search, Emoji Picker, Process Killer** (§7 — the process killer exercises the brokered `system.processes.list/kill` capability, pre-granted for built-ins). Clipboard history and window management are NOT ported: they remain native per §7.8, and the brokered `clipboard.*`/`window.*` capabilities are explicitly deferred past v1.
**Exit criteria:** an extension crash/OOM shows an error card with host uninterrupted; all five dogfooded extensions run purely on the public API; an external tester scaffolds and runs "hello world" from docs alone in < 30 minutes.

### M4 — Polish, signing, public beta (4+ weeks)
**Goal:** installable by strangers. **Scope:** signed per-user shell installer + service MSI (§9.1) with auto-update channels live end-to-end (including service UAC flow), uninstall test automated, AV vendor submissions, SmartScreen reputation ramp via gradual rollout, docs site, Store listing for portable mode, public beta.
**Exit criteria:** clean install→update→uninstall on Win10 1809, Win10 22H2, Win11 fresh VMs, including a clean Win10 1809 VM with **no WebView2 Runtime** (Evergreen Bootstrapper path exercised, §9.1); zero Defender detections on release binaries; 100 external beta installs with crash-free sessions > 99% (metric per §8.5).

## 11. Risks and Open Questions

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| 1 | ultrasearch license — **RESOLVED** (§3.9): license verified internally contradictory with a field-of-use rider; cannot fork | — | — | Decision made: cleanroom re-implementation from public Microsoft docs (MFT+USN+Tantivy techniques are not copyrightable); CONTRIBUTING MUST forbid copying ultrasearch code. Row kept as a record |
| 2 | AV/EDR false positives: raw volume access + global hotkey + young signature | High | High | Sign everything from first beta; gradual rollout; proactively submit binaries to Microsoft/major AV vendor whitelists; document behavior publicly; no packers/obfuscation |
| 3 | `SetForegroundWindow` refused from the WM_HOTKEY handler on some builds; focus-restore edge cases on dismiss (elevated foreground app, secure desktop) | Medium | High | M0 proves the activate-and-restore path (§5.2) including IME; on refusal, a **logged flash-free retry**; edge cases enumerated and tested per Windows build |
| 4 | WebView2 memory creep breaks the 150 MB idle budget | Medium | Medium | CI memory benchmark; renderer recycle **only** when private working set > 250 MB AND ≥ 10 min without a summon, immediately re-warmed (reload + wait for `frontendReady`); a summon during re-warm shows the §2.6 native placeholder and counts as a budget miss in telemetry; virtualized lists; no heavyweight UI deps |
| 5 | `SetForegroundWindow` restrictions block focus handoff for "open app" actions | Medium | Medium | Use the documented allowances (foreground process may grant via `AllowSetForegroundWindow`); `SendInput` nudge as last resort; test per Windows build |
| 6 | Windows Search / OleDB fallback deprecated or degraded in future Windows | Low | Medium | Fallback is isolated behind a provider trait in the shell (§3.1, §9.5); alternate fallback = best-effort `ReadDirectoryChangesW` + on-demand walk |
| 7 | Bus factor of one | High | High | Boring tech choices, CI-enforced budgets/fuzzing so quality doesn't depend on memory, docs written as if onboarding maintainer #2, public issue tracker early |
| 8 | Raycast for Windows ships first-party content search before M2 | Medium | High | Speed on M0–M2; Windows-first depth (every volume, settings, Control Panel) and open extension isolation story remain differentiators regardless |

**Open questions** (decide before the public repo goes live):

1. **YSpot's own license.** Must be chosen before the repo is public — relicensing later requires every contributor's consent. Tension: permissive (MIT/Apache-2.0) maximizes adoption but lets a competitor ship the differentiator; source-available (BSL/FSL) protects it but chills contributions. Recommendation: decide by end of M1; default lean Apache-2.0 for core + separate terms for the store.
2. **Extension store moderation.** Raycast-style reviewed monorepo vs. open registry? Reviewed monorepo is the safer default for a permissioned platform, but is labor the bus-factor-of-one cannot absorb long-term.
3. **Monetization.** Free core is table stakes (PowerToys is free). Candidates: paid Pro (sync, AI features), team features, store revenue share. No decision needed before M4, but pricing intent should be public before beta to avoid community backlash.
