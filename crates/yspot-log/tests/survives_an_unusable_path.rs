//! A log directory that cannot be opened must cost the log, never the process.
//!
//! This is not hypothetical for either caller. The service writes under
//! `%ProgramData%`, which an unelevated dev run may not be able to create; the
//! shell writes under `%LOCALAPPDATA%`, which is missing on a stripped-down
//! account and redirected on a managed one. Refusing to start a launcher
//! because it could not open its log would be a far worse bug than the
//! missing log, so `init` degrades to stderr and says so.
//!
//! One test per file here, because `log` accepts one logger per process and
//! `init` returns early on a second call — so a second test sharing this
//! process would exercise nothing but that early return.

#[test]
fn an_unopenable_path_degrades_to_stderr_instead_of_panicking() {
    // A path under a FILE, so `create_dir_all` cannot succeed however the
    // platform feels about the name.
    let blocker = std::env::temp_dir().join(format!("yspot-log-blocker-{}", std::process::id()));
    std::fs::write(&blocker, b"not a directory").unwrap();
    let path = blocker.join("logs").join("shell.log");

    yspot_log::init("shell", env!("CARGO_PKG_VERSION"), Some(path.clone()));

    // The point: these do not panic, and the process is still here after.
    log::error!("still logging");
    log::logger().flush();

    assert!(
        !path.exists(),
        "the impossible path somehow produced a file"
    );

    let _ = std::fs::remove_file(&blocker);
}
