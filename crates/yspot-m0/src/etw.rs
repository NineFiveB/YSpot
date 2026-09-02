//! ETW session control and real-time consumption for the M0 harness.
//!
//! One session, two providers, one clock. The session is started with
//! `ClientContext = 1`, so every event — the shell's markers and DWM's
//! composition events — is stamped with raw QPC ticks, directly comparable
//! with the [`crate::input::qpc`] reading the harness takes before injecting
//! input. That shared timeline is the entire measurement method.
//!
//! Delivery is not timing. Real-time ETW hands buffers over when they fill or
//! when the flush timer (≥ 1 s) fires, so events can arrive late; their
//! `TimeStamp` fields are exact regardless. The harness forces a flush
//! ([`Session::flush`]) whenever it is about to wait, which turns the worst
//! case from a second into milliseconds without touching the timestamps.
//!
//! Starting a session needs Performance Log Users membership or elevation;
//! [`Session::start`] maps `ERROR_ACCESS_DENIED` to advice saying so.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND,
};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_HEADER_FLAG_STRING_ONLY,
    EVENT_RECORD, EVENT_TRACE_CONTROL_FLUSH, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESS_TRACE_MODE_REAL_TIME, WNODE_FLAG_TRACED_GUID,
};

/// Microsoft-Windows-Dwm-Core, `logman query providers`.
pub const DWM_CORE: GUID = GUID {
    data1: 0x9E9B_BA3C,
    data2: 0x2E38,
    data3: 0x40CB,
    data4: [0x99, 0xF4, 0x9E, 0x82, 0x81, 0x42, 0x51, 0x64],
};

/// Dwm-Core keywords for MEASUREMENT sessions: Composition (0x1) |
/// DetailedFrameInformation (0x2) only. Composition-pass events are the
/// present endpoint; Scheduling (0x80) and DwmFrameRate (0x8) fire
/// continuously while DWM is active, and any of them would satisfy a
/// "first DWM event after X" wait with chatter unrelated to our frame.
pub const DWM_KEYWORDS_MEASURE: u64 = 0x3;

/// Dwm-Core keywords for `etw-dump`: the measurement set plus Scheduling and
/// DwmFrameRate, so a reference machine's event mix can be characterized and
/// its composition-pass IDs pinned for `--dwm-ids`.
pub const DWM_KEYWORDS_DUMP: u64 = 0x8B;

const SESSION_NAME: &str = "YSpot-M0";

pub fn marker_guid() -> GUID {
    let (d1, d2, d3, d4) = yspot_proto::M0_MARKER_PROVIDER;
    GUID {
        data1: d1,
        data2: d2,
        data3: d3,
        data4: d4,
    }
}

/// One consumed event, timestamp in raw QPC ticks.
#[derive(Debug, Clone)]
pub enum Event {
    /// A shell marker string (`shown`, `applied gen=3`, …).
    Marker { qpc: i64, text: String },
    /// Any Microsoft-Windows-Dwm-Core event. `keyword` is the event's own
    /// keyword mask — which enable-keywords it matches — recorded so a dump
    /// can tell which ids survive the measurement session's narrow filter.
    Dwm { qpc: i64, id: u16, keyword: u64 },
}

impl Event {
    pub fn qpc(&self) -> i64 {
        match self {
            Event::Marker { qpc, .. } | Event::Dwm { qpc, .. } => *qpc,
        }
    }
}

/// The consumer callback has no user pointer worth threading through OpenTrace
/// safely across the API's u32/u64 context quirks, so the channel is a global.
/// One harness process runs one session; a second `Session::start` fails at
/// StartTraceW long before this matters.
static SINK: OnceLock<Sender<Event>> = OnceLock::new();

unsafe extern "system" fn on_event(rec: *mut EVENT_RECORD) {
    let Some(sink) = SINK.get() else { return };
    // SAFETY: ETW guarantees `rec` is valid for the duration of the callback.
    let rec = unsafe { &*rec };
    let qpc = rec.EventHeader.TimeStamp;
    let pid = rec.EventHeader.ProviderId;
    let ev = if guid_eq(&pid, &marker_guid()) {
        if rec.EventHeader.Flags & EVENT_HEADER_FLAG_STRING_ONLY as u16 == 0 {
            return;
        }
        // Checked before the slice is built: the provider GUID is unsecured
        // (any process may write under it), so payload shape is input, not
        // invariant — a null, odd-length, or misaligned payload must be
        // dropped, never turned into a `&[u16]`.
        let words = rec.UserDataLength as usize / 2;
        if rec.UserData.is_null() || words == 0 || !(rec.UserData as usize).is_multiple_of(2) {
            return;
        }
        // SAFETY: non-null, 2-aligned, and UserDataLength bytes live for the
        // duration of the callback per the ETW contract.
        let s = unsafe { std::slice::from_raw_parts(rec.UserData as *const u16, words) };
        let text = String::from_utf16_lossy(s)
            .trim_end_matches('\0')
            .to_string();
        Event::Marker { qpc, text }
    } else if guid_eq(&pid, &DWM_CORE) {
        Event::Dwm {
            qpc,
            id: rec.EventHeader.EventDescriptor.Id,
            keyword: rec.EventHeader.EventDescriptor.Keyword,
        }
    } else {
        return;
    };
    let _ = sink.send(ev);
}

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// EVENT_TRACE_PROPERTIES plus the two name buffers ETW writes behind it.
///
/// A real struct rather than a `Vec<u8>` cast, because the reference the
/// control APIs receive obligates the struct's own (8-byte) alignment and a
/// byte vector promises none — that cast is formally UB that only works while
/// the allocator happens to over-align.
#[repr(C)]
struct PropsBuf {
    props: EVENT_TRACE_PROPERTIES,
    /// Logger-name buffer. StartTraceW writes the instance name here; a
    /// ControlTraceW query writes both names back into the trailing space.
    logger_name: [u16; 256],
    /// Log-file-name buffer. Never named by an offset on input — real-time
    /// only — but control operations are documented to write into it.
    file_name: [u16; 256],
}

fn alloc_props() -> Box<PropsBuf> {
    // SAFETY: every field of PropsBuf is valid all-zeroes.
    let mut b: Box<PropsBuf> = unsafe { Box::new(std::mem::zeroed()) };
    b.props.Wnode.BufferSize = std::mem::size_of::<PropsBuf>() as u32;
    b.props.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    b.props.Wnode.ClientContext = 1; // QPC timestamps — the load-bearing line.
    b.props.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
    b.props.BufferSize = 64; // KB
    b.props.MinimumBuffers = 4;
    b.props.MaximumBuffers = 16;
    b.props.FlushTimer = 1; // seconds; floor of the API. Session::flush beats it.
    b.props.LoggerNameOffset = std::mem::offset_of!(PropsBuf, logger_name) as u32;
    // LogFileNameOffset stays 0: this session never logs to a file, and the
    // documented contract for real-time-only is offset 0 — a nonzero offset
    // declares a file name that would here be empty.
    b
}

fn props_ptr(b: &mut PropsBuf) -> *mut EVENT_TRACE_PROPERTIES {
    &mut b.props
}

pub struct Session {
    handle: CONTROLTRACE_HANDLE,
    consumer: Option<std::thread::JoinHandle<()>>,
    pub rx: Receiver<Event>,
}

impl Session {
    /// Stop any stale session of our name, start fresh, enable the marker
    /// provider and Dwm-Core with `dwm_keywords`, spawn the consumer thread.
    ///
    /// Measurement subcommands pass [`DWM_KEYWORDS_MEASURE`]; only `etw-dump`
    /// passes the wide [`DWM_KEYWORDS_DUMP`], because Scheduling/FrameRate
    /// events fire continuously while DWM is active and would satisfy a
    /// "first DWM event after X" wait with chatter unrelated to our frame.
    pub fn start(dwm_keywords: u64) -> Result<Session> {
        let name = wide(SESSION_NAME);

        // A crashed prior run leaves the session running (kernel object, not
        // process-owned). Stop-by-name; instance-not-found is the clean case.
        let mut props = alloc_props();
        // SAFETY: props points at a live, correctly-aligned
        // EVENT_TRACE_PROPERTIES with the name buffers behind it; name is NUL
        // terminated.
        let rc = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                name.as_ptr(),
                props_ptr(&mut props),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        if rc != ERROR_SUCCESS && rc != ERROR_WMI_INSTANCE_NOT_FOUND {
            log::debug!("stale-session stop returned {rc}");
        }

        let mut props = alloc_props();
        let mut handle = CONTROLTRACE_HANDLE { Value: 0 };
        // SAFETY: as above; handle receives the session on success.
        let rc = unsafe { StartTraceW(&mut handle, name.as_ptr(), props_ptr(&mut props)) };
        if rc == ERROR_ACCESS_DENIED {
            bail!(
                "StartTrace denied (os error 5). Consuming real-time ETW needs an elevated \
                 prompt or Performance Log Users membership — rerun this harness elevated."
            );
        }
        if rc != ERROR_SUCCESS {
            bail!("StartTraceW failed: {rc}");
        }
        // From here on, every failure path must stop the session it started.
        let stop = |handle: CONTROLTRACE_HANDLE| {
            let mut props = alloc_props();
            // SAFETY: valid properties buffer; stop by handle.
            unsafe {
                ControlTraceW(
                    handle,
                    std::ptr::null(),
                    props_ptr(&mut props),
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
        };

        for (guid, keywords) in [(marker_guid(), 0u64), (DWM_CORE, dwm_keywords)] {
            // SAFETY: guid lives across the call; no enable parameters.
            let rc = unsafe {
                EnableTraceEx2(
                    handle,
                    &guid,
                    EVENT_CONTROL_CODE_ENABLE_PROVIDER,
                    5, // TRACE_LEVEL_VERBOSE
                    keywords,
                    0,
                    0,
                    std::ptr::null(),
                )
            };
            if rc != ERROR_SUCCESS {
                stop(handle);
                bail!("EnableTraceEx2 failed: {rc}");
            }
        }

        // SAFETY: zeroed EVENT_TRACE_LOGFILEW is the documented starting
        // state; only the fields set below are read for a real-time consumer.
        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
        let logger_name = wide(SESSION_NAME);
        logfile.LoggerName = logger_name.as_ptr() as *mut u16;
        // RAW_TIMESTAMP is what makes ClientContext=1 reach us: without it,
        // ProcessTrace converts every EVENT_RECORD timestamp back to FILETIME
        // before the callback, and comparing those against the harness's raw
        // QPC readings would be cross-epoch garbage — every wait predicate
        // vacuously true, every latency ~1e13 ms. The whole one-clock
        // measurement model hangs on this flag.
        logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME
            | PROCESS_TRACE_MODE_EVENT_RECORD
            | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
        logfile.Anonymous2.EventRecordCallback = Some(on_event);

        // SAFETY: logfile and the name it points at outlive the call (the
        // name is copied during OpenTraceW).
        let trace = unsafe { OpenTraceW(&mut logfile) };
        if trace.Value == u64::MAX {
            let err = std::io::Error::last_os_error();
            stop(handle);
            return Err(anyhow::Error::from(err).context("OpenTraceW"));
        }

        // The sink is set only once everything fallible before the consumer
        // succeeded — a OnceLock cannot be reset, so setting it earlier would
        // let one failed start poison every retry in the process. Set-then-
        // spawn, so no event can be delivered before the sink exists.
        let (tx, rx) = std::sync::mpsc::channel();
        if SINK.set(tx).is_err() {
            // SAFETY: close the trace we just opened before stopping.
            unsafe { CloseTrace(trace) };
            stop(handle);
            bail!("one ETW session per process");
        }

        let consumer = std::thread::Builder::new()
            .name("etw-consume".into())
            .spawn(move || {
                // SAFETY: `trace` is the open trace handle; ProcessTrace blocks
                // until the session stops, then the handle is closed here.
                unsafe {
                    ProcessTrace(&trace, 1, std::ptr::null(), std::ptr::null());
                    CloseTrace(trace);
                }
            })
            .context("spawn etw consumer")?;

        Ok(Session {
            handle,
            consumer: Some(consumer),
            rx,
        })
    }

    /// Force buffered events out now instead of waiting on the ≥ 1 s flush
    /// timer.
    pub fn flush(&self) {
        let mut props = alloc_props();
        // SAFETY: valid properties buffer; flush by handle.
        unsafe {
            ControlTraceW(
                self.handle,
                std::ptr::null(),
                props_ptr(&mut props),
                EVENT_TRACE_CONTROL_FLUSH,
            )
        };
    }

    /// Drain everything already delivered, discarding it. Used between
    /// measurement steps so stale events cannot satisfy the next wait.
    pub fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }

    /// Wait up to `timeout` for an event matching `pred`, discarding
    /// non-matches. Returns the matching event.
    ///
    /// Flushes once on entry and then only when the channel runs DRY — never
    /// per received event. The awaited marker is usually written milliseconds
    /// after the wait starts, so without periodic flushes it would sit in a
    /// partial buffer until the ≥ 1 s flush timer; but flushing on every
    /// received event turns DWM chatter into a flush storm at event-arrival
    /// rate, churning the very session buffers on the machine being measured.
    /// Dry-channel flushing is bounded at ~20/s and only while quiet.
    pub fn wait_for(
        &self,
        timeout: std::time::Duration,
        mut pred: impl FnMut(&Event) -> bool,
    ) -> Option<Event> {
        let deadline = std::time::Instant::now() + timeout;
        self.flush();
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            match self
                .rx
                .recv_timeout(left.min(std::time::Duration::from_millis(50)))
            {
                Ok(ev) if pred(&ev) => return Some(ev),
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => self.flush(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Collect every event delivered within `window`, flushing on entry and
    /// on dry spells, without judging them. The caller sorts by QPC and picks
    /// endpoints — real-time ETW merges per-CPU buffers and does NOT promise
    /// cross-buffer timestamp order, so sequential consume-and-discard waits
    /// can eat an out-of-order event that a later wait needed.
    pub fn collect(&self, window: std::time::Duration) -> Vec<Event> {
        let deadline = std::time::Instant::now() + window;
        let mut out = Vec::new();
        self.flush();
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return out;
            }
            match self
                .rx
                .recv_timeout(left.min(std::time::Duration::from_millis(50)))
            {
                Ok(ev) => out.push(ev),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => self.flush(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return out,
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let mut props = alloc_props();
        // SAFETY: valid properties buffer; stopping the session unblocks
        // ProcessTrace so the consumer thread can be joined.
        unsafe {
            ControlTraceW(
                self.handle,
                std::ptr::null(),
                props_ptr(&mut props),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        if let Some(t) = self.consumer.take() {
            let _ = t.join();
        }
    }
}
