//! App catalog (SPEC.md §7.1): `AppsFolder` enumeration, name matching, and
//! launching.
//!
//! The `AppsFolder` virtual folder is the one namespace that lists Win32 apps
//! (Start Menu shortcuts) and packaged apps uniformly, each with an
//! AppUserModelID. The catalog is a snapshot of that folder — names and
//! AUMIDs only, no COM objects retained — rebuilt on a worker thread and
//! swapped in atomically, so a keystroke never waits on enumeration.
//!
//! Matching is a small tiered scorer on the same scale as the file index
//! (§3.4: exact 1.0, prefix 0.9, word start 0.8, initials 0.7, substring
//! 0.55, subsequence 0.3–0.5), so the frontend can interleave apps and files
//! by one global score (§5.11, §7.3).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_LOCAL_SERVER};
use windows::Win32::UI::Shell::ILFree;
use windows::Win32::UI::Shell::{
    ApplicationActivationManager, BHID_EnumItems, FOLDERID_AppsFolder,
    IApplicationActivationManager, IEnumShellItems, IShellItem, IShellItem2,
    SHCreateItemFromParsingName, SHGetIDListFromObject, SHGetKnownFolderItem, ShellExecuteExW,
    ShellExecuteW, AO_NONE, KF_FLAG_DEFAULT, SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST,
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SIGDN_NORMALDISPLAY,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::com::{take_cotaskmem_string, wide, Apartment};

/// `System.AppUserModel.ID` — the property key family `{9F4C2855-…}`.
const PKEY_APPUSERMODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
    pid: 5,
};
/// `System.AppUserModel.PackageFullName`: set only on packaged (MSIX/UWP)
/// apps, which is how the two launch paths are told apart.
const PKEY_APPUSERMODEL_PACKAGE_FULL_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: windows::core::GUID::from_u128(0x9F4C2855_9F79_4B39_A8D0_E1D42DE1D5F3),
    pid: 21,
};

/// §7.1: re-enumerate on popup show if the catalog is older than this.
pub const STALE_AFTER: Duration = Duration::from_secs(5 * 60);
/// §7.1: and on this period in the background regardless.
pub const REFRESH_EVERY: Duration = Duration::from_secs(30 * 60);
/// Most apps a root query shows; the file page is capped separately (§7.3).
pub const MAX_APP_RESULTS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AppKind {
    Packaged,
    Win32,
}

#[derive(Clone, Debug)]
pub struct AppEntry {
    pub aumid: String,
    pub name: String,
    /// Case-folded name, one folded char per original char (see [`fold`]),
    /// so a match position maps straight back to the display name.
    folded: Vec<char>,
    /// UTF-16 offset of each original char, plus the total, for §5.13 ranges.
    utf16_offsets: Vec<u32>,
    /// Char indexes where a segment starts (after a separator or at a camel
    /// transition), for the word-start and initials tiers.
    segment_starts: Vec<u32>,
    pub kind: AppKind,
}

impl AppEntry {
    pub fn new(aumid: String, name: String, kind: AppKind) -> AppEntry {
        let folded: Vec<char> = name.chars().map(fold_char).collect();
        let mut utf16_offsets = Vec::with_capacity(folded.len() + 1);
        let mut off = 0u32;
        for c in name.chars() {
            utf16_offsets.push(off);
            off += c.len_utf16() as u32;
        }
        utf16_offsets.push(off);
        let segment_starts = segment_starts(&name);
        AppEntry {
            aumid,
            name,
            folded,
            utf16_offsets,
            segment_starts,
            kind,
        }
    }
}

/// One folded char per input char: lowercase, keeping only the first char of
/// a multi-char expansion (`İ` → `i`), so positions stay aligned with the
/// display name. The file index folds with full NFC + lowercase; app names
/// are short and the difference is invisible for matching purposes.
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_sep(c: char) -> bool {
    matches!(
        c,
        ' ' | '-' | '_' | '.' | '/' | '\\' | '(' | ')' | '&' | '+'
    )
}

/// Segment starts by the same rule the index uses for initials: after a
/// separator, and at a lower→upper camel transition.
fn segment_starts(name: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut prev: Option<char> = None;
    for (i, c) in name.chars().enumerate() {
        let start = match prev {
            None => !is_sep(c),
            Some(p) => (is_sep(p) && !is_sep(c)) || (p.is_lowercase() && c.is_uppercase()),
        };
        if start {
            out.push(i as u32);
        }
        prev = Some(c);
    }
    out
}

/// UTF-16 code-unit ranges into a display name (§5.13).
type Ranges = Vec<(u32, u32)>;

/// A scored app hit, in the shape the frontend row needs.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMatch {
    pub id: String,
    pub name: String,
    pub kind: AppKind,
    pub score: f32,
    /// UTF-16 code-unit ranges into `name` (§5.13).
    pub match_ranges: Ranges,
}

/// Tier bases, identical to the index's (§3.4).
const EXACT: f32 = 1.0;
const PREFIX: f32 = 0.9;
const WORD_START: f32 = 0.8;
const INITIALS: f32 = 0.7;
const SUBSTRING: f32 = 0.55;

/// Score one entry against a folded query; `None` when it does not match.
fn score_entry(e: &AppEntry, q: &[char]) -> Option<(f32, Ranges)> {
    if q.is_empty() || e.folded.is_empty() {
        return None;
    }
    let range = |start: usize, len: usize| -> (u32, u32) {
        (e.utf16_offsets[start], e.utf16_offsets[start + len])
    };
    let n = e.folded.len();
    // Contiguous occurrences, first one per tier is enough.
    let mut first_sub: Option<usize> = None;
    let mut first_word: Option<usize> = None;
    if q.len() <= n {
        for start in 0..=n - q.len() {
            if e.folded[start..start + q.len()] == *q {
                if first_sub.is_none() {
                    first_sub = Some(start);
                }
                if e.segment_starts.contains(&(start as u32)) {
                    first_word = Some(start);
                    break;
                }
            }
        }
    }
    if first_sub == Some(0) {
        let base = if q.len() == n { EXACT } else { PREFIX };
        return Some((base, vec![range(0, q.len())]));
    }
    if let Some(s) = first_word {
        return Some((WORD_START, vec![range(s, q.len())]));
    }
    // Initials: the query is a prefix of the segment-initial sequence.
    if q.len() >= 2 && q.len() <= e.segment_starts.len() {
        let ok = e
            .segment_starts
            .iter()
            .zip(q)
            .all(|(&s, &qc)| e.folded[s as usize] == qc);
        if ok {
            let ranges = e.segment_starts[..q.len()]
                .iter()
                .map(|&s| range(s as usize, 1))
                .collect();
            return Some((INITIALS, ranges));
        }
    }
    if let Some(s) = first_sub {
        return Some((SUBSTRING, vec![range(s, q.len())]));
    }
    // Subsequence (fuzzy), 3+ chars like the index: density = qlen / span.
    if q.len() >= 3 {
        let mut positions = Vec::with_capacity(q.len());
        let mut qi = 0;
        for (i, &c) in e.folded.iter().enumerate() {
            if qi < q.len() && c == q[qi] {
                positions.push(i);
                qi += 1;
            }
        }
        if qi == q.len() {
            let span = positions[q.len() - 1] - positions[0] + 1;
            let density = q.len() as f32 / span as f32;
            let ranges = positions.iter().map(|&p| range(p, 1)).collect();
            return Some((0.3 + 0.2 * density, ranges));
        }
    }
    None
}

/// Match `query` against the catalog: best `max` entries by score, ties by
/// name. Pure and allocation-light — it runs inside the `search` command on
/// every keystroke (§5.11: catalog sources answer in ~1 ms).
pub fn match_query(entries: &[AppEntry], query: &str, max: usize) -> Vec<AppMatch> {
    let q: Vec<char> = query.trim().chars().map(fold_char).collect();
    if q.is_empty() || max == 0 {
        return Vec::new();
    }
    let mut hits: Vec<(f32, &AppEntry, Ranges)> = entries
        .iter()
        .filter_map(|e| score_entry(e, &q).map(|(s, r)| (s, e, r)))
        .collect();
    hits.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.name.len().cmp(&b.1.name.len()))
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    hits.truncate(max);
    hits.into_iter()
        .map(|(score, e, match_ranges)| AppMatch {
            id: e.aumid.clone(),
            name: e.name.clone(),
            kind: e.kind,
            score,
            match_ranges,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Catalog lifecycle.

pub struct AppCatalog {
    entries: RwLock<Arc<Vec<AppEntry>>>,
    enumerated_at: Mutex<Option<Instant>>,
    refreshing: AtomicBool,
}

impl AppCatalog {
    pub fn new() -> Arc<AppCatalog> {
        Arc::new(AppCatalog {
            entries: RwLock::new(Arc::new(Vec::new())),
            enumerated_at: Mutex::new(None),
            refreshing: AtomicBool::new(false),
        })
    }

    pub fn snapshot(&self) -> Arc<Vec<AppEntry>> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_stale(&self) -> bool {
        match *self.enumerated_at.lock().unwrap_or_else(|e| e.into_inner()) {
            None => true,
            Some(t) => t.elapsed() > STALE_AFTER,
        }
    }

    /// Re-enumerate on a worker thread and swap the snapshot in; a no-op
    /// while a refresh is already running.
    pub fn refresh_async(self: &Arc<Self>) {
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("apps-enum".into())
            .spawn(move || {
                let t0 = Instant::now();
                let result = {
                    let _sta = Apartment::sta();
                    enumerate()
                };
                match result {
                    Ok(list) => {
                        log::info!(
                            "AppsFolder: {} apps enumerated in {} ms",
                            list.len(),
                            t0.elapsed().as_millis()
                        );
                        *me.entries.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(list);
                        *me.enumerated_at.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(Instant::now());
                    }
                    Err(e) => log::error!("AppsFolder enumeration failed: {e}"),
                }
                me.refreshing.store(false, Ordering::SeqCst);
            });
        if let Err(e) = spawned {
            log::error!("apps-enum thread spawn failed: {e}");
            self.refreshing.store(false, Ordering::SeqCst);
        }
    }

    /// §7.1: a background refresh every [`REFRESH_EVERY`].
    pub fn spawn_periodic(self: &Arc<Self>) {
        let me = self.clone();
        let _ = std::thread::Builder::new()
            .name("apps-periodic".into())
            .spawn(move || loop {
                std::thread::sleep(REFRESH_EVERY);
                me.refresh_async();
            });
    }
}

/// Walk `AppsFolder` on the calling thread (which must hold an apartment).
fn enumerate() -> windows::core::Result<Vec<AppEntry>> {
    let mut out = Vec::new();
    // SAFETY: FOLDERID_AppsFolder is a static GUID; default flags; no token.
    let folder: IShellItem =
        unsafe { SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)? };
    // SAFETY: valid item; BHID_EnumItems is the documented enumerator handler.
    let items: IEnumShellItems = unsafe { folder.BindToHandler(None, &BHID_EnumItems)? };
    loop {
        let mut batch: [Option<IShellItem>; 16] = Default::default();
        let mut fetched = 0u32;
        // SAFETY: `batch` is a valid out-array; `fetched` a valid out-slot.
        // S_FALSE (fewer than requested) is a success HRESULT.
        unsafe { items.Next(&mut batch, Some(&mut fetched))? };
        if fetched == 0 {
            break;
        }
        for item in batch.iter().take(fetched as usize).flatten() {
            match describe(item) {
                Ok(Some(entry)) => out.push(entry),
                Ok(None) => {}
                Err(e) => log::debug!("AppsFolder item skipped: {e}"),
            }
        }
    }
    Ok(out)
}

/// Name, AUMID and kind of one `AppsFolder` item; `None` for an item without
/// an AUMID (nothing could launch it).
fn describe(item: &IShellItem) -> windows::core::Result<Option<AppEntry>> {
    // SAFETY: valid item; the returned string is CoTaskMem-allocated and
    // freed exactly once by `take_cotaskmem_string`.
    let name = unsafe { take_cotaskmem_string(item.GetDisplayName(SIGDN_NORMALDISPLAY)?) };
    let item2: IShellItem2 = item.cast()?;
    // SAFETY: valid item; property key is a static; string ownership as above.
    let aumid = match unsafe { item2.GetString(&PKEY_APPUSERMODEL_ID) } {
        Ok(p) => unsafe { take_cotaskmem_string(p) },
        Err(_) => return Ok(None),
    };
    if aumid.is_empty() || name.is_empty() {
        return Ok(None);
    }
    // SAFETY: as above; absence of the property is the Win32 case.
    let packaged = match unsafe { item2.GetString(&PKEY_APPUSERMODEL_PACKAGE_FULL_NAME) } {
        Ok(p) => !unsafe { take_cotaskmem_string(p) }.is_empty(),
        Err(_) => false,
    };
    let kind = if packaged {
        AppKind::Packaged
    } else {
        AppKind::Win32
    };
    Ok(Some(AppEntry::new(aumid, name, kind)))
}

// ---------------------------------------------------------------------------
// Launching.

/// Launch an app by AUMID (§7.1). `admin` selects the `runas` verb, which is
/// offered for Win32 apps only.
pub fn launch(aumid: &str, kind: AppKind, admin: bool) -> Result<(), String> {
    let _sta = Apartment::sta();
    match (kind, admin) {
        (AppKind::Packaged, true) => {
            Err("run as administrator is not available for packaged apps".into())
        }
        (AppKind::Packaged, false) => {
            match activate_packaged(aumid) {
                Ok(pid) => {
                    log::info!("activated packaged app {aumid} (pid {pid})");
                    Ok(())
                }
                Err(e) => {
                    // §7.1 fallback: ShellExecute on the AppsFolder parsing path.
                    log::warn!("ActivateApplication failed for {aumid} ({e}); falling back to ShellExecute");
                    shell_execute_apps_folder(aumid)
                }
            }
        }
        (AppKind::Win32, admin) => execute_item(aumid, admin),
    }
}

fn activate_packaged(aumid: &str) -> windows::core::Result<u32> {
    // SAFETY: documented CLSID/IID pair; out-of-proc activation manager.
    let mgr: IApplicationActivationManager =
        unsafe { CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER)? };
    let w = wide(aumid);
    // SAFETY: NUL-terminated AUMID; empty arguments; no options.
    unsafe { mgr.ActivateApplication(PCWSTR(w.as_ptr()), PCWSTR::null(), AO_NONE) }
}

fn apps_folder_path(aumid: &str) -> String {
    format!("shell:AppsFolder\\{aumid}")
}

/// `ShellExecuteEx` on the `AppsFolder` item itself: invokes the Start Menu
/// shortcut with its own arguments and working directory, and honors the
/// `runas` verb for elevation.
fn execute_item(aumid: &str, admin: bool) -> Result<(), String> {
    let path = wide(&apps_folder_path(aumid));
    // SAFETY: NUL-terminated parsing path; no bind context.
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None) }
        .map_err(|e| format!("AppsFolder item {aumid} not found: {e}"))?;
    // SAFETY: valid item; the returned PIDL is CoTaskMem-allocated and freed
    // with ILFree below.
    let pidl =
        unsafe { SHGetIDListFromObject(&item) }.map_err(|e| format!("pidl for {aumid}: {e}"))?;
    let verb = wide("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpIDList: pidl as *mut _,
        lpVerb: if admin {
            PCWSTR(verb.as_ptr())
        } else {
            PCWSTR::null()
        },
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    // SAFETY: `info` is fully initialized for the fields its mask names;
    // `pidl` and `verb` outlive the call.
    let result = unsafe { ShellExecuteExW(&mut info) };
    // SAFETY: `pidl` came from SHGetIDListFromObject; freed exactly once.
    unsafe { ILFree(Some(pidl)) };
    result.map_err(|e| format!("ShellExecuteEx for {aumid} failed: {e}"))
}

fn shell_execute_apps_folder(aumid: &str) -> Result<(), String> {
    let path = wide(&apps_folder_path(aumid));
    // SAFETY: NUL-terminated path; null verb (default), parameters, directory.
    let inst = unsafe {
        ShellExecuteW(
            None,
            PCWSTR::null(),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if inst.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!(
            "ShellExecute shell:AppsFolder\\{aumid} failed (code {})",
            inst.0 as isize
        ))
    }
}

// Keep the unused-import lint honest for the PWSTR type used in signatures.
#[allow(dead_code)]
fn _pwstr_type(_: PWSTR) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> AppEntry {
        AppEntry::new(format!("aumid:{name}"), name.to_string(), AppKind::Win32)
    }

    fn best(name: &str, q: &str) -> Option<(f32, Ranges)> {
        let e = entry(name);
        let qc: Vec<char> = q.chars().map(fold_char).collect();
        score_entry(&e, &qc)
    }

    #[test]
    fn tiers_in_order() {
        assert_eq!(best("Notepad", "notepad").unwrap().0, EXACT);
        assert_eq!(best("Notepad", "note").unwrap().0, PREFIX);
        assert_eq!(best("Visual Studio Code", "code").unwrap().0, WORD_START);
        assert_eq!(best("Visual Studio Code", "vsc").unwrap().0, INITIALS);
        assert_eq!(best("Notepad", "tep").unwrap().0, SUBSTRING);
        let (s, r) = best("Visual Studio Code", "vslc").unwrap();
        assert!(s > 0.3 && s < 0.5, "fuzzy {s}");
        assert_eq!(r.len(), 4);
        assert!(best("Notepad", "xyz").is_none());
        assert!(best("Notepad", "").is_none());
    }

    #[test]
    fn ranges_are_utf16_and_track_the_display_name() {
        let (_, r) = best("Visual Studio Code", "code").unwrap();
        assert_eq!(r, vec![(14, 18)]);
        let (_, r) = best("Visual Studio Code", "vsc").unwrap();
        assert_eq!(r, vec![(0, 1), (7, 8), (14, 15)]);
        // A supplementary-plane char is two UTF-16 units.
        let (s, r) = best("𝄞 Music", "music").unwrap();
        assert_eq!(s, WORD_START);
        assert_eq!(r, vec![(3, 8)]);
        // Camel transitions start segments; case folds.
        assert_eq!(best("PowerToys", "toys").unwrap().0, WORD_START);
        assert_eq!(best("PowerToys", "PT").unwrap().0, INITIALS);
    }

    #[test]
    fn match_query_ranks_and_caps() {
        let entries = vec![
            entry("Code"),
            entry("Visual Studio Code"),
            entry("Codecs Pack"),
            entry("Decoder"),
            entry("Unrelated"),
        ];
        let hits = match_query(&entries, "code", 3);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].name, "Code"); // exact
        assert_eq!(hits[1].name, "Codecs Pack"); // prefix
        assert_eq!(hits[2].name, "Visual Studio Code"); // word start
        assert!(hits[0].score > hits[1].score && hits[1].score > hits[2].score);
        assert!(match_query(&entries, "   ", 3).is_empty());
    }

    /// The real AppsFolder on this machine: enumeration must produce entries
    /// with AUMIDs. Runs on CI too (windows-latest has a Start Menu).
    #[test]
    fn apps_folder_enumerates() {
        let _sta = Apartment::sta();
        let list = enumerate().expect("AppsFolder enumeration");
        assert!(!list.is_empty(), "no apps in AppsFolder");
        assert!(list
            .iter()
            .all(|e| !e.aumid.is_empty() && !e.name.is_empty()));
        let packaged = list.iter().filter(|e| e.kind == AppKind::Packaged).count();
        log::info!("{} apps, {} packaged", list.len(), packaged);
    }
}
