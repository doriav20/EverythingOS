//! Building and running mount and unmount commands.
//!
//! Network mounts default to *soft* semantics with bounded timeouts: a dead
//! server must make filesystem calls fail with an error, never hang the
//! scanner threads in uninterruptible sleep, which is what hard NFS mounts
//! (the kernel default) would do.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use evos_proto::{MountKind, MountSpec};

/// A command line to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandLine {
    fn new(program: &str) -> Self {
        CommandLine {
            program: program.into(),
            args: Vec::new(),
        }
    }

    fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }
}

/// Default options per kind. User options come after and win.
fn default_options(kind: MountKind) -> &'static [&'static str] {
    match kind {
        MountKind::Smb => &["soft", "echo_interval=10", "actimeo=1", "iocharset=utf8"],
        MountKind::Nfs => &["soft", "timeo=150", "retrans=3"],
        MountKind::Sshfs => &[
            "reconnect",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
            "allow_other",
            "default_permissions",
            "StrictHostKeyChecking=accept-new",
        ],
        MountKind::Bind | MountKind::Existing => &[],
    }
}

/// The command that mounts `spec` at `mountpoint`, or `None` for
/// [`MountKind::Existing`], which is never mounted.
pub fn mount_command(
    spec: &MountSpec,
    mountpoint: &str,
    credentials: Option<&Path>,
) -> Option<CommandLine> {
    let mut options: Vec<String> = default_options(spec.kind)
        .iter()
        .map(|s| s.to_string())
        .collect();
    if spec.kind == MountKind::Smb {
        options.push(match credentials {
            Some(path) => format!("credentials={}", path.display()),
            None => "guest".into(),
        });
    }
    options.extend(spec.options.iter().cloned());
    let opts = options.join(",");
    Some(match spec.kind {
        MountKind::Existing => return None,
        MountKind::Smb => CommandLine::new("mount")
            .arg("-t")
            .arg("cifs")
            .arg(&spec.source)
            .arg(mountpoint)
            .arg("-o")
            .arg(opts),
        MountKind::Nfs => CommandLine::new("mount")
            .arg("-t")
            .arg("nfs")
            .arg(&spec.source)
            .arg(mountpoint)
            .arg("-o")
            .arg(opts),
        MountKind::Sshfs => CommandLine::new("sshfs")
            .arg(&spec.source)
            .arg(mountpoint)
            .arg("-o")
            .arg(opts),
        MountKind::Bind => {
            let cmd = CommandLine::new("mount")
                .arg("--bind")
                .arg(&spec.source)
                .arg(mountpoint);
            if opts.is_empty() {
                cmd
            } else {
                cmd.arg("-o").arg(opts)
            }
        }
    })
}

/// Runs a command, killing it after `timeout`. Returns stderr on failure.
pub fn run(cmd: &CommandLine, timeout: Duration) -> Result<(), String> {
    let mut child = Command::new(&cmd.program)
        .args(&cmd.args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", cmd.program))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{} timed out after {}s",
                    cmd.program,
                    timeout.as_secs()
                ));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if status.success() {
        return Ok(());
    }
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let stderr = stderr.trim();
    Err(if stderr.is_empty() {
        format!("{} failed with {status}", cmd.program)
    } else {
        stderr.to_owned()
    })
}

/// Unmounts, falling back to a lazy unmount if the mount is busy (for
/// example a blocked SMB change-notify request still holds it).
pub fn unmount(mountpoint: &str, timeout: Duration) -> Result<(), String> {
    match run(&CommandLine::new("umount").arg(mountpoint), timeout) {
        Ok(()) => Ok(()),
        Err(e) if e.contains("busy") => run(
            &CommandLine::new("umount").arg("-l").arg(mountpoint),
            timeout,
        ),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_proto::IndexSettings;

    fn spec(kind: MountKind, source: &str, options: &[&str]) -> MountSpec {
        MountSpec {
            name: "n".into(),
            kind,
            source: source.into(),
            mountpoint: None,
            options: options.iter().map(|s| s.to_string()).collect(),
            auto_mount: true,
            index: IndexSettings::default(),
        }
    }

    #[test]
    fn smb_uses_credentials_file_or_guest() {
        let s = spec(MountKind::Smb, "//nas/share", &["vers=3.1.1"]);
        let cmd = mount_command(
            &s,
            "/mnt/evos/n",
            Some(Path::new("/etc/evos/credentials/n")),
        )
        .unwrap();
        assert_eq!(cmd.program, "mount");
        assert_eq!(cmd.args[..4], ["-t", "cifs", "//nas/share", "/mnt/evos/n"]);
        assert_eq!(
            cmd.args[5],
            "soft,echo_interval=10,actimeo=1,iocharset=utf8,credentials=/etc/evos/credentials/n,vers=3.1.1"
        );
        let guest = mount_command(&s, "/m", None).unwrap();
        assert!(guest.args[5].contains(",guest,"));
    }

    #[test]
    fn nfs_is_soft_by_default() {
        let cmd = mount_command(&spec(MountKind::Nfs, "nas:/export", &[]), "/m", None).unwrap();
        assert_eq!(
            cmd.args,
            [
                "-t",
                "nfs",
                "nas:/export",
                "/m",
                "-o",
                "soft,timeo=150,retrans=3"
            ]
        );
    }

    #[test]
    fn sshfs_and_bind() {
        let cmd = mount_command(
            &spec(MountKind::Sshfs, "me@h:/d", &["IdentityFile=/k"]),
            "/m",
            None,
        )
        .unwrap();
        assert_eq!(cmd.program, "sshfs");
        assert!(cmd.args[3].starts_with("reconnect,") && cmd.args[3].ends_with(",IdentityFile=/k"));
        let bind = mount_command(&spec(MountKind::Bind, "/srv/x", &[]), "/m", None).unwrap();
        assert_eq!(bind.args, ["--bind", "/srv/x", "/m"]);
        assert!(mount_command(&spec(MountKind::Existing, "/x", &[]), "/x", None).is_none());
    }

    #[test]
    fn run_reports_stderr_and_timeouts() {
        assert!(run(&CommandLine::new("true"), Duration::from_secs(5)).is_ok());
        let err = run(
            &CommandLine::new("sh")
                .arg("-c")
                .arg("echo boom >&2; exit 3"),
            Duration::from_secs(5),
        );
        assert_eq!(err.unwrap_err(), "boom");
        let slow = run(
            &CommandLine::new("sleep").arg("5"),
            Duration::from_millis(200),
        );
        assert!(slow.unwrap_err().contains("timed out"));
    }
}
