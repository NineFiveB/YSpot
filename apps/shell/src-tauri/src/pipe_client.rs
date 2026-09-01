//! Named-pipe client to `yspot-indexd` (SPEC.md §4.1, relay per §4.6).
//!
//! One background thread owns connect → Hello/HelloAck → read loop, with a
//! 1 s → 10 s capped reconnect backoff. Writers go through
//! `Mutex<Option<File>>` (commands run on Tauri's thread pool).
//!
//! TODO(M1, §4.1 client hardening): verify the server before first write —
//! `GetNamedPipeServerProcessId` + pipe owner SID == S-1-5-18 via
//! `GetSecurityInfo`.

use std::fs::File;
use std::io;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_BUSY, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
use yspot_proto::{Filters, Message, ResultItem, MAX_FRAME_S2C, PIPE_NAME, PROTO_VERSION};

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
    writer: Mutex<Option<File>>,
    req_id: AtomicU64,
    current_gen: AtomicU64,
    connected: AtomicBool,
}

impl PipeClient {
    pub fn new() -> Self {
        PipeClient {
            writer: Mutex::new(None),
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

    fn send(&self, msg: &Message) -> Result<(), String> {
        // Poison-tolerant (see run_loop): recover the inner Option<File>.
        let mut guard = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_mut() {
            Some(file) => yspot_proto::write_msg(file, msg).map_err(|e| {
                log::warn!("pipe write failed: {e}");
                format!("index service write failed: {e}")
            }),
            None => Err("index service not connected".to_string()),
        }
    }

    /// §4.6 `search`: filename query, empty scopes, default filters, 50 rows.
    pub fn search(&self, gen: u64, text: String) -> Result<(), String> {
        self.current_gen.store(gen, Ordering::SeqCst);
        self.send(&Message::SearchQuery {
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
        self.send(&Message::Cancel { gen })
    }

    /// §4.6 `getIndexStatus` relay: reply arrives as an `index:status` event.
    pub fn request_status(&self) -> Result<(), String> {
        self.send(&Message::IndexStatusReq { id: self.next_id() })
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
    let mut backoff = BACKOFF_START;
    loop {
        match connect_and_handshake() {
            Ok(file) => {
                backoff = BACKOFF_START;
                let reader = match file.try_clone() {
                    Ok(f) => f,
                    Err(e) => {
                        log::warn!("pipe handle clone failed: {e}");
                        std::thread::sleep(backoff);
                        continue;
                    }
                };
                // Poison-tolerant: the guarded value is just an Option<File>,
                // never left half-written — a panic elsewhere must not desync
                // `connected` from the actual writer state.
                *client.writer.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
                client.connected.store(true, Ordering::SeqCst);
                emit_conn_state(&app, true);

                read_loop(&app, &client, reader);

                *client.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

fn read_loop(app: &AppHandle, client: &PipeClient, mut file: File) {
    loop {
        match yspot_proto::read_msg(&mut file, MAX_FRAME_S2C) {
            Ok(Some(msg)) => handle_msg(app, client, msg),
            Ok(None) => {
                log::info!("indexd pipe closed by service");
                return;
            }
            Err(e) => {
                log::warn!("indexd pipe read error: {e}");
                return;
            }
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
