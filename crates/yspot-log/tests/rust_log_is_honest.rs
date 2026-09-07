//! A `RUST_LOG` this logger cannot read must say so, not shrug.
//!
//! The service used `env_logger` until §8.5's format landed, and `env_logger`
//! accepts per-module filters — `RUST_LOG=yspot_indexd=debug`. This logger
//! takes a bare level. Anyone reaching for the old spelling out of habit would
//! otherwise turn debug logging on, see none of it, and conclude the thing
//! they are chasing is not logged at all. That is the wrong conclusion, and it
//! is reached in silence, which is the part worth a test.
//!
//! Its own process: the logger is process-global and the level is set once.

use std::path::PathBuf;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yspot-log-rustlog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn an_unreadable_rust_log_is_reported_and_the_level_is_named() {
    let dir = scratch();
    let path = dir.join("indexd.log");

    // The env_logger spelling, which is the mistake this exists to catch.
    std::env::set_var("RUST_LOG", "yspot_indexd=debug");
    yspot_log::init("indexd", "0.0.0-test", Some(path.clone()));
    std::env::remove_var("RUST_LOG");

    log::debug!("a debug line nobody asked for correctly");
    log::logger().flush();

    let text = std::fs::read_to_string(&path).expect("no log file");
    let warning = text
        .lines()
        .find(|l| l.contains("RUST_LOG"))
        .unwrap_or_else(|| panic!("the ignored RUST_LOG was never mentioned:\n{text}"));

    let v: serde_json::Value = serde_json::from_str(warning).expect("not JSON");
    assert_eq!(v["level"], "warn", "an ignored setting is a warning");
    let msg = v["message"].as_str().unwrap();
    assert!(
        msg.contains("yspot_indexd=debug"),
        "the rejected value is not quoted back, so the reader cannot see their typo: {msg}"
    );
    assert!(
        msg.contains("INFO") || msg.contains("info"),
        "the level actually in force is not named: {msg}"
    );

    // And the setting really was ignored: debug must not have been enabled.
    assert!(
        !text.contains("a debug line nobody asked for correctly"),
        "the unparsed filter somehow took effect"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
