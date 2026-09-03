//! Clipboard history (SPEC.md §7.4).
//!
//! A message-only window listens for `WM_CLIPBOARDUPDATE`, snapshots what
//! arrived, and stores it; the launcher shows the history as a view you
//! search, and picking an entry restores focus and pastes it.
//!
//! **Storage, deviating from §7.4's SQLCipher.** Entries are stored in a
//! plain SQLite file with every value encrypted individually through DPAPI
//! (`CryptProtectData`, `CRYPTPROTECT_UI_FORBIDDEN`). §7.4 asked for
//! SQLCipher with a DPAPI-protected key; that is one more layer of key
//! management for the same guarantee, and it drags OpenSSL into every build
//! and every CI run. Encrypting the values directly gives the same
//! at-rest protection — the ciphertext is bound to the user's login
//! credentials and there is no key for us to store or leak — at the cost of
//! leaving row metadata (timestamp, source app, kind) in the clear, which is
//! recorded in `docs/M1.md` as the accepted trade.
//!
//! **Everything a row shows is encrypted too, not just its content.** A
//! clipboard entry is usually short — a password is well under the preview
//! length — so a plaintext preview column would hand over exactly what the
//! encryption is for. Previews are decrypted into memory once, on a
//! background thread so startup does not wait for the crypto, and every
//! search runs from there without touching the disk.
//!
//! That decryption is per row, so the searchable window is capped at
//! [`INDEX_ENTRIES`] — the most recent entries, which is what a history is
//! for — while retention on disk keeps §7.4's full 10,000.
//!
//! Images (`CF_DIB`/`CF_DIBV5`) are **not** captured yet: text and file
//! lists are the overwhelming majority of what a launcher's history is for,
//! and DIB decoding is a body of work of its own. Recorded as deferred.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, GetClipboardOwner,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL, VK_V,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowThreadProcessId,
    RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLIPBOARDUPDATE, WNDCLASSW,
};

use crate::matcher::{self, Ranges, Target};

/// §7.4 defaults: 30 days, 10,000 entries.
const RETENTION_DAYS: i64 = 30;
const MAX_ENTRIES: usize = 10_000;
/// How many of those are decrypted into the searchable in-memory index.
/// Each one is a DPAPI call, so this is the knob that keeps loading the
/// history off the startup path; beyond it, entries are retained but not
/// searched. For comparison, Windows' own clipboard history keeps 25.
const INDEX_ENTRIES: usize = 1_000;
/// What a row shows, and what the matcher sees. Long text is matched on its
/// head — a clipboard entry is found by how it starts, not by its middle.
const PREVIEW_CHARS: usize = 200;
/// Refuse to store anything larger; a giant paste is not history, it is a
/// file, and encrypting megabytes on the clipboard thread would stall it.
const MAX_CONTENT_BYTES: usize = 1 << 20;
pub const MAX_RESULTS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClipKind {
    Text,
    Files,
}

impl ClipKind {
    fn as_str(self) -> &'static str {
        match self {
            ClipKind::Text => "text",
            ClipKind::Files => "files",
        }
    }

    fn parse(s: &str) -> ClipKind {
        match s {
            "files" => ClipKind::Files,
            _ => ClipKind::Text,
        }
    }
}

/// One history entry as the view sees it. The full content lives on disk,
/// encrypted; only the preview is held in memory.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipMatch {
    pub id: i64,
    pub kind: ClipKind,
    pub preview: String,
    pub source: String,
    /// Unix seconds.
    pub ts: i64,
    pub score: f32,
    pub match_ranges: Ranges,
}

struct Entry {
    id: i64,
    kind: ClipKind,
    preview: String,
    source: String,
    ts: i64,
    /// Hash of the full content, for the "same thing copied twice" check.
    hash: u64,
    target: Target,
}

pub struct ClipboardStore {
    conn: Mutex<Option<rusqlite::Connection>>,
    entries: Mutex<Vec<Entry>>,
    /// §7.4's exclusion formats, registered once.
    exclusions: Exclusions,
    enabled: AtomicBool,
}

#[derive(Clone, Copy)]
struct Exclusions {
    exclude_from_monitor: u32,
    can_include_in_history: u32,
    can_upload_to_cloud: u32,
}

impl Exclusions {
    fn register() -> Exclusions {
        // SAFETY: static format names; registration is idempotent per name
        // and returns 0 only on failure, which reads as "never matches".
        unsafe {
            Exclusions {
                exclude_from_monitor: RegisterClipboardFormatW(w!(
                    "ExcludeClipboardContentFromMonitorProcessing"
                )),
                can_include_in_history: RegisterClipboardFormatW(w!(
                    "CanIncludeInClipboardHistory"
                )),
                can_upload_to_cloud: RegisterClipboardFormatW(w!("CanUploadToCloudClipboard")),
            }
        }
    }
}

impl ClipboardStore {
    pub fn open() -> Arc<ClipboardStore> {
        let path = std::env::var_os("LOCALAPPDATA").map(|b| {
            std::path::PathBuf::from(b)
                .join("YSpot")
                .join("clipboard.db")
        });
        let store = Arc::new(ClipboardStore {
            conn: Mutex::new(None),
            entries: Mutex::new(Vec::new()),
            exclusions: Exclusions::register(),
            enabled: AtomicBool::new(true),
        });
        match path {
            Some(p) => {
                // Loading decrypts one preview per entry, so it happens off
                // the startup path: the store is usable immediately and the
                // history fills in behind it.
                let me = store.clone();
                let spawned = std::thread::Builder::new()
                    .name("clipboard-load".into())
                    .spawn(move || me.attach(&p));
                if let Err(e) = spawned {
                    log::error!("clipboard: loader thread failed to spawn: {e}");
                }
            }
            None => log::error!("clipboard: LOCALAPPDATA unset; history is memory-only"),
        }
        store
    }

    pub(crate) fn attach(&self, path: &std::path::Path) {
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                log::warn!("clipboard: create {}: {e}", dir.display());
                return;
            }
        }
        match open_db(path) {
            Ok((conn, rows)) => {
                log::info!(
                    "clipboard: {} entries loaded from {}",
                    rows.len(),
                    path.display()
                );
                // The connection goes in FIRST: a capture that lands while
                // this is running must reach the database, and the index it
                // prepends to is replaced below rather than appended to.
                *self.conn.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn);
                let mut index = self.entries.lock().unwrap_or_else(|e| e.into_inner());
                // Anything captured during the load is newer than everything
                // on disk, so it stays at the front.
                let seen: std::collections::HashSet<u64> = index.iter().map(|e| e.hash).collect();
                index.extend(rows.into_iter().filter(|r| !seen.contains(&r.hash)));
                index.truncate(INDEX_ENTRIES);
            }
            Err(e) => log::error!(
                "clipboard: cannot open {} ({e}); history is memory-only",
                path.display()
            ),
        }
    }

    /// Search the history. An empty query lists the most recent entries,
    /// which is what opening the view should show.
    pub fn match_query(&self, query: &str, max: usize) -> Vec<ClipMatch> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let q = matcher::fold_query(query);
        let mut hits: Vec<ClipMatch> = if q.is_empty() {
            entries
                .iter()
                .map(|e| e.to_match(1.0, Vec::new()))
                .collect()
        } else {
            let mut scored: Vec<(f32, &Entry, Ranges)> = entries
                .iter()
                .filter_map(|e| matcher::score(&e.target, &q).map(|(s, r)| (s, e, r)))
                .collect();
            // Recency breaks ties: two equally good matches, the newer one is
            // the one you meant.
            scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.ts.cmp(&a.1.ts)));
            scored
                .into_iter()
                .map(|(s, e, r)| e.to_match(s, r))
                .collect()
        };
        hits.truncate(max);
        hits
    }

    /// The full text of an entry, decrypted. A file list comes back as one
    /// path per line, which is what it was stored as.
    pub fn content(&self, id: i64) -> Option<String> {
        let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let conn = guard.as_ref()?;
        let blob: Vec<u8> = conn
            .query_row("SELECT content FROM entries WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .ok()?;
        String::from_utf8(unprotect(&blob)?).ok()
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|e| e.id != id);
        let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = guard.as_ref() {
            conn.execute("DELETE FROM entries WHERE id = ?1", [id])
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn clear(&self) -> Result<(), String> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = guard.as_ref() {
            conn.execute("DELETE FROM entries", [])
                .map_err(|e| e.to_string())?;
        }
        log::info!("clipboard history cleared");
        Ok(())
    }

    /// Stop or resume capture — the setting a user reaches for when they are
    /// about to paste something they do not want remembered.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::SeqCst);
        log::info!(
            "clipboard capture {}",
            if on { "enabled" } else { "paused" }
        );
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Take whatever is on the clipboard now and remember it.
    fn capture(&self) {
        if !self.is_enabled() {
            return;
        }
        let Some(snap) = read_clipboard(&self.exclusions) else {
            return;
        };
        let hash = fnv1a(snap.text.as_bytes());
        {
            // The same thing copied twice is one entry, moved to the top —
            // and this is also what keeps our own "copy path" action from
            // filling the history with duplicates.
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(pos) = entries.iter().position(|e| e.hash == hash) {
                let mut existing = entries.remove(pos);
                existing.ts = now_ts();
                let id = existing.id;
                let ts = existing.ts;
                entries.insert(0, existing);
                drop(entries);
                let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(conn) = guard.as_ref() {
                    let _ = conn.execute("UPDATE entries SET ts = ?2 WHERE id = ?1", [id, ts]);
                }
                return;
            }
        }
        let ts = now_ts();
        let preview: String = snap.text.chars().take(PREVIEW_CHARS).collect();
        let (Some(content), Some(preview_enc)) =
            (protect(snap.text.as_bytes()), protect(preview.as_bytes()))
        else {
            log::warn!("clipboard: DPAPI protect failed; entry dropped");
            return;
        };
        let id = {
            let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(conn) => {
                    let r = conn.execute(
                        "INSERT INTO entries (kind, content, preview, source, ts, hash)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        rusqlite::params![
                            snap.kind.as_str(),
                            content,
                            preview_enc,
                            snap.source,
                            ts,
                            hash as i64
                        ],
                    );
                    match r {
                        Ok(_) => conn.last_insert_rowid(),
                        Err(e) => {
                            log::warn!("clipboard: insert failed: {e}");
                            return;
                        }
                    }
                }
                // Memory-only: a negative id cannot collide with a rowid.
                None => -(ts * 1000 + (hash % 1000) as i64),
            }
        };
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.insert(
            0,
            Entry {
                id,
                kind: snap.kind,
                target: Target::new(&preview),
                preview,
                source: snap.source,
                ts,
                hash,
            },
        );
        entries.truncate(INDEX_ENTRIES);
        drop(entries);
        self.prune();
    }

    /// §7.4 retention: age and count, applied on the disk copy.
    fn prune(&self) {
        let guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = guard.as_ref() else { return };
        let cutoff = now_ts() - RETENTION_DAYS * 86_400;
        let _ = conn.execute("DELETE FROM entries WHERE ts < ?1", [cutoff]);
        let _ = conn.execute(
            "DELETE FROM entries WHERE id NOT IN
             (SELECT id FROM entries ORDER BY ts DESC LIMIT ?1)",
            [MAX_ENTRIES as i64],
        );
    }
}

impl Entry {
    fn to_match(&self, score: f32, match_ranges: Ranges) -> ClipMatch {
        ClipMatch {
            id: self.id,
            kind: self.kind,
            preview: self.preview.clone(),
            source: self.source.clone(),
            ts: self.ts,
            score,
            match_ranges,
        }
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn open_db(path: &std::path::Path) -> rusqlite::Result<(rusqlite::Connection, Vec<Entry>)> {
    let conn = rusqlite::Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         CREATE TABLE IF NOT EXISTS entries (
             id      INTEGER PRIMARY KEY AUTOINCREMENT,
             kind    TEXT NOT NULL,
             content BLOB NOT NULL,
             preview TEXT NOT NULL,
             source  TEXT NOT NULL,
             ts      INTEGER NOT NULL,
             hash    INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS entries_ts ON entries (ts DESC);",
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, kind, preview, source, ts, hash FROM entries ORDER BY ts DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([INDEX_ENTRIES as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    // An entry whose preview will not decrypt was written by another user or
    // another machine; it is skipped rather than shown as an empty row.
    let entries = rows
        .into_iter()
        .filter_map(|(id, kind, preview_enc, source, ts, hash)| {
            let preview = String::from_utf8(unprotect(&preview_enc)?).ok()?;
            Some(Entry {
                id,
                kind: ClipKind::parse(&kind),
                target: Target::new(&preview),
                preview,
                source,
                ts,
                hash: hash as u64,
            })
        })
        .collect();
    Ok((conn, entries))
}

// ---------------------------------------------------------------------------
// DPAPI

/// Encrypt for this user (§7.4's at-rest protection, applied per value).
fn protect(plain: &[u8]) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: `input` describes `plain` for the duration of the call; `out`
    // receives a LocalAlloc'd buffer copied and freed below. UI_FORBIDDEN
    // because this runs on a background thread with no window to prompt on.
    let ok = unsafe {
        CryptProtectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    ok.ok()?;
    Some(take_blob(out))
}

fn unprotect(cipher: &[u8]) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: cipher.len() as u32,
        pbData: cipher.as_ptr() as *mut u8,
    };
    let mut out = CRYPT_INTEGER_BLOB::default();
    // SAFETY: as above; a wrong or foreign ciphertext fails rather than
    // producing garbage, which is why the result is an Option.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    ok.ok()?;
    Some(take_blob(out))
}

/// Copy a DPAPI output blob and free the original.
fn take_blob(blob: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    // SAFETY: the API filled `blob` with a LocalAlloc'd buffer of `cbData`
    // bytes; copied once, then freed once.
    unsafe {
        let out = std::slice::from_raw_parts(blob.pbData, blob.cbData as usize).to_vec();
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            blob.pbData as *mut _,
        )));
        out
    }
}

// ---------------------------------------------------------------------------
// Reading the clipboard

struct Snapshot {
    kind: ClipKind,
    /// Text, or one path per line for a file list.
    text: String,
    source: String,
}

fn read_clipboard(ex: &Exclusions) -> Option<Snapshot> {
    // SAFETY: every path below closes the clipboard exactly once.
    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        let result = read_open_clipboard(ex);
        let _ = CloseClipboard();
        result
    }
}

/// # Safety
/// The clipboard must be open and owned by this thread.
unsafe fn read_open_clipboard(ex: &Exclusions) -> Option<Snapshot> {
    // §7.4 exclusions, checked before anything is read: a password manager
    // says "do not remember this", and the answer is to not even look.
    // SAFETY: format availability queries take no memory.
    unsafe {
        if ex.exclude_from_monitor != 0
            && IsClipboardFormatAvailable(ex.exclude_from_monitor).is_ok()
        {
            return None;
        }
        for fmt in [ex.can_include_in_history, ex.can_upload_to_cloud] {
            if fmt != 0 && IsClipboardFormatAvailable(fmt).is_ok() && dword_value(fmt) == Some(0) {
                return None;
            }
        }
    }
    let source = clipboard_source();
    // SAFETY: reading a format the clipboard advertises; the handle stays
    // owned by the clipboard, so it is locked and unlocked but never freed.
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
            let h = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
            let text = wide_from_handle(HGLOBAL(h.0))?;
            let trimmed = text.trim();
            if trimmed.is_empty() || text.len() > MAX_CONTENT_BYTES {
                return None;
            }
            return Some(Snapshot {
                kind: ClipKind::Text,
                text,
                source,
            });
        }
        if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
            let h = GetClipboardData(CF_HDROP.0 as u32).ok()?;
            let drop = HDROP(h.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let mut paths = Vec::with_capacity(count as usize);
            for i in 0..count {
                let len = DragQueryFileW(drop, i, None);
                if len == 0 {
                    continue;
                }
                let mut buf = vec![0u16; len as usize + 1];
                let n = DragQueryFileW(drop, i, Some(&mut buf));
                if n > 0 {
                    paths.push(String::from_utf16_lossy(&buf[..n as usize]));
                }
            }
            if paths.is_empty() {
                return None;
            }
            return Some(Snapshot {
                kind: ClipKind::Files,
                text: paths.join("\n"),
                source,
            });
        }
    }
    None
}

/// The DWORD a marker format carries, if it carries one.
///
/// # Safety
/// The clipboard must be open.
unsafe fn dword_value(format: u32) -> Option<u32> {
    // SAFETY: the caller holds the clipboard; the handle stays clipboard-owned.
    unsafe {
        let h = GetClipboardData(format).ok()?;
        let p = GlobalLock(HGLOBAL(h.0)) as *const u32;
        if p.is_null() {
            return None;
        }
        let v = *p;
        let _ = GlobalUnlock(HGLOBAL(h.0));
        Some(v)
    }
}

/// # Safety
/// `h` must be a clipboard-owned global holding a NUL-terminated wide string.
unsafe fn wide_from_handle(h: HGLOBAL) -> Option<String> {
    // SAFETY: locked and unlocked once; the string is NUL-terminated by the
    // CF_UNICODETEXT contract.
    unsafe {
        let p = GlobalLock(h) as *const u16;
        if p.is_null() {
            return None;
        }
        let mut n = 0usize;
        while *p.add(n) != 0 && n < MAX_CONTENT_BYTES {
            n += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
        let _ = GlobalUnlock(h);
        Some(s)
    }
}

/// §7.4: the app that put this on the clipboard, for the row's subtitle.
fn clipboard_source() -> String {
    // SAFETY: read-only queries; a null owner is normal and yields "".
    unsafe {
        let owner = GetClipboardOwner().unwrap_or_default();
        if owner.is_invalid() {
            return String::new();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(owner, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }
        crate::winman::process_name_of_pid(pid).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// The listener

/// Start the §7.4 capture listener on its own thread with a message-only
/// window and its own message loop.
///
/// A message-only window is what `AddClipboardFormatListener` is documented
/// against, and it costs nothing: no pixels, no taskbar, no focus.
pub fn spawn_listener(store: Arc<ClipboardStore>) {
    let spawned = std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || listener_thread(store));
    if let Err(e) = spawned {
        log::error!("clipboard listener thread failed to spawn: {e}");
    }
}

/// The store the window procedure reaches, set once before the window is
/// created and read only from that thread.
static mut LISTENER_STORE: Option<Arc<ClipboardStore>> = None;

fn listener_thread(store: Arc<ClipboardStore>) {
    // SAFETY: written once here before the window that reads it exists, and
    // read only from this thread's window procedure.
    unsafe { LISTENER_STORE = Some(store) };
    // SAFETY: a standard class registration and message-only window, with a
    // message loop that runs for the life of the process.
    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                log::error!("clipboard: GetModuleHandle failed: {e}");
                return;
            }
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: w!("YSpotClipboardListener"),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            log::error!("clipboard: RegisterClass failed");
            return;
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("YSpotClipboardListener"),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        );
        let Ok(hwnd) = hwnd else {
            log::error!("clipboard: message-only window creation failed");
            return;
        };
        if AddClipboardFormatListener(hwnd).is_err() {
            log::error!("clipboard: AddClipboardFormatListener failed; history will not record");
            return;
        }
        log::info!("clipboard listener running (§7.4)");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        // SAFETY: set before this window existed, read only on this thread.
        let store = unsafe { (*std::ptr::addr_of!(LISTENER_STORE)).clone() };
        if let Some(store) = store {
            store.capture();
        }
        return LRESULT(0);
    }
    // SAFETY: the documented default handler.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// Pasting

/// §7.4's paste: put the entry on the clipboard, restore the window that had
/// focus before the launcher took it, and inject Ctrl+V.
///
/// §7.4 also lists "paste as plain text" as a separate action. It has no
/// separate behaviour here and so is not offered: this history stores only
/// `CF_UNICODETEXT` (and file lists as their paths), so every paste already
/// writes plain text and nothing else. It becomes a real distinction the day
/// rich formats are captured.
pub fn paste(text: &str) -> Result<(), String> {
    crate::file_actions::copy_text(text)?;
    crate::focus::restore_foreground();
    // Give the restored window a moment to actually take focus; injecting
    // into a window that is not yet foreground pastes into nothing.
    std::thread::sleep(std::time::Duration::from_millis(60));
    // SAFETY: a well-formed four-event array of the size passed alongside
    // it; every key that goes down comes back up, so no modifier is left
    // stuck even if the target ignores the paste.
    unsafe {
        let mut inputs = [INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_CONTROL,
                    ..Default::default()
                },
            },
        }; 4];
        inputs[1].Anonymous.ki.wVk = VK_V;
        inputs[2].Anonymous.ki.wVk = VK_V;
        inputs[2].Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
        inputs[3].Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent as usize != inputs.len() {
            return Err("the paste keystroke was blocked".to_string());
        }
    }
    Ok(())
}

/// Type witness: `HANDLE` is used through the clipboard APIs above.
#[allow(dead_code)]
fn _handle_type(_: HANDLE) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_round_trips_and_rejects_garbage() {
        let secret = b"a password, probably";
        let sealed = protect(secret).expect("protect");
        assert_ne!(sealed.as_slice(), secret, "stored in the clear");
        assert_eq!(unprotect(&sealed).as_deref(), Some(&secret[..]));
        // Ciphertext that is not ours does not decrypt to something.
        assert!(unprotect(b"not a dpapi blob").is_none());
        assert!(unprotect(&[]).is_none());
    }

    #[test]
    fn dpapi_handles_empty_and_large_values() {
        let empty = protect(b"").expect("protect empty");
        assert_eq!(unprotect(&empty).as_deref(), Some(&b""[..]));
        let big = vec![7u8; 300_000];
        let sealed = protect(&big).expect("protect large");
        assert_eq!(unprotect(&sealed), Some(big));
    }

    fn store_at(tag: &str) -> (Arc<ClipboardStore>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("yspot-clip-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clipboard.db");
        let store = Arc::new(ClipboardStore {
            conn: Mutex::new(None),
            entries: Mutex::new(Vec::new()),
            exclusions: Exclusions::register(),
            enabled: AtomicBool::new(true),
        });
        store.attach(&path);
        (store, dir)
    }

    /// Insert without going through the real clipboard, so the store's own
    /// behaviour is testable without touching the machine's clipboard.
    fn remember(store: &ClipboardStore, text: &str, source: &str) {
        let snap = Snapshot {
            kind: ClipKind::Text,
            text: text.to_string(),
            source: source.to_string(),
        };
        let hash = fnv1a(snap.text.as_bytes());
        let content = protect(snap.text.as_bytes()).unwrap();
        let ts = now_ts();
        let preview: String = snap.text.chars().take(PREVIEW_CHARS).collect();
        let preview_enc = protect(preview.as_bytes()).unwrap();
        let id = {
            let guard = store.conn.lock().unwrap();
            let conn = guard.as_ref().unwrap();
            conn.execute(
                "INSERT INTO entries (kind, content, preview, source, ts, hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    snap.kind.as_str(),
                    content,
                    preview_enc,
                    snap.source,
                    ts,
                    hash as i64
                ],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        store.entries.lock().unwrap().insert(
            0,
            Entry {
                id,
                kind: snap.kind,
                target: Target::new(&preview),
                preview,
                source: snap.source,
                ts,
                hash,
            },
        );
    }

    #[test]
    fn entries_persist_encrypted_and_come_back_decrypted() {
        let (store, dir) = store_at("persist");
        remember(&store, "the quick brown fox", "notepad.exe");
        let id = store.match_query("", 10)[0].id;
        assert_eq!(store.content(id).unwrap(), "the quick brown fox");

        // On disk, nothing readable: the preview is a short entry's whole
        // content, so a plaintext preview column would defeat the point.
        drop(store);
        let bytes = std::fs::read(dir.join("clipboard.db")).unwrap();
        for needle in [&b"the quick brown fox"[..], &b"quick brown"[..]] {
            assert!(
                !bytes.windows(needle.len()).any(|w| w == needle),
                "clipboard text stored in the clear"
            );
        }

        // Reopening decrypts the index back into something searchable.
        let reopened = Arc::new(ClipboardStore {
            conn: Mutex::new(None),
            entries: Mutex::new(Vec::new()),
            exclusions: Exclusions::register(),
            enabled: AtomicBool::new(true),
        });
        reopened.attach(&dir.join("clipboard.db"));
        let hits = reopened.match_query("brown", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].preview, "the quick brown fox");
        assert_eq!(reopened.content(hits[0].id).unwrap(), "the quick brown fox");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_is_searchable_and_lists_newest_first_when_empty() {
        let (store, dir) = store_at("search");
        remember(&store, "first thing", "a.exe");
        remember(&store, "second thing", "b.exe");
        remember(&store, "unrelated", "c.exe");
        // An empty query is "show me the history", newest first.
        let all = store.match_query("", 10);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].preview, "unrelated");
        // A query filters it.
        let hits = store.match_query("second", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].preview, "second thing");
        assert_eq!(hits[0].source, "b.exe");
        assert!(store.match_query("zzqxjv", 10).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_and_clearing_remove_entries_from_memory_and_disk() {
        let (store, dir) = store_at("delete");
        remember(&store, "keep me", "a.exe");
        remember(&store, "drop me", "a.exe");
        let victim = store.match_query("drop", 10)[0].id;
        store.delete(victim).unwrap();
        assert_eq!(store.match_query("", 10).len(), 1);
        assert!(store.content(victim).is_none());
        store.clear().unwrap();
        assert!(store.match_query("", 10).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_can_be_paused() {
        let (store, dir) = store_at("pause");
        assert!(store.is_enabled());
        store.set_enabled(false);
        assert!(!store.is_enabled());
        // capture() returns immediately while paused, whatever is on the
        // clipboard — verified by the history staying empty.
        store.capture();
        assert!(store.match_query("", 10).is_empty());
        store.set_enabled(true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_exclusion_formats_register() {
        let ex = Exclusions::register();
        // Zero means the registration failed, which would silently disable
        // the §7.4 exclusions a password manager relies on.
        assert_ne!(ex.exclude_from_monitor, 0);
        assert_ne!(ex.can_include_in_history, 0);
        assert_ne!(ex.can_upload_to_cloud, 0);
    }
}
