//! A generation the rotation could not file must still be a file the reader
//! can find.
//!
//! When the final step of a rotation fails, the live log stays parked in the
//! pending file. That is the right outcome — far better than overwriting
//! something — but only if the parked file is discoverable. The one tool
//! anyone reads these logs with globs `*.log`, so a pending file named
//! anything else is a generation that does not exist to the person looking
//! for it.
//!
//! The test this replaces asserted that a filename it had itself written ended
//! in `.log`, which is a fact about the test and not about the logger. Here the
//! rotation is driven for real and the resulting name comes from production
//! code.
//!
//! Its own process, because `init` installs a logger once.

use std::path::{Path, PathBuf};

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yspot-log-stuck-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_generation_that_could_not_be_filed_is_still_a_dot_log() {
    let dir = scratch();
    let live = dir.join("shell.log");

    // Slot 1 is occupied and held open without delete sharing, so the shift
    // cannot vacate it and the final claim will fail. Nothing is parked yet —
    // the rotation itself is what creates the pending file.
    std::fs::write(dir.join("shell.1.log"), "generation 1").unwrap();
    let held = hold_without_delete(&dir.join("shell.1.log"));
    std::fs::write(&live, vec![b'x'; 11 * 1024 * 1024]).unwrap();

    yspot_log::init("shell", "0.0.0-test", Some(live.clone()));
    log::error!("the line that triggers the rotation");
    log::logger().flush();
    drop(held);

    // Whatever the logger decided to call it, the reader's glob must match.
    let logs: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let parked: Vec<&String> = logs
        .iter()
        .filter(|n| n.contains("rotating") || n.contains("pending"))
        .collect();
    for p in &parked {
        assert!(
            p.ends_with(".log"),
            "a parked generation named {p:?} is invisible to Read-YSpotLogs.ps1, \
             which globs *.log; all files present: {logs:?}"
        );
    }

    // And every byte is still accounted for: nothing was dropped on the floor
    // by a rotation that could not complete.
    let total: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".log"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    assert!(
        total >= 11 * 1024 * 1024,
        "bytes went missing during a failed rotation: {total} left"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Share read and write but NOT delete, which blocks `MoveFileEx`.
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
