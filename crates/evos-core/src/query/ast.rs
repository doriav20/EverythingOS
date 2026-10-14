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
    /// Case-folded extensions without the dot (`ext:jpg;png`).
    Ext(Vec<String>),
    /// File size in bytes. Never matches folders.
    Size(Bounds<u64>),
    /// Modification time in Unix nanoseconds.
    Modified(Bounds<i64>),
    /// Number of components in the absolute path (`/etc/hosts` is 2).
    Depth(Bounds<u64>),
    /// Name length in characters.
    Len(Bounds<u64>),
    /// Direct children of this absolute folder path.
    Parent(String),
    /// Everything below this absolute folder path.
    Under(String),
    /// Folders without children.
    Empty,
    /// Regular files with an execute permission bit.
    Executable,
}

/// Files or folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Folder,
}

/// What a text pattern is matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The entry's own name.
    Name,
    /// The entry's full absolute path.
    Path,
}

/// How a text pattern is interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    /// The pattern occurs anywhere in the target.
    Substring,
    /// `*` and `?` wildcards; the whole target must match.
    Wildcard,
    /// A regular expression, searched anywhere in the target.
    Regex,
}

/// A text condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextMatch {
    pub pattern: String,
    pub kind: PatternKind,
    pub target: Target,
    pub case_sensitive: bool,
    pub whole_word: bool,
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
