//! Searching an ETP server directly, as Everything's "Connect to ETP
//! Server" does: a client session switches its searches to the server
//! (Everything on a Windows computer, or another EverythingOS), and result
//! rows are fetched from it a window at a time as the list scrolls.

use std::collections::HashMap;
use std::time::Duration;

use evos_etp::{Columns, EtpClient, EtpError, Item, Query};
use evos_proto::{ErrorCode, RemoteSearch, Row, SearchOptions, Sort};

use crate::server::Failure;

const TIMEOUT: Duration = Duration::from_secs(20);
/// Results asked for at a time.
const PAGE: u64 = 256;
/// Windows kept per result; older ones are fetched again when needed.
const MAX_PAGES: usize = 256;

/// A session's connection to an ETP server.
pub struct Connection {
    pub target: RemoteSearch,
    pub welcome: String,
    client: Option<EtpClient>,
}

/// A search run on the server: its settings and the windows fetched.
pub struct RemoteResult {
    query: Query,
    pub total: u64,
    pages: HashMap<u64, Vec<Item>>,
}

fn failure(e: EtpError) -> Failure {
    let code = match e {
        EtpError::Login(_) => ErrorCode::PermissionDenied,
        EtpError::Server(_) => ErrorCode::BadQuery,
        _ => ErrorCode::Failed,
    };
    (code, e.to_string())
}

/// The FTP path of a result's full path: Everything serves `C:\a\b` as
/// `/C:/a/b`.
pub fn ftp_path(path: &str) -> String {
    if evos_etp::is_windows_path(path) {
        format!("/{}", path.trim_start_matches('\\').replace('\\', "/"))
    } else {
        path.to_owned()
    }
}

impl Connection {
    pub fn open(target: RemoteSearch) -> Result<Connection, Failure> {
        let client = EtpClient::connect(
            &target.host,
            target.port,
            &target.username,
            &target.password,
            TIMEOUT,
        )
        .map_err(failure)?;
        Ok(Connection {
            welcome: client.welcome.clone(),
            target,
            client: Some(client),
        })
    }

    /// Runs `f` on the server, reconnecting once if the connection broke.
    fn with_client<T>(
        &mut self,
        mut f: impl FnMut(&mut EtpClient) -> Result<T, EtpError>,
    ) -> Result<T, Failure> {
        for attempt in 0..2 {
            if self.client.is_none() {
                let t = &self.target;
                self.client = Some(
                    EtpClient::connect(&t.host, t.port, &t.username, &t.password, TIMEOUT)
                        .map_err(failure)?,
                );
            }
            let client = self.client.as_mut().expect("connected");
            match f(client) {
                Err(EtpError::Io(e)) => {
                    self.client = None;
                    if attempt == 1 {
                        return Err((ErrorCode::Failed, format!("connection lost: {e}")));
                    }
                }
                other => return other.map_err(failure),
            }
        }
        unreachable!("the second attempt returns")
    }

    pub fn search(
        &mut self,
        search: &str,
        options: &SearchOptions,
        sort: Sort,
    ) -> Result<RemoteResult, Failure> {
        let query = Query {
            search: search.to_owned(),
            match_case: options.match_case,
            whole_word: options.whole_word,
            match_path: options.match_path,
            diacritics: options.match_diacritics,
            prefix: options.match_prefix,
            suffix: options.match_suffix,
            ignore_punctuation: options.ignore_punctuation,
            ignore_whitespace: options.ignore_white_space,
            regex: options.regex,
            sort: evos_etp::sort_name(sort.key, sort.descending),
            offset: 0,
            count: PAGE,
            columns: Columns::all(),
        };
        let reply = self.with_client(|c| c.query(&query))?;
        Ok(RemoteResult {
            query,
            total: reply.total,
            pages: HashMap::from([(0, reply.items)]),
        })
    }

    /// The same search in another order.
    pub fn sort(&mut self, result: &RemoteResult, sort: Sort) -> Result<RemoteResult, Failure> {
        let mut query = result.query.clone();
        query.sort = evos_etp::sort_name(sort.key, sort.descending);
        query.offset = 0;
        let reply = self.with_client(|c| c.query(&query))?;
        Ok(RemoteResult {
            query,
            total: reply.total,
            pages: HashMap::from([(0, reply.items)]),
        })
    }

    /// Rows `offset..offset + count`, asking the server for windows not
    /// fetched yet.
    pub fn fetch(
        &mut self,
        result: &mut RemoteResult,
        offset: u32,
        count: u32,
    ) -> Result<Vec<Row>, Failure> {
        let start = u64::from(offset).min(result.total);
        let end = (start + u64::from(count)).min(result.total);
        let mut rows = Vec::with_capacity((end - start) as usize);
        let mut page = start / PAGE * PAGE;
        while page < end {
            if !result.pages.contains_key(&page) {
                if result.pages.len() >= MAX_PAGES {
                    result.pages.clear();
                }
                let mut query = result.query.clone();
                query.offset = page;
                let reply = self.with_client(|c| c.query(&query))?;
                result.pages.insert(page, reply.items);
            }
            let items = &result.pages[&page];
            let from = start.max(page) - page;
            let to = (end.min(page + PAGE) - page).min(items.len() as u64);
            rows.extend(items[from.min(to) as usize..to as usize].iter().map(|i| {
                let mut row = evos_etp::item_to_row(i);
                row.remote = true;
                row
            }));
            page += PAGE;
        }
        Ok(rows)
    }

    /// Reads part of a result's file from the server.
    pub fn read_file(&mut self, path: &[u8], offset: u64, limit: u32) -> Result<Vec<u8>, Failure> {
        let path = ftp_path(&String::from_utf8_lossy(path));
        self.with_client(|c| c.read_file(&path, offset, u64::from(limit)))
            .map_err(|(_, m)| (ErrorCode::Failed, m))
    }

    pub fn close(self) {
        if let Some(c) = self.client {
            c.quit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ftp_paths() {
        assert_eq!(ftp_path("C:\\Users\\me\\a.txt"), "/C:/Users/me/a.txt");
        assert_eq!(ftp_path("/home/me/a.txt"), "/home/me/a.txt");
    }
}
