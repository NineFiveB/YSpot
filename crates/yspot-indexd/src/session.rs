//! Per-connection protocol handling (SPEC §4.3–§4.4).
//!
//! The connection thread does nothing but read: every frame is decoded here
//! and either answered inline (status, acks, errors) or handed to this
//! connection's search worker. Reads and writes are overlapped (`yspot-pipe`,
//! issue #9), so the worker's replies never wait behind the pending read, and
//! a `Cancel` or a newer `gen` is decoded WHILE a search runs — the worker
//! sees the raised watermark at its next cancellation poll, within one
//! matcher stride (§4.4 "promptly, SHOULD be < 5 ms").
//!
//! One worker per connection, fed through a single-slot mailbox: a query that
//! arrives while an older one is still pending replaces it, so a burst of
//! keystrokes never starts a search for a generation that is already stale
//! (§4.4 "MUST NOT start new batches for them") and never queues up behind
//! one. The running search is cancelled by the watermark, not pre-empted.
//!
//! M0 deviations that remain, deliberate and documented:
//! - ONE `SearchResults` batch per generation (`seq: 0, is_final: true`);
//!   streamed batches are a later M1 item.
//! - `Subscribe` is Ack'd but no events are emitted yet.

use std::io::ErrorKind;
use std::os::windows::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use yspot_pipe::{Pipe, PipeReader, PipeWriter};
use yspot_proto::{
    codes, read_msg, write_msg, Filters, Message, ProtoError, ResultId, ResultItem,
    SessionActivity, MAX_FRAME_C2S, PROTO_VERSION,
};

use crate::idx_api;
use crate::state::ServiceState;

/// Longest query the matcher will accept, bytes.
///
/// An NTFS name is at most 255 UTF-16 units, so a filename fragment cannot
/// legitimately need more than this once folded. The 1 MiB frame cap is not a
/// useful bound here: folding expands (U+0130 -> 3 bytes), and the matcher's
/// preflight - class mask, presence probes, the char-vector fallback - is
/// O(query) work that polls no cancellation.
const MAX_QUERY_BYTES: usize = 1024;

/// Withdraws this connection's "active" vote however the session ends.
///
/// §3.6 makes the machine idle only when every session reports idle, so a
/// session that drops while active would hold heavy maintenance off forever.
/// `run` returns from several places (protocol error, EOF, write failure), so
/// this is a guard rather than bookkeeping at each exit.
struct ActivityVote {
    state: Arc<ServiceState>,
    active: bool,
}

impl Drop for ActivityVote {
    fn drop(&mut self) {
        self.state.set_session_active(self.active, false);
    }
}

/// Everything a `SearchQuery` carries to the worker.
struct Query {
    gen: u64,
    text: String,
    filters: Filters,
    max_results: u32,
}

#[derive(Default)]
struct Slot {
    pending: Option<Query>,
    closed: bool,
}

#[derive(Default)]
struct Mailbox {
    slot: Mutex<Slot>,
    ready: Condvar,
}

impl Mailbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// This connection's search thread. Dropping it cancels whatever is running
/// (the watermark goes to `u64::MAX`), closes the mailbox, and joins.
struct SearchWorker {
    mailbox: Arc<Mailbox>,
    latest_gen: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
    /// Fallback when the thread could not be spawned: searches run inline on
    /// the connection thread, which is the M0 behavior — answers still come,
    /// only §4.4 mid-search cancellation is lost.
    inline: Option<(Arc<ServiceState>, Arc<Mutex<PipeWriter>>)>,
}

impl SearchWorker {
    fn spawn(
        state: Arc<ServiceState>,
        latest_gen: Arc<AtomicU64>,
        writer: Arc<Mutex<PipeWriter>>,
    ) -> SearchWorker {
        let mailbox = Arc::new(Mailbox::default());
        let (mb, st, lg, wr) = (
            mailbox.clone(),
            state.clone(),
            latest_gen.clone(),
            writer.clone(),
        );
        let thread = std::thread::Builder::new()
            .name("pipe-search".into())
            .spawn(move || worker_loop(mb, st, lg, wr));
        let (thread, inline) = match thread {
            Ok(t) => (Some(t), None),
            Err(e) => {
                log::error!("search worker spawn failed ({e}); searches will run inline");
                (None, Some((state, writer)))
            }
        };
        SearchWorker {
            mailbox,
            latest_gen,
            thread,
            inline,
        }
    }

    fn submit(&self, q: Query) {
        if let Some((state, writer)) = &self.inline {
            run_search(state, &self.latest_gen, writer, q);
            return;
        }
        let mut slot = self.mailbox.lock();
        if slot.pending.replace(q).is_some() {
            log::debug!("pending query replaced before it started (§4.4)");
        }
        drop(slot);
        self.mailbox.ready.notify_one();
    }
}

impl Drop for SearchWorker {
    fn drop(&mut self) {
        // Kill the running search (latest > every gen) and close the mailbox.
        self.latest_gen.store(u64::MAX, Ordering::SeqCst);
        {
            let mut slot = self.mailbox.lock();
            slot.closed = true;
            slot.pending = None;
        }
        self.mailbox.ready.notify_all();
        if let Some(t) = self.thread.take() {
            if t.join().is_err() {
                log::error!("search worker panicked");
            }
        }
    }
}

fn worker_loop(
    mailbox: Arc<Mailbox>,
    state: Arc<ServiceState>,
    latest_gen: Arc<AtomicU64>,
    writer: Arc<Mutex<PipeWriter>>,
) {
    loop {
        let q = {
            let mut slot = mailbox.lock();
            loop {
                if slot.closed {
                    return;
                }
                if let Some(q) = slot.pending.take() {
                    break q;
                }
                slot = mailbox.ready.wait(slot).unwrap_or_else(|e| e.into_inner());
            }
        };
        // Superseded while it waited its turn: never start it (§4.4).
        if latest_gen.load(Ordering::SeqCst) > q.gen {
            log::debug!("gen {} superseded before it started; skipped", q.gen);
            continue;
        }
        run_search(&state, &latest_gen, &writer, q);
    }
}

pub fn run(pipe: Arc<Pipe>, state: Arc<ServiceState>) {
    let (mut reader, writer) = match pipe.split() {
        Ok(halves) => halves,
        Err(e) => {
            log::error!("pipe split failed: {e}");
            return;
        }
    };
    let writer = Arc::new(Mutex::new(writer));

    if !handshake(&mut reader, &writer, &state) {
        return;
    }

    // Per-connection generation watermark (§4.4): queries and cancels only
    // ever raise it; a search is dead once latest_gen exceeds its gen.
    let latest_gen = Arc::new(AtomicU64::new(0));
    // What this connection last reported; the guard withdraws it on exit.
    let mut activity = ActivityVote {
        state: state.clone(),
        active: false,
    };
    // Declared last so it is dropped first: the worker is cancelled and
    // joined before the reader half goes, and the handle closes after both.
    let worker = SearchWorker::spawn(state.clone(), latest_gen.clone(), writer.clone());

    loop {
        let msg = match read_msg(&mut reader, MAX_FRAME_C2S) {
            Ok(Some(m)) => m,
            Ok(None) => {
                log::debug!("client disconnected cleanly");
                return;
            }
            Err(ProtoError::Io(ref io)) if io.kind() == ErrorKind::BrokenPipe => {
                log::debug!("client connection broken");
                return;
            }
            // Oversized or undecodable frame: protocol error, drop the
            // connection (§4.2).
            Err(e) => {
                log::warn!("protocol error, dropping connection: {e}");
                return;
            }
        };

        match msg {
            Message::SearchQuery {
                id: _,
                gen,
                text,
                scopes,
                filters,
                max_results,
            } => {
                latest_gen.fetch_max(gen, Ordering::SeqCst);
                if !scopes.is_empty() {
                    log::debug!("scopes ignored in M0: {scopes:?}");
                }
                // A query is a filename fragment; nothing legitimate is longer
                // than an NTFS name. The frame cap (1 MiB) is not a bound on
                // its own, and folding EXPANDS (U+0130 -> 3 bytes), so an
                // oversized query buys a caller a multiple of that in
                // uninterruptible preflight - class-mask and presence probes
                // that poll no cancellation. Reject it here rather than let it
                // reach the matcher (SPEC 8.1: pipe input is untrusted).
                if text.len() > MAX_QUERY_BYTES {
                    log::debug!(
                        "query of {} bytes rejected (cap {})",
                        text.len(),
                        MAX_QUERY_BYTES
                    );
                    if !send(&writer, &empty_final(gen)) {
                        return;
                    }
                    continue;
                }
                if text.is_empty() {
                    // Empty query ⇒ empty final batch, no index work.
                    if !send(&writer, &empty_final(gen)) {
                        return;
                    }
                    continue;
                }
                worker.submit(Query {
                    gen,
                    text,
                    filters,
                    max_results,
                });
            }
            Message::Cancel { gen } => {
                // Raise past `gen` so is_cancelled (latest > gen) kills the
                // generation itself — "stop entirely" (§4.4).
                latest_gen.fetch_max(gen.saturating_add(1), Ordering::SeqCst);
                log::debug!("cancel gen={gen}");
            }
            Message::IndexStatusReq { id } => {
                let status = Message::IndexStatus {
                    id,
                    volumes: vec![state.volume_status()],
                };
                if !send(&writer, &status) {
                    return;
                }
            }
            Message::SessionState { state: reported } => {
                // Fire-and-forget (§4.3): no id, nothing to Ack. Tracked
                // rather than merely logged because §3.6 gates heavy
                // maintenance on EVERY session being idle, so one active
                // session has to hold it off for the whole machine.
                let now_active = matches!(reported, SessionActivity::Active);
                state.set_session_active(activity.active, now_active);
                activity.active = now_active;
                log::info!("session activity: {reported:?}");
            }
            Message::PauseIndexing { id } => {
                state.paused.store(true, Ordering::SeqCst);
                log::info!("indexing PAUSED machine-wide by client request (§3.6, logged)");
                if !send(&writer, &Message::Ack { id }) {
                    return;
                }
            }
            Message::ResumeIndexing { id } => {
                state.paused.store(false, Ordering::SeqCst);
                log::info!("indexing RESUMED by client request (logged)");
                if !send(&writer, &Message::Ack { id }) {
                    return;
                }
            }
            Message::ContentSearchQuery { id, gen, .. } => {
                let err = Message::Error {
                    id: Some(id),
                    gen: Some(gen),
                    code: codes::SCOPE_UNSUPPORTED,
                    message: "no content index in M0".into(),
                    retryable: false,
                };
                if !send(&writer, &err) {
                    return;
                }
            }
            Message::Subscribe { id, topics } => {
                log::debug!("Subscribe accepted, but M0 emits no events yet: {topics:?}");
                if !send(&writer, &Message::Ack { id }) {
                    return;
                }
            }
            other => {
                // Not a v1 client request (§4.5: unknown requests get 105, not
                // a disconnect).
                log::debug!("unexpected message from client: {other:?}");
                let err = Message::Error {
                    id: None,
                    gen: None,
                    code: codes::UNKNOWN_MESSAGE,
                    message: "unexpected message type".into(),
                    retryable: false,
                };
                if !send(&writer, &err) {
                    return;
                }
            }
        }
    }
}

fn empty_final(gen: u64) -> Message {
    Message::SearchResults {
        gen,
        seq: 0,
        is_final: true,
        items: Vec::new(),
    }
}

/// Expect `Hello` first (else Error 105 + drop); negotiate the version and
/// reply `HelloAck` (§4.3).
fn handshake(reader: &mut PipeReader, writer: &Mutex<PipeWriter>, state: &ServiceState) -> bool {
    match read_msg(reader, MAX_FRAME_C2S) {
        Ok(Some(Message::Hello {
            proto_min,
            proto_max,
            client,
            pid,
        })) => {
            if proto_min > PROTO_VERSION || proto_max < PROTO_VERSION {
                // Disjoint version ranges ⇒ Error 100, close (§4.8).
                let err = Message::Error {
                    id: None,
                    gen: None,
                    code: codes::UNSUPPORTED_VERSION,
                    message: format!(
                        "service speaks proto {PROTO_VERSION}, client offered {proto_min}..={proto_max}"
                    ),
                    retryable: false,
                };
                send(writer, &err);
                return false;
            }
            log::info!("client connected: {client:?} pid={pid} proto={PROTO_VERSION}");
            send(
                writer,
                &Message::HelloAck {
                    proto: PROTO_VERSION,
                    service_version: env!("CARGO_PKG_VERSION").to_string(),
                    index_epoch: state.index_epoch.load(Ordering::SeqCst),
                },
            )
        }
        Ok(Some(other)) => {
            log::warn!("first frame was not Hello ({other:?}); dropping connection");
            let err = Message::Error {
                id: None,
                gen: None,
                code: codes::UNKNOWN_MESSAGE,
                message: "expected Hello".into(),
                retryable: false,
            };
            send(writer, &err);
            false
        }
        Ok(None) => false,
        Err(e) => {
            log::debug!("handshake read failed: {e}");
            false
        }
    }
}

/// Serialize one frame under the writer mutex so the worker and the
/// connection thread never interleave frames. Returns false when the
/// connection is dead.
fn send(writer: &Mutex<PipeWriter>, msg: &Message) -> bool {
    let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
    match write_msg(&mut *w, msg) {
        Ok(()) => true,
        Err(e) => {
            log::debug!("pipe write failed ({e}); connection is gone");
            false
        }
    }
}

fn run_search(state: &ServiceState, latest_gen: &AtomicU64, writer: &Mutex<PipeWriter>, q: Query) {
    let Query {
        gen,
        text,
        filters,
        max_results,
    } = q;
    let t0 = Instant::now();
    let is_cancelled = || latest_gen.load(Ordering::SeqCst) > gen;

    let max = max_results.clamp(1, 512) as usize;
    let exts = normalize_exts(&filters.ext);
    let path_needle = filters
        .path_substr
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(idx_api::fold);
    if filters.kind.is_some() {
        log::debug!("kind filter ignored in M0");
    }
    // The ext filter is a test on the NAME, so it goes into the matcher and the
    // page fills with `max` accepted rows. A path filter needs `path_of` per
    // candidate — a parent-chain walk — which is far too expensive to run while
    // ranking, so it stays a post-filter and still has to over-fetch. That
    // over-fetch is a guess: a path filter selective enough to reject more than
    // `fetch` rows returns short, which is why only the ext half moved.
    let fetch = if path_needle.is_some() {
        max.saturating_mul(8).clamp(max, 4096)
    } else {
        max
    };

    let match_ms;
    let mut items: Vec<ResultItem> = Vec::new();
    {
        let idx = state.index_read();
        let hits = idx_api::search_filtered(&idx, &text, fetch, &is_cancelled, &|name| {
            exts.is_empty() || ext_matches(name, &exts)
        });
        match_ms = t0.elapsed().as_secs_f64() * 1e3;
        for h in hits {
            if items.len() >= max {
                break;
            }
            let name = match idx_api::name_of(&idx, h.frn) {
                Some(n) => n,
                None => continue, // entry vanished between match and assembly
            };
            let path = match idx_api::path_of(&idx, h.frn) {
                Some(p) => p,
                None => continue,
            };
            // path_substr: case-folded substring match on the parent path
            // (§4.3) — parent = full path minus the trailing name.
            if let Some(needle) = &path_needle {
                if !idx_api::fold(parent_of(&path, &name)).contains(needle.as_str()) {
                    continue;
                }
            }
            items.push(ResultItem {
                id: ResultId {
                    volume_idx: 0, // single M0 volume
                    frn: h.frn,
                },
                path,
                name,
                score: h.score,
                size: None,
                mtime: None,
                match_ranges: h.match_ranges,
                snippet: None,
            });
        }
    } // read lock released before stat-ing and writing

    if is_cancelled() {
        log::debug!("gen {gen} superseded after match; dropped");
        return;
    }

    // Lazily stat ONLY the returned page (§4.3): size + mtime.
    // last_write_time() is already FILETIME ticks (100 ns since 1601 UTC).
    for it in &mut items {
        if let Ok(md) = std::fs::metadata(&it.path) {
            it.size = Some(md.len());
            it.mtime = Some(md.last_write_time());
        }
    }

    // Final staleness check just before the write; stale results drop silently.
    if is_cancelled() {
        log::debug!("gen {gen} superseded before write; dropped");
        return;
    }

    let n = items.len();
    let batch = Message::SearchResults {
        gen,
        seq: 0,
        is_final: true, // M0: one batch per generation
        items,
    };
    if send(writer, &batch) {
        log::info!(
            "query gen={gen} qlen={} match_ms={match_ms:.2} total_ms={:.2} n={n}",
            text.chars().count(),
            t0.elapsed().as_secs_f64() * 1e3
        );
    }
}

/// Normalize the `ext` filter list: trim, strip leading dots, lowercase, and
/// prepend one dot — `"RS"`, `"rs"`, `".rs"` all become `".rs"`.
fn normalize_exts(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|s| s.trim().trim_start_matches('.'))
        .filter(|s| !s.is_empty())
        .map(|s| format!(".{}", s.to_lowercase()))
        .collect()
}

/// Case-insensitive suffix match of any normalized extension against the name.
fn ext_matches(name: &str, exts: &[String]) -> bool {
    let folded = name.to_lowercase();
    exts.iter().any(|e| folded.ends_with(e.as_str()))
}

/// Parent path = full path minus the trailing name (keeps the separator).
/// Falls back to the full path when the name is not its suffix.
fn parent_of<'a>(path: &'a str, name: &str) -> &'a str {
    if !name.is_empty() && path.len() > name.len() && path.ends_with(name) {
        &path[..path.len() - name.len()]
    } else {
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_normalization() {
        assert_eq!(
            normalize_exts(&[
                "rs".into(),
                ".RS".into(),
                "".into(),
                ".".into(),
                " .Txt ".into()
            ]),
            vec![".rs".to_string(), ".rs".into(), ".txt".into()]
        );
    }

    #[test]
    fn ext_matching_is_case_insensitive_suffix() {
        let exts = normalize_exts(&["rs".into()]);
        assert!(ext_matches("main.RS", &exts));
        assert!(ext_matches("main.rs", &exts));
        assert!(!ext_matches("main.rss", &exts));
        assert!(!ext_matches("mainrs", &exts));
        assert!(!ext_matches("rs", &exts));
    }

    #[test]
    fn multiple_exts_are_or_semantics() {
        let exts = normalize_exts(&["rs".into(), "toml".into()]);
        assert!(ext_matches("Cargo.TOML", &exts));
        assert!(ext_matches("lib.rs", &exts));
        assert!(!ext_matches("lib.md", &exts));
    }

    #[test]
    fn parent_path_derivation() {
        assert_eq!(parent_of("C:\\a\\b.txt", "b.txt"), "C:\\a\\");
        assert_eq!(parent_of("C:\\a\\b.txt", "nope"), "C:\\a\\b.txt");
        assert_eq!(parent_of("x", "x"), "x");
        assert_eq!(parent_of("C:\\ünïcode\\ñ.txt", "ñ.txt"), "C:\\ünïcode\\");
        assert_eq!(parent_of("C:\\a\\b", ""), "C:\\a\\b");
    }
}

/// End-to-end session tests over a real named pipe in this process: the
/// connection thread, its worker, and a client speaking §4 exactly as the
/// shell does.
#[cfg(test)]
mod session_tests {
    use std::io::Write;
    use std::sync::atomic::AtomicU32;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use yspot_pipe::{client, server, Duplex};
    use yspot_proto::MAX_FRAME_S2C;

    use super::*;
    use crate::state::Mode;

    const WAIT: Duration = Duration::from_secs(20);

    fn unique_name() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            r"\\.\pipe\yspot-session-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// A few thousand names so a search does real work but stays fast.
    fn test_state() -> Arc<ServiceState> {
        let root = "C:\\yspot-session-test\\";
        let mut idx = idx_api::new_index(0, root);
        for i in 0..4000u64 {
            idx_api::apply_usn(
                &mut idx,
                idx_api::UsnEvent::Create {
                    frn: 1_000 + i,
                    parent_frn: 0,
                    name: format!("report-{i}.txt"),
                    flags: 0,
                },
            );
        }
        Arc::new(ServiceState::new(idx, root.to_string(), Mode::Walk))
    }

    /// Start a session on its own thread and return the connected, handshaken
    /// client plus the session thread's completion channel.
    fn start(state: Arc<ServiceState>) -> (Duplex, mpsc::Receiver<()>) {
        let name = unique_name();
        let listener = server::create_instance(&name, None, true).unwrap();
        let connector = thread::spawn(move || client::connect(&name).unwrap());
        server::accept(&listener).unwrap();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            run(listener, state);
            let _ = done_tx.send(());
        });
        let mut c = connector.join().unwrap().duplex().unwrap();
        write_msg(
            &mut c,
            &Message::Hello {
                proto_min: PROTO_VERSION,
                proto_max: PROTO_VERSION,
                client: "session-test".into(),
                pid: std::process::id(),
            },
        )
        .unwrap();
        match read_msg(&mut c, MAX_FRAME_S2C).unwrap() {
            Some(Message::HelloAck { proto, .. }) => assert_eq!(proto, PROTO_VERSION),
            other => panic!("handshake reply {other:?}"),
        }
        (c, done_rx)
    }

    fn query(c: &mut Duplex, gen: u64, text: &str) {
        write_msg(
            c,
            &Message::SearchQuery {
                id: gen,
                gen,
                text: text.to_string(),
                scopes: vec![],
                filters: Filters::default(),
                max_results: 50,
            },
        )
        .unwrap();
        c.flush().unwrap();
    }

    #[test]
    fn a_keystroke_burst_is_answered_in_generation_order_ending_with_the_latest() {
        let (mut c, _done) = start(test_state());

        // 40 queries back-to-back, nothing read in between: the shape of a
        // fast typist. With inline searches every one of these ran; with the
        // worker most are superseded before they start.
        let last = 40u64;
        for gen in 1..=last {
            query(&mut c, gen, &"report"[..1 + (gen as usize % 6)]);
        }
        let mut seen = Vec::new();
        loop {
            match read_msg(&mut c, MAX_FRAME_S2C).unwrap() {
                Some(Message::SearchResults {
                    gen,
                    is_final,
                    items,
                    ..
                }) => {
                    assert!(gen >= 1 && gen <= last, "gen {gen} out of range");
                    assert!(
                        seen.last().is_none_or(|&prev| gen > prev),
                        "batches out of order: {seen:?} then {gen}"
                    );
                    seen.push(gen);
                    if gen == last {
                        assert!(is_final);
                        assert!(!items.is_empty());
                        break;
                    }
                }
                other => panic!("unexpected frame {other:?}"),
            }
        }
        assert!(seen.len() <= last as usize);

        // Cancel "stop entirely" leaves the connection healthy: a status
        // request is answered, and the next generation searches normally.
        query(&mut c, 41, "report-1");
        write_msg(&mut c, &Message::Cancel { gen: 41 }).unwrap();
        write_msg(&mut c, &Message::IndexStatusReq { id: 5 }).unwrap();
        loop {
            match read_msg(&mut c, MAX_FRAME_S2C).unwrap() {
                Some(Message::IndexStatus { id, volumes }) => {
                    assert_eq!(id, 5);
                    assert_eq!(volumes[0].files_indexed, 4000);
                    break;
                }
                // Gen 41's batch MAY still arrive (§4.4); nothing else may.
                Some(Message::SearchResults { gen: 41, .. }) => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }
        query(&mut c, 42, "report-39");
        loop {
            match read_msg(&mut c, MAX_FRAME_S2C).unwrap() {
                Some(Message::SearchResults {
                    gen: 42,
                    is_final: true,
                    items,
                    ..
                }) => {
                    assert!(items.iter().any(|i| i.name == "report-39.txt"));
                    break;
                }
                Some(Message::SearchResults { gen: 41, .. }) => {}
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    #[test]
    fn empty_and_oversized_queries_are_answered_inline_with_empty_final_batches() {
        let (mut c, _done) = start(test_state());
        query(&mut c, 1, "");
        query(&mut c, 2, &"x".repeat(MAX_QUERY_BYTES + 1));
        for expect in [1u64, 2] {
            match read_msg(&mut c, MAX_FRAME_S2C).unwrap() {
                Some(Message::SearchResults {
                    gen,
                    is_final,
                    items,
                    ..
                }) => {
                    assert_eq!(gen, expect);
                    assert!(is_final);
                    assert!(items.is_empty());
                }
                other => panic!("unexpected frame {other:?}"),
            }
        }
    }

    #[test]
    fn client_disconnect_ends_the_session_and_joins_its_worker() {
        let (mut c, done) = start(test_state());
        for gen in 1..=10 {
            query(&mut c, gen, "report");
        }
        drop(c);
        done.recv_timeout(WAIT)
            .expect("session thread did not exit after the client disconnected");
    }
}
