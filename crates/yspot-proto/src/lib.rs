//! YSpot pipe protocol — SPEC.md §4.
//!
//! Wire format (§4.2): `u32` little-endian payload length, then one
//! MessagePack-encoded map (string keys — additive versioning depends on
//! named fields, so encoding MUST go through [`encode`], which uses
//! `rmp_serde::to_vec_named`).
//!
//! Frame caps (§4.2, §8.1): client→service 1 MiB, service→client 16 MiB.
//! An oversized or undecodable inbound frame is a protocol error and the
//! connection is dropped.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// Negotiated protocol version (§4.5). Additive-only within pipe name v1.
pub const PROTO_VERSION: u32 = 1;

/// Pipe name (§4.1). The `v1` suffix is the transport major version.
pub const PIPE_NAME: &str = r"\\.\pipe\yspot.indexd.v1";

/// Max inbound frame size at the service (client→service), bytes.
pub const MAX_FRAME_C2S: u32 = 1 << 20; // 1 MiB
/// Max inbound frame size at the client (service→client), bytes.
pub const MAX_FRAME_S2C: u32 = 16 << 20; // 16 MiB

/// SDDL for the service pipe (§4.1).
pub const PIPE_SDDL: &str = "O:SYG:SYD:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019B;;;IU)S:(ML;;NW;;;ME)";

/// [`PIPE_SDDL`] without the `O:SY G:SY` owner/group prefix. A process that is
/// not SYSTEM cannot assign SYSTEM as owner — `CreateNamedPipeW` then fails
/// with `ERROR_INVALID_OWNER` (1307) — so unelevated dev runs use this form.
/// The DACL and integrity label are identical, so the pipe stays ACL-hardened;
/// only the owner differs (the creating user instead of SYSTEM), which means
/// the §4.1 client-side owner check does not apply to dev-mode pipes.
pub const PIPE_SDDL_NO_OWNER: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x12019B;;;IU)S:(ML;;NW;;;ME)";

/// Explicit client open rights (§4.1): FILE_GENERIC_READ |
/// (FILE_GENERIC_WRITE & !FILE_APPEND_DATA). Never GENERIC_WRITE.
pub const CLIENT_PIPE_ACCESS: u32 = 0x0012_019B;

// Application error codes (§4.8).
pub mod codes {
    pub const UNSUPPORTED_VERSION: u32 = 100;
    pub const VOLUME_OFFLINE: u32 = 101;
    pub const INDEX_REBUILDING: u32 = 102;
    pub const SCOPE_DENIED: u32 = 103;
    pub const OVERLOADED: u32 = 104;
    pub const UNKNOWN_MESSAGE: u32 = 105;
    pub const INVALID_CONFIG: u32 = 106;
    pub const SCOPE_UNSUPPORTED: u32 = 107;
}

/// Opaque stable result identity (§4.3): volume-GUID index + NTFS FRN.
/// Keys `executeAction`, frontend rows, and the shell's frecency store.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResultId {
    pub volume_idx: u32,
    pub frn: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Filters {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ext: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_substr: Option<String>,
}

/// One search hit (§4.3). `match_ranges` are UTF-16 code-unit indexes into
/// `name` (§5.13). `size`/`mtime` are lazily stat-ed for the returned page
/// only and may be absent.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ResultItem {
    pub id: ResultId,
    pub path: String,
    pub name: String,
    pub score: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Modified time, Windows FILETIME (100 ns ticks since 1601-01-01 UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_ranges: Vec<(u32, u32)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionActivity {
    Active,
    Idle,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VolumeState {
    Enumerating,
    Tailing,
    Rebuilding,
    Paused,
    Unsupported,
    Offline,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RamBytes {
    pub filename: u64,
    pub content: u64,
    pub caches: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct VolumeStatus {
    /// Volume GUID path, e.g. `\\?\Volume{...}\`.
    pub volume: String,
    /// Mount roots, e.g. `["C:\\"]`.
    pub mounts: Vec<String>,
    pub fs: String,
    pub state: VolumeState,
    pub files_indexed: u64,
    pub usn_lag_ms: u32,
    pub content_docs: u64,
    pub ram_bytes: RamBytes,
}

/// Every frame is a map with a `t` type tag (§4.3).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
pub enum Message {
    Hello {
        proto_min: u32,
        proto_max: u32,
        client: String,
        pid: u32,
    },
    HelloAck {
        proto: u32,
        service_version: String,
        index_epoch: u64,
    },
    SearchQuery {
        id: u64,
        gen: u64,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<String>,
        #[serde(default)]
        filters: Filters,
        max_results: u32,
    },
    ContentSearchQuery {
        id: u64,
        gen: u64,
        query: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scopes: Vec<String>,
        max_results: u32,
        snippets: bool,
    },
    SearchResults {
        gen: u64,
        seq: u32,
        is_final: bool,
        items: Vec<ResultItem>,
    },
    Cancel {
        gen: u64,
    },
    IndexStatusReq {
        id: u64,
    },
    IndexStatus {
        id: u64,
        volumes: Vec<VolumeStatus>,
    },
    SessionState {
        state: SessionActivity,
    },
    PauseIndexing {
        id: u64,
    },
    ResumeIndexing {
        id: u64,
    },
    Ack {
        id: u64,
    },
    Subscribe {
        id: u64,
        topics: Vec<String>,
    },
    Event {
        topic: String,
        payload: rmpv::Value,
    },
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        gen: Option<u64>,
        code: u32,
        message: String,
        retryable: bool,
    },
}

#[derive(thiserror::Error, Debug)]
pub enum ProtoError {
    #[error("frame of {0} bytes exceeds cap of {1} bytes")]
    FrameTooLarge(u32, u32),
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("encode: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("decode: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
}

/// Encode a message as a framed buffer (length prefix + named-field MessagePack).
pub fn encode(msg: &Message) -> Result<Vec<u8>, ProtoError> {
    let payload = rmp_serde::to_vec_named(msg)?;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Write one framed message to a blocking writer.
pub fn write_msg<W: Write>(w: &mut W, msg: &Message) -> Result<(), ProtoError> {
    let buf = encode(msg)?;
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

/// Read one framed message from a blocking reader, enforcing `cap`.
/// Returns `Ok(None)` on clean EOF at a frame boundary.
pub fn read_msg<R: Read>(r: &mut R, cap: u32) -> Result<Option<Message>, ProtoError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len_buf);
    if len > cap {
        return Err(ProtoError::FrameTooLarge(len, cap));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Some(rmp_serde::from_slice(&payload)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_search_query() {
        let m = Message::SearchQuery {
            id: 1,
            gen: 42,
            text: "réadme".into(),
            scopes: vec![],
            filters: Filters::default(),
            max_results: 32,
        };
        let buf = encode(&m).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let back = read_msg(&mut cur, MAX_FRAME_C2S).unwrap().unwrap();
        match back {
            Message::SearchQuery { gen, text, .. } => {
                assert_eq!(gen, 42);
                assert_eq!(text, "réadme");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn frames_are_maps_with_t_tag() {
        // Additive versioning (§4.5) requires named fields, not tuples.
        let m = Message::Cancel { gen: 7 };
        let buf = encode(&m).unwrap();
        let v: rmpv::Value = rmp_serde::from_slice(&buf[4..]).unwrap();
        let map = v.as_map().expect("frame must be a msgpack map");
        assert!(map
            .iter()
            .any(|(k, val)| k.as_str() == Some("t") && val.as_str() == Some("Cancel")));
    }

    #[test]
    fn cap_enforced() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_C2S + 1).to_le_bytes());
        buf.extend_from_slice(&[0; 16]);
        let mut cur = std::io::Cursor::new(buf);
        assert!(matches!(
            read_msg(&mut cur, MAX_FRAME_C2S),
            Err(ProtoError::FrameTooLarge(_, _))
        ));
    }

    #[test]
    fn eof_at_boundary_is_none() {
        let mut cur = std::io::Cursor::new(Vec::<u8>::new());
        assert!(read_msg(&mut cur, MAX_FRAME_C2S).unwrap().is_none());
    }
}
