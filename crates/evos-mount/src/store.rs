//! On-disk mount definitions and credentials.
//!
//! ```text
//! <config>/mounts.d/<name>.toml     mount definition (world-readable)
//! <config>/credentials/<name>       SMB login, root-only (0600 in a 0700 dir)
//! ```
//!
//! Credentials use the cifs `credentials=` file format, so the mount helper
//! reads them directly and passwords never appear on a command line.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use evos_proto::{Credentials, MountSpec};

/// Mount definitions and credentials under a configuration directory.
#[derive(Debug, Clone)]
pub struct MountStore {
    root: PathBuf,
}

impl MountStore {
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        MountStore {
            root: config_dir.into(),
        }
    }

    fn mounts_dir(&self) -> PathBuf {
        self.root.join("mounts.d")
    }

    fn credentials_dir(&self) -> PathBuf {
        self.root.join("credentials")
    }

    fn spec_path(&self, name: &str) -> PathBuf {
        self.mounts_dir().join(format!("{name}.toml"))
    }

    /// Path of the credentials file for a mount (whether or not it exists).
    pub fn credentials_path(&self, name: &str) -> PathBuf {
        self.credentials_dir().join(name)
    }

    pub fn has_credentials(&self, name: &str) -> bool {
        self.credentials_path(name).exists()
    }

    /// Loads every valid definition; invalid files are reported and skipped.
    pub fn load(&self) -> io::Result<Vec<MountSpec>> {
        let dir = self.mounts_dir();
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut specs = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            match fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|text| toml::from_str::<MountSpec>(&text).map_err(|e| e.to_string()))
                .and_then(|spec| spec.validate().map(|()| spec))
            {
                Ok(spec) => specs.push(spec),
                Err(e) => tracing::warn!("ignoring mount definition {}: {e}", path.display()),
            }
        }
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(specs)
    }

    /// Writes a definition atomically.
    pub fn save(&self, spec: &MountSpec) -> io::Result<()> {
        let dir = self.mounts_dir();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&dir)?;
        let text = toml::to_string_pretty(spec)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_atomic(&self.spec_path(&spec.name), text.as_bytes(), 0o644)
    }

    /// Stores credentials readable by root only.
    pub fn save_credentials(&self, name: &str, creds: &Credentials) -> io::Result<()> {
        let invalid = |s: &str| s.contains(['\n', '\r', '\0']);
        if invalid(&creds.username)
            || invalid(&creds.password)
            || creds.domain.as_deref().is_some_and(invalid)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "credentials contain line breaks",
            ));
        }
        let dir = self.credentials_dir();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let mut text = format!("username={}\npassword={}\n", creds.username, creds.password);
        if let Some(domain) = &creds.domain {
            text.push_str(&format!("domain={domain}\n"));
        }
        write_atomic(&self.credentials_path(name), text.as_bytes(), 0o600)
    }

    /// Writes credentials to a private temporary file (for test mounts).
    pub fn temp_credentials(&self, creds: &Credentials) -> io::Result<TempCredentials> {
        let name = format!(".test-{}-{}", std::process::id(), rand_suffix());
        self.save_credentials(&name, creds)?;
        Ok(TempCredentials {
            path: self.credentials_path(&name),
        })
    }

    /// Removes a definition and its credentials.
    pub fn remove(&self, name: &str) -> io::Result<()> {
        for path in [self.spec_path(name), self.credentials_path(name)] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// A credentials file deleted on drop.
pub struct TempCredentials {
    pub path: PathBuf,
}

impl Drop for TempCredentials {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(std::time::SystemTime::now())
}

fn write_atomic(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.set_permissions(fs::Permissions::from_mode(mode))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evos_proto::{IndexSettings, MountKind};

    fn spec(name: &str) -> MountSpec {
        MountSpec {
            name: name.into(),
            kind: MountKind::Smb,
            source: "//nas/share".into(),
            mountpoint: None,
            options: vec!["vers=3.0".into()],
            auto_mount: true,
            index: IndexSettings {
                max_ops_per_sec: Some(500.0),
                ..Default::default()
            },
        }
    }

    #[test]
    fn specs_roundtrip_through_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MountStore::new(tmp.path());
        assert!(store.load().unwrap().is_empty());
        store.save(&spec("b")).unwrap();
        store.save(&spec("a")).unwrap();
        fs::write(tmp.path().join("mounts.d/broken.toml"), "kind = 3").unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(
            loaded,
            vec![spec("a"), spec("b")],
            "sorted, broken file skipped"
        );
        store.remove("a").unwrap();
        assert_eq!(store.load().unwrap().len(), 1);
    }

    #[test]
    fn minimal_toml_gets_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("mounts.d")).unwrap();
        fs::write(
            tmp.path().join("mounts.d/x.toml"),
            "name = \"x\"\nkind = \"nfs\"\nsource = \"nas:/export\"\n",
        )
        .unwrap();
        let loaded = MountStore::new(tmp.path()).load().unwrap();
        assert!(loaded[0].auto_mount && loaded[0].index.enabled && loaded[0].index.smb_notify);
    }

    #[test]
    fn credentials_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MountStore::new(tmp.path());
        let creds = Credentials {
            username: "u".into(),
            password: "p w".into(),
            domain: Some("D".into()),
        };
        store.save_credentials("x", &creds).unwrap();
        let path = store.credentials_path("x");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "username=u\npassword=p w\ndomain=D\n"
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        let bad = Credentials {
            username: "u\nx".into(),
            password: "p".into(),
            domain: None,
        };
        assert!(store.save_credentials("y", &bad).is_err());

        let temp_path = {
            let t = store.temp_credentials(&creds).unwrap();
            assert!(t.path.exists());
            t.path.clone()
        };
        assert!(!temp_path.exists(), "temporary credentials are deleted");
    }
}
