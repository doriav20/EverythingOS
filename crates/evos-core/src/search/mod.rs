//! Parallel query execution over volume indexes.
//!
//! A parsed [`Expr`] is compiled into a tree of predicates once. Execution
//! splits every volume into blocks of ids that rayon evaluates in parallel;
//! predicates that need per-volume state (path automata, resolved folders)
//! get it from a [`VolumeCtx`] built at the start of each run.

mod path_dfa;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use hashbrown::HashSet;
use memchr::memmem;
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};

use crate::fold;
use crate::query::{Bounds, Expr, Filter, Kind, PatternKind, Target, TextMatch};
use crate::store::{mode, EntryId, Index, ROOT};
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

/// A compiled query, reusable across runs and volumes.
pub struct Query {
    root: Node,
    path_slots: Vec<PathSlot>,
    folder_slots: Vec<String>,
}

struct PathSlot {
    dfa: Dfa,
    folded: bool,
}

enum Node {
    All,
    Nothing,
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    Leaf(Leaf),
}

enum Leaf {
    NameSub {
        finder: Box<memmem::Finder<'static>>,
        folded: bool,
        whole_word: bool,
    },
    NameGlob {
        glob: Wildcard,
        folded: bool,
    },
    NameRegex(Regex),
    PathSub {
        slot: usize,
    },
    PathGlob {
        glob: Wildcard,
        folded: bool,
        name_suffix: Option<Vec<u8>>,
        name_exact: bool,
    },
    PathRegex(Regex),
    Kind(Kind),
    Ext(HashSet<Vec<u8>>),
    Size(Bounds<u64>),
    Modified(Bounds<i64>),
    Depth(Bounds<u64>),
    Len(Bounds<u64>),
    Parent {
        slot: usize,
    },
    Under {
        slot: usize,
    },
    Empty,
    Executable,
}

impl Leaf {
    /// Rough relative cost, used to evaluate cheap predicates first.
    fn cost(&self) -> u8 {
        match self {
            Leaf::Kind(_)
            | Leaf::Size(_)
            | Leaf::Modified(_)
            | Leaf::Empty
            | Leaf::Executable
            | Leaf::Len(_) => 1,
            Leaf::Ext(_) | Leaf::Parent { .. } => 2,
            Leaf::NameSub { .. } | Leaf::NameGlob { .. } => 3,
            Leaf::Depth(_) | Leaf::Under { .. } => 4,
            Leaf::PathSub { .. } => 5,
            Leaf::NameRegex(_) => 6,
            Leaf::PathGlob { .. } | Leaf::PathRegex(_) => 8,
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
        };
        q.root = q.compile_expr(expr)?;
        Ok(q)
    }

    /// Runs the query over all volumes. Returns `None` if cancelled.
    pub fn run(
        &self,
        volumes: &[(u16, &Index)],
        filter: Option<EntryFilter<'_>>,
        cancel: &AtomicBool,
    ) -> Option<Vec<Hit>> {
        const BLOCK: usize = 16 * 1024;
        let mut hits = Vec::new();
        for &(vol, index) in volumes {
            let ctx = VolumeCtx::new(self, index);
            let blocks = index.len().div_ceil(BLOCK);
            let parts: Vec<Vec<Hit>> = (0..blocks)
                .into_par_iter()
                .map_init(Vec::new, |buf, block| {
                    if cancel.load(Ordering::Relaxed) {
                        return Vec::new();
                    }
                    let start = (block * BLOCK).max(1) as EntryId; // skip the volume root
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
        let ctxs: Vec<(u16, &Index, VolumeCtx)> = volumes
            .iter()
            .map(|&(vol, ix)| (vol, ix, VolumeCtx::new(self, ix)))
            .collect();
        let hits: Vec<Hit> = candidates
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
        let leaf = match filter {
            Filter::Text(t) => return self.compile_text(t),
            Filter::Kind(k) => Leaf::Kind(*k),
            Filter::Ext(exts) => Leaf::Ext(exts.iter().map(|e| e.as_bytes().to_vec()).collect()),
            Filter::Size(b) => Leaf::Size(*b),
            Filter::Modified(b) => Leaf::Modified(*b),
            Filter::Depth(b) => Leaf::Depth(*b),
            Filter::Len(b) => Leaf::Len(*b),
            Filter::Parent(path) => Leaf::Parent {
                slot: self.folder_slot(path),
            },
            Filter::Under(path) => Leaf::Under {
                slot: self.folder_slot(path),
            },
            Filter::Empty => Leaf::Empty,
            Filter::Executable => Leaf::Executable,
        };
        Ok(Node::Leaf(leaf))
    }

    fn folder_slot(&mut self, path: &str) -> usize {
        self.folder_slots.push(path.to_owned());
        self.folder_slots.len() - 1
    }

    fn compile_text(&mut self, t: &TextMatch) -> Result<Node, QueryError> {
        let folded = !t.case_sensitive;
        let pattern: Vec<u8> = if folded {
            fold::fold(t.pattern.as_bytes())
        } else {
            t.pattern.as_bytes().to_vec()
        };
        let leaf = match (t.kind, t.target) {
            (PatternKind::Substring, _) if pattern.is_empty() => return Ok(Node::All),
            (PatternKind::Substring, Target::Name) => Leaf::NameSub {
                finder: Box::new(memmem::Finder::new(&pattern).into_owned()),
                folded,
                whole_word: t.whole_word,
            },
            (PatternKind::Substring, Target::Path) => {
                if pattern.len() > MAX_NEEDLE {
                    return Ok(Node::Nothing);
                }
                self.path_slots.push(PathSlot {
                    dfa: Dfa::new(&pattern),
                    folded,
                });
                Leaf::PathSub {
                    slot: self.path_slots.len() - 1,
                }
            }
            (PatternKind::Wildcard, Target::Name) => {
                let glob = Wildcard::new(&pattern);
                if glob.matches_everything() {
                    return Ok(Node::All);
                }
                Leaf::NameGlob { glob, folded }
            }
            (PatternKind::Wildcard, Target::Path) => {
                // The literal tail after the last wildcard must end the path,
                // which lets most entries be rejected by name alone.
                let tail_start = pattern
                    .iter()
                    .rposition(|&b| b == b'*' || b == b'?')
                    .map_or(0, |i| i + 1);
                let tail = &pattern[tail_start..];
                let (name_suffix, name_exact) = match tail.iter().rposition(|&b| b == b'/') {
                    _ if tail.is_empty() => (None, false),
                    Some(slash) => (Some(tail[slash + 1..].to_vec()), true),
                    None => (Some(tail.to_vec()), false),
                };
                Leaf::PathGlob {
                    glob: Wildcard::new(&pattern),
                    folded,
                    name_suffix,
                    name_exact,
                }
            }
            (PatternKind::Regex, target) => {
                let re = RegexBuilder::new(&t.pattern)
                    .case_insensitive(!t.case_sensitive)
                    .build()
                    .map_err(|e| QueryError::Regex(e.to_string()))?;
                match target {
                    Target::Name => Leaf::NameRegex(re),
                    Target::Path => Leaf::PathRegex(re),
                }
            }
        };
        Ok(Node::Leaf(leaf))
    }
}

/// Per-volume state for one query run.
struct VolumeCtx<'a> {
    index: &'a Index,
    query: &'a Query,
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
    fn new(query: &'a Query, index: &'a Index) -> Self {
        let root = index.root_path();
        let root_depth = root.split(|&b| b == b'/').filter(|c| !c.is_empty()).count() as u64;
        let paths = query
            .path_slots
            .iter()
            .map(|s| PathMemo::new(&s.dfa, index, s.folded))
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
            index,
            query,
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

    fn name(&self, id: EntryId, folded: bool) -> &'a [u8] {
        if folded {
            self.index.folded_name(id)
        } else {
            self.index.name(id)
        }
    }

    fn path<'b>(&self, id: EntryId, folded: bool, buf: &'b mut Vec<u8>) -> &'b [u8] {
        buf.clear();
        if folded {
            self.index.folded_path_into(id, buf);
        } else {
            self.index.path_into(id, buf);
        }
        buf
    }

    fn eval_leaf(&self, leaf: &Leaf, id: EntryId, buf: &mut Vec<u8>) -> bool {
        let ix = self.index;
        match leaf {
            Leaf::NameSub {
                finder,
                folded,
                whole_word,
            } => {
                let name = self.name(id, *folded);
                if *whole_word {
                    contains_word(finder, name)
                } else {
                    finder.find(name).is_some()
                }
            }
            Leaf::NameGlob { glob, folded } => glob.is_match(self.name(id, *folded)),
            Leaf::NameRegex(re) => re.is_match(ix.name(id)),
            Leaf::PathSub { slot } => {
                let dfa = &self.query.path_slots[*slot].dfa;
                dfa.accepts(self.paths[*slot].state(dfa, ix, id))
            }
            Leaf::PathGlob {
                glob,
                folded,
                name_suffix,
                name_exact,
            } => {
                if let Some(suffix) = name_suffix {
                    let name = self.name(id, *folded);
                    let ok = if *name_exact {
                        name == suffix.as_slice()
                    } else {
                        name.ends_with(suffix)
                    };
                    if !ok {
                        return false;
                    }
                }
                glob.is_match(self.path(id, *folded, buf))
            }
            Leaf::PathRegex(re) => re.is_match(self.path(id, false, buf)),
            Leaf::Kind(Kind::File) => !ix.is_dir(id),
            Leaf::Kind(Kind::Folder) => ix.is_dir(id),
            Leaf::Ext(exts) => !ix.is_dir(id) && exts.contains(extension(ix.folded_name(id))),
            Leaf::Size(b) => !ix.is_dir(id) && b.contains(ix.size(id)),
            Leaf::Modified(b) => b.contains(ix.mtime(id)),
            Leaf::Depth(b) => b.contains(self.root_depth + ix.depth(id) as u64),
            Leaf::Len(b) => b.contains(char_count(ix.name(id))),
            Leaf::Parent { slot } => self.folders[*slot].ids.contains(&ix.parent(id)),
            Leaf::Under { slot } => self.under(&self.folders[*slot], ix.parent(id)),
            Leaf::Empty => ix.is_dir(id) && !ix.has_children(id),
            Leaf::Executable => {
                ix.mode(id) & mode::S_IFMT == mode::S_IFREG && ix.mode(id) & 0o111 != 0
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

fn extension(name: &[u8]) -> &[u8] {
    match memchr::memrchr(b'.', name) {
        Some(0) | None => b"",
        Some(dot) => &name[dot + 1..],
    }
}

fn char_count(name: &[u8]) -> u64 {
    name.iter().filter(|&&b| b & 0xc0 != 0x80).count() as u64
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn contains_word(finder: &memmem::Finder<'_>, hay: &[u8]) -> bool {
    let n = finder.needle().len();
    finder.find_iter(hay).any(|pos| {
        let before = pos == 0 || !is_word_byte(hay[pos - 1]);
        let after = pos + n == hay.len() || !is_word_byte(hay[pos + n]);
        before && after
    })
}

#[cfg(test)]
mod tests;
