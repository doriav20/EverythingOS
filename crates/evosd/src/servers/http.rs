//! Everything's HTTP server: search and download from a web browser, and
//! the query interface scripts use (`/?search=...&json=1`).
//!
//! The parameters and the JSON format follow voidtools' HTTP server plugin
//! (`s`/`q`/`search`, `o`/`offset`, `c`/`n`/`count`, `j`/`json`,
//! `i`/`case`, `w`/`wholeword`, `p`/`path`, `r`/`regex`, `m`/`diacritics`,
//! `prefix`, `suffix`, `ignore_punctuation`, `ignore_whitespace`,
//! `path_column`, `size_column`, `date_modified_column`, `sort`,
//! `ascending`). Results and downloads are limited to what the "run as"
//! account may see and read; nothing is ever written.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use evos_core::perm::Caller;
use evos_proto::{HttpServer, Row, SearchOptions, Sort, SortKey};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::files;
use crate::accounts::verify_password;
use crate::server::{blocking, Audience, Daemon};

const MAX_HEAD: usize = 16 * 1024;
const MAX_CLIENTS: usize = 64;
/// Results per page in the browser.
const PAGE: u64 = 32;

pub async fn serve(daemon: Arc<Daemon>, listener: TcpListener) {
    super::accept_loop(
        listener,
        MAX_CLIENTS,
        b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        |stream, _| {
            let daemon = daemon.clone();
            async move {
                let _ =
                    tokio::time::timeout(Duration::from_secs(3600), handle(daemon, stream)).await;
            }
        },
    )
    .await
}

/// A parsed request head.
struct Request {
    method: String,
    path: Vec<u8>,
    params: HashMap<String, String>,
    authorization: Option<String>,
    range: Option<String>,
}

/// Decodes `%XX` escapes (and `+` as a space when `plus` is set).
pub fn unescape(s: &str, plus: bool) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' if plus => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    out
}

/// Escapes a path for a URL (keeps `/`).
fn escape_url(path: &[u8]) -> String {
    let mut s = String::new();
    for &b in path {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::new();
    for chunk in bytes.chunks(4) {
        let mut acc = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            acc |= value(c)? << (18 - 6 * i);
        }
        out.push((acc >> 16) as u8);
        if chunk.len() > 2 {
            out.push((acc >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(acc as u8);
        }
    }
    Some(out)
}

fn parse_head(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_ascii_uppercase();
    let target = first.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut params = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(
            String::from_utf8_lossy(&unescape(k, true)).to_ascii_lowercase(),
            String::from_utf8_lossy(&unescape(v, true)).into_owned(),
        );
    }
    let mut authorization = None;
    let mut range = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "authorization" => authorization = Some(v.trim().to_owned()),
                "range" => range = Some(v.trim().to_owned()),
                _ => {}
            }
        }
    }
    Some(Request {
        method,
        path: unescape(path, false),
        params,
        authorization,
        range,
    })
}

fn authorized(settings: &HttpServer, header: Option<&str>) -> bool {
    let password = settings.password.as_deref().unwrap_or("");
    if settings.username.is_empty() && password.is_empty() {
        return true;
    }
    let Some(encoded) = header.and_then(|h| h.strip_prefix("Basic ")) else {
        return false;
    };
    let Some(decoded) = base64_decode(encoded.trim()) else {
        return false;
    };
    let decoded = String::from_utf8_lossy(&decoded);
    let (user, pass) = decoded.split_once(':').unwrap_or((&decoded, ""));
    (settings.username.is_empty() || user == settings.username) && verify_password(password, pass)
}

async fn respond(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra: &str,
    body: &[u8],
    head_only: bool,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nServer: EverythingOS\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    if !head_only {
        stream.write_all(body).await?;
    }
    stream.shutdown().await
}

async fn handle(daemon: Arc<Daemon>, mut stream: TcpStream) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(30), stream.read(&mut buf))
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "slow client"))??;
        if n == 0 || head.len() > MAX_HEAD {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let Some(req) = parse_head(&String::from_utf8_lossy(&head)) else {
        return respond(
            &mut stream,
            "400 Bad Request",
            "text/plain",
            "",
            b"Bad request",
            false,
        )
        .await;
    };
    let settings = daemon.servers.stored().http;
    let head_only = req.method == "HEAD";
    if req.method != "GET" && !head_only {
        return respond(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain",
            "Allow: GET, HEAD\r\n",
            b"Read-only server",
            false,
        )
        .await;
    }
    if !authorized(&settings, req.authorization.as_deref()) {
        tokio::time::sleep(Duration::from_millis(500)).await;
        return respond(
            &mut stream,
            "401 Unauthorized",
            "text/html; charset=UTF-8",
            "WWW-Authenticate: Basic realm=\"EverythingOS\"\r\n",
            b"<h1>401 Unauthorized</h1>",
            head_only,
        )
        .await;
    }
    let Ok(caller) = super::run_as(&settings.run_as) else {
        return respond(
            &mut stream,
            "503 Service Unavailable",
            "text/plain",
            "",
            b"No account to run as",
            false,
        )
        .await;
    };
    let is_search = ["s", "q", "search"]
        .iter()
        .any(|k| req.params.contains_key(*k))
        || req.path == b"/";
    if is_search {
        return search(&daemon, &caller, &req, &mut stream, head_only).await;
    }
    let (d, c, path) = (daemon.clone(), caller.clone(), req.path.clone());
    let entry = blocking(move || files::lookup(&d, &c, &[], &path)).await;
    match entry {
        Some(e) if e.is_dir => folder(&daemon, &caller, &req, &mut stream, head_only).await,
        Some(_) => download(&daemon, &caller, &settings, &req, &mut stream, head_only).await,
        None => {
            respond(
                &mut stream,
                "404 Not Found",
                "text/html; charset=UTF-8",
                "",
                b"<h1>404 Not Found</h1>",
                head_only,
            )
            .await
        }
    }
}

fn param_flag(params: &HashMap<String, String>, keys: &[&str]) -> bool {
    keys.iter()
        .filter_map(|k| params.get(*k))
        .any(|v| v.trim().parse::<i64>().is_ok_and(|n| n != 0))
}

fn param_num(params: &HashMap<String, String>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .filter_map(|k| params.get(*k))
        .find_map(|v| v.trim().parse().ok())
}

fn human(size: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = size as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn local_date(ns: i64) -> String {
    use chrono::TimeZone;
    if ns == 0 {
        return String::new();
    }
    chrono::Local
        .timestamp_opt(ns.div_euclid(1_000_000_000), 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

const STYLE: &str = "body{font-family:system-ui,sans-serif;margin:0;color:#222}header{background:#f4f4f4;padding:12px 16px;border-bottom:1px solid #ddd}input[type=text]{width:min(600px,80vw);padding:6px;font-size:16px}table{border-collapse:collapse;width:100%}th,td{text-align:left;padding:4px 16px;white-space:nowrap}th{border-bottom:1px solid #ccc}td.num,th.num{text-align:right}tr:nth-child(even){background:#fafafa}a{color:#0645ad;text-decoration:none}.nav{padding:8px 16px}.count{padding:8px 16px;color:#666}b{background:#ffd}@media(prefers-color-scheme:dark){body{background:#1e1e1e;color:#ddd}header{background:#2a2a2a;border-color:#333}tr:nth-child(even){background:#252525}a{color:#8ab4f8}b{background:#554}}";

fn page(title: &str, search: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{} - EverythingOS</title><style>{STYLE}</style></head><body><header><form action=\"/\" method=\"get\"><input type=\"text\" name=\"search\" value=\"{}\" autofocus placeholder=\"Search\"> <input type=\"submit\" value=\"Search\"></form></header>{body}</body></html>",
        escape_html(title),
        escape_html(search)
    )
}

/// Search results as HTML (a page of them) or Everything's JSON.
async fn search(
    daemon: &Arc<Daemon>,
    caller: &Caller,
    req: &Request,
    stream: &mut TcpStream,
    head_only: bool,
) -> std::io::Result<()> {
    let p = &req.params;
    let query = ["s", "q", "search"]
        .iter()
        .find_map(|k| p.get(*k))
        .cloned()
        .unwrap_or_default();
    let json = param_flag(p, &["j", "json"]);
    let options = SearchOptions {
        match_case: param_flag(p, &["i", "case"]),
        whole_word: param_flag(p, &["w", "wholeword", "whole_word"]),
        match_path: param_flag(p, &["p", "path"]),
        regex: param_flag(p, &["r", "regex"]),
        match_diacritics: param_flag(p, &["m", "diacritics"]),
        match_prefix: param_flag(p, &["prefix"]),
        match_suffix: param_flag(p, &["suffix"]),
        ignore_punctuation: param_flag(p, &["ignore_punctuation"]),
        ignore_white_space: param_flag(p, &["ignore_whitespace"]),
    };
    let key = match p.get("sort").map(String::as_str) {
        Some("size") => SortKey::Size,
        Some("path") => SortKey::Path,
        Some("date_modified") => SortKey::Modified,
        _ => SortKey::Name,
    };
    let ascending = param_num(p, &["ascending"]).is_none_or(|v| v != 0);
    let sort = Sort {
        key,
        descending: !ascending,
    };
    let offset = param_num(p, &["o", "offset"]).unwrap_or(0);
    // JSON returns everything unless a count is given, as in Everything.
    let count = param_num(p, &["c", "n", "count"]).unwrap_or(if json { u64::MAX } else { PAGE });
    let (d, c, q, o) = (
        daemon.clone(),
        caller.clone(),
        query.clone(),
        options.clone(),
    );
    let found = blocking(move || -> Result<(u64, Vec<Row>, Vec<String>), String> {
        let (set, terms) =
            crate::server::search(&d, &c, &q, &o, sort, Audience::Served).map_err(|e| e.1)?;
        let total = set.hits.len() as u64;
        let end = offset.saturating_add(count).min(total);
        let mut rows = Vec::new();
        let mut at = offset.min(total);
        while at < end {
            let n = (end - at).min(10_000) as u32;
            let batch = crate::server::fetch(&d, c.uid, &set, at as u32, n).map_err(|e| e.1)?;
            if batch.is_empty() {
                break;
            }
            at += batch.len() as u64;
            rows.extend(batch);
        }
        Ok((total, rows, terms))
    })
    .await;
    let (total, rows, terms) = match found {
        Ok(f) => f,
        Err(e) => {
            let body = if json {
                format!("{{\"error\":\"{}\"}}", escape_json(&e))
            } else {
                page(
                    &query,
                    &query,
                    &format!("<p class=\"count\">{}</p>", escape_html(&e)),
                )
            };
            let ct = if json {
                "application/json"
            } else {
                "text/html; charset=UTF-8"
            };
            return respond(
                stream,
                "400 Bad Request",
                ct,
                "",
                body.as_bytes(),
                head_only,
            )
            .await;
        }
    };
    if json {
        let path_col = param_flag(p, &["path_column"]);
        let size_col = param_flag(p, &["size_column"]);
        let date_col = param_flag(p, &["date_modified_column"]);
        let mut out = format!("{{\r\n\t\"totalResults\":{total},\r\n\t\"results\":[\r\n");
        for (i, row) in rows.iter().enumerate() {
            let item = evos_etp::row_to_item(row);
            out.push_str(&format!(
                "\t\t{{\r\n\t\t\t\"type\":\"{}\",\r\n\t\t\t\"name\":\"{}",
                if row.is_dir { "folder" } else { "file" },
                escape_json(&item.name)
            ));
            if path_col {
                out.push_str(&format!(
                    "\",\r\n\t\t\t\"path\":\"{}",
                    escape_json(item.path.as_deref().unwrap_or(""))
                ));
            }
            if size_col {
                out.push_str("\",\r\n\t\t\t\"size\":\"");
                if let Some(s) = row.size {
                    out.push_str(&s.to_string());
                }
            }
            if date_col {
                out.push_str("\",\r\n\t\t\t\"date_modified\":\"");
                if row.mtime != 0 {
                    out.push_str(&evos_etp::date_to_wire(row.mtime).to_string());
                }
            }
            out.push_str(&format!(
                "\"\r\n\t\t}}{}\r\n",
                if i + 1 < rows.len() { "," } else { "" }
            ));
        }
        out.push_str("\t]\r\n}\r\n");
        return respond(
            stream,
            "200 OK",
            "application/json",
            "",
            out.as_bytes(),
            head_only,
        )
        .await;
    }
    // A browser page.
    let link = |o: u64, s: SortKey, asc: bool| {
        let sort = match s {
            SortKey::Size => "size",
            SortKey::Path => "path",
            SortKey::Modified => "date_modified",
            _ => "name",
        };
        format!(
            "/?search={}&offset={o}&sort={sort}&ascending={}",
            escape_url(query.as_bytes()),
            asc as u8
        )
    };
    let header = |title: &str, k: SortKey, class: &str| {
        let asc = if sort.key == k {
            sort.descending
        } else {
            k == SortKey::Name || k == SortKey::Path
        };
        format!(
            "<th class=\"{class}\"><a href=\"{}\">{title}{}</a></th>",
            escape_html(&link(0, k, asc)),
            if sort.key == k {
                if sort.descending {
                    " ▾"
                } else {
                    " ▴"
                }
            } else {
                ""
            }
        )
    };
    let mut body = format!(
        "<div class=\"count\">{total} {}</div><table><tr>{}{}{}{}</tr>",
        if total == 1 { "result" } else { "results" },
        header("Name", SortKey::Name, ""),
        header("Path", SortKey::Path, ""),
        header("Size", SortKey::Size, "num"),
        header("Date Modified", SortKey::Modified, "")
    );
    for row in &rows {
        let name = String::from_utf8_lossy(&row.name);
        let folder = String::from_utf8_lossy(&row.folder);
        let mut href = escape_url(&row.path());
        if row.is_dir {
            href.push('/');
        }
        let marked = evos_core::fold::highlight(&name, &terms);
        let mut shown = String::new();
        let mut at = 0;
        for r in marked {
            shown.push_str(&escape_html(&name[at..r.start]));
            shown.push_str(&format!("<b>{}</b>", escape_html(&name[r.clone()])));
            at = r.end;
        }
        shown.push_str(&escape_html(&name[at..]));
        body.push_str(&format!(
            "<tr><td><a href=\"{href}\">{}{shown}</a></td><td><a href=\"{}/\">{}</a></td><td class=\"num\">{}</td><td>{}</td></tr>",
            if row.is_dir { "📁 " } else { "" },
            escape_url(&row.folder),
            escape_html(&folder),
            if row.is_dir { String::new() } else { row.size.map(human).unwrap_or_default() },
            local_date(row.mtime)
        ));
    }
    body.push_str("</table><div class=\"nav\">");
    if offset > 0 {
        body.push_str(&format!(
            "<a href=\"{}\">&larr; Previous</a> ",
            escape_html(&link(
                offset.saturating_sub(count),
                sort.key,
                !sort.descending
            ))
        ));
    }
    if offset.saturating_add(count) < total {
        body.push_str(&format!(
            "<a href=\"{}\">Next &rarr;</a>",
            escape_html(&link(offset + count, sort.key, !sort.descending))
        ));
    }
    body.push_str("</div>");
    let html = page(
        if query.is_empty() {
            "Everything"
        } else {
            &query
        },
        &query,
        &body,
    );
    respond(
        stream,
        "200 OK",
        "text/html; charset=UTF-8",
        "",
        html.as_bytes(),
        head_only,
    )
    .await
}

/// A folder's contents, from the index.
async fn folder(
    daemon: &Arc<Daemon>,
    caller: &Caller,
    req: &Request,
    stream: &mut TcpStream,
    head_only: bool,
) -> std::io::Result<()> {
    let (d, c, path) = (daemon.clone(), caller.clone(), req.path.clone());
    let entries = blocking(move || files::list(&d, &c, &[], &path))
        .await
        .unwrap_or_default();
    let base = files::normalize(&req.path);
    let shown = String::from_utf8_lossy(&base).into_owned();
    let mut body = format!(
        "<div class=\"count\">Index of {}</div><table><tr><th>Name</th><th class=\"num\">Size</th><th>Date Modified</th></tr>",
        escape_html(&shown)
    );
    if base != b"/" {
        let parent = match base.iter().rposition(|&b| b == b'/') {
            Some(0) | None => b"/".to_vec(),
            Some(i) => base[..i].to_vec(),
        };
        body.push_str(&format!(
            "<tr><td><a href=\"{}\">..</a></td><td></td><td></td></tr>",
            escape_url(&parent)
        ));
    }
    for e in entries {
        let full = if base == b"/" {
            [b"/".as_slice(), &e.name].concat()
        } else {
            [base.as_slice(), b"/", &e.name].concat()
        };
        let mut href = escape_url(&full);
        if e.is_dir {
            href.push('/');
        }
        body.push_str(&format!(
            "<tr><td><a href=\"{href}\">{}{}</a></td><td class=\"num\">{}</td><td>{}</td></tr>",
            if e.is_dir { "📁 " } else { "" },
            escape_html(&String::from_utf8_lossy(&e.name)),
            if e.is_dir {
                String::new()
            } else {
                human(e.size)
            },
            local_date(e.mtime)
        ));
    }
    body.push_str("</table>");
    let html = page(&shown, "", &body);
    respond(
        stream,
        "200 OK",
        "text/html; charset=UTF-8",
        "",
        html.as_bytes(),
        head_only,
    )
    .await
}

fn content_type(path: &[u8]) -> &'static str {
    let ext = path
        .iter()
        .rposition(|&b| b == b'.')
        .map(|i| String::from_utf8_lossy(&path[i + 1..]).to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "htm" | "html" => "text/html; charset=UTF-8",
        "txt" | "log" | "md" | "csv" => "text/plain; charset=UTF-8",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` as a start and length.
fn byte_range(header: &str, size: u64) -> Option<(u64, u64)> {
    let spec = header.strip_prefix("bytes=")?.split(',').next()?.trim();
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (size.saturating_sub(n), size.checked_sub(1)?)
        }
        (a, "") => (a.parse().ok()?, size.checked_sub(1)?),
        (a, b) => (
            a.parse().ok()?,
            b.parse::<u64>().ok()?.min(size.checked_sub(1)?),
        ),
    };
    if start <= end && start < size {
        Some((start, end - start + 1))
    } else {
        None
    }
}

async fn download(
    daemon: &Arc<Daemon>,
    caller: &Caller,
    settings: &HttpServer,
    req: &Request,
    stream: &mut TcpStream,
    head_only: bool,
) -> std::io::Result<()> {
    if !settings.allow_file_download {
        return respond(
            stream,
            "403 Forbidden",
            "text/html; charset=UTF-8",
            "",
            b"<h1>File downloads are disabled</h1>",
            head_only,
        )
        .await;
    }
    let (d, c, path) = (daemon.clone(), caller.clone(), req.path.clone());
    let opened = blocking(move || files::open(&d, &c, &[], &path)).await;
    let (file, entry) = match opened {
        Ok(f) => f,
        Err(e) => {
            return respond(
                stream,
                "403 Forbidden",
                "text/plain",
                "",
                e.as_bytes(),
                head_only,
            )
            .await;
        }
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(entry.size);
    let (status, start, len, extra) = match req.range.as_deref().and_then(|r| byte_range(r, size)) {
        Some((start, len)) => (
            "206 Partial Content",
            start,
            len,
            format!(
                "Content-Range: bytes {start}-{}/{size}\r\n",
                start + len - 1
            ),
        ),
        None => ("200 OK", 0, size, String::new()),
    };
    let name = req.path.rsplit(|&b| b == b'/').next().unwrap_or(&[]);
    let head = format!(
        "HTTP/1.1 {status}\r\nServer: EverythingOS\r\nContent-Type: {}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nContent-Disposition: inline; filename*=UTF-8''{}\r\nConnection: close\r\n{extra}\r\n",
        content_type(&req.path),
        escape_url(name).replace('/', "%2F")
    );
    stream.write_all(head.as_bytes()).await?;
    if !head_only {
        let mut file = tokio::fs::File::from_std(file);
        file.seek(std::io::SeekFrom::Start(start)).await?;
        tokio::io::copy(&mut file.take(len), stream).await?;
    }
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_escapes() {
        let req = parse_head("GET /?search=%D7%A9%D7%9C%D7%95%D7%9D+world&j=1&C=5 HTTP/1.1\r\nAuthorization: Basic dXNlcjpwYXNz\r\nRange: bytes=0-9\r\n\r\n").unwrap();
        assert_eq!(req.params["search"], "שלום world");
        assert_eq!(req.params["c"], "5", "keys are case-insensitive");
        assert_eq!(base64_decode("dXNlcjpwYXNz").unwrap(), b"user:pass");
        assert_eq!(req.range.as_deref(), Some("bytes=0-9"));
        let req = parse_head("GET /home/a%20b/c+d.txt HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(req.path, b"/home/a b/c+d.txt", "+ is literal in paths");
        assert_eq!(escape_url("/a b/ש".as_bytes()), "/a%20b/%D7%A9");
        assert_eq!(escape_html("<a&\">"), "&lt;a&amp;&quot;&gt;");
        assert_eq!(escape_json("a\"b\\c\n"), "a\\\"b\\\\c\\n");
    }

    #[test]
    fn ranges() {
        assert_eq!(byte_range("bytes=0-9", 100), Some((0, 10)));
        assert_eq!(byte_range("bytes=90-", 100), Some((90, 10)));
        assert_eq!(byte_range("bytes=-5", 100), Some((95, 5)));
        assert_eq!(byte_range("bytes=50-500", 100), Some((50, 50)));
        assert_eq!(byte_range("bytes=200-", 100), None);
        assert_eq!(byte_range("items=1-2", 100), None);
    }

    #[test]
    fn logins() {
        let mut s = HttpServer::default();
        assert!(authorized(&s, None), "open server");
        s.username = "user".into();
        s.password = Some(crate::accounts::hash_password("pass"));
        assert!(authorized(&s, Some("Basic dXNlcjpwYXNz")));
        assert!(!authorized(&s, Some("Basic dXNlcjp3cm9uZw==")));
        assert!(!authorized(&s, None));
    }
}
