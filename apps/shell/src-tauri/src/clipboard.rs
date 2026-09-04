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
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VK_CONTROL, VK_V,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CLIPBOARDUPDATE, WNDCLASSW,
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
    /// The sealed full content, for an entry that has no row on disk yet.
    ///
    /// Captures that land before the loader thread installs the connection —
    /// or at all, when there is no database — would otherwise be listed with
    /// an id no `SELECT` can resolve, so the row is visible and unpastable.
    /// Holding the ciphertext here keeps the index self-sufficient; `attach`
    /// clears it as each entry gains a real row.
    pending: Option<Vec<u8>>,
}

/// Where the on-disk half of the history stands.
///
/// `clear` and `delete` are privacy controls, so "the database has not opened
/// yet", "it failed to open" and "there is no database to open" have to be
/// three different answers rather than one absent connection: only the last
/// of them means a deletion is already complete.
enum Db {
    /// The loader thread is still opening and decrypting. Anything captured
    /// now is memory-only until `attach` reconciles it.
    Loading,
    Ready(rusqlite::Connection),
    /// The open failed. Rows already on disk cannot be reached, and saying a
    /// deletion succeeded would be a lie about the thing that matters most.
    Failed,
    /// No store to open (no `LOCALAPPDATA`): memory-only by design, so there
    /// is nothing on disk that a deletion could miss.
    Absent,
}

pub struct ClipboardStore {
    db: Mutex<Db>,
    entries: Mutex<Vec<Entry>>,
    /// §7.4's exclusion formats, registered once.
    exclusions: Exclusions,
    enabled: AtomicBool,
    /// A `clear` that arrived while the store was still loading, to be
    /// applied by `attach` against the rows it has only just read.
    cleared_while_loading: AtomicBool,
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
    /// `capture` is the persisted §7.4 setting, so a paused history comes
    /// back paused rather than quietly recording again after a restart.
    pub fn open(capture: bool) -> Arc<ClipboardStore> {
        let path = std::env::var_os("LOCALAPPDATA").map(|b| {
            std::path::PathBuf::from(b)
                .join("YSpot")
                .join("clipboard.db")
        });
        let store = Arc::new(ClipboardStore {
            db: Mutex::new(if path.is_some() {
                Db::Loading
            } else {
                Db::Absent
            }),
            entries: Mutex::new(Vec::new()),
            exclusions: Exclusions::register(),
            enabled: AtomicBool::new(capture),
            cleared_while_loading: AtomicBool::new(false),
        });
        if !capture {
            log::info!("clipboard capture is paused (§7.4, from settings)");
        }
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
                *self.db.lock().unwrap_or_else(|e| e.into_inner()) = Db::Failed;
                return;
            }
        }
        let (conn, rows) = match open_db(path) {
            Ok(v) => v,
            Err(e) => {
                log::error!(
                    "clipboard: cannot open {} ({e}); history is memory-only",
                    path.display()
                );
                *self.db.lock().unwrap_or_else(|e| e.into_inner()) = Db::Failed;
                return;
            }
        };
        log::info!(
            "clipboard: {} entries loaded from {}",
            rows.len(),
            path.display()
        );
        {
            // Both locks, `db` before `entries`, which is the order every
            // other path takes them in. A capture that lands mid-reconcile
            // waits here and then sees a ready connection, so there is no
            // window in which an entry is written memory-only by accident.
            let mut guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
            let mut index = self.entries.lock().unwrap_or_else(|e| e.into_inner());

            // A clear issued while this was loading names exactly the rows
            // this thread has just read. Apply it here rather than letting
            // the load put back what the user asked us to forget.
            let rows = if self.cleared_while_loading.swap(false, Ordering::SeqCst) {
                if let Err(e) = conn.execute("DELETE FROM entries", []) {
                    log::error!("clipboard: deferred clear failed: {e}");
                }
                log::info!(
                    "clipboard: deferred clear applied; {} loaded rows discarded",
                    rows.len()
                );
                Vec::new()
            } else {
                rows
            };

            // Give every memory-only entry a real row. Oldest first, so the
            // rowids the inserts hand out ascend with time like the rest of
            // the table's do.
            for e in index.iter_mut().rev() {
                // Cloned, not borrowed: the entry is written to below, and a
                // reference into its own `pending` would still be live.
                let Some(content) = e.pending.clone() else {
                    continue;
                };
                // The same text may already be stored from an earlier
                // session. Adopt that row — it is the one holding the
                // content blob — instead of inserting a second copy.
                let existing: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM entries WHERE hash = ?1",
                        [e.hash as i64],
                        |r| r.get(0),
                    )
                    .ok();
                if let Some(id) = existing {
                    let _ = conn.execute("UPDATE entries SET ts = ?2 WHERE id = ?1", [id, e.ts]);
                    e.id = id;
                    e.pending = None;
                    continue;
                }
                let Some(preview_enc) = protect(e.preview.as_bytes()) else {
                    log::warn!("clipboard: DPAPI protect failed; entry stays memory-only");
                    continue;
                };
                let r = conn.execute(
                    "INSERT INTO entries (kind, content, preview, source, ts, hash)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        e.kind.as_str(),
                        content,
                        preview_enc,
                        e.source,
                        e.ts,
                        e.hash as i64
                    ],
                );
                match r {
                    Ok(_) => {
                        e.id = conn.last_insert_rowid();
                        e.pending = None;
                    }
                    // Left memory-only: still listed, still pastable from
                    // `pending`, just not durable.
                    Err(err) => log::warn!("clipboard: reconciling insert failed: {err}"),
                }
            }

            // Anything captured during the load is newer than everything on
            // disk, so it stays at the front — and now that duplicates have
            // adopted the stored row's id, dropping the disk copy here drops
            // a genuine duplicate rather than the only usable one.
            let seen: std::collections::HashSet<u64> = index.iter().map(|e| e.hash).collect();
            index.extend(rows.into_iter().filter(|r| !seen.contains(&r.hash)));
            index.truncate(INDEX_ENTRIES);
            *guard = Db::Ready(conn);
        }
        // §7.4's limits are about how long something is kept, not about how
        // recently something was copied, so they apply on every start.
        self.prune();
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
        // An entry with no disk row carries its own sealed copy: whatever
        // the index lists has to be pastable.
        let pending = {
            let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| e.pending.clone())
        };
        if let Some(blob) = pending {
            return String::from_utf8(unprotect(&blob)?).ok();
        }
        let guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let Db::Ready(conn) = &*guard else {
            return None;
        };
        let blob: Vec<u8> = conn
            .query_row("SELECT content FROM entries WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .ok()?;
        String::from_utf8(unprotect(&blob)?).ok()
    }

    /// Forget one entry, on disk as well as in the index.
    ///
    /// Reports failure rather than success when the row cannot be reached: a
    /// history that says "deleted" and keeps the row is worse than one that
    /// admits it could not.
    pub fn delete(&self, id: i64) -> Result<(), String> {
        let guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|e| e.id != id);
        match &*guard {
            Db::Ready(conn) => conn
                .execute("DELETE FROM entries WHERE id = ?1", [id])
                .map(|_| ())
                .map_err(|e| e.to_string()),
            // A memory-only entry has no row anywhere else, so dropping it
            // from the index above is the whole of the deletion.
            _ if id < 0 => Ok(()),
            Db::Absent => Ok(()),
            Db::Loading => Err("clipboard history is still loading — try again in a moment".into()),
            Db::Failed => Err(
                "clipboard history could not be opened, so the stored copy is still there".into(),
            ),
        }
    }

    pub fn clear(&self) -> Result<(), String> {
        let guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        match &*guard {
            Db::Ready(conn) => {
                conn.execute("DELETE FROM entries", [])
                    .map_err(|e| e.to_string())?;
                log::info!("clipboard history cleared");
                Ok(())
            }
            // The rows exist but this thread cannot name them yet. Record
            // the intent: `attach` applies it against what it loaded, so the
            // success reported here is one that actually happens.
            Db::Loading => {
                self.cleared_while_loading.store(true, Ordering::SeqCst);
                log::info!("clipboard history cleared (applied when the store finishes loading)");
                Ok(())
            }
            Db::Absent => Ok(()),
            Db::Failed => Err(
                "clipboard history could not be opened, so the stored copy is still there".into(),
            ),
        }
    }

    /// Stop or resume capture — the setting a user reaches for when they are
    /// about to paste something they do not want remembered.
    ///
    /// Persisting it is the caller's job (`clipboard_set_enabled`), because a
    /// pause that forgets itself at the next logon is worse than no pause at
    /// all: the user believes the history is still off.
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
                let guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
                if let Db::Ready(conn) = &*guard {
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
        let (id, pending) = {
            let guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
            match &*guard {
                Db::Ready(conn) => {
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
                        Ok(_) => (conn.last_insert_rowid(), None),
                        Err(e) => {
                            log::warn!("clipboard: insert failed: {e}");
                            return;
                        }
                    }
                }
                // Memory-only: a negative id cannot collide with a rowid, and
                // the sealed content rides along so the entry is pastable
                // before — or without — a database.
                _ => (-(ts * 1000 + (hash % 1000) as i64), Some(content)),
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
                pending,
            },
        );
        entries.truncate(INDEX_ENTRIES);
        drop(entries);
        self.prune();
    }

    /// §7.4 retention: age and count.
    ///
    /// Applied to the in-memory index as well as the table. They are two
    /// views of one history, and an entry the index still lists after its row
    /// has been reaped is one the user can select but not paste.
    fn prune(&self) {
        let cutoff = now_ts() - RETENTION_DAYS * 86_400;
        let guard = self.db.lock().unwrap_or_else(|e| e.into_inner());
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|e| e.ts >= cutoff);
        let Db::Ready(conn) = &*guard else { return };
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
    // The age cutoff is applied here as well as in `prune`, so the index is
    // never populated with rows that retention is about to reap.
    let mut stmt = conn.prepare(
        "SELECT id, kind, preview, source, ts, hash FROM entries
         WHERE ts >= ?2 ORDER BY ts DESC LIMIT ?1",
    )?;
    let cutoff = now_ts() - RETENTION_DAYS * 86_400;
    let rows = stmt
        .query_map([INDEX_ENTRIES as i64, cutoff], |r| {
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
                pending: None,
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

/// How many times to retry opening the clipboard, and how long to wait
/// between attempts.
///
/// The clipboard is one global lock, and `WM_CLIPBOARDUPDATE` arrives while
/// the app that just wrote it may still hold it open. A single failed
/// `OpenClipboard` would mean silently missing the entry — for a clipboard
/// history, "you copied something and it is not in the list" is the whole
/// product failing, so it is worth a few milliseconds of patience. Observed
/// in practice: two writers in quick succession make the loser fail.
const OPEN_ATTEMPTS: u32 = 8;
const OPEN_RETRY: std::time::Duration = std::time::Duration::from_millis(15);

fn read_clipboard(ex: &Exclusions) -> Option<Snapshot> {
    for attempt in 0..OPEN_ATTEMPTS {
        // SAFETY: on success every path below closes the clipboard exactly
        // once; on failure nothing was opened.
        unsafe {
            if OpenClipboard(None).is_ok() {
                let result = read_open_clipboard(ex);
                let _ = CloseClipboard();
                return result;
            }
        }
        if attempt + 1 < OPEN_ATTEMPTS {
            std::thread::sleep(OPEN_RETRY);
        }
    }
    // Worth a line: a miss here is an entry the user expects to find later
    // and will not, and the cause is another process, not us.
    log::debug!(
        "clipboard: could not open after {OPEN_ATTEMPTS} attempts; entry missed          (another process is holding it)"
    );
    None
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
    // The block comes from another process and "this format carries a DWORD"
    // is only a convention, so its size is checked rather than assumed — a
    // short block would be read past the end, and the garbage that came back
    // would decide whether §7.4's exclusion is honoured.
    unsafe {
        let h = GetClipboardData(format).ok()?;
        let block = HGLOBAL(h.0);
        let p = GlobalLock(block) as *const u32;
        if p.is_null() {
            return None;
        }
        let v = if GlobalSize(block) >= std::mem::size_of::<u32>() {
            Some(*p)
        } else {
            log::debug!("clipboard: marker format {format} is smaller than a DWORD; ignored");
            None
        };
        let _ = GlobalUnlock(block);
        v
    }
}

/// # Safety
/// `h` must be a clipboard-owned global holding a NUL-terminated wide string.
unsafe fn wide_from_handle(h: HGLOBAL) -> Option<String> {
    // SAFETY: locked and unlocked once. The CF_UNICODETEXT contract says the
    // string is NUL-terminated, but the block belongs to another process, so
    // the scan is bounded by the block's actual size as well — a producer
    // that omits the terminator must not walk us off the end of it.
    unsafe {
        let p = GlobalLock(h) as *const u16;
        if p.is_null() {
            return None;
        }
        let limit = (GlobalSize(h) / std::mem::size_of::<u16>()).min(MAX_CONTENT_BYTES);
        let mut n = 0usize;
        while n < limit && *p.add(n) != 0 {
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

thread_local! {
    /// The store the window procedure reaches.
    ///
    /// Thread-local rather than a `static mut`: the window procedure runs on
    /// exactly the thread that created the window and pumps its messages, so
    /// the store never needs to cross threads, and a thread-local says that
    /// in the type system instead of in a comment. A `static mut` would also
    /// be a data race the moment anything called `spawn_listener` twice.
    static LISTENER_STORE: std::cell::RefCell<Option<Arc<ClipboardStore>>> =
        const { std::cell::RefCell::new(None) };
}

fn listener_thread(store: Arc<ClipboardStore>) {
    LISTENER_STORE.with(|s| *s.borrow_mut() = Some(store));
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
        // Cloned out of the cell before `capture` runs: capture takes locks
        // and could, in principle, pump messages, and holding a `RefCell`
        // borrow across that would panic on re-entry.
        let store = LISTENER_STORE.with(|s| s.borrow().clone());
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
/// `target` is the window the caller restored focus to — `dismiss` does that,
/// because the launcher has to be out of the way first. Injection happens only
/// if that window really is foreground: Windows can refuse
/// `SetForegroundWindow` under the foreground lock and activate whatever is
/// next in Z-order instead, and synthesising Ctrl+V into a window nobody chose
/// is how a stored password gets typed into a chat box.
pub fn paste(text: &str, target: Option<isize>) -> Result<(), String> {
    crate::file_actions::copy_text(text)?;
    const NOT_PASTED: &str = "could not return focus to the previous window — the entry is on \
                              the clipboard, so you can paste it yourself";
    let Some(target) = target else {
        return Err(NOT_PASTED.into());
    };
    // Give the restored window a moment to actually take focus; injecting
    // into a window that is not yet foreground pastes into nothing.
    std::thread::sleep(std::time::Duration::from_millis(60));
    // SAFETY: GetForegroundWindow has no preconditions and may return null.
    if unsafe { GetForegroundWindow() }.0 as isize != target {
        log::warn!("clipboard: paste target is not foreground; Ctrl+V withheld (§7.4)");
        return Err(NOT_PASTED.into());
    }
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

    /// A store that has not been attached to anything yet — the state the
    /// real one is in for the whole of its background load.
    fn loose_store() -> Arc<ClipboardStore> {
        Arc::new(ClipboardStore {
            db: Mutex::new(Db::Loading),
            entries: Mutex::new(Vec::new()),
            exclusions: Exclusions::register(),
            enabled: AtomicBool::new(true),
            cleared_while_loading: AtomicBool::new(false),
        })
    }

    fn store_at(tag: &str) -> (Arc<ClipboardStore>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("yspot-clip-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clipboard.db");
        let store = loose_store();
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
            let guard = store.db.lock().unwrap();
            let Db::Ready(conn) = &*guard else {
                panic!("store is not open")
            };
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
                pending: None,
            },
        );
    }

    /// What `capture` does when there is no connection yet: index the entry
    /// with a negative id and its own sealed copy of the content.
    fn remember_in_memory(store: &ClipboardStore, text: &str, source: &str) {
        let hash = fnv1a(text.as_bytes());
        let ts = now_ts();
        let preview: String = text.chars().take(PREVIEW_CHARS).collect();
        store.entries.lock().unwrap().insert(
            0,
            Entry {
                id: -(ts * 1000 + (hash % 1000) as i64),
                kind: ClipKind::Text,
                target: Target::new(&preview),
                preview,
                source: source.to_string(),
                ts,
                hash,
                pending: Some(protect(text.as_bytes()).unwrap()),
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
        let reopened = loose_store();
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

    /// An entry captured during the background load has no row to select
    /// from, and used to be listed with an id nothing could resolve.
    #[test]
    fn an_entry_captured_before_the_database_opens_is_still_pastable() {
        let store = loose_store();
        remember_in_memory(&store, "typed while loading", "a.exe");
        let hit = &store.match_query("", 10)[0];
        assert!(hit.id < 0, "memory-only entries carry a synthetic id");
        assert_eq!(
            store.content(hit.id).as_deref(),
            Some("typed while loading"),
            "listed but unpastable"
        );
    }

    /// ...and once the loader lands, it gains a real row rather than staying
    /// a second-class entry for the life of the process.
    #[test]
    fn attach_gives_entries_captured_during_the_load_a_real_row() {
        // A previous session stored something; this session re-copies the
        // same text, plus something new, while the store is still loading.
        let (seed, dir) = store_at("reconcile");
        remember(&seed, "copied twice", "a.exe");
        drop(seed);
        let path = dir.join("clipboard.db");

        let store = loose_store();
        remember_in_memory(&store, "copied twice", "b.exe");
        remember_in_memory(&store, "brand new", "b.exe");
        store.attach(&path);

        // Both are pastable, both have real rowids, and the duplicate did
        // not become two rows.
        let hits = store.match_query("", 10);
        assert_eq!(hits.len(), 2, "the duplicate should have merged");
        for h in &hits {
            assert!(h.id > 0, "entry {} kept a memory-only id", h.preview);
            assert!(store.content(h.id).is_some(), "{} is unpastable", h.preview);
        }
        assert_eq!(store.content(hits[0].id).as_deref(), Some("brand new"));

        // Surviving a restart is the point of having given them rows.
        drop(store);
        let reopened = loose_store();
        reopened.attach(&path);
        assert_eq!(reopened.match_query("", 10).len(), 2);
        let found = reopened.match_query("brand", 10)[0].id;
        assert_eq!(reopened.content(found).as_deref(), Some("brand new"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Clearing is a privacy control: it may not report a success it did not
    /// earn, and it may not be undone by a load that was already in flight.
    #[test]
    fn clearing_is_honest_about_what_it_could_not_reach() {
        let store = loose_store();
        // Still loading: the rows exist but cannot be named yet. Reporting
        // Ok is only allowed because `attach` is about to honour it.
        assert!(store.clear().is_ok());
        assert!(store.cleared_while_loading.load(Ordering::SeqCst));

        // A failed open cannot delete anything, and says so.
        let failed = loose_store();
        *failed.db.lock().unwrap() = Db::Failed;
        assert!(failed.clear().is_err(), "clear claimed a wipe it never did");
        assert!(failed.delete(7).is_err());
        // A memory-only entry has no stored copy, so removing it is complete.
        assert!(failed.delete(-7).is_ok());
    }

    #[test]
    fn a_clear_during_the_load_survives_the_load() {
        let (seed, dir) = store_at("race");
        remember(&seed, "from yesterday", "a.exe");
        drop(seed);
        let path = dir.join("clipboard.db");

        let store = loose_store();
        store.clear().unwrap();
        store.attach(&path);
        assert!(
            store.match_query("", 10).is_empty(),
            "the load put back what the user cleared"
        );

        // And it is gone from disk, not just from the index.
        drop(store);
        let reopened = loose_store();
        reopened.attach(&path);
        assert!(reopened.match_query("", 10).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §7.4 retention has to reach the index too: an entry the view lists
    /// after its row is reaped is one you can select but not paste.
    #[test]
    fn retention_expires_the_index_and_the_table_together() {
        let (store, dir) = store_at("retention");
        remember(&store, "recent", "a.exe");
        remember(&store, "ancient", "a.exe");
        let old = store.match_query("ancient", 10)[0].id;
        let stale = now_ts() - (RETENTION_DAYS + 1) * 86_400;
        {
            let guard = store.db.lock().unwrap();
            let Db::Ready(conn) = &*guard else {
                panic!("open")
            };
            conn.execute("UPDATE entries SET ts = ?2 WHERE id = ?1", [old, stale])
                .unwrap();
        }
        store.entries.lock().unwrap().iter_mut().for_each(|e| {
            if e.id == old {
                e.ts = stale;
            }
        });

        store.prune();
        let left = store.match_query("", 10);
        assert_eq!(left.len(), 1, "the index still lists an expired entry");
        assert_eq!(left[0].preview, "recent");
        assert!(store.content(old).is_none(), "the row outlived retention");
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
