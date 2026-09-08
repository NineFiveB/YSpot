//! App icons (SPEC.md §7.1, §5.10): `IShellItemImageFactory::GetImage` on the
//! `AppsFolder` item, encoded to PNG, served to the frontend as a `data:` URI.
//!
//! Extraction never runs on the query path: the frontend asks for an icon
//! when a row mounts (§5.10 "load asynchronously off the critical path via an
//! LRU cache; a missing icon renders a placeholder"), the command runs the
//! extraction on the blocking pool, and the result is cached in memory and on
//! disk at `%LOCALAPPDATA%\YSpot\icons\`.
//!
//! Cache key deviation from §7.1: keyed by AUMID + physical pixel size, with a
//! time-to-live on the disk file, rather than by source mtime — `AppsFolder`
//! items carry no single source file whose mtime is meaningful for packaged
//! apps. An icon changed by an app update is therefore stale for at most
//! [`DISK_TTL`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::Shell::{
    IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK,
    SIIGBF_ICONONLY,
};

use crate::com::{wide, Apartment};

const DISK_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Memory cache bound: ~300 apps × a few KB is well under this.
const MEM_MAX: usize = 2048;
/// Sanity bound on what the shell may hand back.
const MAX_PX: i32 = 512;

pub struct IconCache {
    dir: Option<PathBuf>,
    mem: Mutex<HashMap<String, Arc<str>>>,
}

impl IconCache {
    pub fn new() -> Arc<IconCache> {
        let dir =
            std::env::var_os("LOCALAPPDATA").map(|b| PathBuf::from(b).join("YSpot").join("icons"));
        if let Some(d) = &dir {
            if let Err(e) = std::fs::create_dir_all(d) {
                log::warn!("icon cache dir {}: {e}", d.display());
            }
        }
        Arc::new(IconCache {
            dir,
            mem: Mutex::new(HashMap::new()),
        })
    }

    /// The icon for a shell item, given its **fully-formed parsing name**, as
    /// a PNG `data:` URI at `px` physical pixels square. Blocking: call from
    /// the blocking pool.
    ///
    /// The caller builds the parsing name — see [`crate::row_icons`] — and
    /// this function never concatenates one, because any string reaching
    /// `SHCreateItemFromParsingName` can activate a shell extension.
    pub fn icon(&self, parsing_name: &str, px: i32) -> Result<Arc<str>, String> {
        let px = px.clamp(8, MAX_PX);
        // Keyed by the parsing name rather than the AUMID (SPEC §7.2's note
        // says AUMID; amended, because the cache now holds more than apps and
        // two kinds could otherwise collide on one id).
        let key = format!("{}-{px}", fnv1a(parsing_name.as_bytes()));
        if let Some(hit) = self.mem.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Ok(hit.clone());
        }
        let png = match self.read_disk(&key) {
            Some(bytes) => bytes,
            None => {
                let bytes = extract_png(parsing_name, px)?;
                self.write_disk(&key, &bytes);
                bytes
            }
        };
        let uri: Arc<str> = Arc::from(format!("data:image/png;base64,{}", base64(&png)));
        let mut mem = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        if mem.len() >= MEM_MAX {
            mem.clear(); // crude but bounded; the disk cache backs it
        }
        mem.insert(key, uri.clone());
        Ok(uri)
    }

    fn disk_path(&self, key: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(format!("{key}.png")))
    }

    fn read_disk(&self, key: &str) -> Option<Vec<u8>> {
        let p = self.disk_path(key)?;
        let md = std::fs::metadata(&p).ok()?;
        let fresh = md
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age < DISK_TTL);
        if !fresh {
            return None;
        }
        std::fs::read(&p).ok().filter(|b| !b.is_empty())
    }

    fn write_disk(&self, key: &str, bytes: &[u8]) {
        if let Some(p) = self.disk_path(key) {
            let tmp = p.with_extension("png.tmp");
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

fn fnv1a(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Extract the icon as straight-alpha RGBA and encode it as PNG.
fn extract_png(parsing_name: &str, px: i32) -> Result<Vec<u8>, String> {
    let _sta = Apartment::sta();
    let path = wide(parsing_name);
    // SAFETY: NUL-terminated parsing path; no bind context.
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(path.as_ptr()), None) }
        .map_err(|e| format!("shell item {parsing_name}: {e}"))?;
    let factory: IShellItemImageFactory =
        windows::core::Interface::cast(&item).map_err(|e| format!("image factory: {e}"))?;
    // SAFETY: valid factory; returns an owned HBITMAP freed with DeleteObject.
    let hbm = unsafe {
        factory.GetImage(
            SIZE { cx: px, cy: px },
            SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK,
        )
    }
    .map_err(|e| format!("GetImage for {parsing_name}: {e}"))?;
    let result = bitmap_to_rgba(hbm);
    // SAFETY: the HBITMAP is ours to free, exactly once.
    unsafe {
        let _ = DeleteObject(hbm.into());
    };
    let (w, h, rgba) = result?;
    encode_png(w, h, &rgba)
}

/// Read a 32-bit bitmap as top-down straight-alpha RGBA.
fn bitmap_to_rgba(
    hbm: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Result<(u32, u32, Vec<u8>), String> {
    let mut bm = BITMAP::default();
    // SAFETY: valid bitmap handle; `bm` is the documented out-struct.
    let got = unsafe {
        GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut _),
        )
    };
    if got == 0
        || bm.bmWidth <= 0
        || bm.bmHeight <= 0
        || bm.bmWidth > MAX_PX
        || bm.bmHeight > MAX_PX
    {
        return Err("GetObject on icon bitmap failed".into());
    }
    let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32), // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    // SAFETY: a memory DC for the call; `bgra` is exactly w·h·4 bytes, which
    // is what a 32 bpp request of `h` lines fills.
    let lines = unsafe {
        let hdc = CreateCompatibleDC(None);
        let lines = GetDIBits(
            hdc,
            hbm,
            0,
            h,
            Some(bgra.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(hdc);
        lines
    };
    if lines as u32 != h {
        return Err("GetDIBits copied fewer lines than expected".into());
    }
    // The shell hands back premultiplied BGRA; PNG wants straight RGBA. A
    // fully transparent bitmap (no alpha channel at all) is treated as opaque.
    let pixels = bgra.as_chunks::<4>().0;
    let any_alpha = pixels.iter().any(|p| p[3] != 0);
    let mut rgba = Vec::with_capacity(bgra.len());
    for p in pixels {
        let (b, g, r, a) = (p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32);
        if !any_alpha {
            rgba.extend_from_slice(&[r as u8, g as u8, b as u8, 255]);
        } else if a == 0 {
            rgba.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let un = |c: u32| ((c * 255 + a / 2) / a).min(255) as u8;
            rgba.extend_from_slice(&[un(r), un(g), un(b), a as u8]);
        }
    }
    Ok((w, h, rgba))
}

fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(rgba.len() / 4);
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Standard base64 (RFC 4648) — a few lines beat a dependency for one call site.
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match *chunk {
            [a, b, c] => (a as u32) << 16 | (b as u32) << 8 | c as u32,
            [a, b] => (a as u32) << 16 | (b as u32) << 8,
            [a] => (a as u32) << 16,
            _ => unreachable!(),
        };
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn png_round_trip_is_valid() {
        let rgba = vec![255u8, 0, 0, 255, 0, 255, 0, 128];
        let png = encode_png(2, 1, &rgba).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let dec = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = dec.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 1));
        assert_eq!(&buf[..8], &rgba[..]);
    }

    /// Real extraction against the first app on this machine.
    /// The first four bytes of any PNG. Written as bytes so no escape can be
    /// mangled by whatever wrote this file.
    const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G'];

    /// The two parsing-name shapes this change added, against the real shell.
    ///
    /// Both are load-bearing claims: that the Settings package resolves from
    /// its URI, and that a Control Panel item resolves from the CLSID the
    /// registry gave us. Neither is provable without a desktop session, which
    /// is why this sits beside the app test rather than in `row_icons`.
    #[test]
    fn extracts_icons_for_the_new_windows_sources() {
        let _sta = Apartment::sta();

        let settings = extract_png(crate::row_icons::SETTINGS_HOME, 32)
            .expect("the Settings package has no icon");
        assert_eq!(&settings[..4], PNG_MAGIC);

        let clsid = crate::control_panel::clsid("Microsoft.System")
            .expect("Microsoft.System is not registered on this machine");
        let system = extract_png(&format!("shell:::{clsid}"), 32)
            .expect("the System Control Panel item has no icon");
        assert_eq!(&system[..4], PNG_MAGIC);

        // Different destinations must not hand back the same bitmap. If they
        // did, the CLSID route would be resolving to something generic and
        // every Control Panel row would quietly wear one icon.
        assert_ne!(
            settings, system,
            "the Settings and System icons are byte-identical, so one of \n             these parsing names is not resolving what it claims"
        );
    }

    #[test]
    fn extracts_an_icon_for_a_real_app() {
        let _sta = Apartment::sta();
        let mut apps = Vec::new();
        {
            // Reach into the catalog enumerator to find any AUMID.
            let cat = crate::apps::AppCatalog::new();
            cat.refresh_async();
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                apps = cat.snapshot().as_ref().clone();
                if !apps.is_empty() {
                    break;
                }
            }
        }
        assert!(!apps.is_empty(), "no apps to extract an icon for");
        let name = format!("shell:AppsFolder\\{}", apps[0].aumid);
        let png = extract_png(&name, 48).expect("icon extraction");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
