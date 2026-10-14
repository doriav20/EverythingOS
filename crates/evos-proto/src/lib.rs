//! Wire protocol between the EverythingOS daemon and its clients.
//!
//! Messages are postcard-encoded and framed with a little-endian `u32`
//! length over a Unix stream socket. Every request carries an id that the
//! matching response repeats; pushed events use id 0.

mod mounts;

use serde::{Deserialize, Serialize};

pub use mounts::{Credentials, IndexSettings, MountInfo, MountKind, MountSpec};

/// Bumped on incompatible protocol changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Where the daemon listens by default.
pub const DEFAULT_SOCKET: &str = "/run/evos/evosd.sock";

/// Largest accepted frame.
pub const MAX_FRAME: usize = 64 << 20;

/// Search toggles, as in Everything's Search menu.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub regex: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Extension,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub key: SortKey,
    pub descending: bool,
}

/// A request from a client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    Hello {
        client: String,
        protocol: u32,
    },
    /// Runs a query and keeps the result set on the server.
    Search {
        query: String,
        options: SearchOptions,
        sort: Sort,
    },
    /// Returns rows `offset..offset + count` of a result set.
    Fetch {
        result: u32,
        offset: u32,
        count: u32,
    },
    /// Re-sorts a result set.
    Sort {
        result: u32,
        sort: Sort,
    },
    /// Frees a result set.
    Release {
        result: u32,
    },
    Status,
    /// Re-checks one path's volume, or every volume.
    Rescan {
        path: Option<Vec<u8>>,
    },
    /// The user is looking at this path; refresh it soon.
    Hint {
        path: Vec<u8>,
    },
    /// Pushes [`Event`]s on this connection from now on.
    Subscribe,
    Mounts,
    MountAdd {
        spec: MountSpec,
        credentials: Option<Credentials>,
    },
    MountRemove {
        name: String,
    },
    MountUp {
        name: String,
    },
    MountDown {
        name: String,
    },
    /// Mounts temporarily and lists the root to validate settings.
    MountTest {
        spec: MountSpec,
        credentials: Option<Credentials>,
    },
}

/// A response or pushed event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Hello {
        server: String,
        protocol: u32,
    },
    SearchDone {
        result: u32,
        total: u64,
        elapsed_us: u64,
    },
    Rows {
        offset: u32,
        rows: Vec<Row>,
    },
    Ok,
    Status(DaemonStatus),
    Mounts(Vec<MountInfo>),
    Error {
        code: ErrorCode,
        message: String,
    },
    Event(Event),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    BadRequest,
    BadQuery,
    UnknownResult,
    NotFound,
    PermissionDenied,
    Failed,
}

/// Pushed to subscribers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Indexed data changed; visible results may be stale.
    IndexChanged,
    /// Volume or mount status changed.
    StatusChanged,
}

/// One result row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub name: Vec<u8>,
    /// Absolute path of the containing folder.
    pub folder: Vec<u8>,
    pub is_dir: bool,
    /// Size in bytes; `None` for folders.
    pub size: Option<u64>,
    /// Modification time in Unix nanoseconds.
    pub mtime: i64,
    /// The row lives on a network share that is currently offline.
    pub offline: bool,
}

impl Row {
    /// Absolute path of the entry.
    pub fn path(&self) -> Vec<u8> {
        let mut p = self.folder.clone();
        if p.last() != Some(&b'/') {
            p.push(b'/');
        }
        p.extend_from_slice(&self.name);
        p
    }
}

/// Daemon-wide status.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub version: String,
    pub volumes: Vec<VolumeInfo>,
}

/// Status of one indexed volume.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub id: u16,
    pub root: Vec<u8>,
    pub fs_type: String,
    pub network: bool,
    /// starting, scanning, reconciling, live, offline or stopped.
    pub phase: String,
    /// fanotify, inotify, smb-notify, smb-notify-basic, sweeps or none.
    pub watch: String,
    pub entries: u64,
    pub dirs_listed: u64,
    pub pending_dirs: u64,
    pub scan_complete: bool,
    /// Unix seconds.
    pub last_scan: Option<i64>,
    pub last_sweep: Option<i64>,
    pub last_change: Option<i64>,
    pub error: Option<String>,
    /// Network volumes: current concurrency limit and throughput.
    pub concurrency: Option<u32>,
    pub ops_per_sec: Option<f64>,
    pub tracked_dirs: u64,
}

/// Request or response with its correlation id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub id: u32,
    pub body: T,
}

/// Encodes a message as a length-prefixed frame.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>, postcard::Error> {
    let body = postcard::to_allocvec(msg)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Decodes a frame body (without the length prefix).
pub fn decode_body<'a, T: Deserialize<'a>>(body: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let msg = Envelope {
            id: 7,
            body: Request::Search {
                query: "ext:jpg size:>1mb".into(),
                options: SearchOptions {
                    match_case: true,
                    ..Default::default()
                },
                sort: Sort {
                    key: SortKey::Size,
                    descending: true,
                },
            },
        };
        let frame = encode_frame(&msg).unwrap();
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        let back: Envelope<Request> = decode_body(&frame[4..]).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn rows_carry_raw_bytes() {
        let row = Row {
            name: b"caf\xe9.txt".to_vec(),
            folder: b"/".to_vec(),
            is_dir: false,
            size: Some(3),
            mtime: -5,
            offline: false,
        };
        assert_eq!(row.path(), b"/caf\xe9.txt");
        let frame = encode_frame(&Response::Rows {
            offset: 0,
            rows: vec![row.clone()],
        })
        .unwrap();
        let back: Response = decode_body(&frame[4..]).unwrap();
        assert_eq!(
            back,
            Response::Rows {
                offset: 0,
                rows: vec![row]
            }
        );
    }
}
