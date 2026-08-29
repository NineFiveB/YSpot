//! USN journal tailing — SPEC.md §3.3.
//!
//! [`Tailer`] owns a synchronously opened volume handle and blocks inside
//! `FSCTL_READ_USN_JOURNAL` until at least one byte of journal data exists
//! (`BytesToWaitFor = 1`, `Timeout = 0`) — no polling. Both wait fields are
//! ignored on asynchronously opened handles, which is why the handle is
//! synchronous and the tailer must live on its own dedicated thread.
//!
//! Journal wrap/truncation/deletion surfaces as [`TailOutcome::NeedsRebuild`];
//! the caller discards the volume index, recreates the journal if needed, and
//! re-enumerates (§3.2).

use std::ffi::c_void;

use anyhow::bail;
use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_JOURNAL_DELETE_IN_PROGRESS, ERROR_JOURNAL_ENTRY_DELETED,
    ERROR_JOURNAL_NOT_ACTIVE,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0, USN_REASON_CLOSE, USN_REASON_FILE_CREATE,
    USN_REASON_FILE_DELETE, USN_REASON_RENAME_NEW_NAME, USN_REASON_SECURITY_CHANGE,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::mft::{attrs_to_flags, for_each_record, open_volume, read_i64, UsnRecord, VolumeHandle};
use crate::{UsnCursor, UsnEvent};

/// Reasons subscribed by the tailer (§3.3). BASIC_INFO_CHANGE and
/// HARD_LINK_CHANGE are deliberately absent until `map_record` produces an
/// attr-update event for them — subscribing without mapping only causes
/// needless tail-thread wakeups (M1: add attr propagation so the
/// hidden/system rank penalty stays fresh).
const REASON_MASK: u32 = USN_REASON_FILE_CREATE
    | USN_REASON_FILE_DELETE
    | USN_REASON_RENAME_NEW_NAME
    | USN_REASON_SECURITY_CHANGE
    | USN_REASON_CLOSE;

const READ_BUF_SIZE: usize = 256 * 1024;

/// Result of one blocking journal read.
#[derive(Debug)]
pub enum TailOutcome {
    /// Decoded events (possibly empty — e.g. only close-only records arrived).
    /// The caller's cursor has been advanced past the consumed records.
    Events(Vec<UsnEvent>),
    /// The volume index is stale beyond repair by tailing: the caller must
    /// re-enumerate (§3.2), recreating the journal first if it was deleted.
    NeedsRebuild(&'static str),
}

/// Blocking USN tailer for one volume; runs on a dedicated thread.
pub struct Tailer {
    handle: VolumeHandle,
    volume_root: String,
    buf: Vec<u8>,
}

impl Tailer {
    /// Open the volume (same privileged open as [`crate::mft::enumerate`];
    /// synchronous — no `FILE_FLAG_OVERLAPPED`, so the blocking-wait fields
    /// of `READ_USN_JOURNAL_DATA_V0` are honored).
    pub fn new(volume_root: &str) -> anyhow::Result<Self> {
        let handle = open_volume(volume_root)?;
        Ok(Self {
            handle,
            volume_root: volume_root.to_string(),
            buf: vec![0u8; READ_BUF_SIZE],
        })
    }

    /// Block until journal data is available at `cursor`, decode one batch,
    /// and advance `cursor.next_usn`. On wrap/truncation/deletion returns
    /// `NeedsRebuild` instead of an error (the caller owns recovery).
    pub fn read_batch(&mut self, cursor: &mut UsnCursor) -> anyhow::Result<TailOutcome> {
        let request = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: cursor.next_usn,
            ReasonMask: REASON_MASK,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 1, // block until >= 1 byte of journal data (§3.3)
            UsnJournalID: cursor.journal_id,
        };
        let mut bytes: u32 = 0;
        // SAFETY: `request` is a live READ_USN_JOURNAL_DATA_V0 for the input
        // size given; `self.buf` is valid for its length; `bytes` is a valid
        // out pointer; the handle is owned and synchronous.
        let ok = unsafe {
            DeviceIoControl(
                self.handle.0,
                FSCTL_READ_USN_JOURNAL,
                &request as *const READ_USN_JOURNAL_DATA_V0 as *const c_void,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                self.buf.as_mut_ptr() as *mut c_void,
                self.buf.len() as u32,
                &mut bytes,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: trivially safe TLS read.
            let err = unsafe { GetLastError() };
            return match err {
                // Cursor older than FirstUsn (records overwritten), or the
                // journal ID no longer matches — index is stale (§3.3).
                ERROR_JOURNAL_ENTRY_DELETED => Ok(TailOutcome::NeedsRebuild(
                    "USN cursor overwritten or journal ID mismatch (ERROR_JOURNAL_ENTRY_DELETED)",
                )),
                ERROR_JOURNAL_DELETE_IN_PROGRESS => Ok(TailOutcome::NeedsRebuild(
                    "USN journal deletion in progress (ERROR_JOURNAL_DELETE_IN_PROGRESS)",
                )),
                ERROR_JOURNAL_NOT_ACTIVE => Ok(TailOutcome::NeedsRebuild(
                    "USN journal not active (ERROR_JOURNAL_NOT_ACTIVE)",
                )),
                _ => bail!(
                    "FSCTL_READ_USN_JOURNAL on {} failed: Win32 error {err}",
                    self.volume_root
                ),
            };
        }

        let filled = bytes as usize;
        if filled < 8 {
            // Success with no header should not happen; treat as empty batch.
            return Ok(TailOutcome::Events(Vec::new()));
        }
        // First 8 bytes of the output: the next StartUsn.
        let next_usn = read_i64(&self.buf, 0);

        let mut events = Vec::new();
        for_each_record(&self.buf[8..filled], |rec| {
            if let Some(ev) = map_record(rec) {
                events.push(ev);
            }
        });

        cursor.next_usn = next_usn;
        Ok(TailOutcome::Events(events))
    }
}

/// Map one journal record to at most one index event (§3.3). Priority:
/// delete wins over a create carried on the same record; then create,
/// rename-new-name, security change. Records carrying only close /
/// basic-info / hard-link reasons produce no event.
fn map_record(rec: UsnRecord) -> Option<UsnEvent> {
    let reason = rec.reason;
    if reason & USN_REASON_FILE_DELETE != 0 {
        return Some(UsnEvent::Delete { frn: rec.frn });
    }
    if reason & USN_REASON_FILE_CREATE != 0 {
        let flags = attrs_to_flags(rec.file_attributes);
        return Some(UsnEvent::Create {
            frn: rec.frn,
            parent_frn: rec.parent_frn,
            name: rec.name,
            flags,
        });
    }
    if reason & USN_REASON_RENAME_NEW_NAME != 0 {
        return Some(UsnEvent::Rename {
            frn: rec.frn,
            new_parent_frn: rec.parent_frn,
            new_name: rec.name,
        });
    }
    if reason & USN_REASON_SECURITY_CHANGE != 0 {
        return Some(UsnEvent::SecurityChange { frn: rec.frn });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;

    fn rec(reason: u32, attrs: u32) -> UsnRecord {
        UsnRecord {
            frn: 0xAA,
            parent_frn: 0xBB,
            reason,
            file_attributes: attrs,
            name: "n.txt".to_string(),
        }
    }

    #[test]
    fn delete_wins_over_create_on_same_record() {
        let ev = map_record(rec(
            USN_REASON_FILE_CREATE | USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
            0,
        ));
        match ev {
            Some(UsnEvent::Delete { frn }) => assert_eq!(frn, 0xAA),
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn create_maps_fields_and_flags() {
        let ev = map_record(rec(
            USN_REASON_FILE_CREATE | USN_REASON_CLOSE,
            FILE_ATTRIBUTE_DIRECTORY,
        ));
        match ev {
            Some(UsnEvent::Create {
                frn,
                parent_frn,
                name,
                flags,
            }) => {
                assert_eq!(frn, 0xAA);
                assert_eq!(parent_frn, 0xBB);
                assert_eq!(name, "n.txt");
                assert_eq!(flags, crate::flags::DIR);
            }
            other => panic!("expected Create, got {other:?}"),
        }
    }

    #[test]
    fn rename_new_name_maps_to_rename() {
        let ev = map_record(rec(USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE, 0));
        match ev {
            Some(UsnEvent::Rename {
                frn,
                new_parent_frn,
                new_name,
            }) => {
                assert_eq!(frn, 0xAA);
                assert_eq!(new_parent_frn, 0xBB);
                assert_eq!(new_name, "n.txt");
            }
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn security_change_alone_maps() {
        match map_record(rec(USN_REASON_SECURITY_CHANGE, 0)) {
            Some(UsnEvent::SecurityChange { frn }) => assert_eq!(frn, 0xAA),
            other => panic!("expected SecurityChange, got {other:?}"),
        }
    }

    #[test]
    fn housekeeping_reasons_produce_no_event() {
        use windows_sys::Win32::System::Ioctl::{
            USN_REASON_BASIC_INFO_CHANGE, USN_REASON_HARD_LINK_CHANGE,
        };
        assert!(map_record(rec(USN_REASON_CLOSE, 0)).is_none());
        assert!(map_record(rec(USN_REASON_BASIC_INFO_CHANGE | USN_REASON_CLOSE, 0)).is_none());
        assert!(map_record(rec(USN_REASON_HARD_LINK_CHANGE, 0)).is_none());
    }

    #[test]
    fn reason_mask_covers_spec_reasons() {
        // BASIC_INFO_CHANGE / HARD_LINK_CHANGE are deliberately unsubscribed
        // until map_record produces attr-update events for them (M1).
        for bit in [
            USN_REASON_FILE_CREATE,
            USN_REASON_FILE_DELETE,
            USN_REASON_RENAME_NEW_NAME,
            USN_REASON_SECURITY_CHANGE,
            USN_REASON_CLOSE,
        ] {
            assert_ne!(REASON_MASK & bit, 0);
        }
    }

    /// Decoding a batch buffer end-to-end (header + packed records) using the
    /// shared parser, exactly as `read_batch` does after `DeviceIoControl`.
    #[test]
    fn batch_buffer_decodes_events_and_next_usn() {
        let mut buf = 12345i64.to_le_bytes().to_vec(); // header: next StartUsn
        buf.extend(crate::mft::test_record(
            10,
            2,
            USN_REASON_FILE_CREATE | USN_REASON_CLOSE,
            0,
            "new.txt",
            true,
        ));
        buf.extend(crate::mft::test_record(
            11,
            2,
            USN_REASON_CLOSE,
            0,
            "ignored.txt",
            true,
        ));
        buf.extend(crate::mft::test_record(
            12,
            3,
            USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
            0,
            "gone.txt",
            true,
        ));

        assert_eq!(read_i64(&buf, 0), 12345);
        let mut events = Vec::new();
        for_each_record(&buf[8..], |r| {
            if let Some(ev) = map_record(r) {
                events.push(ev);
            }
        });
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], UsnEvent::Create { frn: 10, .. }));
        assert!(matches!(events[1], UsnEvent::Delete { frn: 12 }));
    }
}
