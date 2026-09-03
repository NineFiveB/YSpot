//! Overlapped `ReadFile`/`WriteFile` behind blocking `Read`/`Write`.

use std::io::{self, ErrorKind, Read, Write};
use std::sync::Arc;

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
    HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::handle::{Event, Pipe};

/// `GetOverlappedResult` reports this when the operation is still pending —
/// only possible with `bWait = FALSE`, which nothing here uses; handled anyway
/// because returning while the kernel still owns a stack `OVERLAPPED` would be
/// memory corruption, not a mere error.
const ERROR_IO_INCOMPLETE: u32 = 996;

/// The read half of a [`Pipe`]. `Read::read` issues one overlapped `ReadFile`
/// and waits for its completion; EOF (`Ok(0)`) is reported when the peer has
/// closed or disconnected its end.
#[derive(Debug)]
pub struct PipeReader {
    pipe: Arc<Pipe>,
    ev: Event,
}

/// The write half of a [`Pipe`]. `Write::write` issues one overlapped
/// `WriteFile` and waits for its completion; a peer that has gone away
/// surfaces as `ErrorKind::BrokenPipe`.
#[derive(Debug)]
pub struct PipeWriter {
    pipe: Arc<Pipe>,
    ev: Event,
}

impl PipeReader {
    pub(crate) fn new(pipe: Arc<Pipe>, ev: Event) -> Self {
        PipeReader { pipe, ev }
    }

    pub fn pipe(&self) -> &Arc<Pipe> {
        &self.pipe
    }
}

impl PipeWriter {
    pub(crate) fn new(pipe: Arc<Pipe>, ev: Event) -> Self {
        PipeWriter { pipe, ev }
    }

    pub fn pipe(&self) -> &Arc<Pipe> {
        &self.pipe
    }
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        overlapped_read(self.pipe.raw(), &self.ev, buf)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        overlapped_write(self.pipe.raw(), &self.ev, buf)
    }

    /// Pipes have no userland buffer to flush; a completed `WriteFile` is in
    /// the pipe's buffer or on the peer's side already.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Both halves in one value, for single-threaded request/reply clients that
/// want a plain `Read + Write` object (the probe, the M0 harness).
#[derive(Debug)]
pub struct Duplex {
    pub reader: PipeReader,
    pub writer: PipeWriter,
}

impl Duplex {
    pub fn pipe(&self) -> &Arc<Pipe> {
        self.reader.pipe()
    }

    pub fn split(self) -> (PipeReader, PipeWriter) {
        (self.reader, self.writer)
    }
}

impl Read for Duplex {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buf)
    }
}

impl Write for Duplex {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// A zeroed `OVERLAPPED` (offsets must be zero for pipes) carrying `ev`.
fn overlapped_for(ev: &Event) -> OVERLAPPED {
    // SAFETY: OVERLAPPED is plain data for which all-zero is the documented
    // initial state; only hEvent is set.
    let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    ov.hEvent = ev.raw();
    ov
}

/// Wait for the operation described by `ov` to finish, whatever the outcome.
///
/// Called after `ERROR_IO_PENDING`. On return the kernel no longer references
/// `ov`: either the operation completed (Ok(bytes) / Err(code)), or the
/// defensive branch cancelled it and waited for the cancellation to land.
fn wait_completion(h: HANDLE, ov: &mut OVERLAPPED) -> Result<u32, u32> {
    let mut n = 0u32;
    // SAFETY: `ov` is the structure the pending operation was issued with and
    // outlives this call; bWait = TRUE blocks until completion.
    if unsafe { GetOverlappedResult(h, ov, &mut n, 1) } != 0 {
        return Ok(n);
    }
    // SAFETY: trivial FFI call.
    let err = unsafe { GetLastError() };
    if err == ERROR_IO_INCOMPLETE {
        // Cannot happen with bWait = TRUE; if it somehow does, the operation is
        // still in flight and `ov` cannot be released. Cancel it and wait for
        // the cancellation to complete before returning.
        // SAFETY: valid handle; `ov` is the pending operation's structure.
        unsafe {
            CancelIoEx(h, ov);
            GetOverlappedResult(h, ov, &mut n, 1);
        }
    }
    Err(err)
}

fn overlapped_read(h: HANDLE, ev: &Event, buf: &mut [u8]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let len = buf.len().min(u32::MAX as usize) as u32;
    let mut ov = overlapped_for(ev);
    ev.reset();
    let mut n = 0u32;
    // SAFETY: `buf` is valid for `len` bytes for the whole call; `ov` lives on
    // this frame and the call does not return until the operation completes.
    let ok = unsafe { ReadFile(h, buf.as_mut_ptr(), len, &mut n, &mut ov) };
    if ok != 0 {
        return Ok(n as usize);
    }
    // SAFETY: trivial FFI call.
    let err = unsafe { GetLastError() };
    let outcome = if err == ERROR_IO_PENDING {
        wait_completion(h, &mut ov)
    } else {
        Err(err)
    };
    match outcome {
        Ok(n) => Ok(n as usize),
        // The peer closed its handle (BROKEN_PIPE) or the server disconnected
        // the instance (PIPE_NOT_CONNECTED / NO_DATA): end of stream.
        Err(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA) => Ok(0),
        Err(e) => Err(io::Error::from_raw_os_error(e as i32)),
    }
}

fn overlapped_write(h: HANDLE, ev: &Event, buf: &[u8]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let len = buf.len().min(u32::MAX as usize) as u32;
    let mut ov = overlapped_for(ev);
    ev.reset();
    let mut n = 0u32;
    // SAFETY: `buf` is valid for `len` bytes for the whole call; `ov` lives on
    // this frame and the call does not return until the operation completes.
    let ok = unsafe { WriteFile(h, buf.as_ptr(), len, &mut n, &mut ov) };
    if ok != 0 {
        return Ok(n as usize);
    }
    // SAFETY: trivial FFI call.
    let err = unsafe { GetLastError() };
    let outcome = if err == ERROR_IO_PENDING {
        wait_completion(h, &mut ov)
    } else {
        Err(err)
    };
    match outcome {
        Ok(n) => Ok(n as usize),
        Err(e @ (ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_NO_DATA)) => {
            Err(io::Error::new(
                ErrorKind::BrokenPipe,
                format!("pipe peer gone (os error {e})"),
            ))
        }
        Err(e) => Err(io::Error::from_raw_os_error(e as i32)),
    }
}
