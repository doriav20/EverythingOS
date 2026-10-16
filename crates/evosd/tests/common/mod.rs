//! A daemon over a real (tmpfs) volume, answering on a socket in a
//! temporary folder.

#![allow(dead_code)]

use std::os::unix::ffi::OsStrExt;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use evos_mount::MountManager;
use evos_proto::{Client, SearchOptions, SearchResult, Sort};
use evosd::config::{Config, LocalConfig};
use evosd::history::RunHistory;
use evosd::remote::RemoteManager;
use evosd::server::{serve, Daemon};
use evosd::servers::ServerManager;
use evosd::volumes::VolumeManager;

/// Each fixture indexes only its own tmpfs, chosen by skipping every other
/// mount; a fixture created concurrently would see the other's tmpfs, so
/// tests using fixtures run one at a time.
pub static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct Fixture {
    pub tmp: tempfile::TempDir,
    pub disk: std::path::PathBuf,
    pub socket: std::path::PathBuf,
    pub daemon: Arc<Daemon>,
    pub rt: tokio::runtime::Runtime,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.daemon.servers.stop();
        self.daemon.remotes.shutdown();
        self.daemon.volumes.shutdown();
        let _ = Command::new("umount").arg(&self.disk).status();
    }
}

pub fn fixture() -> Option<Fixture> {
    let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let disk = tmp.path().join("disk");
    std::fs::create_dir_all(&disk).unwrap();
    let ok = Command::new("mount")
        .args(["-t", "tmpfs", "evos-test"])
        .arg(&disk)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        return None;
    }
    for d in ["docs/reports", "photos"] {
        std::fs::create_dir_all(disk.join(d)).unwrap();
    }
    std::fs::write(disk.join("docs/reports/q1 report.pdf"), vec![0u8; 3000]).unwrap();
    std::fs::write(disk.join("docs/reports/q2 report.pdf"), vec![0u8; 1000]).unwrap();
    std::fs::write(disk.join("photos/cat.jpg"), vec![0u8; 2000]).unwrap();
    std::fs::write(disk.join("photos/מסמך חשוב.txt"), "שלום עולם").unwrap();

    let disk_bytes = disk.as_os_str().as_bytes().to_vec();
    let skip = evos_scan::mountinfo::read_mounts()
        .unwrap()
        .into_iter()
        .filter(|m| m.mount_point != disk_bytes)
        .map(|m| String::from_utf8_lossy(&m.mount_point).into_owned())
        .collect();
    let cfg = Arc::new(Config {
        state_dir: tmp.path().join("state"),
        local: LocalConfig {
            index_tmpfs: true,
            skip_mounts: skip,
            ..Default::default()
        },
        ..Default::default()
    });
    std::fs::create_dir_all(&cfg.state_dir).unwrap();
    let mounts =
        Arc::new(MountManager::open(tmp.path().join("etc"), tmp.path().join("run")).unwrap());
    let volumes = Arc::new(VolumeManager::new(cfg.clone(), mounts.clone()));
    volumes.sync();
    let (events, _) = tokio::sync::broadcast::channel(16);
    let history = Arc::new(RunHistory::load(cfg.state_dir.join("run-history")));
    let servers = Arc::new(ServerManager::load(&tmp.path().join("etc")));
    let remotes = Arc::new(RemoteManager::open(
        &tmp.path().join("etc"),
        volumes.clone(),
    ));
    let daemon = Arc::new(Daemon {
        cfg,
        volumes,
        mounts,
        history,
        servers,
        remotes,
        events,
    });

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let socket = tmp.path().join("evosd.sock");
    let listener = rt
        .block_on(async { tokio::net::UnixListener::bind(&socket) })
        .unwrap();
    rt.spawn(serve(daemon.clone(), listener));
    Some(Fixture {
        tmp,
        disk,
        socket,
        daemon,
        rt,
        _serial: serial,
    })
}

pub fn wait_indexed(client: &mut Client, query: &str, expected: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let SearchResult {
            id: result, total, ..
        } = client
            .search(query, SearchOptions::default(), Sort::default())
            .unwrap();
        client.release(result).unwrap();
        if total == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{query:?}: {total} hits, wanted {expected}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn names(client: &mut Client, result: u32) -> Vec<String> {
    client
        .fetch(result, 0, 100)
        .unwrap()
        .iter()
        .map(|r| String::from_utf8_lossy(&r.name).into_owned())
        .collect()
}
