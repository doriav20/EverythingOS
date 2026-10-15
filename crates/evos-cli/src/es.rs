//! Compatibility with the argument style of Everything's `es.exe`.
//!
//! es takes long options with a single dash (`-sort size`, `-dm`,
//! `-export-csv out.csv`) and DIR-style switches (`/ad`, `/a-d`, `/os`,
//! `/o-d`). Before clap sees the arguments they are rewritten into the
//! GNU style: known single-dash long options get a second dash, DIR-style
//! attribute switches become query terms and DIR-style sorts become
//! `--sort`. The order of column options is recorded, because es prints
//! columns in the order they were asked for.

use evos_proto::SortKey;

/// Result columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    /// The containing folder.
    Path,
    FullPath,
    Extension,
    Size,
    DateCreated,
    DateModified,
    DateAccessed,
    Attributes,
    RunCount,
    DateRun,
    DateRecentlyChanged,
    Type,
}

impl Column {
    /// Column options, by their long names.
    fn from_option(name: &str) -> Option<Column> {
        Some(match name {
            "name" => Column::Name,
            "path-column" => Column::Path,
            "full-path-and-name" | "filename-column" => Column::FullPath,
            "extension" | "ext" => Column::Extension,
            "size" => Column::Size,
            "date-created" | "dc" => Column::DateCreated,
            "date-modified" | "dm" => Column::DateModified,
            "date-accessed" | "da" => Column::DateAccessed,
            "attributes" | "attribs" | "attrib" => Column::Attributes,
            "run-count" => Column::RunCount,
            "date-run" => Column::DateRun,
            "date-recently-changed" | "rc" => Column::DateRecentlyChanged,
            "type" => Column::Type,
            _ => return None,
        })
    }

    /// Header text, as Everything names the property.
    pub fn title(self) -> &'static str {
        match self {
            Column::Name => "Name",
            Column::Path => "Path",
            Column::FullPath => "Filename",
            Column::Extension => "Extension",
            Column::Size => "Size",
            Column::DateCreated => "Date Created",
            Column::DateModified => "Date Modified",
            Column::DateAccessed => "Date Accessed",
            Column::Attributes => "Attributes",
            Column::RunCount => "Run Count",
            Column::DateRun => "Date Run",
            Column::DateRecentlyChanged => "Date Recently Changed",
            Column::Type => "Type",
        }
    }
}

/// Long options `evos` understands; with a single dash they are rewritten.
const LONG_OPTIONS: &[&str] = &[
    // matching
    "regex",
    "case",
    "match-case",
    "ww",
    "whole-word",
    "whole-words",
    "match-path",
    "diacritics",
    "match-diacritics",
    "prefix",
    "suffix",
    "ignore-punctuation",
    "ignore-white-space",
    // scope
    "path",
    "parent-path",
    "parent",
    "filter",
    "filelist",
    // results
    "offset",
    "max-results",
    "max",
    "sort",
    "sort-ascending",
    "sort-descending",
    "desc",
    // columns
    "name",
    "path-column",
    "full-path-and-name",
    "filename-column",
    "extension",
    "ext",
    "size",
    "date-created",
    "dc",
    "date-modified",
    "dm",
    "date-accessed",
    "da",
    "attributes",
    "attribs",
    "attrib",
    "run-count",
    "date-run",
    "date-recently-changed",
    "rc",
    "type",
    // formats
    "csv",
    "tsv",
    "efu",
    "txt",
    "m3u",
    "m3u8",
    "json",
    "null",
    "export-csv",
    "export-tsv",
    "export-efu",
    "export-txt",
    "export-m3u",
    "export-m3u8",
    "export-json",
    "no-header",
    "double-quote",
    "size-format",
    "date-format",
    "human",
    "highlight",
    "count",
    "get-result-count",
    "get-total-size",
    // actions
    "get-run-count",
    "inc-run-count",
    "set-run-count",
    "save-db",
    "reindex",
    "rescan-all",
    "get-everything-version",
    "create-file-list",
    "version",
    "help",
    "socket",
];

/// Options that only turn a default-off setting off again.
fn is_noop_negation(name: &str) -> bool {
    name.strip_prefix("no-").is_some_and(|rest| {
        matches!(
            rest,
            "regex"
                | "case"
                | "ww"
                | "whole-word"
                | "whole-words"
                | "match-path"
                | "diacritics"
                | "prefix"
                | "suffix"
                | "ignore-punctuation"
                | "ignore-white-space"
                | "highlight"
        )
    })
}

/// Arguments rewritten for clap, plus the requested column order.
#[derive(Debug, Default, PartialEq)]
pub struct Translated {
    pub args: Vec<String>,
    pub columns: Vec<Column>,
}

/// Rewrites es-style arguments (see the module docs). `args[0]` is the
/// program name. Everything after `--` is passed through untouched.
pub fn translate(args: impl IntoIterator<Item = String>) -> Translated {
    let mut out = Translated::default();
    let mut args = args.into_iter();
    out.args.extend(args.next());
    let mut literal = false;
    for arg in args {
        if literal || arg == "--" {
            literal = true;
            out.args.push(arg);
            continue;
        }
        if let Some(name) = arg.strip_prefix("--") {
            if let Some(c) = Column::from_option(name) {
                out.columns.push(c);
            }
            out.args.push(arg);
            continue;
        }
        if let Some(rest) = arg.strip_prefix('/') {
            // "/apps" could be a switch or a search for that folder; an
            // existing path wins.
            let exists = std::fs::symlink_metadata(&arg).is_ok();
            if let Some(converted) = dir_switch(rest).filter(|_| !exists) {
                out.args.extend(converted);
                continue;
            }
        }
        let Some(name) = arg.strip_prefix('-').filter(|n| n.len() > 1) else {
            out.args.push(arg);
            continue;
        };
        let name = name.to_ascii_lowercase();
        if LONG_OPTIONS.contains(&name.as_str()) {
            if let Some(c) = Column::from_option(&name) {
                out.columns.push(c);
            }
            out.args.push(format!("--{name}"));
        } else if is_noop_negation(&name) {
            // -no-case and friends: those settings are off by default.
        } else if let Some(sort) = name.strip_prefix("sort-").filter(|s| sort_spec(s)) {
            out.args.push("--sort".into());
            out.args.push(sort.to_owned());
        } else {
            // Short options (-n, -i, -iw, ...) and anything unknown are
            // left for clap.
            out.args.push(arg);
        }
    }
    out
}

/// Is `s` a sort name with an optional `-ascending`/`-descending` suffix?
pub fn sort_spec(s: &str) -> bool {
    parse_sort(s).is_some()
}

/// Parses `name[-ascending|-descending]` into a key and a descending flag
/// (None when no direction is given).
pub fn parse_sort(s: &str) -> Option<(SortKey, Option<bool>)> {
    let s = s.to_ascii_lowercase();
    for (suffix, desc) in [("-ascending", false), ("-descending", true)] {
        if let Some(name) = s.strip_suffix(suffix) {
            return SortKey::parse(name).map(|k| (k, Some(desc)));
        }
    }
    SortKey::parse(&s).map(|k| (k, None))
}

/// DIR-style switches: `/ad` folders, `/a-d` files, `/ah` attributes,
/// `/a-h` without them, `/on` sort by name, `/o-s` by size descending.
fn dir_switch(rest: &str) -> Option<Vec<String>> {
    let (kind, rest) = rest.split_at_checked(1)?;
    let (negate, letters) = match rest.strip_prefix('-') {
        Some(l) => (true, l),
        None => (false, rest),
    };
    if letters.is_empty() || !letters.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    match kind.to_ascii_lowercase().as_str() {
        "a" => {
            let letters = letters.to_ascii_lowercase();
            // Windows attribute letters, as DIR and es accept them.
            if !letters.chars().all(|c| "rhsdavntplcoie".contains(c)) {
                return None;
            }
            let term = match (letters.as_str(), negate) {
                ("d", false) => "folder:".to_owned(),
                ("d", true) => "file:".to_owned(),
                (l, false) => format!("attrib:{l}"),
                (l, true) => format!("!attrib:{l}"),
            };
            Some(vec![term])
        }
        "o" if letters.len() == 1 => {
            let key = match letters.to_ascii_lowercase().as_str() {
                "n" => "name",
                "p" => "path",
                "s" => "size",
                "e" => "extension",
                "d" => "date-modified",
                "c" => "date-created",
                "a" => "date-accessed",
                "t" => "type",
                _ => return None,
            };
            let dir = if negate { "descending" } else { "ascending" };
            Some(vec!["--sort".into(), format!("{key}-{dir}")])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(args: &[&str]) -> Translated {
        translate(
            std::iter::once("evos")
                .chain(args.iter().copied())
                .map(String::from),
        )
    }

    #[test]
    fn single_dash_long_options() {
        let r = t(&["-sort", "size", "-dm", "foo", "-n", "5", "-iw"]);
        assert_eq!(
            r.args,
            ["evos", "--sort", "size", "--dm", "foo", "-n", "5", "-iw"]
        );
        assert_eq!(r.columns, [Column::DateModified]);
        let r = t(&[
            "-export-csv",
            "out.csv",
            "-no-case",
            "-sort-size-descending",
        ]);
        assert_eq!(
            r.args,
            [
                "evos",
                "--export-csv",
                "out.csv",
                "--sort",
                "size-descending"
            ]
        );
    }

    #[test]
    fn column_order_follows_the_command_line() {
        let r = t(&["--size", "-name", "-dm", "x"]);
        assert_eq!(
            r.columns,
            [Column::Size, Column::Name, Column::DateModified]
        );
    }

    #[test]
    fn dir_style_switches() {
        assert_eq!(t(&["/ad"]).args, ["evos", "folder:"]);
        assert_eq!(t(&["/a-d"]).args, ["evos", "file:"]);
        assert_eq!(t(&["/ah"]).args, ["evos", "attrib:h"]);
        assert_eq!(t(&["/a-h"]).args, ["evos", "!attrib:h"]);
        assert_eq!(t(&["/o-s"]).args, ["evos", "--sort", "size-descending"]);
        assert_eq!(t(&["/on"]).args, ["evos", "--sort", "name-ascending"]);
        // Paths are search terms, not switches.
        assert_eq!(t(&["/opt/"]).args, ["evos", "/opt/"]);
        assert_eq!(t(&["/home"]).args, ["evos", "/home"]);
        assert_eq!(t(&["/abc"]).args, ["evos", "/abc"], "b is no attribute");
        assert_eq!(t(&["/odd"]).args, ["evos", "/odd"], "one sort letter only");
    }

    #[test]
    fn literal_after_double_dash() {
        assert_eq!(t(&["--", "-dm"]).args, ["evos", "--", "-dm"]);
    }

    #[test]
    fn sort_names() {
        assert_eq!(
            parse_sort("date-modified-descending"),
            Some((SortKey::Modified, Some(true)))
        );
        assert_eq!(parse_sort("dm"), Some((SortKey::Modified, None)));
        assert_eq!(parse_sort("bogus-ascending"), None);
    }
}
