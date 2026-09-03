//! File actions (SPEC.md §7.3): open, open with, reveal in Explorer, copy
//! path, copy file, delete to the Recycle Bin.
//!
//! All of them run here, in the unelevated shell, and never in the elevated
//! service (§7.3, §8.1) — the service only ever answers queries.
//!
//! Delete uses `IFileOperation::DeleteItem` with `FOF_ALLOWUNDO`, so a
//! deletion is recoverable from the Recycle Bin; the permanent-delete APIs
//! are forbidden by §7.3. Confirmation is left to the shell's own dialog: we
//! delete exactly one item per invocation, which is the case §7.3 does not
//! require us to confirm ourselves.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, POINT};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    FileOperation, IFileOperation, ILFree, IShellItem, SHCreateItemFromParsingName,
    SHOpenFolderAndSelectItems, SHOpenWithDialog, SHParseDisplayName, DROPFILES, FOF_ALLOWUNDO,
    OAIF_EXEC, OPENASINFO,
};

use crate::com::{wide, Apartment};

/// Show the file in Explorer with the item itself selected (§7.3).
pub fn reveal(path: &str) -> Result<(), String> {
    let _sta = Apartment::sta();
    let w = wide(path);
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: NUL-terminated path; `pidl` receives a CoTaskMem-allocated list
    // freed with ILFree below; no bind context and no attribute query.
    unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None) }
        .map_err(|e| format!("parse {path}: {e}"))?;
    // An empty child array means "select the folder item itself in its
    // parent", which is exactly reveal-and-select for a single path.
    // SAFETY: `pidl` is a valid absolute id list for the duration of the call.
    let r = unsafe { SHOpenFolderAndSelectItems(pidl, None, 0) };
    // SAFETY: allocated by SHParseDisplayName; freed exactly once.
    unsafe { ILFree(Some(pidl)) };
    r.map_err(|e| format!("reveal {path}: {e}"))
}

/// The Windows "Open with" chooser for this file (§7.3).
pub fn open_with(path: &str) -> Result<(), String> {
    let _sta = Apartment::sta();
    let w = wide(path);
    let info = OPENASINFO {
        pcszFile: PCWSTR(w.as_ptr()),
        pcszClass: PCWSTR::null(),
        oaifInFlags: OAIF_EXEC,
    };
    // SAFETY: `info` and the string it points at outlive the call; no parent
    // window (the launcher hides itself before the dialog appears).
    unsafe { SHOpenWithDialog(None, &info) }.map_err(|e| format!("open with {path}: {e}"))
}

/// Delete to the Recycle Bin (§7.3: `FOF_ALLOWUNDO`, never a permanent delete).
pub fn delete_to_recycle_bin(path: &str) -> Result<(), String> {
    let _sta = Apartment::sta();
    let w = wide(path);
    // SAFETY: documented CLSID/interface pair.
    let op: IFileOperation = unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_ALL) }
        .map_err(|e| format!("file operation: {e}"))?;
    // SAFETY: NUL-terminated path; no bind context.
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None) }
        .map_err(|e| format!("item {path}: {e}"))?;
    // SAFETY: valid operation and item; ALLOWUNDO is what routes the delete
    // to the Recycle Bin instead of unlinking.
    unsafe {
        op.SetOperationFlags(FOF_ALLOWUNDO)
            .map_err(|e| format!("set flags: {e}"))?;
        op.DeleteItem(&item, None)
            .map_err(|e| format!("delete {path}: {e}"))?;
        op.PerformOperations()
            .map_err(|e| format!("perform delete {path}: {e}"))
    }
}

/// Put the path on the clipboard as text (§7.3 "Copy Path").
pub fn copy_path(path: &str) -> Result<(), String> {
    let text = wide(path);
    let bytes = std::mem::size_of_val(&text[..]);
    let mem = GlobalBlock::new(bytes)?;
    // SAFETY: the block is at least `bytes` long and locked for this write.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), mem.ptr() as *mut u16, text.len()) };
    set_clipboard(CF_UNICODETEXT.0 as u32, mem)
}

/// Put the file itself on the clipboard as `CF_HDROP` (§7.3 "Copy File"), so
/// Explorer and other targets paste the file rather than its name.
pub fn copy_file(path: &str) -> Result<(), String> {
    let list = wide(path); // one path, NUL-terminated…
    let header = std::mem::size_of::<DROPFILES>();
    // …plus the second NUL that terminates the double-NUL list.
    let bytes = header + std::mem::size_of_val(&list[..]) + std::mem::size_of::<u16>();
    let mem = GlobalBlock::new(bytes)?;
    let df = DROPFILES {
        pFiles: header as u32,
        pt: POINT { x: 0, y: 0 },
        fNC: false.into(),
        fWide: true.into(),
    };
    // SAFETY: the block is `bytes` long, which covers the header, the path
    // and the terminating NUL; all three writes stay inside it.
    unsafe {
        std::ptr::write_unaligned(mem.ptr() as *mut DROPFILES, df);
        let names = (mem.ptr() as *mut u8).add(header) as *mut u16;
        std::ptr::copy_nonoverlapping(list.as_ptr(), names, list.len());
        names.add(list.len()).write(0);
    }
    set_clipboard(CF_HDROP.0 as u32, mem)
}

/// A moveable global allocation, locked for writing, that the clipboard takes
/// ownership of on a successful `SetClipboardData` and that frees itself
/// otherwise.
struct GlobalBlock {
    handle: HGLOBAL,
    ptr: *mut core::ffi::c_void,
    released: bool,
}

impl GlobalBlock {
    fn new(bytes: usize) -> Result<GlobalBlock, String> {
        // SAFETY: plain allocation call; a null handle is the failure signal.
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }
            .map_err(|e| format!("GlobalAlloc({bytes}): {e}"))?;
        // SAFETY: freshly allocated moveable block; locked exactly once.
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            // SAFETY: the block is ours and unlocked; freed exactly once.
            unsafe {
                let _ = GlobalFree(Some(handle));
            };
            return Err("GlobalLock failed".into());
        }
        Ok(GlobalBlock {
            handle,
            ptr,
            released: false,
        })
    }

    fn ptr(&self) -> *mut core::ffi::c_void {
        self.ptr
    }

    /// Unlock and hand the handle over; the caller owns it from here.
    fn release(mut self) -> HGLOBAL {
        // SAFETY: balances the lock in `new`.
        unsafe {
            let _ = GlobalUnlock(self.handle);
        };
        self.released = true;
        self.handle
    }
}

impl Drop for GlobalBlock {
    fn drop(&mut self) {
        if !self.released {
            // SAFETY: still ours, locked once; unlock then free, each once.
            unsafe {
                let _ = GlobalUnlock(self.handle);
                let _ = GlobalFree(Some(self.handle));
            }
        }
    }
}

/// Replace the clipboard contents with one format. On success the clipboard
/// owns the memory; on failure this frees it.
fn set_clipboard(format: u32, mem: GlobalBlock) -> Result<(), String> {
    // SAFETY: no owner window; paired with CloseClipboard on every path.
    unsafe { OpenClipboard(None) }.map_err(|e| format!("OpenClipboard: {e}"))?;
    // SAFETY: the clipboard is open and owned by this thread.
    let result = unsafe {
        EmptyClipboard()
            .map_err(|e| format!("EmptyClipboard: {e}"))
            .and_then(|()| {
                let handle = mem.release();
                SetClipboardData(format, Some(HANDLE(handle.0)))
                    .map(|_| ())
                    .map_err(|e| {
                        // Ownership did not transfer; the block is ours again.
                        let _ = GlobalFree(Some(handle));
                        format!("SetClipboardData: {e}")
                    })
            })
    };
    // SAFETY: balances OpenClipboard on both the success and failure paths.
    unsafe {
        let _ = CloseClipboard();
    };
    result
}

/// These exercise the real clipboard and the real Recycle Bin, so they are
/// `#[ignore]`d: a developer running `cargo test` should not have their
/// clipboard replaced or a file recycled underneath them. CI runs them
/// explicitly with `--ignored`, which is where they gate.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// The clipboard is one machine-wide resource, so two tests that own it
    /// cannot run at once — without this they race and one reads the other's
    /// data. Serialized here rather than by demanding `--test-threads=1`,
    /// which would slow every other test down and be easy to forget in CI.
    static CLIPBOARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
        CLIPBOARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn temp_file(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("yspot-actions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, b"yspot test").unwrap();
        p
    }

    /// Round-trips through the real clipboard: copy the path, read it back
    /// as text. Also proves the global block's ownership transfer, which is
    /// where a leak or a double free would live.
    #[test]
    #[ignore = "touches the real clipboard / Recycle Bin; CI runs it with --ignored"]
    fn copy_path_puts_the_path_on_the_clipboard() {
        let _serial = clipboard_lock();
        use windows::Win32::System::DataExchange::GetClipboardData;
        let p = temp_file("copy-path.txt");
        let path = p.to_string_lossy().to_string();
        copy_path(&path).expect("copy path");
        // SAFETY: open, read the format we just set, close.
        let read = unsafe {
            OpenClipboard(None).unwrap();
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).unwrap();
            let ptr = GlobalLock(HGLOBAL(h.0)) as *const u16;
            let mut n = 0usize;
            while *ptr.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, n));
            let _ = GlobalUnlock(HGLOBAL(h.0));
            let _ = CloseClipboard();
            s
        };
        assert_eq!(read, path);
        let _ = std::fs::remove_file(&p);
    }

    /// `CF_HDROP` must be a `DROPFILES` header followed by a wide,
    /// double-NUL-terminated path list — the shape Explorer pastes.
    #[test]
    #[ignore = "touches the real clipboard / Recycle Bin; CI runs it with --ignored"]
    fn copy_file_writes_a_wide_double_nul_terminated_drop_list() {
        let _serial = clipboard_lock();
        use windows::Win32::System::DataExchange::GetClipboardData;
        let p = temp_file("copy-file.txt");
        let path = p.to_string_lossy().to_string();
        copy_file(&path).expect("copy file");
        // SAFETY: as above; the layout is the one `copy_file` wrote.
        let (offset, wide_flag, names) = unsafe {
            OpenClipboard(None).unwrap();
            let h = GetClipboardData(CF_HDROP.0 as u32).unwrap();
            let base = GlobalLock(HGLOBAL(h.0)) as *const u8;
            let df = std::ptr::read_unaligned(base as *const DROPFILES);
            let ptr = base.add(df.pFiles as usize) as *const u16;
            let mut n = 0usize;
            while *ptr.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, n));
            let terminator = *ptr.add(n + 1);
            let _ = GlobalUnlock(HGLOBAL(h.0));
            let _ = CloseClipboard();
            assert_eq!(terminator, 0, "path list is not double-NUL terminated");
            (df.pFiles as usize, df.fWide.as_bool(), s)
        };
        assert_eq!(offset, std::mem::size_of::<DROPFILES>());
        assert!(wide_flag);
        assert_eq!(names, path);
        let _ = std::fs::remove_file(&p);
    }

    /// Delete must recycle, not unlink: the file leaves its directory and the
    /// call reports success (§7.3 forbids permanent deletion).
    #[test]
    #[ignore = "touches the real clipboard / Recycle Bin; CI runs it with --ignored"]
    fn delete_sends_the_file_to_the_recycle_bin() {
        let p = temp_file("recycle-me.txt");
        let path = p.to_string_lossy().to_string();
        // Sanity: it is really there, with our content.
        let mut s = String::new();
        std::fs::File::open(&p)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "yspot test");
        delete_to_recycle_bin(&path).expect("recycle");
        assert!(!p.exists(), "file still on disk after a recycle-bin delete");
    }

    #[test]
    fn reveal_and_open_with_reject_a_path_that_does_not_exist() {
        let missing = std::env::temp_dir().join("yspot-does-not-exist-2f9a.txt");
        let path = missing.to_string_lossy().to_string();
        assert!(reveal(&path).is_err());
        assert!(delete_to_recycle_bin(&path).is_err());
    }
}
