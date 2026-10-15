//! The client-facing Unix socket server.
//!
//! Each connection is a session: its search results stay on the server and
//! the client fetches windows of rows, so a GUI can scroll through millions
//! of hits while only the visible rows cross the socket. Searches run on
//! the blocking thread pool while holding read locks on all volume indexes;
//! results are filtered for the peer's uid and groups.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use evos_core::perm::{Caller, Visibility};
use evos_core::query::{highlight_terms, parse, ParseOptions};
use evos_core::search::{attributes_of, Extra, Hit, Query, RunInfo as CoreRunInfo};
use evos_core::sort::{sort_hits_with, SortKey as CoreKey, SortSpec};
use evos_core::store::mode;
use evos_core::types::type_name;
use evos_core::{EntryId, Index, ROOT};
use evos_mount::MountManager;
use evos_proto::{
    decode_body, encode_frame, DaemonStatus, Envelope, ErrorCode, Event, Request, Response, Row,
    RunChange, SearchOptions, Sort, SortKey, VolumeInfo, VolumeStatistics, MAX_FRAME,
    PROTOCOL_VERSION,
};
use evos_scan::volume::{Phase, WatchMode};
use parking_lot::RwLockReadGuard;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

use crate::config::Config;
use crate::history::RunHistory;
use crate::volumes::{VolumeHandle, VolumeManager};

/// Shared daemon state.
pub struct Daemon {
    pub cfg: Arc<Config>,
    pub volumes: Arc<VolumeManager>,
    pub mounts: Arc<MountManager>,
    pub history: Arc<RunHistory>,
    pub events: broadcast::Sender<Event>,
}

type Failure = (ErrorCode, String);

/// Most result sets one connection may hold; the oldest is dropped.
const MAX_RESULTS: usize = 32;
/// Most rows per fetch.
const MAX_FETCH: u32 = 10_000;

struct ResultSet {
    hits: Vec<Hit>,
    /// Index epoch of each volume when the search ran; ids are only valid
    /// while it is unchanged.
    epochs: HashMap<u16, u64>,
}

#[derive(Default)]
struct Session {
    results: HashMap<u32, Arc<ResultSet>>,
    order: Vec<u32>,
    next: u32,
    subscribed: bool,
}

/// The connecting user, from SO_PEERCRED.
#[derive(Debug, Clone)]
pub struct Peer {
    pub caller: Caller,
    pub pid: Option<u32>,
}

/// Accepts connections forever.
pub async fn serve(daemon: Arc<Daemon>, listener: UnixListener) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let daemon = daemon.clone();
        tokio::spawn(async move {
            if let Err(e) = connection(daemon, stream).await {
                tracing::debug!("connection closed: {e}");
            }
        });
    }
}

fn peer(stream: &UnixStream) -> io::Result<Peer> {
    let cred = stream.peer_cred()?;
    let pid = cred.pid().map(|p| p as u32);
    let mut gids = vec![cred.gid()];
    if let Some(pid) = pid {
        gids.extend(supplementary_groups(pid));
    }
    Ok(Peer {
        caller: Caller {
            uid: cred.uid(),
            gids,
        },
        pid,
    })
}

fn supplementary_groups(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| l.strip_prefix("Groups:")).map(|g| {
                g.split_whitespace()
                    .filter_map(|n| n.parse().ok())
                    .collect()
            })
        })
        .unwrap_or_default()
}

async fn connection(daemon: Arc<Daemon>, stream: UnixStream) -> io::Result<()> {
    let peer = peer(&stream)?;
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut rx) = mpsc::channel::<Envelope<Response>>(64);
    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let Ok(frame) = encode_frame(&msg) else {
                continue;
            };
            if writer.write_all(&frame).await.is_err() {
                break;
            }
        }
    });

    let mut session = Session {
        next: 1,
        ..Default::default()
    };
    loop {
        let len = match reader.read_u32_le().await {
            Ok(len) => len as usize,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };
        if len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).await?;
        let response = match decode_body::<Envelope<Request>>(&body) {
            Ok(req) => {
                let id = req.id;
                let resp = handle(&daemon, &mut session, &peer, req.body, &tx).await;
                Envelope { id, body: resp }
            }
            Err(e) => Envelope {
                id: 0,
                body: Response::Error {
                    code: ErrorCode::BadRequest,
                    message: e.to_string(),
                },
            },
        };
        if tx.send(response).await.is_err() {
            break;
        }
    }
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

fn failure(f: Failure) -> Response {
    Response::Error {
        code: f.0,
        message: f.1,
    }
}

pub(crate) async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f)
        .await
        .expect("blocking task panicked")
}

async fn handle(
    daemon: &Arc<Daemon>,
    session: &mut Session,
    peer: &Peer,
    request: Request,
    tx: &mpsc::Sender<Envelope<Response>>,
) -> Response {
    match request {
        Request::Hello { .. } => Response::Hello {
            server: format!("evosd {}", env!("CARGO_PKG_VERSION")),
            protocol: PROTOCOL_VERSION,
        },
        Request::Search {
            query,
            options,
            sort,
        } => {
            let (d, caller) = (daemon.clone(), peer.caller.clone());
            let started = Instant::now();
            match blocking(move || search(&d, &caller, &query, &options, sort)).await {
                Ok((set, highlight)) => {
                    let total = set.hits.len() as u64;
                    let result = session.store(set);
                    Response::SearchDone {
                        result,
                        total,
                        elapsed_us: started.elapsed().as_micros() as u64,
                        highlight,
                    }
                }
                Err(f) => failure(f),
            }
        }
        Request::Fetch {
            result,
            offset,
            count,
        } => {
            let Some(set) = session.results.get(&result).cloned() else {
                return failure(unknown_result());
            };
            let (d, uid) = (daemon.clone(), peer.caller.uid);
            match blocking(move || fetch(&d, uid, &set, offset, count.min(MAX_FETCH))).await {
                Ok(rows) => Response::Rows { offset, rows },
                Err(f) => failure(f),
            }
        }
        Request::Sort { result, sort } => {
            let Some(set) = session.results.get(&result).cloned() else {
                return failure(unknown_result());
            };
            let (d, uid) = (daemon.clone(), peer.caller.uid);
            match blocking(move || resort(&d, uid, &set, sort)).await {
                Ok(sorted) => {
                    session.results.insert(result, Arc::new(sorted));
                    Response::Ok
                }
                Err(f) => failure(f),
            }
        }
        Request::Release { result } => {
            session.results.remove(&result);
            session.order.retain(|&r| r != result);
            Response::Ok
        }
        Request::Status => Response::Status(status(daemon)),
        Request::Statistics => {
            let d = daemon.clone();
            Response::Statistics(blocking(move || statistics(&d)).await)
        }
        Request::Save => {
            let d = daemon.clone();
            blocking(move || {
                d.volumes.save_snapshots(true);
                d.history.save();
            })
            .await;
            Response::Ok
        }
        Request::ReadFile { path, limit } => {
            let (d, caller) = (daemon.clone(), peer.caller.clone());
            match blocking(move || read_file(&d, &caller, &path, limit)).await {
                Ok(data) => Response::FileContent(data),
                Err(f) => failure(f),
            }
        }
        Request::RunCount { path, change } => {
            let (history, uid) = (&daemon.history, peer.caller.uid);
            let info = match change {
                RunChange::Get => history.get(uid, &path),
                RunChange::Opened => history.opened(uid, &path),
                RunChange::Set(count) => history.set(uid, &path, count),
            };
            if change != RunChange::Get {
                // The Run Count and Date Run of shown results changed.
                let _ = daemon.events.send(Event::IndexChanged);
            }
            Response::RunCount(info)
        }
        Request::Rescan { path } => {
            let targets = match &path {
                Some(p) => daemon.volumes.find(p).into_iter().collect(),
                None => daemon.volumes.volumes(),
            };
            if targets.is_empty() {
                return failure((
                    ErrorCode::NotFound,
                    "no indexed volume contains that path".into(),
                ));
            }
            for t in targets {
                t.vol.request_rescan();
            }
            Response::Ok
        }
        Request::Hint { path } => {
            if let Some(v) = daemon.volumes.find(&path) {
                v.vol.hint(&path);
            }
            Response::Ok
        }
        Request::Subscribe => {
            if !session.subscribed {
                session.subscribed = true;
                let mut events = daemon.events.subscribe();
                let tx = tx.clone();
                tokio::spawn(async move {
                    loop {
                        match events.recv().await {
                            Ok(e) => {
                                if tx
                                    .send(Envelope {
                                        id: 0,
                                        body: Response::Event(e),
                                    })
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => return,
                        }
                    }
                });
            }
            Response::Ok
        }
        mount_request => crate::mounts::handle(daemon, peer, mount_request).await,
    }
}

impl Session {
    fn store(&mut self, set: ResultSet) -> u32 {
        let id = self.next;
        self.next = self.next.wrapping_add(1).max(1);
        self.results.insert(id, Arc::new(set));
        self.order.push(id);
        while self.order.len() > MAX_RESULTS {
            let old = self.order.remove(0);
            self.results.remove(&old);
        }
        id
    }
}

fn unknown_result() -> Failure {
    (
        ErrorCode::UnknownResult,
        "unknown or expired result; search again".into(),
    )
}

fn core_sort(sort: Sort) -> SortSpec {
    let key = match sort.key {
        SortKey::Name => CoreKey::Name,
        SortKey::Path => CoreKey::Path,
        SortKey::Size => CoreKey::Size,
        SortKey::Modified => CoreKey::Modified,
        SortKey::Extension => CoreKey::Extension,
        SortKey::Type => CoreKey::Type,
        SortKey::Created => CoreKey::Created,
        SortKey::Accessed => CoreKey::Accessed,
        SortKey::Attributes => CoreKey::Attributes,
        SortKey::RecentlyChanged => CoreKey::RecentlyChanged,
        SortKey::RunCount => CoreKey::RunCount,
        SortKey::DateRun => CoreKey::DateRun,
    };
    SortSpec {
        key,
        descending: sort.descending,
    }
}

fn parse_options(options: &SearchOptions) -> ParseOptions {
    ParseOptions {
        match_case: options.match_case,
        whole_word: options.whole_word,
        match_path: options.match_path,
        regex: options.regex,
        match_diacritics: options.match_diacritics,
        match_prefix: options.match_prefix,
        match_suffix: options.match_suffix,
        ignore_punctuation: options.ignore_punctuation,
        ignore_white_space: options.ignore_white_space,
        now: None,
    }
}

/// Read guards on every attached volume, in the order of `handles`.
struct Snapshot<'a> {
    handles: &'a [Arc<VolumeHandle>],
    guards: Vec<(u16, RwLockReadGuard<'a, Index>)>,
}

impl<'a> Snapshot<'a> {
    fn take(handles: &'a [Arc<VolumeHandle>]) -> Self {
        Snapshot {
            handles,
            guards: handles.iter().map(|h| (h.id, h.vol.index.read())).collect(),
        }
    }

    fn volumes(&self) -> Vec<(u16, &Index)> {
        self.guards.iter().map(|(id, g)| (*id, &**g)).collect()
    }

    fn position(&self, vol: u16) -> Option<usize> {
        self.guards.iter().position(|(id, _)| *id == vol)
    }

    /// The entry at an absolute path, in the volume with the longest root
    /// containing it.
    fn resolve(&self, path: &[u8]) -> Option<(u16, EntryId)> {
        self.guards
            .iter()
            .filter(|(_, ix)| ix.relative(path).is_some())
            .max_by_key(|(_, ix)| ix.root_path().len())
            .and_then(|(vol, ix)| Some((*vol, ix.resolve(path)?)))
    }
}

/// What the engine needs beyond the index, for one caller: folder sizes
/// (computed only if the query or sort asks for them), the caller's run
/// history, and whether the caller may read a file's content.
struct DaemonExtra<'a> {
    snap: &'a Snapshot<'a>,
    sizes: Vec<OnceLock<Arc<Vec<u64>>>>,
    runs: HashMap<(u16, EntryId), CoreRunInfo>,
    visibility: Option<&'a Visibility>,
}

impl<'a> DaemonExtra<'a> {
    fn new(
        snap: &'a Snapshot<'a>,
        history: &RunHistory,
        uid: u32,
        visibility: Option<&'a Visibility>,
    ) -> Self {
        let runs = history
            .of_user(uid)
            .into_iter()
            .filter_map(|(path, info)| {
                let at = snap.resolve(&path)?;
                Some((
                    at,
                    CoreRunInfo {
                        count: info.count,
                        last: info.last,
                    },
                ))
            })
            .collect();
        DaemonExtra {
            snap,
            sizes: snap.guards.iter().map(|_| OnceLock::new()).collect(),
            runs,
            visibility,
        }
    }
}

impl Extra for DaemonExtra<'_> {
    fn folder_size(&self, vol: u16, id: EntryId) -> Option<u64> {
        let pos = self.snap.position(vol)?;
        let sizes = self.sizes[pos]
            .get_or_init(|| self.snap.handles[pos].folder_sizes(&self.snap.guards[pos].1));
        sizes.get(id as usize).copied()
    }

    fn run_info(&self, vol: u16, id: EntryId) -> CoreRunInfo {
        self.runs.get(&(vol, id)).copied().unwrap_or_default()
    }

    fn can_read(&self, vol: u16, index: &Index, id: EntryId) -> bool {
        self.visibility.is_none_or(|v| v.can_read(vol, index, id))
    }
}

fn search(
    daemon: &Daemon,
    caller: &Caller,
    query: &str,
    options: &SearchOptions,
    sort: Sort,
) -> Result<(ResultSet, Vec<String>), Failure> {
    let expr = parse(query, &parse_options(options));
    let highlight = highlight_terms(&expr);
    let query = Query::compile(&expr).map_err(|e| (ErrorCode::BadQuery, e.to_string()))?;
    let handles = daemon.volumes.volumes();
    let snap = Snapshot::take(&handles);
    let vols = snap.volumes();
    let visibility = (!daemon.cfg.single_user && !caller.is_root())
        .then(|| Visibility::new(caller.clone(), &vols));
    let extra = DaemonExtra::new(&snap, &daemon.history, caller.uid, visibility.as_ref());
    let filter = |vol: u16, ix: &Index, id: EntryId| {
        visibility
            .as_ref()
            .is_none_or(|v| v.is_visible(vol, ix, id))
    };
    let mut hits = query
        .run_with(&vols, Some(&filter), &extra, &AtomicBool::new(false))
        .expect("search is never cancelled");
    sort_hits_with(&mut hits, &vols, core_sort(sort), &extra);
    // count: keeps the first results in the requested order.
    if let Some(limit) = query.limit() {
        hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    let epochs = snap.guards.iter().map(|(id, g)| (*id, g.epoch())).collect();
    Ok((ResultSet { hits, epochs }, highlight))
}

fn check_epochs(snap: &Snapshot<'_>, set: &ResultSet) -> Result<(), Failure> {
    for (id, guard) in &snap.guards {
        if set.epochs.get(id).is_some_and(|&e| e != guard.epoch()) {
            return Err(unknown_result());
        }
    }
    Ok(())
}

fn resort(daemon: &Daemon, uid: u32, set: &ResultSet, sort: Sort) -> Result<ResultSet, Failure> {
    let handles = daemon.volumes.volumes();
    let snap = Snapshot::take(&handles);
    check_epochs(&snap, set)?;
    // Hits of detached volumes cannot be sorted; drop them.
    let mut hits: Vec<Hit> = set
        .hits
        .iter()
        .copied()
        .filter(|h| snap.position(h.vol).is_some())
        .collect();
    let extra = DaemonExtra::new(&snap, &daemon.history, uid, None);
    sort_hits_with(&mut hits, &snap.volumes(), core_sort(sort), &extra);
    Ok(ResultSet {
        hits,
        epochs: set.epochs.clone(),
    })
}

fn fetch(
    daemon: &Daemon,
    uid: u32,
    set: &ResultSet,
    offset: u32,
    count: u32,
) -> Result<Vec<Row>, Failure> {
    let start = (offset as usize).min(set.hits.len());
    let end = (start + count as usize).min(set.hits.len());
    let window = &set.hits[start..end];
    let handles = daemon.volumes.volumes();
    let snap = Snapshot::take(&handles);
    check_epochs(&snap, set)?;
    let extra = DaemonExtra::new(&snap, &daemon.history, uid, None);
    let offline: HashMap<u16, bool> = handles
        .iter()
        .map(|h| (h.id, h.vol.status().phase == Phase::Offline))
        .collect();
    let mut folders: HashMap<(u16, EntryId), Vec<u8>> = HashMap::new();
    Ok(window
        .iter()
        .map(|hit| {
            let Some(pos) = snap.position(hit.vol) else {
                // The volume was detached since the search.
                return Row {
                    offline: true,
                    ..Default::default()
                };
            };
            let ix = &snap.guards[pos].1;
            let (name, folder) = if hit.id == ROOT {
                (ix.root_path().to_vec(), Vec::new())
            } else {
                let parent = ix.parent(hit.id);
                let folder = folders
                    .entry((hit.vol, parent))
                    .or_insert_with(|| ix.path(parent))
                    .clone();
                (ix.name(hit.id).to_vec(), folder)
            };
            let is_dir = ix.is_dir(hit.id);
            let m = ix.mode(hit.id);
            let executable = m & mode::S_IFMT == mode::S_IFREG && m & 0o111 != 0;
            let run = extra.run_info(hit.vol, hit.id);
            Row {
                type_name: type_name(ix.name(hit.id), is_dir, executable),
                attributes: attributes_of(ix, hit.id),
                name,
                folder,
                is_dir,
                size: if is_dir {
                    extra.folder_size(hit.vol, hit.id)
                } else {
                    Some(ix.size(hit.id))
                },
                mtime: ix.mtime(hit.id),
                created: ix.btime(hit.id),
                accessed: ix.atime(hit.id),
                changed: ix.changed(hit.id),
                run_count: run.count,
                date_run: run.last,
                offline: offline.get(&hit.vol).copied().unwrap_or(false),
            }
        })
        .collect())
}

/// Largest file preview served.
const MAX_READ: u32 = 16 << 20;

/// The start of an indexed file, if the caller could read it themselves.
fn read_file(
    daemon: &Daemon,
    caller: &Caller,
    path: &[u8],
    limit: u32,
) -> Result<Vec<u8>, Failure> {
    {
        let handles = daemon.volumes.volumes();
        let snap = Snapshot::take(&handles);
        let (vol, id) = snap
            .resolve(path)
            .ok_or_else(|| (ErrorCode::NotFound, "not an indexed file".to_owned()))?;
        let ix = &snap.guards[snap.position(vol).expect("resolved volume")].1;
        if ix.is_dir(id) {
            return Err((ErrorCode::BadRequest, "folders have no content".into()));
        }
        if !daemon.cfg.single_user && !caller.is_root() {
            let visibility = Visibility::new(caller.clone(), &snap.volumes());
            if !visibility.can_read(vol, ix, id) {
                return Err((ErrorCode::PermissionDenied, "permission denied".into()));
            }
        }
        // Read without holding the index locks: a slow share must not
        // stall updates.
    }
    evos_core::props::read_bytes(path, limit.min(MAX_READ) as u64)
        .map_err(|e| (ErrorCode::Failed, e.to_string()))
}

/// Totals per volume.
fn statistics(daemon: &Daemon) -> Vec<VolumeStatistics> {
    daemon
        .volumes
        .volumes()
        .iter()
        .map(|h| {
            let ix = h.vol.index.read();
            let (mut files, mut folders, mut total_size) = (0u64, 0u64, 0u64);
            for id in ix.live_ids().filter(|&id| id != ROOT) {
                if ix.is_dir(id) {
                    folders += 1;
                } else {
                    files += 1;
                    total_size += ix.size(id);
                }
            }
            VolumeStatistics {
                id: h.id,
                root: h.vol.root().to_vec(),
                fs_type: h.vol.fs_type().to_owned(),
                network: h.network,
                files,
                folders,
                total_size,
                index_bytes: ix.memory_bytes() as u64,
            }
        })
        .collect()
}

fn unix_secs(t: Option<SystemTime>) -> Option<i64> {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Builds the status report.
pub fn status(daemon: &Daemon) -> DaemonStatus {
    use std::sync::atomic::Ordering::Relaxed;
    let volumes = daemon
        .volumes
        .volumes()
        .iter()
        .map(|h| {
            let s = h.vol.status();
            VolumeInfo {
                id: h.id,
                root: h.vol.root().to_vec(),
                fs_type: h.vol.fs_type().to_owned(),
                network: h.network,
                phase: match s.phase {
                    Phase::Starting => "starting",
                    Phase::Scanning => "scanning",
                    Phase::Reconciling => "reconciling",
                    Phase::Live => "live",
                    Phase::Offline => "offline",
                    Phase::Stopped => "stopped",
                }
                .into(),
                watch: match s.watch {
                    WatchMode::None => "none",
                    WatchMode::Fanotify => "fanotify",
                    WatchMode::Inotify => "inotify",
                    WatchMode::SmbNotify { details: true } => "smb-notify",
                    WatchMode::SmbNotify { details: false } => "smb-notify-basic",
                    WatchMode::Sweeps => "sweeps",
                }
                .into(),
                entries: h.vol.index.read().live_count().saturating_sub(1) as u64,
                dirs_listed: h.vol.progress.dirs_listed.load(Relaxed),
                pending_dirs: h.vol.progress.pending.load(Relaxed),
                scan_complete: s.scan_complete,
                last_scan: unix_secs(s.last_scan),
                last_sweep: unix_secs(s.last_sweep),
                last_change: unix_secs(s.last_change),
                error: s.error,
                concurrency: s.gate.map(|g| g.limit as u32),
                ops_per_sec: s.gate.map(|g| g.ops_per_sec),
                tracked_dirs: s.tracked_dirs as u64,
            }
        })
        .collect();
    DaemonStatus {
        version: env!("CARGO_PKG_VERSION").into(),
        volumes,
    }
}

/// Broadcasts IndexChanged/StatusChanged events at most twice a second.
pub fn spawn_event_ticker(daemon: Arc<Daemon>) {
    tokio::spawn(async move {
        let mut last_generation = 0u64;
        let mut last_status = String::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        loop {
            tick.tick().await;
            if daemon.events.receiver_count() == 0 {
                continue;
            }
            let handles = daemon.volumes.volumes();
            let generation = handles.iter().fold(0u64, |acc, h| {
                acc.wrapping_add(h.vol.index.read().generation())
                    .wrapping_add(h.id as u64)
            });
            if generation != last_generation {
                last_generation = generation;
                let _ = daemon.events.send(Event::IndexChanged);
            }
            let st: Vec<String> = handles
                .iter()
                .map(|h| {
                    let s = h.vol.status();
                    format!("{}:{:?}:{:?}:{:?}", h.id, s.phase, s.watch, s.error)
                })
                .collect();
            let st = st.join("|");
            if st != last_status {
                last_status = st;
                let _ = daemon.events.send(Event::StatusChanged);
            }
        }
    });
}
