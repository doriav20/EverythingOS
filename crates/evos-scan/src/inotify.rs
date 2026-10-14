//! Fallback change watcher using inotify.
//!
//! Used when fanotify cannot watch a filesystem (no file handle support,
//! older kernels with btrfs subvolumes, or no CAP_SYS_ADMIN). inotify needs
//! one watch per folder, so huge trees can exhaust
//! `fs.inotify.max_user_watches`; the watcher then reports itself as
//! exhausted and the caller falls back to periodic sweeps.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::time::Duration;

use evos_core::{EntryId, Index};
use inotify::{EventMask, Inotify, WatchDescriptor, WatchMask};

use crate::changes::Change;
use crate::fsops;

const MASK: WatchMask = WatchMask::CREATE
    .union(WatchMask::DELETE)
    .union(WatchMask::MODIFY)
    .union(WatchMask::ATTRIB)
    .union(WatchMask::CLOSE_WRITE)
    .union(WatchMask::MOVED_FROM)
    .union(WatchMask::MOVED_TO)
    .union(WatchMask::DONT_FOLLOW)
    .union(WatchMask::ONLYDIR)
    .union(WatchMask::EXCL_UNLINK);

/// Recursive watcher built from per-folder inotify watches.
pub struct InotifyWatcher {
    inotify: Inotify,
    paths: HashMap<WatchDescriptor, Vec<u8>>,
    buf: Vec<u8>,
    /// The kernel refused more watches; part of the tree is unwatched.
    pub exhausted: bool,
}

impl InotifyWatcher {
    pub fn new() -> io::Result<Self> {
        Ok(InotifyWatcher {
            inotify: Inotify::init()?,
            paths: HashMap::new(),
            buf: vec![0u8; 64 * 1024],
            exhausted: false,
        })
    }

    /// Number of folders being watched.
    pub fn watch_count(&self) -> usize {
        self.paths.len()
    }

    /// Watches one folder.
    pub fn watch_dir(&mut self, path: &[u8]) -> io::Result<()> {
        match self.inotify.watches().add(OsStr::from_bytes(path), MASK) {
            Ok(wd) => {
                self.paths.insert(wd, path.to_vec());
                Ok(())
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                self.exhausted = true;
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Watches `dir` and every folder below it in the index. Stops early
    /// if the watch limit is reached.
    pub fn watch_subtree(&mut self, index: &Index, dir: EntryId) {
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            if self.watch_dir(&index.path(d)).is_err() && self.exhausted {
                return;
            }
            stack.extend(index.children(d).filter(|&c| index.is_dir(c)));
        }
    }

    /// Waits up to `timeout` for events and appends the decoded changes.
    pub fn read_changes(&mut self, timeout: Duration, out: &mut Vec<Change>) -> io::Result<()> {
        let fd = self.inotify.as_fd();
        let mut fds = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        let ts = rustix::event::Timespec {
            tv_sec: timeout.as_secs() as _,
            tv_nsec: timeout.subsec_nanos() as _,
        };
        match rustix::event::poll(&mut fds, Some(&ts)) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => return Ok(()),
            Err(e) => return Err(e.into()),
        }

        let mut moved_from: HashMap<u32, (Vec<u8>, Vec<u8>)> = HashMap::new();
        let mut new_dirs = Vec::new();
        let mut renamed_dirs = Vec::new();
        let mut dropped = Vec::new();
        let events = match self.inotify.read_events(&mut self.buf) {
            Ok(events) => events,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        };
        for event in events {
            if event.mask.contains(EventMask::Q_OVERFLOW) {
                out.push(Change::Overflow);
                continue;
            }
            if event.mask.contains(EventMask::IGNORED) {
                dropped.push(event.wd.clone());
                continue;
            }
            let Some(dir) = self.paths.get(&event.wd).cloned() else {
                continue;
            };
            let Some(name) = event.name.map(|n| n.as_bytes().to_vec()) else {
                if event.mask.contains(EventMask::ATTRIB) {
                    out.push(Change::Dir { path: dir });
                }
                continue;
            };
            let is_dir = event.mask.contains(EventMask::ISDIR);
            if event.mask.contains(EventMask::MOVED_FROM) {
                moved_from.insert(event.cookie, (dir, name));
                continue;
            }
            if event.mask.contains(EventMask::MOVED_TO) {
                if let Some(from) = moved_from.remove(&event.cookie) {
                    if is_dir {
                        renamed_dirs
                            .push((fsops::join(&from.0, &from.1), fsops::join(&dir, &name)));
                    }
                    out.push(Change::Rename {
                        from,
                        to: (dir, name),
                    });
                    continue;
                }
            }
            if is_dir
                && event
                    .mask
                    .intersects(EventMask::CREATE | EventMask::MOVED_TO)
            {
                new_dirs.push(fsops::join(&dir, &name));
            }
            out.push(Change::Entry { dir, name });
        }
        // Moved out of the watched tree.
        for (_, (dir, name)) in moved_from {
            out.push(Change::Entry { dir, name });
        }
        for wd in dropped {
            self.paths.remove(&wd);
        }
        for (from, to) in renamed_dirs {
            self.rebase(&from, &to);
        }
        for dir in new_dirs {
            let _ = self.watch_dir(&dir);
        }
        Ok(())
    }

    /// Updates watched paths after a folder moved from `from` to `to`.
    fn rebase(&mut self, from: &[u8], to: &[u8]) {
        for path in self.paths.values_mut() {
            let inside =
                path.starts_with(from) && (path.len() == from.len() || path[from.len()] == b'/');
            if inside {
                let mut moved = to.to_vec();
                moved.extend_from_slice(&path[from.len()..]);
                *path = moved;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_core::{Meta, ROOT};
    use std::fs;
    use std::path::Path;
    use std::time::Instant;

    fn b(p: &Path) -> Vec<u8> {
        p.as_os_str().as_bytes().to_vec()
    }

    fn collect(w: &mut InotifyWatcher, until: impl Fn(&[Change]) -> bool) -> Vec<Change> {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !until(&out) {
            w.read_changes(Duration::from_millis(100), &mut out)
                .unwrap();
        }
        out
    }

    #[test]
    fn recursive_watch_follows_new_and_moved_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        let mut ix = Index::new(&b(root), Meta::dir(0));
        let a = ix.insert(ROOT, b"a", Meta::dir(0));
        ix.insert(a, b"b", Meta::dir(0));
        let mut w = InotifyWatcher::new().unwrap();
        w.watch_subtree(&ix, ROOT);
        assert_eq!(w.watch_count(), 3);

        fs::write(root.join("a/b/f.txt"), b"1").unwrap();
        let created = Change::Entry {
            dir: b(&root.join("a/b")),
            name: b"f.txt".to_vec(),
        };
        assert!(collect(&mut w, |c| c.contains(&created)).contains(&created));

        // A new folder is watched automatically.
        fs::create_dir(root.join("new")).unwrap();
        collect(&mut w, |c| {
            c.iter()
                .any(|x| matches!(x, Change::Entry { name, .. } if name == b"new"))
        });
        fs::write(root.join("new/g"), b"").unwrap();
        let inner = Change::Entry {
            dir: b(&root.join("new")),
            name: b"g".to_vec(),
        };
        assert!(collect(&mut w, |c| c.contains(&inner)).contains(&inner));

        // Renames pair up, and watches below a moved folder get new paths.
        fs::rename(root.join("a"), root.join("z")).unwrap();
        let renamed = Change::Rename {
            from: (b(root), b"a".to_vec()),
            to: (b(root), b"z".to_vec()),
        };
        assert!(collect(&mut w, |c| c.contains(&renamed)).contains(&renamed));
        fs::write(root.join("z/b/h"), b"").unwrap();
        let moved = Change::Entry {
            dir: b(&root.join("z/b")),
            name: b"h".to_vec(),
        };
        assert!(collect(&mut w, |c| c.contains(&moved)).contains(&moved));
    }
}
