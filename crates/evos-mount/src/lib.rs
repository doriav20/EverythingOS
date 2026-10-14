//! Mount management for EverythingOS: mount definitions, credential storage
//! and the mount/unmount/reconnect logic run by the daemon.

pub mod command;
pub mod store;

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use evos_proto::{Credentials, MountInfo, MountKind, MountSpec};
use parking_lot::Mutex;

pub use store::MountStore;

/// Error from a mount operation.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("invalid mount: {0}")]
    Invalid(String),
    #[error("no mount named {0:?}")]
    NotFound(String),
    #[error("{0}")]
    Failed(String),
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
}

const MIN_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug)]
struct State {
    spec: MountSpec,
    error: Option<String>,
    retry_at: Instant,
    backoff: Duration,
    /// Unmounted on request; do not remount until brought up again.
    held_down: bool,
}

impl State {
    fn new(spec: MountSpec) -> Self {
        State {
            spec,
            error: None,
            retry_at: Instant::now(),
            backoff: MIN_BACKOFF,
            held_down: false,
        }
    }
}

/// Mounts defined under a configuration directory, kept mounted.
pub struct MountManager {
    store: MountStore,
    run_dir: PathBuf,
    timeout: Duration,
    states: Mutex<BTreeMap<String, State>>,
}

/// Is something mounted at `mountpoint`?
pub fn is_mounted(mountpoint: &str) -> bool {
    evos_scan::mountinfo::read_mounts()
        .map(|mounts| {
            mounts
                .iter()
                .any(|m| m.mount_point == mountpoint.as_bytes())
        })
        .unwrap_or(false)
}

fn present(spec: &MountSpec) -> bool {
    match spec.kind {
        MountKind::Existing => Path::new(&spec.source).is_dir(),
        _ => is_mounted(&spec.mountpoint()),
    }
}

impl MountManager {
    /// Loads the definitions under `config_dir`. `run_dir` holds temporary
    /// mount points for connection tests.
    pub fn open(config_dir: impl Into<PathBuf>, run_dir: impl Into<PathBuf>) -> io::Result<Self> {
        let store = MountStore::new(config_dir);
        let states = store
            .load()?
            .into_iter()
            .map(|s| (s.name.clone(), State::new(s)))
            .collect();
        Ok(MountManager {
            store,
            run_dir: run_dir.into(),
            timeout: Duration::from_secs(60),
            states: Mutex::new(states),
        })
    }

    pub fn specs(&self) -> Vec<MountSpec> {
        self.states
            .lock()
            .values()
            .map(|s| s.spec.clone())
            .collect()
    }

    /// The managed mount whose mount point (or indexed path) is `path`.
    pub fn spec_for(&self, path: &[u8]) -> Option<MountSpec> {
        self.states
            .lock()
            .values()
            .find(|s| {
                let indexed = match s.spec.kind {
                    MountKind::Existing => s.spec.source.clone(),
                    _ => s.spec.mountpoint(),
                };
                indexed.as_bytes() == path
            })
            .map(|s| s.spec.clone())
    }

    pub fn list(&self) -> Vec<MountInfo> {
        self.states
            .lock()
            .values()
            .map(|s| MountInfo {
                spec: s.spec.clone(),
                mounted: present(&s.spec),
                has_credentials: self.store.has_credentials(&s.spec.name),
                error: s.error.clone(),
            })
            .collect()
    }

    /// Adds or replaces a mount. Credentials of `None` keep stored ones.
    pub fn add(&self, spec: MountSpec, credentials: Option<Credentials>) -> Result<(), MountError> {
        spec.validate().map_err(MountError::Invalid)?;
        if let Some(old) = self.states.lock().get(&spec.name).map(|s| s.spec.clone()) {
            if old.kind != MountKind::Existing && is_mounted(&old.mountpoint()) {
                command::unmount(&old.mountpoint(), self.timeout).map_err(MountError::Failed)?;
            }
        }
        if let Some(creds) = &credentials {
            self.store.save_credentials(&spec.name, creds)?;
        }
        self.store.save(&spec)?;
        let auto = spec.auto_mount;
        let name = spec.name.clone();
        self.states.lock().insert(name.clone(), State::new(spec));
        if auto {
            self.up(&name)?;
        }
        Ok(())
    }

    /// Unmounts and forgets a mount.
    pub fn remove(&self, name: &str) -> Result<(), MountError> {
        self.down(name)?;
        self.store.remove(name)?;
        self.states.lock().remove(name);
        Ok(())
    }

    /// Mounts now and keeps it mounted.
    pub fn up(&self, name: &str) -> Result<(), MountError> {
        let spec = self.spec(name)?;
        let result = self.mount(&spec, Some(&self.store.credentials_path(name)));
        let mut states = self.states.lock();
        if let Some(state) = states.get_mut(name) {
            state.held_down = false;
            Self::record(state, &result);
        }
        result.map_err(MountError::Failed)
    }

    /// Unmounts and keeps it unmounted until [`MountManager::up`].
    pub fn down(&self, name: &str) -> Result<(), MountError> {
        let spec = self.spec(name)?;
        if let Some(state) = self.states.lock().get_mut(name) {
            state.held_down = true;
        }
        if spec.kind != MountKind::Existing && is_mounted(&spec.mountpoint()) {
            command::unmount(&spec.mountpoint(), self.timeout).map_err(MountError::Failed)?;
        }
        Ok(())
    }

    /// Mounts at a temporary location, lists the root and unmounts again.
    pub fn test(
        &self,
        spec: &MountSpec,
        credentials: Option<&Credentials>,
    ) -> Result<(), MountError> {
        spec.validate().map_err(MountError::Invalid)?;
        if spec.kind == MountKind::Existing {
            fs::read_dir(&spec.source)?;
            return Ok(());
        }
        let creds = credentials
            .map(|c| self.store.temp_credentials(c))
            .transpose()?;
        let stored = self.store.credentials_path(&spec.name);
        let creds_path = match &creds {
            Some(t) => Some(t.path.as_path()),
            None => stored.exists().then_some(stored.as_path()),
        };
        let mountpoint = self
            .run_dir
            .join(format!("test-{}-{}", spec.name, std::process::id()));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&mountpoint)?;
        let mp = mountpoint.to_string_lossy().into_owned();
        let cmd = command::mount_command(spec, &mp, creds_path).expect("not Existing");
        let result = command::run(&cmd, self.timeout).and_then(|()| {
            let listed = fs::read_dir(&mountpoint)
                .map(|_| ())
                .map_err(|e| format!("mounted, but cannot list it: {e}"));
            let unmounted = command::unmount(&mp, self.timeout);
            listed.and(unmounted)
        });
        let _ = fs::remove_dir(&mountpoint);
        result.map_err(MountError::Failed)
    }

    /// One supervision pass: (re)mounts auto mounts that are down and due,
    /// with exponential backoff after failures. Returns true if anything
    /// was mounted.
    pub fn supervise(&self) -> bool {
        let now = Instant::now();
        let due: Vec<MountSpec> = self
            .states
            .lock()
            .values()
            .filter(|s| s.spec.auto_mount && !s.held_down && s.retry_at <= now)
            .filter(|s| s.spec.kind != MountKind::Existing && !is_mounted(&s.spec.mountpoint()))
            .map(|s| s.spec.clone())
            .collect();
        let mut mounted = false;
        for spec in due {
            let result = self.mount(&spec, Some(&self.store.credentials_path(&spec.name)));
            if let Err(e) = &result {
                tracing::warn!(mount = %spec.name, "mount failed: {e}");
            } else {
                tracing::info!(mount = %spec.name, "mounted {}", spec.mountpoint());
                mounted = true;
            }
            if let Some(state) = self.states.lock().get_mut(&spec.name) {
                Self::record(state, &result);
            }
        }
        mounted
    }

    fn spec(&self, name: &str) -> Result<MountSpec, MountError> {
        self.states
            .lock()
            .get(name)
            .map(|s| s.spec.clone())
            .ok_or_else(|| MountError::NotFound(name.into()))
    }

    fn record(state: &mut State, result: &Result<(), String>) {
        match result {
            Ok(()) => {
                state.error = None;
                state.backoff = MIN_BACKOFF;
            }
            Err(e) => {
                state.error = Some(e.clone());
                state.retry_at = Instant::now() + state.backoff;
                state.backoff = (state.backoff * 2).min(MAX_BACKOFF);
            }
        }
    }

    fn mount(&self, spec: &MountSpec, credentials: Option<&Path>) -> Result<(), String> {
        if spec.kind == MountKind::Existing {
            return if Path::new(&spec.source).is_dir() {
                Ok(())
            } else {
                Err(format!("{} is not a folder", spec.source))
            };
        }
        let mountpoint = spec.mountpoint();
        if is_mounted(&mountpoint) {
            return Ok(());
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&mountpoint)
            .map_err(|e| format!("cannot create {mountpoint}: {e}"))?;
        let credentials = credentials.filter(|p| p.exists());
        let cmd = command::mount_command(spec, &mountpoint, credentials).expect("not Existing");
        command::run(&cmd, self.timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_proto::IndexSettings;

    fn bind(name: &str, source: &Path, mountpoint: &Path) -> MountSpec {
        MountSpec {
            name: name.into(),
            kind: MountKind::Bind,
            source: source.to_string_lossy().into_owned(),
            mountpoint: Some(mountpoint.to_string_lossy().into_owned()),
            options: vec![],
            auto_mount: true,
            index: IndexSettings::default(),
        }
    }

    /// Bind mounts need CAP_SYS_ADMIN; the dev container has it.
    fn can_mount() -> bool {
        command::run(
            &command::CommandLine {
                program: "mount".into(),
                args: vec!["--version".into()],
            },
            Duration::from_secs(5),
        )
        .is_ok()
            && rustix::process::geteuid().is_root()
    }

    #[test]
    fn bind_mount_lifecycle() {
        if !can_mount() {
            eprintln!("skipping: not root");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (config, src, mp) = (
            tmp.path().join("etc"),
            tmp.path().join("src"),
            tmp.path().join("mp"),
        );
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("hello.txt"), b"hi").unwrap();
        let mgr = MountManager::open(&config, tmp.path().join("run")).unwrap();

        mgr.add(bind("data", &src, &mp), None).unwrap();
        assert!(mp.join("hello.txt").exists(), "auto-mounted on add");
        let info = &mgr.list()[0];
        assert!(info.mounted && info.error.is_none() && !info.has_credentials);
        assert_eq!(
            mgr.spec_for(mp.to_string_lossy().as_bytes()).unwrap().name,
            "data"
        );

        mgr.down("data").unwrap();
        assert!(!mp.join("hello.txt").exists());
        assert!(!mgr.supervise(), "held down");
        mgr.up("data").unwrap();
        assert!(mp.join("hello.txt").exists());

        // Reloading from disk keeps the definition.
        let again = MountManager::open(&config, tmp.path().join("run")).unwrap();
        assert_eq!(again.specs().len(), 1);

        mgr.test(&bind("probe", &src, &mp), None).unwrap();
        mgr.remove("data").unwrap();
        assert!(!mp.join("hello.txt").exists());
        assert!(mgr.list().is_empty());
        assert!(MountManager::open(&config, tmp.path().join("run"))
            .unwrap()
            .specs()
            .is_empty());
    }

    #[test]
    fn failures_back_off() {
        let tmp = tempfile::tempdir().unwrap();
        let mgr = MountManager::open(tmp.path().join("etc"), tmp.path().join("run")).unwrap();
        let missing = bind("gone", &tmp.path().join("missing"), &tmp.path().join("mp"));
        assert!(mgr.add(missing, None).is_err());
        let info = &mgr.list()[0];
        assert!(!info.mounted && info.error.is_some());
        assert!(!mgr.supervise(), "retry is not due yet");
        assert!(matches!(mgr.up("nope"), Err(MountError::NotFound(_))));
        assert!(matches!(
            mgr.add(bind("bad name", tmp.path(), tmp.path()), None),
            Err(MountError::Invalid(_))
        ));
    }
}
