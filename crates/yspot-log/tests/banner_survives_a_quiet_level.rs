//! `RUST_LOG=warn` must not take the run boundary with it.
//!
//! Turning the level down is an ordinary thing to do to a noisy tool. It used
//! to also delete every run boundary from the file, because the banner went
//! through `log::info!` and the macros consult `max_level`. That is a silent
//! trade nobody agreed to: the reader loses the ability to tell one run from
//! the next across a fortnight, and §8.5's crash-free metric — counted per run
//! — becomes uncomputable, in exchange for slightly less noise.
//!
//! Its own process, because the level is set once per process.

use std::path::PathBuf;

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("yspot-log-quiet-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn the_run_boundary_survives_a_level_that_would_filter_it() {
    let dir = scratch();
    let path = dir.join("shell.log");

    std::env::set_var("RUST_LOG", "warn");
    yspot_log::init("shell", "7.7.7-test", Some(path.clone()));
    std::env::remove_var("RUST_LOG");

    // Ordinary info is still filtered — the level was genuinely applied, and
    // the banner is an exception rather than a hole in the filter.
    log::info!("routine chatter nobody asked for");
    log::warn!("something worth hearing");
    log::logger().flush();

    let text = std::fs::read_to_string(&path).expect("no log file");
    let banner = text
        .lines()
        .find(|l| l.contains(yspot_log::RUN_START))
        .unwrap_or_else(|| panic!("RUST_LOG=warn deleted the run boundary:\n{text}"));

    let v: serde_json::Value = serde_json::from_str(banner).expect("not JSON");
    assert_eq!(
        v["level"], "info",
        "the banner changed level to sneak through"
    );
    let msg = v["message"].as_str().unwrap();
    assert!(msg.contains("7.7.7-test"), "{msg}");

    assert!(
        !text.contains("routine chatter"),
        "the level was not applied at all, so this test proves nothing:\n{text}"
    );
    assert!(
        text.contains("something worth hearing"),
        "warn-level records went missing too:\n{text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
