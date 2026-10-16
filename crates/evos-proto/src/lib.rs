//! Wire protocol between the EverythingOS daemon and its clients, plus a
//! small blocking client.
//!
//! Messages are postcard-encoded and framed with a little-endian `u32`
//! length over a Unix stream socket. Every request carries an id that the
//! matching response repeats; pushed events use id 0.

mod client;
pub mod efu;
mod mounts;
mod servers;

use serde::{Deserialize, Serialize};

pub use client::{Client, ClientError, SearchResult};
pub use mounts::{Credentials, IndexSettings, MountInfo, MountKind, MountSpec};
pub use servers::{
    EtpServer, HttpServer, IndexServer, IndexUser, IndexVolume, RemoteIndexInfo, RemoteIndexSpec,
    RemoteKind, RemoteSearch, ServerSettings, ETP_PORT, HTTP_PORT, INDEX_PORT,
};

/// Bumped on incompatible protocol changes.
pub const PROTOCOL_VERSION: u32 = 3;

/// Where the daemon listens by default.
pub const DEFAULT_SOCKET: &str = "/run/evos/evosd.sock";

/// Largest accepted frame.
pub const MAX_FRAME: usize = 64 << 20;

/// Largest accepted response: index snapshots of big volumes.
pub const MAX_RESPONSE: usize = 1 << 30;

/// Search toggles, as in Everything's Search menu.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub regex: bool,
    /// Distinguish "é" from "e" (off: diacritics are ignored).
    pub match_diacritics: bool,
    /// Terms must match at the start of a word.
    pub match_prefix: bool,
    /// Terms must match at the end of a word.
    pub match_suffix: bool,
    pub ignore_punctuation: bool,
    pub ignore_white_space: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Extension,
    /// Type description ("PNG Image").
    Type,
    Created,
    Accessed,
    Attributes,
    RecentlyChanged,
    RunCount,
    DateRun,
}

impl SortKey {
    pub const ALL: [SortKey; 12] = [
        SortKey::Name,
        SortKey::Path,
        SortKey::Size,
        SortKey::Modified,
        SortKey::Extension,
        SortKey::Type,
        SortKey::Created,
        SortKey::Accessed,
        SortKey::Attributes,
        SortKey::RecentlyChanged,
        SortKey::RunCount,
        SortKey::DateRun,
    ];

    /// The name used on command lines and in settings ("date-modified").
    pub fn name(self) -> &'static str {
        match self {
            SortKey::Name => "name",
            SortKey::Path => "path",
            SortKey::Size => "size",
            SortKey::Modified => "date-modified",
            SortKey::Extension => "extension",
            SortKey::Type => "type",
            SortKey::Created => "date-created",
            SortKey::Accessed => "date-accessed",
            SortKey::Attributes => "attributes",
            SortKey::RecentlyChanged => "date-recently-changed",
            SortKey::RunCount => "run-count",
            SortKey::DateRun => "date-run",
        }
    }

    /// Parses a sort name, accepting Everything's short forms (dm, dc, ...).
    pub fn parse(s: &str) -> Option<SortKey> {
        let s = s.to_ascii_lowercase().replace('_', "-");
        Some(match s.as_str() {
            "name" | "n" => SortKey::Name,
            "path" | "p" | "full-path" | "full-path-and-name" => SortKey::Path,
            "size" | "s" => SortKey::Size,
            "date-modified" | "dm" | "modified" | "mtime" | "d" => SortKey::Modified,
            "extension" | "ext" | "e" => SortKey::Extension,
            "type" | "t" => SortKey::Type,
            "date-created" | "dc" | "created" => SortKey::Created,
            "date-accessed" | "da" | "accessed" => SortKey::Accessed,
            "attributes" | "attribs" | "attrib" | "a" => SortKey::Attributes,
            "date-recently-changed" | "rc" | "recently-changed" => SortKey::RecentlyChanged,
            "run-count" | "runcount" => SortKey::RunCount,
            "date-run" | "dr" => SortKey::DateRun,
            _ => return None,
        })
    }
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
    /// Reads or changes the caller's run history for a path (how often
    /// and when it was opened from EverythingOS).
    RunCount {
        path: Vec<u8>,
        change: RunChange,
    },
    /// File, folder and size totals of every volume.
    Statistics,
    /// Writes every index snapshot to disk now.
    Save,
    /// Up to `limit` bytes of an indexed file, for previews. The daemon
    /// reads it without changing its access time, and only if the caller
    /// could read it themselves.
    ReadFile {
        path: Vec<u8>,
        offset: u64,
        limit: u32,
    },
    /// The ETP, HTTP and index server settings (passwords left out).
    Servers,
    /// Changes them; needs authorization.
    SetServers(Box<ServerSettings>),
    RemoteIndexes,
    /// Adds or replaces a remote index; `password: None` keeps the stored
    /// one. Needs authorization.
    RemoteIndexAdd {
        spec: RemoteIndexSpec,
        password: Option<String>,
    },
    RemoteIndexRemove {
        name: String,
    },
    /// Connects to the server once to check the settings.
    RemoteIndexTest {
        spec: RemoteIndexSpec,
        password: Option<String>,
    },
    /// Searches on this connection go to an ETP server from now on.
    Connect(RemoteSearch),
    /// Back to the local index.
    Disconnect,
    /// Index server: log in (the first request on its TCP port).
    IndexLogin {
        user: String,
        password: String,
    },
    /// Index server: the volumes this user may copy.
    IndexVolumes,
    /// Index server: a full copy of a volume (a snapshot file).
    IndexSnapshot {
        volume: u16,
    },
    /// Index server: what changed since a copy was taken (a
    /// [`Response::IndexData`] delta, or [`Response::IndexResync`]).
    IndexDelta {
        volume: u16,
        epoch: u64,
        len: u32,
        since: i64,
    },
}

/// What a [`Request::RunCount`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunChange {
    Get,
    /// The path was just opened: count it and set its run date.
    Opened,
    Set(u32),
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
        /// Literal terms to highlight in names and paths.
        highlight: Vec<String>,
    },
    Rows {
        offset: u32,
        rows: Vec<Row>,
    },
    Ok,
    Status(DaemonStatus),
    Mounts(Vec<MountInfo>),
    RunCount(RunInfo),
    Statistics(Vec<VolumeStatistics>),
    FileContent(Vec<u8>),
    Servers(Box<ServerSettings>),
    RemoteIndexes(Vec<RemoteIndexInfo>),
    /// Connected to an ETP server; its welcome message.
    Connected {
        server: String,
    },
    IndexVolumes(Vec<IndexVolume>),
    IndexData(Vec<u8>),
    /// The copy is from an older epoch: take a new snapshot.
    IndexResync,
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

/// One result row. Times are Unix nanoseconds; 0 means unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// The entry's name; for the root of a volume, its full path.
    pub name: Vec<u8>,
    /// Absolute path of the containing folder; empty for volume roots.
    pub folder: Vec<u8>,
    pub is_dir: bool,
    /// Size in bytes; for folders the total size of their contents, when
    /// known.
    pub size: Option<u64>,
    pub mtime: i64,
    /// Birth time, where the filesystem records it.
    pub created: i64,
    pub accessed: i64,
    /// When the index last saw the entry appear or change.
    pub changed: i64,
    /// Everything-style attribute letters: R(ead-only), H(idden),
    /// D(irectory), A(rchive: any file), L(ink).
    pub attributes: String,
    /// Type description ("PNG Image", "Folder").
    pub type_name: String,
    /// How often the caller opened it from EverythingOS, and when last.
    pub run_count: u32,
    pub date_run: i64,
    /// The row lives on a network share that is currently offline.
    pub offline: bool,
    /// The row is on another computer (a remote index or an ETP server):
    /// its file is read through the daemon, and opening it opens a copy.
    pub remote: bool,
}

impl Row {
    /// Absolute path of the entry.
    pub fn path(&self) -> Vec<u8> {
        if self.folder.is_empty() {
            return self.name.clone();
        }
        // Results from a Windows computer (through ETP) keep its paths.
        let windows = (self.folder.get(1) == Some(&b':') && !self.folder.contains(&b'/'))
            || self.folder.starts_with(b"\\\\");
        let sep = if windows { b'\\' } else { b'/' };
        let mut p = self.folder.clone();
        if p.last() != Some(&sep) {
            p.push(sep);
        }
        p.extend_from_slice(&self.name);
        p
    }
}

/// Run history of one path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    pub count: u32,
    /// Unix nanoseconds; 0 if never run.
    pub last: i64,
}

/// Totals of one volume, for the statistics view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeStatistics {
    pub id: u16,
    pub root: Vec<u8>,
    pub fs_type: String,
    pub network: bool,
    pub files: u64,
    pub folders: u64,
    /// Sum of all file sizes in bytes.
    pub total_size: u64,
    /// Approximate memory the index uses, in bytes.
    pub index_bytes: u64,
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
            type_name: "TXT File".into(),
            ..Default::default()
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

    #[test]
    fn volume_roots_are_their_own_path() {
        let root = Row {
            name: b"/mnt/nas".to_vec(),
            is_dir: true,
            ..Default::default()
        };
        assert_eq!(root.path(), b"/mnt/nas");
    }

    #[test]
    fn sort_names_roundtrip() {
        for key in SortKey::ALL {
            assert_eq!(SortKey::parse(key.name()), Some(key));
        }
        assert_eq!(SortKey::parse("DM"), Some(SortKey::Modified));
        assert_eq!(SortKey::parse("run_count"), Some(SortKey::RunCount));
        assert_eq!(SortKey::parse("nope"), None);
    }
}
