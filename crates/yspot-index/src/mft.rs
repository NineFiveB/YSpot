//! Initial NTFS filename enumeration via `FSCTL_ENUM_USN_DATA` — SPEC.md §3.2.
//!
//! Also hosts the pieces shared with [`crate::usn`]: the privileged volume
//! open ([`open_volume`]), and the packed `USN_RECORD_V2` parser
//! ([`for_each_record`]) that reads every multi-byte field with unaligned
//! reads and never takes references into the raw buffer.

use std::ffi::c_void;

use anyhow::bail;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_JOURNAL_NOT_ACTIVE,
    ERROR_NOT_ALL_ASSIGNED, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, LUID,
};
use windows_sys::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_BACKUP_NAME,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA,
    FSCTL_QUERY_USN_JOURNAL, MFT_ENUM_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::{EntrySink, UsnCursor};

/// Low 48 bits of an NTFS FRN are the MFT file-record number; the high 16
/// bits are the sequence number.
pub(crate) const FRN_FILE_NUMBER_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
/// MFT file number of the volume root directory (`.`), always record 5.
pub(crate) const ROOT_FILE_NUMBER: u64 = 5;
/// File numbers below this are reserved NTFS metafiles ($MFT, $LogFile,
/// $Bitmap, $Secure, …) plus the root directory — none is user-openable
/// content, so enumeration skips them all (§3.8).
pub(crate) const FIRST_USER_FILE_NUMBER: u64 = 16;
const _: () = assert!(ROOT_FILE_NUMBER < FIRST_USER_FILE_NUMBER);

/// Fixed part of `USN_RECORD_V2` before the inline `FileName` array, bytes.
const USN_RECORD_V2_HEADER: usize = 60;

const ENUM_BUF_SIZE: usize = 1 << 20; // 1 MiB (§3.2: >= 1 MiB)
/// How often a running enumeration reports progress.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const JOURNAL_MAX_SIZE: u64 = 64 * 1024 * 1024; // 64 MiB
const JOURNAL_ALLOC_DELTA: u64 = 8 * 1024 * 1024; // 8 MiB

/// Owned volume handle; closed on drop.
pub(crate) struct VolumeHandle(pub(crate) HANDLE);

// SAFETY: a Win32 kernel handle is a process-wide object reference with no
// thread affinity; it may be used and closed from any thread. The tailer
// (§3.3) moves its handle to a dedicated blocking thread.
unsafe impl Send for VolumeHandle {}

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a valid handle we own; closed exactly once.
        unsafe { CloseHandle(self.0) };
    }
}

/// Enumerate every in-use MFT record of `volume_root` (`"C:"`, `"C:\"`, or a
/// `\\?\Volume{...}\` GUID path) into `sink`, returning the USN cursor
/// captured *before* enumeration — the tail-start position, so changes that
/// race the scan are replayed by the tailer rather than lost (§3.2).
///
/// The volume-root directory itself (MFT file number 5) gets no entry;
/// entries whose parent chain reaches it anchor at the index's `root_path`.
pub fn enumerate<S: EntrySink>(volume_root: &str, sink: &mut S) -> anyhow::Result<UsnCursor> {
    let vol = open_volume(volume_root)?;

    // Query (or create, then query) the USN journal BEFORE enumerating.
    let journal = match query_usn_journal(vol.0) {
        Ok(j) => j,
        Err(err) if err == ERROR_JOURNAL_NOT_ACTIVE => {
            create_usn_journal(vol.0).map_err(|e| {
                anyhow::anyhow!("FSCTL_CREATE_USN_JOURNAL on {volume_root} failed: Win32 error {e}")
            })?;
            query_usn_journal(vol.0).map_err(|e| {
                anyhow::anyhow!(
                    "FSCTL_QUERY_USN_JOURNAL on {volume_root} failed after create: Win32 error {e}"
                )
            })?
        }
        Err(err) => bail!("FSCTL_QUERY_USN_JOURNAL on {volume_root} failed: Win32 error {err}"),
    };
    let cursor = UsnCursor {
        journal_id: journal.UsnJournalID,
        next_usn: journal.NextUsn,
    };

    let mut med = MFT_ENUM_DATA_V0 {
        StartFileReferenceNumber: 0,
        LowUsn: 0,
        HighUsn: i64::MAX,
    };
    let mut buf = vec![0u8; ENUM_BUF_SIZE];

    // Progress accounting. A whole-volume scan is the one operation that can
    // run for many seconds with nothing to show for it, so it reports at a
    // fixed cadence — this is what turns "it hung" into a rate we can compare
    // against the §10 M0 budget. It also backs the `index.progress` topic
    // (§4.3) once the service subscribes to it.
    let started = std::time::Instant::now();
    let mut last_report = started;
    let mut seen: u64 = 0;
    let mut added: u64 = 0;

    loop {
        let mut bytes: u32 = 0;
        // SAFETY: `med` is a live MFT_ENUM_DATA_V0 for the input size given;
        // `buf` is valid for `buf.len()` bytes; `bytes` is a valid out ptr.
        let ok = unsafe {
            DeviceIoControl(
                vol.0,
                FSCTL_ENUM_USN_DATA,
                &med as *const MFT_ENUM_DATA_V0 as *const c_void,
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut bytes,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // SAFETY: trivially safe TLS read.
            let err = unsafe { GetLastError() };
            if err == ERROR_HANDLE_EOF {
                break; // §3.2: enumeration complete
            }
            bail!("FSCTL_ENUM_USN_DATA on {volume_root} failed: Win32 error {err}");
        }

        let filled = bytes as usize;
        if filled < 8 {
            break; // no header — nothing more to read
        }
        // First 8 bytes of the output: next StartFileReferenceNumber.
        let next_start = read_u64(&buf, 0);

        for_each_record(&buf[8..filled], |rec| {
            seen += 1;
            // File numbers 0–15 are reserved NTFS metafiles ($MFT, $LogFile,
            // $Bitmap, …, and record 5 = the volume root directory). None of
            // them is user-openable content; indexing them is pure noise (§3.8).
            if rec.frn & FRN_FILE_NUMBER_MASK < FIRST_USER_FILE_NUMBER {
                return;
            }
            added += 1;
            sink.add(
                rec.frn,
                rec.parent_frn,
                &rec.name,
                attrs_to_flags(rec.file_attributes),
            );
        });

        if last_report.elapsed() >= PROGRESS_INTERVAL {
            let secs = started.elapsed().as_secs_f64();
            log::info!(
                "enumerating {volume_root}: {added} entries ({seen} records) in {secs:.1} s — \
                 {:.0} entries/s",
                added as f64 / secs.max(f64::EPSILON)
            );
            last_report = std::time::Instant::now();
        }

        if next_start == med.StartFileReferenceNumber {
            log::warn!("FSCTL_ENUM_USN_DATA on {volume_root} made no forward progress; stopping");
            break;
        }
        med.StartFileReferenceNumber = next_start;
    }

    // The sink has seen every record: let it settle its storage (§3.4 memory
    // budget) before the volume is served from.
    sink.finish();

    let secs = started.elapsed().as_secs_f64();
    log::info!(
        "enumerated {volume_root}: {added} entries kept of {seen} records in {secs:.2} s — \
         {:.0} entries/s",
        added as f64 / secs.max(f64::EPSILON)
    );
    Ok(cursor)
}

/// Open `\\.\C:`-style volume handle for reading with backup semantics.
/// Enables `SeBackupPrivilege` first (best-effort — only works elevated).
pub(crate) fn open_volume(volume_root: &str) -> anyhow::Result<VolumeHandle> {
    enable_backup_privilege();

    let device = volume_device_path(volume_root);
    let wide = crate::volumes::to_utf16z(&device);
    // SAFETY: `wide` is NUL-terminated and outlives the call; security
    // attributes and template handle are documented-optional and null.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: trivially safe TLS read.
        let err = unsafe { GetLastError() };
        // Attach a real io::Error so callers can downcast for the raw OS code
        // (yspot-indexd keys its elevation hint off raw_os_error() == 5).
        let io = std::io::Error::from_raw_os_error(err as i32);
        if err == ERROR_ACCESS_DENIED {
            return Err(anyhow::Error::new(io).context(format!(
                "opening volume {device} denied (Win32 error 5): reading the MFT/USN journal \
                 requires elevation (run as Administrator / Backup Operators / LocalSystem); \
                 unelevated dev builds should fall back to the walk-based indexer"
            )));
        }
        return Err(anyhow::Error::new(io).context(format!("CreateFileW({device}) failed")));
    }
    Ok(VolumeHandle(handle))
}

/// `"C:"` / `"C:\"` → `\\.\C:`; `\\?\Volume{...}\` → `\\?\Volume{...}` (the
/// trailing backslash would open the root *directory*, not the volume).
pub(crate) fn volume_device_path(volume_root: &str) -> String {
    let trimmed = volume_root.trim_end_matches('\\');
    if trimmed.starts_with(r"\\") {
        trimmed.to_string()
    } else {
        format!(r"\\.\{trimmed}")
    }
}

/// Best-effort `SeBackupPrivilege` enable on the process token. Failure is
/// expected when unelevated and is only logged (§3.2: the subsequent volume
/// open reports the actionable error).
pub(crate) fn enable_backup_privilege() {
    // SAFETY: each call passes valid or documented-optional pointers; the
    // token handle is closed on every path that acquired it.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        ) == 0
        {
            log::debug!("OpenProcessToken failed: Win32 error {}", GetLastError());
            return;
        }
        let mut luid: LUID = std::mem::zeroed();
        if LookupPrivilegeValueW(std::ptr::null(), SE_BACKUP_NAME, &mut luid) == 0 {
            log::debug!(
                "LookupPrivilegeValueW(SeBackupPrivilege) failed: Win32 error {}",
                GetLastError()
            );
        } else {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let ok =
                AdjustTokenPrivileges(token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut());
            // AdjustTokenPrivileges "succeeds" even when nothing was assigned.
            if ok == 0 || GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                log::debug!("SeBackupPrivilege not enabled (needs elevation)");
            }
        }
        CloseHandle(token);
    }
}

fn query_usn_journal(vol: HANDLE) -> Result<USN_JOURNAL_DATA_V0, u32> {
    // SAFETY: plain-old-data out struct, valid for its stated size.
    let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    let mut bytes: u32 = 0;
    // SAFETY: out pointer valid for size_of::<USN_JOURNAL_DATA_V0>() bytes;
    // no input buffer is required for this FSCTL.
    let ok = unsafe {
        DeviceIoControl(
            vol,
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut data as *mut USN_JOURNAL_DATA_V0 as *mut c_void,
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            &mut bytes,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: trivially safe TLS read.
        Err(unsafe { GetLastError() })
    } else {
        Ok(data)
    }
}

fn create_usn_journal(vol: HANDLE) -> Result<(), u32> {
    let params = CREATE_USN_JOURNAL_DATA {
        MaximumSize: JOURNAL_MAX_SIZE,
        AllocationDelta: JOURNAL_ALLOC_DELTA,
    };
    let mut bytes: u32 = 0;
    // SAFETY: input struct valid for its stated size; no output buffer.
    let ok = unsafe {
        DeviceIoControl(
            vol,
            FSCTL_CREATE_USN_JOURNAL,
            &params as *const CREATE_USN_JOURNAL_DATA as *const c_void,
            std::mem::size_of::<CREATE_USN_JOURNAL_DATA>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        // SAFETY: trivially safe TLS read.
        Err(unsafe { GetLastError() })
    } else {
        Ok(())
    }
}

/// Map `FILE_ATTRIBUTE_*` bits to the crate's entry flags (§3.4).
pub(crate) fn attrs_to_flags(attrs: u32) -> u16 {
    let mut flags = 0u16;
    if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        flags |= crate::flags::DIR;
    }
    if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
        flags |= crate::flags::HIDDEN;
    }
    if attrs & FILE_ATTRIBUTE_SYSTEM != 0 {
        flags |= crate::flags::SYSTEM;
    }
    flags
}

/// One decoded `USN_RECORD_V2` (shared by §3.2 enumeration and §3.3 tailing).
#[derive(Debug, Clone)]
pub(crate) struct UsnRecord {
    pub frn: u64,
    pub parent_frn: u64,
    pub reason: u32,
    pub file_attributes: u32,
    pub name: String,
}

/// Walk packed `USN_RECORD_V2`s in `buf` (the region *after* the 8-byte
/// next-cursor header), calling `f` per well-formed V2 record.
///
/// Every multi-byte field is read with an unaligned read at an explicit byte
/// offset — no references are ever taken into the buffer. Advances by
/// `RecordLength` (rounded up to the 8-byte record alignment); stops at a
/// zero `RecordLength` or any malformed length, warning rather than panicking.
pub(crate) fn for_each_record(buf: &[u8], mut f: impl FnMut(UsnRecord)) {
    let mut off = 0usize;
    // `saturating_add`: the 8-byte round-up below may push `off` past the end.
    while off.saturating_add(USN_RECORD_V2_HEADER) <= buf.len() {
        let rec_len = read_u32(buf, off) as usize;
        if rec_len == 0 {
            break; // terminator
        }
        if rec_len < USN_RECORD_V2_HEADER || rec_len > buf.len() - off {
            log::warn!(
                "malformed USN record: RecordLength {rec_len} at offset {off} of {}; stopping",
                buf.len()
            );
            break;
        }
        let major = read_u16(buf, off + 4);
        if major == 2 {
            if let Some(rec) = parse_v2_record(buf, off, rec_len) {
                f(rec);
            }
        }
        // else: not a V2 record (never expected for V0 enum/read requests) — skip.
        off += rec_len;
        off = (off + 7) & !7; // records are 8-byte aligned
    }
}

/// Decode the record at `off` (validated to span `rec_len` bytes in `buf`).
fn parse_v2_record(buf: &[u8], off: usize, rec_len: usize) -> Option<UsnRecord> {
    // USN_RECORD_V2 field offsets, bytes from record start:
    //  0 RecordLength u32 |  4 MajorVersion u16 |  6 MinorVersion u16
    //  8 FileReferenceNumber u64 | 16 ParentFileReferenceNumber u64
    // 24 Usn i64 | 32 TimeStamp i64 | 40 Reason u32 | 44 SourceInfo u32
    // 48 SecurityId u32 | 52 FileAttributes u32
    // 56 FileNameLength u16 (BYTES) | 58 FileNameOffset u16 (BYTES)
    // 60 FileName [u16; ...] (UTF-16LE, not NUL-terminated)
    let frn = read_u64(buf, off + 8);
    let parent_frn = read_u64(buf, off + 16);
    let reason = read_u32(buf, off + 40);
    let file_attributes = read_u32(buf, off + 52);
    let name_len = read_u16(buf, off + 56) as usize;
    let name_off = read_u16(buf, off + 58) as usize;

    if name_off < USN_RECORD_V2_HEADER
        || !name_len.is_multiple_of(2)
        || name_off + name_len > rec_len
    {
        log::warn!(
            "malformed USN record name (offset {name_off}, length {name_len}, record {rec_len}); skipping record"
        );
        return None;
    }
    let start = off + name_off;
    let units: Vec<u16> = buf[start..start + name_len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    Some(UsnRecord {
        frn,
        parent_frn,
        reason,
        file_attributes,
        name: String::from_utf16_lossy(&units),
    })
}

// ---- unaligned little-endian field reads ---------------------------------

#[inline]
fn read_u16(buf: &[u8], off: usize) -> u16 {
    assert!(buf.len() >= 2 && off <= buf.len() - 2);
    // SAFETY: bounds asserted above; read_unaligned tolerates any alignment.
    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const u16) }
}

#[inline]
fn read_u32(buf: &[u8], off: usize) -> u32 {
    assert!(buf.len() >= 4 && off <= buf.len() - 4);
    // SAFETY: bounds asserted above; read_unaligned tolerates any alignment.
    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const u32) }
}

#[inline]
pub(crate) fn read_u64(buf: &[u8], off: usize) -> u64 {
    assert!(buf.len() >= 8 && off <= buf.len() - 8);
    // SAFETY: bounds asserted above; read_unaligned tolerates any alignment.
    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const u64) }
}

#[inline]
pub(crate) fn read_i64(buf: &[u8], off: usize) -> i64 {
    read_u64(buf, off) as i64
}

// ---- test support --------------------------------------------------------

/// Build a synthetic `USN_RECORD_V2` byte image (tests only). `pad_to_8`
/// controls whether `RecordLength` itself includes the 8-byte-alignment
/// padding (the kernel always pads; the parser must tolerate both).
#[cfg(test)]
pub(crate) fn test_record(
    frn: u64,
    parent_frn: u64,
    reason: u32,
    attrs: u32,
    name: &str,
    pad_to_8: bool,
) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_bytes = units.len() * 2;
    let unpadded = USN_RECORD_V2_HEADER + name_bytes;
    let rec_len = if pad_to_8 {
        (unpadded + 7) & !7
    } else {
        unpadded
    };
    let alloc = (unpadded + 7) & !7; // buffer keeps 8-byte record alignment
    let mut b = vec![0u8; alloc];
    b[0..4].copy_from_slice(&(rec_len as u32).to_le_bytes());
    b[4..6].copy_from_slice(&2u16.to_le_bytes()); // MajorVersion
    b[8..16].copy_from_slice(&frn.to_le_bytes());
    b[16..24].copy_from_slice(&parent_frn.to_le_bytes());
    b[40..44].copy_from_slice(&reason.to_le_bytes());
    b[52..56].copy_from_slice(&attrs.to_le_bytes());
    b[56..58].copy_from_slice(&(name_bytes as u16).to_le_bytes());
    b[58..60].copy_from_slice(&(USN_RECORD_V2_HEADER as u16).to_le_bytes());
    for (i, u) in units.iter().enumerate() {
        let at = USN_RECORD_V2_HEADER + i * 2;
        b[at..at + 2].copy_from_slice(&u.to_le_bytes());
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_records(buf: &[u8]) -> Vec<UsnRecord> {
        let mut out = Vec::new();
        for_each_record(buf, |r| out.push(r));
        out
    }

    #[test]
    fn parses_two_packed_records() {
        let mut buf = test_record(0x0001_0000_0000_002A, 5, 0x100, 0x20, "hello.txt", true);
        buf.extend(test_record(
            0x0002_0000_0000_002B,
            42,
            0x200,
            0x10,
            "Ünïcode 日本語",
            true,
        ));
        let recs = collect_records(&buf);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].frn, 0x0001_0000_0000_002A);
        assert_eq!(recs[0].parent_frn, 5);
        assert_eq!(recs[0].reason, 0x100);
        assert_eq!(recs[0].file_attributes, 0x20);
        assert_eq!(recs[0].name, "hello.txt");
        assert_eq!(recs[1].name, "Ünïcode 日本語");
        assert_eq!(recs[1].file_attributes, 0x10);
    }

    #[test]
    fn unpadded_record_length_still_reaches_next_record() {
        // RecordLength excludes alignment padding; the next record starts on
        // the following 8-byte boundary. The parser must round up.
        let mut buf = test_record(1, 5, 0, 0, "abc", false); // 60 + 6 = 66, buffer 72
        buf.extend(test_record(2, 5, 0, 0, "def", true));
        let recs = collect_records(&buf);
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[1].frn, 2);
        assert_eq!(recs[1].name, "def");
    }

    #[test]
    fn zero_record_length_terminates() {
        let mut buf = test_record(7, 5, 0, 0, "a.txt", true);
        buf.extend(vec![0u8; 128]); // zeroed tail — RecordLength 0
        assert_eq!(collect_records(&buf).len(), 1);
    }

    #[test]
    fn truncated_or_oversized_record_stops_without_panic() {
        let mut buf = test_record(7, 5, 0, 0, "a.txt", true);
        let good = buf.len();
        // Second record claims to be longer than the remaining buffer.
        let mut bad = test_record(8, 5, 0, 0, "b.txt", true);
        bad[0..4].copy_from_slice(&4096u32.to_le_bytes());
        buf.extend(bad);
        let recs = collect_records(&buf[..good + 64]);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].frn, 7);
    }

    #[test]
    fn non_v2_records_are_skipped() {
        let mut first = test_record(1, 5, 0, 0, "v3ish", true);
        first[4..6].copy_from_slice(&3u16.to_le_bytes()); // MajorVersion = 3
        let mut buf = first;
        buf.extend(test_record(2, 5, 0, 0, "v2", true));
        let recs = collect_records(&buf);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].frn, 2);
    }

    #[test]
    fn malformed_name_bounds_skip_record_only() {
        let mut bad = test_record(1, 5, 0, 0, "abcd", true);
        // FileNameLength pointing past RecordLength.
        bad[56..58].copy_from_slice(&512u16.to_le_bytes());
        let mut buf = bad;
        buf.extend(test_record(2, 5, 0, 0, "ok.txt", true));
        let recs = collect_records(&buf);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "ok.txt");
    }

    #[test]
    fn attrs_map_to_flags() {
        assert_eq!(attrs_to_flags(0), 0);
        assert_eq!(attrs_to_flags(FILE_ATTRIBUTE_DIRECTORY), crate::flags::DIR);
        assert_eq!(
            attrs_to_flags(FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM),
            crate::flags::HIDDEN | crate::flags::SYSTEM
        );
        assert_eq!(
            attrs_to_flags(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_HIDDEN | 0x80),
            crate::flags::DIR | crate::flags::HIDDEN
        );
        // DEAD is the index's tombstone bit, not a filesystem attribute: no
        // attribute word may ever produce it, or a create would index a file
        // that search then refuses to see.
        assert_eq!(attrs_to_flags(u32::MAX) & crate::flags::DEAD, 0);
    }

    #[test]
    fn device_paths() {
        assert_eq!(volume_device_path("C:"), r"\\.\C:");
        assert_eq!(volume_device_path(r"C:\"), r"\\.\C:");
        assert_eq!(volume_device_path(r"\\.\D:"), r"\\.\D:");
        assert_eq!(
            volume_device_path(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\"),
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}"
        );
    }

    #[test]
    fn root_file_number_mask() {
        // Typical NTFS root FRN: sequence 5 in the high word, file number 5.
        assert_eq!(
            0x0005_0000_0000_0005u64 & FRN_FILE_NUMBER_MASK,
            ROOT_FILE_NUMBER
        );
        assert_ne!(
            0x0005_0000_0000_0006u64 & FRN_FILE_NUMBER_MASK,
            ROOT_FILE_NUMBER
        );
    }
}
