//! `evos server ...` and `evos remote ...`: the network servers (ETP/FTP,
//! HTTP and the index server) and remote indexes.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Subcommand, ValueEnum};
use evos_proto::{IndexUser, RemoteIndexSpec, RemoteKind};

use crate::{connect, Failure};

#[derive(Subcommand)]
pub enum ServerCommand {
    /// Show the servers' settings.
    Status,
    /// Turn a server on, changing the given settings and keeping the rest.
    Enable(ServerArgs),
    /// Turn a server off.
    Disable {
        #[arg(value_enum)]
        server: Which,
    },
    /// Add or change an account of the index server.
    AddUser(UserArgs),
    /// Remove an account of the index server.
    RmUser { name: String },
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum Which {
    /// The ETP/FTP server (Everything's "Connect to ETP Server").
    Etp,
    /// The HTTP server.
    Http,
    /// The index server other EverythingOS computers copy from.
    Index,
}

#[derive(Args)]
pub struct ServerArgs {
    #[arg(value_enum)]
    server: Which,
    #[arg(long)]
    port: Option<u16>,
    /// Address to listen on, repeatable (default: all).
    #[arg(long)]
    bind: Vec<String>,
    /// Login name (ETP and HTTP); empty lets anyone in.
    #[arg(long)]
    user: Option<String>,
    /// Read the password from standard input.
    #[arg(long)]
    password_stdin: bool,
    /// Require no password.
    #[arg(long, conflicts_with = "password_stdin")]
    no_password: bool,
    /// The Linux account whose view of the files remote users get.
    #[arg(long)]
    run_as: Option<String>,
    /// Allow file downloads.
    #[arg(long, conflicts_with = "no_downloads")]
    downloads: bool,
    /// Refuse file downloads (searching still works).
    #[arg(long)]
    no_downloads: bool,
    /// ETP welcome message (%v: version, %n: new line).
    #[arg(long)]
    welcome: Option<String>,
}

#[derive(Args)]
pub struct UserArgs {
    name: String,
    /// Read the password from standard input instead of prompting.
    #[arg(long)]
    password_stdin: bool,
    /// Only this folder (and what is in it), repeatable.
    #[arg(long = "only")]
    include_only: Vec<String>,
}

#[derive(Subcommand)]
pub enum RemoteCommand {
    /// List remote indexes and how they are doing.
    List,
    /// Add or change a remote index; its index is copied right away.
    Add(RemoteArgs),
    /// Connect once with these settings.
    Test(RemoteArgs),
    /// Remove a remote index and the copy of its index.
    Rm { name: String },
}

#[derive(Args)]
pub struct RemoteArgs {
    /// Unique name (letters, digits, - and _).
    name: String,
    /// The other computer's name or address.
    host: String,
    /// Copy from an ETP server (such as Everything on Windows) instead of
    /// an EverythingOS index server.
    #[arg(long)]
    etp: bool,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    user: Option<String>,
    /// Read the password from standard input.
    #[arg(long)]
    password_stdin: bool,
    /// Ask for the password.
    #[arg(long, conflicts_with = "password_stdin")]
    ask_password: bool,
    /// Where its files appear (default /net/<name>).
    #[arg(long)]
    mount_as: Option<String>,
    /// Only this server folder, repeatable.
    #[arg(long = "only")]
    include_only: Vec<String>,
    /// Seconds between checks for changes (ETP: full copies).
    #[arg(long)]
    refresh: Option<u64>,
    /// Define it without copying yet.
    #[arg(long)]
    disabled: bool,
}

fn read_password_line() -> Result<String, Failure> {
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

impl RemoteArgs {
    fn spec(&self) -> RemoteIndexSpec {
        RemoteIndexSpec {
            name: self.name.clone(),
            kind: if self.etp {
                RemoteKind::Etp
            } else {
                RemoteKind::Evos
            },
            host: self.host.clone(),
            port: self.port,
            username: self.user.clone().unwrap_or_default(),
            mount_as: self.mount_as.clone(),
            include_only: self.include_only.clone(),
            refresh_secs: self.refresh,
            enabled: !self.disabled,
        }
    }

    /// `None` keeps a stored password.
    fn password(&self) -> Result<Option<String>, Failure> {
        if self.password_stdin {
            return read_password_line().map(Some);
        }
        if self.ask_password {
            return Ok(Some(rpassword::prompt_password(format!(
                "Password for {} on {}: ",
                self.user.as_deref().unwrap_or(""),
                self.host
            ))?));
        }
        Ok(None)
    }
}

fn which_name(w: Which) -> &'static str {
    match w {
        Which::Etp => "etp",
        Which::Http => "http",
        Which::Index => "index",
    }
}

fn address(bind: &[String], port: u16) -> String {
    if bind.is_empty() {
        format!("*:{port}")
    } else {
        bind.iter()
            .map(|b| {
                if b.contains(':') {
                    format!("[{b}]:{port}")
                } else {
                    format!("{b}:{port}")
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

pub fn run_server(socket: &PathBuf, cmd: ServerCommand) -> Result<ExitCode, Failure> {
    let mut client = connect(socket)?;
    let mut s = client.servers()?;
    match cmd {
        ServerCommand::Status => {
            let mut out = io::stdout().lock();
            writeln!(
                out,
                "{:<6} {:<5} {:<24} {:<26} {:<10} DOWNLOADS",
                "SERVER", "STATE", "ADDRESS", "LOGIN", "RUN AS"
            )?;
            let login = |user: &str, set: bool| match (user.is_empty(), set) {
                (true, false) => "anyone".to_owned(),
                (true, true) => "any name, password".to_owned(),
                (false, false) => user.to_owned(),
                (false, true) => format!("{user}, password"),
            };
            let on = |b: bool| if b { "on" } else { "off" };
            let yes = |b: bool| if b { "yes" } else { "no" };
            let e = &s.etp;
            writeln!(
                out,
                "{:<6} {:<5} {:<24} {:<26} {:<10} {}",
                "etp",
                on(e.enabled),
                address(&e.bind, e.port),
                login(&e.username, e.password_set),
                e.run_as,
                yes(e.allow_file_download)
            )?;
            let h = &s.http;
            writeln!(
                out,
                "{:<6} {:<5} {:<24} {:<26} {:<10} {}",
                "http",
                on(h.enabled),
                address(&h.bind, h.port),
                login(&h.username, h.password_set),
                h.run_as,
                yes(h.allow_file_download)
            )?;
            let i = &s.index;
            let accounts: Vec<&str> = i.users.iter().map(|u| u.name.as_str()).collect();
            writeln!(
                out,
                "{:<6} {:<5} {:<24} {:<26} {:<10} {}",
                "index",
                on(i.enabled),
                address(&i.bind, i.port),
                if accounts.is_empty() {
                    "no accounts".to_owned()
                } else {
                    accounts.join(", ")
                },
                i.run_as,
                yes(i.allow_file_download)
            )?;
            return Ok(ExitCode::SUCCESS);
        }
        ServerCommand::Enable(a) => {
            let password = if a.no_password {
                Some(String::new())
            } else if a.password_stdin {
                Some(read_password_line()?)
            } else {
                None
            };
            let downloads = match (a.downloads, a.no_downloads) {
                (true, _) => Some(true),
                (_, true) => Some(false),
                _ => None,
            };
            macro_rules! common {
                ($srv:expr) => {{
                    let srv = $srv;
                    srv.enabled = true;
                    if let Some(p) = a.port {
                        srv.port = p;
                    }
                    if !a.bind.is_empty() {
                        srv.bind = a.bind.clone();
                    }
                    if let Some(r) = &a.run_as {
                        srv.run_as = r.clone();
                    }
                    if let Some(d) = downloads {
                        srv.allow_file_download = d;
                    }
                }};
            }
            match a.server {
                Which::Etp => {
                    common!(&mut s.etp);
                    if let Some(u) = &a.user {
                        s.etp.username = u.clone();
                    }
                    s.etp.password = password;
                    if let Some(w) = &a.welcome {
                        s.etp.welcome_message = w.clone();
                    }
                }
                Which::Http => {
                    common!(&mut s.http);
                    if let Some(u) = &a.user {
                        s.http.username = u.clone();
                    }
                    s.http.password = password;
                }
                Which::Index => {
                    if a.user.is_some() || password.is_some() {
                        return Err(Failure::Usage(
                            "index server logins are accounts: use `evos server add-user`".into(),
                        ));
                    }
                    common!(&mut s.index);
                }
            }
            client.set_servers(s)?;
            println!("the {} server is on", which_name(a.server));
        }
        ServerCommand::Disable { server } => {
            match server {
                Which::Etp => s.etp.enabled = false,
                Which::Http => s.http.enabled = false,
                Which::Index => s.index.enabled = false,
            }
            client.set_servers(s)?;
            println!("the {} server is off", which_name(server));
        }
        ServerCommand::AddUser(u) => {
            let password = if u.password_stdin {
                read_password_line()?
            } else {
                rpassword::prompt_password(format!("Password for {}: ", u.name))?
            };
            s.index.users.retain(|x| x.name != u.name);
            s.index.users.push(IndexUser {
                name: u.name.clone(),
                password: Some(password),
                password_set: false,
                include_only: u.include_only,
            });
            client.set_servers(s)?;
            println!("{} can log in to the index server", u.name);
        }
        ServerCommand::RmUser { name } => {
            let before = s.index.users.len();
            s.index.users.retain(|x| x.name != name);
            if s.index.users.len() == before {
                return Err(Failure::Usage(format!("no account named {name}")));
            }
            client.set_servers(s)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub fn run_remote(socket: &PathBuf, cmd: RemoteCommand) -> Result<ExitCode, Failure> {
    match cmd {
        RemoteCommand::List => {
            let list = connect(socket)?.remote_indexes()?;
            let mut out = io::stdout().lock();
            if list.is_empty() {
                writeln!(out, "no remote indexes; add one with `evos remote add`")?;
                return Ok(ExitCode::SUCCESS);
            }
            writeln!(
                out,
                "{:<14} {:<5} {:<28} {:<24} {:<10} {:>10}  ERROR",
                "NAME", "TYPE", "SERVER", "SHOWN UNDER", "STATE", "ENTRIES"
            )?;
            for r in list {
                let s = &r.spec;
                let kind = match s.kind {
                    RemoteKind::Evos => "evos",
                    RemoteKind::Etp => "etp",
                };
                writeln!(
                    out,
                    "{:<14} {:<5} {:<28} {:<24} {:<10} {:>10}  {}",
                    s.name,
                    kind,
                    format!("{}:{}", s.host, s.port()),
                    s.mount_as(),
                    if s.enabled {
                        r.phase.as_str()
                    } else {
                        "disabled"
                    },
                    r.entries,
                    r.error.unwrap_or_default()
                )?;
            }
        }
        RemoteCommand::Add(a) | RemoteCommand::Test(a) if a.spec().validate().is_err() => {
            return Err(Failure::Usage(a.spec().validate().unwrap_err()));
        }
        RemoteCommand::Add(a) => {
            let password = a.password()?;
            connect(socket)?.remote_index_add(a.spec(), password)?;
            println!("{} is copied to {}", a.name, a.spec().mount_as());
        }
        RemoteCommand::Test(a) => {
            let password = a.password()?;
            let server = connect(socket)?.remote_index_test(a.spec(), password)?;
            println!("{}: OK {}", a.host, server.trim());
        }
        RemoteCommand::Rm { name } => connect(socket)?.remote_index_remove(&name)?,
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(address(&[], 21), "*:21");
        assert_eq!(
            address(&["127.0.0.1".into(), "::1".into()], 80),
            "127.0.0.1:80,[::1]:80"
        );
    }
}
