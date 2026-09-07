//! The whole point, end to end: a `log::info!` reaches the file on disk.
//!
//! The unit tests cover the line format and the rotation, but both call
//! `format_line` and `Rotating` directly. Neither would notice if `init` wired
//! them up wrongly — installed a logger at the wrong level, opened the wrong
//! path, or failed to install at all — and that is the failure that matters,
//! because it is silent. M1.md records the shell shipping exactly that bug:
//! its log "went nowhere the moment it was not launched from a terminal",
//! which is every real run, and nothing failed loudly enough to say so.
//!
//! An integration test gets its own process, which this needs: `log` accepts
//! one logger per process, so `init` can only be honestly exercised once.

use std::path::PathBuf;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yspot-log-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_logged_line_lands_in_the_file_at_the_given_path() {
    let dir = scratch();
    // Deliberately a path whose parent does not exist: the service writes
    // under %ProgramData%\YSpot\logs, and on a first run neither the YSpot
    // directory nor logs beneath it is there yet.
    let path = dir.join("nested").join("indexd.log");
    assert!(!path.exists());

    yspot_log::init("indexd", "9.9.9-test", Some(path.clone()));

    log::info!("hello from the test");
    log::warn!(target: "yspot_indexd::pipe", "and a warning");
    // Below the default level, so it must NOT appear: this is what proves the
    // level was actually applied rather than left at the `log` default of Off
    // (which would have failed the assertions above) or Trace.
    log::debug!("this should not be written");
    log::logger().flush();

    let text = std::fs::read_to_string(&path).expect("the log file was never created");
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        3,
        "expected the banner and two records, got:\n{text}"
    );

    // The banner opens the run. Without it, a fortnight of logons, crashes and
    // rebuilds is one undifferentiated stream, and §8.5's own crash-free
    // metric — counted per shell process lifetime — cannot be counted at all.
    let banner: serde_json::Value = serde_json::from_str(lines[0]).expect("banner is not JSON");
    let b = banner["message"].as_str().unwrap();
    assert!(b.starts_with(yspot_log::RUN_START), "{b}");
    assert!(b.contains("9.9.9-test"), "the version is missing: {b}");
    assert!(
        b.contains(&std::process::id().to_string()),
        "the pid is missing: {b}"
    );
    assert!(
        b.contains("indexd.log"),
        "the resolved path is missing, and it is what answers where the logs are: {b}"
    );

    let first: serde_json::Value = serde_json::from_str(lines[1]).expect("line 2 is not JSON");
    assert_eq!(first["message"], "hello from the test");
    assert_eq!(first["level"], "info");
    assert_eq!(
        first["process"], "indexd",
        "the process name init was given did not reach the line"
    );

    let second: serde_json::Value = serde_json::from_str(lines[2]).expect("line 3 is not JSON");
    assert_eq!(second["level"], "warn");
    assert_eq!(second["component"], "yspot_indexd::pipe");

    assert!(
        !text.contains("this should not be written"),
        "a debug record was written at the default level"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
