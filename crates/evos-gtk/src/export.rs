//! Exporting results to a file, as in Everything's File > Export: the
//! format follows the file extension.

use std::io::{self, Write};
use std::path::Path;

use evos_proto::{efu, Row};

use crate::columns::ColumnDef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Everything file list.
    Efu,
    /// The visible columns.
    Csv,
    Tsv,
    /// Full paths, one per line.
    Txt,
    M3u,
    M3u8,
}

impl Format {
    pub fn for_path(path: &Path) -> Format {
        match path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .as_deref()
        {
            Some("efu") => Format::Efu,
            Some("csv") => Format::Csv,
            Some("tsv") => Format::Tsv,
            Some("m3u") => Format::M3u,
            Some("m3u8") => Format::M3u8,
            _ => Format::Txt,
        }
    }
}

pub struct Exporter<W: Write> {
    out: W,
    format: Format,
    columns: Vec<&'static ColumnDef>,
}

impl<W: Write> Exporter<W> {
    /// Starts an export, writing the header.
    pub fn new(mut out: W, format: Format, columns: Vec<&'static ColumnDef>) -> io::Result<Self> {
        let titles: Vec<&str> = columns.iter().map(|c| c.title).collect();
        match format {
            Format::Efu => write!(out, "{}\r\n", efu::HEADER)?,
            Format::Csv => write!(out, "{}\r\n", titles.join(","))?,
            Format::Tsv => writeln!(out, "{}", titles.join("\t"))?,
            Format::M3u8 => writeln!(out, "#EXTM3U")?,
            Format::Txt | Format::M3u => {}
        }
        Ok(Exporter {
            out,
            format,
            columns,
        })
    }

    pub fn row(&mut self, row: &Row) -> io::Result<()> {
        match self.format {
            Format::Efu => efu::write_row(&mut self.out, row),
            Format::Csv => {
                for (i, c) in self.columns.iter().enumerate() {
                    if i > 0 {
                        self.out.write_all(b",")?;
                    }
                    efu::write_quoted(&mut self.out, c.text(row).as_bytes())?;
                }
                self.out.write_all(b"\r\n")
            }
            Format::Tsv => {
                let fields: Vec<String> = self
                    .columns
                    .iter()
                    .map(|c| c.text(row).replace(['\t', '\n', '\r'], " "))
                    .collect();
                writeln!(self.out, "{}", fields.join("\t"))
            }
            Format::Txt | Format::M3u | Format::M3u8 => {
                self.out.write_all(&row.path())?;
                self.out.write_all(b"\n")
            }
        }
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::by_id;

    fn rows() -> Vec<Row> {
        vec![Row {
            name: "מסמך, \"א\".txt".as_bytes().to_vec(),
            folder: b"/d".to_vec(),
            size: Some(5),
            type_name: "Text Document".into(),
            attributes: "A".into(),
            ..Default::default()
        }]
    }

    fn export(format: Format) -> String {
        let cols = vec![by_id("name").unwrap(), by_id("size").unwrap()];
        let mut e = Exporter::new(Vec::new(), format, cols).unwrap();
        for r in rows() {
            e.row(&r).unwrap();
        }
        String::from_utf8(e.finish().unwrap()).unwrap()
    }

    #[test]
    fn formats() {
        assert_eq!(Format::for_path(Path::new("/x/a.EFU")), Format::Efu);
        assert_eq!(Format::for_path(Path::new("a")), Format::Txt);
        assert_eq!(
            export(Format::Csv),
            "Name,Size\r\n\"מסמך, \"\"א\"\".txt\",\"5\"\r\n"
        );
        assert_eq!(export(Format::Tsv), "Name\tSize\nמסמך, \"א\".txt\t5\n");
        assert_eq!(export(Format::Txt), "/d/מסמך, \"א\".txt\n");
        assert_eq!(export(Format::M3u8), "#EXTM3U\n/d/מסמך, \"א\".txt\n");
        assert!(export(Format::Efu).starts_with(efu::HEADER));
    }
}
