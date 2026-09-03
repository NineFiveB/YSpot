//! Single funnel for every call into `yspot-index`.
//!
//! Signatures below are verified against the real crate API (index.rs, usn.rs,
//! mft.rs, walk.rs). If the index API drifts, this file is the only place in
//! the service that needs fixing.

use std::path::Path;

pub use yspot_index::index::VolumeIndex;
pub use yspot_index::usn::TailOutcome;
pub use yspot_index::{UsnCursor, UsnEvent};

/// Construct an empty per-volume index. M0 runs a single volume, `volume_idx` 0.
/// `root_path` carries a trailing backslash, e.g. `C:\`.
pub fn new_index(volume_idx: u32, root_path: &str) -> VolumeIndex {
    VolumeIndex::new(volume_idx, root_path.to_string())
}

/// Case fold a string exactly the way the index folds its shadow arena, so
/// `path_substr` filtering (§4.3) agrees with the matcher's folding.
pub fn fold(s: &str) -> String {
    yspot_index::index::fold(s)
}

pub fn entry_count(idx: &VolumeIndex) -> u64 {
    idx.len() as u64
}

pub fn ram_bytes(idx: &VolumeIndex) -> u64 {
    idx.ram_bytes()
}

/// Per-structure attribution of [`ram_bytes`] (§4.3 `ram_bytes.filename`),
/// plus the live+stale name payload the per-entry figures scale through.
pub fn ram_breakdown(idx: &VolumeIndex) -> yspot_index::index::RamBreakdown {
    idx.ram_breakdown()
}

pub fn name_arena_len(idx: &VolumeIndex) -> usize {
    idx.name_arena_len()
}

pub fn name_of(idx: &VolumeIndex, frn: u64) -> Option<String> {
    idx.name_of(frn).map(|s| s.to_string())
}

/// Full path (parent chain + name) of an entry, if it still exists.
pub fn path_of(idx: &VolumeIndex, frn: u64) -> Option<String> {
    idx.path_of(frn)
}

pub fn apply_usn(idx: &mut VolumeIndex, ev: UsnEvent) {
    idx.apply(ev);
}

/// Whether a directory reparent has left descendant ranking depths stale and
/// its debounce has elapsed (§3.4 `depth_penalty`).
pub fn depth_repair_due(idx: &VolumeIndex) -> bool {
    idx.depth_repair_due()
}

/// Advance the depth repair by one slice; `true` while more remains, so the
/// caller can drop and retake the write lock between slices.
pub fn repair_depths_slice(idx: &mut VolumeIndex) -> bool {
    idx.repair_depths_slice()
}

/// Dev-mode population (unelevated): iterative filesystem walk (§3 `walk` module).
pub fn walk_into(root: &str, idx: &mut VolumeIndex) -> anyhow::Result<()> {
    yspot_index::walk::walk(Path::new(root), idx)?;
    Ok(())
}

/// Initial MFT enumeration via FSCTL_ENUM_USN_DATA (§3.2). `drive` is `"C:"`.
/// Returns the tail-start cursor captured before the enumeration began.
pub fn mft_enumerate(drive: &str, idx: &mut VolumeIndex) -> anyhow::Result<UsnCursor> {
    yspot_index::mft::enumerate(drive, idx)
}

pub type UsnTailer = yspot_index::usn::Tailer;

/// Open the tailing handle (synchronous, lives on the dedicated tail thread).
pub fn usn_open(drive: &str) -> anyhow::Result<UsnTailer> {
    UsnTailer::new(drive)
}

/// Blocking read of the next USN batch (§3.3: BytesToWaitFor=1, Timeout=0).
/// Advances `cursor` on success; journal wrap/truncation surfaces as
/// `Ok(TailOutcome::NeedsRebuild(_))`, never as `Err`.
pub fn usn_read_batch(
    tailer: &mut UsnTailer,
    cursor: &mut UsnCursor,
) -> anyhow::Result<TailOutcome> {
    tailer.read_batch(cursor)
}

/// ERROR_ACCESS_DENIED anywhere in the chain — used for the elevation hint.
/// Requires the index crate to attach `std::io::Error` sources on raw Win32
/// failures (mft.rs does for volume opens).
pub fn is_access_denied(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.raw_os_error() == Some(5))
    })
}

/// [`search`] with a name predicate applied during ranking rather than after,
/// so a filtered page fills with `max_results` ACCEPTED hits (§4.3 filters).
pub fn search_filtered(
    idx: &VolumeIndex,
    query: &str,
    max_results: usize,
    is_cancelled: &dyn Fn() -> bool,
    accept: &dyn Fn(&str) -> bool,
) -> Vec<yspot_index::Hit> {
    idx.search_filtered(query, max_results, is_cancelled, accept)
}
