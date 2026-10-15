//! Formatting of results, status and statistics for the terminal and for
//! export files.

use std::io::{self, Write};

use chrono::{Local, TimeZone, Utc};
use evos_proto::{DaemonStatus, Row, VolumeStatistics};
use serde::Serialize;

use crate::es::Column;
use crate::filelist;

/// Output formats, as es names them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Txt,
    Csv,
    Tsv,
    Efu,
    M3u,
    M3u8,
    /// JSON lines with every column.
    Json,
}

/// `-size-format`: 0 automatic, 1 bytes, 2 KB, 3 MB.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SizeFormat {
    Auto,
    #[default]
    Bytes,
    Kb,
    Mb,
}

/// `-date-format`: 0 local, 1 ISO 8601, 2 FILETIME, 3 ISO 8601 in UTC.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DateFormat {
    #[default]
    Local,
    Iso,
    FileTime,
    IsoUtc,
}

pub struct Style {
    pub format: Format,
    pub columns: Vec<Column>,
    pub size_format: SizeFormat,
    pub date_format: DateFormat,
    pub header: bool,
    pub double_quote: bool,
    /// Separate text results with NUL.
    pub null: bool,
    /// Terms to highlight with terminal colors.
    pub highlight: Option<Vec<String>>,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            format: Format::Txt,
            columns: vec![Column::FullPath],
            size_format: SizeFormat::Bytes,
            date_format: DateFormat::Local,
            header: true,
            double_quote: false,
            null: false,
            highlight: None,
        }
    }
}

const HIGHLIGHT_ON: &str = "\x1b[1;32m";
const HIGHLIGHT_OFF: &str = "\x1b[0m";

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

pub fn local_time(ns: i64) -> chrono::DateTime<Local> {
    Local
        .timestamp_opt(
            ns.div_euclid(1_000_000_000),
            ns.rem_euclid(1_000_000_000) as u32,
        )
        .single()
        .unwrap_or_default()
}

impl Style {
    fn size(&self, size: Option<u64>) -> String {
        let Some(s) = size else {
            return String::new();
        };
        match self.size_format {
            SizeFormat::Auto => human_size(s),
            SizeFormat::Bytes => s.to_string(),
            // Everything rounds up, so a 1-byte file is 1 KB.
            SizeFormat::Kb => format!("{} KB", s.div_ceil(1024)),
            SizeFormat::Mb => format!("{} MB", s.div_ceil(1 << 20)),
        }
    }

    fn date(&self, ns: i64) -> String {
        if ns == 0 {
            return String::new();
        }
        match self.date_format {
            DateFormat::Local => local_time(ns).format("%Y-%m-%d %H:%M").to_string(),
            DateFormat::Iso => local_time(ns).format("%Y-%m-%dT%H:%M:%S").to_string(),
            DateFormat::FileTime => filelist::to_filetime(ns).to_string(),
            DateFormat::IsoUtc => Utc
                .timestamp_opt(ns.div_euclid(1_000_000_000), 0)
                .single()
                .unwrap_or_default()
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string(),
        }
    }

    /// The raw value of a column.
    fn value(&self, row: &Row, column: Column) -> Vec<u8> {
        match column {
            Column::Name => row.name.clone(),
            Column::Path => row.folder.clone(),
            Column::FullPath => row.path(),
            Column::Extension if row.is_dir => Vec::new(),
            Column::Extension => match row.name.iter().rposition(|&b| b == b'.') {
                Some(dot) if dot > 0 => row.name[dot + 1..].to_vec(),
                _ => Vec::new(),
            },
            Column::Size => self.size(row.size).into_bytes(),
            Column::DateCreated => self.date(row.created).into_bytes(),
            Column::DateModified => self.date(row.mtime).into_bytes(),
            Column::DateAccessed => self.date(row.accessed).into_bytes(),
            Column::DateRecentlyChanged => self.date(row.changed).into_bytes(),
            Column::DateRun => self.date(row.date_run).into_bytes(),
            Column::Attributes => row.attributes.clone().into_bytes(),
            Column::RunCount => row.run_count.to_string().into_bytes(),
            Column::Type => row.type_name.clone().into_bytes(),
        }
    }
}

fn is_text_column(c: Column) -> bool {
    matches!(
        c,
        Column::Name | Column::Path | Column::FullPath | Column::Extension | Column::Type
    )
}

/// Streams results in one format.
pub struct Writer<W: Write> {
    out: W,
    style: Style,
    started: bool,
}

#[derive(Serialize)]
struct JsonRow<'a> {
    path: String,
    name: &'a str,
    folder: &'a str,
    is_dir: bool,
    size: Option<u64>,
    /// RFC 3339 local times; null when unknown.
    date_modified: Option<String>,
    date_created: Option<String>,
    date_accessed: Option<String>,
    date_recently_changed: Option<String>,
    date_run: Option<String>,
    attributes: &'a str,
    #[serde(rename = "type")]
    type_name: &'a str,
    run_count: u32,
    offline: bool,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W, style: Style) -> Self {
        Writer {
            out,
            style,
            started: false,
        }
    }

    fn start(&mut self) -> io::Result<()> {
        if self.started {
            return Ok(());
        }
        self.started = true;
        let titles: Vec<&str> = self.style.columns.iter().map(|c| c.title()).collect();
        match self.style.format {
            Format::Csv if self.style.header => write!(self.out, "{}\r\n", titles.join(",")),
            Format::Tsv if self.style.header => writeln!(self.out, "{}", titles.join("\t")),
            Format::Efu if self.style.header => write!(self.out, "{}\r\n", filelist::HEADER),
            Format::M3u8 => writeln!(self.out, "#EXTM3U"),
            _ => Ok(()),
        }
    }

    /// Writes the header (if any) even when there are no results.
    pub fn finish(mut self) -> io::Result<W> {
        self.start()?;
        self.out.flush()?;
        Ok(self.out)
    }

    pub fn row(&mut self, row: &Row) -> io::Result<()> {
        self.start()?;
        match self.style.format {
            Format::Txt => self.txt(row),
            Format::Csv => {
                for (i, &c) in self.style.columns.iter().enumerate() {
                    if i > 0 {
                        self.out.write_all(b",")?;
                    }
                    let v = self.style.value(row, c);
                    if is_text_column(c) {
                        filelist::write_quoted(&mut self.out, &v)?;
                    } else {
                        self.out.write_all(&v)?;
                    }
                }
                self.out.write_all(b"\r\n")
            }
            Format::Tsv => {
                let fields: Vec<Vec<u8>> = self
                    .style
                    .columns
                    .iter()
                    .map(|&c| {
                        let mut v = self.style.value(row, c);
                        // Tabs and line breaks would break the format.
                        for b in &mut v {
                            if matches!(*b, b'\t' | b'\n' | b'\r') {
                                *b = b' ';
                            }
                        }
                        v
                    })
                    .collect();
                self.out.write_all(&fields.join(&b'\t'))?;
                self.out.write_all(b"\n")
            }
            Format::Efu => filelist::write_row(&mut self.out, row),
            Format::M3u | Format::M3u8 => {
                self.out.write_all(&row.path())?;
                self.out.write_all(b"\n")
            }
            Format::Json => self.json(row),
        }
    }

    fn txt(&mut self, row: &Row) -> io::Result<()> {
        let columns = self.style.columns.clone();
        for (i, &c) in columns.iter().enumerate() {
            if i > 0 {
                self.out.write_all(b"  ")?;
            }
            let v = self.style.value(row, c);
            match c {
                Column::Size => {
                    let text = match (&row.size, row.is_dir) {
                        (None, true) => "<DIR>".to_owned(),
                        _ => String::from_utf8_lossy(&v).into_owned(),
                    };
                    write!(self.out, "{text:>12}")?;
                }
                Column::RunCount => write!(self.out, "{:>5}", String::from_utf8_lossy(&v))?,
                Column::Attributes => write!(self.out, "{:<4}", String::from_utf8_lossy(&v))?,
                Column::DateCreated
                | Column::DateModified
                | Column::DateAccessed
                | Column::DateRecentlyChanged
                | Column::DateRun => {
                    let width = match self.style.date_format {
                        DateFormat::Local => 16,
                        DateFormat::Iso => 19,
                        DateFormat::FileTime => 18,
                        DateFormat::IsoUtc => 20,
                    };
                    write!(self.out, "{:<width$}", String::from_utf8_lossy(&v))?;
                }
                _ => {
                    let quote = self.style.double_quote && is_text_column(c);
                    if quote {
                        self.out.write_all(b"\"")?;
                    }
                    self.highlighted(&v)?;
                    if quote {
                        self.out.write_all(b"\"")?;
                    }
                }
            }
        }
        self.out
            .write_all(if self.style.null { b"\0" } else { b"\n" })
    }

    /// Writes text with the search terms in color. Raw bytes are written
    /// as they are: Linux names need not be valid UTF-8.
    fn highlighted(&mut self, v: &[u8]) -> io::Result<()> {
        let (Some(terms), Ok(text)) = (&self.style.highlight, std::str::from_utf8(v)) else {
            return self.out.write_all(v);
        };
        let mut at = 0;
        for range in evos_core::fold::highlight(text, terms) {
            self.out.write_all(&text.as_bytes()[at..range.start])?;
            write!(
                self.out,
                "{HIGHLIGHT_ON}{}{HIGHLIGHT_OFF}",
                &text[range.clone()]
            )?;
            at = range.end;
        }
        self.out.write_all(&text.as_bytes()[at..])
    }

    fn json(&mut self, row: &Row) -> io::Result<()> {
        let name = String::from_utf8_lossy(&row.name);
        let folder = String::from_utf8_lossy(&row.folder);
        let date = |ns: i64| (ns != 0).then(|| local_time(ns).to_rfc3339());
        let json = JsonRow {
            path: String::from_utf8_lossy(&row.path()).into_owned(),
            name: &name,
            folder: &folder,
            is_dir: row.is_dir,
            size: row.size,
            date_modified: date(row.mtime),
            date_created: date(row.created),
            date_accessed: date(row.accessed),
            date_recently_changed: date(row.changed),
            date_run: date(row.date_run),
            attributes: &row.attributes,
            type_name: &row.type_name,
            run_count: row.run_count,
            offline: row.offline,
        };
        serde_json::to_writer(&mut self.out, &json)?;
        self.out.write_all(b"\n")
    }
}

fn ago(secs: Option<i64>) -> String {
    let Some(t) = secs else { return "-".into() };
    let now = Utc::now().timestamp();
    let d = (now - t).max(0);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}

pub fn write_status(out: &mut impl Write, status: &DaemonStatus) -> io::Result<()> {
    writeln!(out, "evosd {}", status.version)?;
    if status.volumes.is_empty() {
        return writeln!(out, "no volumes are indexed");
    }
    let width = status
        .volumes
        .iter()
        .map(|v| v.root.len())
        .max()
        .unwrap_or(6)
        .max(6);
    writeln!(
        out,
        "{:<width$}  {:<10} {:<11} {:<16} {:>12}  {:<10} DETAILS",
        "VOLUME", "FS", "STATE", "WATCH", "ENTRIES", "CHANGED"
    )?;
    for v in &status.volumes {
        let mut details = Vec::new();
        if v.phase == "scanning" {
            details.push(format!(
                "{} folders listed, {} queued",
                v.dirs_listed, v.pending_dirs
            ));
        }
        if let Some(c) = v.concurrency {
            details.push(format!(
                "{c} parallel, {:.0} ops/s, {} folders tracked",
                v.ops_per_sec.unwrap_or(0.0),
                v.tracked_dirs
            ));
        }
        if let Some(e) = &v.error {
            details.push(e.clone());
        }
        writeln!(
            out,
            "{:<width$}  {:<10} {:<11} {:<16} {:>12}  {:<10} {}",
            String::from_utf8_lossy(&v.root),
            v.fs_type,
            v.phase,
            v.watch,
            v.entries,
            ago(v.last_change),
            details.join("; ")
        )?;
    }
    Ok(())
}

pub fn write_statistics(out: &mut impl Write, stats: &[VolumeStatistics]) -> io::Result<()> {
    let width = stats.iter().map(|v| v.root.len()).max().unwrap_or(6).max(6);
    let line = |out: &mut dyn Write, cells: [&str; 6]| {
        writeln!(
            out,
            "{:<width$}  {:<10} {:>12} {:>12} {:>12} {:>12}",
            cells[0], cells[1], cells[2], cells[3], cells[4], cells[5]
        )
    };
    line(out, ["VOLUME", "FS", "FILES", "FOLDERS", "SIZE", "INDEX"])?;
    let (mut files, mut folders, mut size, mut index) = (0, 0, 0, 0);
    for v in stats {
        line(
            out,
            [
                &String::from_utf8_lossy(&v.root),
                &v.fs_type,
                &v.files.to_string(),
                &v.folders.to_string(),
                &human_size(v.total_size),
                &human_size(v.index_bytes),
            ],
        )?;
        files += v.files;
        folders += v.folders;
        size += v.total_size;
        index += v.index_bytes;
    }
    line(
        out,
        [
            "TOTAL",
            "",
            &files.to_string(),
            &folders.to_string(),
            &human_size(size),
            &human_size(index),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        Row {
            name: b"a b.txt".to_vec(),
            folder: b"/home/me".to_vec(),
            is_dir: false,
            size: Some(2048),
            mtime: 1_700_000_000_000_000_000,
            attributes: "A".into(),
            type_name: "Text Document".into(),
            run_count: 3,
            ..Default::default()
        }
    }

    fn render(style: Style, rows: &[Row]) -> String {
        let mut w = Writer::new(Vec::new(), style);
        for r in rows {
            w.row(r).unwrap();
        }
        String::from_utf8(w.finish().unwrap()).unwrap()
    }

    #[test]
    fn plain_and_columns() {
        assert_eq!(render(Style::default(), &[row()]), "/home/me/a b.txt\n");
        let cols = Style {
            columns: vec![Column::Size, Column::Name, Column::Extension],
            size_format: SizeFormat::Auto,
            null: true,
            ..Default::default()
        };
        assert_eq!(render(cols, &[row()]), "     2.0 KiB  a b.txt  txt\0");
        let kb = Style {
            columns: vec![Column::Size, Column::RunCount, Column::Attributes],
            size_format: SizeFormat::Kb,
            ..Default::default()
        };
        assert_eq!(render(kb, &[row()]), "        2 KB      3  A   \n");
    }

    #[test]
    fn csv_tsv_efu_and_m3u() {
        let csv = Style {
            format: Format::Csv,
            columns: vec![Column::FullPath, Column::Size, Column::Type],
            ..Default::default()
        };
        assert_eq!(
            render(csv, &[row()]),
            "Filename,Size,Type\r\n\"/home/me/a b.txt\",2048,\"Text Document\"\r\n"
        );
        let tsv = Style {
            format: Format::Tsv,
            columns: vec![Column::Name, Column::Path],
            header: false,
            ..Default::default()
        };
        assert_eq!(render(tsv, &[row()]), "a b.txt\t/home/me\n");
        let efu = Style {
            format: Format::Efu,
            ..Default::default()
        };
        let text = render(efu, &[row()]);
        assert!(text.starts_with("Filename,Size,Date Modified,Date Created,Attributes\r\n"));
        assert!(text.contains("\"/home/me/a b.txt\",2048,"));
        assert!(text.ends_with(",,32\r\n"), "unknown creation date, archive");
        let m3u8 = Style {
            format: Format::M3u8,
            ..Default::default()
        };
        assert_eq!(render(m3u8, &[row()]), "#EXTM3U\n/home/me/a b.txt\n");
        let empty = Style {
            format: Format::Csv,
            ..Default::default()
        };
        assert_eq!(render(empty, &[]), "Filename\r\n", "header without results");
    }

    #[test]
    fn json_lines() {
        let json = Style {
            format: Format::Json,
            ..Default::default()
        };
        let v: serde_json::Value = serde_json::from_str(&render(json, &[row()])).unwrap();
        assert_eq!(v["path"], "/home/me/a b.txt");
        assert_eq!(v["size"], 2048);
        assert_eq!(v["type"], "Text Document");
        assert_eq!(v["run_count"], 3);
        assert!(v["date_created"].is_null());
    }

    #[test]
    fn highlighting_and_quotes() {
        let style = Style {
            columns: vec![Column::Name],
            highlight: Some(vec!["B".into()]),
            double_quote: true,
            ..Default::default()
        };
        assert_eq!(render(style, &[row()]), "\"a \x1b[1;32mb\x1b[0m.txt\"\n");
    }

    #[test]
    fn dates_and_sizes() {
        let s = Style {
            date_format: DateFormat::IsoUtc,
            ..Default::default()
        };
        assert_eq!(s.date(1_700_000_000_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(s.date(0), "", "unknown");
        let s = Style {
            date_format: DateFormat::FileTime,
            ..Default::default()
        };
        assert_eq!(s.date(1_700_000_000_000_000_000), "133444736000000000");
        assert_eq!(human_size(1536), "1.5 KiB");
    }
}
