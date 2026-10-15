//! Writing Everything file lists (`.efu`): CSV files of full paths with
//! size, dates (Windows FILETIME) and Windows attribute bits, which
//! Everything on Windows can open too.

use std::io::{self, Write};

use crate::Row;

pub const HEADER: &str = "Filename,Size,Date Modified,Date Created,Attributes";

/// 100 ns intervals between 1601-01-01 (FILETIME) and 1970-01-01.
const FILETIME_UNIX: i64 = 116_444_736_000_000_000;

pub const ATTR_READONLY: u32 = 0x1;
pub const ATTR_HIDDEN: u32 = 0x2;
pub const ATTR_DIRECTORY: u32 = 0x10;
pub const ATTR_ARCHIVE: u32 = 0x20;

/// Unix nanoseconds to FILETIME.
pub fn to_filetime(ns: i64) -> i64 {
    ns.div_euclid(100) + FILETIME_UNIX
}

/// FILETIME to Unix nanoseconds.
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

/// Writes `field` as a quoted CSV field.
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

/// Writes one file list line (the header is [`HEADER`]).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_and_times() {
        let ns = 1_700_000_000_123_456_700;
        assert_eq!(from_filetime(to_filetime(ns)), ns);
        let row = Row {
            name: b"say \"hi\".txt".to_vec(),
            folder: b"/d".to_vec(),
            size: Some(3),
            mtime: 1_700_000_000_000_000_000,
            attributes: "RA".into(),
            ..Default::default()
        };
        let mut out = Vec::new();
        write_row(&mut out, &row).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "\"/d/say \"\"hi\"\".txt\",3,133444736000000000,,33\r\n"
        );
    }
}
