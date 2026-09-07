//! A rotation that cannot happen must cost the size cap, never the history.
//!
//! The natural order — drop the oldest, shift the rest, rename the live file,
//! reopen it — does the irreversible work first and the failable work last. If
//! the reopen keeps failing (a full disk, a directory whose ACL changed,
//! Controlled Folder Access refusing the create), the caller retries on the
//! very next line, and the shift cascade walks all five generations into the
//! slot it deletes. Fifty megabytes of the only record of what went wrong,
//! gone in five log lines, silently, at exactly the moment it mattered.
//!
//! This drives the real logger against a directory it cannot rotate in, and
//! asserts the history is still there afterwards.

use std::path::{Path, PathBuf};

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yspot-log-rot-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Fill the live log past the cap, then make rotation impossible, then keep
/// logging. The four rotated generations must all survive.
#[test]
fn a_rotation_that_cannot_complete_keeps_every_generation() {
    let dir = scratch("keep");
    let live = dir.join("shell.log");

    // Four generations of history, as five-by-ten-megabyte rotation leaves.
    for i in 1..5 {
        std::fs::write(
            dir.join(format!("shell.{i}.log")),
            format!("generation {i}"),
        )
        .unwrap();
    }

    // The live file, already over the cap so the next write wants to rotate.
    std::fs::write(&live, vec![b'x'; 11 * 1024 * 1024]).unwrap();

    // Make the rename impossible by holding the live file open without
    // FILE_SHARE_DELETE, which is what an editor or a backup agent does.
    let held = hold_exclusively(&live);

    yspot_log::init("shell", "0.0.0-test", Some(live.clone()));
    for i in 0..8 {
        log::error!("line {i} while rotation is blocked");
    }
    log::logger().flush();
    drop(held);

    for i in 1..5 {
        let g = dir.join(format!("shell.{i}.log"));
        assert!(
            g.exists(),
            "shell.{i}.log was destroyed by a rotation that never completed"
        );
        assert_eq!(
            std::fs::read_to_string(&g).unwrap(),
            format!("generation {i}"),
            "shell.{i}.log was overwritten by the shift cascade"
        );
    }

    // And the lines were kept rather than dropped: an oversized log beats no
    // log, which is the whole trade this makes.
    let text = std::fs::read_to_string(&live).unwrap();
    for i in 0..8 {
        assert!(
            text.contains(&format!("line {i} while rotation is blocked")),
            "line {i} was thrown away because rotation failed"
        );
    }
    // And it says why it is oversized, in the file, where the reader is.
    assert!(
        text.contains("could not rotate"),
        "an oversized log with no explanation in it"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Open the file in a way that blocks `MoveFileEx`: share read and write but
/// NOT delete, which is what ordinary Windows openers do.
#[cfg(windows)]
fn hold_exclusively(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
        .expect("could not hold the log open")
}

#[cfg(not(windows))]
fn hold_exclusively(path: &Path) -> std::fs::File {
    std::fs::File::open(path).unwrap()
}
