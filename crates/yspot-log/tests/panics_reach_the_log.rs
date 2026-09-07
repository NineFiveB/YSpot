//! A panic must leave a line in the FILE, not only on stderr.
//!
//! std's default hook writes to stderr and nowhere else, which is how the most
//! informative line a process ever produces becomes the one line its log does
//! not have. For the service that stderr is a console that scrolls away and
//! dies with its window; for a GUI shell it is not connected to anything at
//! all. Either way the log's last entry is whatever happened just before, and
//! the crash itself is invisible to anyone reading afterwards.
//!
//! Its own process: the hook and the logger are both process-global.

use std::path::PathBuf;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yspot-log-panic-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_panic_is_written_to_the_log_file() {
    let dir = scratch();
    let path = dir.join("indexd.log");
    yspot_log::init("indexd", env!("CARGO_PKG_VERSION"), Some(path.clone()));
    yspot_log::install_panic_hook();

    // `catch_unwind` still runs the hook, which is what lets this be a test
    // rather than a dead process. The thread is named, because the name is
    // most of the diagnosis in the case this exists for: a panic on the USN
    // tailer freezes the index, a panic on a session thread costs one query.
    let handle = std::thread::Builder::new()
        .name("usn-tail".into())
        .spawn(|| {
            let _ = std::panic::catch_unwind(|| {
                panic!("journal read blew up");
            });
        })
        .unwrap();
    handle.join().unwrap();
    log::logger().flush();

    let text = std::fs::read_to_string(&path).expect("no log file");
    let panic_line = text
        .lines()
        .find(|l| l.contains("PANIC"))
        .unwrap_or_else(|| panic!("the panic never reached the log:\n{text}"));

    let v: serde_json::Value = serde_json::from_str(panic_line).expect("not JSON");
    assert_eq!(
        v["level"], "error",
        "a panic must not be logged below error"
    );
    let msg = v["message"].as_str().unwrap();
    assert!(
        msg.contains("journal read blew up"),
        "the payload was lost: {msg}"
    );
    assert!(
        msg.contains("usn-tail"),
        "the thread name was lost, which is the diagnosis: {msg}"
    );
    assert!(
        msg.contains("panics_reach_the_log.rs"),
        "the location was lost: {msg}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
