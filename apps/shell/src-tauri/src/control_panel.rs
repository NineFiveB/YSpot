//! Control Panel canonical name → CLSID, read from the shell's own registry
//! namespace (SPEC.md §7.2).
//!
//! The catalog stores Control Panel items by canonical name — the string
//! `control.exe /name` takes — because that is the launch contract. An icon
//! needs a shell item, and the bridge between the two is the CLSID the
//! Control Panel namespace registers.
//!
//! **The CLSID is looked up at runtime and never baked into the catalog
//! JSON**, and that is a correctness decision rather than a tidiness one. A
//! canonical name this machine does not register is a *known* miss: the
//! lookup returns `None` and the row keeps its glyph. A baked CLSID that is
//! wrong or retired is a *silent* miss, because `shell:::{0000…0000}`
//! **succeeds** and hands back a generic blank-page icon with no error. There
//! is no reliable way to recognise that bitmap afterwards, so registry
//! presence is the only honest gate.
//!
//! Measured on Windows 11 26200: 40 namespace entries, ~10 ms to build.

use std::collections::HashMap;
use std::sync::OnceLock;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CLASSES_ROOT,
    HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ,
};

use crate::com::wide;

/// Where the shell registers the Control Panel's namespace extensions.
const NAMESPACE: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\ControlPanel\NameSpace";

/// A canonical name longer than this is not one; the cap bounds what a
/// hostile or corrupt registry value can put into a parsing name.
const MAX_CANONICAL: usize = 128;

static INDEX: OnceLock<HashMap<String, String>> = OnceLock::new();

/// `Some("{clsid}")` when this machine registers that canonical name.
///
/// Built once on first call. Cheap enough to do lazily on the blocking pool;
/// never call it from the query path.
pub fn clsid(canonical: &str) -> Option<String> {
    INDEX.get_or_init(build).get(canonical).cloned()
}

/// Walk the namespace under both hives and resolve each CLSID's canonical
/// name through `HKEY_CLASSES_ROOT`, which is the merged view, so one read
/// per CLSID covers a per-machine and a per-user registration alike.
fn build() -> HashMap<String, String> {
    let mut out = HashMap::new();
    for root in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        for name in subkeys(root, NAMESPACE) {
            if !is_clsid(&name) {
                continue;
            }
            if let Some(canonical) = canonical_name(&name) {
                out.entry(canonical).or_insert(name);
            }
        }
    }
    log::info!("control panel: {} canonical names registered", out.len());
    out
}

/// A `{8-4-4-4-12}` GUID and nothing else. The subkey name goes into a
/// parsing name, so it is validated by shape before it is ever concatenated.
fn is_clsid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 38 || b[0] != b'{' || b[37] != b'}' {
        return false;
    }
    for (i, c) in b[1..37].iter().enumerate() {
        let dash = matches!(i, 8 | 13 | 18 | 23);
        if dash != (*c == b'-') || (!dash && !c.is_ascii_hexdigit()) {
            return false;
        }
    }
    true
}

fn canonical_name(clsid: &str) -> Option<String> {
    let v = reg_string(
        HKEY_CLASSES_ROOT,
        &format!(r"CLSID\{clsid}"),
        "System.ApplicationName",
    )?;
    (!v.is_empty() && v.len() <= MAX_CANONICAL).then_some(v)
}

fn subkeys(root: HKEY, path: &str) -> Vec<String> {
    let wpath = wide(path);
    let mut key = HKEY::default();
    // SAFETY: NUL-terminated path; the handle is closed below.
    let opened =
        unsafe { RegOpenKeyExW(root, PCWSTR(wpath.as_ptr()), Some(0), KEY_READ, &mut key) };
    if opened.is_err() {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut i = 0u32;
    loop {
        // A CLSID subkey name is 38 chars; 256 is room to spare, and the
        // loop stops at the first non-success rather than trusting a count.
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        // SAFETY: `buf`/`len` are an out-parameter pair; every other
        // argument is optional and passed as None.
        let r = unsafe {
            RegEnumKeyExW(
                key,
                i,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if r.is_err() {
            break;
        }
        names.push(String::from_utf16_lossy(&buf[..len as usize]));
        i += 1;
    }
    // SAFETY: opened above and not used again.
    let _ = unsafe { RegCloseKey(key) };
    names
}

fn reg_string(root: HKEY, path: &str, value: &str) -> Option<String> {
    let wpath = wide(path);
    let wvalue = wide(value);
    let mut size = 0u32;
    // SAFETY: a size query — no buffer is written when the data pointer is
    // None, which is what the size-first form requires.
    let probe = unsafe {
        RegGetValueW(
            root,
            PCWSTR(wpath.as_ptr()),
            PCWSTR(wvalue.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if probe.is_err() || size == 0 {
        return None;
    }
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    let mut got = size;
    // SAFETY: `buf` is sized from the probe above and `got` bounds the write.
    let read = unsafe {
        RegGetValueW(
            root,
            PCWSTR(wpath.as_ptr()),
            PCWSTR(wvalue.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut got),
        )
    };
    if read.is_err() {
        return None;
    }
    let chars = (got as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..chars]);
    Some(s.trim_end_matches('\0').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The subkey name is concatenated into a parsing name, and a parsing
    /// name can activate an in-process shell extension. Shape is checked
    /// before it is ever used, so a registry key with a hostile name cannot
    /// become one.
    #[test]
    fn only_a_well_formed_guid_is_accepted() {
        assert!(is_clsid("{26EE0668-A00A-44D7-9371-BEB064C98683}"));
        assert!(is_clsid("{00000000-0000-0000-0000-000000000000}"));
        // Wrong shape in every position that matters.
        assert!(!is_clsid("26EE0668-A00A-44D7-9371-BEB064C98683"));
        assert!(!is_clsid("{26EE0668-A00A-44D7-9371-BEB064C9868}"));
        assert!(!is_clsid("{26EE0668-A00A-44D7-9371-BEB064C98683"));
        assert!(!is_clsid("{26EE0668+A00A-44D7-9371-BEB064C98683}"));
        assert!(!is_clsid("{26EE0668-A00A-44D7-9371-BEB064C9868Z}"));
        assert!(!is_clsid(""));
        assert!(!is_clsid(r"{..}\..\..\windows\system32"));
    }

    /// Reads the real registry, so it asserts only what must be true on any
    /// Windows: the namespace exists and the well-known System item is in it.
    #[test]
    fn the_control_panel_namespace_resolves_on_this_machine() {
        let index = build();
        assert!(
            index.len() > 10,
            "only {} canonical names; the namespace walk is probably broken",
            index.len()
        );
        let system = index.get("Microsoft.System").expect("Microsoft.System");
        assert!(is_clsid(system), "not a CLSID: {system}");
    }
}
