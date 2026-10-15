//! Blocking client for the daemon socket.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::{
    decode_body, encode_frame, DaemonStatus, Envelope, ErrorCode, Event, MountInfo, Request,
    Response, Row, RunChange, RunInfo, SearchOptions, Sort, VolumeStatistics, MAX_FRAME,
    PROTOCOL_VERSION,
};

/// The outcome of [`Client::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// Id of the result set kept on the server.
    pub id: u32,
    pub total: u64,
    pub elapsed_us: u64,
    /// Literal terms to highlight in names and paths.
    pub highlight: Vec<String>,
}

/// Error talking to the daemon.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot reach the EverythingOS daemon: {0}")]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("{message}")]
    Server { code: ErrorCode, message: String },
}

/// A connection to `evosd`.
pub struct Client {
    stream: UnixStream,
    next_id: u32,
    events: VecDeque<Event>,
    /// Version string reported by the daemon.
    pub server: String,
}

impl Client {
    /// Connects and performs the handshake.
    pub fn connect(socket: impl AsRef<Path>, client_name: &str) -> Result<Client, ClientError> {
        let stream = UnixStream::connect(socket)?;
        let mut client = Client {
            stream,
            next_id: 1,
            events: VecDeque::new(),
            server: String::new(),
        };
        match client.call(Request::Hello {
            client: client_name.into(),
            protocol: PROTOCOL_VERSION,
        })? {
            Response::Hello { server, protocol } if protocol == PROTOCOL_VERSION => {
                client.server = server;
                Ok(client)
            }
            Response::Hello { protocol, .. } => Err(ClientError::Protocol(format!(
                "daemon speaks protocol {protocol}, this client {PROTOCOL_VERSION}"
            ))),
            other => Err(unexpected(other)),
        }
    }

    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    /// Sends a request and waits for its response. Events that arrive in
    /// the meantime are queued for [`Client::next_event`].
    pub fn call(&mut self, request: Request) -> Result<Response, ClientError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let frame = encode_frame(&Envelope { id, body: request })
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        self.stream.write_all(&frame)?;
        loop {
            let msg = self.read()?;
            if msg.id == id {
                return match msg.body {
                    Response::Error { code, message } => Err(ClientError::Server { code, message }),
                    body => Ok(body),
                };
            }
            match msg.body {
                Response::Event(e) => self.events.push_back(e),
                // The server could not even decode our request, so it
                // cannot answer with our id; fail instead of waiting.
                Response::Error { code, message } if msg.id == 0 => {
                    return Err(ClientError::Server { code, message })
                }
                _ => {}
            }
        }
    }

    /// Waits for the next pushed event (after [`Client::subscribe`]).
    pub fn next_event(&mut self) -> Result<Event, ClientError> {
        if let Some(e) = self.events.pop_front() {
            return Ok(e);
        }
        loop {
            if let Response::Event(e) = self.read()?.body {
                return Ok(e);
            }
        }
    }

    fn read(&mut self) -> Result<Envelope<Response>, ClientError> {
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if len > MAX_FRAME {
            return Err(ClientError::Protocol(format!(
                "frame of {len} bytes is too large"
            )));
        }
        let mut body = vec![0u8; len];
        self.stream.read_exact(&mut body)?;
        decode_body(&body).map_err(|e| ClientError::Protocol(e.to_string()))
    }

    /// Runs a search; the results stay on the server for [`Client::fetch`].
    pub fn search(
        &mut self,
        query: &str,
        options: SearchOptions,
        sort: Sort,
    ) -> Result<SearchResult, ClientError> {
        match self.call(Request::Search {
            query: query.into(),
            options,
            sort,
        })? {
            Response::SearchDone {
                result,
                total,
                elapsed_us,
                highlight,
            } => Ok(SearchResult {
                id: result,
                total,
                elapsed_us,
                highlight,
            }),
            other => Err(unexpected(other)),
        }
    }

    pub fn fetch(&mut self, result: u32, offset: u32, count: u32) -> Result<Vec<Row>, ClientError> {
        match self.call(Request::Fetch {
            result,
            offset,
            count,
        })? {
            Response::Rows { rows, .. } => Ok(rows),
            other => Err(unexpected(other)),
        }
    }

    pub fn sort(&mut self, result: u32, sort: Sort) -> Result<(), ClientError> {
        self.expect_ok(Request::Sort { result, sort })
    }

    pub fn release(&mut self, result: u32) -> Result<(), ClientError> {
        self.expect_ok(Request::Release { result })
    }

    pub fn status(&mut self) -> Result<DaemonStatus, ClientError> {
        match self.call(Request::Status)? {
            Response::Status(s) => Ok(s),
            other => Err(unexpected(other)),
        }
    }

    pub fn mounts(&mut self) -> Result<Vec<MountInfo>, ClientError> {
        match self.call(Request::Mounts)? {
            Response::Mounts(m) => Ok(m),
            other => Err(unexpected(other)),
        }
    }

    pub fn subscribe(&mut self) -> Result<(), ClientError> {
        self.expect_ok(Request::Subscribe)
    }

    /// Reads or changes the run history of a path.
    pub fn run_count(&mut self, path: &[u8], change: RunChange) -> Result<RunInfo, ClientError> {
        match self.call(Request::RunCount {
            path: path.to_vec(),
            change,
        })? {
            Response::RunCount(info) => Ok(info),
            other => Err(unexpected(other)),
        }
    }

    pub fn statistics(&mut self) -> Result<Vec<VolumeStatistics>, ClientError> {
        match self.call(Request::Statistics)? {
            Response::Statistics(s) => Ok(s),
            other => Err(unexpected(other)),
        }
    }

    /// Up to `limit` bytes of an indexed file (see [`Request::ReadFile`]).
    pub fn read_file(&mut self, path: &[u8], limit: u32) -> Result<Vec<u8>, ClientError> {
        match self.call(Request::ReadFile {
            path: path.to_vec(),
            limit,
        })? {
            Response::FileContent(data) => Ok(data),
            other => Err(unexpected(other)),
        }
    }

    /// Sends a request whose only success response is `Ok`.
    pub fn expect_ok(&mut self, request: Request) -> Result<(), ClientError> {
        match self.call(request)? {
            Response::Ok => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}

fn unexpected(r: Response) -> ClientError {
    ClientError::Protocol(format!("unexpected response {r:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    /// A fake daemon answering one connection from a script.
    fn serve(handler: impl Fn(Request) -> Vec<Response> + Send + 'static) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(dir.path().join("s")).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            loop {
                let mut len = [0u8; 4];
                if s.read_exact(&mut len).is_err() {
                    return;
                }
                let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
                s.read_exact(&mut body).unwrap();
                let req: Envelope<Request> = decode_body(&body).unwrap();
                for resp in handler(req.body) {
                    let connection_level = matches!(&resp, Response::Error { message, .. } if message == "undecodable");
                    let id = if matches!(resp, Response::Event(_)) || connection_level {
                        0
                    } else {
                        req.id
                    };
                    s.write_all(&encode_frame(&Envelope { id, body: resp }).unwrap())
                        .unwrap();
                }
            }
        });
        dir
    }

    #[test]
    fn handshake_search_and_events() {
        let dir = serve(|req| match req {
            Request::Hello { .. } => {
                vec![Response::Hello {
                    server: "test".into(),
                    protocol: PROTOCOL_VERSION,
                }]
            }
            Request::Search { .. } => vec![
                Response::Event(Event::IndexChanged),
                Response::SearchDone {
                    result: 3,
                    total: 42,
                    elapsed_us: 1,
                    highlight: vec!["x".into()],
                },
            ],
            Request::Fetch { .. } => {
                vec![Response::Error {
                    code: ErrorCode::UnknownResult,
                    message: "gone".into(),
                }]
            }
            _ => vec![Response::Ok],
        });
        let mut c = Client::connect(dir.path().join("s"), "test").unwrap();
        assert_eq!(c.server, "test");
        assert_eq!(
            c.search("x", SearchOptions::default(), Sort::default())
                .unwrap(),
            SearchResult {
                id: 3,
                total: 42,
                elapsed_us: 1,
                highlight: vec!["x".into()],
            }
        );
        assert_eq!(
            c.next_event().unwrap(),
            Event::IndexChanged,
            "queued while waiting"
        );
        let err = c.fetch(3, 0, 10).unwrap_err();
        assert!(matches!(
            err,
            ClientError::Server {
                code: ErrorCode::UnknownResult,
                ..
            }
        ));
        c.release(3).unwrap();
    }

    #[test]
    fn connection_level_errors_fail_the_call() {
        let dir = serve(|req| match req {
            Request::Hello { .. } => {
                vec![Response::Hello {
                    server: "t".into(),
                    protocol: PROTOCOL_VERSION,
                }]
            }
            _ => vec![Response::Error {
                code: ErrorCode::BadRequest,
                message: "undecodable".into(),
            }],
        });
        let mut c = Client::connect(dir.path().join("s"), "t").unwrap();
        let err = c.status().unwrap_err();
        assert!(matches!(
            err,
            ClientError::Server {
                code: ErrorCode::BadRequest,
                ..
            }
        ));
    }

    #[test]
    fn rejects_protocol_mismatch() {
        let dir = serve(|_| {
            vec![Response::Hello {
                server: "old".into(),
                protocol: 999,
            }]
        });
        assert!(matches!(
            Client::connect(dir.path().join("s"), "t"),
            Err(ClientError::Protocol(_))
        ));
    }
}
