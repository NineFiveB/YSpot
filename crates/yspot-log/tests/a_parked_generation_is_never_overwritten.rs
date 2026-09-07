//! A parked generation that cannot be filed must not be run over.
//!
//! Rotation parks the live log in a pending file before creating its
//! replacement. If an earlier rotation was interrupted, a generation is
//! already sitting there. Recovery tries to file it into slot 1 — and when
//! something holds slot 1 open, that fails.
//!
//! The dangerous part is what came next. `rename` on Windows REPLACES, so
//! moving the live log into the pending file did not fail; it silently
//! overwrote the parked generation. No error, no log line, and the ten
//! megabytes destroyed were the newest history there was. Refusing the whole
//! rotation costs disk, which the caller then reports; going ahead cost data.
//!
//! Its own process, because `init` installs a logger once.

use std::path::{Path, PathBuf};

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yspot-log-noclobber-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_pending_generation_survives_a_rotation_that_cannot_file_it() {
    let dir = scratch();
    let live = dir.join("shell.log");

    // An interrupted rotation left this behind.
    std::fs::write(dir.join("shell.rotating.log"), "PARKED-GENERATION").unwrap();
    // And slot 1 is held open by something that does not share delete, so the
    // recovery cannot file the parked generation into it.
    std::fs::write(dir.join("shell.1.log"), "generation 1").unwrap();
    let held = hold_without_delete(&dir.join("shell.1.log"));

    // A live file already past the cap, so the first line wants to rotate.
    std::fs::write(&live, vec![b'x'; 11 * 1024 * 1024]).unwrap();

    yspot_log::init("shell", "0.0.0-test", Some(live.clone()));
    for i in 0..4 {
        log::error!("line {i} while the parked generation is stuck");
    }
    log::logger().flush();
    drop(held);

    assert_eq!(
        std::fs::read_to_string(dir.join("shell.rotating.log")).unwrap(),
        "PARKED-GENERATION",
        "the live log was moved on top of a generation that could not be filed"
    );
    // And the lines were still written rather than dropped.
    let text = std::fs::read_to_string(&live).unwrap();
    assert!(
        text.contains("line 3 while the parked generation is stuck"),
        "lines were thrown away because rotation was refused"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Share read and write but NOT delete, which is what an ordinary Windows
/// opener does and what blocks `MoveFileEx`.
#[cfg(windows)]
fn hold_without_delete(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .expect("could not hold slot 1 open")
}

#[cfg(not(windows))]
fn hold_without_delete(path: &Path) -> std::fs::File {
    std::fs::File::open(path).unwrap()
}
