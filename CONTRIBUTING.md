# Contributing to YSpot

YSpot is in pre-development; the spec ([SPEC.md](SPEC.md)) is the source of truth. Changes that contradict the spec need a spec PR first.

## Cleanroom rule (important)

The indexer architecture (MFT enumeration + USN journal tailing + Tantivy) is re-implemented from public Microsoft documentation and first principles. The [ultrasearch](https://github.com/Dicklesworthstone/ultrasearch) project served as an existence proof only — its license was reviewed and found contradictory/restricted (see SPEC.md §3.9).

**Do not copy, port, or closely paraphrase code from ultrasearch.** Reading its documentation and architecture write-ups is fine; its source code is off-limits as a reference while writing YSpot code. The same caution applies to Everything (closed-source) reverse-engineering write-ups of its internals.

Safe references: Microsoft Learn documentation (`FSCTL_ENUM_USN_DATA`, `FSCTL_READ_USN_JOURNAL`, USN record layouts), the `windows-rs`/`windows-sys` crates, Tantivy's own docs and examples.

## Builds failing with `os error 4551`

That is Smart App Control (a Windows 11 Code Integrity policy) blocking an
unsigned binary — most often a cargo build script or a proc-macro DLL, not
anything you wrote. Run `pwsh -File scripts/sac-status.ps1` to see exactly what
was blocked and what launched it. Code signing does **not** fix this; see
[docs/SIGNING.md](docs/SIGNING.md) §2 for why and what to do instead.

## Ground rules

- Rust for the service and shell, TypeScript/React for the frontend and extensions — see SPEC.md §2.7 for the rationale; no new languages without a spec change.
- The elevated service is security-critical: any change to pipe handling, MFT/USN parsing, or the extractor sandbox needs fuzz coverage (cargo-fuzz targets are mandatory per SPEC.md §8).
- Performance budgets in SPEC.md §1.2 are normative; PRs that regress the CI latency harness don't merge.
