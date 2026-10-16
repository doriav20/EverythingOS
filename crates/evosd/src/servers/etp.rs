//! Everything's ETP/FTP server: Everything on another computer searches
//! this one ("Connect to ETP Server"), and FTP clients browse and download.
//!
//! Replies follow voidtools' ETP server plugin, so Everything's client
//! works unchanged. Folder listings come from the index; downloads are
//! read-only. Everything a remote user sees is limited to what the
//! "run as" account may see and read.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use evos_core::perm::Caller;
use evos_etp::{format_reply, parse_sort, row_to_item, Columns};
use evos_proto::{EtpServer, SearchOptions, Sort};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

use super::files::{self, Entry};
use crate::accounts::verify_password;
use crate::server::{blocking, Audience, Daemon};

const IDLE: Duration = Duration::from_secs(600);
const MAX_LINE: usize = 16 * 1024;
const MAX_CLIENTS: usize = 64;

/// Accepts ETP/FTP connections forever.
pub async fn serve(daemon: Arc<Daemon>, listener: TcpListener) {
    super::accept_loop(
        listener,
        MAX_CLIENTS,
        b"421 Too many connections.\r\n",
        |stream, peer| {
            let daemon = daemon.clone();
            async move {
                if let Err(e) = Connection::run(daemon, stream, peer).await {
                    tracing::debug!("ETP connection from {peer} ended: {e}");
                }
            }
        },
    )
    .await
}

#[derive(PartialEq)]
enum Login {
    NeedUser,
    NeedPassword { valid_user: bool },
    LoggedIn,
}

/// The search options of a connection, as set by EVERYTHING commands.
#[derive(Clone, PartialEq)]
struct Search {
    text: String,
    filter: String,
    options: SearchOptions,
    filter_options: SearchOptions,
    sort: Sort,
    offset: u64,
    count: u64,
    columns: Columns,
}

impl Default for Search {
    fn default() -> Self {
        Search {
            text: String::new(),
            filter: String::new(),
            options: SearchOptions::default(),
            filter_options: SearchOptions::default(),
            sort: Sort::default(),
            offset: 0,
            count: u32::MAX as u64,
            columns: Columns::default(),
        }
    }
}

impl Search {
    /// What decides the result set (not the window or the columns).
    fn key(&self) -> (String, String, SearchOptions, SearchOptions, Sort) {
        (
            self.text.clone(),
            self.filter.clone(),
            self.options.clone(),
            self.filter_options.clone(),
            self.sort,
        )
    }

    /// The query run: the search, narrowed by the filter search.
    fn query(&self) -> String {
        match (self.text.trim().is_empty(), self.filter.trim().is_empty()) {
            (_, true) => self.text.clone(),
            (true, false) => self.filter.clone(),
            (false, false) => format!("<{}> <{}>", self.text, self.filter),
        }
    }
}

enum Data {
    None,
    Passive(TcpListener),
    Active(SocketAddr),
}

struct Connection {
    daemon: Arc<Daemon>,
    settings: EtpServer,
    caller: Caller,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    peer: SocketAddr,
    local: SocketAddr,
    login: Login,
    cwd: Vec<u8>,
    rest: u64,
    data: Data,
    search: Search,
    /// The last query and its results.
    cache: Option<(CacheKey, Arc<crate::server::ResultSet>)>,
}

/// What a query's results depend on: search, filter, both options, sort.
type CacheKey = (String, String, SearchOptions, SearchOptions, Sort);

fn welcome(message: &str) -> String {
    let message = if message.trim().is_empty() {
        "Welcome to Everything ETP/FTP"
    } else {
        message
    };
    let text = message
        .replace("%v", env!("CARGO_PKG_VERSION"))
        .replace("%n", "\n")
        .replace("%%", "%");
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let sep = if i + 1 == lines.len() { ' ' } else { '-' };
        out.push_str(&format!("220{sep}{line}\r\n"));
    }
    out
}

/// FTP's `MDTM` and `MLST` time: UTC `YYYYMMDDHHMMSS`.
fn ftp_time(ns: i64) -> String {
    chrono::DateTime::from_timestamp(ns.div_euclid(1_000_000_000), 0)
        .unwrap_or_default()
        .format("%Y%m%d%H%M%S")
        .to_string()
}

/// A `LIST` line in the Unix `ls -l` style FTP clients parse.
fn list_line(e: &Entry) -> String {
    let when =
        chrono::DateTime::from_timestamp(e.mtime.div_euclid(1_000_000_000), 0).unwrap_or_default();
    let six_months = chrono::Utc::now() - chrono::Duration::days(182);
    let date = if when > six_months {
        when.format("%b %e %H:%M").to_string()
    } else {
        when.format("%b %e  %Y").to_string()
    };
    format!(
        "{} 1 ftp ftp {:>12} {} {}\r\n",
        if e.is_dir { "drwxr-xr-x" } else { "-rw-r--r--" },
        if e.is_dir { 0 } else { e.size },
        date,
        String::from_utf8_lossy(&e.name)
    )
}

fn mlst_facts(e: &Entry) -> String {
    let mut s = format!(
        "type={};modify={};",
        if e.is_dir { "dir" } else { "file" },
        ftp_time(e.mtime)
    );
    if !e.is_dir {
        s.push_str(&format!("size={};", e.size));
    }
    s
}

fn flag(value: &str) -> bool {
    value.trim().parse::<i64>().is_ok_and(|v| v != 0)
}

impl Connection {
    async fn run(daemon: Arc<Daemon>, stream: TcpStream, peer: SocketAddr) -> std::io::Result<()> {
        let settings = daemon.servers.stored().etp;
        let Ok(caller) = super::run_as(&settings.run_as) else {
            let mut s = stream;
            s.write_all(b"421 Service not available.\r\n").await?;
            return Ok(());
        };
        let local = stream.local_addr()?;
        let (r, w) = stream.into_split();
        let mut c = Connection {
            daemon,
            settings,
            caller,
            reader: BufReader::new(r),
            writer: w,
            peer,
            local,
            login: Login::NeedUser,
            cwd: b"/".to_vec(),
            rest: 0,
            data: Data::None,
            search: Search::default(),
            cache: None,
        };
        let banner = welcome(&c.settings.welcome_message);
        c.send(&banner).await?;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = tokio::time::timeout(
                IDLE,
                (&mut c.reader)
                    .take(MAX_LINE as u64)
                    .read_until(b'\n', &mut line),
            )
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "idle"))??;
            if n == 0 {
                return Ok(());
            }
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            let (cmd, param) = match text.split_once(' ') {
                Some((c, p)) => (c.to_owned(), p.to_owned()),
                None => (text.clone(), String::new()),
            };
            if !c.command(&cmd.to_ascii_lowercase(), &param).await? {
                return Ok(());
            }
        }
    }

    async fn send(&mut self, text: &str) -> std::io::Result<()> {
        self.writer.write_all(text.as_bytes()).await
    }

    async fn reply(&mut self, code: u16, text: &str) -> std::io::Result<()> {
        self.send(&format!("{code} {text}\r\n")).await
    }

    /// Handles one command; false ends the session.
    async fn command(&mut self, cmd: &str, param: &str) -> std::io::Result<bool> {
        let logged_in = self.login == Login::LoggedIn;
        let open = matches!(cmd, "user" | "pass" | "quit" | "noop" | "opts" | "feat");
        if !logged_in && !open {
            self.reply(530, "Not logged on.").await?;
            return Ok(true);
        }
        match cmd {
            "user" => {
                let s = &self.settings;
                let valid_user = s.username.is_empty() || s.username == param;
                let needs_password = s.password.as_deref().is_some_and(|p| !p.is_empty());
                if valid_user && !needs_password {
                    self.login = Login::LoggedIn;
                    self.reply(230, "Logged on.").await?;
                } else {
                    // Ask for a password either way, so names cannot be probed.
                    self.login = Login::NeedPassword { valid_user };
                    self.reply(331, &format!("Password required for {param}")).await?;
                }
            }
            "pass" => match self.login {
                Login::NeedPassword { valid_user } => {
                    let stored = self.settings.password.clone().unwrap_or_default();
                    if valid_user && verify_password(&stored, param) {
                        self.login = Login::LoggedIn;
                        self.reply(230, "Logged on.").await?;
                    } else {
                        // Slow down password guessing.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        self.login = Login::NeedUser;
                        self.reply(530, "Login or password incorrect!").await?;
                    }
                }
                Login::LoggedIn => self.reply(503, "Invalid sequence of commands.").await?,
                Login::NeedUser => self.reply(530, "Not logged on.").await?,
            },
            "quit" => {
                self.reply(221, "Goodbye.").await?;
                return Ok(false);
            }
            "noop" => self.reply(200, "OK").await?,
            "opts" => {
                if param.eq_ignore_ascii_case("utf8 on") {
                    self.reply(200, "UTF8 mode enabled.").await?;
                } else {
                    self.reply(501, "Option not understood.").await?;
                }
            }
            "feat" => {
                self.send(
                    "211-Features:\r\n MDTM\r\n REST STREAM\r\n SIZE\r\n MLST type*;size*;modify*;\r\n MLSD\r\n UTF8\r\n EPSV\r\n EVERYTHING\r\n211 End\r\n",
                )
                .await?
            }
            "help" => {
                self.send("214-Site commands:\r\n EVERYTHING\r\n214 End\r\n")
                    .await?
            }
            "syst" => self.reply(215, "UNIX emulated by EverythingOS").await?,
            "type" => {
                let t = param.trim().to_ascii_lowercase();
                if matches!(t.as_str(), "a" | "i" | "a n" | "l 8") {
                    self.reply(200, &format!("Type set to {}.", t.to_uppercase()))
                        .await?;
                } else {
                    self.reply(504, "Type not supported.").await?;
                }
            }
            "mode" => {
                if param.eq_ignore_ascii_case("s") {
                    self.reply(200, "Mode set to S.").await?
                } else {
                    self.reply(504, "Mode not supported.").await?
                }
            }
            "stru" => {
                if param.eq_ignore_ascii_case("f") {
                    self.reply(200, "Structure set to F.").await?
                } else {
                    self.reply(504, "Structure not supported.").await?
                }
            }
            "pwd" | "xpwd" => {
                let cwd = String::from_utf8_lossy(&self.cwd).replace('"', "\"\"");
                self.reply(257, &format!("\"{cwd}\" is current directory."))
                    .await?
            }
            "cwd" | "cdup" => {
                let target = if cmd == "cdup" { "..".to_owned() } else { param.to_owned() };
                let path = self.resolve(&target);
                let (d, caller, include) = self.ctx();
                let p = path.clone();
                let found = blocking(move || files::lookup(&d, &caller, &include, &p)).await;
                if found.is_some_and(|e| e.is_dir) {
                    self.cwd = path;
                    self.reply(250, "CWD successful.").await?
                } else {
                    self.reply(550, "Directory not found.").await?
                }
            }
            "size" | "mdtm" => {
                let path = self.resolve(param);
                let (d, caller, include) = self.ctx();
                let found = blocking(move || files::lookup(&d, &caller, &include, &path)).await;
                match found {
                    Some(e) if cmd == "size" && !e.is_dir => {
                        self.reply(213, &e.size.to_string()).await?
                    }
                    Some(e) if cmd == "mdtm" => self.reply(213, &ftp_time(e.mtime)).await?,
                    _ => self.reply(550, "File not found.").await?,
                }
            }
            "mlst" => {
                let target = if param.is_empty() { "." } else { param };
                let path = self.resolve(target);
                let (d, caller, include) = self.ctx();
                let p = path.clone();
                match blocking(move || files::lookup(&d, &caller, &include, &p)).await {
                    Some(e) => {
                        let name = String::from_utf8_lossy(&path).into_owned();
                        self.send(&format!(
                            "250-Listing {name}\r\n {} {name}\r\n250 End.\r\n",
                            mlst_facts(&e)
                        ))
                        .await?
                    }
                    None => self.reply(550, "File not found.").await?,
                }
            }
            "rest" => match param.trim().parse::<u64>() {
                Ok(n) => {
                    self.rest = n;
                    self.reply(350, &format!("Rest supported. Restarting at {n}"))
                        .await?
                }
                Err(_) => self.reply(501, "Syntax error.").await?,
            },
            "pasv" => self.passive(false).await?,
            "epsv" => self.passive(true).await?,
            "port" | "eprt" => self.active(cmd, param).await?,
            "list" | "nlst" | "mlsd" => self.listing(cmd, param).await?,
            "retr" => self.retr(param).await?,
            "everything" => self.everything(param).await?,
            "site" => {
                let (sub, rest) = param.split_once(' ').unwrap_or((param, ""));
                if sub.eq_ignore_ascii_case("everything") {
                    self.everything(rest).await?
                } else if sub.is_empty() {
                    self.reply(500, "SITE: Missing argument.").await?
                } else {
                    self.reply(500, "Unknown SITE command.").await?
                }
            }
            "stor" | "stou" | "appe" | "dele" | "rmd" | "mkd" | "xmkd" | "xrmd" | "rnfr"
            | "rnto" => {
                self.reply(550, "Permission denied: this server is read-only.")
                    .await?
            }
            _ => self.reply(500, "Syntax error, command unrecognized.").await?,
        }
        Ok(true)
    }

    fn ctx(&self) -> (Arc<Daemon>, Caller, Vec<String>) {
        (self.daemon.clone(), self.caller.clone(), Vec::new())
    }

    /// An FTP path relative to the working folder, as an absolute path.
    fn resolve(&self, param: &str) -> Vec<u8> {
        let p = param.as_bytes();
        let joined = if p.starts_with(b"/") {
            p.to_vec()
        } else {
            [self.cwd.as_slice(), b"/", p].concat()
        };
        files::normalize(&joined)
    }

    async fn passive(&mut self, extended: bool) -> std::io::Result<()> {
        let ip = self.local.ip();
        if !extended && matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none()) {
            return self.reply(522, "Use EPSV over IPv6.").await;
        }
        let listener = match TcpListener::bind(SocketAddr::new(ip, 0)).await {
            Ok(l) => l,
            Err(_) => return self.reply(425, "Cannot open data connection.").await,
        };
        let port = listener.local_addr()?.port();
        self.data = Data::Passive(listener);
        if extended {
            self.reply(229, &format!("Entering Extended Passive Mode (|||{port}|)"))
                .await
        } else {
            let v4 = match ip {
                IpAddr::V4(v4) => v4,
                IpAddr::V6(v6) => v6.to_ipv4_mapped().unwrap_or(std::net::Ipv4Addr::LOCALHOST),
            };
            let o = v4.octets();
            self.reply(
                227,
                &format!(
                    "Entering Passive Mode ({},{},{},{},{},{})",
                    o[0],
                    o[1],
                    o[2],
                    o[3],
                    port >> 8,
                    port & 0xff
                ),
            )
            .await
        }
    }

    async fn active(&mut self, cmd: &str, param: &str) -> std::io::Result<()> {
        let addr = if cmd == "port" {
            let n: Vec<u16> = param
                .split(',')
                .filter_map(|p| p.trim().parse().ok())
                .collect();
            (n.len() == 6 && n.iter().all(|&v| v < 256)).then(|| {
                SocketAddr::new(
                    IpAddr::from([n[0] as u8, n[1] as u8, n[2] as u8, n[3] as u8]),
                    n[4] * 256 + n[5],
                )
            })
        } else {
            // |proto|address|port|
            let parts: Vec<&str> = param.split(param.chars().next().unwrap_or('|')).collect();
            parts
                .get(2)
                .zip(parts.get(3))
                .and_then(|(a, p)| Some(SocketAddr::new(a.parse().ok()?, p.parse().ok()?)))
        };
        // Only the client itself: no bouncing connections elsewhere.
        let same = |a: IpAddr, b: IpAddr| a.to_canonical() == b.to_canonical();
        match addr {
            Some(a) if same(a.ip(), self.peer.ip()) && a.port() >= 1024 => {
                self.data = Data::Active(a);
                self.reply(200, "Port command successful.").await
            }
            Some(_) => {
                self.reply(504, "Data connections only to the client.")
                    .await
            }
            None => self.reply(501, "Syntax error.").await,
        }
    }

    async fn data_stream(&mut self) -> Option<TcpStream> {
        let peer = self.peer.ip().to_canonical();
        match std::mem::replace(&mut self.data, Data::None) {
            Data::Passive(listener) => {
                let accepted = tokio::time::timeout(Duration::from_secs(30), listener.accept())
                    .await
                    .ok()?
                    .ok()?;
                (accepted.1.ip().to_canonical() == peer).then_some(accepted.0)
            }
            Data::Active(addr) => {
                tokio::time::timeout(Duration::from_secs(30), TcpStream::connect(addr))
                    .await
                    .ok()?
                    .ok()
            }
            Data::None => None,
        }
    }

    async fn listing(&mut self, cmd: &str, param: &str) -> std::io::Result<()> {
        // Ignore ls-style options such as "-a".
        let target = param
            .split(' ')
            .filter(|p| !p.starts_with('-') && !p.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let path = self.resolve(if target.is_empty() { "." } else { &target });
        let (d, caller, include) = self.ctx();
        let p = path.clone();
        let entries = blocking(move || files::list(&d, &caller, &include, &p)).await;
        let Some(entries) = entries else {
            return self.reply(550, "Directory not found.").await;
        };
        let text: String = entries
            .iter()
            .map(|e| match cmd {
                "nlst" => format!("{}\r\n", String::from_utf8_lossy(&e.name)),
                "mlsd" => format!("{} {}\r\n", mlst_facts(e), String::from_utf8_lossy(&e.name)),
                _ => list_line(e),
            })
            .collect();
        self.reply(150, "Opening data channel for directory listing.")
            .await?;
        match self.data_stream().await {
            Some(mut data) => {
                let ok = data.write_all(text.as_bytes()).await.is_ok();
                let _ = data.shutdown().await;
                if ok {
                    self.reply(226, "Successfully transferred.").await
                } else {
                    self.reply(426, "Connection closed; transfer aborted.")
                        .await
                }
            }
            None => self.reply(425, "Cannot open data connection.").await,
        }
    }

    async fn retr(&mut self, param: &str) -> std::io::Result<()> {
        if !self.settings.allow_file_download {
            self.data = Data::None;
            return self.reply(550, "File downloads are disabled.").await;
        }
        let path = self.resolve(param);
        let (d, caller, include) = self.ctx();
        let opened = blocking(move || files::open(&d, &caller, &include, &path)).await;
        let file = match opened {
            Ok((file, _)) => file,
            Err(e) => {
                self.data = Data::None;
                return self.reply(550, &e).await;
            }
        };
        let offset = std::mem::take(&mut self.rest);
        let mut file = tokio::fs::File::from_std(file);
        if offset > 0 && file.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
            return self.reply(550, "Cannot restart at that offset.").await;
        }
        self.reply(150, "Data connection accepted; transfer starting.")
            .await?;
        match self.data_stream().await {
            Some(mut data) => {
                let ok = tokio::io::copy(&mut file, &mut data).await.is_ok();
                let _ = data.shutdown().await;
                if ok {
                    self.reply(226, "Transfer OK").await
                } else {
                    self.reply(426, "Connection closed; transfer aborted.")
                        .await
                }
            }
            None => self.reply(425, "Cannot open data connection.").await,
        }
    }

    /// `EVERYTHING <option> <value>` and `EVERYTHING QUERY`.
    async fn everything(&mut self, param: &str) -> std::io::Result<()> {
        let (option, value) = param.split_once(' ').unwrap_or((param, ""));
        let option = option.to_ascii_lowercase();
        let s = &mut self.search;
        let set = |field: &mut bool| *field = flag(value);
        let reply = match option.as_str() {
            "case" => {
                set(&mut s.options.match_case);
                format!("Case set to ({}).", flag(value) as u8)
            }
            "whole_word" => {
                set(&mut s.options.whole_word);
                format!("Whole word set to ({}).", flag(value) as u8)
            }
            "path" => {
                set(&mut s.options.match_path);
                format!("Path set to ({}).", flag(value) as u8)
            }
            "diacritics" => {
                set(&mut s.options.match_diacritics);
                format!("Diacritics set to ({}).", flag(value) as u8)
            }
            "prefix" => {
                set(&mut s.options.match_prefix);
                format!("Prefix set to ({}).", flag(value) as u8)
            }
            "suffix" => {
                set(&mut s.options.match_suffix);
                format!("Suffix set to ({}).", flag(value) as u8)
            }
            "ignore_punctuation" => {
                set(&mut s.options.ignore_punctuation);
                format!("Ignore punctuation set to ({}).", flag(value) as u8)
            }
            "ignore_whitespace" => {
                set(&mut s.options.ignore_white_space);
                format!("Ignore whitespace set to ({}).", flag(value) as u8)
            }
            "regex" => {
                set(&mut s.options.regex);
                format!("Regex set to ({}).", flag(value) as u8)
            }
            "hide_empty_search_results" => {
                format!("Hide empty search results set to ({}).", flag(value) as u8)
            }
            "search" => {
                s.text = value.to_owned();
                format!("Search set to ({value}).")
            }
            "filter_search" => {
                s.filter = value.to_owned();
                format!("Filter search set to ({value}).")
            }
            "filter_case" => {
                set(&mut s.filter_options.match_case);
                format!("Filter case set to ({}).", flag(value) as u8)
            }
            "filter_whole_word" => {
                set(&mut s.filter_options.whole_word);
                format!("Filter whole word set to ({}).", flag(value) as u8)
            }
            "filter_path" => {
                set(&mut s.filter_options.match_path);
                format!("Filter path set to ({}).", flag(value) as u8)
            }
            "filter_diacritics"
            | "filter_prefix"
            | "filter_suffix"
            | "filter_regex"
            | "filter_ignore_punctuation"
            | "filter_ignore_whitespace" => {
                format!(
                    "{} set to ({}).",
                    option.replace('_', " "),
                    flag(value) as u8
                )
            }
            "sort" => match parse_sort(value) {
                Some((key, descending)) => {
                    s.sort = Sort { key, descending };
                    format!("Sort set to ({value}).")
                }
                None => return self.reply(500, "Unknown sort type.").await,
            },
            "offset" => {
                s.offset = value.trim().parse().unwrap_or(0);
                format!("Offset set to ({}).", s.offset)
            }
            "count" => {
                s.count = value.trim().parse().unwrap_or(u32::MAX as u64);
                format!("Count set to ({}).", s.count)
            }
            "size_column" => {
                s.columns.size = flag(value);
                format!("Size column set to ({}).", flag(value) as u8)
            }
            "attributes_column" => {
                s.columns.attributes = flag(value);
                format!("Attributes column set to ({}).", flag(value) as u8)
            }
            "date_modified_column" => {
                s.columns.date_modified = flag(value);
                format!("Date modified column set to ({}).", flag(value) as u8)
            }
            "date_created_column" => {
                s.columns.date_created = flag(value);
                format!("Date created column set to ({}).", flag(value) as u8)
            }
            "path_column" => {
                s.columns.path = flag(value);
                format!("Path column set to ({}).", flag(value) as u8)
            }
            "file_list_filename_column" => {
                s.columns.file_list_filename = flag(value);
                format!("File list filename column set to ({}).", flag(value) as u8)
            }
            "date_recently_changed_column" => {
                s.columns.date_recently_changed = flag(value);
                format!(
                    "Date recently changed column set to ({}).",
                    flag(value) as u8
                )
            }
            "query" => return self.query().await,
            _ => return self.reply(500, "Unknown Everything command.").await,
        };
        self.reply(200, &reply).await
    }

    async fn query(&mut self) -> std::io::Result<()> {
        let key = self.search.key();
        let cached = self
            .cache
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, set)| set.clone());
        let set = match cached {
            Some(set) => set,
            None => {
                let (d, caller) = (self.daemon.clone(), self.caller.clone());
                let (query, options, sort) = (
                    self.search.query(),
                    self.search.options.clone(),
                    self.search.sort,
                );
                match blocking(move || {
                    crate::server::search(&d, &caller, &query, &options, sort, Audience::Served)
                })
                .await
                {
                    Ok((set, _)) => {
                        let set = Arc::new(set);
                        self.cache = Some((key, set.clone()));
                        set
                    }
                    Err((_, message)) => return self.reply(500, &message).await,
                }
            }
        };
        let total = set.hits.len() as u64;
        let (d, uid) = (self.daemon.clone(), self.caller.uid);
        let (offset, count) = (self.search.offset.min(total), self.search.count);
        let columns = self.search.columns;
        let text = blocking(move || {
            let mut rows = Vec::new();
            let end = offset.saturating_add(count).min(total);
            let mut at = offset;
            while at < end {
                let n = (end - at).min(10_000) as u32;
                match crate::server::fetch(&d, uid, &set, at as u32, n) {
                    Ok(batch) if !batch.is_empty() => {
                        at += batch.len() as u64;
                        rows.extend(batch);
                    }
                    _ => break,
                }
            }
            let items: Vec<_> = rows.iter().map(row_to_item).collect();
            format_reply(total, &items, &columns)
        })
        .await;
        self.send(&text).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banners_and_lines() {
        assert_eq!(welcome(""), "220 Welcome to Everything ETP/FTP\r\n");
        assert_eq!(
            welcome("Hi%nv%v"),
            format!("220-Hi\r\n220 v{}\r\n", env!("CARGO_PKG_VERSION"))
        );
        let dir = Entry {
            name: b"docs".to_vec(),
            is_dir: true,
            size: 0,
            mtime: 1_700_000_000_000_000_000,
        };
        assert!(list_line(&dir).starts_with("drwxr-xr-x 1 ftp ftp            0 "));
        assert!(list_line(&dir).ends_with(" docs\r\n"));
        assert_eq!(mlst_facts(&dir), "type=dir;modify=20231114221320;");
        assert_eq!(ftp_time(1_700_000_000_000_000_000), "20231114221320");
        assert!(flag("1") && flag(" 2") && !flag("0") && !flag("x"));
    }

    #[test]
    fn filter_searches_narrow_the_search() {
        let mut s = Search {
            text: "report".into(),
            ..Default::default()
        };
        assert_eq!(s.query(), "report");
        s.filter = "ext:pdf".into();
        assert_eq!(s.query(), "<report> <ext:pdf>");
        s.text.clear();
        assert_eq!(s.query(), "ext:pdf");
    }
}
