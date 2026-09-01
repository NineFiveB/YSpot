//! ETW marker provider for the §10 M0 latency harness.
//!
//! One registered provider ([`yspot_proto::M0_MARKER_PROVIDER`]), plain-string
//! events. ETW stamps every event with the session clock at write time — the
//! harness starts its session with `ClientContext = 1` (QPC) — so a marker
//! carries no timestamp of its own: writing it *is* the timestamp, taken in
//! the same clock domain as the harness's injected keydowns and the
//! `Microsoft-Windows-Dwm-Core` present events.
//!
//! Always-on by design. `EventWriteString` with no enabled session is a
//! couple of predictable branches, so there is nothing to configure and no
//! way for a measurement build to diverge from the shipped one — the §2.5
//! numbers are taken on the binary users run.

use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::core::GUID;
use windows_sys::Win32::System::Diagnostics::Etw::{EventRegister, EventWriteString};

/// Provider handle from `EventRegister`, 0 while unregistered. Registration
/// failure (diagnostics disabled by policy, say) leaves it 0 and every
/// [`mark`] becomes a cheap no-op — instrumentation must never take the shell
/// down.
static REGHANDLE: AtomicU64 = AtomicU64::new(0);

fn provider_guid() -> GUID {
    let (d1, d2, d3, d4) = yspot_proto::M0_MARKER_PROVIDER;
    GUID {
        data1: d1,
        data2: d2,
        data3: d3,
        data4: d4,
    }
}

/// Register the provider. Called once at startup; idempotent enough for the
/// one caller (a second registration would just orphan the first handle at
/// process exit, which ETW tolerates).
pub fn init() {
    let mut handle: u64 = 0;
    let guid = provider_guid();
    // SAFETY: `guid` outlives the call; no callback is registered, so ETW
    // holds no pointer into us after return.
    let status = unsafe { EventRegister(&guid, None, std::ptr::null(), &mut handle) };
    if status == 0 {
        REGHANDLE.store(handle, Ordering::SeqCst);
        log::debug!("M0 marker provider registered");
    } else {
        log::warn!("M0 marker provider registration failed (status {status}); markers disabled");
    }
}

/// Write one marker. The string is the whole payload; keep it short and
/// space-delimited (`applied gen=12`), the harness splits on the first token.
pub fn mark(text: &str) {
    let handle = REGHANDLE.load(Ordering::Relaxed);
    if handle == 0 {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `handle` came from EventRegister; `wide` is a valid
    // NUL-terminated UTF-16 string for the duration of the call. The cast is
    // windows-sys's own inconsistency: EventRegister's out-param is `u64`,
    // EventWriteString's REGHANDLE is `i64`; both are the same opaque handle.
    unsafe {
        EventWriteString(handle as i64, 0, 0, wide.as_ptr());
    }
}
