//! Everything's ETP protocol: FTP with the `EVERYTHING` extension, which
//! Everything uses to search another computer's index ("Connect to ETP
//! Server").
//!
//! The wire format follows voidtools' ETP server plugin
//! (<https://github.com/voidtools/etp_server>, MIT). A client logs in with
//! `USER`/`PASS`, sets search options with `EVERYTHING <OPTION> <value>`
//! (or `SITE EVERYTHING ...`), each answered with `200 ...`, and runs
//! `EVERYTHING QUERY`, answered on the control connection:
//!
//! ```text
//! 200-Query results
//!  RESULT_COUNT 2
//!  PATH C:\Users\me            (optional columns, in this order)
//!  ATTRIBUTES 32
//!  SIZE 1234                    (2^64-1 for folders without a size)
//!  DATE_MODIFIED 133444736000000000
//!  DATE_CREATED 133444736000000000
//!  FILE_LIST_FILENAME ...
//!  DATE_RECENTLY_CHANGED ...
//!  FILE notes.txt               (or FOLDER name; ends each result)
//! 200 End.
//! ```
//!
//! Dates are Windows FILETIMEs and attributes Windows attribute bits.
//! Plain FTP (LIST, RETR, ...) browses and downloads files.

mod client;

pub use client::{EtpClient, EtpError};

use evos_proto::{efu, Row, SortKey};

/// Everything's default ETP/FTP port.
pub const DEFAULT_PORT: u16 = 21;

/// Size and date value meaning "unknown".
pub const UNKNOWN: u64 = u64::MAX;

pub const ATTR_REPARSE_POINT: u32 = 0x400;

/// Unix nanoseconds (0 = unknown) to an ETP date.
pub fn date_to_wire(ns: i64) -> u64 {
    if ns == 0 {
        UNKNOWN
    } else {
        efu::to_filetime(ns).max(0) as u64
    }
}

/// An ETP date to Unix nanoseconds (0 = unknown).
pub fn date_from_wire(ft: u64) -> i64 {
    if ft == UNKNOWN || ft == 0 {
        0
    } else {
        efu::from_filetime(ft.min(i64::MAX as u64) as i64)
    }
}

/// Windows attribute bits of a row (directory, archive, read-only,
/// hidden, reparse point for symbolic links).
pub fn attributes_to_wire(row: &Row) -> u32 {
    let mut bits = efu::attribute_bits(row);
    if row.attributes.contains('L') {
        bits |= ATTR_REPARSE_POINT;
    }
    bits
}

/// Attribute letters, as EverythingOS shows them, from Windows bits.
pub fn attributes_from_wire(bits: u32, is_dir: bool) -> String {
    let mut s = String::new();
    if bits & efu::ATTR_READONLY != 0 {
        s.push('R');
    }
    if bits & efu::ATTR_HIDDEN != 0 {
        s.push('H');
    }
    if is_dir || bits & efu::ATTR_DIRECTORY != 0 {
        s.push('D');
    }
    if bits & efu::ATTR_ARCHIVE != 0 {
        s.push('A');
    }
    if bits & ATTR_REPARSE_POINT != 0 {
        s.push('L');
    }
    s
}

/// The ETP sort name of a sort key.
pub fn sort_name(key: SortKey, descending: bool) -> String {
    let base = match key {
        SortKey::Name => "NAME",
        SortKey::Path => "PATH",
        SortKey::Size => "SIZE",
        SortKey::Extension => "EXTENSION",
        SortKey::Type => "TYPE",
        SortKey::Modified => "DATE_MODIFIED",
        SortKey::Created => "DATE_CREATED",
        SortKey::Accessed => "DATE_ACCESSED",
        SortKey::Attributes => "ATTRIBUTES",
        SortKey::RecentlyChanged => "DATE_RECENTLY_CHANGED",
        SortKey::RunCount => "RUN_COUNT",
        SortKey::DateRun => "DATE_RUN",
    };
    format!(
        "{base}_{}",
        if descending {
            "DESCENDING"
        } else {
            "ASCENDING"
        }
    )
}

/// Parses an ETP sort name (case-insensitive), including Everything's
/// `INVERSE_SIZE_*` and its client's `DATE_CREATEDDESCENDING` spelling.
pub fn parse_sort(name: &str) -> Option<(SortKey, bool)> {
    let name = name.trim().to_ascii_lowercase();
    if name == "date_createddescending" {
        return Some((SortKey::Created, true));
    }
    let (base, descending) = if let Some(b) = name.strip_suffix("_ascending") {
        (b, false)
    } else {
        (name.strip_suffix("_descending")?, true)
    };
    let key = match base {
        "name" => SortKey::Name,
        "path" => SortKey::Path,
        "size" => SortKey::Size,
        // Everything's inverse size sorts are the opposite direction.
        "inverse_size" => return Some((SortKey::Size, !descending)),
        "extension" => SortKey::Extension,
        "type" => SortKey::Type,
        "date_modified" => SortKey::Modified,
        "date_created" => SortKey::Created,
        "date_accessed" => SortKey::Accessed,
        "attributes" => SortKey::Attributes,
        "date_recently_changed" => SortKey::RecentlyChanged,
        "run_count" => SortKey::RunCount,
        "date_run" => SortKey::DateRun,
        // File lists are not part of an index: sort by name instead.
        "file_list_filename" => SortKey::Name,
        _ => return None,
    };
    Some((key, descending))
}

/// Optional result columns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Columns {
    pub path: bool,
    pub attributes: bool,
    pub size: bool,
    pub date_modified: bool,
    pub date_created: bool,
    pub file_list_filename: bool,
    pub date_recently_changed: bool,
}

impl Columns {
    pub fn all() -> Self {
        Columns {
            path: true,
            attributes: true,
            size: true,
            date_modified: true,
            date_created: true,
            file_list_filename: false,
            date_recently_changed: true,
        }
    }
}

/// One result of a query.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Item {
    pub is_folder: bool,
    pub name: String,
    /// The containing folder as the server writes it (`C:\Users` on
    /// Windows, `/home/me` on Linux).
    pub path: Option<String>,
    pub attributes: Option<u32>,
    pub size: Option<u64>,
    pub date_modified: Option<u64>,
    pub date_created: Option<u64>,
    pub file_list_filename: Option<String>,
    pub date_recently_changed: Option<u64>,
}

/// The lines of one result, as the server sends them.
pub fn format_item(item: &Item, columns: &Columns, out: &mut String) {
    use std::fmt::Write;
    if columns.path {
        let _ = write!(out, " PATH {}\r\n", item.path.as_deref().unwrap_or(""));
    }
    if columns.attributes {
        let _ = write!(out, " ATTRIBUTES {}\r\n", item.attributes.unwrap_or(0));
    }
    if columns.size {
        let _ = write!(out, " SIZE {}\r\n", item.size.unwrap_or(UNKNOWN));
    }
    if columns.date_modified {
        let _ = write!(
            out,
            " DATE_MODIFIED {}\r\n",
            item.date_modified.unwrap_or(UNKNOWN)
        );
    }
    if columns.date_created {
        let _ = write!(
            out,
            " DATE_CREATED {}\r\n",
            item.date_created.unwrap_or(UNKNOWN)
        );
    }
    if columns.file_list_filename {
        let _ = write!(
            out,
            " FILE_LIST_FILENAME {}\r\n",
            item.file_list_filename.as_deref().unwrap_or("")
        );
    }
    if columns.date_recently_changed {
        let _ = write!(
            out,
            " DATE_RECENTLY_CHANGED {}\r\n",
            item.date_recently_changed.unwrap_or(UNKNOWN)
        );
    }
    let _ = write!(
        out,
        " {} {}\r\n",
        if item.is_folder { "FOLDER" } else { "FILE" },
        item.name
    );
}

/// A whole query reply.
pub fn format_reply(total: u64, items: &[Item], columns: &Columns) -> String {
    let mut out = format!("200-Query results\r\n RESULT_COUNT {total}\r\n");
    for item in items {
        format_item(item, columns, &mut out);
    }
    out.push_str("200 End.\r\n");
    out
}

/// A parsed query reply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reply {
    pub total: u64,
    pub items: Vec<Item>,
}

/// Parses the body lines of a query reply (between `200-` and `200 `),
/// with or without the leading space.
pub fn parse_reply_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Reply {
    let mut reply = Reply::default();
    let mut item = Item::default();
    for line in lines {
        let line = line.strip_prefix(' ').unwrap_or(line);
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        let num = || value.trim().parse::<u64>().ok();
        match key.to_ascii_uppercase().as_str() {
            "RESULT_COUNT" => reply.total = num().unwrap_or(0),
            "PATH" => item.path = Some(value.to_owned()),
            "ATTRIBUTES" => item.attributes = num().map(|v| v as u32),
            "SIZE" => item.size = num().filter(|&v| v != UNKNOWN),
            "DATE_MODIFIED" => item.date_modified = num(),
            "DATE_CREATED" => item.date_created = num(),
            "FILE_LIST_FILENAME" => item.file_list_filename = Some(value.to_owned()),
            "DATE_RECENTLY_CHANGED" => item.date_recently_changed = num(),
            "FILE" | "FOLDER" => {
                item.is_folder = key.eq_ignore_ascii_case("FOLDER");
                item.name = value.to_owned();
                reply.items.push(std::mem::take(&mut item));
            }
            _ => {}
        }
    }
    reply
}

/// True for paths written the Windows way (`C:\...`, `\\server\...`).
pub fn is_windows_path(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) || path.starts_with("\\\\")
}

/// The FTP path of a result: Everything serves `C:\a\b` as `/C:/a/b`; a
/// Linux server serves its paths as they are.
pub fn ftp_path(folder: &str, name: &str) -> String {
    if is_windows_path(folder) {
        let mut p = format!("/{}", folder.trim_start_matches('\\').replace('\\', "/"));
        if !p.ends_with('/') {
            p.push('/');
        }
        p.push_str(name);
        p
    } else if folder.ends_with('/') {
        format!("{folder}{name}")
    } else {
        format!("{folder}/{name}")
    }
}

/// A result as a row of EverythingOS's result list.
pub fn item_to_row(item: &Item) -> Row {
    let folder = item.path.clone().unwrap_or_default();
    let date = |v: Option<u64>| v.map_or(0, date_from_wire);
    let attributes = item
        .attributes
        .map(|bits| attributes_from_wire(bits, item.is_folder))
        .unwrap_or_else(|| if item.is_folder { "D" } else { "A" }.into());
    Row {
        type_name: evos_core::types::type_name(item.name.as_bytes(), item.is_folder, false),
        name: item.name.clone().into_bytes(),
        folder: folder.into_bytes(),
        is_dir: item.is_folder,
        size: item.size,
        mtime: date(item.date_modified),
        created: date(item.date_created),
        accessed: 0,
        changed: date(item.date_recently_changed),
        attributes,
        run_count: 0,
        date_run: 0,
        offline: false,
    }
}

/// A row as a result to send.
pub fn row_to_item(row: &Row) -> Item {
    // The containing folder, or for a volume root the folder above it.
    let path = if row.folder.is_empty() {
        let p = String::from_utf8_lossy(&row.name).into_owned();
        match p.rfind('/') {
            Some(0) | None => "/".to_owned(),
            Some(i) => p[..i].to_owned(),
        }
    } else {
        String::from_utf8_lossy(&row.folder).into_owned()
    };
    let name = if row.folder.is_empty() {
        let p = String::from_utf8_lossy(&row.name).into_owned();
        p.rsplit('/').next().unwrap_or("").to_owned()
    } else {
        String::from_utf8_lossy(&row.name).into_owned()
    };
    Item {
        is_folder: row.is_dir,
        name,
        path: Some(path),
        attributes: Some(attributes_to_wire(row)),
        size: row.size,
        date_modified: Some(date_to_wire(row.mtime)),
        date_created: Some(date_to_wire(row.created)),
        file_list_filename: Some(String::new()),
        date_recently_changed: Some(date_to_wire(row.changed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exactly what voidtools' server sends (etp_server.c,
    /// etp_server_send_query_results).
    const EVERYTHING_REPLY: &str = "200-Query results\r\n RESULT_COUNT 2\r\n PATH C:\\Users\\me\r\n ATTRIBUTES 16\r\n SIZE 18446744073709551615\r\n DATE_MODIFIED 133444736000000000\r\n FOLDER Documents\r\n PATH C:\\Users\\me\\Documents\r\n ATTRIBUTES 33\r\n SIZE 1234\r\n DATE_MODIFIED 133444736000000000\r\n FILE מסמך.txt\r\n200 End.\r\n";

    #[test]
    fn parses_everything_replies() {
        let body: Vec<&str> = EVERYTHING_REPLY
            .split("\r\n")
            .filter(|l| !l.starts_with("200"))
            .collect();
        let reply = parse_reply_lines(body);
        assert_eq!(reply.total, 2);
        assert_eq!(reply.items.len(), 2);
        let folder = &reply.items[0];
        assert!(folder.is_folder);
        assert_eq!(folder.size, None, "2^64-1 is no size");
        let file = item_to_row(&reply.items[1]);
        assert_eq!(file.name, "מסמך.txt".as_bytes());
        assert_eq!(file.folder, b"C:\\Users\\me\\Documents");
        assert_eq!(
            file.path(),
            b"C:\\Users\\me\\Documents\\\xd7\x9e\xd7\xa1\xd7\x9e\xd7\x9a.txt"
        );
        assert_eq!(file.size, Some(1234));
        assert_eq!(file.mtime, 1_700_000_000_000_000_000);
        assert_eq!(file.attributes, "RA");
        assert_eq!(file.type_name, "Text Document");
    }

    #[test]
    fn replies_roundtrip() {
        let row = Row {
            name: b"a b.txt".to_vec(),
            folder: b"/home/me".to_vec(),
            size: Some(7),
            mtime: 1_700_000_000_000_000_000,
            attributes: "A".into(),
            ..Default::default()
        };
        let item = row_to_item(&row);
        let text = format_reply(1, std::slice::from_ref(&item), &Columns::all());
        assert!(text.starts_with("200-Query results\r\n RESULT_COUNT 1\r\n PATH /home/me\r\n"));
        assert!(
            text.contains(" DATE_CREATED 18446744073709551615\r\n"),
            "unknown"
        );
        assert!(text.ends_with(" FILE a b.txt\r\n200 End.\r\n"));
        let lines: Vec<&str> = text
            .split("\r\n")
            .filter(|l| !l.starts_with("200"))
            .collect();
        let back = item_to_row(&parse_reply_lines(lines).items[0]);
        assert_eq!(back.path(), b"/home/me/a b.txt");
        assert_eq!(
            (back.size, back.mtime, back.created),
            (Some(7), row.mtime, 0)
        );
    }

    #[test]
    fn sorts_and_paths() {
        for key in SortKey::ALL {
            for desc in [false, true] {
                assert_eq!(parse_sort(&sort_name(key, desc)), Some((key, desc)));
            }
        }
        assert_eq!(
            parse_sort("inverse_size_ascending"),
            Some((SortKey::Size, true))
        );
        assert_eq!(
            parse_sort("DATE_CREATEDDESCENDING"),
            Some((SortKey::Created, true))
        );
        assert_eq!(parse_sort("bogus"), None);
        assert_eq!(ftp_path("C:\\Users\\me", "a.txt"), "/C:/Users/me/a.txt");
        assert_eq!(ftp_path("/home/me", "a.txt"), "/home/me/a.txt");
        assert_eq!(ftp_path("/", "etc"), "/etc");
        let root = Row {
            name: b"/mnt/nas".to_vec(),
            is_dir: true,
            ..Default::default()
        };
        let item = row_to_item(&root);
        assert_eq!(
            (item.path.as_deref(), item.name.as_str()),
            (Some("/mnt"), "nas")
        );
    }
}
