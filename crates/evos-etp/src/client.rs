//! A blocking ETP client: searches an Everything (or EverythingOS) ETP
//! server and reads files from it.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::{parse_reply_lines, Columns, Reply};

#[derive(Debug, thiserror::Error)]
pub enum EtpError {
    #[error("cannot reach the ETP server: {0}")]
    Io(#[from] io::Error),
    #[error("login failed: {0}")]
    Login(String),
    #[error("the server does not support Everything searches (plain FTP only)")]
    NotEtp,
    #[error("server: {0}")]
    Server(String),
}

type Result<T> = std::result::Result<T, EtpError>;

/// Search settings sent before a query; only changed ones are sent again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub search: String,
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    pub diacritics: bool,
    pub prefix: bool,
    pub suffix: bool,
    pub ignore_punctuation: bool,
    pub ignore_whitespace: bool,
    pub regex: bool,
    /// An ETP sort name such as `NAME_ASCENDING`.
    pub sort: String,
    pub offset: u64,
    pub count: u64,
    pub columns: Columns,
}

impl Default for Query {
    fn default() -> Self {
        Query {
            search: String::new(),
            match_case: false,
            whole_word: false,
            match_path: false,
            diacritics: false,
            prefix: false,
            suffix: false,
            ignore_punctuation: false,
            ignore_whitespace: false,
            regex: false,
            sort: "NAME_ASCENDING".into(),
            offset: 0,
            count: 100,
            columns: Columns::all(),
        }
    }
}

impl Query {
    fn settings(&self) -> Vec<(&'static str, String)> {
        let b = |v: bool| if v { "1" } else { "0" }.to_owned();
        let c = &self.columns;
        vec![
            ("CASE", b(self.match_case)),
            ("WHOLE_WORD", b(self.whole_word)),
            ("PATH", b(self.match_path)),
            ("DIACRITICS", b(self.diacritics)),
            ("PREFIX", b(self.prefix)),
            ("SUFFIX", b(self.suffix)),
            ("IGNORE_PUNCTUATION", b(self.ignore_punctuation)),
            ("IGNORE_WHITESPACE", b(self.ignore_whitespace)),
            ("REGEX", b(self.regex)),
            ("SORT", self.sort.clone()),
            ("OFFSET", self.offset.to_string()),
            ("COUNT", self.count.to_string()),
            ("PATH_COLUMN", b(c.path)),
            ("ATTRIBUTES_COLUMN", b(c.attributes)),
            ("SIZE_COLUMN", b(c.size)),
            ("DATE_MODIFIED_COLUMN", b(c.date_modified)),
            ("DATE_CREATED_COLUMN", b(c.date_created)),
            ("DATE_RECENTLY_CHANGED_COLUMN", b(c.date_recently_changed)),
            ("SEARCH", self.search.clone()),
        ]
    }
}

/// A logged-in connection to an ETP server.
pub struct EtpClient {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    peer: IpAddr,
    /// Options as last sent, so unchanged ones are not repeated.
    sent: HashMap<&'static str, String>,
    /// The server's welcome message.
    pub welcome: String,
}

/// A reply: code and its lines (the last one carries the code too).
struct Response {
    code: u16,
    lines: Vec<String>,
}

impl Response {
    fn text(&self) -> String {
        self.lines
            .last()
            .map(|l| l.get(4..).unwrap_or("").to_owned())
            .unwrap_or_default()
    }
}

impl EtpClient {
    /// Connects and logs in. An empty user logs in as `anonymous`.
    pub fn connect(
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<EtpClient> {
        let addr = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no address for host"))?;
        let stream = TcpStream::connect_timeout(&addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        let mut client = EtpClient {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
            peer: addr.ip(),
            sent: HashMap::new(),
            welcome: String::new(),
        };
        let banner = client.read_response()?;
        if banner.code != 220 {
            return Err(EtpError::Server(banner.text()));
        }
        client.welcome = banner
            .lines
            .iter()
            .map(|l| l.get(4..).unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let user = if user.is_empty() { "anonymous" } else { user };
        let r = client.command(&format!("USER {user}"))?;
        let r = match r.code {
            230 => r,
            331 => client.command(&format!("PASS {password}"))?,
            _ => return Err(EtpError::Login(r.text())),
        };
        if r.code != 230 {
            return Err(EtpError::Login(r.text()));
        }
        client.command("OPTS UTF8 ON")?;
        let feat = client.command("FEAT")?;
        if !feat
            .lines
            .iter()
            .any(|l| l.trim().eq_ignore_ascii_case("EVERYTHING"))
        {
            return Err(EtpError::NotEtp);
        }
        Ok(client)
    }

    fn send(&mut self, line: &str) -> Result<()> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\r\n")?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String> {
        let mut buf = Vec::new();
        if self.reader.read_until(b'\n', &mut buf)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed").into());
        }
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Reads a (possibly multi-line) reply.
    fn read_response(&mut self) -> Result<Response> {
        let first = self.read_line()?;
        let code: u16 = first
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| EtpError::Server(format!("bad reply {first:?}")))?;
        let mut lines = vec![first.clone()];
        if first.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            loop {
                let line = self.read_line()?;
                let done = line.starts_with(&end) || line == end.trim_end();
                lines.push(line);
                if done {
                    break;
                }
            }
        }
        Ok(Response { code, lines })
    }

    fn command(&mut self, line: &str) -> Result<Response> {
        self.send(line)?;
        self.read_response()
    }

    /// Sends one `EVERYTHING` option, expecting `200`.
    fn set(&mut self, key: &'static str, value: &str) -> Result<()> {
        if self.sent.get(key).map(String::as_str) == Some(value) {
            return Ok(());
        }
        let r = self.command(&format!("EVERYTHING {key} {value}"))?;
        if r.code != 200 {
            // Older servers lack some options (prefix, suffix, ...); a
            // query still works without them.
            if key == "SEARCH" || key == "SORT" {
                return Err(EtpError::Server(r.text()));
            }
        }
        self.sent.insert(key, value.to_owned());
        Ok(())
    }

    /// Runs a query: the total number of results and one window of them.
    pub fn query(&mut self, query: &Query) -> Result<Reply> {
        for (key, value) in query.settings() {
            self.set(key, &value)?;
        }
        self.send("EVERYTHING QUERY")?;
        let r = self.read_response()?;
        if r.code != 200 {
            return Err(EtpError::Server(r.text()));
        }
        let body = r
            .lines
            .iter()
            .skip(1)
            .take(r.lines.len().saturating_sub(2))
            .map(String::as_str);
        Ok(parse_reply_lines(body))
    }

    /// Opens a passive data connection.
    fn data_connection(&mut self) -> Result<TcpStream> {
        let r = self.command("EPSV")?;
        let addr = if r.code == 229 {
            // 229 Entering Extended Passive Mode (|||port|)
            let text = r.text();
            let port = text
                .split('|')
                .filter_map(|p| p.parse::<u16>().ok())
                .next_back()
                .ok_or_else(|| EtpError::Server(format!("bad EPSV reply {text:?}")))?;
            SocketAddr::new(self.peer, port)
        } else {
            let r = self.command("PASV")?;
            let text = r.text();
            let nums: Vec<u16> = text
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|p| p.parse().ok())
                .collect();
            if r.code != 227 || nums.len() < 6 {
                return Err(EtpError::Server(format!("bad PASV reply {text:?}")));
            }
            let n = &nums[nums.len() - 6..];
            // Use the control connection's address (servers behind NAT
            // often report a private one).
            SocketAddr::new(self.peer, n[4] * 256 + n[5])
        };
        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(15))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        Ok(stream)
    }

    /// Reads up to `limit` bytes of a file from `offset` (FTP path, see
    /// [`crate::ftp_path`]).
    pub fn read_file(&mut self, path: &str, offset: u64, limit: u64) -> Result<Vec<u8>> {
        self.command("TYPE I")?;
        let data = self.data_connection()?;
        if offset > 0 {
            let r = self.command(&format!("REST {offset}"))?;
            if r.code != 350 {
                return Err(EtpError::Server(r.text()));
            }
        }
        let r = self.command(&format!("RETR {path}"))?;
        if r.code != 150 && r.code != 125 {
            return Err(EtpError::Server(r.text()));
        }
        let mut out = Vec::new();
        data.take(limit).read_to_end(&mut out)?;
        // Stopping early aborts the transfer (426); finishing completes it
        // (226). Either way one reply follows.
        let _ = self.read_response();
        Ok(out)
    }

    pub fn quit(mut self) {
        let _ = self.command("QUIT");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A fake Everything ETP server answering with voidtools' exact
    /// replies.
    fn fake_server(password: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            w.write_all(b"220-Welcome to Everything ETP/FTP\r\n220 Everything 1.5\r\n")
                .unwrap();
            let mut line = String::new();
            let mut search = String::new();
            while r.read_line(&mut line).unwrap_or(0) > 0 {
                let cmd = line.trim_end().to_owned();
                line.clear();
                let reply = if cmd.starts_with("USER") {
                    "331 Password required for x\r\n".to_owned()
                } else if let Some(p) = cmd.strip_prefix("PASS ") {
                    if p == password {
                        "230 Logged on.\r\n".to_owned()
                    } else {
                        "530 Login or password incorrect!\r\n".to_owned()
                    }
                } else if cmd == "FEAT" {
                    "211-Features:\r\n MDTM\r\n SIZE\r\n UTF8\r\n EVERYTHING\r\n211 End\r\n"
                        .to_owned()
                } else if let Some(s) = cmd.strip_prefix("EVERYTHING SEARCH ") {
                    search = s.to_owned();
                    format!("200 Search set to ({s}).\r\n")
                } else if cmd == "EVERYTHING QUERY" {
                    format!(
                        "200-Query results\r\n RESULT_COUNT 1\r\n PATH C:\\data\r\n SIZE 5\r\n FILE {search}.txt\r\n200 End.\r\n"
                    )
                } else if cmd.starts_with("EVERYTHING ") || cmd.starts_with("OPTS") {
                    "200 OK.\r\n".to_owned()
                } else {
                    "500 Syntax error, command unrecognized.\r\n".to_owned()
                };
                w.write_all(reply.as_bytes()).unwrap();
            }
        });
        port
    }

    #[test]
    fn logs_in_and_queries() {
        let port = fake_server("secret");
        let mut c =
            EtpClient::connect("127.0.0.1", port, "me", "secret", Duration::from_secs(5)).unwrap();
        assert!(c.welcome.contains("Welcome to Everything ETP/FTP"));
        let reply = c
            .query(&Query {
                search: "שלום".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(reply.total, 1);
        assert_eq!(reply.items[0].name, "שלום.txt");
        assert_eq!(reply.items[0].path.as_deref(), Some("C:\\data"));
        assert_eq!(reply.items[0].size, Some(5));
    }

    #[test]
    fn wrong_password() {
        let port = fake_server("secret");
        let err = EtpClient::connect("127.0.0.1", port, "me", "nope", Duration::from_secs(5))
            .err()
            .unwrap();
        assert!(matches!(err, EtpError::Login(_)), "{err}");
    }
}
