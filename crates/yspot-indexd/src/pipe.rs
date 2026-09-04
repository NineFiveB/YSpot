//! Named-pipe server (SPEC §4.1).
//!
//! Instances are created with `FILE_FLAG_OVERLAPPED` and accepted with an
//! overlapped `ConnectNamedPipe` (issue #9); the primitive lives in
//! `yspot-pipe`, shared with every client. Each accepted connection gets a
//! "pipe-conn" thread running [`session::run`], and the listening instance is
//! re-armed on this thread after each accept. A client connecting in that gap
//! sees `ERROR_PIPE_BUSY` and retries via `WaitNamedPipe` (retry behavior
//! §4.1 already mandates for clients).
//!
//! M0/M1 console-mode deviation, deliberate and documented: unelevated dev
//! runs cannot assert the SDDL's `O:SY` owner (`CreateNamedPipeW` fails with
//! `ERROR_INVALID_OWNER`); the pipe is then created with a descriptor that
//! keeps the same DACL and integrity label without the owner/group prefix, so
//! it stays ACL-hardened and only the owner differs. Clients see that owner
//! and log it as dev mode (`yspot_pipe::client::ServerOwner`).
//!
//! Squat detection (§4.1) is honored: the first instance carries
//! `FILE_FLAG_FIRST_PIPE_INSTANCE`, and `ERROR_ACCESS_DENIED` on that create
//! logs a security event and refuses to start.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_OWNER};
use yspot_pipe::server::{self, SecDesc};

use crate::session;
use crate::state::ServiceState;

/// How many client connections are served at once.
///
/// §4.1's DACL grants interactive users open rights, so the number of
/// connections is the number of processes on the machine that feel like
/// opening one — and each costs two threads (the connection reader and its
/// search worker) plus whatever its queued batch holds. Unbounded, a loop
/// calling `CreateFile` in a script takes the service down for every real
/// client; capped, the newest connection is refused and the ones already
/// being served keep working.
///
/// Sized for the real population with room to spare: one shell per interactive
/// session, plus the probe and harness during development.
const MAX_CONNECTIONS: usize = 32;

/// Occupies one connection slot until dropped.
///
/// A guard, so a session thread that panics or returns early gives its slot
/// back — a leaked slot is permanent, and enough of them close the service to
/// new clients with no way back short of a restart.
struct ConnSlot(Arc<AtomicUsize>);

impl ConnSlot {
    /// `None` when the service is already at [`MAX_CONNECTIONS`].
    fn try_acquire(live: &Arc<AtomicUsize>) -> Option<ConnSlot> {
        let mut n = live.load(Ordering::SeqCst);
        loop {
            if n >= MAX_CONNECTIONS {
                return None;
            }
            match live.compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(ConnSlot(live.clone())),
                Err(actual) => n = actual,
            }
        }
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Serve the pipe forever. Returns only on a fatal startup error; exits the
/// process directly on squat detection (§4.1).
pub fn serve(state: Arc<ServiceState>) -> anyhow::Result<()> {
    let name = yspot_proto::PIPE_NAME;

    let mut secdesc = SecDesc::from_sddl(yspot_proto::PIPE_SDDL);
    if secdesc.is_none() {
        log::warn!(
            "PIPE_SDDL did not convert; pipe will use DEFAULT security — M0 DEV MODE ONLY, \
             the pipe is NOT ACL-hardened (SPEC §4.1). Do not ship this configuration."
        );
    }

    // First instance carries FILE_FLAG_FIRST_PIPE_INSTANCE — the squat check.
    let mut pipe = match server::create_instance(name, secdesc.as_ref(), true) {
        Ok(p) => p,
        Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => squat_refusal(),
        // Only SYSTEM may assign SYSTEM as owner, so an unelevated dev run
        // fails the full descriptor with ERROR_INVALID_OWNER. Fall back to the
        // same DACL and integrity label without the owner/group prefix — still
        // ACL-hardened, only the owner differs.
        Err(err) if err.raw_os_error() == Some(ERROR_INVALID_OWNER as i32) && secdesc.is_some() => {
            log::warn!(
                "CreateNamedPipeW with the SPEC §4.1 SDDL failed (ERROR_INVALID_OWNER): this \
                 process is not SYSTEM. Retrying with the same DACL minus the O:SY/G:SY prefix — \
                 the pipe stays ACL-hardened, but its owner is this user, so clients cannot \
                 perform the §4.1 SYSTEM-owner check. DEV MODE."
            );
            secdesc = dev_sddl()
                .as_deref()
                .and_then(SecDesc::from_sddl)
                .or_else(|| SecDesc::from_sddl(yspot_proto::PIPE_SDDL_NO_OWNER));
            match server::create_instance(name, secdesc.as_ref(), true) {
                Ok(p) => p,
                Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) => squat_refusal(),
                Err(e) => anyhow::bail!("CreateNamedPipeW (dev descriptor) failed: {e}"),
            }
        }
        Err(err) => anyhow::bail!("CreateNamedPipeW failed: {err}"),
    };
    log::info!("pipe server listening on {name}");

    let live = Arc::new(AtomicUsize::new(0));

    loop {
        match server::accept(&pipe) {
            Ok(()) => match ConnSlot::try_acquire(&live) {
                Some(slot) => {
                    let st = state.clone();
                    if let Err(e) =
                        std::thread::Builder::new()
                            .name("pipe-conn".into())
                            .spawn(move || {
                                // Moved in so the slot is held for exactly the life
                                // of the session, released even on an unwind.
                                let _slot = slot;
                                session::run(pipe, st)
                            })
                    {
                        // The moved `pipe` is dropped with the failed closure;
                        // the client sees a disconnect.
                        log::error!("connection thread spawn failed: {e}");
                    }
                }
                None => {
                    // Dropping the instance disconnects this client, which is
                    // the honest answer: it retries per §4.1 and gets in when
                    // a slot frees. Worth an error — at the cap something is
                    // either wrong or hostile.
                    log::error!(
                        "refusing connection: already serving {MAX_CONNECTIONS} clients,                          the §4.1 cap"
                    );
                    drop(pipe);
                }
            },
            Err(e) => {
                log::debug!("ConnectNamedPipe failed ({e}); recycling instance");
                drop(pipe);
            }
        }

        // Re-arm a fresh listening instance — WITHOUT the first-instance flag
        // (§4.1: with it, every later create would itself fail ACCESS_DENIED).
        pipe = loop {
            match server::create_instance(name, secdesc.as_ref(), false) {
                Ok(p) => break p,
                Err(e) => {
                    log::error!("re-arm CreateNamedPipeW failed ({e}); retrying in 200 ms");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        };
    }
}

/// Dev-mode security descriptor: the §4.1 DACL plus full control for the user
/// this process is running as.
///
/// Without that extra ACE an unelevated service can create the FIRST pipe
/// instance and no others. `PIPE_SDDL_NO_OWNER` grants `GA` to SYSTEM and
/// Administrators — neither of which an ordinary dev process is — and gives
/// interactive users `0x12019B`, which deliberately withholds
/// `FILE_CREATE_PIPE_INSTANCE` so a client cannot stand up a rogue instance of
/// our name. Creating a second instance is checked against the existing pipe's
/// DACL, so the service matched only the client ACE and got
/// `ERROR_ACCESS_DENIED` forever: it served exactly one connection and then
/// spun. In production the service IS SYSTEM, so the SY ace covers it and this
/// path never runs.
///
/// Other interactive users keep the reduced mask, so this widens nothing for
/// anyone but the process itself.
fn dev_sddl() -> Option<String> {
    let sid = yspot_pipe::sid::current_user_sid()?.to_string_sid()?;
    Some(format!(
        "D:P(A;;GA;;;{sid})(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019B;;;IU)S:(ML;;NW;;;ME)"
    ))
}

fn squat_refusal() -> ! {
    // §4.1: another process owns our name — log a security event, refuse to
    // start degraded.
    log::error!(
        "SECURITY EVENT: first CreateNamedPipeW(FILE_FLAG_FIRST_PIPE_INSTANCE) failed with \
         ERROR_ACCESS_DENIED — another process has squatted {}; refusing to start (SPEC §4.1)",
        yspot_proto::PIPE_NAME
    );
    std::process::exit(10);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_are_capped_and_slots_come_back() {
        let live = Arc::new(AtomicUsize::new(0));
        let held: Vec<ConnSlot> = (0..MAX_CONNECTIONS)
            .map(|_| ConnSlot::try_acquire(&live).expect("under the cap"))
            .collect();
        assert_eq!(live.load(Ordering::SeqCst), MAX_CONNECTIONS);
        // The one that would have been the (cap + 1)th is refused, not queued.
        assert!(ConnSlot::try_acquire(&live).is_none());

        drop(held);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert!(ConnSlot::try_acquire(&live).is_some());
    }

    #[test]
    fn a_session_that_panics_does_not_leak_its_slot() {
        // A leaked slot is permanent: enough of them and the service refuses
        // every client with nothing short of a restart to clear it.
        let live = Arc::new(AtomicUsize::new(0));
        let caught = std::panic::catch_unwind({
            let live = live.clone();
            move || {
                let _slot = ConnSlot::try_acquire(&live).expect("acquired");
                panic!("session blew up");
            }
        });
        assert!(caught.is_err());
        assert_eq!(live.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn dev_descriptor_carries_the_current_user_and_converts() {
        let sddl = dev_sddl().expect("current user SID");
        assert!(sddl.contains("S-1-5-"));
        assert!(sddl.ends_with("S:(ML;;NW;;;ME)"));
        assert!(SecDesc::from_sddl(&sddl).is_some());
    }
}
