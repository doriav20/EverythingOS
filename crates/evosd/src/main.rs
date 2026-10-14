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
}

fn main() -> ExitCode {
    let args = Args::parse();
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
