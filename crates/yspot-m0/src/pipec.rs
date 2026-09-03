//! Minimal blocking pipe client for the freshness and startup subcommands —
//! the same §4.1 open sequence and server verification the shell and `probe`
//! use (`yspot-pipe`), kept synchronous: one request, read until the reply,
//! no background thread.

use anyhow::{bail, Context, Result};
use yspot_pipe::Duplex;
use yspot_proto::{
    Filters, Message, ResultItem, VolumeStatus, MAX_FRAME_S2C, PIPE_NAME, PROTO_VERSION,
};

pub struct Pipe {
    file: Duplex,
    next_id: u64,
    next_gen: u64,
}

impl Pipe {
    pub fn connect() -> Result<Pipe> {
        let conn = yspot_pipe::client::connect(PIPE_NAME)
            .context("open pipe (is yspot-indexd running?)")?;
        let mut file = conn.duplex().context("split pipe")?;
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
