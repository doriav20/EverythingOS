//! Formatting of results and status for the terminal.

use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

use chrono::{Local, TimeZone};
use evos_proto::{DaemonStatus, Row};
use serde::Serialize;

pub struct Style {
    pub size: bool,
    pub dm: bool,
    pub human: bool,
    pub json: bool,
    pub null: bool,
}

#[derive(Serialize)]
struct JsonRow<'a> {
    path: String,
    name: &'a str,
    folder: &'a str,
    is_dir: bool,
    size: Option<u64>,
    /// RFC 3339 local time.
    modified: String,
    offline: bool,
}

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

pub fn write_row(out: &mut impl Write, row: &Row, style: &Style) -> io::Result<()> {
    if style.json {
        let name = String::from_utf8_lossy(&row.name);
        let folder = String::from_utf8_lossy(&row.folder);
        let json = JsonRow {
            path: String::from_utf8_lossy(&row.path()).into_owned(),
            name: &name,
            folder: &folder,
            is_dir: row.is_dir,
            size: row.size,
            modified: local_time(row.mtime).to_rfc3339(),
            offline: row.offline,
        };
        serde_json::to_writer(&mut *out, &json)?;
        return out.write_all(b"\n");
    }
    if style.size {
        let size = match row.size {
            None => "<DIR>".to_owned(),
            Some(s) if style.human => human_size(s),
            Some(s) => s.to_string(),
        };
        write!(out, "{size:>12}  ")?;
    }
    if style.dm {
        write!(out, "{}  ", local_time(row.mtime).format("%Y-%m-%d %H:%M"))?;
    }
    // Raw bytes: Linux paths need not be valid UTF-8.
    out.write_all(std::ffi::OsStr::from_bytes(&row.path()).as_bytes())?;
    out.write_all(if style.null { b"\0" } else { b"\n" })
}

fn ago(secs: Option<i64>) -> String {
    let Some(t) = secs else { return "-".into() };
    let now = chrono::Utc::now().timestamp();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> Row {
        Row {
            name: b"a b.txt".to_vec(),
            folder: b"/home/me".to_vec(),
            is_dir: false,
            size: Some(2048),
            ..Default::default()
        }
    }

    fn render(style: Style) -> String {
        let mut out = Vec::new();
        write_row(&mut out, &row(), &style).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn plain_and_columns() {
        let plain = Style {
            size: false,
            dm: false,
            human: false,
            json: false,
            null: false,
        };
        assert_eq!(render(plain), "/home/me/a b.txt\n");
        let cols = Style {
            size: true,
            dm: false,
            human: true,
            json: false,
            null: true,
        };
        assert_eq!(render(cols), "     2.0 KiB  /home/me/a b.txt\0");
    }

    #[test]
    fn json_lines() {
        let json = Style {
            size: false,
            dm: false,
            human: false,
            json: true,
            null: false,
        };
        let v: serde_json::Value = serde_json::from_str(&render(json)).unwrap();
        assert_eq!(v["path"], "/home/me/a b.txt");
        assert_eq!(v["size"], 2048);
        assert_eq!(v["is_dir"], false);
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(5 << 30), "5.0 GiB");
    }
}
