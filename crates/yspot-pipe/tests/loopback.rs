//! Loopback tests over a real named pipe: a server instance and a client end in
//! this process, exercising exactly the threading shapes issue #9 deadlocked.
//!
//! Every wait is bounded, because the failure mode under test is a hang: with
//! a synchronous pipe the "write while another thread reads" cases below
//! block forever, and a test that blocks forever is a test that says nothing.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use yspot_pipe::{client, server, Duplex, Pipe};
use yspot_proto::{read_msg, write_msg, Message, MAX_FRAME_C2S, MAX_FRAME_S2C};

const WAIT: Duration = Duration::from_secs(10);

fn unique_name() -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        r"\\.\pipe\yspot-pipe-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// One accepted connection: the server's `Pipe` and the client's `Duplex`.
fn connected_pair() -> (std::sync::Arc<Pipe>, Duplex, client::ServerInfo) {
    let name = unique_name();
    let listener = server::create_instance(&name, None, true).expect("create instance");
    let client_thread = {
        let name = name.clone();
        thread::spawn(move || client::connect(&name).expect("client connect"))
    };
    server::accept(&listener).expect("accept");
    let c = client_thread.join().unwrap();
    let info = c.server;
    (listener, c.duplex().unwrap(), info)
}

#[test]
fn server_write_completes_while_server_read_is_pending() {
    // The dd228ee shape: the connection thread sits in ReadFile waiting for
    // the client's next message while a search thread writes the reply.
    let (srv, mut cli, _) = connected_pair();
    let (mut sr, mut sw) = srv.split().unwrap();

    let (read_tx, read_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 4];
        sr.read_exact(&mut buf).unwrap();
        read_tx.send(buf).unwrap();
    });
    // Let the read become pending before the write is issued.
    thread::sleep(Duration::from_millis(50));

    let (write_tx, write_rx) = mpsc::channel();
    let writer = thread::spawn(move || {
        sw.write_all(b"ping").unwrap();
        write_tx.send(()).unwrap();
    });
    write_rx
        .recv_timeout(WAIT)
        .expect("the write blocked behind the pending read — the synchronous-pipe deadlock");
    writer.join().unwrap();

    let mut buf = [0u8; 4];
    cli.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    cli.write_all(b"pong").unwrap();
    assert_eq!(&read_rx.recv_timeout(WAIT).unwrap(), b"pong");
    reader.join().unwrap();
}

#[test]
fn client_write_completes_while_client_read_is_pending() {
    // The 0965adb shape: the shell's read thread blocked in ReadFile while a
    // command thread writes the SearchQuery.
    let (srv, cli, _) = connected_pair();
    let (mut cr, mut cw) = cli.split();
    let (mut sr, mut sw) = srv.split().unwrap();

    let (read_tx, read_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let msg = read_msg(&mut cr, MAX_FRAME_S2C).unwrap();
        read_tx.send(msg).unwrap();
    });
    thread::sleep(Duration::from_millis(50));

    let (write_tx, write_rx) = mpsc::channel();
    let writer = thread::spawn(move || {
        write_msg(&mut cw, &Message::Cancel { gen: 7 }).unwrap();
        write_tx.send(()).unwrap();
    });
    write_rx
        .recv_timeout(WAIT)
        .expect("client write blocked behind the client's pending read");
    writer.join().unwrap();

    match read_msg(&mut sr, MAX_FRAME_C2S).unwrap() {
        Some(Message::Cancel { gen }) => assert_eq!(gen, 7),
        other => panic!("server read {other:?}"),
    }
    write_msg(&mut sw, &Message::Ack { id: 1 }).unwrap();
    match read_rx.recv_timeout(WAIT).unwrap() {
        Some(Message::Ack { id }) => assert_eq!(id, 1),
        other => panic!("client read {other:?}"),
    }
    reader.join().unwrap();
}

#[test]
fn frames_larger_than_the_pipe_buffer_cross_in_both_directions() {
    // 64 KiB pipe buffers each way; a 1 MiB frame forces the writer to block
    // on the reader draining, and the reader to assemble many completions.
    let (srv, mut cli, _) = connected_pair();
    let (mut sr, mut sw) = srv.split().unwrap();
    let big = "x".repeat(1 << 20);

    let payload = big.clone();
    let server_side = thread::spawn(move || {
        write_msg(
            &mut sw,
            &Message::Error {
                id: None,
                gen: None,
                code: 1,
                message: payload,
                retryable: false,
            },
        )
        .unwrap();
        read_msg(&mut sr, MAX_FRAME_C2S).unwrap()
    });

    match read_msg(&mut cli, MAX_FRAME_S2C).unwrap() {
        Some(Message::Error { message, .. }) => assert_eq!(message.len(), big.len()),
        other => panic!("client read {other:?}"),
    }
    write_msg(
        &mut cli,
        &Message::SearchQuery {
            id: 1,
            gen: 1,
            text: "y".repeat(1000),
            scopes: vec![],
            filters: Default::default(),
            max_results: 1,
        },
    )
    .unwrap();
    match server_side.join().unwrap() {
        Some(Message::SearchQuery { text, .. }) => assert_eq!(text.len(), 1000),
        other => panic!("server read {other:?}"),
    }
}

#[test]
fn peer_close_is_eof_for_the_reader_and_broken_pipe_for_the_writer() {
    let (srv, cli, _) = connected_pair();
    let (mut cr, mut cw) = cli.split();

    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 16];
        tx.send(cr.read(&mut buf).unwrap()).unwrap();
    });
    thread::sleep(Duration::from_millis(50));
    drop(srv); // server closes its end while the client read is pending
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), 0, "expected EOF");
    reader.join().unwrap();

    let err = cw.write_all(b"late").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
}

#[test]
fn cancel_all_unblocks_a_pending_read() {
    // The shell's reconnect path: abort whatever is pending on a dead-looking
    // connection without waiting for the peer.
    let (_srv, cli, _) = connected_pair();
    let pipe = cli.pipe().clone();
    let (mut cr, _cw) = cli.split();

    let (tx, rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 16];
        tx.send(cr.read(&mut buf).map_err(|e| e.raw_os_error()))
            .unwrap();
    });
    thread::sleep(Duration::from_millis(50));
    pipe.cancel_all();
    let outcome = rx
        .recv_timeout(WAIT)
        .expect("read did not return after cancel");
    // ERROR_OPERATION_ABORTED = 995
    assert_eq!(outcome, Err(Some(995)));
    reader.join().unwrap();
}

#[test]
fn accept_handles_a_client_that_connected_before_accept_was_called() {
    let name = unique_name();
    let listener = server::create_instance(&name, None, true).unwrap();
    let c = client::connect(&name).unwrap(); // connects immediately: instance exists
    thread::sleep(Duration::from_millis(20));
    server::accept(&listener).expect("ERROR_PIPE_CONNECTED must read as connected");
    let (mut sr, _sw) = listener.split().unwrap();
    let mut d = c.duplex().unwrap();
    d.write_all(b"hi").unwrap();
    let mut buf = [0u8; 2];
    sr.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hi");
}

#[test]
fn first_instance_flag_detects_a_squatter() {
    let name = unique_name();
    let _squatter = server::create_instance(&name, None, true).unwrap();
    let err = server::create_instance(&name, None, true).unwrap_err();
    // ERROR_ACCESS_DENIED = 5: the §4.1 squat signal.
    assert_eq!(err.raw_os_error(), Some(5));
    // Without the flag a further instance of our own pipe is fine.
    assert!(server::create_instance(&name, None, false).is_ok());
}

#[test]
fn verification_accepts_a_pipe_this_process_created() {
    // This process is the server, so the owner is this user (unelevated) or
    // Administrators (elevated runner) — never SYSTEM, always dev mode.
    let (_srv, _cli, info) = connected_pair();
    assert_eq!(info.pid, std::process::id());
    assert!(info.owner.is_dev_mode());
    assert!(matches!(
        info.owner,
        client::ServerOwner::CurrentUser | client::ServerOwner::Administrators
    ));
}

#[test]
fn connecting_to_a_missing_pipe_fails_fast() {
    let err = client::connect(&unique_name()).unwrap_err();
    // ERROR_FILE_NOT_FOUND = 2
    assert_eq!(err.raw_os_error(), Some(2));
}
