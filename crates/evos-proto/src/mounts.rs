//! Mount definitions, shared by the daemon (which stores them as TOML in
//! `/etc/evos/mounts.d`) and the clients that edit them.

use serde::{Deserialize, Serialize};

/// What kind of filesystem a mount is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountKind {
    /// SMB/CIFS share, `//server/share[/path]`.
    Smb,
    /// NFS export, `server:/export`.
    Nfs,
    /// SFTP over SSH, `[user@]host:/path`.
    Sshfs,
    /// Bind mount of a local folder.
    Bind,
    /// Index a path that is already mounted (or any folder) as-is.
    Existing,
}

impl MountKind {
    pub fn is_network(self) -> bool {
        matches!(self, MountKind::Smb | MountKind::Nfs | MountKind::Sshfs)
    }
}

/// Indexing settings for one mount.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IndexSettings {
    pub enabled: bool,
    /// Upper bound for concurrent requests to the server.
    pub max_concurrency: Option<u32>,
    /// Cap on filesystem operations per second against the server.
    pub max_ops_per_sec: Option<f64>,
    /// Absolute paths (or paths relative to the mount) not to index.
    pub exclude: Vec<String>,
    /// Check interval of folders that just changed.
    pub min_recheck_secs: Option<u64>,
    /// Longest any folder may go unchecked.
    pub max_staleness_secs: Option<u64>,
    /// Use SMB change notifications when available.
    pub smb_notify: bool,
}

impl Default for IndexSettings {
    fn default() -> Self {
        IndexSettings {
            enabled: true,
            max_concurrency: None,
            max_ops_per_sec: None,
            exclude: Vec::new(),
            min_recheck_secs: None,
            max_staleness_secs: None,
            smb_notify: true,
        }
    }
}

/// A mount managed by EverythingOS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountSpec {
    /// Unique name: letters, digits, `-` and `_`.
    pub name: String,
    pub kind: MountKind,
    pub source: String,
    /// Defaults to `/mnt/evos/<name>`.
    #[serde(default)]
    pub mountpoint: Option<String>,
    /// Extra `-o` options for mount.
    #[serde(default)]
    pub options: Vec<String>,
    /// Mount at daemon start and remount after failures.
    #[serde(default = "yes")]
    pub auto_mount: bool,
    #[serde(default)]
    pub index: IndexSettings,
}

fn yes() -> bool {
    true
}

impl MountSpec {
    /// Where the mount appears.
    pub fn mountpoint(&self) -> String {
        self.mountpoint
            .clone()
            .unwrap_or_else(|| format!("/mnt/evos/{}", self.name))
    }

    /// Checks the fields that can be validated without touching the system.
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
        let source_ok = match self.kind {
            MountKind::Smb => self.source.starts_with("//") && self.source[2..].contains('/'),
            MountKind::Nfs => self.source.contains(":/"),
            MountKind::Sshfs => self.source.contains(':'),
            MountKind::Bind | MountKind::Existing => self.source.starts_with('/'),
        };
        if !source_ok {
            return Err(match self.kind {
                MountKind::Smb => "SMB source must look like //server/share",
                MountKind::Nfs => "NFS source must look like server:/export",
                MountKind::Sshfs => "SSHFS source must look like user@host:/path",
                MountKind::Bind | MountKind::Existing => "source must be an absolute path",
            }
            .into());
        }
        if !self.mountpoint().starts_with('/') {
            return Err("mountpoint must be an absolute path".into());
        }
        if let Some(bad) = self
            .options
            .iter()
            .find(|o| o.contains([',', '\n']) || o.is_empty())
        {
            return Err(format!("invalid mount option {bad:?}"));
        }
        Ok(())
    }
}

/// Login for SMB shares. Stored root-only on the server side and never
/// returned to clients.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("domain", &self.domain)
            .finish()
    }
}

/// A mount and its current state, as reported to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MountInfo {
    pub spec: MountSpec,
    pub mounted: bool,
    pub has_credentials: bool,
    /// Last mount error, if any.
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(kind: MountKind, source: &str) -> MountSpec {
        MountSpec {
            name: "nas".into(),
            kind,
            source: source.into(),
            mountpoint: None,
            options: vec![],
            auto_mount: true,
            index: IndexSettings::default(),
        }
    }

    #[test]
    fn validation() {
        assert!(spec(MountKind::Smb, "//nas/share").validate().is_ok());
        assert!(spec(MountKind::Smb, "//nas").validate().is_err());
        assert!(spec(MountKind::Nfs, "nas:/export").validate().is_ok());
        assert!(spec(MountKind::Nfs, "nas").validate().is_err());
        assert!(spec(MountKind::Sshfs, "me@host:/data").validate().is_ok());
        assert!(spec(MountKind::Bind, "relative").validate().is_err());
        let mut s = spec(MountKind::Smb, "//nas/share");
        s.name = "bad name".into();
        assert!(s.validate().is_err());
        let mut s = spec(MountKind::Smb, "//nas/share");
        s.options = vec!["vers=3.0,sec=none".into()];
        assert!(s.validate().is_err(), "options are passed one by one");
        assert_eq!(spec(MountKind::Nfs, "a:/b").mountpoint(), "/mnt/evos/nas");
    }

    #[test]
    fn specs_survive_the_wire_format() {
        // postcard is not self-describing: every field must be serialized.
        let s = spec(MountKind::Nfs, "nas:/export");
        let bytes = postcard::to_allocvec(&s).unwrap();
        assert_eq!(postcard::from_bytes::<MountSpec>(&bytes).unwrap(), s);
    }

    #[test]
    fn credentials_are_redacted_in_debug_output() {
        let c = Credentials {
            username: "me".into(),
            password: "hunter2".into(),
            domain: None,
        };
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
