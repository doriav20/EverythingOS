//! Parallel directory walker.
//!
//! Worker threads list directories (the slow, I/O-bound part) while the
//! calling thread applies finished listings to the index in batches under
//! one short write lock, and queues the subdirectories they reveal. Index
//! writes therefore stay single-threaded and searches are never blocked for
//! long.
//!
//! The walker serves both initial scans (descend everywhere) and
//! incremental rescans (re-list given folders, descend only into folders
//! that are new). Cancelled or interrupted walks report the folders they
//! did not finish, so a scan of a huge share can resume where it stopped.

use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use evos_core::{EntryId, Index};
use parking_lot::RwLock;

use crate::fsops::{self, CacheMode, Listing};

/// An index shared between the scanners and the search side.
pub type SharedIndex = Arc<RwLock<Index>>;

/// Path exclusion predicate (absolute path bytes).
pub type ExcludeFn = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// Outcome of one filesystem operation, fed back to a [`Gate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpOutcome {
    Ok,
    Error,
    /// The server is unreachable; the walk should stop.
    Unreachable,
}

/// Concurrency and rate control for filesystem operations, used to adapt
/// scanning to network shares.
pub trait Gate: Send + Sync {
    /// Blocks until another operation may start.
    fn begin(&self);
    /// Reports a finished operation.
    fn end(&self, elapsed: Duration, outcome: OpOutcome);
}

/// Live counters, readable while a walk runs.
#[derive(Debug, Default)]
pub struct Progress {
    pub dirs_listed: AtomicU64,
    pub entries_seen: AtomicU64,
    pub errors: AtomicU64,
    pub pending: AtomicU64,
}

/// Walk configuration.
#[derive(Clone)]
pub struct WalkOptions {
    pub threads: usize,
    pub cache: CacheMode,
    /// Descend into folders that already existed in the index (full scan)
    /// rather than only into new ones (incremental rescan).
    pub recurse_existing: bool,
    pub exclude: Option<ExcludeFn>,
    pub gate: Option<Arc<dyn Gate>>,
}

impl Default for WalkOptions {
    fn default() -> Self {
        WalkOptions {
            threads: std::thread::available_parallelism()
                .map_or(4, |n| n.get() * 2)
                .min(32),
            cache: CacheMode::AsStat,
            recurse_existing: true,
            exclude: None,
            gate: None,
        }
    }
}

/// Summary of a finished walk.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WalkReport {
    pub dirs_listed: u64,
    pub added: u64,
    pub removed: u64,
    pub updated: u64,
    pub errors: u64,
    /// A network error indicated the filesystem is unreachable.
    pub unreachable: bool,
    /// Folders that were queued but not listed (cancelled or unreachable).
    pub frontier: Vec<EntryId>,
}

impl WalkReport {
    pub fn completed(&self) -> bool {
        self.frontier.is_empty() && !self.unreachable
    }
}

struct Job {
    dir: EntryId,
    path: Vec<u8>,
    recurse_existing: bool,
}

type JobResult = (Job, io::Result<Listing>);

const BATCH_MAX: usize = 512;
const BATCH_WINDOW: Duration = Duration::from_millis(20);

/// Lists `roots` and applies the results to `index`, descending as
/// configured. Returns when every queued folder is done, or the walk was
/// cancelled or found the filesystem unreachable.
pub fn walk(
    index: &RwLock<Index>,
    roots: &[EntryId],
    opts: &WalkOptions,
    progress: &Progress,
    cancel: &AtomicBool,
) -> WalkReport {
    let jobs: Vec<Job> = {
        let ix = index.read();
        roots
            .iter()
            .filter(|&&dir| ix.is_alive(dir) && ix.is_dir(dir))
            .map(|&dir| Job {
                dir,
                path: ix.path(dir),
                recurse_existing: opts.recurse_existing,
            })
            .collect()
    };
    let halt = AtomicBool::new(false);
    let (job_tx, job_rx) = unbounded::<Job>();
    let (res_tx, res_rx) = unbounded::<JobResult>();
    let mut report = WalkReport::default();

    std::thread::scope(|scope| {
        for _ in 0..opts.threads.max(1) {
            let (job_rx, res_tx) = (job_rx.clone(), res_tx.clone());
            let halt = &halt;
            scope.spawn(move || worker(job_rx, res_tx, opts, halt, cancel));
        }
        drop(res_tx);

        let mut outstanding = 0u64;
        for job in jobs {
            outstanding += 1;
            job_tx.send(job).expect("workers alive");
        }
        while outstanding > 0 {
            let Ok(first) = res_rx.recv() else { break };
            let mut batch = vec![first];
            let deadline = Instant::now() + BATCH_WINDOW;
            while batch.len() < BATCH_MAX {
                match res_rx.recv_deadline(deadline) {
                    Ok(r) => batch.push(r),
                    Err(_) => break,
                }
            }
            outstanding -= batch.len() as u64;
            let mut ix = index.write();
            for (job, result) in batch {
                let halted = halt.load(Ordering::Relaxed);
                match result {
                    Ok(listing) if !halted => {
                        let queued = apply(&mut ix, &job, listing, opts, &mut report);
                        for next in queued {
                            outstanding += 1;
                            job_tx.send(next).expect("workers alive");
                        }
                    }
                    Ok(_) => report.frontier.push(job.dir),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        report.frontier.push(job.dir)
                    }
                    Err(e) if fsops::is_unreachable(&e) => {
                        report.unreachable = true;
                        report.frontier.push(job.dir);
                        halt.store(true, Ordering::Relaxed);
                    }
                    Err(e) if is_benign(&e) => {}
                    Err(_) => {
                        report.errors += 1;
                        progress.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            drop(ix);
            if cancel.load(Ordering::Relaxed) {
                halt.store(true, Ordering::Relaxed);
            }
            progress.pending.store(outstanding, Ordering::Relaxed);
            progress
                .dirs_listed
                .store(report.dirs_listed, Ordering::Relaxed);
        }
        drop(job_tx);
    });
    progress.pending.store(0, Ordering::Relaxed);
    report
}

fn worker(
    jobs: Receiver<Job>,
    results: Sender<JobResult>,
    opts: &WalkOptions,
    halt: &AtomicBool,
    cancel: &AtomicBool,
) {
    for job in jobs.iter() {
        if halt.load(Ordering::Relaxed) || cancel.load(Ordering::Relaxed) {
            let _ = results.send((job, Err(io::ErrorKind::Interrupted.into())));
            continue;
        }
        if let Some(gate) = &opts.gate {
            gate.begin();
        }
        let started = Instant::now();
        let result = fsops::list_dir(&job.path, opts.cache);
        if let Some(gate) = &opts.gate {
            let outcome = match &result {
                Ok(_) => OpOutcome::Ok,
                Err(e) if fsops::is_unreachable(e) => OpOutcome::Unreachable,
                Err(_) => OpOutcome::Error,
            };
            gate.end(started.elapsed(), outcome);
        }
        if results.send((job, result)).is_err() {
            return;
        }
    }
}

/// Applies one listing and returns the subfolders to visit next.
fn apply(
    ix: &mut Index,
    job: &Job,
    listing: Listing,
    opts: &WalkOptions,
    report: &mut WalkReport,
) -> Vec<Job> {
    // The folder may have been removed or replaced by a watcher meanwhile.
    if !ix.is_alive(job.dir) || !ix.is_dir(job.dir) {
        return Vec::new();
    }
    ix.set_meta(job.dir, listing.dir);
    let outcome = ix.apply_listing(job.dir, &listing.entries);
    report.dirs_listed += 1;
    report.added += outcome.added as u64;
    report.removed += outcome.removed as u64;
    report.updated += outcome.updated as u64;

    let mount_points: HashSet<&[u8]> = listing.mount_points.iter().map(Vec::as_slice).collect();
    let existing = if job.recurse_existing {
        outcome.existing_dirs
    } else {
        Vec::new()
    };
    let mut next = Vec::new();
    for dir in outcome.new_dirs.into_iter().chain(existing) {
        let name = ix.name(dir);
        if mount_points.contains(name) {
            continue;
        }
        let path = fsops::join(&job.path, name);
        if opts.exclude.as_ref().is_some_and(|ex| ex(&path)) {
            continue;
        }
        next.push(Job {
            dir,
            path,
            recurse_existing: job.recurse_existing,
        });
    }
    next
}

/// Errors that just mean the folder changed under us.
fn is_benign(e: &io::Error) -> bool {
    use rustix::io::Errno;
    let Some(code) = e.raw_os_error() else {
        return false;
    };
    [Errno::NOENT, Errno::NOTDIR, Errno::LOOP]
        .iter()
        .any(|x| x.raw_os_error() == code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_core::{Meta, ROOT};
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    fn index_for(root: &Path) -> RwLock<Index> {
        RwLock::new(Index::new(root.as_os_str().as_bytes(), Meta::dir(0)))
    }

    fn paths(index: &RwLock<Index>) -> Vec<String> {
        let ix = index.read();
        let root_len = ix.root_path().len();
        let mut v: Vec<String> = ix
            .live_ids()
            .skip(1)
            .map(|id| String::from_utf8(ix.path(id)[root_len..].to_vec()).unwrap())
            .collect();
        v.sort();
        v
    }

    fn make_tree(root: &Path) {
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::create_dir_all(root.join("skip/deep")).unwrap();
        fs::write(root.join("a/one.txt"), b"1").unwrap();
        fs::write(root.join("a/b/two.txt"), b"22").unwrap();
        fs::write(root.join("a/b/c/three.txt"), b"333").unwrap();
        fs::write(root.join("top.txt"), b"").unwrap();
    }

    #[test]
    fn full_walk_mirrors_the_filesystem() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let index = index_for(tmp.path());
        let progress = Progress::default();
        let report = walk(
            &index,
            &[ROOT],
            &WalkOptions::default(),
            &progress,
            &AtomicBool::new(false),
        );
        assert!(report.completed());
        assert_eq!(report.errors, 0);
        assert_eq!(
            paths(&index),
            [
                "/a",
                "/a/b",
                "/a/b/c",
                "/a/b/c/three.txt",
                "/a/b/two.txt",
                "/a/one.txt",
                "/skip",
                "/skip/deep",
                "/top.txt"
            ]
        );
        let ix = index.read();
        let three = ix.resolve(&[tmp.path().as_os_str().as_bytes(), b"/a/b/c/three.txt"].concat());
        assert_eq!(ix.size(three.unwrap()), 3);
        assert_eq!(progress.dirs_listed.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn rescans_pick_up_changes() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let index = index_for(tmp.path());
        let opts = WalkOptions::default();
        let none = AtomicBool::new(false);
        walk(&index, &[ROOT], &opts, &Progress::default(), &none);

        fs::remove_dir_all(tmp.path().join("a/b")).unwrap();
        fs::create_dir_all(tmp.path().join("a/new/inner")).unwrap();
        fs::write(tmp.path().join("a/one.txt"), b"changed").unwrap();

        // Incremental: re-list only /a, descend only into new folders.
        let a = index.read().lookup(ROOT, b"a").unwrap();
        let inc = WalkOptions {
            recurse_existing: false,
            ..opts
        };
        let report = walk(&index, &[a], &inc, &Progress::default(), &none);
        assert_eq!(report.dirs_listed, 3, "a, a/new, a/new/inner");
        assert_eq!(report.removed, 4, "b, two.txt, c, three.txt");
        assert_eq!(report.updated, 1);
        let p = paths(&index);
        assert!(p.contains(&"/a/new/inner".to_owned()));
        assert!(!p.iter().any(|p| p.starts_with("/a/b")));
    }

    #[test]
    fn exclusions_are_not_descended() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let index = index_for(tmp.path());
        let skip = [tmp.path().as_os_str().as_bytes(), b"/skip"].concat();
        let opts = WalkOptions {
            exclude: Some(Arc::new(move |p: &[u8]| p == skip.as_slice())),
            ..Default::default()
        };
        walk(
            &index,
            &[ROOT],
            &opts,
            &Progress::default(),
            &AtomicBool::new(false),
        );
        let p = paths(&index);
        assert!(
            p.contains(&"/skip".to_owned()),
            "the folder itself is indexed"
        );
        assert!(!p.contains(&"/skip/deep".to_owned()));
    }

    #[test]
    fn cancellation_reports_frontier() {
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let index = index_for(tmp.path());
        let report = walk(
            &index,
            &[ROOT],
            &WalkOptions::default(),
            &Progress::default(),
            &AtomicBool::new(true),
        );
        assert!(!report.completed());
        assert_eq!(report.frontier, vec![ROOT]);
    }

    #[test]
    fn gate_sees_every_operation() {
        #[derive(Default)]
        struct Counting(AtomicU64, AtomicU64);
        impl Gate for Counting {
            fn begin(&self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn end(&self, _: Duration, outcome: OpOutcome) {
                assert_eq!(outcome, OpOutcome::Ok);
                self.1.fetch_add(1, Ordering::Relaxed);
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        make_tree(tmp.path());
        let index = index_for(tmp.path());
        let gate = Arc::new(Counting::default());
        let opts = WalkOptions {
            gate: Some(gate.clone()),
            threads: 3,
            ..Default::default()
        };
        walk(
            &index,
            &[ROOT],
            &opts,
            &Progress::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(gate.0.load(Ordering::Relaxed), 6);
        assert_eq!(gate.1.load(Ordering::Relaxed), 6);
    }
}
