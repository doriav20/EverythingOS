//! Run history: how often and when each user opened a path from
//! EverythingOS (Everything's Run Count and Date Run). It is kept per user,
//! so one user's habits are never visible to another, and saved in the
//! state directory.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use evos_proto::RunInfo;
use parking_lot::Mutex;

const MAGIC: &[u8; 8] = b"EVOSRUN1";
/// Paths remembered per user; the least recently run are forgotten first.
const MAX_PER_USER: usize = 100_000;

type Paths = HashMap<Vec<u8>, RunInfo>;

pub struct RunHistory {
    file: PathBuf,
    users: Mutex<HashMap<u32, Paths>>,
    dirty: AtomicBool,
}

impl RunHistory {
    /// Loads the history file; a missing or damaged file starts empty.
    pub fn load(file: PathBuf) -> Self {
        let users = match std::fs::read(&file) {
            Ok(bytes) => decode(&bytes).unwrap_or_else(|| {
                tracing::warn!("ignoring damaged run history {}", file.display());
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        RunHistory {
            file,
            users: Mutex::new(users),
            dirty: AtomicBool::new(false),
        }
    }

    pub fn get(&self, uid: u32, path: &[u8]) -> RunInfo {
        self.users
            .lock()
            .get(&uid)
            .and_then(|p| p.get(path))
            .copied()
            .unwrap_or_default()
    }

    /// Counts one more run of `path`, now.
    pub fn opened(&self, uid: u32, path: &[u8]) -> RunInfo {
        self.update(uid, path, |info| {
            info.count = info.count.saturating_add(1);
            info.last = now_ns();
        })
    }

    /// Sets the run count; 0 forgets the path.
    pub fn set(&self, uid: u32, path: &[u8], count: u32) -> RunInfo {
        self.update(uid, path, |info| info.count = count)
    }

    fn update(&self, uid: u32, path: &[u8], f: impl FnOnce(&mut RunInfo)) -> RunInfo {
        let mut users = self.users.lock();
        let paths = users.entry(uid).or_default();
        let info = paths.entry(path.to_vec()).or_default();
        f(info);
        let info = *info;
        if info.count == 0 {
            paths.remove(path);
        }
        if paths.len() > MAX_PER_USER {
            let mut by_age: Vec<(i64, Vec<u8>)> =
                paths.iter().map(|(p, i)| (i.last, p.clone())).collect();
            by_age.sort();
            for (_, p) in by_age.into_iter().take(paths.len() - MAX_PER_USER) {
                paths.remove(&p);
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        info
    }

    /// Every path the user has run.
    pub fn of_user(&self, uid: u32) -> Vec<(Vec<u8>, RunInfo)> {
        self.users
            .lock()
            .get(&uid)
            .map(|p| p.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default()
    }

    /// Writes the history if it changed.
    pub fn save(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let bytes = encode(&self.users.lock());
        if let Err(e) = write_atomic(&self.file, &bytes) {
            self.dirty.store(true, Ordering::Relaxed);
            tracing::warn!("cannot save run history {}: {e}", self.file.display());
        }
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

fn write_atomic(file: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = file.with_extension("tmp");
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, file)
}

fn encode(users: &HashMap<u32, Paths>) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    for (uid, paths) in users {
        for (path, info) in paths {
            out.extend_from_slice(&uid.to_le_bytes());
            out.extend_from_slice(&info.count.to_le_bytes());
            out.extend_from_slice(&info.last.to_le_bytes());
            out.extend_from_slice(&(path.len() as u32).to_le_bytes());
            out.extend_from_slice(path);
        }
    }
    out
}

fn decode(bytes: &[u8]) -> Option<HashMap<u32, Paths>> {
    fn take<'a>(rest: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
        let (head, tail) = rest.split_at_checked(n)?;
        *rest = tail;
        Some(head)
    }
    let mut rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let mut users: HashMap<u32, Paths> = HashMap::new();
    while !rest.is_empty() {
        let uid = u32::from_le_bytes(take(&mut rest, 4)?.try_into().ok()?);
        let count = u32::from_le_bytes(take(&mut rest, 4)?.try_into().ok()?);
        let last = i64::from_le_bytes(take(&mut rest, 8)?.try_into().ok()?);
        let len = u32::from_le_bytes(take(&mut rest, 4)?.try_into().ok()?) as usize;
        let path = take(&mut rest, len)?.to_vec();
        users
            .entry(uid)
            .or_default()
            .insert(path, RunInfo { count, last });
    }
    Some(users)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_runs_per_user_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("run-history");
        let h = RunHistory::load(file.clone());
        let path = "/home/דוד/מסמך.pdf".as_bytes();
        assert_eq!(h.opened(1000, path).count, 1);
        let info = h.opened(1000, path);
        assert_eq!(info.count, 2);
        assert!(info.last > 0);
        assert_eq!(h.get(1001, path), RunInfo::default(), "per user");
        h.set(1000, b"/x", 7);
        h.save();

        let back = RunHistory::load(file.clone());
        assert_eq!(back.get(1000, path), info);
        assert_eq!(back.get(1000, b"/x").count, 7);
        back.set(1000, b"/x", 0);
        assert_eq!(back.of_user(1000).len(), 1, "a zero count forgets");

        std::fs::write(&file, b"garbage").unwrap();
        assert!(RunHistory::load(file).of_user(1000).is_empty());
    }

    #[test]
    fn truncated_files_are_rejected() {
        let mut users = HashMap::new();
        users.insert(
            1,
            Paths::from([(b"/a".to_vec(), RunInfo { count: 1, last: 2 })]),
        );
        let bytes = encode(&users);
        assert_eq!(decode(&bytes), Some(users));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
    }
}
