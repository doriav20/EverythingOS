//! Driver for network shares: keep huge shares fresh without re-crawling.
//!
//! 1. The first scan is a resumable walk through an [`AdaptiveGate`], which
//!    finds the concurrency the server sustains and respects an ops/sec cap.
//!    If interrupted (offline, shutdown) the unfinished folders are saved in
//!    the checkpoint and the next run continues from there.
//! 2. On cifs mounts a background thread waits for SMB change
//!    notifications and the driver re-lists exactly the folders reported.
//! 3. Every folder has a hot/cold check schedule; due folders are swept in
//!    batches sized to the gate's current limit. Folders a user looks at are
//!    boosted, and long-unlisted folders get a full metadata refresh.
//! 4. When the share becomes unreachable the volume goes offline, keeping
//!    its index searchable, and resumes when the server is back.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{unbounded, Receiver, TryRecvError};
use evos_core::{EntryId, Index, ROOT};

use crate::changes::nearest_ancestor;
use crate::fsops::{self, CacheMode};
use crate::schedule::{ScheduleConfig, Scheduler};
use crate::smbnotify::{Notice, SmbNotifier};
use crate::sweep::sweep;
use crate::throttle::{AdaptiveGate, GateConfig};
use crate::volume::{Checkpoint, Phase, Volume, WatchMode};
use crate::walker::{walk, ExcludeFn, Gate, WalkOptions};

/// Network driver configuration.
#[derive(Clone)]
pub struct NetworkOptions {
    /// Upper bound on worker threads; the gate decides how many are busy.
    pub threads: usize,
    pub gate: GateConfig,
    pub schedule: ScheduleConfig,
    pub exclude: Option<ExcludeFn>,
    /// Use SMB change notifications on cifs mounts.
    pub smb_notify: bool,
    /// How often to probe an offline share.
    pub offline_retry: Duration,
    /// Treat the share as offline unless its root is a mount point.
    pub require_mount: bool,
}

impl Default for NetworkOptions {
    fn default() -> Self {
        NetworkOptions {
            threads: 64,
            gate: GateConfig::default(),
            schedule: ScheduleConfig::default(),
            exclude: None,
            smb_notify: true,
            offline_retry: Duration::from_secs(30),
            require_mount: true,
        }
    }
}

/// Is the share there? With `require_mount` the root must also be a mount
/// point: an unmounted share leaves an empty local folder behind, and
/// sweeping that would wipe the share's index.
fn share_available(root: &[u8], require_mount: bool) -> bool {
    let Ok(info) = fsops::stat_info(root, CacheMode::Revalidate) else {
        return false;
    };
    if !require_mount || info.mount_root {
        return true;
    }
    // Kernels without STATX_ATTR_MOUNT_ROOT: compare with the parent.
    let parent = match root.iter().rposition(|&b| b == b'/') {
        Some(0) | None => &b"/"[..],
        Some(cut) => &root[..cut],
    };
    fsops::stat_info(parent, CacheMode::Cached).is_ok_and(|p| p.dev != info.dev)
}

/// Delay before restarting SMB notifications after they stopped.
const NOTIFY_RETRY: Duration = Duration::from_secs(30);

fn is_cifs(fs_type: &str) -> bool {
    matches!(fs_type, "cifs" | "smb3")
}

/// Waits for SMB notifications on a background thread. The ioctl cannot be
/// interrupted, so the thread exits after the next notification once the
/// volume is stopping.
fn spawn_notifier(vol: Arc<Volume>) -> Option<Receiver<Notice>> {
    let mut notifier = match SmbNotifier::open(vol.root()) {
        Ok(n) => n,
        Err(e) => {
            tracing::info!("SMB notifications unavailable: {e}");
            return None;
        }
    };
    // Assume detailed notifications until the first request says otherwise.
    vol.update_status(|s| s.watch = WatchMode::SmbNotify { details: true });
    let (tx, rx) = unbounded();
    std::thread::Builder::new()
        .name("evos-smb-notify".into())
        .spawn(move || {
            while !vol.is_stopping() {
                match notifier.wait() {
                    Ok(notice) => {
                        vol.update_status(|s| {
                            s.watch = WatchMode::SmbNotify {
                                details: notifier.has_details(),
                            }
                        });
                        if tx.send(notice).is_err() {
                            return;
                        }
                        vol.notify();
                    }
                    Err(e) => {
                        tracing::info!("SMB notifications stopped: {e}");
                        vol.update_status(|s| s.watch = WatchMode::Sweeps);
                        return;
                    }
                }
            }
        })
        .ok()?;
    Some(rx)
}

/// Folders to re-list for the paths an SMB notification reported.
fn dirs_for_paths(ix: &Index, paths: &[Vec<u8>]) -> Vec<EntryId> {
    let mut dirs: Vec<EntryId> = paths
        .iter()
        .filter_map(|rel| {
            let parent = match rel.iter().rposition(|&b| b == b'/') {
                Some(cut) => fsops::join(ix.root_path(), &rel[..cut]),
                None => ix.root_path().to_vec(),
            };
            ix.resolve(&parent)
                .filter(|&id| ix.is_dir(id))
                .or_else(|| nearest_ancestor(ix, &parent))
        })
        .collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs
}

/// The folder a hinted path refers to (the path itself or its parent).
fn dir_for_hint(ix: &Index, path: &[u8]) -> Option<EntryId> {
    let id = ix.resolve(path).or_else(|| nearest_ancestor(ix, path))?;
    Some(if ix.is_dir(id) { id } else { ix.parent(id) })
}

/// Runs the driver until the volume is stopped.
pub fn run_network(vol: &Arc<Volume>, opts: &NetworkOptions) {
    let gate = Arc::new(AdaptiveGate::new(opts.gate.clone()));
    let scan_opts = WalkOptions {
        threads: opts.threads,
        cache: CacheMode::Cached,
        recurse_existing: true,
        exclude: opts.exclude.clone(),
        gate: Some(gate.clone() as Arc<dyn Gate>),
    };
    let check_opts = WalkOptions {
        cache: CacheMode::Revalidate,
        recurse_existing: false,
        ..scan_opts.clone()
    };
    let relist_opts = WalkOptions {
        recurse_existing: false,
        ..scan_opts.clone()
    };
    let cancel = vol.stop_flag();
    let mut sched = Scheduler::new(opts.schedule.clone());
    let wants_notify = opts.smb_notify && is_cifs(vol.fs_type());
    vol.update_status(|s| s.watch = WatchMode::Sweeps);
    let mut notices: Option<Receiver<Notice>> = None;
    // When to (re)start the notification thread; it ends when the share
    // becomes unreachable and must be restarted once it is back.
    let mut notify_at = wants_notify.then(Instant::now);
    let mut offline = false;

    let go_offline = |offline: &mut bool| {
        *offline = true;
        tracing::warn!(root = %String::from_utf8_lossy(vol.root()), "share unreachable");
        vol.update_status(|s| {
            s.phase = Phase::Offline;
            s.error = Some("share unreachable".into());
        });
    };

    while !vol.is_stopping() {
        let available = share_available(vol.root(), opts.require_mount);
        if offline && available {
            offline = false;
            vol.update_status(|s| {
                s.phase = Phase::Live;
                s.error = None;
            });
        } else if !available {
            if !offline {
                go_offline(&mut offline);
            }
            vol.wait(opts.offline_retry);
            continue;
        }

        let checkpoint = vol.checkpoint();
        if !checkpoint.scan_complete {
            vol.update_status(|s| s.phase = Phase::Scanning);
            let roots = if checkpoint.frontier.is_empty() {
                vec![ROOT]
            } else {
                checkpoint.frontier
            };
            let report = walk(&vol.index, &roots, &scan_opts, &vol.progress, cancel);
            if report.completed() {
                vol.set_checkpoint(Checkpoint {
                    scan_complete: true,
                    frontier: Vec::new(),
                });
                vol.update_status(|s| {
                    s.phase = Phase::Live;
                    s.last_scan = Some(SystemTime::now());
                });
            } else {
                vol.set_checkpoint(Checkpoint {
                    scan_complete: false,
                    frontier: report.frontier,
                });
                if report.unreachable {
                    go_offline(&mut offline);
                }
            }
            continue;
        }

        let now = Instant::now();
        if sched.is_empty() {
            // Fresh after the first scan or a restart from a snapshot.
            let ix = vol.index.read();
            sched.track_all(ix.live_ids().filter(|&id| ix.is_dir(id)), now);
            drop(ix);
            vol.update_status(|s| s.phase = Phase::Live);
        }

        if notices.is_none() && notify_at.is_some_and(|t| now >= t) {
            notices = spawn_notifier(vol.clone());
            notify_at = notices.is_none().then(|| now + NOTIFY_RETRY);
        }

        // SMB notifications: re-list exactly what the server reported.
        let mut relist = Vec::new();
        let mut notifier_ended = false;
        while let Some(rx) = &notices {
            match rx.try_recv() {
                Ok(Notice::Changed(changes)) => {
                    let paths: Vec<Vec<u8>> = changes.into_iter().map(|(_, p)| p).collect();
                    relist.extend(dirs_for_paths(&vol.index.read(), &paths));
                }
                Ok(Notice::Unspecified) => sched.boost_hot(2, now),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    notifier_ended = true;
                    break;
                }
            }
        }
        if notifier_ended {
            notices = None;
            notify_at = Some(now + NOTIFY_RETRY);
        }
        for path in vol.take_hints() {
            if let Some(dir) = dir_for_hint(&vol.index.read(), &path) {
                sched.boost(dir, now);
            }
        }
        if vol.take_rescan_request() {
            let ix = vol.index.read();
            for id in ix.live_ids().filter(|&id| ix.is_dir(id)) {
                sched.boost(id, now);
            }
        }

        if !relist.is_empty() {
            relist.sort_unstable();
            relist.dedup();
            let report = walk(&vol.index, &relist, &relist_opts, &vol.progress, cancel);
            for &id in &relist {
                sched.record(id, true, now);
            }
            sched.track_all(report.new_dirs.iter().copied(), now);
            vol.update_status(|s| s.last_change = Some(SystemTime::now()));
            if report.unreachable {
                go_offline(&mut offline);
                continue;
            }
        }

        let budget = (gate.stats().limit * 16).max(32);
        let due = sched.pop_due(now, budget);
        if !due.is_empty() {
            let (refresh, check): (Vec<EntryId>, Vec<EntryId>) = due
                .into_iter()
                .partition(|&id| sched.needs_refresh(id, now));
            if !refresh.is_empty() {
                let report = walk(&vol.index, &refresh, &relist_opts, &vol.progress, cancel);
                for &id in &refresh {
                    sched.record(id, false, now);
                    sched.record_listed(id, now);
                }
                sched.track_all(report.new_dirs.iter().copied(), now);
                if report.unreachable {
                    go_offline(&mut offline);
                    continue;
                }
            }
            if !check.is_empty() {
                let report = sweep(&vol.index, Some(&check), &check_opts, &vol.progress, cancel);
                {
                    let ix = vol.index.read();
                    for &id in &check {
                        if !ix.is_alive(id) || !ix.is_dir(id) {
                            sched.untrack(id);
                        } else {
                            sched.record(id, report.changed.binary_search(&id).is_ok(), now);
                        }
                    }
                }
                sched.track_all(report.walk.new_dirs.iter().copied(), now);
                vol.update_status(|s| {
                    s.last_sweep = Some(SystemTime::now());
                    if !report.changed.is_empty() {
                        s.last_change = Some(SystemTime::now());
                    }
                });
                if report.unreachable {
                    go_offline(&mut offline);
                    continue;
                }
            }
        }

        if vol.index.read().should_compact() {
            let map = vol.index.write().compact();
            sched.remap(&map);
        }
        vol.update_status(|s| {
            s.gate = Some(gate.stats());
            s.tracked_dirs = sched.len();
        });
        let wait = sched
            .next_due()
            .map_or(Duration::from_secs(5), |d| {
                d.saturating_duration_since(Instant::now())
            })
            .min(Duration::from_secs(5));
        vol.wait(wait);
    }
    vol.update_status(|s| s.phase = Phase::Stopped);
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_core::Meta;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn fast() -> NetworkOptions {
        NetworkOptions {
            threads: 8,
            schedule: ScheduleConfig {
                min_interval: Duration::from_millis(100),
                max_interval: Duration::from_millis(400),
                refresh_interval: Duration::from_secs(3600),
            },
            smb_notify: false,
            require_mount: false,
            ..Default::default()
        }
    }

    fn wait_for(vol: &Volume, cond: impl Fn(&Index) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond(&vol.index.read()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn p(root: &Path, rel: &str) -> Vec<u8> {
        [root.as_os_str().as_bytes(), rel.as_bytes()].concat()
    }

    fn start(checkpoint: Checkpoint, index: Index) -> (Arc<Volume>, std::thread::JoinHandle<()>) {
        let vol = Arc::new(Volume::new(index, "nfs4", checkpoint));
        let v = vol.clone();
        let opts = fast();
        (vol, std::thread::spawn(move || run_network(&v, &opts)))
    }

    #[test]
    fn scheduled_checks_find_deep_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        let (vol, driver) = start(
            Checkpoint::default(),
            Index::new(root.as_os_str().as_bytes(), Meta::dir(0)),
        );
        assert!(wait_for(&vol, |ix| ix
            .resolve(&p(root, "/a/b/c"))
            .is_some()));
        assert!(wait_for(&vol, |_| vol.status().tracked_dirs == 4));

        fs::write(root.join("a/b/c/new.txt"), b"x").unwrap();
        fs::create_dir_all(root.join("a/b/c/sub/deeper")).unwrap();
        assert!(wait_for(&vol, |ix| ix
            .resolve(&p(root, "/a/b/c/new.txt"))
            .is_some()));
        assert!(wait_for(&vol, |ix| ix
            .resolve(&p(root, "/a/b/c/sub/deeper"))
            .is_some()));
        assert!(
            wait_for(&vol, |_| vol.status().tracked_dirs == 6),
            "new folders get tracked"
        );

        fs::remove_dir_all(root.join("a/b")).unwrap();
        assert!(wait_for(&vol, |ix| ix.resolve(&p(root, "/a/b")).is_none()));
        let status = vol.status();
        assert_eq!(status.phase, Phase::Live);
        assert!(status.gate.is_some());
        vol.stop();
        driver.join().unwrap();
    }

    #[test]
    fn interrupted_scan_resumes_from_the_frontier() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("done")).unwrap();
        fs::create_dir_all(root.join("todo/inner")).unwrap();
        // State left by an interrupted scan: root listed, "todo" still queued.
        let mut ix = Index::new(root.as_os_str().as_bytes(), Meta::dir(0));
        ix.insert(ROOT, b"done", Meta::dir(0));
        let todo = ix.insert(ROOT, b"todo", Meta::dir(0));
        let cp = Checkpoint {
            scan_complete: false,
            frontier: vec![todo],
        };
        let (vol, driver) = start(cp, ix);
        assert!(wait_for(&vol, |ix| ix
            .resolve(&p(root, "/todo/inner"))
            .is_some()));
        assert!(wait_for(&vol, |_| vol.checkpoint().scan_complete));
        vol.stop();
        driver.join().unwrap();
    }

    #[test]
    fn unmounted_share_goes_offline_and_keeps_its_index() {
        // A plain folder stands in for the empty mount point left behind
        // by an unmounted share.
        let tmp = tempfile::tempdir().unwrap();
        let mut ix = Index::new(tmp.path().as_os_str().as_bytes(), Meta::dir(0));
        ix.insert(ROOT, b"remote-file.txt", Meta::file(1, 1));
        let vol = Arc::new(Volume::new(
            ix,
            "cifs",
            Checkpoint {
                scan_complete: true,
                frontier: vec![],
            },
        ));
        let opts = NetworkOptions {
            require_mount: true,
            ..fast()
        };
        let v = vol.clone();
        let driver = std::thread::spawn(move || run_network(&v, &opts));
        assert!(wait_for(&vol, |_| vol.status().phase == Phase::Offline));
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            vol.index.read().lookup(ROOT, b"remote-file.txt").is_some(),
            "index kept"
        );
        vol.stop();
        driver.join().unwrap();
    }

    #[test]
    fn notification_paths_map_to_folders() {
        let mut ix = Index::new(b"/mnt/share", Meta::dir(0));
        let docs = ix.insert(ROOT, b"docs", Meta::dir(0));
        let paths = vec![
            b"docs/new.txt".to_vec(),
            b"top.txt".to_vec(),
            b"docs/unknown/x".to_vec(),
        ];
        assert_eq!(dirs_for_paths(&ix, &paths), vec![ROOT, docs]);
        ix.insert(docs, b"f", Meta::file(0, 0));
        assert_eq!(dir_for_hint(&ix, b"/mnt/share/docs/f"), Some(docs));
        assert_eq!(dir_for_hint(&ix, b"/mnt/share/docs"), Some(docs));
    }
}
