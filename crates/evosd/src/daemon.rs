//! Daemon lifecycle: start-up, background threads, the socket and a clean
//! shutdown that saves every index.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use evos_mount::MountManager;
use evos_scan::mountinfo::MountWatcher;

use crate::config::Config;
use crate::history::RunHistory;
use crate::remote::RemoteManager;
use crate::server::{serve, spawn_event_ticker, Daemon};
use crate::servers::ServerManager;
use crate::volumes::VolumeManager;

/// Runs the daemon until SIGTERM or SIGINT.
pub fn run(cfg: Config) -> Result<(), String> {
    for (dir, mode) in [
        (&cfg.state_dir, 0o755),
        (&cfg.run_dir, 0o755),
        (&cfg.config_dir, 0o755),
    ] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let cfg = Arc::new(cfg);
    let mounts = Arc::new(
        MountManager::open(&cfg.config_dir, &cfg.run_dir)
            .map_err(|e| format!("cannot load mounts: {e}"))?,
    );
    let volumes = Arc::new(VolumeManager::new(cfg.clone(), mounts.clone()));
    let history = Arc::new(RunHistory::load(cfg.state_dir.join("run-history")));
    let servers = Arc::new(ServerManager::load(&cfg.config_dir));
    let remotes = Arc::new(RemoteManager::open(&cfg.config_dir, volumes.clone()));

    // Mount managed shares first so they are indexed from the start.
    mounts.supervise();
    volumes.sync();
    remotes.start();

    let stop = Arc::new(AtomicBool::new(false));
    let threads = vec![
        spawn_loop("evos-mounts", stop.clone(), Duration::from_secs(10), {
            let (mounts, volumes) = (mounts.clone(), volumes.clone());
            move || {
                if mounts.supervise() {
                    volumes.sync();
                }
            }
        }),
        spawn_loop("evos-snapshots", stop.clone(), cfg.snapshot_interval(), {
            let volumes = volumes.clone();
            move || volumes.save_snapshots(false)
        }),
        spawn_loop("evos-history", stop.clone(), Duration::from_secs(30), {
            let history = history.clone();
            move || history.save()
        }),
        spawn_mount_watcher(stop.clone(), volumes.clone()),
    ];

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("evos-io")
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))?;
    let result = runtime.block_on(async {
        let listener = bind_socket(&cfg.socket)?;
        let (events, _) = tokio::sync::broadcast::channel(64);
        let daemon = Arc::new(Daemon {
            cfg: cfg.clone(),
            volumes: volumes.clone(),
            mounts: mounts.clone(),
            history: history.clone(),
            servers: servers.clone(),
            remotes: remotes.clone(),
            events,
        });
        spawn_event_ticker(daemon.clone());
        // A server that cannot start (port taken) must not stop searching.
        if let Err(e) = servers.restart(&daemon).await {
            tracing::warn!("{e}");
        }
        let server = tokio::spawn(serve(daemon, listener));
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]);
        tracing::info!(socket = %cfg.socket.display(), "evosd {} ready", env!("CARGO_PKG_VERSION"));
        shutdown_signal().await;
        server.abort();
        servers.stop();
        Ok::<(), String>(())
    });

    tracing::info!("shutting down");
    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
    remotes.shutdown();
    // Join the background threads first so none of them re-attaches a
    // volume while the others are being saved.
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    volumes.shutdown();
    history.save();
    let _ = std::fs::remove_file(&cfg.socket);
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

/// Calls `f` every `interval` until `stop` is set.
fn spawn_loop(
    name: &str,
    stop: Arc<AtomicBool>,
    interval: Duration,
    mut f: impl FnMut() + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let mut next = Instant::now() + interval;
            while !stop.load(Ordering::Relaxed) {
                if Instant::now() >= next {
                    f();
                    next = Instant::now() + interval;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        })
        .expect("spawn thread")
}

/// Re-syncs volumes whenever something is mounted or unmounted.
fn spawn_mount_watcher(
    stop: Arc<AtomicBool>,
    volumes: Arc<VolumeManager>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("evos-mountinfo".into())
        .spawn(move || {
            let mut watcher = match MountWatcher::new() {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("cannot watch the mount table: {e}");
                    return;
                }
            };
            while !stop.load(Ordering::Relaxed) {
                match watcher.wait(500) {
                    Ok(true) => {
                        // Let a burst of mount changes settle.
                        std::thread::sleep(Duration::from_millis(300));
                        volumes.sync();
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!("mount table watch failed: {e}");
                        std::thread::sleep(Duration::from_secs(5));
                    }
                }
            }
        })
        .expect("spawn thread")
}

/// Binds the client socket, replacing a stale one but never stealing it
/// from a running daemon. Every local user may connect; results are
/// filtered per user.
fn bind_socket(path: &Path) -> Result<tokio::net::UnixListener, String> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(format!(
                "another evosd is already listening on {}",
                path.display()
            ));
        }
        let _ = std::fs::remove_file(path);
    }
    let listener = tokio::net::UnixListener::bind(path)
        .map_err(|e| format!("cannot listen on {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("cannot set permissions on {}: {e}", path.display()))?;
    Ok(listener)
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
