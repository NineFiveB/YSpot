//! YSpot named-pipe transport (SPEC.md §4.1) — the one implementation of the
//! overlapped pipe that `yspot-indexd` serves and every client opens.
//!
//! Why overlapped, and why it is load-bearing rather than an optimization: a
//! pipe handle created without `FILE_FLAG_OVERLAPPED` is synchronous, and
//! Windows serializes synchronous I/O per FILE OBJECT. `File::try_clone`
//! duplicates a handle to the same file object, so a `WriteFile` from one
//! thread waits behind another thread's outstanding `ReadFile`. On a
//! request/reply pipe that read is waiting for the peer's next message,
//! which is waiting for the reply the write carries — issue #9, which
//! deadlocked every search on the service (dd228ee) and then the shell's
//! first search (0965adb). Both sides worked around it (inline searches; a
//! `PeekNamedPipe` poll) at the cost of §4.4 cancellation and up to 2 ms of
//! arrival latency. With `FILE_FLAG_OVERLAPPED` on the handle, every
//! `ReadFile`/`WriteFile` carries its own `OVERLAPPED` and completes
//! independently, so one thread can sit in a read forever while another
//! writes.
//!
//! Shape: a [`Pipe`] owns the handle and is shared through an `Arc`;
//! [`Pipe::split`] yields a [`PipeReader`] and a [`PipeWriter`], each with its
//! own manual-reset event, that implement the blocking `std::io::Read` and
//! `Write` traits by issuing an overlapped operation and waiting for that
//! operation's completion. "Blocking" here is a property of the calling
//! thread, not of the file object — the property the deadlock needed.
//!
//! Every operation waits for its own completion before returning, so the
//! `OVERLAPPED` never outlives the I/O it describes, and no thread ever exits
//! with an operation of its own in flight (thread exit cancels a thread's
//! pending I/O). `std::fs::File` must never wrap one of these handles: its
//! `Read`/`Write` pass a null `OVERLAPPED`, which on an overlapped handle is
//! undefined.

pub mod client;
mod handle;
mod io;
pub mod server;
pub mod sid;

pub use handle::Pipe;
pub use io::{Duplex, PipeReader, PipeWriter};
