//! Daemon configuration, read from `/etc/evos/evosd.toml`.
//!
//! Every field has a default, so an empty or missing file is valid.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use evos_scan::walker::ExcludeFn;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Index snapshots.
    pub state_dir: PathBuf,
    /// Holds `mounts.d/` and `credentials/`.
    pub config_dir: PathBuf,
    /// Runtime files: the socket and temporary test mounts.
    pub run_dir: PathBuf,
    pub socket: PathBuf,
    pub snapshot_interval_secs: u64,
    /// Return every result to every user (skip permission filtering).
    /// Only for single-user machines.
    pub single_user: bool,
    pub local: LocalConfig,
    pub network: NetworkConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    /// Paths (and everything below them) that are never indexed.
    pub exclude: Vec<String>,
    /// Mount points not to index.
    pub skip_mounts: Vec<String>,
    /// Index tmpfs mounts such as /tmp.
    pub index_tmpfs: bool,
    /// Use fanotify when possible (requires CAP_SYS_ADMIN).
    pub fanotify: bool,
    pub safety_sweep_hours: u64,
    pub threads: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// Also index network mounts that EverythingOS did not create.
    pub index_unmanaged: bool,
    pub max_concurrency: usize,
    /// 0 means unlimited.
    pub max_ops_per_sec: f64,
    pub min_recheck_secs: u64,
    pub max_staleness_hours: u64,
    pub refresh_days: u64,
    pub smb_notify: bool,
    pub offline_retry_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            state_dir: "/var/lib/evos".into(),
            config_dir: "/etc/evos".into(),
            run_dir: "/run/evos".into(),
            socket: evos_proto::DEFAULT_SOCKET.into(),
            snapshot_interval_secs: 300,
            single_user: false,
            local: LocalConfig::default(),
            network: NetworkConfig::default(),
        }
    }
}

impl Default for LocalConfig {
    fn default() -> Self {
        LocalConfig {
            exclude: [
                "/proc",
                "/sys",
                "/dev",
                "/run",
                "/var/lib/docker",
                "/var/lib/containers",
            ]
            .map(String::from)
            .to_vec(),
            skip_mounts: Vec::new(),
            index_tmpfs: false,
            fanotify: true,
            safety_sweep_hours: 24,
            threads: None,
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        NetworkConfig {
            index_unmanaged: true,
            max_concurrency: 64,
            max_ops_per_sec: 0.0,
            min_recheck_secs: 30,
            max_staleness_hours: 24,
            refresh_days: 7,
            smb_notify: true,
            offline_retry_secs: 30,
        }
    }
}

impl Config {
    /// Loads the configuration; a missing file means all defaults.
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn snapshot_interval(&self) -> Duration {
        Duration::from_secs(self.snapshot_interval_secs.max(10))
    }
}

/// Builds an exclusion predicate: a path is excluded if it equals one of
/// `prefixes` or lies below one.
pub fn exclude_fn(prefixes: &[String]) -> Option<ExcludeFn> {
    let prefixes: Vec<Vec<u8>> = prefixes
        .iter()
        .map(|p| p.trim_end_matches('/').as_bytes().to_vec())
        .filter(|p| !p.is_empty())
        .collect();
    if prefixes.is_empty() {
        return None;
    }
    Some(Arc::new(move |path: &[u8]| {
        prefixes
            .iter()
            .any(|p| path.starts_with(p) && (path.len() == p.len() || path[p.len()] == b'/'))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.socket, PathBuf::from("/run/evos/evosd.sock"));
        assert!(cfg.local.exclude.contains(&"/proc".to_owned()));
        assert_eq!(cfg.network.max_concurrency, 64);
    }

    #[test]
    fn partial_sections_merge_with_defaults() {
        let cfg: Config = toml::from_str(
            "single_user = true\n[network]\nmax_ops_per_sec = 200\n[local]\nindex_tmpfs = true\n",
        )
        .unwrap();
        assert!(cfg.single_user && cfg.local.index_tmpfs && cfg.local.fanotify);
        assert_eq!(cfg.network.max_ops_per_sec, 200.0);
        assert!(cfg.network.smb_notify);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(toml::from_str::<Config>("sokcet = \"/x\"").is_err());
    }

    #[test]
    fn missing_file_is_default() {
        let cfg = Config::load(Path::new("/nonexistent/evosd.toml")).unwrap();
        assert_eq!(cfg.snapshot_interval_secs, 300);
    }

    #[test]
    fn exclusion_matches_whole_components() {
        let ex = exclude_fn(&["/var/lib/docker".into(), "/proc/".into()]).unwrap();
        assert!(ex(b"/var/lib/docker"));
        assert!(ex(b"/var/lib/docker/overlay2/x"));
        assert!(!ex(b"/var/lib/docker2"));
        assert!(ex(b"/proc/1"));
        assert!(!ex(b"/home"));
        assert!(exclude_fn(&[]).is_none());
    }
}
