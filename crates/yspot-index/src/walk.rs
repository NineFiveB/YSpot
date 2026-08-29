//! Dev-mode index population via a plain filesystem walk.
//!
//! MFT enumeration (§3.2) needs an admin-openable volume handle; unelevated
//! dev builds fall back to this iterative `read_dir` walk. FRNs are synthetic
//! (monotonic counter from 1,000,000) — they satisfy the index's parent-chain
//! layout (§3.4) but are NOT stable NTFS FRNs and must never be persisted as
//! such.
//!
//! The walk root itself gets no entry; its immediate children get
//! `parent_frn = 0`, which anchors them at the index's `root_path` (mirroring
//! how MFT enumeration anchors at the volume root). Reparse points (symlinks,
//! junctions, OneDrive placeholders) are indexed as entries but never
//! descended into — no cycles, no placeholder hydration. Per-entry I/O errors
//! are logged and skipped.

use std::path::{Path, PathBuf};

use anyhow::Context;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
};

use crate::EntrySink;

/// First synthetic FRN handed out (root's children count up from here).
const FIRST_SYNTHETIC_FRN: u64 = 1_000_000;

/// Walk `root` iteratively (explicit stack, no recursion), pushing one entry
/// per file/directory into `sink`. Returns the number of entries added.
pub fn walk<S: EntrySink>(root: &Path, sink: &mut S) -> anyhow::Result<u64> {
    let mut next_frn: u64 = FIRST_SYNTHETIC_FRN;
    let mut count: u64 = 0;
    let mut stack: Vec<(PathBuf, u64)> = Vec::new();

    // The root must be readable — otherwise there is nothing to index.
    let root_iter = std::fs::read_dir(root)
        .with_context(|| format!("cannot read walk root {}", root.display()))?;
    scan_dir(root_iter, 0, sink, &mut stack, &mut next_frn, &mut count);

    while let Some((dir, dir_frn)) = stack.pop() {
        match std::fs::read_dir(&dir) {
            Ok(iter) => scan_dir(iter, dir_frn, sink, &mut stack, &mut next_frn, &mut count),
            // Access denied etc. — skip the subtree, keep walking (contract:
            // per-entry io errors continue).
            Err(err) => log::debug!("walk: skipping {}: {err}", dir.display()),
        }
    }

    Ok(count)
}

/// Add every entry of one directory listing; queue non-reparse subdirectories.
fn scan_dir<S: EntrySink>(
    iter: std::fs::ReadDir,
    parent_frn: u64,
    sink: &mut S,
    stack: &mut Vec<(PathBuf, u64)>,
    next_frn: &mut u64,
    count: &mut u64,
) {
    use std::os::windows::fs::MetadataExt;

    for entry in iter {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                log::debug!("walk: directory entry error: {err}");
                continue;
            }
        };
        // `DirEntry::metadata` does not traverse symlinks, so reparse points
        // report their own attributes (FILE_ATTRIBUTE_REPARSE_POINT set).
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                log::debug!("walk: metadata error for {}: {err}", entry.path().display());
                continue;
            }
        };
        let attrs = meta.file_attributes();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        let frn = *next_frn;
        *next_frn += 1;
        sink.add(frn, parent_frn, &name, crate::mft::attrs_to_flags(attrs));
        *count += 1;

        let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        let is_reparse = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        if is_dir && !is_reparse {
            stack.push((entry.path(), frn));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Collect {
        entries: Vec<(u64, u64, String, u16)>,
    }

    impl EntrySink for Collect {
        fn add(&mut self, frn: u64, parent_frn: u64, name: &str, flags: u16) {
            self.entries
                .push((frn, parent_frn, name.to_string(), flags));
        }
    }

    impl Collect {
        fn by_name(&self, name: &str) -> &(u64, u64, String, u16) {
            self.entries
                .iter()
                .find(|(_, _, n, _)| n == name)
                .unwrap_or_else(|| panic!("no entry named {name}"))
        }
    }

    #[test]
    fn walks_nested_tree_with_synthetic_frns() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("b.txt"), b"b").unwrap();
        std::fs::create_dir(root.join("sub").join("deeper")).unwrap();
        std::fs::write(root.join("sub").join("deeper").join("c.txt"), b"c").unwrap();

        let mut sink = Collect::default();
        let count = walk(root, &mut sink).unwrap();
        assert_eq!(count, 5);
        assert_eq!(sink.entries.len(), 5);

        // Root children anchor at parent 0.
        let a = sink.by_name("a.txt");
        assert_eq!(a.1, 0);
        assert_eq!(a.3 & crate::flags::DIR, 0);

        let sub = sink.by_name("sub").clone();
        assert_eq!(sub.1, 0);
        assert_ne!(sub.3 & crate::flags::DIR, 0);

        // Children chain to their directory's synthetic FRN.
        let b = sink.by_name("b.txt");
        assert_eq!(b.1, sub.0);
        let deeper = sink.by_name("deeper").clone();
        assert_eq!(deeper.1, sub.0);
        let c = sink.by_name("c.txt");
        assert_eq!(c.1, deeper.0);

        // FRNs are unique and start at the synthetic base.
        let mut frns: Vec<u64> = sink.entries.iter().map(|e| e.0).collect();
        frns.sort_unstable();
        frns.dedup();
        assert_eq!(frns.len(), 5);
        assert!(frns.iter().all(|&f| f >= FIRST_SYNTHETIC_FRN));
    }

    #[test]
    fn empty_root_yields_zero_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let mut sink = Collect::default();
        assert_eq!(walk(tmp.path(), &mut sink).unwrap(), 0);
        assert!(sink.entries.is_empty());
    }

    #[test]
    fn missing_root_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("does-not-exist");
        let mut sink = Collect::default();
        assert!(walk(&gone, &mut sink).is_err());
    }
}
