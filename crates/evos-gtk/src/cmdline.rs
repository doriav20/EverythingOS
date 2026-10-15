//! Everything's command line for the search window, so launchers and
//! scripts written for `Everything.exe` work: `evos-gtk -s "*.pdf" -sort
//! size -sort-descending -new-window`. Options take one or two dashes.

use evos_proto::SortKey;

/// Which control gets the keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Search,
    Results,
}

/// What one invocation asks for.
#[derive(Debug, Default, PartialEq)]
pub struct Launch {
    pub search: Option<String>,
    /// Index into [`crate::window::FILTERS`].
    pub filter: Option<usize>,
    pub sort: Option<SortKey>,
    pub descending: Option<bool>,
    /// Search options to set, by action name.
    pub options: Vec<(&'static str, bool)>,
    /// Terms appended to the search (`-path`, `-parent`).
    pub scope: Vec<String>,
    pub new_window: bool,
    pub toggle: bool,
    pub bookmark: Option<String>,
    pub columns: Option<Vec<&'static str>>,
    pub add_columns: Vec<&'static str>,
    pub remove_columns: Vec<&'static str>,
    pub preview: Option<bool>,
    pub status_bar: Option<bool>,
    pub focus: Option<Focus>,
    pub maximize: Option<bool>,
    pub minimize: bool,
    pub fullscreen: Option<bool>,
    pub home: bool,
    pub close: bool,
    pub exit: bool,
    pub reindex: bool,
    pub save_db: bool,
    pub help: bool,
}

pub const USAGE: &str = "\
Usage: evos-gtk [search] [options]

Options take one or two dashes, as in Everything:
  -s, -search <text>        Set the search
  -filter <name>            everything, audio, video, pictures, documents,
                            compressed, executables or folders
  -sort <name>              name, path, size, extension, type, date-modified,
                            date-created, date-accessed, attributes,
                            date-recently-changed, run-count or date-run,
                            optionally with -ascending or -descending
  -sort-ascending, -sort-descending
  -case, -diacritics, -ww, -regex, -match-path, -prefix, -suffix,
  -ignore-punctuation, -ignore-white-space
                            Enable a search option (-no-case etc. disable)
  -p, -path <path>          Search under a folder
  -parent <path>            Search directly in a folder
  -bookmark <name>          Open a bookmark
  -columns <list>           Set the columns (semicolon separated);
  -add-columns <list>       also -remove-columns
  -preview, -no-preview     Show or hide the preview pane
  -status-bar, -no-status-bar
  -new-window               Open a new search window
  -toggle-window            Show the window, or hide it if it is focused
  -focus-results, -focus-search-edit
  -maximize, -minimize, -fullscreen (and -no- forms)
  -home                     Clear the search
  -close                    Close the search window
  -exit                     Quit
  -reindex                  Rescan every volume
  -save-db                  Write the index to disk
";

fn option(name: &str) -> Option<&'static str> {
    Some(match name {
        "case" | "match-case" => "match-case",
        "diacritics" | "match-diacritics" => "match-diacritics",
        "ww" | "whole-word" | "whole-words" => "whole-word",
        "regex" => "regex",
        "match-path" => "match-path",
        "prefix" | "match-prefix" => "match-prefix",
        "suffix" | "match-suffix" => "match-suffix",
        "ignore-punctuation" => "ignore-punctuation",
        "ignore-white-space" => "ignore-white-space",
        _ => return None,
    })
}

fn filter(name: &str) -> Option<usize> {
    let name = name.to_ascii_lowercase();
    let name = name.trim_end_matches('s');
    crate::window::FILTERS.iter().position(|(title, _)| {
        let title = title.to_ascii_lowercase();
        title.trim_end_matches('s') == name
    })
}

/// Parses `name[-ascending|-descending]`; spaces count as dashes, so
/// `"Date Modified"` works too.
fn sort(value: &str) -> Option<(SortKey, Option<bool>)> {
    let value = value.to_ascii_lowercase().replace(' ', "-");
    for (suffix, desc) in [("-ascending", false), ("-descending", true)] {
        if let Some(name) = value.strip_suffix(suffix) {
            return SortKey::parse(name).map(|k| (k, Some(desc)));
        }
    }
    SortKey::parse(&value).map(|k| (k, None))
}

fn quoted(path: &str) -> String {
    format!("\"{}\"", path.trim_end_matches('/'))
}

/// Parses the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Launch, String> {
    let mut l = Launch::default();
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let Some(name) = arg
            .strip_prefix("--")
            .or_else(|| arg.strip_prefix('-'))
            .filter(|n| !n.is_empty())
        else {
            words.push(arg.clone());
            continue;
        };
        let name = name.to_ascii_lowercase();
        let mut value = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("-{name} needs {what}"))
        };
        match name.as_str() {
            "s" | "search" => l.search = Some(value("a search")?),
            "s*" | "search*" => {
                l.search = Some(it.by_ref().cloned().collect::<Vec<_>>().join(" "));
            }
            "filter" => {
                let v = value("a filter name")?;
                l.filter = Some(filter(&v).ok_or_else(|| format!("unknown filter {v:?}"))?);
            }
            "sort" => {
                let v = value("a sort name")?;
                let (key, desc) = sort(&v).ok_or_else(|| format!("unknown sort {v:?}"))?;
                l.sort = Some(key);
                if desc.is_some() {
                    l.descending = desc;
                }
            }
            "sort-ascending" => l.descending = Some(false),
            "sort-descending" => l.descending = Some(true),
            "p" | "path" => l.scope.push(format!("under:{}", quoted(&value("a path")?))),
            "parent" => l
                .scope
                .push(format!("parent:{}", quoted(&value("a path")?))),
            "bookmark" => l.bookmark = Some(value("a bookmark name")?),
            "columns" => l.columns = Some(crate::columns::parse_list(&value("columns")?)),
            "add-columns" => l
                .add_columns
                .extend(crate::columns::parse_list(&value("columns")?)),
            "remove-columns" => l
                .remove_columns
                .extend(crate::columns::parse_list(&value("columns")?)),
            "new-window" => l.new_window = true,
            "no-new-window" => l.new_window = false,
            "toggle-window" | "toggle" | "t" => l.toggle = true,
            "preview" => l.preview = Some(true),
            "no-preview" => l.preview = Some(false),
            "status-bar" => l.status_bar = Some(true),
            "no-status-bar" => l.status_bar = Some(false),
            "focus-results" | "focus-result-list" => l.focus = Some(Focus::Results),
            "focus-search-edit" => l.focus = Some(Focus::Search),
            "maximize" => l.maximize = Some(true),
            "no-maximize" => l.maximize = Some(false),
            "minimize" => l.minimize = true,
            "fullscreen" => l.fullscreen = Some(true),
            "no-fullscreen" => l.fullscreen = Some(false),
            "home" => l.home = true,
            "close" => l.close = true,
            "exit" | "quit" => l.exit = true,
            "reindex" | "rescan-all" => l.reindex = true,
            "save-db" | "save-db-now" => l.save_db = true,
            "h" | "help" | "?" => l.help = true,
            other => match (other.strip_prefix("no-"), option(other)) {
                (_, Some(opt)) => l.options.push((opt, true)),
                (Some(rest), None) if option(rest).is_some() => {
                    l.options.push((option(rest).expect("checked"), false));
                }
                _ => return Err(format!("unknown option {arg:?} (see -help)")),
            },
        }
    }
    if !words.is_empty() && l.search.is_none() {
        l.search = Some(words.join(" "));
    }
    Ok(l)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Launch {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn everything_style_options() {
        let l = p(&[
            "-s",
            "*.pdf",
            "-sort",
            "Date Modified",
            "-sort-descending",
            "-case",
            "--no-regex",
            "-filter",
            "Documents",
            "-new-window",
        ]);
        assert_eq!(l.search.as_deref(), Some("*.pdf"));
        assert_eq!(l.sort, Some(SortKey::Modified));
        assert_eq!(l.descending, Some(true));
        assert_eq!(l.options, [("match-case", true), ("regex", false)]);
        assert_eq!(l.filter, Some(4));
        assert!(l.new_window);
    }

    #[test]
    fn bare_words_scopes_and_columns() {
        let l = p(&[
            "report",
            "final",
            "-parent",
            "/home/me/",
            "-columns",
            "name;dm",
        ]);
        assert_eq!(l.search.as_deref(), Some("report final"));
        assert_eq!(l.scope, ["parent:\"/home/me\""]);
        assert_eq!(l.columns, Some(vec!["name", "date-modified"]));
        let l = p(&["-sort", "size-descending", "-s*", "a", "-b"]);
        assert_eq!((l.sort, l.descending), (Some(SortKey::Size), Some(true)));
        assert_eq!(l.search.as_deref(), Some("a -b"), "-s* takes the rest");
        assert!(p(&["--toggle"]).toggle);
    }

    #[test]
    fn errors() {
        let bad = |args: &[&str]| {
            parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap_err()
        };
        assert!(bad(&["-bogus"]).contains("unknown option"));
        assert!(bad(&["-sort"]).contains("needs"));
        assert!(bad(&["-filter", "nope"]).contains("unknown filter"));
    }
}
