//! Settings of the servers the daemon can run, as in Everything's
//! options: the ETP/FTP server, the HTTP server and the index server
//! (EverythingOS's counterpart of the Everything Server), plus remote
//! indexes copied from other computers.
//!
//! Passwords travel only towards the daemon: settings read back carry
//! `password_set` instead, and a `None` password keeps the stored one.

use serde::{Deserialize, Serialize};

/// Everything's ETP server port.
pub const ETP_PORT: u16 = 21;
/// Everything's HTTP server port.
pub const HTTP_PORT: u16 = 80;
/// The index server port (the Everything Server's).
pub const INDEX_PORT: u16 = 14630;

/// Everything's ETP/FTP server: search with Everything ("Connect to ETP
/// Server") and browse or download with any FTP client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EtpServer {
    pub enabled: bool,
    /// Addresses to listen on; empty listens on all.
    pub bind: Vec<String>,
    pub port: u16,
    /// Empty: any user name is accepted.
    pub username: String,
    /// A new password; `None` keeps the stored one (empty: none needed).
    pub password: Option<String>,
    /// Read-only: a password is stored.
    pub password_set: bool,
    pub allow_file_download: bool,
    pub welcome_message: String,
    /// Results and downloads are limited to what this Linux account may
    /// see and read.
    pub run_as: String,
}

impl Default for EtpServer {
    fn default() -> Self {
        EtpServer {
            enabled: false,
            bind: Vec::new(),
            port: ETP_PORT,
            username: String::new(),
            password: None,
            password_set: false,
            allow_file_download: true,
            welcome_message: String::new(),
            run_as: "nobody".into(),
        }
    }
}

/// Everything's HTTP server: search and download from a web browser, and
/// the `?search=...&json=1` interface scripts use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HttpServer {
    pub enabled: bool,
    pub bind: Vec<String>,
    pub port: u16,
    /// Empty: no login.
    pub username: String,
    pub password: Option<String>,
    pub password_set: bool,
    pub allow_file_download: bool,
    pub run_as: String,
}

impl Default for HttpServer {
    fn default() -> Self {
        HttpServer {
            enabled: false,
            bind: Vec::new(),
            port: HTTP_PORT,
            username: String::new(),
            password: None,
            password_set: false,
            allow_file_download: true,
            run_as: "nobody".into(),
        }
    }
}

/// A user of the index server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexUser {
    pub name: String,
    pub password: Option<String>,
    pub password_set: bool,
    /// Folders this user's copy includes; empty includes everything.
    pub include_only: Vec<String>,
}

/// The index server: other EverythingOS computers add this one as a
/// remote index and keep a live copy of its index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexServer {
    pub enabled: bool,
    pub bind: Vec<String>,
    pub port: u16,
    /// Clients log in with one of these accounts.
    pub users: Vec<IndexUser>,
    pub allow_file_download: bool,
    pub run_as: String,
}

impl Default for IndexServer {
    fn default() -> Self {
        IndexServer {
            enabled: false,
            bind: Vec::new(),
            port: INDEX_PORT,
            users: Vec::new(),
            allow_file_download: true,
            run_as: "nobody".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerSettings {
    pub etp: EtpServer,
    pub http: HttpServer,
    pub index: IndexServer,
}

/// What kind of server a remote index comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteKind {
    /// Another EverythingOS computer's index server: a live copy.
    #[default]
    Evos,
    /// An Everything (or EverythingOS) ETP server: copied periodically.
    Etp,
}

impl RemoteKind {
    pub fn default_port(self) -> u16 {
        match self {
            RemoteKind::Evos => INDEX_PORT,
            RemoteKind::Etp => ETP_PORT,
        }
    }
}

/// A remote index: another computer's index, copied and kept current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteIndexSpec {
    /// Letters, digits, `-` and `_`.
    pub name: String,
    #[serde(default)]
    pub kind: RemoteKind,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub username: String,
    /// Where its files appear; defaults to `/net/<name>`.
    #[serde(default)]
    pub mount_as: Option<String>,
    /// Only these server folders; empty takes all the server offers.
    #[serde(default)]
    pub include_only: Vec<String>,
    /// How often to check for changes (EverythingOS) or copy again (ETP),
    /// in seconds.
    #[serde(default)]
    pub refresh_secs: Option<u64>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl Default for RemoteIndexSpec {
    fn default() -> Self {
        RemoteIndexSpec {
            name: String::new(),
            kind: RemoteKind::default(),
            host: String::new(),
            port: None,
            username: String::new(),
            mount_as: None,
            include_only: Vec::new(),
            refresh_secs: None,
            enabled: true,
        }
    }
}

impl RemoteIndexSpec {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(self.kind.default_port())
    }

    pub fn mount_as(&self) -> String {
        self.mount_as
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| format!("/net/{}", self.name))
    }

    pub fn refresh_secs(&self) -> u64 {
        self.refresh_secs.unwrap_or(match self.kind {
            RemoteKind::Evos => 10,
            RemoteKind::Etp => 600,
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        let name_ok = !self.name.is_empty()
            && self.name.len() <= 64
            && self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !name_ok {
            return Err("name must be 1-64 letters, digits, '-' or '_'".into());
        }
        if self.host.trim().is_empty() || self.host.contains(char::is_whitespace) {
            return Err("host must be a host name or address".into());
        }
        if !self.mount_as().starts_with('/') {
            return Err("the mount location must be an absolute path".into());
        }
        Ok(())
    }
}

/// A remote index and how it is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteIndexInfo {
    pub spec: RemoteIndexSpec,
    /// connecting, copying, live, offline or disabled.
    pub phase: String,
    pub entries: u64,
    /// Unix seconds.
    pub last_sync: Option<i64>,
    pub error: Option<String>,
}

/// An ETP server to search directly, from a window or the command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteSearch {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

impl RemoteSearch {
    /// Parses Everything's connection syntax `[user[:password]@]host[:port]`.
    pub fn parse(s: &str) -> Option<RemoteSearch> {
        let (login, host) = match s.rsplit_once('@') {
            Some((l, h)) => (Some(l), h),
            None => (None, s),
        };
        let (username, password) = match login {
            Some(l) => match l.split_once(':') {
                Some((u, p)) => (u.to_owned(), p.to_owned()),
                None => (l.to_owned(), String::new()),
            },
            None => (String::new(), String::new()),
        };
        let (host, port) = match host.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, p.parse().ok()?),
            _ => (host, ETP_PORT),
        };
        let host = host.trim_matches(['[', ']']).to_owned();
        (!host.is_empty()).then_some(RemoteSearch {
            host,
            port,
            username,
            password,
        })
    }
}

/// A volume an index server offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexVolume {
    pub id: u16,
    pub root: Vec<u8>,
    pub fs_type: String,
    pub epoch: u64,
    pub generation: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_strings() {
        let r = RemoteSearch::parse("me:pw@pc.local:2121").unwrap();
        assert_eq!(
            (
                r.username.as_str(),
                r.password.as_str(),
                r.host.as_str(),
                r.port
            ),
            ("me", "pw", "pc.local", 2121)
        );
        let r = RemoteSearch::parse("10.0.0.5").unwrap();
        assert_eq!(
            (r.host.as_str(), r.port, r.username.as_str()),
            ("10.0.0.5", 21, "")
        );
        assert!(RemoteSearch::parse("user@").is_none());
    }

    #[test]
    fn remote_defaults() {
        let spec = RemoteIndexSpec {
            name: "pc".into(),
            kind: RemoteKind::Etp,
            host: "pc.local".into(),
            port: None,
            username: String::new(),
            mount_as: None,
            include_only: Vec::new(),
            refresh_secs: None,
            enabled: true,
        };
        assert_eq!(spec.port(), 21);
        assert_eq!(spec.mount_as(), "/net/pc");
        assert_eq!(spec.refresh_secs(), 600);
        assert!(spec.validate().is_ok());
        let bad = RemoteIndexSpec {
            name: "a b".into(),
            ..spec
        };
        assert!(bad.validate().is_err());
    }
}
