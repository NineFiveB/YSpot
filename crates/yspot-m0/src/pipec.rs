//! Minimal blocking pipe client for the freshness and startup subcommands —
//! the same §4.1 open sequence the shell and `probe` use, kept synchronous:
//! one request, read until the reply, no background thread.

use std::fs::File;
use std::io;
use std::os::windows::io::{FromRawHandle, RawHandle};

use anyhow::{bail, Context, Result};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_PIPE_BUSY, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
use yspot_proto::{
    Filters, Message, ResultItem, VolumeStatus, MAX_FRAME_S2C, PIPE_NAME, PROTO_VERSION,
};

pub struct Pipe {
    file: File,
    next_id: u64,
    next_gen: u64,
}

fn open_pipe() -> io::Result<File> {
    let name: Vec<u16> = PIPE_NAME.encode_utf16().chain(std::iter::once(0)).collect();
    let mut attempts = 0u32;
    loop {
        // SAFETY: NUL-terminated pipe name; the remaining arguments are plain
        // values or documented-null pointers.
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                yspot_proto::CLIENT_PIPE_ACCESS,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: fresh owned handle, transferred exactly once.
            return Ok(unsafe { File::from_raw_handle(handle as RawHandle) });
        }
        // SAFETY: trivially safe thread-local read.
        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_BUSY && attempts < 5 {
            attempts += 1;
            // SAFETY: same valid pipe name; 100 ms per §4.1.
            let _ = unsafe { WaitNamedPipeW(name.as_ptr(), 100) };
            continue;
        }
        return Err(io::Error::from_raw_os_error(err as i32));
    }
}

impl Pipe {
    pub fn connect() -> Result<Pipe> {
        let mut file = open_pipe().context("open pipe (is yspot-indexd running?)")?;
        let hello = Message::Hello {
            proto_min: PROTO_VERSION,
            proto_max: PROTO_VERSION,
            client: "yspot-m0".to_string(),
            pid: std::process::id(),
        };
        yspot_proto::write_msg(&mut file, &hello).context("write Hello")?;
        match yspot_proto::read_msg(&mut file, MAX_FRAME_S2C).context("read HelloAck")? {
            Some(Message::HelloAck { .. }) => Ok(Pipe {
                file,
                next_id: 1,
                next_gen: 1,
            }),
            Some(other) => bail!("unexpected handshake reply: {other:?}"),
            None => bail!("pipe closed during handshake"),
        }
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// One search, blocking until the final batch; all batches concatenated.
    pub fn search(&mut self, text: &str, max_results: u32) -> Result<Vec<ResultItem>> {
        self.next_gen += 1;
        let gen = self.next_gen;
        let id = self.id();
        yspot_proto::write_msg(
            &mut self.file,
            &Message::SearchQuery {
                id,
                gen,
                text: text.to_string(),
                scopes: Vec::new(),
                filters: Filters::default(),
                max_results,
            },
        )
        .context("write SearchQuery")?;
        let mut out = Vec::new();
        loop {
            match yspot_proto::read_msg(&mut self.file, MAX_FRAME_S2C).context("read results")? {
                Some(Message::SearchResults {
                    gen: g,
                    is_final,
                    items,
                    ..
                }) => {
                    if g != gen {
                        continue; // stale generation, keep reading
                    }
                    out.extend(items);
                    if is_final {
                        return Ok(out);
                    }
                }
                Some(Message::Error { code, message, .. }) => {
                    bail!("service error {code}: {message}")
                }
                Some(_) => continue,
                None => bail!("pipe closed mid-search"),
            }
        }
    }

    pub fn status(&mut self) -> Result<Vec<VolumeStatus>> {
        let id = self.id();
        yspot_proto::write_msg(&mut self.file, &Message::IndexStatusReq { id })
            .context("write IndexStatusReq")?;
        loop {
            match yspot_proto::read_msg(&mut self.file, MAX_FRAME_S2C).context("read status")? {
                Some(Message::IndexStatus { volumes, .. }) => return Ok(volumes),
                Some(_) => continue,
                None => bail!("pipe closed awaiting status"),
            }
        }
    }
}
