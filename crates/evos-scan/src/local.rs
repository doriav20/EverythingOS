//! Driver for local filesystems: scan once, then follow every change live.
//!
//! The watcher is armed *before* the first scan, so changes made while the
//! scan runs queue up in the kernel and are applied right after it. A
//! volume restored from a snapshot is reconciled with a sweep instead of
//! being rescanned. Lost events (queue overflow) also trigger a sweep, and
//! an occasional safety sweep guards against anything the watcher missed.

use std::time::{Duration, Instant, SystemTime};

use crate::changes::{apply_changes, coalesce, Change};
use crate::fanotify::FanotifyWatcher;
use crate::fsops::CacheMode;
use crate::inotify::InotifyWatcher;
use crate::sweep::sweep;
use crate::volume::{Checkpoint, Phase, Volume, WatchMode};
use crate::walker::{walk, ExcludeFn, WalkOptions};
use evos_core::{EntryId, ROOT};

/// Local driver configuration.
#[derive(Clone)]
pub struct LocalOptions {
    pub threads: usize,
    pub exclude: Option<ExcludeFn>,
    /// Interval between safety sweeps, and between sweeps when no watcher
    /// is available at all.
    pub safety_sweep: Duration,
    /// Disable fanotify (tests, unprivileged runs).
    pub allow_fanotify: bool,
}

impl Default for LocalOptions {
    fn default() -> Self {
        LocalOptions {
            threads: WalkOptions::default().threads,
            exclude: None,
            safety_sweep: Duration::from_secs(24 * 3600),
            allow_fanotify: true,
        }
    }
}

enum Watcher {
    Fanotify(FanotifyWatcher),
    Inotify(InotifyWatcher),
    None,
}

impl Watcher {
    fn open(vol: &Volume, opts: &LocalOptions) -> Watcher {
        if opts.allow_fanotify {
            match FanotifyWatcher::new(vol.root()) {
                Ok(w) => return Watcher::Fanotify(w),
                Err(e) => tracing::info!(
                    root = %String::from_utf8_lossy(vol.root()),
                    "fanotify unavailable ({e}), falling back to inotify"
                ),
            }
        }
        match InotifyWatcher::new() {
            Ok(w) => Watcher::Inotify(w),
            Err(e) => {
                tracing::warn!("inotify unavailable ({e}); relying on sweeps");
                Watcher::None
            }
        }
    }

    fn mode(&self) -> WatchMode {
        match self {
            Watcher::Fanotify(_) => WatchMode::Fanotify,
            Watcher::Inotify(w) if !w.exhausted => WatchMode::Inotify,
            _ => WatchMode::Sweeps,
        }
    }

    fn read(&mut self, timeout: Duration, out: &mut Vec<Change>) {
        let result = match self {
            Watcher::Fanotify(w) => w.read_changes(timeout, out),
            Watcher::Inotify(w) => w.read_changes(timeout, out),
            Watcher::None => {
                std::thread::sleep(timeout);
                Ok(())
            }
        };
        if let Err(e) = result {
            tracing::warn!("watch read failed: {e}");
            out.push(Change::Overflow);
            std::thread::sleep(timeout);
        }
    }

    fn watch_new(&mut self, vol: &Volume, dirs: &[EntryId]) {
        if let Watcher::Inotify(w) = self {
            let ix = vol.index.read();
            for &d in dirs {
                if ix.is_alive(d) {
                    w.watch_subtree(&ix, d);
                }
            }
        }
    }
}

/// Runs the driver until the volume is stopped.
pub fn run_local(vol: &Volume, opts: &LocalOptions) {
    let mut watcher = Watcher::open(vol, opts);
    let walk_opts = WalkOptions {
        threads: opts.threads,
        cache: CacheMode::AsStat,
        recurse_existing: true,
        exclude: opts.exclude.clone(),
        gate: None,
    };
    let cancel = vol.stop_flag();

    if vol.checkpoint().scan_complete {
        vol.update_status(|s| s.phase = Phase::Reconciling);
        sweep(&vol.index, None, &walk_opts, &vol.progress, cancel);
        vol.update_status(|s| s.last_sweep = Some(SystemTime::now()));
    } else {
        vol.update_status(|s| s.phase = Phase::Scanning);
        let report = walk(&vol.index, &[ROOT], &walk_opts, &vol.progress, cancel);
        if !report.completed() {
            vol.set_checkpoint(Checkpoint {
                scan_complete: false,
                frontier: report.frontier,
            });
            vol.update_status(|s| s.phase = Phase::Stopped);
            return;
        }
        vol.set_checkpoint(Checkpoint {
            scan_complete: true,
            frontier: Vec::new(),
        });
        vol.update_status(|s| s.last_scan = Some(SystemTime::now()));
    }
    watcher.watch_new(vol, &[ROOT]);
    vol.update_status(|s| {
        s.phase = Phase::Live;
        s.watch = watcher.mode();
    });

    let sweep_every = match watcher.mode() {
        WatchMode::Sweeps => opts.safety_sweep.min(Duration::from_secs(600)),
        _ => opts.safety_sweep,
    };
    let mut last_sweep = Instant::now();
    while !vol.is_stopping() {
        let mut changes = Vec::new();
        watcher.read(Duration::from_millis(250), &mut changes);
        if !changes.is_empty() {
            // Let a burst settle so duplicates coalesce.
            let settle = Instant::now() + Duration::from_millis(50);
            while Instant::now() < settle && changes.len() < 100_000 {
                watcher.read(Duration::from_millis(10), &mut changes);
            }
        }
        if let Some(ex) = &opts.exclude {
            changes.retain(|c| match c {
                Change::Entry { dir, .. } => !ex(dir),
                Change::Dir { path } => !ex(path),
                Change::Rename { from, to } => !(ex(&from.0) && ex(&to.0)),
                Change::Overflow => true,
            });
        }

        let mut need_sweep = vol.take_rescan_request() || last_sweep.elapsed() >= sweep_every;
        if !changes.is_empty() {
            let report = apply_changes(&vol.index, &coalesce(changes), CacheMode::AsStat);
            need_sweep |= report.overflow;
            if !report.scan.is_empty() {
                walk(&vol.index, &report.scan, &walk_opts, &vol.progress, cancel);
                watcher.watch_new(vol, &report.scan);
            }
            if !report.relist.is_empty() {
                let relist = WalkOptions {
                    recurse_existing: false,
                    ..walk_opts.clone()
                };
                let r = walk(&vol.index, &report.relist, &relist, &vol.progress, cancel);
                watcher.watch_new(vol, &r.new_dirs);
            }
            if report.applied > 0 {
                vol.update_status(|s| s.last_change = Some(SystemTime::now()));
            }
        }
        if need_sweep {
            let r = sweep(&vol.index, None, &walk_opts, &vol.progress, cancel);
            watcher.watch_new(vol, &r.walk.new_dirs);
            last_sweep = Instant::now();
            vol.update_status(|s| {
                s.last_sweep = Some(SystemTime::now());
                s.watch = watcher.mode();
            });
        }
        if vol.index.read().should_compact() {
            vol.index.write().compact();
        }
    }
    vol.update_status(|s| s.phase = Phase::Stopped);
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_core::{Index, Meta};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::Arc;

    fn wait_for(vol: &Volume, cond: impl Fn(&evos_core::Index) -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if cond(&vol.index.read()) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn path(root: &Path, rel: &str) -> Vec<u8> {
        [root.as_os_str().as_bytes(), rel.as_bytes()].concat()
    }

    fn live_updates(root: &Path, opts: LocalOptions) {
        let root = root.to_path_buf();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/before.txt"), b"x").unwrap();
        let vol = Arc::new(Volume::new(
            Index::new(root.as_os_str().as_bytes(), Meta::dir(0)),
            "tmpfs",
            Checkpoint::default(),
        ));
        let driver = {
            let vol = vol.clone();
            std::thread::spawn(move || run_local(&vol, &opts))
        };
        assert!(wait_for(&vol, |ix| ix
            .resolve(&path(&root, "/a/before.txt"))
            .is_some()));
        assert!(wait_for(&vol, |_| vol.status().phase == Phase::Live));

        fs::create_dir_all(root.join("a/new/deeper")).unwrap();
        fs::write(root.join("a/new/deeper/f.txt"), b"hello").unwrap();
        assert!(wait_for(&vol, |ix| ix
            .resolve(&path(&root, "/a/new/deeper/f.txt"))
            .is_some()));

        fs::rename(root.join("a/new"), root.join("moved")).unwrap();
        assert!(wait_for(&vol, |ix| ix
            .resolve(&path(&root, "/moved/deeper/f.txt"))
            .is_some()));
        assert!(wait_for(&vol, |ix| ix
            .resolve(&path(&root, "/a/new"))
            .is_none()));

        fs::remove_file(root.join("a/before.txt")).unwrap();
        assert!(wait_for(&vol, |ix| ix
            .resolve(&path(&root, "/a/before.txt"))
            .is_none()));

        vol.stop();
        driver.join().unwrap();
        assert_eq!(vol.status().phase, Phase::Stopped);
    }

    #[test]
    fn follows_changes_with_inotify() {
        let tmp = tempfile::tempdir().unwrap();
        live_updates(
            tmp.path(),
            LocalOptions {
                allow_fanotify: false,
                ..Default::default()
            },
        );
    }

    #[test]
    fn follows_changes_with_fanotify() {
        let tmp = tempfile::tempdir().unwrap();
        let mounted = std::process::Command::new("mount")
            .args(["-t", "tmpfs", "evos-test"])
            .arg(tmp.path())
            .status()
            .is_ok_and(|s| s.success());
        if !mounted {
            eprintln!("skipping: cannot mount tmpfs here");
            return;
        }
        live_updates(tmp.path(), LocalOptions::default());
        let _ = std::process::Command::new("umount")
            .arg(tmp.path())
            .status();
    }

    #[test]
    fn reconciles_restored_volumes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("d")).unwrap();
        // An index restored from a snapshot that is missing d/late.txt.
        let mut ix = Index::new(root.as_os_str().as_bytes(), Meta::dir(0));
        ix.insert(ROOT, b"d", Meta::dir(0));
        fs::write(root.join("d/late.txt"), b"").unwrap();
        let vol = Arc::new(Volume::new(
            ix,
            "ext4",
            Checkpoint {
                scan_complete: true,
                frontier: vec![],
            },
        ));
        let opts = LocalOptions {
            allow_fanotify: false,
            ..Default::default()
        };
        let driver = {
            let vol = vol.clone();
            std::thread::spawn(move || run_local(&vol, &opts))
        };
        assert!(wait_for(&vol, |ix| ix
            .resolve(&path(root, "/d/late.txt"))
            .is_some()));
        vol.stop();
        driver.join().unwrap();
    }
}
