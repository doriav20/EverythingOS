//! SMB change notifications on cifs mounts.
//!
//! The Linux cifs client exposes SMB2 CHANGE_NOTIFY through ioctls on an
//! open directory (see `fs/smb/client/cifs_ioctl.h`):
//!
//! * `CIFS_IOC_NOTIFY_INFO` (Linux 6.x) blocks until the server reports
//!   changes and returns them as `FILE_NOTIFY_INFORMATION` records, so only
//!   the affected folders need re-listing;
//! * `CIFS_IOC_NOTIFY` (Linux 5.6+) only says that *something* changed.
//!
//! With `watch_tree` set, one request covers the whole share. The client
//! opens a fresh server handle per request, so changes that happen between
//! two requests are not reported: notifications accelerate freshness, and
//! the folder sweeps remain the safety net.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;

// _IOW(0xCF, 9, struct smb3_notify) with sizeof == 5 (packed).
const CIFS_IOC_NOTIFY: u64 = 0x4005_CF09;
// _IOWR(0xCF, 11, struct smb3_notify_info) with sizeof == 9 (packed).
const CIFS_IOC_NOTIFY_INFO: u64 = 0xC009_CF0B;

const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x1;
const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x2;
const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x8;
const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x10;
const FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE;

const HEADER: usize = 9;
const DATA_CAPACITY: usize = 64 * 1024;

/// What changed, as reported by the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Added,
    Removed,
    Modified,
    RenamedOld,
    RenamedNew,
    Other(u32),
}

impl From<u32> for Action {
    fn from(v: u32) -> Self {
        match v {
            1 => Action::Added,
            2 => Action::Removed,
            3 => Action::Modified,
            4 => Action::RenamedOld,
            5 => Action::RenamedNew,
            other => Action::Other(other),
        }
    }
}

/// One wake-up from the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Paths (relative to the watched folder, `/`-separated) that changed.
    Changed(Vec<(Action, Vec<u8>)>),
    /// Something changed, but the details are unknown (old kernel, or the
    /// server's notify buffer overflowed).
    Unspecified,
}

/// Recursive change notifications for a folder on a cifs mount.
pub struct SmbNotifier {
    dir: File,
    details: bool,
    buf: Vec<u8>,
}

impl SmbNotifier {
    pub fn open(path: &[u8]) -> io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY)
            .open(std::ffi::OsStr::from_bytes(path))?;
        Ok(SmbNotifier {
            dir,
            details: true,
            buf: vec![0u8; HEADER + DATA_CAPACITY],
        })
    }

    /// True while detailed notifications are available.
    pub fn has_details(&self) -> bool {
        self.details
    }

    /// Blocks until the server reports a change anywhere below the folder.
    /// Fails if the mount does not support change notifications at all.
    pub fn wait(&mut self) -> io::Result<Notice> {
        if self.details {
            self.buf[..4].copy_from_slice(&FILTER.to_le_bytes());
            self.buf[4] = 1; // watch_tree
            self.buf[5..9].copy_from_slice(&(DATA_CAPACITY as u32).to_le_bytes());
            // If the reply does not fit, the kernel leaves the data area
            // untouched; clear the first record so stale data never parses.
            self.buf[HEADER..HEADER + 16].fill(0);
            // SAFETY: the buffer is a valid smb3_notify_info followed by
            // DATA_CAPACITY bytes of space, as announced in data_len.
            let rc = unsafe {
                libc::ioctl(
                    self.dir.as_raw_fd(),
                    CIFS_IOC_NOTIFY_INFO as _,
                    self.buf.as_mut_ptr(),
                )
            };
            if rc == 0 {
                let len = u32::from_le_bytes(self.buf[5..9].try_into().unwrap()) as usize;
                if len == 0 || len > DATA_CAPACITY {
                    return Ok(Notice::Unspecified);
                }
                let mut changes = parse_records(&self.buf[HEADER..HEADER + len]);
                changes.retain(|(_, path)| !path.is_empty());
                return Ok(if changes.is_empty() {
                    Notice::Unspecified
                } else {
                    Notice::Changed(changes)
                });
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::ENOTTY) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP) => {
                    self.details = false;
                }
                _ => return Err(err),
            }
        }
        let mut req = [0u8; 5];
        req[..4].copy_from_slice(&FILTER.to_le_bytes());
        req[4] = 1;
        // SAFETY: req is a valid packed smb3_notify.
        let rc =
            unsafe { libc::ioctl(self.dir.as_raw_fd(), CIFS_IOC_NOTIFY as _, req.as_mut_ptr()) };
        if rc == 0 {
            Ok(Notice::Unspecified)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// Parses a chain of `FILE_NOTIFY_INFORMATION` records.
pub fn parse_records(data: &[u8]) -> Vec<(Action, Vec<u8>)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while let Some(head) = data.get(off..off + 12) {
        let next = u32::from_le_bytes(head[0..4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(head[4..8].try_into().unwrap());
        let name_len = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
        let Some(name) = data.get(off + 12..off + 12 + name_len) else {
            break;
        };
        let units: Vec<u16> = name
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let path = String::from_utf16_lossy(&units).replace('\\', "/");
        out.push((Action::from(action), path.into_bytes()));
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(next: u32, action: u32, name: &str) -> Vec<u8> {
        let utf16: Vec<u8> = name.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut r = Vec::new();
        r.extend_from_slice(&next.to_le_bytes());
        r.extend_from_slice(&action.to_le_bytes());
        r.extend_from_slice(&(utf16.len() as u32).to_le_bytes());
        r.extend_from_slice(&utf16);
        r
    }

    #[test]
    fn parses_record_chains() {
        let first = record(0, 1, "dir\\new file.txt");
        let mut data = record(0, 4, "old");
        // Records are 4-byte aligned; chain the first after padding.
        let mut chained = first.clone();
        while chained.len() % 4 != 0 {
            chained.push(0);
        }
        let next = chained.len() as u32;
        chained[0..4].copy_from_slice(&next.to_le_bytes());
        chained.append(&mut data);
        let parsed = parse_records(&chained);
        assert_eq!(
            parsed,
            vec![
                (Action::Added, b"dir/new file.txt".to_vec()),
                (Action::RenamedOld, b"old".to_vec())
            ]
        );
        assert_eq!(
            parse_records(&record(0, 9, "ü")),
            vec![(Action::Other(9), "ü".as_bytes().to_vec())]
        );
    }

    #[test]
    fn truncated_data_is_tolerated() {
        let r = record(0, 2, "abc");
        assert!(parse_records(&r[..10]).is_empty());
        assert_eq!(parse_records(&r[..r.len() - 2]).len(), 0);
    }

    #[test]
    fn local_folders_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let mut n = SmbNotifier::open(tmp.path().as_os_str().as_bytes()).unwrap();
        assert!(n.wait().is_err(), "not a cifs mount");
        assert!(!n.has_details());
    }
}
