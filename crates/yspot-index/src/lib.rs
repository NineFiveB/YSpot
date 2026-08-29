//! YSpot filename index — SPEC.md §3.
//!
//! Module map:
//! - [`volumes`] — volume discovery/classification (§3.1)
//! - [`mft`] — initial enumeration via `FSCTL_ENUM_USN_DATA` (§3.2)
//! - [`usn`] — USN journal tailing (§3.3)
//! - [`walk`] — dev-mode index population via a filesystem walk (unelevated
//!   fallback: MFT enumeration needs an admin-openable volume handle)
//! - [`index`] — in-memory entry table + name arenas (§3.4)
//! - [`matching`] — tiered matcher + candidate prefilters (§3.4)

pub mod index;
pub mod matching;
pub mod mft;
pub mod usn;
pub mod volumes;
pub mod walk;

/// Entry flags (§3.4 `flags: u16`).
pub mod flags {
    pub const DIR: u16 = 1 << 0;
    pub const HIDDEN: u16 = 1 << 1;
    pub const SYSTEM: u16 = 1 << 2;
}

/// Where enumeration/tailing sources push entries. `VolumeIndex` implements
/// this; tests may implement it to count entries.
pub trait EntrySink {
    fn add(&mut self, frn: u64, parent_frn: u64, name: &str, flags: u16);
}

/// A change event decoded from the USN journal (§3.3), applied to the index.
#[derive(Debug, Clone)]
pub enum UsnEvent {
    Create {
        frn: u64,
        parent_frn: u64,
        name: String,
        flags: u16,
    },
    Delete {
        frn: u64,
    },
    Rename {
        frn: u64,
        new_parent_frn: u64,
        new_name: String,
    },
    /// `USN_REASON_SECURITY_CHANGE` — invalidates ACL caches, no index change.
    SecurityChange {
        frn: u64,
    },
}

/// Cursor persisted per volume: where tailing resumes (§3.2/§3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsnCursor {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// One scored hit out of [`index::VolumeIndex::search`].
#[derive(Debug, Clone)]
pub struct Hit {
    pub frn: u64,
    /// Pure `match_quality × depth_penalty` (§3.4) — no frecency in the service.
    pub score: f32,
    /// UTF-16 code-unit ranges into the entry's original-case name (§5.13).
    pub match_ranges: Vec<(u32, u32)>,
}
