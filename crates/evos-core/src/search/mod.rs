//! Parallel query execution over volume indexes.
//!
//! A parsed [`Expr`] is compiled into a tree of predicates once. Execution
//! splits every volume into blocks of ids that rayon evaluates in parallel;
//! predicates that need per-volume state (path automata, resolved folders)
//! get it from a [`VolumeCtx`] built at the start of each run. Information
//! the index does not hold itself (folder sizes, run history, the caller's
//! read permission) comes from an [`Extra`] provider.

mod path_dfa;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use hashbrown::{HashMap, HashSet};
use memchr::memmem;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};

use crate::fold::{self, Normalize};
use crate::props;
use crate::query::{
    Anchor, Attribute, Bounds, ChildKind, ContentMatch, DateField, DupeKey, Expr, Filter, Kind,
    PatternKind, Property, PropertyMatch, Target, TextMatch,
};
use crate::store::{extension, mode, EntryId, Index, ROOT};
use crate::wildcard::Wildcard;
use path_dfa::{Dfa, PathMemo, MAX_NEEDLE};

/// A search result: an entry of one volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hit {
    pub vol: u16,
    pub id: EntryId,
}

/// Error compiling a query.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("invalid regular expression: {0}")]
    Regex(String),
}

/// Extra per-entry filter, e.g. permission checks for the calling user.
pub type EntryFilter<'a> = &'a (dyn Fn(u16, &Index, EntryId) -> bool + Sync);

/// Run count and last run time (Unix nanoseconds) of an entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunInfo {
    pub count: u32,
    pub last: i64,
}

/// Information that lives outside the index.
pub trait Extra: Sync {
    /// Total size of a folder's contents, if folder sizes are available.
    fn folder_size(&self, _vol: u16, _id: EntryId) -> Option<u64> {
        None
    }

    /// How often and when an entry was last opened.
    fn run_info(&self, _vol: u16, _id: EntryId) -> RunInfo {
        RunInfo::default()
    }

    /// May the content of this file be read on the caller's behalf?
    fn can_read(&self, _vol: u16, _index: &Index, _id: EntryId) -> bool {
        true
    }
}

/// No extra information.
pub struct NoExtra;

impl Extra for NoExtra {}

/// A compiled query, reusable across runs and volumes.
pub struct Query {
    root: Node,
    path_slots: Vec<PathSlot>,
    folder_slots: Vec<String>,
    limit: Option<u64>,
    dupes: Option<Vec<DupeKey>>,
    include_roots: bool,
}

struct PathSlot {
    dfa: Dfa,
    norm: Normalize,
}

enum Node {
    All,
    Nothing,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Leaf),
}

/// A compiled text pattern, matched against already normalized text.
struct Matcher {
    norm: Normalize,
    kind: MatcherKind,
}

enum MatcherKind {
    Sub {
        finder: Box<memmem::Finder<'static>>,
        anchor: Anchor,
        word_start: bool,
        word_end: bool,
    },
    Glob(Wildcard),
    /// Regular expressions see the original text.
    Regex(Regex),
}

impl Matcher {
    fn new(t: &TextMatch, path: bool) -> Result<Matcher, QueryError> {
        let norm = Normalize {
            case: t.case_sensitive,
            diacritics: t.diacritics,
            ignore_punct: t.ignore_punct,
            ignore_ws: t.ignore_ws,
        };
        let kind = match t.kind {
            PatternKind::Regex => MatcherKind::Regex(
                RegexBuilder::new(&t.pattern)
                    .case_insensitive(!t.case_sensitive)
                    .build()
                    .map_err(|e| QueryError::Regex(e.to_string()))?,
            ),
            PatternKind::Wildcard => {
                let mut pattern = Vec::new();
                fold::normalize_into(t.pattern.as_bytes(), &norm, &['*', '?'], &mut pattern);
                MatcherKind::Glob(if path {
                    Wildcard::new_path(&pattern)
                } else {
                    Wildcard::new(&pattern)
                })
            }
            PatternKind::Substring => {
                let pattern = fold::normalize(t.pattern.as_bytes(), &norm);
                MatcherKind::Sub {
                    finder: Box::new(memmem::Finder::new(&pattern).into_owned()),
                    anchor: t.anchor,
                    word_start: t.whole_word || t.word_prefix,
                    word_end: t.whole_word || t.word_suffix,
                }
            }
        };
        Ok(Matcher { norm, kind })
    }

    fn is_regex(&self) -> bool {
        matches!(self.kind, MatcherKind::Regex(_))
    }

    /// Plain "contains" on folded text, which allows the fast paths.
    fn is_plain_substring(&self) -> bool {
        matches!(
            &self.kind,
            MatcherKind::Sub {
                anchor: Anchor::Anywhere,
                word_start: false,
                word_end: false,
                ..
            }
        )
    }

    fn matches_everything(&self) -> bool {
        match &self.kind {
            MatcherKind::Sub { finder, anchor, .. } => {
                finder.needle().is_empty() && *anchor != Anchor::Whole
            }
            MatcherKind::Glob(g) => g.matches_everything(),
            MatcherKind::Regex(_) => false,
        }
    }

    fn is_match(&self, text: &[u8]) -> bool {
        match &self.kind {
            MatcherKind::Regex(re) => re.is_match(text),
            MatcherKind::Glob(g) => g.is_match(text),
            MatcherKind::Sub {
                finder,
                anchor,
                word_start,
                word_end,
            } => {
                let needle = finder.needle();
                let ok_at = |pos: usize| {
                    (!word_start || at_word_start(text, pos))
                        && (!word_end || at_word_end(text, pos + needle.len()))
                };
                match anchor {
                    Anchor::Start => text.starts_with(needle) && ok_at(0),
                    Anchor::End => text.ends_with(needle) && ok_at(text.len() - needle.len()),
                    Anchor::Whole => text == needle,
                    Anchor::Anywhere if !word_start && !word_end => finder.find(text).is_some(),
                    Anchor::Anywhere => finder.find_iter(text).any(ok_at),
                }
            }
        }
    }

    /// Normalizes `text` for this matcher (regexes get it unchanged).
    fn prepare<'t>(&self, text: &'t [u8], buf: &'t mut Vec<u8>) -> &'t [u8] {
        if self.is_regex() {
            return text;
        }
        buf.clear();
        fold::normalize_into(text, &self.norm, &[], buf);
        buf
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The character ending right before byte `pos`.
fn prev_char(text: &[u8], pos: usize) -> Option<char> {
    let mut start = pos.checked_sub(1)?;
    while start > 0 && pos - start < 4 && text[start] & 0xc0 == 0x80 {
        start -= 1;
    }
    std::str::from_utf8(&text[start..pos]).ok()?.chars().next()
}

fn at_word_start(text: &[u8], pos: usize) -> bool {
    prev_char(text, pos).is_none_or(|c| !is_word_char(c))
}

fn at_word_end(text: &[u8], pos: usize) -> bool {
    let after = &text[pos..];
    match after
        .utf8_chunks()
        .next()
        .and_then(|c| c.valid().chars().next())
    {
        Some(c) => !is_word_char(c),
        None => true,
    }
}

struct TextLeaf {
    matcher: Matcher,
    target: Target,
    /// For plain substrings on paths: an automaton slot.
    path_slot: Option<usize>,
}

enum Leaf {
    Text(TextLeaf),
    Kind(Kind),
    Ext(HashSet<Vec<u8>>),
    FileList(HashSet<Vec<u8>>),
    Size(Bounds<u64>),
    Date(DateField, Bounds<i64>),
    Depth(Bounds<u64>),
    Len(Bounds<u64>),
    RunCount(Bounds<u64>),
    ChildCount(ChildKind, Bounds<u64>),
    Child(Matcher),
    Attrib(Vec<Attribute>),
    Type(Matcher),
    Parent { slot: usize },
    Under { slot: usize },
    Root,
    Empty,
    Executable,
    Content(ContentLeaf),
    Property(Property, PropertyLeaf),
}

struct ContentLeaf {
    case: bool,
    kind: ContentKind,
}

enum ContentKind {
    Plain {
        finder: Box<memmem::Finder<'static>>,
        whole_word: bool,
    },
    Regex(Regex),
}

enum PropertyLeaf {
    Number(Bounds<u64>),
    Text(Matcher),
}

impl Leaf {
    /// Rough relative cost, used to evaluate cheap predicates first.
    fn cost(&self) -> u8 {
        match self {
            Leaf::Kind(_)
            | Leaf::Size(_)
            | Leaf::Date(..)
            | Leaf::Empty
            | Leaf::Executable
            | Leaf::Root
            | Leaf::Attrib(_)
            | Leaf::Len(_)
            | Leaf::RunCount(_) => 1,
            Leaf::Ext(_) | Leaf::FileList(_) | Leaf::Parent { .. } => 2,
            Leaf::Text(t) if t.target == Target::Name && t.matcher.norm.is_folded() => 3,
            Leaf::Depth(_) | Leaf::Under { .. } | Leaf::Type(_) => 4,
            Leaf::Text(t) if t.path_slot.is_some() => 5,
            Leaf::ChildCount(..) | Leaf::Child(_) => 6,
            Leaf::Text(_) => 7,
            Leaf::Property(..) => 20,
            Leaf::Content(_) => 30,
        }
    }
}

impl Node {
    fn cost(&self) -> u8 {
        match self {
            Node::All | Node::Nothing => 0,
            Node::Leaf(l) => l.cost(),
            Node::Not(n) => n.cost(),
            Node::And(v) | Node::Or(v) => v.iter().map(Node::cost).max().unwrap_or(0),
        }
    }
}

impl Query {
    /// Compiles a parsed query.
    pub fn compile(expr: &Expr) -> Result<Query, QueryError> {
        let mut q = Query {
            root: Node::All,
            path_slots: Vec::new(),
            folder_slots: Vec::new(),
            limit: None,
            dupes: None,
            include_roots: false,
        };
        q.root = q.compile_expr(expr)?;
        Ok(q)
    }

    /// Result limit from `count:`, applied after sorting.
    pub fn limit(&self) -> Option<u64> {
        self.limit
    }

    /// Runs the query over all volumes. Returns `None` if cancelled.
    pub fn run(
        &self,
        volumes: &[(u16, &Index)],
        filter: Option<EntryFilter<'_>>,
        cancel: &AtomicBool,
    ) -> Option<Vec<Hit>> {
        self.run_with(volumes, filter, &NoExtra, cancel)
    }

    /// Runs the query with information from outside the index.
    pub fn run_with(
        &self,
        volumes: &[(u16, &Index)],
        filter: Option<EntryFilter<'_>>,
        extra: &dyn Extra,
        cancel: &AtomicBool,
    ) -> Option<Vec<Hit>> {
        const BLOCK: usize = 16 * 1024;
        let first = if self.include_roots { 0 } else { 1 };
        let mut hits = Vec::new();
        for &(vol, index) in volumes {
            let ctx = VolumeCtx::new(self, vol, index, extra);
            let blocks = index.len().div_ceil(BLOCK);
            let parts: Vec<Vec<Hit>> = (0..blocks)
                .into_par_iter()
                .map_init(Vec::new, |buf, block| {
                    if cancel.load(Ordering::Relaxed) {
                        return Vec::new();
                    }
                    let start = (block * BLOCK).max(first) as EntryId;
                    let end = ((block + 1) * BLOCK).min(index.len()) as EntryId;
                    (start..end)
                        .filter(|&id| index.is_alive(id))
                        .filter(|&id| ctx.eval(&self.root, id, buf))
                        .filter(|&id| filter.is_none_or(|f| f(vol, index, id)))
                        .map(|id| Hit { vol, id })
                        .collect()
                })
                .collect();
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            hits.extend(parts.into_iter().flatten());
        }
        if let Some(keys) = &self.dupes {
            retain_dupes(&mut hits, volumes, keys);
        }
        Some(hits)
    }

    /// Re-evaluates the query over a previous result set (for refinement).
    pub fn run_within(
        &self,
        volumes: &[(u16, &Index)],
        candidates: &[Hit],
        filter: Option<EntryFilter<'_>>,
        cancel: &AtomicBool,
    ) -> Option<Vec<Hit>> {
        let extra = NoExtra;
        let ctxs: Vec<(u16, &Index, VolumeCtx)> = volumes
            .iter()
            .map(|&(vol, ix)| (vol, ix, VolumeCtx::new(self, vol, ix, &extra)))
            .collect();
        let mut hits: Vec<Hit> = candidates
            .par_chunks(16 * 1024)
            .map_init(Vec::new, |buf, chunk| {
                if cancel.load(Ordering::Relaxed) {
                    return Vec::new();
                }
                chunk
                    .iter()
                    .copied()
                    .filter(|hit| {
                        let Some((_, ix, ctx)) = ctxs.iter().find(|(v, _, _)| *v == hit.vol) else {
                            return false;
                        };
                        (hit.id as usize) < ix.len()
                            && ix.is_alive(hit.id)
                            && ctx.eval(&self.root, hit.id, buf)
                            && filter.is_none_or(|f| f(hit.vol, ix, hit.id))
                    })
                    .collect::<Vec<_>>()
            })
            .flatten()
            .collect();
        if let Some(keys) = &self.dupes {
            retain_dupes(&mut hits, volumes, keys);
        }
        (!cancel.load(Ordering::Relaxed)).then_some(hits)
    }

    fn compile_expr(&mut self, expr: &Expr) -> Result<Node, QueryError> {
        Ok(match expr {
            Expr::All => Node::All,
            Expr::Not(inner) => match self.compile_expr(inner)? {
                Node::All => Node::Nothing,
                Node::Nothing => Node::All,
                n => Node::Not(Box::new(n)),
            },
            Expr::And(parts) => {
                let mut nodes = parts
                    .iter()
                    .map(|p| self.compile_expr(p))
                    .collect::<Result<Vec<_>, _>>()?;
                if nodes.iter().any(|n| matches!(n, Node::Nothing)) {
                    return Ok(Node::Nothing);
                }
                nodes.retain(|n| !matches!(n, Node::All));
                nodes.sort_by_key(Node::cost);
                match nodes.len() {
                    0 => Node::All,
                    1 => nodes.pop().unwrap(),
                    _ => Node::And(nodes),
                }
            }
            Expr::Or(parts) => {
                let mut nodes = parts
                    .iter()
                    .map(|p| self.compile_expr(p))
                    .collect::<Result<Vec<_>, _>>()?;
                if nodes.iter().any(|n| matches!(n, Node::All)) {
                    return Ok(Node::All);
                }
                nodes.retain(|n| !matches!(n, Node::Nothing));
                nodes.sort_by_key(Node::cost);
                match nodes.len() {
                    0 => Node::Nothing,
                    1 => nodes.pop().unwrap(),
                    _ => Node::Or(nodes),
                }
            }
            Expr::Filter(f) => self.compile_filter(f)?,
        })
    }

    fn compile_filter(&mut self, filter: &Filter) -> Result<Node, QueryError> {
        let bytes = |list: &[String]| list.iter().map(|e| e.as_bytes().to_vec()).collect();
        let leaf = match filter {
            Filter::Text(t) => return self.compile_text(t),
            Filter::Kind(k) => Leaf::Kind(*k),
            Filter::Ext(exts) => Leaf::Ext(bytes(exts)),
            Filter::FileList(names) => Leaf::FileList(bytes(names)),
            Filter::Size(b) => Leaf::Size(*b),
            Filter::Date(field, b) => Leaf::Date(*field, *b),
            Filter::Depth(b) => Leaf::Depth(*b),
            Filter::Len(b) => Leaf::Len(*b),
            Filter::RunCount(b) => Leaf::RunCount(*b),
            Filter::ChildCount(kind, b) => Leaf::ChildCount(*kind, *b),
            Filter::Child(t) => Leaf::Child(Matcher::new(t, false)?),
            Filter::Attrib(attrs) => Leaf::Attrib(attrs.clone()),
            Filter::Type(t) => Leaf::Type(Matcher::new(t, false)?),
            Filter::Parent(path) => Leaf::Parent {
                slot: self.folder_slot(path),
            },
            Filter::Under(path) => Leaf::Under {
                slot: self.folder_slot(path),
            },
            Filter::Root => {
                self.include_roots = true;
                Leaf::Root
            }
            Filter::Empty => Leaf::Empty,
            Filter::Executable => Leaf::Executable,
            Filter::Count(n) => {
                self.limit = Some(self.limit.map_or(*n, |l| l.min(*n)));
                return Ok(Node::All);
            }
            Filter::Dupe(keys) => {
                self.dupes = Some(keys.clone());
                return Ok(Node::All);
            }
            Filter::Content(c) => Leaf::Content(compile_content(c)?),
            Filter::Property(p, m) => Leaf::Property(
                *p,
                match m {
                    PropertyMatch::Number(b) => PropertyLeaf::Number(*b),
                    PropertyMatch::Text(t) => PropertyLeaf::Text(Matcher::new(t, false)?),
                },
            ),
        };
        Ok(Node::Leaf(leaf))
    }

    fn folder_slot(&mut self, path: &str) -> usize {
        self.folder_slots.push(path.to_owned());
        self.folder_slots.len() - 1
    }

    fn compile_text(&mut self, t: &TextMatch) -> Result<Node, QueryError> {
        let on_path = matches!(t.target, Target::Path | Target::Location);
        let matcher = Matcher::new(t, on_path)?;
        if matcher.matches_everything() {
            return Ok(Node::All);
        }
        let mut path_slot = None;
        if on_path && matcher.is_plain_substring() {
            if let MatcherKind::Sub { finder, .. } = &matcher.kind {
                if finder.needle().len() > MAX_NEEDLE {
                    return Ok(Node::Nothing);
                }
                self.path_slots.push(PathSlot {
                    dfa: Dfa::new(finder.needle()),
                    norm: matcher.norm,
                });
                path_slot = Some(self.path_slots.len() - 1);
            }
        }
        Ok(Node::Leaf(Leaf::Text(TextLeaf {
            matcher,
            target: t.target,
            path_slot,
        })))
    }
}

fn compile_content(c: &ContentMatch) -> Result<ContentLeaf, QueryError> {
    let kind = if c.regex {
        ContentKind::Regex(
            RegexBuilder::new(&c.pattern)
                .case_insensitive(!c.case_sensitive)
                .multi_line(true)
                .build()
                .map_err(|e| QueryError::Regex(e.to_string()))?,
        )
    } else {
        let pattern = if c.case_sensitive {
            c.pattern.as_bytes().to_vec()
        } else {
            fold::fold(c.pattern.as_bytes())
        };
        ContentKind::Plain {
            finder: Box::new(memmem::Finder::new(&pattern).into_owned()),
            whole_word: c.whole_word,
        }
    };
    Ok(ContentLeaf {
        case: c.case_sensitive,
        kind,
    })
}

/// Keeps the hits whose dupe key occurs more than once among the hits.
fn retain_dupes(hits: &mut Vec<Hit>, volumes: &[(u16, &Index)], keys: &[DupeKey]) {
    let index_of = |vol: u16| volumes.iter().find(|(v, _)| *v == vol).map(|(_, ix)| *ix);
    let key = |hit: &Hit| -> Option<Vec<u8>> {
        let ix = index_of(hit.vol)?;
        let mut k = Vec::new();
        for part in keys {
            match part {
                DupeKey::Name => k.extend_from_slice(ix.folded_name(hit.id)),
                DupeKey::Size => k.extend_from_slice(&ix.size(hit.id).to_le_bytes()),
                DupeKey::Stem => k.extend_from_slice(stem(ix.folded_name(hit.id))),
                DupeKey::Extension => k.extend_from_slice(extension(ix.folded_name(hit.id))),
                DupeKey::Modified => k.extend_from_slice(&ix.mtime(hit.id).to_le_bytes()),
                DupeKey::Created => k.extend_from_slice(&ix.btime(hit.id).to_le_bytes()),
            }
            k.push(0);
        }
        Some(k)
    };
    let keyed: Vec<Option<Vec<u8>>> = hits.par_iter().map(key).collect();
    let mut counts: HashMap<&[u8], u32> = HashMap::new();
    for k in keyed.iter().flatten() {
        *counts.entry(k.as_slice()).or_default() += 1;
    }
    let keep: Vec<bool> = keyed
        .iter()
        .map(|k| k.as_ref().is_some_and(|k| counts[k.as_slice()] > 1))
        .collect();
    let mut i = 0;
    hits.retain(|_| {
        i += 1;
        keep[i - 1]
    });
}

/// The name without its extension.
fn stem(name: &[u8]) -> &[u8] {
    match memchr::memrchr(b'.', name) {
        Some(0) | None => name,
        Some(dot) => &name[..dot],
    }
}

/// Everything-style attribute letters of an entry.
pub fn attributes_of(index: &Index, id: EntryId) -> String {
    let mut s = String::new();
    let m = index.mode(id);
    if m & 0o222 == 0 {
        s.push('R');
    }
    if index.name(id).first() == Some(&b'.') {
        s.push('H');
    }
    if index.is_dir(id) {
        s.push('D');
    } else {
        s.push('A');
    }
    if m & mode::S_IFMT == mode::S_IFLNK {
        s.push('L');
    }
    s
}

fn has_attribute(index: &Index, id: EntryId, attr: Attribute) -> bool {
    let m = index.mode(id);
    let hidden = index.name(id).first() == Some(&b'.');
    let readonly = m & 0o222 == 0;
    match attr {
        Attribute::Archive => !index.is_dir(id),
        Attribute::Directory => index.is_dir(id),
        Attribute::Hidden => hidden,
        Attribute::ReadOnly => readonly,
        Attribute::ReparsePoint => m & mode::S_IFMT == mode::S_IFLNK,
        Attribute::Normal => m & mode::S_IFMT == mode::S_IFREG && !hidden && !readonly,
        Attribute::Unsupported => false,
    }
}

/// Per-volume state for one query run.
struct VolumeCtx<'a> {
    vol: u16,
    index: &'a Index,
    query: &'a Query,
    extra: &'a dyn Extra,
    root_depth: u64,
    paths: Vec<PathMemo>,
    folders: Vec<FolderSet>,
}

/// Folders resolved for parent:/under: in one volume.
struct FolderSet {
    ids: HashSet<EntryId>,
    /// The folder contains the whole volume.
    covers_volume: bool,
    /// under: memo per directory: 0 unknown, 1 inside, 2 outside.
    under: Vec<AtomicU8>,
}

impl<'a> VolumeCtx<'a> {
    fn new(query: &'a Query, vol: u16, index: &'a Index, extra: &'a dyn Extra) -> Self {
        let root = index.root_path();
        let root_depth = root.split(|&b| b == b'/').filter(|c| !c.is_empty()).count() as u64;
        let paths = query
            .path_slots
            .iter()
            .map(|s| PathMemo::new(&s.dfa, index, s.norm))
            .collect();
        let folders = query
            .folder_slots
            .iter()
            .map(|path| {
                let covers_volume = is_dir_prefix(path.as_bytes(), root);
                FolderSet {
                    ids: resolve_folded(index, path.as_bytes()),
                    covers_volume,
                    under: (0..if covers_volume { 0 } else { index.len() })
                        .map(|_| AtomicU8::new(0))
                        .collect(),
                }
            })
            .collect();
        VolumeCtx {
            vol,
            index,
            query,
            extra,
            root_depth,
            paths,
            folders,
        }
    }

    fn eval(&self, node: &Node, id: EntryId, buf: &mut Vec<u8>) -> bool {
        match node {
            Node::All => true,
            Node::Nothing => false,
            Node::And(parts) => parts.iter().all(|n| self.eval(n, id, buf)),
            Node::Or(parts) => parts.iter().any(|n| self.eval(n, id, buf)),
            Node::Not(inner) => !self.eval(inner, id, buf),
            Node::Leaf(leaf) => self.eval_leaf(leaf, id, buf),
        }
    }

    /// Does the name of `id` match? Uses the folded arena when possible.
    fn name_matches(&self, m: &Matcher, id: EntryId, buf: &mut Vec<u8>) -> bool {
        if m.norm.is_folded() && !m.is_regex() {
            return m.is_match(self.index.folded_name(id));
        }
        m.is_match(m.prepare(self.index.name(id), buf))
    }

    fn text(&self, leaf: &TextLeaf, id: EntryId, buf: &mut Vec<u8>) -> bool {
        let ix = self.index;
        let m = &leaf.matcher;
        match leaf.target {
            Target::Name => self.name_matches(m, id, buf),
            Target::Stem => {
                // Cut the extension off the original name: normalization may
                // drop the dot.
                if m.norm.is_folded() && !m.is_regex() && ix.name(id).is_ascii() {
                    return m.is_match(stem(ix.folded_name(id)));
                }
                m.is_match(m.prepare(stem(ix.name(id)), buf))
            }
            Target::Path | Target::Location => {
                let of = if leaf.target == Target::Location {
                    if id == ROOT {
                        return false;
                    }
                    ix.parent(id)
                } else {
                    id
                };
                if let Some(slot) = leaf.path_slot {
                    let dfa = &self.query.path_slots[slot].dfa;
                    let state = if leaf.target == Target::Location {
                        self.paths[slot].dir_state(dfa, ix, of)
                    } else {
                        self.paths[slot].state(dfa, ix, of)
                    };
                    return dfa.accepts(state);
                }
                let mut path = Vec::new();
                ix.path_into(of, &mut path);
                m.is_match(m.prepare(&path, buf))
            }
        }
    }

    fn eval_leaf(&self, leaf: &Leaf, id: EntryId, buf: &mut Vec<u8>) -> bool {
        let ix = self.index;
        match leaf {
            Leaf::Text(t) => self.text(t, id, buf),
            Leaf::Kind(Kind::File) => !ix.is_dir(id),
            Leaf::Kind(Kind::Folder) => ix.is_dir(id),
            Leaf::Ext(exts) => !ix.is_dir(id) && exts.contains(extension(ix.folded_name(id))),
            Leaf::FileList(names) => names.contains(ix.folded_name(id)),
            Leaf::Size(b) => match ix.is_dir(id) {
                false => b.contains(ix.size(id)),
                true => self
                    .extra
                    .folder_size(self.vol, id)
                    .is_some_and(|s| b.contains(s)),
            },
            Leaf::Date(field, b) => {
                let value = match field {
                    DateField::Modified => ix.mtime(id),
                    DateField::Created => ix.btime(id),
                    DateField::Accessed => ix.atime(id),
                    DateField::RecentlyChanged => ix.changed(id),
                    DateField::Run => self.extra.run_info(self.vol, id).last,
                };
                // 0 means unknown (never run, no birth time): only `unknown` matches.
                let unknown = (b.lo, b.hi) == crate::query::UNKNOWN_DATE;
                if value == 0 {
                    unknown
                } else {
                    !unknown && b.contains(value)
                }
            }
            Leaf::Depth(b) => b.contains(self.root_depth + ix.depth(id) as u64),
            Leaf::Len(b) => b.contains(char_count(ix.name(id))),
            Leaf::RunCount(b) => b.contains(self.extra.run_info(self.vol, id).count as u64),
            Leaf::ChildCount(kind, b) => {
                ix.is_dir(id)
                    && b.contains(
                        ix.children(id)
                            .filter(|&c| match kind {
                                ChildKind::All => true,
                                ChildKind::Files => !ix.is_dir(c),
                                ChildKind::Folders => ix.is_dir(c),
                            })
                            .count() as u64,
                    )
            }
            Leaf::Child(m) => {
                ix.is_dir(id) && ix.children(id).any(|c| self.name_matches(m, c, buf))
            }
            Leaf::Attrib(attrs) => attrs.iter().all(|a| has_attribute(ix, id, *a)),
            Leaf::Type(m) => {
                let exec = ix.mode(id) & 0o111 != 0;
                let ty = crate::types::type_name(ix.name(id), ix.is_dir(id), exec);
                m.is_match(m.prepare(ty.as_bytes(), buf))
            }
            Leaf::Parent { slot } => id != ROOT && self.folders[*slot].ids.contains(&ix.parent(id)),
            Leaf::Under { slot } => id != ROOT && self.under(&self.folders[*slot], ix.parent(id)),
            Leaf::Root => id == ROOT,
            Leaf::Empty => ix.is_dir(id) && !ix.has_children(id),
            Leaf::Executable => {
                ix.mode(id) & mode::S_IFMT == mode::S_IFREG && ix.mode(id) & 0o111 != 0
            }
            Leaf::Content(c) => self.content(c, id),
            Leaf::Property(p, m) => self.property(*p, m, id, buf),
        }
    }

    fn readable_file(&self, id: EntryId) -> bool {
        let ix = self.index;
        ix.mode(id) & mode::S_IFMT == mode::S_IFREG && self.extra.can_read(self.vol, ix, id)
    }

    fn content(&self, c: &ContentLeaf, id: EntryId) -> bool {
        let ix = self.index;
        if !self.readable_file(id) || ix.size(id) > props::MAX_CONTENT {
            return false;
        }
        let Ok(data) = props::read_content(&ix.path(id), props::MAX_CONTENT) else {
            return false;
        };
        match &c.kind {
            ContentKind::Regex(re) => re.is_match(&data),
            ContentKind::Plain { finder, whole_word } => {
                let text = if c.case { data } else { fold::fold(&data) };
                let n = finder.needle().len();
                if *whole_word {
                    finder
                        .find_iter(&text)
                        .any(|p| at_word_start(&text, p) && at_word_end(&text, p + n))
                } else {
                    finder.find(&text).is_some()
                }
            }
        }
    }

    fn property(&self, p: Property, m: &PropertyLeaf, id: EntryId, buf: &mut Vec<u8>) -> bool {
        let ix = self.index;
        if !props::has_props(ix.name(id)) || !self.readable_file(id) {
            return false;
        }
        let props = props::cached_props(&ix.path(id), ix.mtime(id), ix.size(id));
        let number = match p {
            Property::Width => props.width,
            Property::Height => props.height,
            Property::BitDepth => props.bit_depth,
            Property::Length => props.length_ms,
            Property::Track => props.track,
            _ => None,
        };
        let text = match p {
            Property::Album => props.album.as_deref(),
            Property::Artist => props.artist.as_deref(),
            Property::Title => props.title.as_deref(),
            Property::Genre => props.genre.as_deref(),
            Property::Comment => props.comment.as_deref(),
            _ => None,
        };
        match m {
            PropertyLeaf::Number(b) => number.is_some_and(|v| b.contains(v)),
            PropertyLeaf::Text(matcher) => {
                text.is_some_and(|t| matcher.is_match(matcher.prepare(t.as_bytes(), buf)))
            }
        }
    }

    /// Is `dir` one of the folders or below one of them?
    fn under(&self, set: &FolderSet, dir: EntryId) -> bool {
        if set.covers_volume {
            return true;
        }
        let mut chain = Vec::new();
        let mut cur = dir;
        let inside = loop {
            if set.ids.contains(&cur) {
                break true;
            }
            if cur == ROOT {
                break false;
            }
            match set.under[cur as usize].load(Ordering::Relaxed) {
                1 => break true,
                2 => break false,
                _ => {}
            }
            chain.push(cur);
            cur = self.index.parent(cur);
        };
        let mark = if inside { 1 } else { 2 };
        for d in chain {
            set.under[d as usize].store(mark, Ordering::Relaxed);
        }
        inside
    }
}

/// Resolves an absolute folder path case-insensitively. Several entries may
/// match when names differ only in case.
fn resolve_folded(index: &Index, path: &[u8]) -> HashSet<EntryId> {
    let mut found = HashSet::new();
    let Some(rel) = index.relative(path) else {
        return found;
    };
    let mut current = vec![ROOT];
    for comp in rel.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
        let comp = fold::fold(comp);
        current = current
            .iter()
            .flat_map(|&dir| index.children(dir))
            .filter(|&c| index.is_dir(c) && index.folded_name(c) == comp.as_slice())
            .collect();
        if current.is_empty() {
            return found;
        }
    }
    found.extend(current);
    found
}

/// True if `prefix` is `root` or one of its ancestors.
fn is_dir_prefix(prefix: &[u8], root: &[u8]) -> bool {
    if prefix == b"/" {
        return true;
    }
    let p = fold::fold(prefix);
    let r = fold::fold(root);
    r.starts_with(&p) && (r.len() == p.len() || r[p.len()] == b'/')
}

fn char_count(name: &[u8]) -> u64 {
    name.iter().filter(|&&b| b & 0xc0 != 0x80).count() as u64
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod features_tests;
