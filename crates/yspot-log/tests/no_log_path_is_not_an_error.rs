//! A process with nowhere to write a log must still run, and still log.
//!
//! `None` is the honest case for a process that cannot build a log directory
//! at all — no environment variable to build one from. Refusing to start a
//! launcher over that would be a far worse bug than the missing log.
//!
//! This lives in its own file, and therefore its own process, because it used
//! to sit beside the unopenable-path test and share one. Once `init` grew an
//! idempotence guard that returns before touching anything, the second call in
//! a process did nothing at all — so the test passed by reaching a bare
//! `return`, proving only that an early return does not panic. A test whose
//! subject has been optimised out from under it is worse than no test: it
//! still reports success.

#[test]
fn no_path_at_all_is_not_an_error() {
    yspot_log::init("shell", env!("CARGO_PKG_VERSION"), None);

    // The point: this does not panic, there is nothing to write it to, and the
    // process is still here afterwards.
    log::error!("still logging with nowhere to log");
    log::logger().flush();

    // And the logger really was installed, so calls go somewhere defined
    // rather than to the no-op default. `set_boxed_logger` would refuse a
    // second install, which is what this asserts against.
    assert!(
        log::log_enabled!(log::Level::Error),
        "no logger was installed, so this test never exercised init at all"
    );
}
