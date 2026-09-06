//! Named-pipe client to `yspot-indexd` (SPEC.md §4.1, relay per §4.6).
//!
//! Two threads, no polling (issue #9):
//! - `yspot-pipe` owns the connection: connect (§4.1 open + server
//!   verification, in `yspot-pipe`) → Hello/HelloAck → a read loop that sits
//!   in an overlapped `ReadFile` until a frame arrives, and a 1 s → 10 s
//!   capped reconnect backoff when the pipe goes away.
//! - `yspot-pipe-w` drains the command queue into the current connection's
//!   write half. Commands from Tauri's thread pool — and the window-hide
//!   path on the event-loop thread — only ever ENQUEUE, so no UI or command
//!   thread can block on pipe I/O, however wedged the service is.
//!
//! Both halves share one overlapped handle, which is what makes the shape
//! safe: a pending read on one thread never blocks a write on the other.
//! (The synchronous-pipe version of exactly this shape deadlocked on the
//! shell's first `SearchQuery` — 0965adb — and was bridged by a
//! `PeekNamedPipe` poll that cost up to 2 ms of arrival latency per frame.
//! Frames now complete the read the moment they land.)
//!
//! Connection epochs keep commands from crossing connections: every queued
//! command carries the epoch it was enqueued under, and the writer drops
//! anything from an earlier one. A command enqueued for a connection that
//! then dies is therefore dropped rather than replayed to the next service
//! instance, whose generation watermark starts over.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use yspot_pipe::client::{self, ServerOwner};
use yspot_pipe::{Pipe, PipeReader, PipeWriter};
use yspot_proto::{Filters, Message, ResultItem, MAX_FRAME_S2C, PIPE_NAME, PROTO_VERSION};

use crate::frecency::Frecency;

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

impl JsResultId {
    /// The key §7.1 frecency is stored under for a file.
    ///
    /// It has to be byte-identical to what `executeAction` records against,
    /// which is the frontend's `rowKey` (§5.6: `volumeIdx:frn`). If these two
    /// ever disagree the failure is silent — launches accumulate under one
    /// key and the ranker reads another, so the file you open every day
    /// never rises and nothing reports an error.
    pub fn frecency_id(&self) -> String {
        format!("{}:{}", self.volume_idx, self.frn)
    }
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

/// A queued frame, stamped with the connection it was meant for.
struct Cmd {
    epoch: u64,
    msg: Message,
}

/// The write half of the live connection, installed by the connection thread
/// after the handshake and cleared when the connection dies.
struct ConnWriter {
    epoch: u64,
    writer: PipeWriter,
}

pub struct PipeClient {
    /// Command queue into the writer thread — the only path to the pipe.
    cmd_tx: Sender<Cmd>,
    /// The writer thread takes this once at spawn.
    cmd_rx: Mutex<Option<Receiver<Cmd>>>,
    /// Current connection's write half (`None` while disconnected).
    writer: Mutex<Option<ConnWriter>>,
    /// Bumps on every successful handshake; stamps queued commands.
    epoch: AtomicU64,
    req_id: AtomicU64,
    current_gen: AtomicU64,
    /// The text of the current generation, kept so a `107 SCOPE_UNSUPPORTED`
    /// reply can be re-asked of the shell's own provider (§3.1).
    current_text: Mutex<String>,
    connected: AtomicBool,
}

impl PipeClient {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        PipeClient {
            cmd_tx,
            cmd_rx: Mutex::new(Some(cmd_rx)),
            writer: Mutex::new(None),
            epoch: AtomicU64::new(0),
            req_id: AtomicU64::new(1),
            current_gen: AtomicU64::new(0),
            current_text: Mutex::new(String::new()),
            connected: AtomicBool::new(false),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn next_id(&self) -> u64 {
        self.req_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Enqueue for the writer thread. The connected check keeps the old error
    /// semantics for callers; a message racing a disconnect merely sits in the
    /// queue under a dead epoch and is dropped by the writer.
    fn send(&self, msg: Message) -> Result<(), String> {
        if !self.is_connected() {
            return Err("index service not connected".to_string());
        }
        self.cmd_tx
            .send(Cmd {
                epoch: self.epoch.load(Ordering::SeqCst),
                msg,
            })
            .map_err(|_| "index service writer thread gone".to_string())
    }

    /// §4.6 `search`: filename query, empty scopes, default filters, 50 rows.
    pub fn search(&self, gen: u64, text: String) -> Result<(), String> {
        self.search_with(gen, text, Filters::default(), MAX_RESULTS)
    }

    /// The root list's query, with §7.3's filters and a page size of the
    /// caller's choosing. The File Search view asks for a long page; the
    /// service clamps it (§4.3).
    pub fn search_with(
        &self,
        gen: u64,
        text: String,
        filters: Filters,
        max_results: u32,
    ) -> Result<(), String> {
        self.current_gen.store(gen, Ordering::SeqCst);
        *self.current_text.lock().unwrap_or_else(|e| e.into_inner()) = text.clone();
        self.send(Message::SearchQuery {
            id: self.next_id(),
            gen,
            text,
            scopes: Vec::new(),
            filters,
            max_results,
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

    /// §4.3 `PauseIndexing`: machine-wide, any interactive user may ask, and
    /// the service logs it. Backs the §5.5 tray toggle.
    pub fn pause_indexing(&self) -> Result<(), String> {
        self.send(Message::PauseIndexing { id: self.next_id() })
    }

    /// §4.3 `ResumeIndexing`.
    pub fn resume_indexing(&self) -> Result<(), String> {
        self.send(Message::ResumeIndexing { id: self.next_id() })
    }

    /// The query text of the current generation (§3.1's 107 re-ask).
    pub fn current_query(&self) -> (u64, String) {
        (
            self.current_gen.load(Ordering::SeqCst),
            self.current_text
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        )
    }

    fn lock_writer(&self) -> std::sync::MutexGuard<'_, Option<ConnWriter>> {
        self.writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Tear down the write half of a dead connection: abort any write in
    /// flight (the writer thread then fails out and releases the slot), take
    /// the slot, drop the half.
    fn drop_writer(&self, pipe: &Pipe) {
        self.connected.store(false, Ordering::SeqCst);
        pipe.cancel_all();
        *self.lock_writer() = None;
    }
}

/// Spawn the connection thread and the writer thread.
pub fn spawn(app: AppHandle, client: Arc<PipeClient>) {
    let cmd_rx = client
        .cmd_rx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .expect("pipe client spawned twice");
    let writer_client = client.clone();
    std::thread::Builder::new()
        .name("yspot-pipe-w".into())
        .spawn(move || write_loop(writer_client, cmd_rx))
        .expect("failed to spawn pipe writer thread");
    std::thread::Builder::new()
        .name("yspot-pipe".into())
        .spawn(move || run_loop(app, client))
        .expect("failed to spawn pipe client thread");
}

fn run_loop(app: AppHandle, client: Arc<PipeClient>) {
    let mut backoff = BACKOFF_START;
    loop {
        match connect_and_handshake() {
            Ok((mut reader, writer)) => {
                backoff = BACKOFF_START;
                let pipe = reader.pipe().clone();
                let epoch = client.epoch.fetch_add(1, Ordering::SeqCst) + 1;
                *client.lock_writer() = Some(ConnWriter { epoch, writer });
                client.connected.store(true, Ordering::SeqCst);
                emit_conn_state(&app, true);

                read_loop(&app, &client, &mut reader);

                client.drop_writer(&pipe);
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

/// Sit in the overlapped read until a frame arrives or the pipe dies.
fn read_loop(app: &AppHandle, client: &PipeClient, reader: &mut PipeReader) {
    loop {
        match yspot_proto::read_msg(reader, MAX_FRAME_S2C) {
            Ok(Some(msg)) => handle_msg(app, client, msg),
            Ok(None) => {
                log::info!("indexd pipe closed by service");
                return;
            }
            Err(e) => {
                // Includes the writer thread aborting this read after a write
                // failure, so both halves agree the connection is over.
                log::warn!("indexd pipe read ended: {e}");
                return;
            }
        }
    }
}

/// Drain commands into the live connection for the life of the process.
fn write_loop(client: Arc<PipeClient>, cmd_rx: Receiver<Cmd>) {
    for cmd in cmd_rx {
        let mut slot = client.lock_writer();
        let Some(conn) = slot.as_mut() else {
            continue; // disconnected: stale by definition
        };
        if conn.epoch != cmd.epoch {
            continue; // queued for a connection that is gone
        }
        if let Err(e) = yspot_proto::write_msg(&mut conn.writer, &cmd.msg) {
            log::warn!("indexd pipe write error: {e}");
            // Wake the connection thread out of its read so it reconnects; it
            // will clear this slot again, harmlessly.
            client.connected.store(false, Ordering::SeqCst);
            conn.writer.pipe().cancel_all();
            *slot = None;
        }
    }
}

pub(crate) fn emit_conn_state(app: &AppHandle, connected: bool) {
    if let Err(e) = app.emit("index:state", ConnState { connected }) {
        log::warn!("emit index:state failed: {e}");
    }
}

/// §4.1 open + server verification (in `yspot-pipe`), then Hello/HelloAck —
/// the first write on the pipe happens after the server has been verified.
fn connect_and_handshake() -> Result<(PipeReader, PipeWriter), String> {
    let conn = client::connect(PIPE_NAME).map_err(|e| format!("connect: {e}"))?;
    let server = conn.server;
    let (mut reader, mut writer) = conn.split().map_err(|e| format!("split: {e}"))?;
    let hello = Message::Hello {
        proto_min: PROTO_VERSION,
        proto_max: PROTO_VERSION,
        client: "yspot-shell".to_string(),
        pid: std::process::id(),
    };
    yspot_proto::write_msg(&mut writer, &hello).map_err(|e| format!("hello: {e}"))?;
    match yspot_proto::read_msg(&mut reader, MAX_FRAME_S2C) {
        Ok(Some(Message::HelloAck {
            proto,
            service_version,
            index_epoch,
        })) => {
            let owner = match server.owner {
                ServerOwner::System => "SYSTEM".to_string(),
                dev => format!("{dev:?} — DEV MODE, not the installed service"),
            };
            log::info!(
                "connected to yspot-indexd: proto {proto}, service {service_version}, epoch \
                 {index_epoch}, server pid {}, pipe owner {owner}",
                server.pid
            );
            Ok((reader, writer))
        }
        Ok(Some(Message::Error { code, message, .. })) => {
            Err(format!("handshake refused: code {code}: {message}"))
        }
        Ok(Some(other)) => Err(format!("unexpected handshake reply: {other:?}")),
        Ok(None) => Err("pipe closed during handshake".to_string()),
        Err(e) => Err(format!("handshake read: {e}")),
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
            let mut items: Vec<JsResultItem> = items.into_iter().map(JsResultItem::from).collect();
            // §5.11 rule 2 wants frecency applied uniformly across sources,
            // and the shell is the only place it can be: the store is
            // per-user and lives on this side of the pipe (§7.1), while the
            // service ranks for a machine. The id is the one `executeAction`
            // records under (§5.6 `volumeIdx:frn`), so opening a file from
            // here is what lifts it next time.
            //
            // Bounded by construction: the bonus cannot promote a file the
            // service never sent, so a rarely-matched favourite still has to
            // clear the service's own cut to be reordered here.
            if let Some(frec) = app.try_state::<Arc<Frecency>>() {
                for it in &mut items {
                    it.score += frec.bonus(&it.id.frecency_id());
                }
                items.sort_by(|a, b| b.score.total_cmp(&a.score));
            }
            let payload = JsSearchResults {
                gen,
                seq,
                is_final,
                items,
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
            // §3.1: a scope the service does not index is not an error to
            // report, it is a scope to ask Windows Search about instead.
            if code == yspot_proto::codes::SCOPE_UNSUPPORTED {
                let (current, text) = client.current_query();
                if gen == Some(current) && !text.is_empty() {
                    log::debug!("gen {current}: scope unsupported, routing to Windows Search");
                    crate::run_fallback_search(app, current, text, "unsupported scope");
                    return;
                }
            }
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

    #[test]
    fn commands_are_refused_while_disconnected() {
        let c = PipeClient::new();
        assert!(c.search(1, "x".into()).is_err());
        assert!(c.cancel_current().is_err());
        assert!(c.request_status().is_err());
        // The generation is still recorded, so stale-frame dropping works
        // from the first reply after a reconnect.
        assert_eq!(c.current_gen.load(Ordering::SeqCst), 1);
    }

    /// The one that matters: this string is written by `executeAction` and
    /// read by the ranker, and the two live in different languages.
    #[test]
    fn the_frecency_key_is_the_one_the_frontend_records_under() {
        let id = JsResultId {
            volume_idx: 2,
            // Past 2^53, which is why `frn` crosses the wire as a string.
            frn: "9007199254740993".to_string(),
        };
        // `rowKey` in apps/shell/src/lib/ipc.ts: `${id.volumeIdx}:${id.frn}`.
        assert_eq!(id.frecency_id(), "2:9007199254740993");
    }
}
