//! Volume discovery from `/proc/self/mountinfo`.
//!
//! Every mount is classified as a local filesystem (indexed and watched),
//! a network filesystem (indexed with the network strategy) or something
//! that is not worth indexing (pseudo, overlay and image filesystems).

use std::fs;
use std::io;
use std::path::PathBuf;

/// How a mount should be indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MountClass {
    Local,
    Network,
    Ignored,
}

/// One line of mountinfo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_id: u32,
    pub parent_id: u32,
    pub major: u32,
    pub minor: u32,
    /// Root of the mount within its filesystem (`/` unless a bind mount).
    pub root: Vec<u8>,
    pub mount_point: Vec<u8>,
    pub fs_type: String,
    pub source: String,
    pub options: String,
}

impl MountEntry {
    pub fn class(&self) -> MountClass {
        classify(&self.fs_type)
    }

    /// A bind mount exposes a subdirectory of a filesystem a second time.
    pub fn is_bind(&self) -> bool {
        self.root != b"/"
    }

    pub fn mount_point_path(&self) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(&self.mount_point))
    }
}

/// Classifies a filesystem type.
pub fn classify(fs_type: &str) -> MountClass {
    match fs_type {
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "f2fs" | "zfs" | "bcachefs" | "jfs"
        | "reiserfs" | "nilfs2" | "ntfs" | "ntfs3" | "fuseblk" | "exfat" | "vfat" | "msdos"
        | "hfsplus" | "udf" | "tmpfs" => MountClass::Local,
        "cifs" | "smb3" | "nfs" | "nfs4" | "fuse.sshfs" | "fuse.rclone" | "ceph" | "glusterfs"
        | "fuse.glusterfs" | "9p" | "afs" | "davfs" | "fuse.davfs2" => MountClass::Network,
        _ => MountClass::Ignored,
    }
}

/// Reads and parses `/proc/self/mountinfo`.
pub fn read_mounts() -> io::Result<Vec<MountEntry>> {
    Ok(parse(&fs::read("/proc/self/mountinfo")?))
}

/// Parses mountinfo content. Malformed lines are skipped.
pub fn parse(content: &[u8]) -> Vec<MountEntry> {
    content
        .split(|&b| b == b'\n')
        .filter_map(parse_line)
        .collect()
}

fn parse_line(line: &[u8]) -> Option<MountEntry> {
    // 36 35 98:0 /mnt1 /mnt2 rw,noatime master:1 - ext3 /dev/root rw,errors=continue
    let fields: Vec<&[u8]> = line.split(|&b| b == b' ').collect();
    let sep = fields.iter().position(|f| *f == b"-")?;
    if sep < 6 || fields.len() < sep + 3 {
        return None;
    }
    let num = |f: &[u8]| std::str::from_utf8(f).ok()?.parse::<u32>().ok();
    let (major, minor) = {
        let dev = std::str::from_utf8(fields[2]).ok()?;
        let (a, b) = dev.split_once(':')?;
        (a.parse().ok()?, b.parse().ok()?)
    };
    let text = |f: &[u8]| String::from_utf8_lossy(&unescape(f)).into_owned();
    Some(MountEntry {
        mount_id: num(fields[0])?,
        parent_id: num(fields[1])?,
        major,
        minor,
        root: unescape(fields[3]),
        mount_point: unescape(fields[4]),
        options: text(fields[5]),
        fs_type: text(fields[sep + 1]),
        source: text(fields[sep + 2]),
    })
}

/// Decodes the octal escapes (`\040` for space etc.) used by the kernel.
fn unescape(field: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        if field[i] == b'\\' && i + 3 < field.len() {
            let oct = &field[i + 1..i + 4];
            if oct.iter().all(|b| (b'0'..=b'7').contains(b)) {
                out.push((oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0'));
                i += 4;
                continue;
            }
        }
        out.push(field[i]);
        i += 1;
    }
    out
}

/// Waits for the mount table to change. The kernel signals mountinfo
/// readers with `POLLPRI` on every mount or unmount.
pub struct MountWatcher {
    file: fs::File,
}

impl MountWatcher {
    pub fn new() -> io::Result<Self> {
        Ok(MountWatcher {
            file: fs::File::open("/proc/self/mountinfo")?,
        })
    }

    /// Blocks until the mount table changes or `timeout_ms` elapses.
    /// Returns true on change.
    pub fn wait(&mut self, timeout_ms: i32) -> io::Result<bool> {
        use rustix::event::{poll, PollFd, PollFlags, Timespec};
        use std::io::{Read, Seek};
        let mut fds = [PollFd::new(&self.file, PollFlags::PRI | PollFlags::ERR)];
        let timeout = (timeout_ms >= 0).then(|| Timespec {
            tv_sec: (timeout_ms / 1000) as _,
            tv_nsec: ((timeout_ms % 1000) * 1_000_000) as _,
        });
        let n = poll(&mut fds, timeout.as_ref())?;
        // Re-arm: the event stays pending until the file is read again.
        let mut sink = Vec::new();
        self.file.rewind()?;
        self.file.read_to_end(&mut sink)?;
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = b"\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw
23 22 0:21 / /proc rw,nosuid shared:12 - proc proc rw
24 22 259:3 / /home rw,relatime shared:2 - btrfs /dev/nvme0n1p3 rw,subvol=/home
25 22 0:45 / /mnt/My\\040Share rw,relatime shared:90 - cifs //nas/share rw,vers=3.1.1
26 22 0:46 / /mnt/nfs rw shared:91 master:3 - nfs4 nas:/export rw,vers=4.2
27 24 259:3 /data /srv/data rw - btrfs /dev/nvme0n1p3 rw
garbage line
28 22 0:50 / /snap/core squashfs ro - squashfs /dev/loop0 ro
";

    #[test]
    fn parses_and_classifies() {
        let mounts = parse(SAMPLE);
        assert_eq!(mounts.len(), 7);
        let root = &mounts[0];
        assert_eq!(root.mount_point, b"/");
        assert_eq!((root.major, root.minor), (259, 2));
        assert_eq!(root.class(), MountClass::Local);
        assert_eq!(mounts[1].class(), MountClass::Ignored);
        let smb = &mounts[3];
        assert_eq!(
            smb.mount_point, b"/mnt/My Share",
            "octal escapes are decoded"
        );
        assert_eq!(smb.source, "//nas/share");
        assert_eq!(smb.class(), MountClass::Network);
        assert_eq!(mounts[4].fs_type, "nfs4", "optional fields are skipped");
        assert!(mounts[5].is_bind());
        assert_eq!(mounts[6].class(), MountClass::Ignored);
    }

    #[test]
    fn reads_live_mount_table() {
        let mounts = read_mounts().unwrap();
        assert!(mounts.iter().any(|m| m.mount_point == b"/"));
    }
}
