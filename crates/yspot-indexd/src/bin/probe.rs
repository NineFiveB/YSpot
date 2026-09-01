//! Pipe-level latency probe — SPEC §10 M0 exit criteria.
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
//!   probe [--queries a,b,c] [--iterations N] [--max-results N]

use std::fs::File;
use std::io;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::time::Instant;

use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_BUSY, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;

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

    let mut gen = 0u64;
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

/// One `SearchQuery` and the wait for its final batch, in microseconds.
fn round_trip(
    pipe: &mut File,
    gen: u64,
    text: &str,
    max_results: u32,
) -> Result<(f64, usize), String> {
    let q = Message::SearchQuery {
        id: gen,
        gen,
        text: text.to_string(),
        scopes: Vec::new(),
        filters: Filters::default(),
        max_results,
    };
    let t0 = Instant::now();
    yspot_proto::write_msg(pipe, &q).map_err(|e| e.to_string())?;
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

fn connect_and_handshake() -> Result<File, String> {
    let mut pipe = connect_pipe().map_err(|e| format!("connect: {e}"))?;
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
            println!(
                "connected: proto {proto}, service {service_version}, index epoch {index_epoch}"
            );
            Ok(pipe)
        }
        Ok(Some(other)) => Err(format!("unexpected handshake reply: {other:?}")),
        Ok(None) => Err("service closed the pipe during handshake".into()),
        Err(e) => Err(format!("handshake: {e}")),
    }
}

/// §4.1 client rules: explicit access rights (never `GENERIC_WRITE`, which
/// carries `FILE_APPEND_DATA` and is refused by the pipe's DACL), identification
/// SQOS, and a bounded `ERROR_PIPE_BUSY` retry.
fn connect_pipe() -> io::Result<File> {
    let name: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut attempts = 0u32;
    loop {
        // SAFETY: `name` is a valid NUL-terminated UTF-16 string; the remaining
        // arguments are plain values and permitted null pointers.
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
            // SAFETY: freshly opened owned handle; ownership moves once.
            return Ok(unsafe { File::from_raw_handle(handle as RawHandle) });
        }
        // SAFETY: trivially safe thread-local read.
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_BUSY && attempts < 5 {
            attempts += 1;
            // SAFETY: same valid pipe name; 100 ms per §4.1.
            let _ = unsafe { WaitNamedPipeW(name.as_ptr(), 100) };
            continue;
        }
        return Err(io::Error::from_raw_os_error(err as i32));
    }
}
