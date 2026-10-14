use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use evosd::config::Config;

/// EverythingOS indexing daemon.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Configuration file.
    #[arg(long, default_value = "/etc/evos/evosd.toml")]
    config: PathBuf,
    /// Override the socket path.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Override the snapshot directory.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Log filter, e.g. `info` or `evosd=debug,evos_scan=debug`.
    #[arg(long, env = "EVOS_LOG", default_value = "info")]
    log: String,
    /// Join the mount namespace of PID 1 before starting. Lets a
    /// container started with `pid: host` index and mount on the host
    /// with the host's real paths.
    #[arg(long)]
    host_mount_namespace: bool,
}

/// Moves this (still single-threaded) process into the host's mount
/// namespace.
fn enter_host_mount_namespace() -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let ns = std::fs::File::open("/proc/1/ns/mnt")?;
    // SAFETY: plain syscall on a valid namespace fd; no other threads exist
    // yet, as setns(CLONE_NEWNS) requires.
    if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNS) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    std::env::set_current_dir("/")
}

fn main() -> ExitCode {
    let args = Args::parse();
    if args.host_mount_namespace {
        if let Err(e) = enter_host_mount_namespace() {
            eprintln!("evosd: cannot join the host mount namespace: {e}");
            return ExitCode::FAILURE;
        }
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(&args.log))
        .with_target(false)
        .init();

    let mut cfg = match Config::load(&args.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(socket) = args.socket {
        cfg.socket = socket;
    }
    if let Some(dir) = args.state_dir {
        cfg.state_dir = dir;
    }
    match evosd::daemon::run(cfg) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}
