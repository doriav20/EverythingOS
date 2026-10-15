//! Settings kept between runs in `~/.config/evos/evos-gtk.ini`: visible
//! columns, search options, sort, the preview pane, bookmarks and the
//! search history.

use std::path::{Path, PathBuf};

use evos_proto::{SearchOptions, Sort, SortKey};
use gtk::glib;

/// Accessor for one boolean search option.
pub type OptionField = fn(&mut SearchOptions) -> &mut bool;

/// The search options by their action and settings names.
pub const OPTIONS: [(&str, OptionField); 9] = [
    ("match-case", |o| &mut o.match_case),
    ("whole-word", |o| &mut o.whole_word),
    ("match-path", |o| &mut o.match_path),
    ("match-diacritics", |o| &mut o.match_diacritics),
    ("match-prefix", |o| &mut o.match_prefix),
    ("match-suffix", |o| &mut o.match_suffix),
    ("ignore-punctuation", |o| &mut o.ignore_punctuation),
    ("ignore-white-space", |o| &mut o.ignore_white_space),
    ("regex", |o| &mut o.regex),
];

/// Searches remembered in the history.
const HISTORY: usize = 50;

/// A saved search, as in Everything's Bookmarks menu.
#[derive(Debug, Clone, PartialEq)]
pub struct Bookmark {
    pub name: String,
    pub search: String,
    /// Index into the window's type filters.
    pub filter: usize,
    pub options: SearchOptions,
    pub sort: Sort,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Visible columns by id, in order.
    pub columns: Vec<String>,
    pub options: SearchOptions,
    pub sort: Sort,
    pub preview: bool,
    pub status_bar: bool,
    pub bookmarks: Vec<Bookmark>,
    /// Most recent first.
    pub history: Vec<String>,
    file: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            columns: crate::columns::COLUMNS
                .iter()
                .filter(|c| c.default_visible)
                .map(|c| c.id.to_owned())
                .collect(),
            options: SearchOptions::default(),
            sort: Sort::default(),
            preview: false,
            status_bar: true,
            bookmarks: Vec::new(),
            history: Vec::new(),
            file: None,
        }
    }
}

fn options_to_string(options: &SearchOptions) -> String {
    let mut o = options.clone();
    OPTIONS
        .iter()
        .filter(|(_, field)| *field(&mut o))
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(",")
}

fn options_from_string(s: &str) -> SearchOptions {
    let mut options = SearchOptions::default();
    for name in s.split(',') {
        if let Some((_, field)) = OPTIONS.iter().find(|(n, _)| *n == name) {
            *field(&mut options) = true;
        }
    }
    options
}

fn sort_from(key: &str, descending: bool) -> Sort {
    Sort {
        key: SortKey::parse(key).unwrap_or_default(),
        descending,
    }
}

impl Settings {
    /// The default settings file.
    pub fn default_file() -> PathBuf {
        glib::user_config_dir().join("evos").join("evos-gtk.ini")
    }

    /// Loads settings; a missing or unreadable file gives the defaults.
    pub fn load(file: &Path) -> Self {
        let mut s = Settings {
            file: Some(file.to_owned()),
            ..Default::default()
        };
        let kf = glib::KeyFile::new();
        if kf.load_from_file(file, glib::KeyFileFlags::NONE).is_err() {
            return s;
        }
        let string = |group: &str, key: &str| kf.string(group, key).ok().map(|v| v.to_string());
        if let Some(columns) = string("view", "columns") {
            let columns: Vec<String> = columns
                .split(';')
                .filter(|id| crate::columns::by_id(id).is_some())
                .map(String::from)
                .collect();
            if !columns.is_empty() {
                s.columns = columns;
            }
        }
        s.preview = kf.boolean("view", "preview").unwrap_or(false);
        s.status_bar = kf.boolean("view", "status-bar").unwrap_or(true);
        s.sort = sort_from(
            &string("view", "sort").unwrap_or_default(),
            kf.boolean("view", "sort-descending").unwrap_or(false),
        );
        s.options = options_from_string(&string("search", "options").unwrap_or_default());
        s.history = string("search", "history")
            .map(|h| h.lines().map(String::from).collect())
            .unwrap_or_default();
        for group in kf.groups().iter() {
            let group = group.as_str();
            if !group.starts_with("bookmark ") {
                continue;
            }
            let Some(name) = string(group, "name") else {
                continue;
            };
            s.bookmarks.push(Bookmark {
                name,
                search: string(group, "search").unwrap_or_default(),
                filter: kf.integer(group, "filter").unwrap_or(0).max(0) as usize,
                options: options_from_string(&string(group, "options").unwrap_or_default()),
                sort: sort_from(
                    &string(group, "sort").unwrap_or_default(),
                    kf.boolean(group, "sort-descending").unwrap_or(false),
                ),
            });
        }
        s
    }

    /// Writes the settings file (if this came from one).
    pub fn save(&self) {
        let Some(file) = &self.file else { return };
        let kf = glib::KeyFile::new();
        kf.set_string("view", "columns", &self.columns.join(";"));
        kf.set_boolean("view", "preview", self.preview);
        kf.set_boolean("view", "status-bar", self.status_bar);
        kf.set_string("view", "sort", self.sort.key.name());
        kf.set_boolean("view", "sort-descending", self.sort.descending);
        kf.set_string("search", "options", &options_to_string(&self.options));
        kf.set_string("search", "history", &self.history.join("\n"));
        for (i, b) in self.bookmarks.iter().enumerate() {
            let group = format!("bookmark {i}");
            kf.set_string(&group, "name", &b.name);
            kf.set_string(&group, "search", &b.search);
            kf.set_integer(&group, "filter", b.filter as i32);
            kf.set_string(&group, "options", &options_to_string(&b.options));
            kf.set_string(&group, "sort", b.sort.key.name());
            kf.set_boolean(&group, "sort-descending", b.sort.descending);
        }
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = kf.save_to_file(file) {
            eprintln!("evos-gtk: cannot save {}: {e}", file.display());
        }
    }

    /// Puts a search at the top of the history.
    pub fn remember(&mut self, search: &str) {
        let search = search.trim();
        if search.is_empty() {
            return;
        }
        self.history.retain(|h| h != search);
        self.history.insert(0, search.to_owned());
        self.history.truncate(HISTORY);
    }

    /// Adds or replaces the bookmark with this name.
    pub fn set_bookmark(&mut self, bookmark: Bookmark) {
        match self.bookmarks.iter_mut().find(|b| b.name == bookmark.name) {
            Some(b) => *b = bookmark,
            None => self.bookmarks.push(bookmark),
        }
    }

    pub fn bookmark(&self, name: &str) -> Option<&Bookmark> {
        self.bookmarks
            .iter()
            .find(|b| b.name.eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_through_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("sub/evos-gtk.ini");
        let mut s = Settings::load(&file);
        assert_eq!(
            s,
            Settings {
                file: Some(file.clone()),
                ..Default::default()
            }
        );
        s.columns = vec!["name".into(), "run-count".into(), "bogus".into()];
        s.options.match_diacritics = true;
        s.options.regex = true;
        s.sort = Sort {
            key: SortKey::DateRun,
            descending: true,
        };
        s.preview = true;
        s.remember("first");
        s.remember("שלום עולם");
        s.remember("first");
        s.set_bookmark(Bookmark {
            name: "Big videos".into(),
            search: "video: size:>1gb".into(),
            filter: 2,
            options: SearchOptions {
                match_case: true,
                ..Default::default()
            },
            sort: Sort {
                key: SortKey::Size,
                descending: true,
            },
        });
        s.save();

        let back = Settings::load(&file);
        assert_eq!(back.columns, ["name", "run-count"], "unknown ids dropped");
        assert_eq!(back.options, s.options);
        assert_eq!(back.sort, s.sort);
        assert!(back.preview);
        assert_eq!(back.history, ["first", "שלום עולם"]);
        assert_eq!(back.bookmarks, s.bookmarks);
        assert_eq!(back.bookmark("big VIDEOS").unwrap().filter, 2);
    }
}
