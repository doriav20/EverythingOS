//! The network servers, as in Everything's options: the ETP/FTP server,
//! the HTTP server and the index server (EverythingOS's counterpart of the
//! Everything Server).
//!
//! Settings live in `<config_dir>/servers.toml`, readable by root only,
//! with salted password hashes. Remote users get what a chosen Linux
//! account ("run as") may see and read, and nothing is ever written.

pub mod etp;
pub mod files;
pub mod http;
pub mod index;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use evos_core::perm::Caller;
use evos_proto::ServerSettings;
use parking_lot::Mutex;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::accounts::{caller_of, hash_password};
use crate::server::Daemon;

/// What a server listens on: enabled, interfaces, port. Other settings
/// apply to the next login without a restart.
type Listen = (bool, Vec<String>, u16);

struct Running {
    listen: Listen,
    /// One accept loop per address; each owns its connections.
    tasks: Vec<JoinHandle<()>>,
}

/// The running servers and their settings.
pub struct ServerManager {
    file: PathBuf,
    /// Settings with password hashes in the password fields.
    stored: Mutex<ServerSettings>,
    running: Mutex<HashMap<&'static str, Running>>,
}

/// Accepts connections until the task is aborted, which also ends every
/// connection it accepted.
pub(crate) async fn accept_loop<F>(
    listener: TcpListener,
    max_clients: usize,
    busy: &'static [u8],
    connection: impl Fn(tokio::net::TcpStream, SocketAddr) -> F,
) where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut connections = tokio::task::JoinSet::new();
    loop {
        let (mut stream, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                // Out of file descriptors, most likely: let some close.
                tracing::debug!("accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        while connections.try_join_next().is_some() {}
        if connections.len() >= max_clients {
            use tokio::io::AsyncWriteExt;
            let _ = stream.write_all(busy).await;
            continue;
        }
        connections.spawn(connection(stream, peer));
    }
}

/// The listen addresses of a server: the given interfaces, or all.
fn addresses(bind: &[String], port: u16) -> Result<Vec<SocketAddr>, String> {
    if bind.iter().all(|b| b.trim().is_empty()) {
        return Ok(vec![SocketAddr::new(IpAddr::from([0u8; 16]), port)]);
    }
    bind.iter()
        .filter(|b| !b.trim().is_empty())
        .map(|b| {
            b.trim()
                .parse::<IpAddr>()
                .map(|ip| SocketAddr::new(ip, port))
                .map_err(|_| format!("not an address to listen on: {b}"))
        })
        .collect()
}

/// Binds every address; dual-stack for the IPv6 wildcard.
fn bind_all(addrs: &[SocketAddr]) -> Result<Vec<std::net::TcpListener>, String> {
    addrs
        .iter()
        .map(|addr| {
            let socket = socket2::Socket::new(
                socket2::Domain::for_address(*addr),
                socket2::Type::STREAM,
                None,
            )
            .map_err(|e| e.to_string())?;
            if addr.is_ipv6() {
                let _ = socket.set_only_v6(false);
            }
            socket.set_reuse_address(true).map_err(|e| e.to_string())?;
            socket
                .bind(&(*addr).into())
                .and_then(|()| socket.listen(128))
                .map_err(|e| format!("cannot listen on port {}: {e}", addr.port()))?;
            socket.set_nonblocking(true).map_err(|e| e.to_string())?;
            Ok(socket.into())
        })
        .collect()
}

/// A stored password: a new value is hashed, `None` keeps the old one.
fn merge_password(new: &Option<String>, old: &Option<String>) -> Option<String> {
    match new {
        Some(p) if p.is_empty() => Some(String::new()),
        Some(p) => Some(hash_password(p)),
        None => old.clone(),
    }
}

/// The account remote users act as.
pub fn run_as(user: &str) -> Result<Caller, String> {
    caller_of(user).ok_or_else(|| format!("there is no account named {user:?} to run as"))
}

impl ServerManager {
    pub fn load(config_dir: &std::path::Path) -> ServerManager {
        let file = config_dir.join("servers.toml");
        let stored = std::fs::read_to_string(&file)
            .ok()
            .and_then(|s| match toml::from_str(&s) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!("ignoring {}: {e}", file.display());
                    None
                }
            })
            .unwrap_or_default();
        ServerManager {
            file,
            stored: Mutex::new(stored),
            running: Mutex::new(HashMap::new()),
        }
    }

    /// The settings as clients see them: no password hashes.
    pub fn public(&self) -> ServerSettings {
        let mut s = self.stored.lock().clone();
        let strip = |p: &mut Option<String>, set: &mut bool| {
            *set = p.as_deref().is_some_and(|p| !p.is_empty());
            *p = None;
        };
        strip(&mut s.etp.password, &mut s.etp.password_set);
        strip(&mut s.http.password, &mut s.http.password_set);
        for u in &mut s.index.users {
            strip(&mut u.password, &mut u.password_set);
        }
        s
    }

    /// The stored settings, with password hashes.
    pub fn stored(&self) -> ServerSettings {
        self.stored.lock().clone()
    }

    fn validate(s: &ServerSettings) -> Result<(), String> {
        for (enabled, port, run, bind) in [
            (s.etp.enabled, s.etp.port, &s.etp.run_as, &s.etp.bind),
            (s.http.enabled, s.http.port, &s.http.run_as, &s.http.bind),
            (
                s.index.enabled,
                s.index.port,
                &s.index.run_as,
                &s.index.bind,
            ),
        ] {
            if !enabled {
                continue;
            }
            if port == 0 {
                return Err("a port must be between 1 and 65535".into());
            }
            run_as(run)?;
            addresses(bind, port)?;
        }
        let enabled: Vec<(&str, u16)> = [
            ("ETP", s.etp.enabled, s.etp.port),
            ("HTTP", s.http.enabled, s.http.port),
            ("index", s.index.enabled, s.index.port),
        ]
        .into_iter()
        .filter(|(_, on, _)| *on)
        .map(|(name, _, port)| (name, port))
        .collect();
        for (i, (a, port)) in enabled.iter().enumerate() {
            if let Some((b, _)) = enabled[i + 1..].iter().find(|(_, p)| p == port) {
                return Err(format!("the {a} and {b} servers cannot share port {port}"));
            }
        }
        let mut names: Vec<&str> = s.index.users.iter().map(|u| u.name.as_str()).collect();
        names.sort_unstable();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return Err("index server user names must be unique".into());
        }
        if s.index.enabled && s.index.users.is_empty() {
            return Err("the index server needs at least one user account".into());
        }
        Ok(())
    }

    /// Replaces the settings (passwords merged), saves them and restarts
    /// the servers whose address changed.
    pub async fn apply(&self, new: ServerSettings, daemon: &Arc<Daemon>) -> Result<(), String> {
        Self::validate(&new)?;
        let merged = {
            let old = self.stored.lock();
            let mut m = new;
            m.etp.password = merge_password(&m.etp.password, &old.etp.password);
            m.http.password = merge_password(&m.http.password, &old.http.password);
            for u in &mut m.index.users {
                let previous = old
                    .index
                    .users
                    .iter()
                    .find(|o| o.name == u.name)
                    .and_then(|o| o.password.clone());
                u.password = merge_password(&u.password, &previous);
            }
            m.etp.password_set = false;
            m.http.password_set = false;
            for u in &mut m.index.users {
                u.password_set = false;
            }
            m
        };
        let text = toml::to_string_pretty(&merged).map_err(|e| e.to_string())?;
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let tmp = self.file.with_extension("tmp");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
            f.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, &self.file).map_err(|e| e.to_string())?;
        *self.stored.lock() = merged;
        self.restart(daemon).await
    }

    /// Starts, stops or moves servers to match the settings; servers
    /// whose address is unchanged keep running with their connections.
    pub async fn restart(&self, daemon: &Arc<Daemon>) -> Result<(), String> {
        let s = self.stored();
        let wanted: [(&'static str, Listen); 3] = [
            ("ETP", (s.etp.enabled, s.etp.bind.clone(), s.etp.port)),
            ("HTTP", (s.http.enabled, s.http.bind.clone(), s.http.port)),
            (
                "Index",
                (s.index.enabled, s.index.bind.clone(), s.index.port),
            ),
        ];
        let mut errors = Vec::new();
        for (name, listen) in wanted {
            let old = {
                let mut running = self.running.lock();
                if running.get(name).is_some_and(|r| r.listen == listen) {
                    continue;
                }
                running.remove(name)
            };
            if let Some(old) = old {
                // Wait for the listeners to close so the port is free.
                for t in old.tasks {
                    t.abort();
                    let _ = t.await;
                }
                tracing::info!("{name} server stopped");
            }
            if !listen.0 {
                continue;
            }
            let listeners = match addresses(&listen.1, listen.2).and_then(|a| bind_all(&a)) {
                Ok(l) => l,
                Err(e) => {
                    errors.push(format!("{name} server: {e}"));
                    continue;
                }
            };
            let mut tasks = Vec::new();
            for l in listeners {
                match TcpListener::from_std(l) {
                    Ok(l) => {
                        tracing::info!("{name} server listening on {:?}", l.local_addr());
                        let d = daemon.clone();
                        tasks.push(match name {
                            "ETP" => tokio::spawn(etp::serve(d, l)),
                            "HTTP" => tokio::spawn(http::serve(d, l)),
                            _ => tokio::spawn(index::serve(d, l)),
                        });
                    }
                    Err(e) => errors.push(format!("{name} server: {e}")),
                }
            }
            self.running.lock().insert(name, Running { listen, tasks });
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Stops every server and ends their connections.
    pub fn stop(&self) {
        for (_, r) in self.running.lock().drain() {
            for t in r.tasks {
                t.abort();
            }
        }
    }
}

/// Is `path` inside one of `folders` (or are there no folders)?
pub fn included(path: &[u8], folders: &[String]) -> bool {
    folders.iter().all(|f| f.trim().is_empty())
        || folders.iter().filter(|f| !f.trim().is_empty()).any(|f| {
            let f = f.trim_end_matches('/').as_bytes();
            f.is_empty() || path == f || (path.starts_with(f) && path.get(f.len()) == Some(&b'/'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listen_addresses() {
        let all = addresses(&[], 2121).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].ip().is_unspecified());
        let some = addresses(&["127.0.0.1".into(), "::1".into()], 80).unwrap();
        assert_eq!(some.len(), 2);
        assert!(addresses(&["not an ip".into()], 80).is_err());
    }

    #[test]
    fn include_only() {
        let f = vec!["/home/me".to_string()];
        assert!(included(b"/home/me", &f));
        assert!(included(b"/home/me/a.txt", &f));
        assert!(!included(b"/home/meow", &f));
        assert!(!included(b"/etc", &f));
        assert!(included(b"/etc", &[]));
        assert!(included(b"/etc", &["/".to_string()]));
    }

    #[test]
    fn passwords_are_merged_and_hashed() {
        let old = Some(crate::accounts::hash_password("old"));
        assert_eq!(merge_password(&None, &old), old, "kept");
        assert_eq!(
            merge_password(&Some(String::new()), &old),
            Some(String::new())
        );
        let new = merge_password(&Some("new".into()), &old).unwrap();
        assert!(crate::accounts::verify_password(&new, "new"));
    }
}
