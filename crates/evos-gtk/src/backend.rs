//! Talking to the daemon without blocking the GTK main loop.
//!
//! A worker thread owns the request connection and runs jobs in order; each
//! job's result is delivered back on the main loop. A second connection
//! subscribes to daemon events. Both reconnect on their own, so the window
//! keeps working across daemon restarts.
//!
//! A window searching an ETP server has its connection pointed at the
//! server after every (re)connect. Files of results on other computers are
//! downloaded on connections of their own, so long downloads never hold up
//! searches.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use evos_proto::{Client, ClientError, RemoteSearch};
use gtk::glib;

type Job = Box<dyn FnOnce(&mut Option<Client>, &str) + Send>;

/// Things the daemon tells the window without being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Connected,
    Disconnected,
    IndexChanged,
    StatusChanged,
}

/// Handle to the worker thread.
#[derive(Clone)]
pub struct Backend {
    jobs: mpsc::Sender<Job>,
    socket: PathBuf,
    search_generation: Arc<AtomicU64>,
    /// The ETP server this window searches, if any.
    remote: Arc<Mutex<Option<RemoteSearch>>>,
}

const CLIENT_NAME: &str = "evos-gtk";
/// Bytes asked for at a time when downloading.
const CHUNK: u32 = 4 << 20;

/// A daemon connection, pointed at `remote` if given.
fn open(socket: &Path, remote: Option<&RemoteSearch>) -> Result<Client, String> {
    let mut c = Client::connect(socket, CLIENT_NAME)
        .map_err(|_| "cannot reach the EverythingOS daemon (is evosd running?)".to_owned())?;
    if let Some(r) = remote {
        c.connect_remote(r.clone())
            .map_err(|e| format!("cannot connect to {}: {e}", r.host))?;
    }
    Ok(c)
}

/// Where `host:port` is written for people, without the password.
pub fn describe(r: &RemoteSearch) -> String {
    let host = if r.host.contains(':') {
        format!("[{}]", r.host)
    } else {
        r.host.clone()
    };
    let port = if r.port == evos_proto::ETP_PORT {
        String::new()
    } else {
        format!(":{}", r.port)
    };
    if r.username.is_empty() {
        format!("{host}{port}")
    } else {
        format!("{}@{host}{port}", r.username)
    }
}

impl Backend {
    /// Starts the worker and the event listener.
    pub fn spawn(socket: PathBuf) -> (Backend, async_channel::Receiver<Notice>) {
        Self::spawn_for(socket, None)
    }

    /// Like [`Backend::spawn`], searching an ETP server.
    pub fn spawn_for(
        socket: PathBuf,
        remote: Option<RemoteSearch>,
    ) -> (Backend, async_channel::Receiver<Notice>) {
        let remote = Arc::new(Mutex::new(remote));
        let (jobs, rx) = mpsc::channel::<Job>();
        let (worker_socket, worker_remote) = (socket.clone(), remote.clone());
        std::thread::Builder::new()
            .name("evos-gtk-backend".into())
            .spawn(move || {
                let mut client: Option<Client> = None;
                let mut why = String::new();
                for job in rx {
                    if client.is_none() {
                        let target = worker_remote.lock().expect("remote").clone();
                        match open(&worker_socket, target.as_ref()) {
                            Ok(c) => client = Some(c),
                            Err(e) => why = e,
                        }
                    }
                    job(&mut client, &why);
                }
            })
            .expect("spawn backend thread");

        let (notices_tx, notices) = async_channel::unbounded();
        let event_socket = socket.clone();
        std::thread::Builder::new()
            .name("evos-gtk-events".into())
            .spawn(move || loop {
                let mut connected = false;
                if let Ok(mut c) = Client::connect(&event_socket, CLIENT_NAME) {
                    if c.subscribe().is_ok() {
                        connected = true;
                        if notices_tx.send_blocking(Notice::Connected).is_err() {
                            return;
                        }
                        while let Ok(event) = c.next_event() {
                            let notice = match event {
                                evos_proto::Event::IndexChanged => Notice::IndexChanged,
                                evos_proto::Event::StatusChanged => Notice::StatusChanged,
                            };
                            if notices_tx.send_blocking(notice).is_err() {
                                return;
                            }
                        }
                    }
                }
                if connected && notices_tx.send_blocking(Notice::Disconnected).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_secs(2));
            })
            .expect("spawn event thread");

        (
            Backend {
                jobs,
                socket,
                search_generation: Arc::new(AtomicU64::new(0)),
                remote,
            },
            notices,
        )
    }

    pub fn socket(&self) -> &PathBuf {
        &self.socket
    }

    /// The ETP server searches go to, if any.
    pub fn remote(&self) -> Option<RemoteSearch> {
        self.remote.lock().expect("remote").clone()
    }

    /// Points searches at an ETP server, or back at this computer.
    pub fn set_remote(
        &self,
        target: Option<RemoteSearch>,
        done: impl FnOnce(Result<String, String>) + 'static,
    ) {
        *self.remote.lock().expect("remote") = target.clone();
        self.call(
            move |c| match target {
                Some(t) => c.connect_remote(t),
                None => c.disconnect_remote().map(|()| String::new()),
            },
            done,
        );
    }

    /// Runs `f` on the worker with a connected client and hands its result
    /// to `done` on the main loop.
    pub fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<T, ClientError> + Send + 'static,
        done: impl FnOnce(Result<T, String>) + 'static,
    ) {
        let (tx, rx) = async_channel::bounded(1);
        let job: Job = Box::new(move |client, why| {
            let result = match client.as_mut() {
                Some(c) => f(c).map_err(|e| {
                    if matches!(e, ClientError::Io(_)) {
                        // Reconnect before the next job.
                        *client = None;
                    }
                    e.to_string()
                }),
                None => Err(why.to_owned()),
            };
            let _ = tx.send_blocking(result);
        });
        if self.jobs.send(job).is_err() {
            return;
        }
        glib::spawn_future_local(async move {
            if let Ok(result) = rx.recv().await {
                done(result);
            }
        });
    }

    /// Starts a new search generation; jobs of older generations that
    /// have not started yet are skipped.
    pub fn next_search(&self) -> u64 {
        self.search_generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Like [`Backend::call`], but skipped if a newer search started.
    pub fn call_search<T: Send + 'static>(
        &self,
        generation: u64,
        f: impl FnOnce(&mut Client) -> Result<T, ClientError> + Send + 'static,
        done: impl FnOnce(Result<T, String>) + 'static,
    ) {
        let current = self.search_generation.clone();
        self.call(
            move |c| {
                if current.load(Ordering::Relaxed) != generation {
                    return Err(ClientError::Protocol("superseded".into()));
                }
                f(c)
            },
            move |r| {
                if !matches!(&r, Err(e) if e.contains("superseded")) {
                    done(r)
                }
            },
        );
    }

    /// Checks that an ETP server accepts this login, on a connection of
    /// its own; gives the server's welcome message.
    pub fn probe(&self, target: RemoteSearch, done: impl FnOnce(Result<String, String>) + 'static) {
        let socket = self.socket.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let result = Client::connect(&socket, CLIENT_NAME)
                .map_err(|_| "cannot reach the EverythingOS daemon (is evosd running?)".to_owned())
                .and_then(|mut c| {
                    let host = target.host.clone();
                    c.connect_remote(target)
                        .map_err(|e| format!("Cannot connect to {host}: {e}"))
                });
            let _ = tx.send_blocking(result);
        });
        glib::spawn_future_local(async move {
            if let Ok(r) = rx.recv().await {
                done(r);
            }
        });
    }

    /// Copies a file of another computer (a remote index's copy, or a
    /// result of the ETP server this window searches) to `dest`, reporting
    /// the bytes received so far. Nothing is written to the other
    /// computer; `dest` appears only once complete.
    pub fn download(
        &self,
        path: Vec<u8>,
        dest: PathBuf,
        progress: impl Fn(u64) + 'static,
        done: impl FnOnce(Result<PathBuf, String>) + 'static,
    ) {
        let socket = self.socket.clone();
        let remote = self.remote();
        let (tx, rx) = async_channel::unbounded::<Result<u64, Result<PathBuf, String>>>();
        std::thread::spawn(move || {
            let result = (|| -> Result<PathBuf, String> {
                let mut c = open(&socket, remote.as_ref())?;
                if let Some(dir) = dest.parent() {
                    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                let part = dest.with_extension("evos-part");
                let mut file =
                    std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
                let mut offset = 0u64;
                loop {
                    let data = c
                        .read_file(&path, offset, CHUNK)
                        .map_err(|e| e.to_string())?;
                    file.write_all(&data).map_err(|e| e.to_string())?;
                    offset += data.len() as u64;
                    let _ = tx.send_blocking(Ok(offset));
                    if data.len() < CHUNK as usize {
                        break;
                    }
                }
                file.sync_all().map_err(|e| e.to_string())?;
                drop(file);
                // A copy, not the original: changes would not reach it.
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o444));
                std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
                Ok(dest)
            })();
            let _ = tx.send_blocking(Err(result));
        });
        glib::spawn_future_local(async move {
            while let Ok(msg) = rx.recv().await {
                match msg {
                    Ok(bytes) => progress(bytes),
                    Err(result) => return done(result),
                }
            }
        });
    }
}

fn remote_cache() -> PathBuf {
    glib::user_cache_dir().join("evos").join("remote")
}

/// Deletes copies of remote files opened more than `age` ago.
pub fn clean_cache(age: Duration) {
    clean_dir(&remote_cache(), age);
}

fn clean_dir(dir: &Path, age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            clean_dir(&path, age);
            // Only empty folders go.
            let _ = std::fs::remove_dir(&path);
        } else if meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|e| e > age)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Where a copy of a remote file is kept for opening: a folder per file
/// path, so the copy keeps its name.
pub fn cache_path(path: &[u8], name: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    let name: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' {
                '_'
            } else {
                c
            }
        })
        .collect();
    remote_cache()
        .join(format!("{:016x}", h.finish()))
        .join(if name.is_empty() { "file".into() } else { name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connections_are_described_without_passwords() {
        let r = RemoteSearch::parse("me:secret@pc:2121").unwrap();
        assert_eq!(describe(&r), "me@pc:2121");
        let r = RemoteSearch::parse("10.0.0.5").unwrap();
        assert_eq!(describe(&r), "10.0.0.5");
    }

    #[test]
    fn old_copies_are_cleaned() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("a/old.txt");
        let new = tmp.path().join("b/new.txt");
        for f in [&old, &new] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, "x").unwrap();
        }
        let week_ago = std::time::SystemTime::now() - Duration::from_secs(8 * 86400);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(week_ago)
            .unwrap();
        clean_dir(tmp.path(), Duration::from_secs(7 * 86400));
        assert!(!old.exists() && !old.parent().unwrap().exists());
        assert!(new.exists());
    }

    #[test]
    fn cached_copies_keep_their_names() {
        let a = cache_path(b"C:\\Users\\me\\report.pdf", "report.pdf");
        let b = cache_path(b"D:\\report.pdf", "report.pdf");
        assert_eq!(a.file_name().unwrap(), "report.pdf");
        assert_ne!(a, b);
        assert_eq!(
            cache_path(b"/x", "a/b").file_name().unwrap(),
            "a_b",
            "never a path"
        );
    }
}
