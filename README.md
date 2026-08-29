# YSpot

**A keyboard-first launcher and command palette for Windows — built Windows-first.**

YSpot combines Raycast-class UX and an isolated TypeScript/React extension platform with search infrastructure Windows itself does not provide: a first-party Rust indexing service that enumerates the NTFS Master File Table and tails the USN journal for instant filename search across every local NTFS volume, plus an opt-in [Tantivy](https://github.com/quickwit-oss/tantivy) full-text content index.

> **Status: pre-development.** The engineering specification is complete ([SPEC.md](SPEC.md)); implementation has not started. Milestone M0 (latency-proving skeleton) is next.

## Why another launcher?

| | YSpot | Raycast for Windows | PowerToys Run |
|---|---|---|---|
| File indexing | Own Rust service: NTFS MFT + USN journal, every local NTFS volume | Own Rust MFT indexer (filenames only) | Windows Search via OleDB — results hostage to your indexing settings |
| Content (full-text) search | **First-party Tantivy index, opt-in scopes** | Delegates to Windows Search | Delegates to Windows Search |
| Matching | Substring + fuzzy + camel-case, frecency-ranked | Fuzzy + frecency | Prefix-oriented |
| Extensions | TS/React, out-of-process, per-extension isolation & memory limits | TS/React, isolated Node workers | In-process .NET assemblies, no isolation |

The three differentiators, as testable claims, are in [SPEC.md §1.5](SPEC.md).

## Architecture (five processes)

```
┌─────────────────────────────┐   ┌──────────────────────────────┐
│ yspot-indexd  (Rust, SYSTEM)│──▶│ yspot-extract (sandboxed     │
│ MFT + USN + Tantivy index   │   │ content extractor worker)    │
└──────────────┬──────────────┘   └──────────────────────────────┘
               │ named pipe (ACL'd)
┌──────────────┴──────────────┐
│ yspot shell  (Tauri v2/Rust)│  global hotkey · tray · window
│  ├─ WebView2: React/TS UI   │  launcher UI + extension renderer
│  └─ yspot-exthost (Node.js) │  one worker thread per extension
└─────────────────────────────┘
```

Only the index service runs elevated; everything else is unelevated user-context. Extensions declare UI from a fixed component library (no HTML/CSS/DOM) and are crash-isolated: a hostile extension produces an error card, never a dead launcher.

## Performance budgets (normative)

- Global hotkey → window visible: **< 50 ms**
- Keystroke → results data at the frontend: **≤ 20 ms p95**
- Initial filename index of a 1M-file NTFS volume: **< 15 s** (SSD reference machine)
- File-system change → searchable: **< 1 s**

## Documents

- [SPEC.md](SPEC.md) — full engineering specification (architecture, indexer, IPC protocol, shell/frontend, extension platform, built-ins, security model, packaging, milestones, risks)
- [CONTRIBUTING.md](CONTRIBUTING.md) — contribution ground rules (read the cleanroom note before touching indexer code)

## License

Not yet decided (tracked as an open question in the spec). All rights reserved until a license is chosen.
