//! Everything file lists (`.efu`): CSV files of full paths with size,
//! dates and attributes. `evos` writes them from search results and can
//! search one directly without the daemon, e.g. a catalog of a drive that
//! is not connected.

use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use evos_core::query::{highlight_terms, parse, ParseOptions};
use evos_core::search::{attributes_of, Extra, Query};
use evos_core::sort::{sort_hits_with, SortSpec};
use evos_core::store::mode;
use evos_core::types::type_name;
use evos_core::{EntryId, Index, Meta, ROOT};
use evos_proto::Row;

pub const HEADER: &str = "Filename,Size,Date Modified,Date Created,Attributes";

/// 100 ns intervals between 1601-01-01 (FILETIME) and 1970-01-01.
const FILETIME_UNIX: i64 = 116_444_736_000_000_000;

const ATTR_READONLY: u32 = 0x1;
const ATTR_HIDDEN: u32 = 0x2;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_ARCHIVE: u32 = 0x20;

pub fn to_filetime(ns: i64) -> i64 {
    ns.div_euclid(100) + FILETIME_UNIX
}

pub fn from_filetime(ft: i64) -> i64 {
    (ft - FILETIME_UNIX).saturating_mul(100)
}

/// Windows attribute bits of a row.
pub fn attribute_bits(row: &Row) -> u32 {
    let mut bits = if row.is_dir {
        ATTR_DIRECTORY
    } else {
        ATTR_ARCHIVE
    };
    if row.attributes.contains('R') {
        bits |= ATTR_READONLY;
    }
    if row.attributes.contains('H') {
        bits |= ATTR_HIDDEN;
    }
    bits
}

/// Writes `field` as a CSV field, quoted.
pub fn write_quoted(out: &mut impl Write, field: &[u8]) -> io::Result<()> {
    out.write_all(b"\"")?;
    for part in field.split_inclusive(|&b| b == b'"') {
        out.write_all(part)?;
        if part.last() == Some(&b'"') {
            out.write_all(b"\"")?;
        }
    }
    out.write_all(b"\"")
}

/// Writes one file list line (without the header).
pub fn write_row(out: &mut impl Write, row: &Row) -> io::Result<()> {
    write_quoted(out, &row.path())?;
    let date = |ns: i64| {
        if ns == 0 {
            String::new()
        } else {
            to_filetime(ns).to_string()
        }
    };
    let size = row.size.map(|s| s.to_string()).unwrap_or_default();
    write!(
        out,
        ",{size},{},{},{}\r\n",
        date(row.mtime),
        date(row.created),
        attribute_bits(row)
    )
}

/// Splits CSV data into records of fields. Quoted fields may contain
/// commas, doubled quotes and line breaks.
fn records(data: &[u8]) -> Vec<Vec<Vec<u8>>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = Vec::new();
    let mut quoted = false;
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        if quoted {
            match b {
                b'"' if data.get(i + 1) == Some(&b'"') => {
                    field.push(b'"');
                    i += 1;
                }
                b'"' => quoted = false,
                _ => field.push(b),
            }
        } else {
            match b {
                b'"' => quoted = true,
                b',' => record.push(std::mem::take(&mut field)),
                b'\r' => {}
                b'\n' => {
                    record.push(std::mem::take(&mut field));
                    records.push(std::mem::take(&mut record));
                }
                _ => field.push(b),
            }
        }
        i += 1;
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_owned())
}

/// Turns a file list path into an absolute Linux-style path: Windows
/// lists use backslashes and drive letters (`C:\a` becomes `/C:/a`).
fn normalize_path(raw: &[u8]) -> Vec<u8> {
    let windows = !raw.contains(&b'/') && raw.contains(&b'\\');
    let mut path: Vec<u8> = if windows {
        raw.iter()
            .map(|&b| if b == b'\\' { b'/' } else { b })
            .collect()
    } else {
        raw.to_vec()
    };
    if path.first() != Some(&b'/') {
        path.insert(0, b'/');
    }
    while path.len() > 1 && path.last() == Some(&b'/') {
        path.pop();
    }
    path
}

fn insert_path(ix: &mut Index, path: &[u8], meta: Meta) {
    let comps: Vec<&[u8]> = path
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let mut cur = ROOT;
    for (i, comp) in comps.iter().enumerate() {
        let last = i + 1 == comps.len();
        cur = match ix.lookup(cur, comp) {
            Some(id) => {
                if last {
                    ix.set_meta(id, meta);
                }
                id
            }
            None if last => ix.insert(cur, comp, meta),
            None => ix.insert(cur, comp, Meta::dir(0)),
        };
    }
}

/// Loads a file list into an index rooted at `/`.
pub fn load(file: &Path) -> io::Result<Index> {
    let data = std::fs::read(file)?;
    let data = data.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&data);
    let mut recs = records(data).into_iter();
    let header = recs.next().ok_or_else(|| invalid("empty file list"))?;
    let col = |name: &str| {
        header
            .iter()
            .position(|h| h.trim_ascii().eq_ignore_ascii_case(name.as_bytes()))
    };
    let filename = col("Filename").ok_or_else(|| invalid("no Filename column"))?;
    let (size, dm, dc, attr) = (
        col("Size"),
        col("Date Modified"),
        col("Date Created"),
        col("Attributes"),
    );
    let mut ix = Index::new(b"/", Meta::dir(0));
    for rec in recs {
        let Some(raw) = rec.get(filename).filter(|r| !r.is_empty()) else {
            continue;
        };
        let num = |c: Option<usize>| -> Option<i64> {
            let v = rec.get(c?)?;
            std::str::from_utf8(v).ok()?.trim().parse().ok()
        };
        let bits = num(attr).unwrap_or(0) as u32;
        let is_dir = bits & ATTR_DIRECTORY != 0 || raw.ends_with(b"\\") || raw.ends_with(b"/");
        let mtime = num(dm).map_or(0, from_filetime);
        let mut meta = if is_dir {
            Meta::dir(mtime)
        } else {
            Meta::file(num(size).unwrap_or(0).max(0) as u64, mtime)
        };
        meta.btime = num(dc).map_or(0, from_filetime);
        if bits & ATTR_READONLY != 0 {
            meta.mode &= !0o222;
        }
        insert_path(&mut ix, &normalize_path(raw), meta);
    }
    Ok(ix)
}

/// The result row of an entry of a file list index.
fn row_of(ix: &Index, id: EntryId, folder_sizes: &[u64]) -> Row {
    let is_dir = ix.is_dir(id);
    let m = ix.mode(id);
    let parent = ix.parent(id);
    Row {
        name: ix.name(id).to_vec(),
        folder: ix.path(parent),
        is_dir,
        size: Some(if is_dir {
            folder_sizes[id as usize]
        } else {
            ix.size(id)
        }),
        mtime: ix.mtime(id),
        created: ix.btime(id),
        accessed: ix.atime(id),
        changed: 0,
        attributes: attributes_of(ix, id),
        type_name: type_name(
            ix.name(id),
            is_dir,
            m & mode::S_IFMT == mode::S_IFREG && m & 0o111 != 0,
        ),
        run_count: 0,
        date_run: 0,
        offline: false,
    }
}

/// Searches a loaded file list. Returns the rows and the terms to
/// highlight.
pub fn search(
    ix: &Index,
    query: &str,
    opts: &ParseOptions,
    sort: SortSpec,
) -> Result<(Vec<Row>, Vec<String>), String> {
    let expr = parse(query, opts);
    let highlight = highlight_terms(&expr);
    let query = Query::compile(&expr).map_err(|e| e.to_string())?;
    let vols = [(0u16, ix)];
    let extra = FolderSizes(ix.folder_sizes());
    let mut hits = query
        .run_with(&vols, None, &extra, &AtomicBool::new(false))
        .expect("never cancelled");
    sort_hits_with(&mut hits, &vols, sort, &extra);
    if let Some(limit) = query.limit() {
        hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    }
    let rows = hits.iter().map(|h| row_of(ix, h.id, &extra.0)).collect();
    Ok((rows, highlight))
}

/// File lists know every file size, so folder sizes are their sums.
struct FolderSizes(Vec<u64>);

impl Extra for FolderSizes {
    fn folder_size(&self, _vol: u16, id: EntryId) -> Option<u64> {
        self.0.get(id as usize).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, is_dir: bool, size: Option<u64>) -> Row {
        let (folder, name) = path.rsplit_once('/').unwrap();
        Row {
            name: name.as_bytes().to_vec(),
            folder: if folder.is_empty() { "/" } else { folder }
                .as_bytes()
                .to_vec(),
            is_dir,
            size,
            mtime: 1_700_000_000_000_000_000,
            created: 0,
            attributes: if is_dir { "D" } else { "A" }.into(),
            ..Default::default()
        }
    }

    #[test]
    fn filetime_roundtrip() {
        let ns = 1_700_000_000_123_456_700;
        assert_eq!(from_filetime(to_filetime(ns)), ns);
        assert_eq!(to_filetime(0), FILETIME_UNIX);
    }

    #[test]
    fn written_lists_load_back_and_search() {
        let mut out = HEADER.as_bytes().to_vec();
        out.extend_from_slice(b"\r\n");
        for r in [
            row("/data/docs", true, None),
            row("/data/docs/a \"quoted\", name.txt", false, Some(10)),
            row("/data/docs/מסמך.pdf", false, Some(20)),
            row("/data/other.bin", false, Some(5)),
        ] {
            write_row(&mut out, &r).unwrap();
        }
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("list.efu");
        std::fs::write(&file, &out).unwrap();

        let ix = load(&file).unwrap();
        let opts = ParseOptions::default();
        let (rows, terms) = search(&ix, "quoted", &opts, SortSpec::default()).unwrap();
        assert_eq!(terms, ["quoted"]);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].path(),
            "/data/docs/a \"quoted\", name.txt".as_bytes()
        );
        assert_eq!(rows[0].size, Some(10));
        assert_eq!(rows[0].mtime, 1_700_000_000_000_000_000);
        let (rows, _) = search(&ix, "מסמך", &opts, SortSpec::default()).unwrap();
        assert_eq!(rows[0].type_name, "PDF Document");
        let (rows, _) = search(&ix, "folder:docs", &opts, SortSpec::default()).unwrap();
        assert_eq!(rows[0].size, Some(30), "folder sizes from the list");
        let (rows, _) = search(&ix, "size:>6", &opts, SortSpec::default()).unwrap();
        assert_eq!(rows.len(), 4, "two files, docs and data");
    }

    #[test]
    fn windows_lists() {
        let data = "Filename,Size,Attributes\r\n\"C:\\Users\\me\\x.txt\",3,32\r\n\"C:\\Users\\me\\\",,16\r\n";
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("win.efu");
        std::fs::write(&file, data).unwrap();
        let ix = load(&file).unwrap();
        let id = ix.resolve(b"/C:/Users/me/x.txt").unwrap();
        assert_eq!(ix.size(id), 3);
        assert!(ix.is_dir(ix.resolve(b"/C:/Users/me").unwrap()));
        assert!(load(&tmp.path().join("missing.efu")).is_err());
        std::fs::write(&file, "Size\r\n1\r\n").unwrap();
        assert!(load(&file).is_err(), "no Filename column");
    }
}
