//! Directory-mtime-guided change detection.
//!
//! Creating, deleting or renaming an entry updates its parent folder's
//! mtime. A sweep therefore only stats folders (typically a tenth of all
//! entries) and re-lists the ones whose mtime moved, instead of re-listing
//! everything. Deep changes are still found because every folder is checked
//! individually; changes do not need to bubble up.
//!
//! This reconciles local volumes after downtime or a watch overflow, and is
//! the main freshness mechanism for network shares, where it runs with
//! [`CacheMode::Revalidate`] to bypass the client attribute cache.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use evos_core::{EntryId, Index, ROOT};
use parking_lot::{Mutex, RwLock};

use crate::fsops::{self, CacheMode};
use crate::walker::{walk, OpOutcome, Progress, WalkOptions, WalkReport};

/// Summary of a sweep.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Folders whose mtime was checked.
    pub checked: u64,
    /// Folders found changed (and re-listed).
    pub changed: Vec<EntryId>,
    /// Folders that could not be checked because the sweep stopped.
    pub unchecked: u64,
    pub unreachable: bool,
    /// The re-listing pass.
    pub walk: WalkReport,
}

/// Checks folders for changes and re-lists the changed ones.
///
/// `dirs` limits the check to some folders; `None` checks every folder of
/// the volume. `opts.cache` should be [`CacheMode::Revalidate`] for network
/// filesystems.
pub fn sweep(
    index: &RwLock<Index>,
    dirs: Option<&[EntryId]>,
    opts: &WalkOptions,
    progress: &Progress,
    cancel: &AtomicBool,
) -> SweepReport {
    let (candidates, volume_dev) = {
        let ix = index.read();
        let ids: Vec<EntryId> = match dirs {
            Some(d) => d
                .iter()
                .copied()
                .filter(|&id| ix.is_alive(id) && ix.is_dir(id))
                .collect(),
            None => ix.live_ids().filter(|&id| ix.is_dir(id)).collect(),
        };
        let candidates: Vec<(EntryId, Vec<u8>, i64)> = ids
            .into_iter()
            .map(|id| (id, ix.path(id), ix.mtime(id)))
            .filter(|(_, path, _)| !opts.exclude.as_ref().is_some_and(|ex| ex(path)))
            .collect();
        let dev = fsops::stat_info(ix.root_path(), CacheMode::Cached)
            .ok()
            .map(|s| s.dev);
        (candidates, dev)
    };

    let mut report = SweepReport::default();
    let changed = Mutex::new(Vec::new());
    let cursor = AtomicUsize::new(0);
    let checked = AtomicUsize::new(0);
    let halt = AtomicBool::new(false);
    let unreachable = AtomicBool::new(false);

    std::thread::scope(|scope| {
        for _ in 0..opts.threads.max(1) {
            scope.spawn(|| loop {
                if halt.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
                    return;
                }
                let i = cursor.fetch_add(1, Ordering::Relaxed);
                let Some((id, path, stored)) = candidates.get(i) else {
                    return;
                };
                if let Some(gate) = &opts.gate {
                    gate.begin();
                }
                let started = Instant::now();
                let result = fsops::stat_info(path, opts.cache);
                let outcome = match &result {
                    Ok(_) => OpOutcome::Ok,
                    Err(e) if fsops::is_unreachable(e) => OpOutcome::Unreachable,
                    Err(_) => OpOutcome::Error,
                };
                if let Some(gate) = &opts.gate {
                    gate.end(started.elapsed(), outcome);
                }
                checked.fetch_add(1, Ordering::Relaxed);
                match result {
                    // Another filesystem is mounted here: not ours to list.
                    Ok(info)
                        if *id != ROOT && (info.mount_root || Some(info.dev) != volume_dev) => {}
                    Ok(info) if info.meta.mtime != *stored || !info.meta.is_dir() => {
                        changed.lock().push(*id);
                    }
                    Ok(_) => {}
                    Err(_) if outcome == OpOutcome::Unreachable => {
                        unreachable.store(true, Ordering::Relaxed);
                        halt.store(true, Ordering::Relaxed);
                    }
                    // Vanished folders are handled when their parent is re-listed.
                    Err(_) => {}
                }
            });
        }
    });

    report.checked = checked.load(Ordering::Relaxed) as u64;
    report.unchecked = candidates.len() as u64 - report.checked;
    report.unreachable = unreachable.load(Ordering::Relaxed);
    let mut changed = changed.into_inner();
    changed.sort_unstable();
    if !report.unreachable && !changed.is_empty() {
        let relist = WalkOptions {
            recurse_existing: false,
            ..opts.clone()
        };
        report.walk = walk(index, &changed, &relist, progress, cancel);
        report.unreachable = report.walk.unreachable;
    }
    report.changed = changed;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_core::Meta;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn snapshot(index: &RwLock<Index>) -> Vec<(String, u64)> {
        let ix = index.read();
        let root_len = ix.root_path().len();
        let mut v: Vec<(String, u64)> = ix
            .live_ids()
            .skip(1)
            .map(|id| {
                (
                    String::from_utf8(ix.path(id)[root_len..].to_vec()).unwrap(),
                    ix.size(id),
                )
            })
            .collect();
        v.sort();
        v
    }

    fn fresh_scan(root: &Path) -> RwLock<Index> {
        let index = RwLock::new(Index::new(root.as_os_str().as_bytes(), Meta::dir(0)));
        walk(
            &index,
            &[ROOT],
            &WalkOptions::default(),
            &Progress::default(),
            &AtomicBool::new(false),
        );
        index
    }

    #[test]
    fn sweep_finds_deep_changes_and_only_relists_changed_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for d in ["a/b/c", "x/y", "quiet/one", "quiet/two"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("a/b/c/deep.txt"), b"1").unwrap();
        fs::write(root.join("x/y/moved.txt"), b"22").unwrap();
        let index = fresh_scan(root);

        // Changes deep in the tree; their ancestors' mtimes do not change.
        fs::write(root.join("a/b/c/new.txt"), b"333").unwrap();
        fs::remove_file(root.join("a/b/c/deep.txt")).unwrap();
        fs::rename(root.join("x/y/moved.txt"), root.join("a/moved.txt")).unwrap();
        fs::create_dir_all(root.join("x/y/fresh/sub")).unwrap();

        let report = sweep(
            &index,
            None,
            &WalkOptions::default(),
            &Progress::default(),
            &AtomicBool::new(false),
        );
        assert!(!report.unreachable);
        assert_eq!(report.checked, 9, "root + 8 folders");
        assert_eq!(report.changed.len(), 3, "a, a/b/c, x/y");
        assert_eq!(snapshot(&index), snapshot(&fresh_scan(root)));

        let again = sweep(
            &index,
            None,
            &WalkOptions::default(),
            &Progress::default(),
            &AtomicBool::new(false),
        );
        assert!(again.changed.is_empty(), "a second sweep finds nothing");
    }

    #[test]
    fn sweep_of_selected_folders_ignores_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        fs::create_dir_all(tmp.path().join("b")).unwrap();
        let index = fresh_scan(tmp.path());
        fs::write(tmp.path().join("a/f"), b"").unwrap();
        fs::write(tmp.path().join("b/g"), b"").unwrap();
        let b = index.read().lookup(ROOT, b"b").unwrap();
        let report = sweep(
            &index,
            Some(&[b]),
            &WalkOptions::default(),
            &Progress::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(report.changed, vec![b]);
        let names: Vec<String> = snapshot(&index).into_iter().map(|(p, _)| p).collect();
        assert!(names.contains(&"/b/g".to_owned()));
        assert!(!names.contains(&"/a/f".to_owned()));
    }
}
