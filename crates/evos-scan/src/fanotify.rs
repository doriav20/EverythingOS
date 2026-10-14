//! Filesystem-wide change notification with fanotify.
//!
//! One fanotify group per filesystem with a `FAN_MARK_FILESYSTEM` mark sees
//! every create, delete, move and modification anywhere on it, without the
//! per-directory watches (and watch limits) of inotify. With
//! `FAN_REPORT_DFID_NAME` each event carries a file handle of the parent
//! folder plus the entry name; handles are turned back into paths with
//! `open_by_handle_at` and `/proc/self/fd`. This is the closest Linux
//! equivalent of the NTFS USN journal Everything relies on.
//!
//! Requires `CAP_SYS_ADMIN` and a filesystem that supports file handles.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use crate::changes::Change;

// From <linux/fanotify.h>.
const FAN_MODIFY: u64 = 0x2;
const FAN_ATTRIB: u64 = 0x4;
const FAN_CLOSE_WRITE: u64 = 0x8;
const FAN_MOVED_FROM: u64 = 0x40;
const FAN_MOVED_TO: u64 = 0x80;
const FAN_CREATE: u64 = 0x100;
const FAN_DELETE: u64 = 0x200;
const FAN_DELETE_SELF: u64 = 0x400;
const FAN_MOVE_SELF: u64 = 0x800;
const FAN_Q_OVERFLOW: u64 = 0x4000;
const FAN_RENAME: u64 = 0x1000_0000;
const FAN_ONDIR: u64 = 0x4000_0000;

const FAN_CLOEXEC: u32 = 0x1;
const FAN_NONBLOCK: u32 = 0x2;
const FAN_CLASS_NOTIF: u32 = 0x0;
const FAN_UNLIMITED_QUEUE: u32 = 0x10;
const FAN_REPORT_DIR_FID: u32 = 0x400;
const FAN_REPORT_NAME: u32 = 0x800;

const FAN_MARK_ADD: u32 = 0x1;
const FAN_MARK_FILESYSTEM: u32 = 0x100;

const INFO_DFID_NAME: u8 = 2;
const INFO_DFID: u8 = 3;
const INFO_OLD_DFID_NAME: u8 = 10;
const INFO_NEW_DFID_NAME: u8 = 12;

const METADATA_LEN: usize = 24;
const FSID_LEN: usize = 8;

const BASE_MASK: u64 = FAN_CREATE
    | FAN_DELETE
    | FAN_MODIFY
    | FAN_ATTRIB
    | FAN_CLOSE_WRITE
    | FAN_DELETE_SELF
    | FAN_MOVE_SELF
    | FAN_ONDIR;

/// A fanotify group watching one whole filesystem.
pub struct FanotifyWatcher {
    fd: OwnedFd,
    mount_fd: OwnedFd,
    /// FAN_RENAME (Linux 5.17+) reports both ends of a move in one event.
    pub rename_events: bool,
    buf: Vec<u8>,
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

impl FanotifyWatcher {
    /// Watches the filesystem mounted at `mount_path`.
    pub fn new(mount_path: &[u8]) -> io::Result<Self> {
        let flags = FAN_CLASS_NOTIF
            | FAN_CLOEXEC
            | FAN_NONBLOCK
            | FAN_UNLIMITED_QUEUE
            | FAN_REPORT_DIR_FID
            | FAN_REPORT_NAME;
        // SAFETY: plain syscall with constant arguments.
        let raw = unsafe {
            libc::fanotify_init(
                flags,
                (libc::O_RDONLY | libc::O_LARGEFILE | libc::O_CLOEXEC) as u32,
            )
        };
        if raw < 0 {
            return Err(last_error());
        }
        // SAFETY: fanotify_init returned a fresh descriptor we now own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let path = std::ffi::CString::new(mount_path)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
        let mark = |mask: u64| {
            // SAFETY: valid fanotify fd and NUL-terminated path.
            let rc = unsafe {
                libc::fanotify_mark(
                    fd.as_raw_fd(),
                    FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
                    mask,
                    libc::AT_FDCWD,
                    path.as_ptr(),
                )
            };
            if rc < 0 {
                Err(last_error())
            } else {
                Ok(())
            }
        };
        let rename_events = match mark(BASE_MASK | FAN_RENAME) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                mark(BASE_MASK | FAN_MOVED_FROM | FAN_MOVED_TO)?;
                false
            }
            Err(e) => return Err(e),
        };

        // SAFETY: NUL-terminated path; we own the returned descriptor.
        let mfd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if mfd < 0 {
            return Err(last_error());
        }
        let mount_fd = unsafe { OwnedFd::from_raw_fd(mfd) };
        Ok(FanotifyWatcher {
            fd,
            mount_fd,
            rename_events,
            buf: vec![0u8; 256 * 1024],
        })
    }

    /// Waits up to `timeout` for events and appends the decoded changes.
    pub fn read_changes(&mut self, timeout: Duration, out: &mut Vec<Change>) -> io::Result<()> {
        let mut pfd = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, ms) };
        if n < 0 {
            let e = last_error();
            return if e.kind() == io::ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(e)
            };
        }
        if n == 0 {
            return Ok(());
        }
        let mut paths = PathCache::default();
        loop {
            // SAFETY: reading into our own buffer.
            let len = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    self.buf.as_mut_ptr().cast(),
                    self.buf.len(),
                )
            };
            if len < 0 {
                let e = last_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(e);
            }
            if len == 0 {
                return Ok(());
            }
            let data = self.buf[..len as usize].to_vec();
            decode(&data, &self.mount_fd, &mut paths, out);
            if (len as usize) < self.buf.len() / 2 {
                return Ok(());
            }
        }
    }
}

/// Resolves directory handles to paths, once per read batch.
#[derive(Default)]
struct PathCache {
    paths: HashMap<Vec<u8>, Option<Vec<u8>>>,
}

impl PathCache {
    fn resolve(&mut self, mount_fd: &OwnedFd, handle: &[u8]) -> Option<Vec<u8>> {
        if let Some(p) = self.paths.get(handle) {
            return p.clone();
        }
        let path = resolve_handle(mount_fd, handle);
        self.paths.insert(handle.to_vec(), path.clone());
        path
    }
}

/// `handle` is a `struct file_handle`: u32 size, i32 type, bytes.
fn resolve_handle(mount_fd: &OwnedFd, handle: &[u8]) -> Option<Vec<u8>> {
    // Copy into an aligned buffer for the kernel.
    let mut aligned = vec![0u64; handle.len().div_ceil(8)];
    // SAFETY: the destination holds at least handle.len() bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            handle.as_ptr(),
            aligned.as_mut_ptr().cast::<u8>(),
            handle.len(),
        )
    };
    // SAFETY: a valid mount fd and a well-formed file_handle buffer.
    let fd = unsafe {
        libc::open_by_handle_at(
            mount_fd.as_raw_fd(),
            aligned.as_mut_ptr().cast(),
            libc::O_PATH | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None; // stale handle: the folder is gone
    }
    // SAFETY: open_by_handle_at returned a descriptor we now own.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let link = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()?;
    use std::os::unix::ffi::OsStrExt;
    let path = link.as_os_str().as_bytes().to_vec();
    (!path.ends_with(b" (deleted)")).then_some(path)
}

struct Fid<'a> {
    handle: &'a [u8],
    name: Option<&'a [u8]>,
}

/// Parses an info record carrying an fsid, a file handle and maybe a name.
fn parse_fid(record: &[u8], with_name: bool) -> Option<Fid<'_>> {
    let body = record.get(4 + FSID_LEN..)?;
    let handle_bytes = u32::from_ne_bytes(body.get(..4)?.try_into().ok()?) as usize;
    let handle = body.get(..8 + handle_bytes)?;
    let name = if with_name {
        let rest = &body[8 + handle_bytes..];
        let end = rest.iter().position(|&b| b == 0)?;
        Some(&rest[..end])
    } else {
        None
    };
    Some(Fid { handle, name })
}

fn decode(data: &[u8], mount_fd: &OwnedFd, paths: &mut PathCache, out: &mut Vec<Change>) {
    let mut off = 0;
    while off + METADATA_LEN <= data.len() {
        let event_len = u32::from_ne_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        let meta_len = u16::from_ne_bytes(data[off + 6..off + 8].try_into().unwrap()) as usize;
        let mask = u64::from_ne_bytes(data[off + 8..off + 16].try_into().unwrap());
        if event_len < METADATA_LEN || off + event_len > data.len() {
            break;
        }
        let event = &data[off..off + event_len];
        off += event_len;
        if mask & FAN_Q_OVERFLOW != 0 {
            out.push(Change::Overflow);
            continue;
        }

        let (mut entry, mut old, mut new, mut dir) = (None, None, None, None);
        let mut ioff = meta_len;
        while ioff + 4 <= event.len() {
            let info_type = event[ioff];
            let len = u16::from_ne_bytes(event[ioff + 2..ioff + 4].try_into().unwrap()) as usize;
            if len < 4 || ioff + len > event.len() {
                break;
            }
            let record = &event[ioff..ioff + len];
            match info_type {
                INFO_DFID_NAME => entry = parse_fid(record, true),
                INFO_OLD_DFID_NAME => old = parse_fid(record, true),
                INFO_NEW_DFID_NAME => new = parse_fid(record, true),
                INFO_DFID => dir = parse_fid(record, false),
                _ => {}
            }
            ioff += len;
        }

        let mut resolve = |fid: &Fid<'_>| -> Option<(Vec<u8>, Vec<u8>)> {
            let path = paths.resolve(mount_fd, fid.handle)?;
            Some((path, fid.name.unwrap_or_default().to_vec()))
        };
        match (
            old.as_ref().and_then(&mut resolve),
            new.as_ref().and_then(&mut resolve),
        ) {
            (Some(from), Some(to)) => {
                out.push(Change::Rename { from, to });
                continue;
            }
            (Some((dir, name)), None) | (None, Some((dir, name))) => {
                out.push(Change::Entry { dir, name });
                continue;
            }
            (None, None) => {}
        }
        if let Some((path, name)) = entry.as_ref().and_then(&mut resolve) {
            if name.is_empty() || name == b"." {
                out.push(Change::Dir { path });
            } else {
                out.push(Change::Entry { dir: path, name });
            }
        } else if let Some((path, _)) = dir.as_ref().and_then(&mut resolve) {
            out.push(Change::Dir { path });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Instant;

    /// A tmpfs mount for the test, or `None` if this environment cannot
    /// mount or watch (no CAP_SYS_ADMIN).
    struct TestFs {
        dir: tempfile::TempDir,
        mounted: bool,
    }

    impl TestFs {
        fn new() -> Option<TestFs> {
            let dir = tempfile::tempdir().ok()?;
            let ok = Command::new("mount")
                .args(["-t", "tmpfs", "evos-test"])
                .arg(dir.path())
                .status()
                .is_ok_and(|s| s.success());
            ok.then_some(TestFs { dir, mounted: true })
        }

        fn path(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }
    }

    impl Drop for TestFs {
        fn drop(&mut self) {
            if self.mounted {
                let _ = Command::new("umount").arg(self.dir.path()).status();
            }
        }
    }

    fn collect(w: &mut FanotifyWatcher, until: impl Fn(&[Change]) -> bool) -> Vec<Change> {
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !until(&out) {
            w.read_changes(Duration::from_millis(100), &mut out)
                .unwrap();
        }
        out
    }

    fn b(p: &Path) -> Vec<u8> {
        p.as_os_str().as_bytes().to_vec()
    }

    #[test]
    fn reports_create_delete_and_rename() {
        let Some(fs_) = TestFs::new() else {
            eprintln!("skipping: cannot mount tmpfs here");
            return;
        };
        let root = fs_.path();
        fs::create_dir(root.join("sub")).unwrap();
        let mut w = match FanotifyWatcher::new(&b(&root)) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("skipping: fanotify unavailable: {e}");
                return;
            }
        };

        fs::write(root.join("sub/a.txt"), b"x").unwrap();
        let created = Change::Entry {
            dir: b(&root.join("sub")),
            name: b"a.txt".to_vec(),
        };
        let got = collect(&mut w, |c| c.contains(&created));
        assert!(got.contains(&created), "{got:?}");

        fs::rename(root.join("sub/a.txt"), root.join("b.txt")).unwrap();
        let got = collect(&mut w, |c| {
            c.iter().any(|x| matches!(x, Change::Rename { .. }))
                || c.iter()
                    .any(|x| matches!(x, Change::Entry { name, .. } if name == b"b.txt"))
        });
        if w.rename_events {
            let expected = Change::Rename {
                from: (b(&root.join("sub")), b"a.txt".to_vec()),
                to: (b(&root), b"b.txt".to_vec()),
            };
            assert!(got.contains(&expected), "{got:?}");
        }

        fs::remove_file(root.join("b.txt")).unwrap();
        let deleted = Change::Entry {
            dir: b(&root),
            name: b"b.txt".to_vec(),
        };
        let got = collect(&mut w, |c| c.contains(&deleted));
        assert!(got.contains(&deleted), "{got:?}");
    }
}
