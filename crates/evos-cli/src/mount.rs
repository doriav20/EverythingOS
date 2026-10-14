//! `evos mount ...`: manage shares and folders mounted by the daemon.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand, ValueEnum};
use evos_proto::{Credentials, IndexSettings, MountKind, MountSpec, Request};

use crate::{connect, Failure};

#[derive(Subcommand)]
pub enum MountCommand {
    /// List defined mounts.
    List,
    /// Define a mount (replacing one with the same name) and mount it.
    Add(AddArgs),
    /// Check settings by mounting temporarily and listing the root.
    Test(AddArgs),
    /// Unmount and forget a mount.
    Rm { name: String },
    /// Mount now and keep it mounted.
    Up { name: String },
    /// Unmount and keep it unmounted.
    Down { name: String },
}

#[derive(Copy, Clone, ValueEnum)]
enum KindArg {
    Smb,
    Nfs,
    Sshfs,
    Bind,
    Existing,
}

#[derive(Args)]
pub struct AddArgs {
    /// Unique name (letters, digits, - and _).
    name: String,
    #[arg(long = "type", value_enum)]
    kind: KindArg,
    /// //server/share, server:/export, user@host:/path or /local/folder.
    #[arg(long)]
    source: String,
    /// Where to mount (default /mnt/evos/<name>).
    #[arg(long)]
    mountpoint: Option<String>,
    /// Extra mount option, repeatable (e.g. -o vers=3.0).
    #[arg(short = 'o', long = "option")]
    options: Vec<String>,
    /// SMB user name; the password is prompted for.
    #[arg(long)]
    user: Option<String>,
    /// SMB domain or workgroup.
    #[arg(long)]
    domain: Option<String>,
    /// Read the password from standard input instead of prompting.
    #[arg(long)]
    password_stdin: bool,
    /// Do not mount automatically at start-up.
    #[arg(long)]
    no_auto: bool,
    /// Mount, but do not index.
    #[arg(long)]
    no_index: bool,
    /// Most parallel requests to the server.
    #[arg(long)]
    max_concurrency: Option<u32>,
    /// Most filesystem operations per second against the server.
    #[arg(long)]
    max_ops: Option<f64>,
    /// Path not to index (absolute, or relative to the mount), repeatable.
    #[arg(long)]
    exclude: Vec<String>,
    /// Seconds between checks of recently changed folders.
    #[arg(long)]
    recheck: Option<u64>,
    /// Longest time in seconds any folder may go unchecked.
    #[arg(long)]
    staleness: Option<u64>,
    /// Do not use SMB change notifications.
    #[arg(long)]
    no_smb_notify: bool,
}

impl AddArgs {
    fn spec(&self) -> MountSpec {
        MountSpec {
            name: self.name.clone(),
            kind: match self.kind {
                KindArg::Smb => MountKind::Smb,
                KindArg::Nfs => MountKind::Nfs,
                KindArg::Sshfs => MountKind::Sshfs,
                KindArg::Bind => MountKind::Bind,
                KindArg::Existing => MountKind::Existing,
            },
            source: self.source.clone(),
            mountpoint: self.mountpoint.clone(),
            options: self.options.clone(),
            auto_mount: !self.no_auto,
            index: IndexSettings {
                enabled: !self.no_index,
                max_concurrency: self.max_concurrency,
                max_ops_per_sec: self.max_ops,
                exclude: self.exclude.clone(),
                min_recheck_secs: self.recheck,
                max_staleness_secs: self.staleness,
                smb_notify: !self.no_smb_notify,
            },
        }
    }

    fn credentials(&self) -> Result<Option<Credentials>, Failure> {
        let Some(user) = &self.user else {
            return Ok(None);
        };
        let password = if self.password_stdin {
            let mut line = String::new();
            io::stdin().lock().read_line(&mut line)?;
            line.trim_end_matches(['\r', '\n']).to_owned()
        } else {
            rpassword::prompt_password(format!("Password for {user} on {}: ", self.source))?
        };
        Ok(Some(Credentials {
            username: user.clone(),
            password,
            domain: self.domain.clone(),
        }))
    }
}

pub fn run(socket: &PathBuf, cmd: MountCommand) -> Result<ExitCode, Failure> {
    match cmd {
        MountCommand::List => {
            let mut client = connect(socket)?;
            let mounts = client.mounts()?;
            let mut out = io::stdout().lock();
            if mounts.is_empty() {
                writeln!(out, "no mounts defined; add one with `evos mount add`")?;
                return Ok(ExitCode::SUCCESS);
            }
            writeln!(
                out,
                "{:<16} {:<9} {:<32} {:<28} STATE",
                "NAME", "TYPE", "SOURCE", "MOUNTPOINT"
            )?;
            for m in mounts {
                let kind = format!("{:?}", m.spec.kind).to_lowercase();
                let state = match (&m.error, m.mounted) {
                    (_, true) => "mounted".to_owned(),
                    (Some(e), false) => format!("error: {e}"),
                    (None, false) if !m.spec.auto_mount => "down".to_owned(),
                    (None, false) => "unmounted".to_owned(),
                };
                writeln!(
                    out,
                    "{:<16} {:<9} {:<32} {:<28} {}",
                    m.spec.name,
                    kind,
                    m.spec.source,
                    m.spec.mountpoint(),
                    state
                )?;
            }
            Ok(ExitCode::SUCCESS)
        }
        MountCommand::Add(args) | MountCommand::Test(args) if args.spec().validate().is_err() => {
            Err(Failure::Usage(args.spec().validate().unwrap_err()))
        }
        MountCommand::Add(args) => {
            let credentials = args.credentials()?;
            let mut client = connect(socket)?;
            client.expect_ok(Request::MountAdd {
                spec: args.spec(),
                credentials,
            })?;
            println!(
                "{} is defined{}",
                args.name,
                if args.no_auto { "" } else { " and mounted" }
            );
            Ok(ExitCode::SUCCESS)
        }
        MountCommand::Test(args) => {
            let credentials = args.credentials()?;
            let mut client = connect(socket)?;
            client.expect_ok(Request::MountTest {
                spec: args.spec(),
                credentials,
            })?;
            println!("{}: OK", args.source);
            Ok(ExitCode::SUCCESS)
        }
        MountCommand::Rm { name } => simple(socket, Request::MountRemove { name }),
        MountCommand::Up { name } => simple(socket, Request::MountUp { name }),
        MountCommand::Down { name } => simple(socket, Request::MountDown { name }),
    }
}

fn simple(socket: &PathBuf, request: Request) -> Result<ExitCode, Failure> {
    connect(socket)?.expect_ok(request)?;
    Ok(ExitCode::SUCCESS)
}
