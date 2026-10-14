//! Talking to the daemon without blocking the GTK main loop.
//!
//! A worker thread owns the request connection and runs jobs in order; each
//! job's result is delivered back on the main loop. A second connection
//! subscribes to daemon events. Both reconnect on their own, so the window
//! keeps working across daemon restarts.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use evos_proto::{Client, ClientError};
use gtk::glib;

type Job = Box<dyn FnOnce(&mut Option<Client>) + Send>;

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
}

const CLIENT_NAME: &str = "evos-gtk";

impl Backend {
    /// Starts the worker and the event listener.
    pub fn spawn(socket: PathBuf) -> (Backend, async_channel::Receiver<Notice>) {
        let (jobs, rx) = mpsc::channel::<Job>();
        let worker_socket = socket.clone();
        std::thread::Builder::new()
            .name("evos-gtk-backend".into())
            .spawn(move || {
                let mut client: Option<Client> = None;
                for job in rx {
                    if client.is_none() {
                        client = Client::connect(&worker_socket, CLIENT_NAME).ok();
                    }
                    job(&mut client);
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
            },
            notices,
        )
    }

    pub fn socket(&self) -> &PathBuf {
        &self.socket
    }

    /// Runs `f` on the worker with a connected client and hands its result
    /// to `done` on the main loop.
    pub fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<T, ClientError> + Send + 'static,
        done: impl FnOnce(Result<T, String>) + 'static,
    ) {
        let (tx, rx) = async_channel::bounded(1);
        let job: Job = Box::new(move |client| {
            let result = match client.as_mut() {
                Some(c) => f(c).map_err(|e| {
                    if matches!(e, ClientError::Io(_)) {
                        // Reconnect before the next job.
                        *client = None;
                    }
                    e.to_string()
                }),
                None => Err("cannot reach the EverythingOS daemon (is evosd running?)".into()),
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
}
