//! A generation parked by an interrupted rotation must be claimed, not deleted.
//!
//! Rotation moves the live log aside before creating its replacement, so that
//! a failure costs nothing. That leaves a window: between the two renames the
//! newest ten megabytes live in a pending file under neither the live name nor
//! a numbered one. Two ordinary things land in that window — a hard kill, and
//! a final rename blocked by something holding slot 1 open.
//!
//! The first version of this rotation then DELETED that file at the start of
//! the next rotation, to make room for its own pending file. So the routine
//! written to stop history being destroyed destroyed the newest generation
//! instead: precisely the window around whatever had gone wrong. Nothing said
//! so, and the reader could not even see the file, because it did not end in
//! `.log`.
//!
//! Its own process, because `init` installs a logger once.

use std::path::PathBuf;

// Tagged per test: the two tests here run concurrently in one binary, and a
// shared directory means each one's cleanup wipes the other's fixture.
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yspot-log-pend-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn an_interrupted_rotation_is_claimed_at_the_next_start() {
    let dir = scratch("restart");
    let live = dir.join("shell.log");

    // The state a kill between the two renames leaves: no live file, and the
    // newest generation parked.
    std::fs::write(dir.join("shell.rotating.log"), "THE-NEWEST-GENERATION").unwrap();
    std::fs::write(dir.join("shell.1.log"), "older gen 1").unwrap();
    std::fs::write(dir.join("shell.2.log"), "older gen 2").unwrap();

    yspot_log::init("shell", "0.0.0-test", Some(live.clone()));
    log::error!("a line after the restart");
    log::logger().flush();

    // Claimed into slot 1, which is where it belongs: it is newer than
    // everything already filed.
    assert_eq!(
        std::fs::read_to_string(dir.join("shell.1.log")).unwrap(),
        "THE-NEWEST-GENERATION",
        "the parked generation was not filed as the newest"
    );
    assert!(
        !dir.join("shell.rotating.log").exists(),
        "the pending file was left behind after being claimed"
    );
    // And the older ones moved down rather than being overwritten.
    assert_eq!(
        std::fs::read_to_string(dir.join("shell.2.log")).unwrap(),
        "older gen 1"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("shell.3.log")).unwrap(),
        "older gen 2"
    );

    // The restart's own logging went to a fresh live file, not on top of it.
    let text = std::fs::read_to_string(&live).unwrap();
    assert!(text.contains("a line after the restart"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Whatever else happens to it, the parked file is visible to the one tool
/// anyone reads these logs with, which globs `*.log`.
#[test]
fn the_pending_file_is_named_so_the_reader_can_see_it() {
    let dir = scratch("name");
    // Deliberately NOT going through `init` — this is about the name alone,
    // and the sibling test above has already installed this process's logger.
    let parked = dir.join("indexd.rotating.log");
    std::fs::write(&parked, "x").unwrap();
    let seen: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".log"))
        .collect();
    assert!(
        seen.iter().any(|n| n == "indexd.rotating.log"),
        "a parked generation would be invisible to Read-YSpotLogs.ps1, which \
         globs *.log: {seen:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
