//! Named-pipe client to `yspot-indexd` (SPEC.md §4.1, relay per §4.6).
//!
//! ONE background thread owns the handle outright: connect → Hello/HelloAck →
//! a pump loop that alternates draining queued commands (writes) with
//! `PeekNamedPipe`-guarded reads, with a 1 s → 10 s capped reconnect backoff.
//! Commands from Tauri's thread pool only ever ENQUEUE.
//!
//! Why single-threaded ownership is load-bearing, not style: the pipe is
//! opened synchronously, and Windows serializes synchronous I/O per FILE
//! OBJECT — which `try_clone` duplicates a handle to rather than escaping. The
//! previous shape (dedicated read thread blocked in `ReadFile`, writes from
//! command threads) deadlocked on the FIRST `SearchQuery`: the write waited
//! behind the outstanding read, the read waited for a reply the service could
//! not send because the request never left this process. The exact
//! client-side twin of the service bug fixed in dd228ee — issue #9's full
//! remedy (`FILE_FLAG_OVERLAPPED`) lands with the M1 service wrapper; until
//! then the peek-before-read poll below costs at most [`POLL_IDLE_CAP`] of
//! added arrival latency against the §2.5 20 ms budget.
//!
//! TODO(M1, §4.1 client hardening): verify the server before first write —
//! `GetNamedPipeServerProcessId` + pipe owner SID == S-1-5-18 via
//! `GetSecurityInfo`.

use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_BUSY, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::{PeekNamedPipe, WaitNamedPipeW};
use yspot_proto::{Filters, Message, ResultItem, MAX_FRAME_S2C, PIPE_NAME, PROTO_VERSION};

/// Pump-loop sleep while traffic is flowing.
const POLL_ACTIVE: Duration = Duration::from_millis(1);
/// Pump-loop sleep cap when idle. Bounds the worst-case delay between a frame
/// arriving at the pipe and this client reading it — i.e. the measurement and
/// UX cost of polling instead of overlapped I/O. Kept at 2 ms so it stays
/// noise against the §2.5 budget; the CPU cost of ≤500 wakeups/s is nil.
const POLL_IDLE_CAP: Duration = Duration::from_millis(2);

const MAX_RESULTS: u32 = 50;
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_CAP: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// JSON-friendly result shapes (§4.6 `search:results` event).
// `frn` is a string: JS numbers lose precision above 2^53.

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JsResultId {
    pub volume_idx: u32,
    pub frn: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JsResultItem {
    pub id: JsResultId,
    pub path: String,
    pub name: String,
    pub score: f32,
    pub match_ranges: Vec<(u32, u32)>,
}

impl From<ResultItem> for JsResultItem {
    fn from(item: ResultItem) -> Self {
        JsResultItem {
            id: JsResultId {
                volume_idx: item.id.volume_idx,
                frn: item.id.frn.to_string(),
            },
            path: item.path,
            name: item.name,
            score: item.score,
            match_ranges: item.match_ranges,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct JsSearchResults {
    pub gen: u64,
    pub seq: u32,
    pub is_final: bool,
    pub items: Vec<JsResultItem>,
}

#[derive(Serialize, Clone, Debug)]
struct ConnState {
    connected: bool,
}

// ---------------------------------------------------------------------------

pub struct PipeClient {
    /// Command queue into the pump thread — the only path to the pipe.
    cmd_tx: Sender<Message>,
    /// The pump thread takes this once at spawn.
    cmd_rx: Mutex<Option<Receiver<Message>>>,
    req_id: AtomicU64,
    current_gen: AtomicU64,
    connected: AtomicBool,
}

impl PipeClient {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        PipeClient {
            cmd_tx,
            cmd_rx: Mutex::new(Some(cmd_rx)),
            req_id: AtomicU64::new(1),
            current_gen: AtomicU64::new(0),
            connected: AtomicBool::new(false),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn next_id(&self) -> u64 {
        self.req_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Enqueue for the pump thread. The connected check keeps the old error
    /// semantics for callers; a message racing a disconnect merely sits in the
    /// queue, and the pump drains stale commands before each new handshake.
    fn send(&self, msg: Message) -> Result<(), String> {
        if !self.is_connected() {
            return Err("index service not connected".to_string());
        }
        self.cmd_tx
            .send(msg)
            .map_err(|_| "index service pump thread gone".to_string())
    }

    /// §4.6 `search`: filename query, empty scopes, default filters, 50 rows.
    pub fn search(&self, gen: u64, text: String) -> Result<(), String> {
        self.current_gen.store(gen, Ordering::SeqCst);
        self.send(Message::SearchQuery {
            id: self.next_id(),
            gen,
            text,
            scopes: Vec::new(),
            filters: Filters::default(),
            max_results: MAX_RESULTS,
        })
    }

    /// §4.3 `Cancel` for the current generation ("stop entirely" — window hidden).
    pub fn cancel_current(&self) -> Result<(), String> {
        let gen = self.current_gen.load(Ordering::SeqCst);
        self.send(Message::Cancel { gen })
    }

    /// §4.6 `getIndexStatus` relay: reply arrives as an `index:status` event.
    pub fn request_status(&self) -> Result<(), String> {
        self.send(Message::IndexStatusReq { id: self.next_id() })
    }
}

/// Spawn the connect/read background thread.
pub fn spawn(app: AppHandle, client: Arc<PipeClient>) {
    std::thread::Builder::new()
        .name("yspot-pipe".into())
        .spawn(move || run_loop(app, client))
        .expect("failed to spawn pipe client thread");
}

fn run_loop(app: AppHandle, client: Arc<PipeClient>) {
    // The pump owns the command receiver for the life of the process.
    let cmd_rx = client
        .cmd_rx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("pipe pump spawned twice");
    let mut backoff = BACKOFF_START;
    loop {
        match connect_and_handshake() {
            Ok(file) => {
                backoff = BACKOFF_START;
                // Commands queued while disconnected are stale by definition
                // (their generations were superseded, their callers told the
                // service was down); a new conversation starts empty.
                while cmd_rx.try_recv().is_ok() {}
                client.connected.store(true, Ordering::SeqCst);
                emit_conn_state(&app, true);

                pump_loop(&app, &client, &cmd_rx, file);

                client.connected.store(false, Ordering::SeqCst);
                emit_conn_state(&app, false);
            }
            Err(e) => {
                log::debug!("indexd pipe connect failed: {e}");
            }
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_CAP);
    }
}

/// Bytes currently readable on the pipe without blocking, or an error when the
/// pipe has died (broken/closed — the reconnect signal).
fn readable_bytes(file: &File) -> io::Result<u32> {
    let mut avail: u32 = 0;
    // SAFETY: valid pipe handle for the duration of the call; only
    // TotalBytesAvail is requested, all buffer pointers are documented-null.
    let ok = unsafe {
        PeekNamedPipe(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut avail,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(avail)
}

pub(crate) fn emit_conn_state(app: &AppHandle, connected: bool) {
    if let Err(e) = app.emit("index:state", ConnState { connected }) {
        log::warn!("emit index:state failed: {e}");
    }
}

/// Open the pipe per §4.1: explicit `CLIENT_PIPE_ACCESS` (never GENERIC_WRITE),
/// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`; on `ERROR_PIPE_BUSY`
/// wait 100 ms via `WaitNamedPipeW` and retry up to 5 times.
fn connect_pipe() -> io::Result<File> {
    let name: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut attempts = 0u32;
    loop {
        // SAFETY: `name` is a valid NUL-terminated UTF-16 string; all other
        // arguments are plain values / null pointers permitted by the API.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                yspot_proto::CLIENT_PIPE_ACCESS,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: `handle` is a freshly opened, owned handle; ownership
            // transfers to `File` exactly once.
            return Ok(unsafe { File::from_raw_handle(handle as RawHandle) });
        }
        // SAFETY: trivially safe thread-local read.
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_BUSY && attempts < 5 {
            attempts += 1;
            // SAFETY: same valid pipe name; 100 ms timeout per §4.1.
            let _ = unsafe { WaitNamedPipeW(name.as_ptr(), 100) };
            continue;
        }
        return Err(io::Error::from_raw_os_error(err as i32));
    }
}

fn connect_and_handshake() -> Result<File, String> {
    let mut file = connect_pipe().map_err(|e| format!("connect: {e}"))?;
    let hello = Message::Hello {
        proto_min: PROTO_VERSION,
        proto_max: PROTO_VERSION,
        client: "yspot-shell".to_string(),
        pid: std::process::id(),
    };
    yspot_proto::write_msg(&mut file, &hello).map_err(|e| format!("hello: {e}"))?;
    match yspot_proto::read_msg(&mut file, MAX_FRAME_S2C) {
        Ok(Some(Message::HelloAck {
            proto,
            service_version,
            index_epoch,
        })) => {
            log::info!(
                "connected to yspot-indexd: proto {proto}, service {service_version}, epoch {index_epoch}"
            );
            Ok(file)
        }
        Ok(Some(Message::Error { code, message, .. })) => {
            Err(format!("handshake refused: code {code}: {message}"))
        }
        Ok(Some(other)) => Err(format!("unexpected handshake reply: {other:?}")),
        Ok(None) => Err("pipe closed during handshake".to_string()),
        Err(e) => Err(format!("handshake read: {e}")),
    }
}

/// The single owner of the pipe handle: drain queued commands (writes), then
/// read whatever frames have arrived, then sleep briefly. Writes can never
/// wait behind a blocked read because nothing here blocks — reads happen only
/// after `PeekNamedPipe` says bytes are available. A partially-arrived frame
/// makes `read_msg` block for the remainder, which is bounded by the service
/// finishing a `write_msg` it has already started.
fn pump_loop(app: &AppHandle, client: &PipeClient, cmd_rx: &Receiver<Message>, mut file: File) {
    let mut idle = POLL_ACTIVE;
    loop {
        let mut active = false;

        loop {
            match cmd_rx.try_recv() {
                Ok(msg) => {
                    if let Err(e) = yspot_proto::write_msg(&mut file, &msg) {
                        log::warn!("indexd pipe write error: {e}");
                        return;
                    }
                    active = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        loop {
            match readable_bytes(&file) {
                Ok(0) => break,
                Ok(_) => match yspot_proto::read_msg(&mut file, MAX_FRAME_S2C) {
                    Ok(Some(msg)) => {
                        handle_msg(app, client, msg);
                        active = true;
                    }
                    Ok(None) => {
                        log::info!("indexd pipe closed by service");
                        return;
                    }
                    Err(e) => {
                        log::warn!("indexd pipe read error: {e}");
                        return;
                    }
                },
                Err(e) => {
                    log::info!("indexd pipe gone: {e}");
                    return;
                }
            }
        }

        if active {
            idle = POLL_ACTIVE;
        } else {
            std::thread::sleep(idle);
            idle = (idle * 2).min(POLL_IDLE_CAP);
        }
    }
}

fn handle_msg(app: &AppHandle, client: &PipeClient, msg: Message) {
    match msg {
        Message::SearchResults {
            gen,
            seq,
            is_final,
            items,
        } => {
            // Drop stale generations before relay (§4.4, §4.6).
            if gen < client.current_gen.load(Ordering::SeqCst) {
                return;
            }
            // §10 M0 harness endpoint, and specifically the §2.5 "results data
            // available ≤ 20 ms after keydown" one: the frame has crossed the
            // pipe and been decoded, before any relay or rendering.
            crate::etw_mark::mark(&format!(
                "results gen={gen} seq={seq} final={}",
                u8::from(is_final)
            ));
            let payload = JsSearchResults {
                gen,
                seq,
                is_final,
                items: items.into_iter().map(JsResultItem::from).collect(),
            };
            if let Err(e) = app.emit("search:results", payload) {
                log::warn!("emit search:results failed: {e}");
            }
        }
        Message::IndexStatus { id: _, volumes } => {
            if let Err(e) = app.emit("index:status", volumes) {
                log::warn!("emit index:status failed: {e}");
            }
        }
        Message::Event { topic, payload } => {
            // Relay service events; topic `a.b` becomes event `a:b` (§4.6).
            let json = serde_json::to_value(&payload).unwrap_or(serde_json::Value::Null);
            let event = topic.replace('.', ":");
            if let Err(e) = app.emit(&event, json) {
                log::warn!("emit {event} failed: {e}");
            }
        }
        Message::Error {
            id,
            gen,
            code,
            message,
            retryable,
        } => {
            log::warn!(
                "service error code {code} (id {id:?}, gen {gen:?}, retryable {retryable}): {message}"
            );
        }
        other => {
            log::debug!("unhandled pipe message: {other:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yspot_proto::ResultId;

    #[test]
    fn frn_serializes_as_decimal_string() {
        let item = ResultItem {
            id: ResultId {
                volume_idx: 2,
                frn: u64::MAX,
            },
            path: "C:\\Users\\x\\readme.md".into(),
            name: "readme.md".into(),
            score: 0.5,
            size: None,
            mtime: None,
            match_ranges: vec![(0, 4)],
            snippet: None,
        };
        let js = JsResultItem::from(item);
        let v = serde_json::to_value(&js).unwrap();
        assert_eq!(v["id"]["frn"], "18446744073709551615");
        assert_eq!(v["id"]["volumeIdx"], 2);
        assert_eq!(v["matchRanges"][0][0], 0);
        assert_eq!(v["matchRanges"][0][1], 4);
        assert_eq!(v["name"], "readme.md");
    }

    #[test]
    fn results_payload_uses_camel_case() {
        let payload = JsSearchResults {
            gen: 7,
            seq: 0,
            is_final: true,
            items: vec![],
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["gen"], 7);
        assert!(v["isFinal"].as_bool().unwrap());
        assert!(v.get("is_final").is_none());
    }
}
