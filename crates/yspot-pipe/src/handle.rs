//! Owned handle wrappers: the pipe itself and the per-operation events.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr::{null, null_mut};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};
use windows_sys::Win32::System::IO::CancelIoEx;

use crate::io::{PipeReader, PipeWriter};

/// One end of a named pipe, opened (or created) with `FILE_FLAG_OVERLAPPED`.
///
/// Shared through an `Arc` by the reader and writer halves; the handle closes
/// when the last half drops, which is also the point at which no operation
/// can be pending on it (every operation completes inside the call that
/// issued it).
#[derive(Debug)]
pub struct Pipe {
    handle: OwnedHandle,
}

impl Pipe {
    /// Take ownership of a raw pipe handle.
    ///
    /// # Safety
    /// `h` must be a valid, open handle to a pipe end created with
    /// `FILE_FLAG_OVERLAPPED`, owned by nobody else; it is closed on drop.
    pub(crate) unsafe fn from_raw(h: HANDLE) -> Pipe {
        debug_assert!(!h.is_null() && h != INVALID_HANDLE_VALUE);
        Pipe {
            // SAFETY: the caller guarantees a valid, exclusively owned handle.
            handle: unsafe { OwnedHandle::from_raw_handle(h as _) },
        }
    }

    pub fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle() as HANDLE
    }

    /// Split into independently usable halves, each with its own completion
    /// event. The halves may live on different threads; a pending read on one
    /// never blocks a write on the other.
    pub fn split(self: &Arc<Pipe>) -> io::Result<(PipeReader, PipeWriter)> {
        Ok((
            PipeReader::new(self.clone(), Event::new()?),
            PipeWriter::new(self.clone(), Event::new()?),
        ))
    }

    /// Abort every operation pending on this pipe end, from any thread of this
    /// process. The aborted operation completes with
    /// `ERROR_OPERATION_ABORTED` inside the call that issued it, so the thread
    /// blocked there returns an error and can release the pipe.
    pub fn cancel_all(&self) {
        // SAFETY: valid handle; null OVERLAPPED means "all operations on this
        // handle", the documented form.
        unsafe { CancelIoEx(self.raw(), null()) };
    }
}

/// A manual-reset, initially non-signaled event for `OVERLAPPED.hEvent`.
///
/// Manual-reset is what `GetOverlappedResult` is documented against; the
/// kernel clears it when an operation starts and sets it on completion, and
/// [`Event::reset`] clears it again by hand before reuse so a completion that
/// finished synchronously cannot be mistaken for the next one's.
#[derive(Debug)]
pub(crate) struct Event(OwnedHandle);

impl Event {
    pub(crate) fn new() -> io::Result<Event> {
        // SAFETY: no security attributes, manual reset, non-signaled, unnamed.
        let h = unsafe { CreateEventW(null(), 1, 0, null_mut()) };
        if h.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fresh handle from CreateEventW, owned here.
        Ok(Event(unsafe { OwnedHandle::from_raw_handle(h as _) }))
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }

    pub(crate) fn reset(&self) {
        // SAFETY: valid event handle owned by self.
        unsafe { ResetEvent(self.raw()) };
    }
}
