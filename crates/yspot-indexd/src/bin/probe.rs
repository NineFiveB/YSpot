//! Pipe-level latency probe — SPEC §10 M0 exit criteria, and the CI client.
//!
//! The matcher benchmark (`yspot-index --bin bench`) measures the index in
//! isolation: no pipe, no serialization, no service. §2.5's budget is stated
//! for the data actually reaching a client, so it needs measuring through the
//! transport that carries it. This connects to a running `yspot-indexd` exactly
//! as the shell does and times `SearchQuery` → final `SearchResults`.
//!
//! What this covers of the §2.5 decomposition: shell routing, the named pipe
//! both ways, MessagePack encode/decode, and the service-side match. What it
//! does NOT cover: the frontend's rAF application, which only the shell's own
//! HUD can report.
//!
//! `--burst N` is the §4.4 supersession check (issue #9): N queries are
//! written back-to-back before anything is read, the way a fast typist's
//! keystrokes arrive, and the reply stream must end with the LAST generation's
//! final batch, in generation order, with nothing from outside the burst. A
//! service that could not read while searching answered every one of these
//! in sequence; one that reads concurrently supersedes most of them before
//! they start. Exit 3 when the check fails.
//!
//!   probe [--queries a,b,c] [--iterations N] [--max-results N] [--burst N]

use std::io::Write;
use std::time::Instant;

use yspot_pipe::client::ServerOwner;
use yspot_pipe::Duplex;
use yspot_proto::{Filters, Message, MAX_FRAME_S2C, PIPE_NAME, PROTO_VERSION};

const DEFAULT_QUERIES: &[&str] = &["re", "rep", "report", "cargo", "index", "zzqxjv"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let queries: Vec<String> = match arg(&args, "--queries") {
        Some(v) => v.split(',').map(|s| s.trim().to_string()).collect(),
        None => DEFAULT_QUERIES.iter().map(|s| s.to_string()).collect(),
    };
    let iterations: usize = arg(&args, "--iterations")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let max_results: u32 = arg(&args, "--max-results")
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let burst: u64 = arg(&args, "--burst")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut pipe = match connect_and_handshake() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("probe: {e}");
            eprintln!("is yspot-indexd running? start it with --walk <path> or --mft <C:>");
            std::process::exit(1);
        }
    };

    // Probe a non-search message first: it is answered inline on the
    // connection thread, so it separates "reads messages at all" from
    // "replies to searches".
    {
        let req = Message::IndexStatusReq { id: 999 };
        yspot_proto::write_msg(&mut pipe, &req).expect("status write");
        match yspot_proto::read_msg(&mut pipe, MAX_FRAME_S2C) {
            Ok(Some(Message::IndexStatus { volumes, .. })) => {
                println!("status ok: {} volume(s)", volumes.len())
            }
            other => println!("status reply: {other:?}"),
        }
    }

    let mut gen = 0u64;
    if burst > 0 {
        if let Err(e) = burst_check(&mut pipe, &mut gen, burst, max_results) {
            eprintln!("probe: burst check FAILED: {e}");
            std::process::exit(3);
        }
    }

    println!("YSpot pipe latency probe — SPEC §2.5 / §10 M0");
    println!("{}", "=".repeat(78));
    println!(
        "Round trip is SearchQuery -> final SearchResults over {PIPE_NAME}: service match plus\n\
         both pipe crossings and both MessagePack conversions. The frontend's rAF application is\n\
         NOT included — that is the shell HUD's half of the §2.5 budget.\n"
    );
    println!(
        "{:<14} {:>9} {:>9} {:>9} {:>9} {:>9} {:>7}",
        "query", "min us", "p50 us", "p95 us", "p99 us", "max us", "hits"
    );
    println!("{}", "-".repeat(78));

    let mut worst_p95 = 0f64;
    for q in &queries {
        // One warm-up so the first row is not paying for a cold connection.
        gen += 1;
        let _ = round_trip(&mut pipe, gen, q, max_results);

        let mut us = Vec::with_capacity(iterations);
        let mut hits = 0usize;
        for _ in 0..iterations {
            gen += 1;
            match round_trip(&mut pipe, gen, q, max_results) {
                Ok((elapsed, n)) => {
                    us.push(elapsed);
                    hits = n;
                }
                Err(e) => {
                    eprintln!("probe: query {q:?} failed: {e}");
                    std::process::exit(2);
                }
            }
        }
        us.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| us[((us.len() as f64 - 1.0) * q).round() as usize];
        worst_p95 = worst_p95.max(p(0.95));
        println!(
            "{:<14} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>7}",
            q,
            us[0],
            p(0.50),
            p(0.95),
            p(0.99),
            us[us.len() - 1],
            hits
        );
    }

    println!("{}", "-".repeat(78));
    // §2.5 allots 20 ms to results reaching the frontend; the pipe round trip is
    // the part of that this can see, so it is a LOWER bound on the real figure.
    let verdict = if worst_p95 <= 20_000.0 {
        "PASS"
    } else {
        "FAIL"
    };
    println!(
        "worst p95 across queries: {worst_p95:.1} us — {verdict} against the 20 ms §2.5 budget"
    );
    println!("(lower bound: excludes the frontend's rAF application)");
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn search_query(gen: u64, text: &str, max_results: u32) -> Message {
    Message::SearchQuery {
        id: gen,
        gen,
        text: text.to_string(),
        scopes: Vec::new(),
        filters: Filters::default(),
        max_results,
    }
}

/// One `SearchQuery` and the wait for its final batch, in microseconds.
fn round_trip(
    pipe: &mut Duplex,
    gen: u64,
    text: &str,
    max_results: u32,
) -> Result<(f64, usize), String> {
    let t0 = Instant::now();
    yspot_proto::write_msg(pipe, &search_query(gen, text, max_results))
        .map_err(|e| e.to_string())?;
    let mut hits = 0usize;
    loop {
        match yspot_proto::read_msg(pipe, MAX_FRAME_S2C) {
            Ok(Some(Message::SearchResults {
                items, is_final, ..
            })) => {
                hits += items.len();
                if is_final {
                    return Ok((t0.elapsed().as_secs_f64() * 1e6, hits));
                }
            }
            Ok(Some(Message::Error { code, message, .. })) => {
                return Err(format!("service error {code}: {message}"))
            }
            Ok(Some(_)) => continue, // events and acks are not our reply
            Ok(None) => return Err("service closed the pipe".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// §4.4 supersession check: `n` generations written before any is read.
fn burst_check(pipe: &mut Duplex, gen: &mut u64, n: u64, max_results: u32) -> Result<(), String> {
    let first = *gen + 1;
    let last = *gen + n;
    *gen = last;
    // Successive prefixes of one word, the way keystrokes arrive.
    const WORD: &str = "repository";
    let t0 = Instant::now();
    for g in first..=last {
        let text = &WORD[..1 + ((g - first) as usize % WORD.len())];
        yspot_proto::write_msg(pipe, &search_query(g, text, max_results))
            .map_err(|e| format!("write gen {g}: {e}"))?;
    }
    pipe.flush().map_err(|e| e.to_string())?;

    let mut answered: Vec<u64> = Vec::new();
    loop {
        match yspot_proto::read_msg(pipe, MAX_FRAME_S2C) {
            Ok(Some(Message::SearchResults { gen, is_final, .. })) => {
                if gen < first || gen > last {
                    return Err(format!("gen {gen} is outside the burst {first}..={last}"));
                }
                if answered.last().is_some_and(|&prev| gen <= prev) {
                    return Err(format!(
                        "batches out of generation order: {answered:?} then {gen}"
                    ));
                }
                answered.push(gen);
                if gen == last {
                    if !is_final {
                        return Err("last generation's batch was not final".into());
                    }
                    break;
                }
            }
            Ok(Some(Message::Error { code, message, .. })) => {
                return Err(format!("service error {code}: {message}"))
            }
            Ok(Some(_)) => continue,
            Ok(None) => return Err("service closed the pipe mid-burst".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
    println!(
        "burst ok: {n} queries written back-to-back; {} generation(s) answered, {} superseded; \
         final gen {last} answered {:.1} ms after the first write",
        answered.len(),
        n - answered.len() as u64,
        t0.elapsed().as_secs_f64() * 1e3
    );
    Ok(())
}

/// §4.1 open + server verification, then Hello/HelloAck.
fn connect_and_handshake() -> Result<Duplex, String> {
    let conn = yspot_pipe::client::connect(PIPE_NAME).map_err(|e| format!("connect: {e}"))?;
    let server = conn.server;
    let mut pipe = conn.duplex().map_err(|e| format!("split: {e}"))?;
    let hello = Message::Hello {
        proto_min: PROTO_VERSION,
        proto_max: PROTO_VERSION,
        client: "yspot-probe".to_string(),
        pid: std::process::id(),
    };
    yspot_proto::write_msg(&mut pipe, &hello).map_err(|e| format!("hello: {e}"))?;
    match yspot_proto::read_msg(&mut pipe, MAX_FRAME_S2C) {
        Ok(Some(Message::HelloAck {
            proto,
            service_version,
            index_epoch,
        })) => {
            let owner = match server.owner {
                ServerOwner::System => "SYSTEM".to_string(),
                dev => format!("{dev:?} (dev mode)"),
            };
            println!(
                "connected: proto {proto}, service {service_version}, index epoch {index_epoch}, \
                 server pid {}, pipe owner {owner}",
                server.pid
            );
            Ok(pipe)
        }
        Ok(Some(other)) => Err(format!("unexpected handshake reply: {other:?}")),
        Ok(None) => Err("service closed the pipe during handshake".into()),
        Err(e) => Err(format!("handshake: {e}")),
    }
}
