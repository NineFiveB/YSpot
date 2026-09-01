//! Shared service state: the hosted index plus the volume-level status bits
//! reported through `IndexStatus` (SPEC §4.3).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use yspot_proto::{RamBytes, VolumeState, VolumeStatus};

use crate::idx_api::{self, VolumeIndex};

/// Index lifecycle for the single M0 volume, kept in an `AtomicU8` so status
/// replies never need the index lock. `Paused` is a separate bool overlay.
pub const VS_ENUMERATING: u8 = 0;
pub const VS_TAILING: u8 = 1;
pub const VS_REBUILDING: u8 = 2;

pub fn vol_state_from_u8(v: u8) -> VolumeState {
    match v {
        VS_ENUMERATING => VolumeState::Enumerating,
        VS_REBUILDING => VolumeState::Rebuilding,
        _ => VolumeState::Tailing,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// `--walk <path>`: unelevated dev mode, filesystem walk, no USN tailing.
    Walk,
    /// `--mft <C:>`: MFT enumeration + USN tailing (elevated).
    Mft,
}

pub struct ServiceState {
    /// SPEC-mandated hosting shape: the volume index behind a std RwLock;
    /// searches take read, USN application/rebuild swap takes write.
    pub index: RwLock<VolumeIndex>,
    pub vol_state: AtomicU8,
    /// Machine-wide pause (§3.6); the USN thread respects it between batches.
    pub paused: AtomicBool,
    /// Bumps on full rebuild (§4.3 HelloAck.index_epoch).
    pub index_epoch: AtomicU64,
    /// Root with trailing backslash (`C:\` or the walk root).
    pub root_path: String,
    /// Sessions currently reporting themselves interactive (§4.3 `SessionState`).
    ///
    /// §3.6 defines the machine as idle only when EVERY connected session says
    /// so, which is why this is a count rather than a flag: one active session
    /// must be able to hold heavy maintenance off for all of them.
    pub active_sessions: AtomicUsize,
    pub mode: Mode,
}

impl ServiceState {
    pub fn new(index: VolumeIndex, root_path: String, mode: Mode) -> Self {
        Self {
            index: RwLock::new(index),
            vol_state: AtomicU8::new(VS_ENUMERATING),
            paused: AtomicBool::new(false),
            index_epoch: AtomicU64::new(1),
            active_sessions: AtomicUsize::new(0),
            root_path,
            mode,
        }
    }

    /// Read access; a poisoned lock (panicked writer) still serves — the index
    /// is never left half-written by our writers (they swap whole values or
    /// apply single events).
    pub fn index_read(&self) -> RwLockReadGuard<'_, VolumeIndex> {
        self.index.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn index_write(&self) -> RwLockWriteGuard<'_, VolumeIndex> {
        self.index.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_vol_state(&self, s: u8) {
        self.vol_state.store(s, Ordering::SeqCst);
    }

    /// §3.6: the machine counts as idle only when every connected session
    /// reports idle. Heavy maintenance — compaction, merges — waits for this.
    pub fn machine_idle(&self) -> bool {
        self.active_sessions.load(Ordering::SeqCst) == 0
    }

    /// Move one session between the active and idle sets. Idempotent per
    /// session: the caller passes what it previously reported, so a repeated
    /// `Active` cannot double-count and a disconnect cannot under-count.
    pub fn set_session_active(&self, was_active: bool, now_active: bool) {
        match (was_active, now_active) {
            (false, true) => {
                self.active_sessions.fetch_add(1, Ordering::SeqCst);
            }
            (true, false) => {
                self.active_sessions.fetch_sub(1, Ordering::SeqCst);
            }
            _ => {}
        }
    }
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn volume_state(&self) -> VolumeState {
        if self.is_paused() {
            return VolumeState::Paused; // paused overlays whatever else is going on (§4.3)
        }
        vol_state_from_u8(self.vol_state.load(Ordering::SeqCst))
    }

    /// The one real `VolumeStatus` M0 serves (§4.3). M0 deviations: no
    /// volume-GUID lookup (the root path stands in for `volume`), fs string
    /// from the mode instead of GetVolumeInformationW, usn_lag_ms not measured.
    pub fn volume_status(&self) -> VolumeStatus {
        let (files, ram) = {
            let idx = self.index_read();
            (idx_api::entry_count(&idx), idx_api::ram_bytes(&idx))
        };
        VolumeStatus {
            volume: self.root_path.clone(),
            mounts: vec![self.root_path.clone()],
            fs: match self.mode {
                Mode::Mft => "NTFS".to_string(),
                Mode::Walk => "WALK".to_string(), // dev mode: fs not probed
            },
            state: self.volume_state(),
            files_indexed: files,
            usn_lag_ms: 0,
            content_docs: 0,
            ram_bytes: RamBytes {
                filename: ram,
                content: 0,
                caches: 0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_mapping() {
        assert_eq!(vol_state_from_u8(VS_ENUMERATING), VolumeState::Enumerating);
        assert_eq!(vol_state_from_u8(VS_TAILING), VolumeState::Tailing);
        assert_eq!(vol_state_from_u8(VS_REBUILDING), VolumeState::Rebuilding);
        // Unknown values degrade to Tailing rather than panicking.
        assert_eq!(vol_state_from_u8(200), VolumeState::Tailing);
    }
}
