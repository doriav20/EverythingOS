//! Remote indexes: copies of other computers' indexes, searchable like
//! local volumes and kept current (Everything's "remote Everything
//! indexes").
//!
//! * From another EverythingOS computer's index server: a snapshot of each
//!   volume it offers, then deltas every few seconds (only what changed).
//! * From an ETP server, such as Everything on Windows: the whole file list
//!   is copied again every few minutes.
//!
//! Copies are saved like local indexes, so they stay searchable while the
//! server is offline. Definitions live in `<config_dir>/remote.d/*.toml`,
//! passwords root-only in `<config_dir>/credentials/remote-<name>`.

use std::collections::HashMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use evos_core::store::mode;
use evos_core::{persist, Delta, Index, Meta, ROOT};
use evos_etp::{EtpClient, Item};
use evos_proto::{Client, RemoteIndexInfo, RemoteIndexSpec, RemoteKind, Request};
use evos_scan::volume::{Phase, WatchMode};
use parking_lot::Mutex;

use crate::volumes::{VolumeHandle, VolumeManager};

const TIMEOUT: Duration = Duration::from_secs(30);
/// Results per ETP request when copying a whole index.
const ETP_PAGE: u64 = 5000;

#[derive(Debug, Clone, Default)]
struct State {
    phase: String,
    last_sync: Option<i64>,
    error: Option<String>,
}

struct Remote {
    spec: RemoteIndexSpec,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

pub struct RemoteManager {
    dir: PathBuf,
    creds: PathBuf,
    volumes: Arc<VolumeManager>,
    remotes: Mutex<HashMap<String, Remote>>,
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Where a server path appears locally.
fn local_path(mount: &str, server: &[u8]) -> Vec<u8> {
    let mut p = mount.trim_end_matches('/').as_bytes().to_vec();
    if server != b"/" {
        if !server.starts_with(b"/") {
            p.push(b'/');
        }
        p.extend_from_slice(server);
    }
    if p.is_empty() {
        p.push(b'/');
    }
    p
}

/// The components of a server path: `C:\Users\me` or `/home/me`.
fn components(path: &str) -> Vec<&str> {
    let sep = if evos_etp::is_windows_path(path) {
        '\\'
    } else {
        '/'
    };
    path.split(sep).filter(|c| !c.is_empty()).collect()
}

/// Adds a server entry (and any missing folders above it) to a copy.
fn insert_item(ix: &mut Index, item: &Item) {
    let folder = item.path.as_deref().unwrap_or("");
    let mut comps = components(folder);
    comps.push(&item.name);
    let date = |v: Option<u64>| v.map_or(0, evos_etp::date_from_wire);
    let mut meta = if item.is_folder {
        Meta::dir(date(item.date_modified))
    } else {
        Meta::file(item.size.unwrap_or(0), date(item.date_modified))
    };
    meta.btime = date(item.date_created);
    if item
        .attributes
        .is_some_and(|a| a & evos_proto::efu::ATTR_READONLY != 0)
    {
        meta.mode &= !0o222;
    }
    let mut cur = ROOT;
    for (i, comp) in comps.iter().enumerate() {
        let last = i + 1 == comps.len();
        cur = match ix.lookup(cur, comp.as_bytes()) {
            Some(id) => {
                if last {
                    ix.set_meta(id, meta);
                }
                id
            }
            None if last => ix.insert(cur, comp.as_bytes(), meta),
            None => ix.insert(cur, comp.as_bytes(), Meta::dir(0)),
        };
        if !last && ix.mode(cur) & mode::S_IFMT != mode::S_IFDIR {
            return;
        }
    }
}

/// Is a server path within the include-only folders?
fn etp_included(item: &Item, include: &[String]) -> bool {
    let folders: Vec<&String> = include.iter().filter(|f| !f.trim().is_empty()).collect();
    if folders.is_empty() {
        return true;
    }
    let full = evos_etp::item_to_row(item).path();
    let full = String::from_utf8_lossy(&full).to_lowercase();
    folders.iter().any(|f| {
        let f = f.trim_end_matches(['/', '\\']).to_lowercase();
        full == f || full.starts_with(&format!("{f}/")) || full.starts_with(&format!("{f}\\"))
    })
}

struct Driver {
    spec: RemoteIndexSpec,
    password: String,
    volumes: Arc<VolumeManager>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

/// Where an index server's copy of a volume left off.
struct Cursor {
    epoch: u64,
    since: i64,
}

impl Driver {
    fn set_phase(&self, phase: &str) {
        self.state.lock().phase = phase.into();
    }

    fn mark(&self, phase: Phase, error: Option<String>) {
        for h in self.volumes.remote_handles(&self.spec.name) {
            h.vol.update_status(|s| {
                s.phase = phase.clone();
                s.watch = WatchMode::Remote;
                s.error = error.clone();
                if phase == Phase::Live {
                    s.scan_complete = true;
                    s.last_scan = Some(SystemTime::now());
                }
            });
        }
    }

    /// Sleeps, waking early when stopped.
    fn pause(&self, d: Duration) {
        let end = Instant::now() + d;
        while Instant::now() < end && !self.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn run(self) {
        // Saved copies from earlier runs are searchable right away.
        let mount = self.spec.mount_as();
        for root in self.volumes.saved_remote_copies(&self.spec.name) {
            let server = root
                .strip_prefix(mount.trim_end_matches('/').as_bytes())
                .map(|r| {
                    if r.is_empty() {
                        b"/".to_vec()
                    } else {
                        r.to_vec()
                    }
                })
                .unwrap_or_else(|| b"/".to_vec());
            let fs = if self.spec.kind == RemoteKind::Etp {
                "etp"
            } else {
                "evos"
            };
            self.volumes
                .attach_remote(&self.spec.name, &root, &server, fs);
        }
        let mut conn: Option<Client> = None;
        let mut cursors: HashMap<Vec<u8>, Cursor> = HashMap::new();
        let mut first = true;
        while !self.stop.load(Ordering::Relaxed) {
            if first {
                self.set_phase("connecting");
            }
            let result = match self.spec.kind {
                RemoteKind::Evos => self.sync_evos(&mut conn, &mut cursors),
                RemoteKind::Etp => self.sync_etp(),
            };
            match result {
                Ok(()) => {
                    {
                        let mut s = self.state.lock();
                        s.phase = "live".into();
                        s.error = None;
                        s.last_sync = Some(unix_now());
                    }
                    self.mark(Phase::Live, None);
                    first = false;
                    self.pause(Duration::from_secs(self.spec.refresh_secs().max(1)));
                }
                Err(e) => {
                    tracing::info!(remote = %self.spec.name, "remote index unavailable: {e}");
                    conn = None;
                    {
                        let mut s = self.state.lock();
                        s.phase = "offline".into();
                        s.error = Some(e.clone());
                    }
                    self.mark(Phase::Offline, Some(e));
                    self.pause(Duration::from_secs(self.spec.refresh_secs().clamp(5, 60)));
                }
            }
        }
    }

    fn sync_evos(
        &self,
        conn: &mut Option<Client>,
        cursors: &mut HashMap<Vec<u8>, Cursor>,
    ) -> Result<(), String> {
        let e = |e: evos_proto::ClientError| e.to_string();
        if conn.is_none() {
            let mut c = Client::connect_tcp(&self.spec.host, self.spec.port(), "evosd", TIMEOUT)
                .map_err(e)?;
            c.index_login(&self.spec.username, &self.password)
                .map_err(e)?;
            *conn = Some(c);
        }
        let c = conn.as_mut().expect("connected");
        let volumes = c.index_volumes().map_err(e)?;
        let mount = self.spec.mount_as();
        let mut keep_roots = Vec::new();
        for v in &volumes {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let root = local_path(&mount, &v.root);
            keep_roots.push(root.clone());
            let handle = self
                .volumes
                .attach_remote(&self.spec.name, &root, &v.root, &v.fs_type);
            let fresh = cursors.get(&v.root).is_some_and(|c| c.epoch == v.epoch);
            if fresh {
                let cursor = cursors.get_mut(&v.root).expect("checked");
                let len = handle.vol.index.read().len() as u32;
                let data = c
                    .index_data(Request::IndexDelta {
                        volume: v.id,
                        epoch: v.epoch,
                        len,
                        since: cursor.since,
                    })
                    .map_err(e)?;
                match data {
                    Some(bytes) => {
                        let delta = Delta::decode(&bytes).map_err(|e| e.to_string())?;
                        cursor.since = delta.now;
                        if !delta.entries.is_empty() {
                            handle.vol.index.write().apply_delta(&delta);
                            handle
                                .vol
                                .update_status(|s| s.last_change = Some(SystemTime::now()));
                        }
                    }
                    // The server's index was rebuilt: copy it again next round.
                    None => {
                        cursors.remove(&v.root);
                    }
                }
            } else {
                self.set_phase("copying");
                handle.vol.update_status(|s| s.phase = Phase::Scanning);
                self.snapshot(c, v, &root, &handle, cursors)?;
            }
        }
        for h in self.volumes.remote_handles(&self.spec.name) {
            if !keep_roots.iter().any(|r| r.as_slice() == h.vol.root()) {
                self.volumes.forget_remote(h.id);
            }
        }
        Ok(())
    }

    fn snapshot(
        &self,
        c: &mut Client,
        v: &evos_proto::IndexVolume,
        root: &[u8],
        handle: &VolumeHandle,
        cursors: &mut HashMap<Vec<u8>, Cursor>,
    ) -> Result<(), String> {
        let bytes = c
            .index_data(Request::IndexSnapshot { volume: v.id })
            .map_err(|e| e.to_string())?
            .ok_or("the server has no copy of that volume")?;
        let (mut ix, _) = persist::read_snapshot(&bytes[..]).map_err(|e| e.to_string())?;
        // Changes after the snapshot carry later stamps (server clock).
        let since = (0..ix.len() as u32)
            .map(|id| ix.changed(id))
            .max()
            .unwrap_or(0);
        ix.set_root_path(root);
        handle.vol.index.write().replace_with(ix);
        handle
            .vol
            .update_status(|s| s.last_change = Some(SystemTime::now()));
        cursors.insert(
            v.root.clone(),
            Cursor {
                epoch: v.epoch,
                since,
            },
        );
        Ok(())
    }

    fn sync_etp(&self) -> Result<(), String> {
        let mut c = EtpClient::connect(
            &self.spec.host,
            self.spec.port(),
            &self.spec.username,
            &self.password,
            TIMEOUT,
        )
        .map_err(|e| e.to_string())?;
        if self.state.lock().last_sync.is_none() {
            self.set_phase("copying");
        }
        let mount = self.spec.mount_as();
        let mut ix = Index::new(mount.as_bytes(), Meta::dir(0));
        let mut offset = 0u64;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let reply = c
                .query(&evos_etp::Query {
                    search: String::new(),
                    sort: "PATH_ASCENDING".into(),
                    offset,
                    count: ETP_PAGE,
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?;
            let got = reply.items.len() as u64;
            for item in reply
                .items
                .iter()
                .filter(|i| etp_included(i, &self.spec.include_only))
            {
                insert_item(&mut ix, item);
            }
            offset += got;
            if got == 0 || offset >= reply.total {
                break;
            }
        }
        c.quit();
        let handle = self
            .volumes
            .attach_remote(&self.spec.name, mount.as_bytes(), b"/", "etp");
        handle.vol.index.write().replace_with(ix);
        handle
            .vol
            .update_status(|s| s.last_change = Some(SystemTime::now()));
        Ok(())
    }
}

impl RemoteManager {
    pub fn open(config_dir: &std::path::Path, volumes: Arc<VolumeManager>) -> RemoteManager {
        RemoteManager {
            dir: config_dir.join("remote.d"),
            creds: config_dir.join("credentials"),
            volumes,
            remotes: Mutex::new(HashMap::new()),
        }
    }

    fn password_file(&self, name: &str) -> PathBuf {
        self.creds.join(format!("remote-{name}"))
    }

    /// Starts every defined remote index.
    pub fn start(&self) {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "toml") {
                match std::fs::read_to_string(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|s| toml::from_str::<RemoteIndexSpec>(&s).map_err(|e| e.to_string()))
                {
                    Ok(spec) => self.launch(spec),
                    Err(e) => tracing::warn!("ignoring {}: {e}", path.display()),
                }
            }
        }
    }

    fn launch(&self, spec: RemoteIndexSpec) {
        let state = Arc::new(Mutex::new(State {
            phase: if spec.enabled {
                "connecting"
            } else {
                "disabled"
            }
            .into(),
            ..Default::default()
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = spec.enabled.then(|| {
            let driver = Driver {
                password: std::fs::read_to_string(self.password_file(&spec.name))
                    .unwrap_or_default(),
                spec: spec.clone(),
                volumes: self.volumes.clone(),
                state: state.clone(),
                stop: stop.clone(),
            };
            std::thread::Builder::new()
                .name(format!("evos-remote-{}", spec.name))
                .spawn(move || driver.run())
                .expect("spawn remote index thread")
        });
        let name = spec.name.clone();
        let old = self.remotes.lock().insert(
            name,
            Remote {
                spec,
                state,
                stop,
                thread,
            },
        );
        if let Some(old) = old {
            Self::halt(old);
        }
    }

    fn halt(mut remote: Remote) {
        remote.stop.store(true, Ordering::Relaxed);
        if let Some(t) = remote.thread.take() {
            // A driver blocked on a dead server gives up on its own timeout.
            std::thread::spawn(move || {
                let _ = t.join();
            });
        }
    }

    pub fn list(&self) -> Vec<RemoteIndexInfo> {
        let mut out: Vec<RemoteIndexInfo> = self
            .remotes
            .lock()
            .values()
            .map(|r| {
                let s = r.state.lock().clone();
                let entries = self
                    .volumes
                    .remote_handles(&r.spec.name)
                    .iter()
                    .map(|h| h.vol.index.read().live_count().saturating_sub(1) as u64)
                    .sum();
                RemoteIndexInfo {
                    spec: r.spec.clone(),
                    phase: s.phase,
                    entries,
                    last_sync: s.last_sync,
                    error: s.error,
                }
            })
            .collect();
        out.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        out
    }

    /// Adds or replaces a remote index and starts copying it.
    pub fn add(&self, spec: RemoteIndexSpec, password: Option<String>) -> Result<(), String> {
        spec.validate()?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&self.creds).map_err(|e| e.to_string())?;
        let text = toml::to_string_pretty(&spec).map_err(|e| e.to_string())?;
        std::fs::write(self.dir.join(format!("{}.toml", spec.name)), text)
            .map_err(|e| e.to_string())?;
        if let Some(p) = password {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(self.password_file(&spec.name))
                .map_err(|e| e.to_string())?;
            f.write_all(p.as_bytes()).map_err(|e| e.to_string())?;
        }
        // A changed mount location or source starts from scratch.
        if let Some(old) = self.remotes.lock().get(&spec.name) {
            if old.spec.mount_as() != spec.mount_as()
                || old.spec.host != spec.host
                || old.spec.kind != spec.kind
            {
                for h in self.volumes.remote_handles(&spec.name) {
                    self.volumes.forget_remote(h.id);
                }
            }
        }
        self.launch(spec);
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<(), String> {
        let removed = self.remotes.lock().remove(name);
        let Some(remote) = removed else {
            return Err(format!("no remote index named {name}"));
        };
        Self::halt(remote);
        for h in self.volumes.remote_handles(name) {
            self.volumes.forget_remote(h.id);
        }
        let _ = std::fs::remove_file(self.dir.join(format!("{name}.toml")));
        let _ = std::fs::remove_file(self.password_file(name));
        Ok(())
    }

    /// The stored password of a remote index, if `spec` is still about the
    /// same server and account: it is never sent anywhere else.
    pub fn stored_password(&self, spec: &RemoteIndexSpec) -> Option<String> {
        let same = self.remotes.lock().get(&spec.name).is_some_and(|r| {
            r.spec.kind == spec.kind
                && r.spec.host == spec.host
                && r.spec.port() == spec.port()
                && r.spec.username == spec.username
        });
        same.then(|| std::fs::read_to_string(self.password_file(&spec.name)).ok())
            .flatten()
    }

    /// Connects once with these settings.
    pub fn test(spec: &RemoteIndexSpec, password: &str) -> Result<String, String> {
        spec.validate()?;
        match spec.kind {
            RemoteKind::Evos => {
                let mut c = Client::connect_tcp(&spec.host, spec.port(), "evosd", TIMEOUT)
                    .map_err(|e| e.to_string())?;
                c.index_login(&spec.username, password)
                    .map_err(|e| e.to_string())?;
                let n = c.index_volumes().map_err(|e| e.to_string())?.len();
                Ok(format!("{} offers {n} volumes", c.server))
            }
            RemoteKind::Etp => {
                let c =
                    EtpClient::connect(&spec.host, spec.port(), &spec.username, password, TIMEOUT)
                        .map_err(|e| e.to_string())?;
                let welcome = c.welcome.clone();
                c.quit();
                Ok(welcome)
            }
        }
    }

    /// Reads a file of a remote copy from its server.
    pub fn read_file(
        &self,
        path: &[u8],
        offset: u64,
        limit: u32,
    ) -> Option<Result<Vec<u8>, String>> {
        let handle = self
            .volumes
            .volumes()
            .into_iter()
            .filter(|h| h.remote.is_some())
            .filter(|h| h.vol.index.read().relative(path).is_some())
            .max_by_key(|h| h.vol.root().len())?;
        let name = handle.remote.clone()?;
        let spec = self.remotes.lock().get(&name)?.spec.clone();
        let password = std::fs::read_to_string(self.password_file(&name)).unwrap_or_default();
        let rel = handle.vol.index.read().relative(path)?.to_vec();
        let result = match spec.kind {
            RemoteKind::Evos => {
                let mut server = handle.remote_root.clone();
                if !rel.is_empty() {
                    if server.last() != Some(&b'/') {
                        server.push(b'/');
                    }
                    server.extend_from_slice(&rel);
                }
                Client::connect_tcp(&spec.host, spec.port(), "evosd", TIMEOUT)
                    .and_then(|mut c| {
                        c.index_login(&spec.username, &password)?;
                        c.read_file(&server, offset, limit)
                    })
                    .map_err(|e| e.to_string())
            }
            RemoteKind::Etp => {
                let rel = String::from_utf8_lossy(&rel).into_owned();
                let ftp = format!("/{}", rel.trim_start_matches('/'));
                EtpClient::connect(&spec.host, spec.port(), &spec.username, &password, TIMEOUT)
                    .and_then(|mut c| c.read_file(&ftp, offset, limit as u64))
                    .map_err(|e| e.to_string())
            }
        };
        Some(result)
    }

    pub fn shutdown(&self) {
        for (_, r) in self.remotes.lock().drain() {
            Self::halt(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_paths() {
        assert_eq!(local_path("/net/pc", b"/"), b"/net/pc");
        assert_eq!(local_path("/net/pc/", b"/home"), b"/net/pc/home");
        assert_eq!(components("C:\\Users\\me"), ["C:", "Users", "me"]);
        assert_eq!(components("/home/me"), ["home", "me"]);
        assert_eq!(components("\\\\srv\\share\\x"), ["srv", "share", "x"]);
    }

    #[test]
    fn etp_lists_become_an_index() {
        let mut ix = Index::new(b"/net/pc", Meta::dir(0));
        let item = |path: &str, name: &str, folder: bool| Item {
            is_folder: folder,
            name: name.into(),
            path: Some(path.into()),
            size: (!folder).then_some(3),
            date_modified: Some(133_444_736_000_000_000),
            attributes: Some(if folder { 0x10 } else { 0x21 }),
            ..Default::default()
        };
        for i in [
            item("C:\\", "Users", true),
            item("C:\\Users", "מסמך.txt", false),
            item("D:\\Media", "song.mp3", false),
        ] {
            insert_item(&mut ix, &i);
        }
        let doc = ix.resolve("/net/pc/C:/Users/מסמך.txt".as_bytes()).unwrap();
        assert_eq!(ix.size(doc), 3);
        assert_eq!(ix.mtime(doc), 1_700_000_000_000_000_000);
        assert_eq!(ix.mode(doc) & 0o222, 0, "read-only");
        assert!(ix.is_dir(ix.resolve(b"/net/pc/D:/Media").unwrap()));
        let include = vec!["C:\\Users".to_string()];
        assert!(etp_included(&item("C:\\Users", "a", false), &include));
        assert!(!etp_included(&item("D:\\Media", "a", false), &include));
    }
}
