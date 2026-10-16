//! The set of indexed volumes, kept in sync with the mount table.
//!
//! Every local filesystem and network share worth indexing gets a volume
//! with its own driver thread. Managed network mounts stay attached even
//! while unmounted, so their (offline) index stays searchable and the
//! driver resumes as soon as the supervisor remounts them. Volumes are
//! saved as snapshots periodically and when detached.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use evos_core::{persist, Index, Meta};
use evos_mount::MountManager;
use evos_proto::{IndexSettings, MountKind};
use evos_scan::fsops::{self, CacheMode};
use evos_scan::local::{run_local, LocalOptions};
use evos_scan::mountinfo::{self, MountClass};
use evos_scan::network::{run_network, NetworkOptions};
use evos_scan::schedule::ScheduleConfig;
use evos_scan::throttle::GateConfig;
use evos_scan::volume::{Checkpoint, Volume};
use parking_lot::{Mutex, RwLock};

use crate::config::{exclude_fn, Config};

/// One attached volume.
pub struct VolumeHandle {
    pub id: u16,
    pub vol: Arc<Volume>,
    pub network: bool,
    pub source: String,
    /// The remote index this volume is a copy of, if any.
    pub remote: Option<String>,
    /// For copies: the volume's root on the server.
    pub remote_root: Vec<u8>,
    snapshot: PathBuf,
    saved_generation: AtomicU64,
    thread: Mutex<Option<JoinHandle<()>>>,
    sizes: Mutex<Option<SizeCache>>,
}

/// Folder sizes computed from one state of an index.
struct SizeCache {
    epoch: u64,
    generation: u64,
    at: Instant,
    sizes: Arc<Vec<u64>>,
}

/// While an index keeps changing, folder sizes are recomputed at most this
/// often.
const SIZE_REFRESH: Duration = Duration::from_secs(3);

impl VolumeHandle {
    /// Total sizes of every folder (see [`Index::folder_sizes`]), cached.
    /// Slightly stale sizes are fine for display: ids only change when the
    /// epoch does, so a cached vector never describes the wrong entry.
    pub fn folder_sizes(&self, index: &Index) -> Arc<Vec<u64>> {
        let mut cache = self.sizes.lock();
        if let Some(c) = cache.as_ref() {
            let fresh = c.generation == index.generation() || c.at.elapsed() < SIZE_REFRESH;
            if c.epoch == index.epoch() && fresh {
                return c.sizes.clone();
            }
        }
        let sizes = Arc::new(index.folder_sizes());
        *cache = Some(SizeCache {
            epoch: index.epoch(),
            generation: index.generation(),
            at: Instant::now(),
            sizes: sizes.clone(),
        });
        sizes
    }

    fn snapshot_extra(&self) -> Vec<u8> {
        SnapshotMeta {
            source: self.source.clone(),
            checkpoint: self.vol.checkpoint(),
        }
        .encode()
    }

    /// Saves the index if it changed since the last save.
    pub fn save(&self, force: bool) {
        let index = self.vol.index.read();
        let generation = index.generation();
        if !force && generation == self.saved_generation.load(Ordering::Relaxed) {
            return;
        }
        match persist::save(&index, &self.snapshot_extra(), &self.snapshot) {
            Ok(()) => self.saved_generation.store(generation, Ordering::Relaxed),
            Err(e) => tracing::warn!("cannot save {}: {e}", self.snapshot.display()),
        }
    }
}

/// Extra snapshot data: what was mounted, and the scan checkpoint.
struct SnapshotMeta {
    source: String,
    checkpoint: Checkpoint,
}

impl SnapshotMeta {
    fn encode(&self) -> Vec<u8> {
        let cp = self.checkpoint.encode();
        let mut out = vec![1u8];
        out.extend_from_slice(&(self.source.len() as u32).to_le_bytes());
        out.extend_from_slice(self.source.as_bytes());
        out.extend_from_slice(&cp);
        out
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        let (&1, rest) = bytes.split_first()? else {
            return None;
        };
        let len = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let source = String::from_utf8(rest.get(4..4 + len)?.to_vec()).ok()?;
        let checkpoint = Checkpoint::decode(rest.get(4 + len..)?)?;
        Some(SnapshotMeta { source, checkpoint })
    }
}

/// Snapshot file name for a volume root: `/` is `root.idx`, `/mnt/a b` is
/// `mnt-a%20b.idx`.
pub fn snapshot_name(root: &[u8]) -> String {
    if root == b"/" {
        return "root.idx".into();
    }
    let mut name = String::new();
    for &b in root.strip_prefix(b"/").unwrap_or(root) {
        match b {
            b'/' => name.push('-'),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'_' => name.push(b as char),
            _ => name.push_str(&format!("%{b:02x}")),
        }
    }
    name + ".idx"
}

/// The root a snapshot file name stands for (see [`snapshot_name`]).
pub fn root_of_snapshot_name(name: &str) -> Option<Vec<u8>> {
    let name = name.strip_suffix(".idx")?;
    if name == "root" {
        return Some(b"/".to_vec());
    }
    let mut root = vec![b'/'];
    let b = name.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'-' => root.push(b'/'),
            b'%' => {
                let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
                root.push(u8::from_str_radix(hex, 16).ok()?);
                i += 2;
            }
            c => root.push(c),
        }
        i += 1;
    }
    Some(root)
}

/// A volume the configuration says should be indexed.
#[derive(Debug, Clone, PartialEq)]
struct Wanted {
    root: Vec<u8>,
    fs_type: String,
    source: String,
    network: bool,
    settings: Option<IndexSettings>,
}

pub struct VolumeManager {
    cfg: Arc<Config>,
    mounts: Arc<MountManager>,
    volumes: RwLock<Vec<Arc<VolumeHandle>>>,
    next_id: AtomicU16,
    closed: AtomicBool,
}

impl VolumeManager {
    pub fn new(cfg: Arc<Config>, mounts: Arc<MountManager>) -> Self {
        VolumeManager {
            cfg,
            mounts,
            volumes: RwLock::new(Vec::new()),
            next_id: AtomicU16::new(1),
            closed: AtomicBool::new(false),
        }
    }

    pub fn volumes(&self) -> Vec<Arc<VolumeHandle>> {
        self.volumes.read().clone()
    }

    pub fn get(&self, id: u16) -> Option<Arc<VolumeHandle>> {
        self.volumes.read().iter().find(|v| v.id == id).cloned()
    }

    /// The volume containing `path` (longest matching root).
    pub fn find(&self, path: &[u8]) -> Option<Arc<VolumeHandle>> {
        self.volumes
            .read()
            .iter()
            .filter(|v| v.vol.index.read().relative(path).is_some())
            .max_by_key(|v| v.vol.root().len())
            .cloned()
    }

    fn wanted(&self) -> Vec<Wanted> {
        let mounts = match mountinfo::read_mounts() {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("cannot read the mount table: {e}");
                return Vec::new();
            }
        };
        let cfg = &self.cfg;
        let excluded = exclude_fn(&cfg.local.exclude);
        let mut seen = HashSet::new();
        let mut wanted = Vec::new();
        for m in &mounts {
            let spec = self.mounts.spec_for(&m.mount_point);
            if spec.as_ref().is_some_and(|s| !s.index.enabled) {
                continue;
            }
            let class = m.class();
            let point = String::from_utf8_lossy(&m.mount_point);
            // A mount the user explicitly asked for is indexed whatever its
            // filesystem type (e.g. a bind mount of an overlay folder).
            let skip = (class == MountClass::Ignored && spec.is_none())
                || (m.fs_type == "tmpfs" && !cfg.local.index_tmpfs && spec.is_none())
                || cfg
                    .local
                    .skip_mounts
                    .iter()
                    .any(|s| s.trim_end_matches('/') == point)
                || excluded
                    .as_ref()
                    .is_some_and(|ex| ex(&m.mount_point) && m.mount_point != b"/")
                || (m.is_bind() && spec.is_none())
                || (class == MountClass::Network && spec.is_none() && !cfg.network.index_unmanaged);
            // The same filesystem subtree mounted twice is indexed once.
            if skip || !seen.insert((m.major, m.minor, m.root.clone())) {
                continue;
            }
            wanted.push(Wanted {
                root: m.mount_point.clone(),
                fs_type: m.fs_type.clone(),
                source: m.source.clone(),
                network: class == MountClass::Network,
                settings: spec.map(|s| s.index),
            });
        }
        // Managed shares stay attached while unmounted: their index remains
        // searchable (offline) and resumes after the remount.
        for spec in self.mounts.specs() {
            if !spec.kind.is_network() || !spec.auto_mount || !spec.index.enabled {
                continue;
            }
            let root = spec.mountpoint().into_bytes();
            if !wanted.iter().any(|w| w.root == root) {
                let fs_type = match spec.kind {
                    MountKind::Smb => "cifs",
                    MountKind::Nfs => "nfs",
                    _ => "fuse.sshfs",
                };
                wanted.push(Wanted {
                    root,
                    fs_type: fs_type.into(),
                    source: spec.source.clone(),
                    network: true,
                    settings: Some(spec.index),
                });
            }
        }
        wanted
    }

    /// Attaches new volumes and detaches vanished ones.
    pub fn sync(&self) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let wanted = self.wanted();
        let current = self.volumes();
        // Copies of remote indexes are managed by their drivers.
        for handle in current.iter().filter(|h| h.remote.is_none()) {
            if !wanted
                .iter()
                .any(|w| w.root == handle.vol.root() && w.source == handle.source)
            {
                self.detach(handle.id);
            }
        }
        for w in wanted {
            let attached = self
                .volumes
                .read()
                .iter()
                .any(|h| h.vol.root() == w.root.as_slice());
            if !attached {
                self.attach(w);
            }
        }
    }

    fn attach(&self, w: Wanted) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let root_meta = fsops::stat(&w.root, CacheMode::Cached).unwrap_or(Meta::dir(0));
        let vol = Arc::new(Volume::new(
            Index::new(&w.root, root_meta),
            &w.fs_type,
            Checkpoint::default(),
        ));
        let handle = Arc::new(VolumeHandle {
            id,
            vol: vol.clone(),
            network: w.network,
            source: w.source.clone(),
            remote: None,
            remote_root: Vec::new(),
            snapshot: self.cfg.state_dir.join(snapshot_name(&w.root)),
            saved_generation: AtomicU64::new(u64::MAX),
            thread: Mutex::new(None),
            sizes: Mutex::new(None),
        });
        tracing::info!(
            id,
            root = %String::from_utf8_lossy(&w.root),
            fs = %w.fs_type,
            network = w.network,
            "attaching volume"
        );
        let local = self.local_options(w.settings.as_ref(), &w.root);
        let network = self.network_options(w.settings.as_ref(), &w.root);
        let snapshot = handle.snapshot.clone();
        let source = w.source.clone();
        let is_network = w.network;
        let thread = std::thread::Builder::new()
            .name(format!("evos-vol-{id}"))
            .spawn(move || {
                restore(&vol, &snapshot, &source);
                if is_network {
                    run_network(&vol, &network);
                } else {
                    run_local(&vol, &local);
                }
            })
            .expect("spawn volume thread");
        *handle.thread.lock() = Some(thread);
        self.volumes.write().push(handle);
    }

    /// The copies of a remote index.
    pub fn remote_handles(&self, remote: &str) -> Vec<Arc<VolumeHandle>> {
        self.volumes
            .read()
            .iter()
            .filter(|h| h.remote.as_deref() == Some(remote))
            .cloned()
            .collect()
    }

    /// Adds a volume holding a copy of a server's volume (no driver: the
    /// remote index updates it). A copy saved earlier is loaded, so the
    /// index is searchable even while the server is offline.
    pub fn attach_remote(
        &self,
        remote: &str,
        local_root: &[u8],
        server_root: &[u8],
        fs_type: &str,
    ) -> Arc<VolumeHandle> {
        if let Some(h) = self
            .remote_handles(remote)
            .into_iter()
            .find(|h| h.vol.root() == local_root)
        {
            return h;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let vol = Arc::new(Volume::new(
            Index::new(local_root, Meta::dir(0)),
            fs_type,
            Checkpoint::default(),
        ));
        let source = format!("remote:{remote}");
        let snapshot = self
            .cfg
            .state_dir
            .join(format!("remote-{remote}@{}", snapshot_name(local_root)));
        restore(&vol, &snapshot, &source);
        vol.update_status(|s| {
            s.watch = evos_scan::volume::WatchMode::Remote;
            s.phase = evos_scan::volume::Phase::Starting;
        });
        let handle = Arc::new(VolumeHandle {
            id,
            vol,
            network: true,
            source,
            remote: Some(remote.to_owned()),
            remote_root: server_root.to_vec(),
            snapshot,
            saved_generation: AtomicU64::new(u64::MAX),
            thread: Mutex::new(None),
            sizes: Mutex::new(None),
        });
        tracing::info!(id, remote, root = %String::from_utf8_lossy(local_root), "attaching remote copy");
        self.volumes.write().push(handle.clone());
        handle
    }

    /// Detaches a remote copy and deletes its saved copy.
    pub fn forget_remote(&self, id: u16) {
        let Some(handle) = self.get(id) else { return };
        self.volumes.write().retain(|h| h.id != id);
        handle.vol.stop();
        let _ = std::fs::remove_file(&handle.snapshot);
    }

    /// Saved copies of a remote index's volumes, from earlier runs.
    pub fn saved_remote_copies(&self, remote: &str) -> Vec<Vec<u8>> {
        let prefix = format!("remote-{remote}@");
        let Ok(dir) = std::fs::read_dir(&self.cfg.state_dir) else {
            return Vec::new();
        };
        dir.flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                root_of_snapshot_name(name.strip_prefix(&prefix)?)
            })
            .collect()
    }

    /// Stops a volume's driver, saves it and removes it.
    pub fn detach(&self, id: u16) {
        let Some(handle) = self.get(id) else { return };
        tracing::info!(id, root = %String::from_utf8_lossy(handle.vol.root()), "detaching volume");
        self.volumes.write().retain(|h| h.id != id);
        stop_and_save(&handle);
    }

    /// Saves every volume that changed since its last snapshot.
    pub fn save_snapshots(&self, force: bool) {
        for handle in self.volumes() {
            handle.save(force);
        }
    }

    /// Stops all drivers and saves every volume.
    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let handles = std::mem::take(&mut *self.volumes.write());
        for h in &handles {
            h.vol.stop();
        }
        for h in &handles {
            stop_and_save(h);
        }
    }

    fn local_options(&self, settings: Option<&IndexSettings>, root: &[u8]) -> LocalOptions {
        let defaults = LocalOptions::default();
        LocalOptions {
            threads: self.cfg.local.threads.unwrap_or(defaults.threads),
            exclude: exclude_fn(&self.excludes(settings, root)),
            safety_sweep: Duration::from_secs(self.cfg.local.safety_sweep_hours.max(1) * 3600),
            allow_fanotify: self.cfg.local.fanotify,
        }
    }

    fn network_options(&self, settings: Option<&IndexSettings>, root: &[u8]) -> NetworkOptions {
        let net = &self.cfg.network;
        let s = settings.cloned().unwrap_or_default();
        let max = s
            .max_concurrency
            .map_or(net.max_concurrency, |c| c as usize)
            .max(1);
        let rate = s
            .max_ops_per_sec
            .or((net.max_ops_per_sec > 0.0).then_some(net.max_ops_per_sec));
        NetworkOptions {
            threads: max,
            gate: GateConfig {
                max,
                initial: max.min(8),
                max_ops_per_sec: rate.filter(|r| *r > 0.0),
                ..GateConfig::default()
            },
            schedule: ScheduleConfig {
                min_interval: Duration::from_secs(
                    s.min_recheck_secs.unwrap_or(net.min_recheck_secs).max(1),
                ),
                max_interval: Duration::from_secs(
                    s.max_staleness_secs
                        .unwrap_or(net.max_staleness_hours * 3600)
                        .max(60),
                ),
                refresh_interval: Duration::from_secs(net.refresh_days.max(1) * 86_400),
            },
            exclude: exclude_fn(&self.excludes(Some(&s), root)),
            smb_notify: net.smb_notify && s.smb_notify,
            offline_retry: Duration::from_secs(net.offline_retry_secs.max(1)),
            require_mount: true,
        }
    }

    /// Global exclusions plus per-mount ones (relative paths are taken
    /// relative to the volume root).
    fn excludes(&self, settings: Option<&IndexSettings>, root: &[u8]) -> Vec<String> {
        let root = String::from_utf8_lossy(root).into_owned();
        let mut all = self.cfg.local.exclude.clone();
        for ex in settings.map(|s| s.exclude.as_slice()).unwrap_or_default() {
            all.push(if ex.starts_with('/') {
                ex.clone()
            } else {
                format!("{}/{}", root.trim_end_matches('/'), ex)
            });
        }
        all
    }
}

fn stop_and_save(handle: &VolumeHandle) {
    handle.vol.stop();
    if let Some(thread) = handle.thread.lock().take() {
        // A driver blocked on a dead server may not return promptly; save
        // what we have rather than waiting forever.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        if thread.is_finished() {
            let _ = thread.join();
        }
    }
    handle.save(false);
}

/// Loads a volume's snapshot if it belongs to the same root and source.
fn restore(vol: &Volume, path: &std::path::Path, source: &str) {
    let started = std::time::Instant::now();
    match persist::load(path) {
        Ok((index, extra)) => {
            let meta = SnapshotMeta::decode(&extra);
            match meta {
                Some(meta) if index.root_path() == vol.root() && meta.source == source => {
                    let entries = index.live_count();
                    *vol.index.write() = index;
                    vol.set_checkpoint(meta.checkpoint);
                    tracing::info!(
                        entries,
                        ms = started.elapsed().as_millis() as u64,
                        "restored {}",
                        path.display()
                    );
                }
                _ => tracing::info!("ignoring snapshot {} of another volume", path.display()),
            }
        }
        Err(persist::SnapshotError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("discarding snapshot {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_names_are_unique_and_safe() {
        assert_eq!(snapshot_name(b"/"), "root.idx");
        assert_eq!(snapshot_name(b"/mnt/evos/nas"), "mnt-evos-nas.idx");
        assert_eq!(snapshot_name(b"/mnt/a b"), "mnt-a%20b.idx");
        assert_ne!(snapshot_name(b"/a-b"), snapshot_name(b"/a/b"));
        for root in [&b"/"[..], b"/net/pc/home", b"/a-b/c d", "/נ/ש".as_bytes()] {
            assert_eq!(root_of_snapshot_name(&snapshot_name(root)).unwrap(), root);
        }
    }

    #[test]
    fn snapshot_meta_roundtrip() {
        let meta = SnapshotMeta {
            source: "//nas/share".into(),
            checkpoint: Checkpoint {
                scan_complete: false,
                frontier: vec![4, 2],
            },
        };
        let back = SnapshotMeta::decode(&meta.encode()).unwrap();
        assert_eq!(back.source, meta.source);
        assert_eq!(back.checkpoint, meta.checkpoint);
        assert!(SnapshotMeta::decode(b"\x02junk").is_none());
    }

    #[test]
    fn manager_follows_the_mount_table_and_saves_snapshots() {
        use std::os::unix::ffi::OsStrExt;
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let disk = tmp.path().join("disk");
        std::fs::create_dir_all(&disk).unwrap();
        let mounted = Command::new("mount")
            .args(["-t", "tmpfs", "evos-test"])
            .arg(&disk)
            .status()
            .is_ok_and(|s| s.success());
        if !mounted {
            eprintln!("skipping: cannot mount tmpfs here");
            return;
        }
        std::fs::write(disk.join("hello.txt"), b"hi").unwrap();
        let disk_bytes = disk.as_os_str().as_bytes().to_vec();
        // Index only our tmpfs, whatever else the test machine has mounted.
        let skip = mountinfo::read_mounts()
            .unwrap()
            .into_iter()
            .filter(|m| m.mount_point != disk_bytes)
            .map(|m| String::from_utf8_lossy(&m.mount_point).into_owned())
            .collect();
        let cfg = Arc::new(Config {
            state_dir: tmp.path().join("state"),
            local: crate::config::LocalConfig {
                index_tmpfs: true,
                skip_mounts: skip,
                ..Default::default()
            },
            ..Default::default()
        });
        std::fs::create_dir_all(&cfg.state_dir).unwrap();
        let mounts =
            Arc::new(MountManager::open(tmp.path().join("etc"), tmp.path().join("run")).unwrap());
        let mgr = VolumeManager::new(cfg.clone(), mounts);

        mgr.sync();
        assert_eq!(mgr.volumes().len(), 1);
        let vol = mgr
            .find(&[disk_bytes.as_slice(), b"/hello.txt"].concat())
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while vol
            .vol
            .index
            .read()
            .lookup(evos_core::ROOT, b"hello.txt")
            .is_none()
        {
            assert!(
                std::time::Instant::now() < deadline,
                "volume was not scanned"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        mgr.sync();
        assert_eq!(mgr.volumes().len(), 1, "sync is idempotent");

        // Unmounting detaches the volume and leaves a snapshot behind.
        assert!(Command::new("umount")
            .arg(&disk)
            .status()
            .unwrap()
            .success());
        mgr.sync();
        assert!(mgr.volumes().is_empty());
        let snapshot = cfg.state_dir.join(snapshot_name(&disk_bytes));
        let (index, extra) = persist::load(&snapshot).unwrap();
        assert!(index.lookup(evos_core::ROOT, b"hello.txt").is_some());
        assert_eq!(SnapshotMeta::decode(&extra).unwrap().source, "evos-test");
    }
}
