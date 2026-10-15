//! Low-level directory listing and stat calls.
//!
//! Directories are opened with `O_NOFOLLOW` and their entries are stat'ed
//! relative to the directory fd with `statx`, which avoids repeated path
//! resolution. The [`CacheMode`] controls attribute caching on network
//! filesystems: listings use cached attributes the READDIR(PLUS) reply just
//! populated, while change sweeps force a round trip to the server.

use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use evos_core::{ListedEntry, Meta};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{AtFlags, Mode, OFlags, Statx, StatxFlags, CWD};

/// `STATX_ATTR_MOUNT_ROOT`: the entry is the root of a mount.
const ATTR_MOUNT_ROOT: u64 = 0x2000;

/// How `statx` may use cached attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Whatever `stat(2)` would do.
    #[default]
    AsStat,
    /// Never contact the server; use cached attributes if present.
    Cached,
    /// Always revalidate with the server.
    Revalidate,
}

impl CacheMode {
    fn flags(self) -> AtFlags {
        match self {
            CacheMode::AsStat => AtFlags::empty(),
            CacheMode::Cached => AtFlags::STATX_DONT_SYNC,
            CacheMode::Revalidate => AtFlags::STATX_FORCE_SYNC,
        }
    }
}

/// Result of listing one directory.
#[derive(Debug, Default)]
pub struct Listing {
    /// Metadata of the directory itself, taken before reading entries so a
    /// concurrent change always leaves a newer mtime behind.
    pub dir: Meta,
    pub entries: Vec<ListedEntry>,
    /// Names of subdirectories that are mount points (other filesystems or
    /// bind mounts); they are indexed but not descended into.
    pub mount_points: Vec<Vec<u8>>,
}

const MASK: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::MODE)
    .union(StatxFlags::UID)
    .union(StatxFlags::GID)
    .union(StatxFlags::SIZE)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::ATIME)
    .union(StatxFlags::BTIME);

fn ns(t: &rustix::fs::StatxTimestamp) -> i64 {
    t.tv_sec * 1_000_000_000 + t.tv_nsec as i64
}

fn meta_of(st: &Statx) -> Meta {
    // Not every filesystem records birth times (statx then omits BTIME).
    let has_btime = st.stx_mask & StatxFlags::BTIME.bits() != 0;
    Meta {
        size: st.stx_size,
        mtime: ns(&st.stx_mtime),
        mode: st.stx_mode as u32,
        uid: st.stx_uid,
        gid: st.stx_gid,
        btime: if has_btime { ns(&st.stx_btime) } else { 0 },
        atime: ns(&st.stx_atime),
    }
}

fn os_path(path: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(path))
}

fn open_dir(path: &[u8]) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        os_path(path),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Metadata plus the identity of the filesystem an entry lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatInfo {
    pub meta: Meta,
    pub dev: (u32, u32),
    /// The entry is the root of a mount (including bind mounts).
    pub mount_root: bool,
}

/// Like [`stat`], but also reports the device and mount-root flag.
pub fn stat_info(path: &[u8], cache: CacheMode) -> io::Result<StatInfo> {
    let st = rustix::fs::statx(
        CWD,
        os_path(path),
        AtFlags::SYMLINK_NOFOLLOW | cache.flags(),
        MASK,
    )?;
    Ok(StatInfo {
        meta: meta_of(&st),
        dev: (st.stx_dev_major, st.stx_dev_minor),
        mount_root: st.stx_attributes.bits() & st.stx_attributes_mask.bits() & ATTR_MOUNT_ROOT != 0,
    })
}

/// Stats a path without following a final symlink.
pub fn stat(path: &[u8], cache: CacheMode) -> io::Result<Meta> {
    let st = rustix::fs::statx(
        CWD,
        os_path(path),
        AtFlags::SYMLINK_NOFOLLOW | cache.flags(),
        MASK,
    )?;
    Ok(meta_of(&st))
}

/// Lists a directory and stats every entry.
///
/// Entries that vanish between `getdents` and `statx` are skipped.
pub fn list_dir(path: &[u8], cache: CacheMode) -> io::Result<Listing> {
    let fd = open_dir(path)?;
    let dir_stat = rustix::fs::statx(&fd, "", AtFlags::EMPTY_PATH | cache.flags(), MASK)?;
    let dir_dev = (dir_stat.stx_dev_major, dir_stat.stx_dev_minor);
    let mut listing = Listing {
        dir: meta_of(&dir_stat),
        ..Default::default()
    };

    let mut dir = rustix::fs::Dir::read_from(fd.as_fd())?;
    while let Some(entry) = dir.read() {
        let entry = entry?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        let st = match rustix::fs::statx(
            &fd,
            entry.file_name(),
            AtFlags::SYMLINK_NOFOLLOW | cache.flags(),
            MASK,
        ) {
            Ok(st) => st,
            Err(rustix::io::Errno::NOENT) => continue,
            Err(e) => return Err(e.into()),
        };
        let meta = meta_of(&st);
        if meta.is_dir()
            && ((st.stx_dev_major, st.stx_dev_minor) != dir_dev
                || st.stx_attributes.bits() & st.stx_attributes_mask.bits() & ATTR_MOUNT_ROOT != 0)
        {
            listing.mount_points.push(name.to_vec());
        }
        listing.entries.push(ListedEntry {
            name: name.to_vec(),
            meta,
        });
    }
    Ok(listing)
}

/// Joins a directory path and a child name.
pub fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(dir.len() + 1 + name.len());
    path.extend_from_slice(dir);
    if path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// Is the error one that a network filesystem reports when the server is
/// unreachable (as opposed to a problem with one path)?
pub fn is_unreachable(err: &io::Error) -> bool {
    use rustix::io::Errno;
    let Some(code) = err.raw_os_error() else {
        return false;
    };
    [
        Errno::HOSTDOWN,
        Errno::HOSTUNREACH,
        Errno::NETUNREACH,
        Errno::NETDOWN,
        Errno::TIMEDOUT,
        Errno::CONNREFUSED,
        Errno::CONNRESET,
        Errno::CONNABORTED,
        Errno::NOTCONN,
        Errno::IO,
        Errno::STALE,
        Errno::AGAIN,
    ]
    .iter()
    .any(|e| e.raw_os_error() == code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn bytes(p: &Path) -> Vec<u8> {
        p.as_os_str().as_bytes().to_vec()
    }

    #[test]
    fn lists_entries_with_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), b"hello").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        symlink("a.txt", tmp.path().join("link")).unwrap();
        fs::set_permissions(tmp.path().join("a.txt"), fs::Permissions::from_mode(0o640)).unwrap();

        let listing = list_dir(&bytes(tmp.path()), CacheMode::AsStat).unwrap();
        assert!(listing.dir.is_dir());
        let mut names: Vec<_> = listing.entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(
            names,
            vec![b"a.txt".to_vec(), b"link".to_vec(), b"sub".to_vec()]
        );
        let a = listing.entries.iter().find(|e| e.name == b"a.txt").unwrap();
        assert_eq!(a.meta.size, 5);
        assert_eq!(a.meta.mode & 0o777, 0o640);
        assert!(a.meta.mtime > 0);
        let link = listing.entries.iter().find(|e| e.name == b"link").unwrap();
        assert_eq!(
            link.meta.mode & 0o170000,
            0o120000,
            "symlinks are not followed"
        );
        assert!(listing.mount_points.is_empty());
    }

    #[test]
    fn refuses_to_follow_symlinked_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("real")).unwrap();
        symlink("real", tmp.path().join("alias")).unwrap();
        assert!(list_dir(&bytes(&tmp.path().join("alias")), CacheMode::AsStat).is_err());
        assert!(
            stat(&bytes(&tmp.path().join("alias")), CacheMode::Revalidate)
                .unwrap()
                .mode
                & 0o170000
                == 0o120000
        );
    }

    #[test]
    fn detects_mount_points() {
        // /proc is always a separate mount below /.
        let listing = list_dir(b"/", CacheMode::AsStat).unwrap();
        assert!(listing.mount_points.iter().any(|n| n == b"proc"));
    }

    #[test]
    fn join_paths() {
        assert_eq!(join(b"/", b"etc"), b"/etc");
        assert_eq!(join(b"/etc", b"hosts"), b"/etc/hosts");
    }
}
