//! Parsed query representation.

pub use super::values::Bounds;

/// A parsed query.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Matches everything (the empty query).
    All,
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    Filter(Filter),
}

/// A single condition on an entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Text(TextMatch),
    Kind(Kind),
    /// Case-folded extensions without the dot (`ext:jpg;png`); empty
    /// matches files without an extension.
    Ext(Vec<String>),
    /// Case-folded file names (`filelist:a.txt;b.txt`).
    FileList(Vec<String>),
    /// Size in bytes. Folders match when folder sizes are available.
    Size(Bounds<u64>),
    /// One of the entry's dates, in Unix nanoseconds.
    Date(DateField, Bounds<i64>),
    /// Number of components in the absolute path (`/etc/hosts` is 2).
    Depth(Bounds<u64>),
    /// Name length in characters.
    Len(Bounds<u64>),
    /// How often the entry was opened from EverythingOS.
    RunCount(Bounds<u64>),
    /// Folders with this many children of a kind.
    ChildCount(ChildKind, Bounds<u64>),
    /// Folders with a child whose name matches.
    Child(TextMatch),
    /// Entries having all of these attributes (`attrib:dh`).
    Attrib(Vec<Attribute>),
    /// The type description ("PNG Image", "Folder", ...) matches.
    Type(TextMatch),
    /// Direct children of this absolute folder path.
    Parent(String),
    /// Everything below this absolute folder path.
    Under(String),
    /// Entries without a parent folder: the roots of indexed volumes.
    Root,
    /// Folders without children.
    Empty,
    /// Regular files with an execute permission bit.
    Executable,
    /// Limits the number of results (`count:100`).
    Count(u64),
    /// Keeps entries whose properties are duplicated within the results.
    Dupe(Vec<DupeKey>),
    /// File content contains the text (read from disk).
    Content(ContentMatch),
    /// A property read from the file (images and media).
    Property(Property, PropertyMatch),
}

/// Files or folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
}

/// Dates kept for every entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DateField {
    Modified,
    Created,
    Accessed,
    /// When the index last saw the entry change.
    RecentlyChanged,
    /// When the entry was last opened from EverythingOS.
    Run,
}

/// Which children `child-count:` counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildKind {
    All,
    Files,
    Folders,
}

/// File attributes, using Everything's letters where Linux has an
/// equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Attribute {
    /// A: any non-folder entry (Windows sets archive on most files).
    Archive,
    /// D
    Directory,
    /// H: the name starts with a dot.
    Hidden,
    /// R: no write permission bit is set.
    ReadOnly,
    /// L: a symbolic link (reparse point on Windows).
    ReparsePoint,
    /// N: a plain file with none of the other attributes.
    Normal,
    /// Letters that have no Linux equivalent never match.
    Unsupported,
}

/// Properties `dupe:` can compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DupeKey {
    Name,
    Size,
    Stem,
    Extension,
    Modified,
    Created,
}

/// What a text pattern is matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The entry's own name.
    Name,
    /// The entry's full absolute path.
    Path,
    /// The name without its extension.
    Stem,
    /// The full path of the containing folder.
    Location,
}

/// How a text pattern is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// The pattern occurs in the target (see [`Anchor`]).
    Substring,
    /// `*` and `?` wildcards; the whole target must match.
    Wildcard,
    /// A regular expression, searched anywhere in the target.
    Regex,
}

/// Where a substring must occur.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Anchor {
    #[default]
    Anywhere,
    /// `startwith:`
    Start,
    /// `endwith:`
    End,
    /// `whole:`, the entire target.
    Whole,
}

/// A text condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    pub pattern: String,
    pub kind: PatternKind,
    pub target: Target,
    pub case_sensitive: bool,
    /// Match diacritical marks (off: "e" finds "é").
    pub diacritics: bool,
    pub whole_word: bool,
    /// `prefix:` the match starts a word.
    pub word_prefix: bool,
    /// `suffix:` the match ends a word.
    pub word_suffix: bool,
    pub anchor: Anchor,
    pub ignore_punct: bool,
    pub ignore_ws: bool,
}

impl TextMatch {
    /// A plain case- and diacritic-insensitive substring match on names.
    pub fn substring(pattern: &str) -> Self {
        TextMatch {
            pattern: pattern.to_owned(),
            kind: PatternKind::Substring,
            target: Target::Name,
            case_sensitive: false,
            diacritics: false,
            whole_word: false,
            word_prefix: false,
            word_suffix: false,
            anchor: Anchor::Anywhere,
            ignore_punct: false,
            ignore_ws: false,
        }
    }
}

/// A content search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentMatch {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

/// Properties read from file headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Property {
    Width,
    Height,
    BitDepth,
    /// Duration in milliseconds.
    Length,
    Track,
    Album,
    Artist,
    Title,
    Genre,
    Comment,
}

impl Property {
    pub fn is_text(self) -> bool {
        matches!(
            self,
            Property::Album
                | Property::Artist
                | Property::Title
                | Property::Genre
                | Property::Comment
        )
    }
}

/// How a property value is compared.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyMatch {
    Number(Bounds<u64>),
    Text(TextMatch),
}

impl Expr {
    /// Builds a conjunction, flattening trivial cases.
    pub fn and(mut parts: Vec<Expr>) -> Expr {
        parts.retain(|e| *e != Expr::All);
        match parts.len() {
            0 => Expr::All,
            1 => parts.pop().unwrap(),
            _ => Expr::And(parts),
        }
    }

    /// Builds a disjunction, flattening trivial cases.
    pub fn or(mut parts: Vec<Expr>) -> Expr {
        if parts.contains(&Expr::All) {
            return Expr::All;
        }
        match parts.len() {
            0 => Expr::All,
            1 => parts.pop().unwrap(),
            _ => Expr::Or(parts),
        }
    }
}
