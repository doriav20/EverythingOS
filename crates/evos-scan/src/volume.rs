//! State shared between a volume's driver thread and the rest of the
//! daemon: the index, live status, scan checkpoints and control signals.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use evos_core::{EntryId, Index};
use parking_lot::{Condvar, Mutex};

use crate::throttle::GateStats;
use crate::walker::{Progress, SharedIndex};

/// What a volume's driver is doing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Starting,
    /// First full scan.
    Scanning,
    /// Catching up with changes made while the daemon was not running.
    Reconciling,
    /// Up to date and following changes.
    Live,
    /// A network share that cannot be reached; the index is kept.
    Offline,
    Stopped,
}

/// How changes are detected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WatchMode {
    #[default]
    None,
    Fanotify,
    Inotify,
    /// SMB change notifications; `details` if changed paths are reported.
    SmbNotify {
        details: bool,
    },
    /// Scheduled folder sweeps only.
    Sweeps,
    /// A copy of another computer's index, kept current by its server.
    Remote,
}

/// Snapshot of a volume's state for status reports.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VolumeStatus {
    pub phase: Phase,
    pub watch: WatchMode,
    pub scan_complete: bool,
    pub last_scan: Option<SystemTime>,
    pub last_sweep: Option<SystemTime>,
    pub last_change: Option<SystemTime>,
    pub error: Option<String>,
    pub gate: Option<GateStats>,
    /// Folders with scheduled checks (network volumes).
    pub tracked_dirs: usize,
}

/// Progress of the first scan, persisted in snapshots so an interrupted
/// scan of a huge share resumes instead of restarting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Checkpoint {
    pub scan_complete: bool,
    /// Folders queued but not yet listed.
    pub frontier: Vec<EntryId>,
}

impl Checkpoint {
    const VERSION: u8 = 1;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + 4 * self.frontier.len());
        out.push(Self::VERSION);
        out.push(self.scan_complete as u8);
        out.extend_from_slice(&(self.frontier.len() as u32).to_le_bytes());
        for id in &self.frontier {
            out.extend_from_slice(&id.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&version, rest) = bytes.split_first()?;
        if version != Self::VERSION {
            return None;
        }
        let (&complete, rest) = rest.split_first()?;
        let n = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let ids = rest.get(4..4 + 4 * n)?;
        Some(Checkpoint {
            scan_complete: complete != 0,
            frontier: ids
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect(),
        })
    }
}

/// One indexed volume.
pub struct Volume {
    root: Vec<u8>,
    fs_type: String,
    pub index: SharedIndex,
    pub progress: Progress,
    status: Mutex<VolumeStatus>,
    checkpoint: Mutex<Checkpoint>,
    stop: AtomicBool,
    rescan: AtomicBool,
    hints: Mutex<Vec<Vec<u8>>>,
    wake: Condvar,
    woken: Mutex<bool>,
}

impl Volume {
    pub fn new(index: Index, fs_type: &str, checkpoint: Checkpoint) -> Self {
        Volume {
            root: index.root_path().to_vec(),
            fs_type: fs_type.to_owned(),
            index: SharedIndex::new(parking_lot::RwLock::new(index)),
            progress: Progress::default(),
            status: Mutex::new(VolumeStatus {
                scan_complete: checkpoint.scan_complete,
                ..Default::default()
            }),
            checkpoint: Mutex::new(checkpoint),
            stop: AtomicBool::new(false),
            rescan: AtomicBool::new(false),
            hints: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            woken: Mutex::new(false),
        }
    }

    pub fn root(&self) -> &[u8] {
        &self.root
    }

    pub fn fs_type(&self) -> &str {
        &self.fs_type
    }

    pub fn status(&self) -> VolumeStatus {
        self.status.lock().clone()
    }

    pub fn update_status(&self, f: impl FnOnce(&mut VolumeStatus)) {
        f(&mut self.status.lock());
    }

    pub fn checkpoint(&self) -> Checkpoint {
        self.checkpoint.lock().clone()
    }

    pub fn set_checkpoint(&self, checkpoint: Checkpoint) {
        self.update_status(|s| s.scan_complete = checkpoint.scan_complete);
        *self.checkpoint.lock() = checkpoint;
    }

    /// Asks the driver to stop; walks in progress are cancelled.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.notify();
    }

    /// The stop flag, usable as a walk cancellation flag.
    pub fn stop_flag(&self) -> &AtomicBool {
        &self.stop
    }

    pub fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Asks the driver to re-check the whole volume.
    pub fn request_rescan(&self) {
        self.rescan.store(true, Ordering::Relaxed);
        self.notify();
    }

    pub fn take_rescan_request(&self) -> bool {
        self.rescan.swap(false, Ordering::Relaxed)
    }

    /// Tells the driver a user is interested in this path (e.g. opened a
    /// result there), so it should be refreshed soon.
    pub fn hint(&self, path: &[u8]) {
        self.hints.lock().push(path.to_vec());
        self.notify();
    }

    pub fn take_hints(&self) -> Vec<Vec<u8>> {
        std::mem::take(&mut *self.hints.lock())
    }

    /// Wakes the driver if it is waiting.
    pub fn notify(&self) {
        *self.woken.lock() = true;
        self.wake.notify_all();
    }

    /// Sleeps up to `timeout` or until notified. Returns false if stopping.
    pub fn wait(&self, timeout: Duration) -> bool {
        let mut woken = self.woken.lock();
        if !*woken && !self.is_stopping() {
            self.wake.wait_for(&mut woken, timeout);
        }
        *woken = false;
        !self.is_stopping()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_core::Meta;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn checkpoint_roundtrip() {
        let cp = Checkpoint {
            scan_complete: false,
            frontier: vec![1, 5, 99],
        };
        assert_eq!(Checkpoint::decode(&cp.encode()), Some(cp));
        assert_eq!(Checkpoint::decode(&[]), None);
        assert_eq!(
            Checkpoint::decode(&[9, 1, 0, 0, 0, 0]),
            None,
            "unknown version"
        );
        assert_eq!(
            Checkpoint::decode(&[1, 1, 2, 0, 0, 0, 1]),
            None,
            "truncated"
        );
    }

    #[test]
    fn wait_wakes_on_hint_and_stop() {
        let vol = Arc::new(Volume::new(
            Index::new(b"/x", Meta::dir(0)),
            "ext4",
            Checkpoint::default(),
        ));
        let v = vol.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            v.hint(b"/x/a");
        });
        let start = Instant::now();
        assert!(vol.wait(Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(vol.take_hints(), vec![b"/x/a".to_vec()]);
        t.join().unwrap();

        vol.stop();
        assert!(!vol.wait(Duration::from_secs(5)));
        assert!(vol.stop_flag().load(Ordering::Relaxed));
    }
}
