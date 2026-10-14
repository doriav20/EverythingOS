//! Applying filesystem change notifications to an index.
//!
//! Watchers report *where* something changed, not what the change was:
//! every notification is answered by stat'ing the entry again and making the
//! index agree with the filesystem. This is idempotent and immune to event
//! reordering or coalescing, so a burst of events can be deduplicated
//! freely and replayed after a concurrent scan.

use std::collections::HashSet;
use std::io;

use evos_core::{EntryId, Index, ROOT};
use parking_lot::RwLock;

use crate::fsops::{self, CacheMode};

/// A change reported by a watcher. Paths are absolute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Change {
    /// Something happened to `name` inside folder `dir`.
    Entry { dir: Vec<u8>, name: Vec<u8> },
    /// `from` was renamed to `to` (each a folder plus a name).
    Rename {
        from: (Vec<u8>, Vec<u8>),
        to: (Vec<u8>, Vec<u8>),
    },
    /// The folder itself changed (attributes, mtime).
    Dir { path: Vec<u8> },
    /// Events were lost; the volume must be reconciled.
    Overflow,
}

/// Follow-up work after applying changes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub applied: usize,
    /// New folders whose contents must be scanned.
    pub scan: Vec<EntryId>,
    /// Folders whose parent was unknown; re-list them to catch up.
    pub relist: Vec<EntryId>,
    /// Events were lost; run a sweep.
    pub overflow: bool,
}

/// Drops duplicate changes, keeping the last occurrence of each, but keeps
/// renames in order relative to everything else.
pub fn coalesce(changes: Vec<Change>) -> Vec<Change> {
    let mut seen = HashSet::new();
    let mut out: Vec<Change> = changes
        .into_iter()
        .rev()
        .filter(|c| matches!(c, Change::Rename { .. }) || seen.insert(c.clone()))
        .collect();
    out.reverse();
    out
}

enum Observed {
    Exists(evos_core::Meta),
    Missing,
    Unknown,
}

fn observe(path: &[u8], cache: CacheMode) -> Observed {
    match fsops::stat(path, cache) {
        Ok(meta) => Observed::Exists(meta),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Observed::Missing,
        Err(_) => Observed::Unknown,
    }
}

/// Applies changes. Stat calls happen before the write lock is taken.
pub fn apply_changes(index: &RwLock<Index>, changes: &[Change], cache: CacheMode) -> ApplyReport {
    let mut report = ApplyReport::default();
    // Phase 1: observe the filesystem without holding the lock.
    let observed: Vec<Vec<Observed>> = changes
        .iter()
        .map(|c| match c {
            Change::Entry { dir, name } => vec![observe(&fsops::join(dir, name), cache)],
            Change::Rename { from, to } => vec![
                observe(&fsops::join(&from.0, &from.1), cache),
                observe(&fsops::join(&to.0, &to.1), cache),
            ],
            Change::Dir { path } => vec![observe(path, cache)],
            Change::Overflow => vec![],
        })
        .collect();

    // Phase 2: reconcile under the write lock.
    let mut ix = index.write();
    for (change, seen) in changes.iter().zip(observed) {
        match change {
            Change::Overflow => report.overflow = true,
            Change::Dir { path } => {
                if let (Some(id), Observed::Exists(meta)) = (ix.resolve(path), &seen[0]) {
                    if ix.is_dir(id) == meta.is_dir() {
                        ix.set_meta(id, *meta);
                        report.applied += 1;
                    }
                }
            }
            Change::Entry { dir, name } => {
                entry(&mut ix, dir, name, &seen[0], &mut report);
            }
            Change::Rename { from, to } => {
                let moved = match (ix.resolve(&from.0), ix.resolve(&to.0)) {
                    (Some(src_dir), Some(dst_dir)) => match ix.lookup(src_dir, &from.1) {
                        // Keep the id (and the subtree) of moved folders.
                        Some(id)
                            if matches!(seen[1], Observed::Exists(_)) && ix.is_dir(dst_dir) =>
                        {
                            ix.rename(id, dst_dir, &to.1)
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if moved {
                    report.applied += 1;
                } else {
                    entry(&mut ix, &from.0, &from.1, &seen[0], &mut report);
                }
                entry(&mut ix, &to.0, &to.1, &seen[1], &mut report);
            }
        }
    }
    report.scan.sort_unstable();
    report.scan.dedup();
    report.relist.sort_unstable();
    report.relist.dedup();
    report
}

fn entry(ix: &mut Index, dir: &[u8], name: &[u8], seen: &Observed, report: &mut ApplyReport) {
    let Some(dir_id) = ix.resolve(dir).filter(|&id| ix.is_dir(id)) else {
        // The parent is not indexed yet; re-list its nearest known ancestor.
        if let Some(anc) = nearest_ancestor(ix, dir) {
            report.relist.push(anc);
        }
        return;
    };
    match seen {
        Observed::Exists(meta) => {
            let (id, created) = ix.upsert(dir_id, name, *meta);
            if created && meta.is_dir() {
                report.scan.push(id);
            }
            report.applied += 1;
        }
        Observed::Missing => {
            if let Some(id) = ix.lookup(dir_id, name) {
                ix.remove(id);
                report.applied += 1;
            }
        }
        Observed::Unknown => {}
    }
}

fn nearest_ancestor(ix: &Index, path: &[u8]) -> Option<EntryId> {
    let mut p = path;
    loop {
        if let Some(id) = ix.resolve(p).filter(|&id| ix.is_dir(id)) {
            return Some(id);
        }
        let cut = p.iter().rposition(|&b| b == b'/')?;
        p = if cut == 0 { b"/" } else { &p[..cut] };
        if ix.relative(p).is_none() {
            return ix.relative(path).is_some().then_some(ROOT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walker::{walk, Progress, WalkOptions};
    use evos_core::Meta;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;

    fn b(p: &Path) -> Vec<u8> {
        p.as_os_str().as_bytes().to_vec()
    }

    fn scanned(root: &Path) -> RwLock<Index> {
        let index = RwLock::new(Index::new(&b(root), Meta::dir(0)));
        walk(
            &index,
            &[ROOT],
            &WalkOptions::default(),
            &Progress::default(),
            &AtomicBool::new(false),
        );
        index
    }

    fn entry(dir: &Path, name: &str) -> Change {
        Change::Entry {
            dir: b(dir),
            name: name.as_bytes().to_vec(),
        }
    }

    #[test]
    fn create_modify_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("old"), b"x").unwrap();
        let index = scanned(root);

        fs::write(root.join("new"), b"12345").unwrap();
        fs::write(root.join("old"), b"xyz").unwrap();
        fs::create_dir(root.join("dir")).unwrap();
        let report = apply_changes(
            &index,
            &[entry(root, "new"), entry(root, "old"), entry(root, "dir")],
            CacheMode::AsStat,
        );
        assert_eq!(report.applied, 3);
        let ix = index.read();
        let new = ix.lookup(ROOT, b"new").unwrap();
        assert_eq!(ix.size(new), 5);
        assert_eq!(ix.size(ix.lookup(ROOT, b"old").unwrap()), 3);
        assert_eq!(report.scan, vec![ix.lookup(ROOT, b"dir").unwrap()]);
        drop(ix);

        fs::remove_file(root.join("new")).unwrap();
        apply_changes(&index, &[entry(root, "new")], CacheMode::AsStat);
        assert!(index.read().lookup(ROOT, b"new").is_none());
    }

    #[test]
    fn rename_keeps_folder_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/inner")).unwrap();
        fs::create_dir(root.join("b")).unwrap();
        let index = scanned(root);
        let a = index.read().lookup(ROOT, b"a").unwrap();

        fs::rename(root.join("a"), root.join("b/moved")).unwrap();
        let change = Change::Rename {
            from: (b(root), b"a".to_vec()),
            to: (b(&root.join("b")), b"moved".to_vec()),
        };
        let report = apply_changes(&index, &[change], CacheMode::AsStat);
        assert!(report.scan.is_empty(), "no rescan for a known folder");
        let ix = index.read();
        assert!(ix.is_alive(a));
        assert_eq!(ix.path(a), [b(root), b"/b/moved".to_vec()].concat());
        assert!(ix
            .resolve(&[b(root), b"/b/moved/inner".to_vec()].concat())
            .is_some());
    }

    #[test]
    fn unknown_parent_triggers_relist() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let index = scanned(root);
        fs::create_dir_all(root.join("x/y")).unwrap();
        fs::write(root.join("x/y/f"), b"").unwrap();
        let report = apply_changes(&index, &[entry(&root.join("x/y"), "f")], CacheMode::AsStat);
        assert_eq!(report.relist, vec![ROOT]);
    }

    #[test]
    fn coalesce_keeps_last_and_renames() {
        let e = |n: &str| Change::Entry {
            dir: b"/d".to_vec(),
            name: n.as_bytes().to_vec(),
        };
        let r = Change::Rename {
            from: (b"/d".to_vec(), b"a".to_vec()),
            to: (b"/d".to_vec(), b"b".to_vec()),
        };
        let out = coalesce(vec![
            e("a"),
            e("b"),
            r.clone(),
            e("a"),
            r.clone(),
            Change::Overflow,
            Change::Overflow,
        ]);
        assert_eq!(out, vec![e("b"), r.clone(), e("a"), r, Change::Overflow]);
    }
}
