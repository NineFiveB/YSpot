//! Per-connection protocol handling (SPEC §4.3–§4.4).
//!
//! M0 deviations, deliberate and documented:
//! - ONE `SearchResults` batch per generation (`seq: 0, is_final: true`);
//!   streamed batches arrive in M1.
//! - Each `SearchQuery` runs on its own spawned thread so the connection
//!   thread keeps reading (newer gens / Cancel stay live); staleness is
//!   re-checked just before the write and stale results are dropped silently.
//! - `Subscribe` is Ack'd but no events are emitted yet.

use std::fs::File;
use std::io::ErrorKind;
use std::os::windows::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use yspot_proto::{
    codes, read_msg, write_msg, Filters, Message, ProtoError, ResultId, ResultItem, MAX_FRAME_C2S,
    PROTO_VERSION,
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

pub fn run(mut reader: File, state: Arc<ServiceState>) {
    // One duplicated handle for writes: reads stay on the connection thread,
    // search threads write through the mutex (byte-mode duplex pipe).
    let writer = match reader.try_clone() {
        Ok(f) => Arc::new(Mutex::new(f)),
        Err(e) => {
            log::error!("pipe handle clone failed: {e}");
            return;
        }
    };

    if !handshake(&mut reader, &writer, &state) {
        return;
    }

    // Per-connection generation watermark (§4.4): queries and cancels only
    // ever raise it; a search is dead once latest_gen exceeds its gen.
    let latest_gen = Arc::new(AtomicU64::new(0));

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
                    let empty = Message::SearchResults {
                        gen,
                        seq: 0,
                        is_final: true,
                        items: Vec::new(),
                    };
                    if !send(&writer, &empty) {
                        return;
                    }
                    continue;
                }
                if text.is_empty() {
                    // Empty query ⇒ empty final batch, no index work.
                    let empty = Message::SearchResults {
                        gen,
                        seq: 0,
                        is_final: true,
                        items: Vec::new(),
                    };
                    if !send(&writer, &empty) {
                        return;
                    }
                    continue;
                }
                let (st, lg, w) = (state.clone(), latest_gen.clone(), writer.clone());
                let spawned = std::thread::Builder::new()
                    .name("search".into())
                    .spawn(move || run_search(st, lg, w, gen, text, filters, max_results));
                if let Err(e) = spawned {
                    log::error!("search thread spawn failed: {e}");
                    let overloaded = Message::Error {
                        id: None,
                        gen: Some(gen),
                        code: codes::OVERLOADED,
                        message: "search worker unavailable".into(),
                        retryable: true,
                    };
                    if !send(&writer, &overloaded) {
                        return;
                    }
                }
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
            Message::SessionState { state: activity } => {
                // Fire-and-forget (§4.3): no id, nothing to Ack.
                log::info!("session activity: {activity:?}");
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

/// Expect `Hello` first (else Error 105 + drop); negotiate the version and
/// reply `HelloAck` (§4.3).
fn handshake(reader: &mut File, writer: &Arc<Mutex<File>>, state: &ServiceState) -> bool {
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

/// Serialize one frame under the writer mutex so concurrent search threads
/// never interleave frames. Returns false when the connection is dead.
fn send(writer: &Mutex<File>, msg: &Message) -> bool {
    let mut f = writer.lock().unwrap_or_else(|e| e.into_inner());
    match write_msg(&mut *f, msg) {
        Ok(()) => true,
        Err(e) => {
            log::debug!("pipe write failed ({e}); connection is gone");
            false
        }
    }
}

fn run_search(
    state: Arc<ServiceState>,
    latest_gen: Arc<AtomicU64>,
    writer: Arc<Mutex<File>>,
    gen: u64,
    text: String,
    filters: Filters,
    max_results: u32,
) {
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
    let filtered = !exts.is_empty() || path_needle.is_some();
    // Over-fetch when filtering so the page can still fill after rejections.
    let fetch = if filtered {
        max.saturating_mul(8).clamp(max, 4096)
    } else {
        max
    };

    let match_ms;
    let mut items: Vec<ResultItem> = Vec::new();
    {
        let idx = state.index_read();
        let hits = idx_api::search(&idx, &text, fetch, &is_cancelled);
        match_ms = t0.elapsed().as_secs_f64() * 1e3;
        for h in hits {
            if items.len() >= max {
                break;
            }
            let name = match idx_api::name_of(&idx, h.frn) {
                Some(n) => n,
                None => continue, // entry vanished between match and assembly
            };
            // ext filter: case-insensitive suffix match on the name (§4.3).
            if !exts.is_empty() && !ext_matches(&name, &exts) {
                continue;
            }
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
    if send(&writer, &batch) {
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
