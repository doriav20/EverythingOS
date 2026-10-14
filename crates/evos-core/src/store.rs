//! Columnar in-memory index of one volume (a filesystem or mounted share).
//!
//! Entries are stored struct-of-arrays style and addressed by a dense
//! [`EntryId`]. Ids are stable for the lifetime of an entry (renames and
//! moves keep the id); deleted entries become tombstones until
//! [`Index::compact`] renumbers everything.
//!
//! Entry `0` is the volume root. Its path is the volume's mount path and its
//! name is empty.

use std::hash::BuildHasher;

use hashbrown::{DefaultHashBuilder, HashTable};

use crate::fold::fold_into;

/// Dense identifier of an entry inside one [`Index`].
pub type EntryId = u32;

/// Sentinel for "no entry" in the parent and sibling columns.
pub const NO_ENTRY: EntryId = u32::MAX;

/// Id of the volume root entry.
pub const ROOT: EntryId = 0;

/// File type bits of `st_mode`, kept here so this crate stays libc-free.
pub mod mode {
    pub const S_IFMT: u32 = 0o170_000;
    pub const S_IFDIR: u32 = 0o040_000;
    pub const S_IFREG: u32 = 0o100_000;
    pub const S_IFLNK: u32 = 0o120_000;
}

const FLAG_DELETED: u8 = 1;

/// Metadata stored for every entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// Size in bytes (0 for directories).
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime: i64,
    /// Raw `st_mode`: file type and permission bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Meta {
    pub fn dir(mtime: i64) -> Self {
        Meta {
            size: 0,
            mtime,
            mode: mode::S_IFDIR | 0o755,
            uid: 0,
            gid: 0,
        }
    }

    pub fn file(size: u64, mtime: i64) -> Self {
        Meta {
            size,
            mtime,
            mode: mode::S_IFREG | 0o644,
            uid: 0,
            gid: 0,
        }
    }

    pub fn is_dir(&self) -> bool {
        self.mode & mode::S_IFMT == mode::S_IFDIR
    }
}

/// One entry of a directory listing, as produced by a scanner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedEntry {
    pub name: Vec<u8>,
    pub meta: Meta,
}

/// What [`Index::apply_listing`] changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ListingOutcome {
    /// Subdirectories that did not exist before and must be scanned.
    pub new_dirs: Vec<EntryId>,
    /// Subdirectories that already existed.
    pub existing_dirs: Vec<EntryId>,
    pub added: u32,
    pub removed: u32,
    pub updated: u32,
}

impl ListingOutcome {
    pub fn changed(&self) -> bool {
        self.added + self.removed + self.updated > 0
    }
}

/// The index of one volume.
#[derive(Clone)]
pub struct Index {
    root_path: Vec<u8>,

    pub(crate) parent: Vec<EntryId>,
    pub(crate) first_child: Vec<EntryId>,
    pub(crate) next_sibling: Vec<EntryId>,
    pub(crate) prev_sibling: Vec<EntryId>,
    pub(crate) name_off: Vec<u32>,
    pub(crate) name_len: Vec<u16>,
    pub(crate) fold_off: Vec<u32>,
    pub(crate) fold_len: Vec<u16>,
    pub(crate) size: Vec<u64>,
    pub(crate) mtime: Vec<i64>,
    pub(crate) mode: Vec<u32>,
    pub(crate) uid: Vec<u32>,
    pub(crate) gid: Vec<u32>,
    pub(crate) flags: Vec<u8>,
    pub(crate) names: Vec<u8>,
    pub(crate) folded: Vec<u8>,

    lookup: HashTable<EntryId>,
    hasher: DefaultHashBuilder,
    pub(crate) live: usize,
    pub(crate) garbage_bytes: usize,
    pub(crate) generation: u64,
    pub(crate) epoch: u64,
}

impl Index {
    /// Creates an index containing only the root entry.
    pub fn new(root_path: &[u8], root_meta: Meta) -> Self {
        let mut index = Index {
            root_path: normalize_root(root_path),
            parent: Vec::new(),
            first_child: Vec::new(),
            next_sibling: Vec::new(),
            prev_sibling: Vec::new(),
            name_off: Vec::new(),
            name_len: Vec::new(),
            fold_off: Vec::new(),
            fold_len: Vec::new(),
            size: Vec::new(),
            mtime: Vec::new(),
            mode: Vec::new(),
            uid: Vec::new(),
            gid: Vec::new(),
            flags: Vec::new(),
            names: Vec::new(),
            folded: Vec::new(),
            lookup: HashTable::new(),
            hasher: DefaultHashBuilder::default(),
            live: 0,
            garbage_bytes: 0,
            generation: 0,
            epoch: 0,
        };
        index.push_entry(NO_ENTRY, b"", root_meta);
        index
    }

    /// Absolute path of the volume root, without a trailing slash (except `/`).
    pub fn root_path(&self) -> &[u8] {
        &self.root_path
    }

    /// Number of id slots, including tombstones.
    pub fn len(&self) -> usize {
        self.parent.len()
    }

    /// Always false: an index contains at least its root.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Number of live entries, including the root.
    pub fn live_count(&self) -> usize {
        self.live
    }

    /// Counter bumped by every mutation.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Counter bumped by [`Index::compact`], which invalidates all ids.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn is_alive(&self, id: EntryId) -> bool {
        (id as usize) < self.flags.len() && self.flags[id as usize] & FLAG_DELETED == 0
    }

    pub fn is_dir(&self, id: EntryId) -> bool {
        self.mode[id as usize] & mode::S_IFMT == mode::S_IFDIR
    }

    pub fn name(&self, id: EntryId) -> &[u8] {
        let off = self.name_off[id as usize] as usize;
        &self.names[off..off + self.name_len[id as usize] as usize]
    }

    pub fn folded_name(&self, id: EntryId) -> &[u8] {
        let off = self.fold_off[id as usize] as usize;
        &self.folded[off..off + self.fold_len[id as usize] as usize]
    }

    pub fn parent(&self, id: EntryId) -> EntryId {
        self.parent[id as usize]
    }

    pub fn size(&self, id: EntryId) -> u64 {
        self.size[id as usize]
    }

    pub fn mtime(&self, id: EntryId) -> i64 {
        self.mtime[id as usize]
    }

    pub fn mode(&self, id: EntryId) -> u32 {
        self.mode[id as usize]
    }

    pub fn meta(&self, id: EntryId) -> Meta {
        let i = id as usize;
        Meta {
            size: self.size[i],
            mtime: self.mtime[i],
            mode: self.mode[i],
            uid: self.uid[i],
            gid: self.gid[i],
        }
    }

    /// Returns true if the directory has no live children.
    pub fn has_children(&self, id: EntryId) -> bool {
        self.first_child[id as usize] != NO_ENTRY
    }

    /// Iterates over the live children of `dir`.
    pub fn children(&self, dir: EntryId) -> Children<'_> {
        Children {
            index: self,
            next: self.first_child[dir as usize],
        }
    }

    /// Iterates over all live entry ids.
    pub fn live_ids(&self) -> impl Iterator<Item = EntryId> + '_ {
        (0..self.len() as EntryId).filter(|&id| self.is_alive(id))
    }

    /// Finds the child of `parent` with exactly this name.
    pub fn lookup(&self, parent: EntryId, name: &[u8]) -> Option<EntryId> {
        let hash = self.key_hash(parent, name);
        self.lookup
            .find(hash, |&id| {
                self.parent[id as usize] == parent && self.name(id) == name
            })
            .copied()
    }

    /// Appends the absolute path of `id` to `buf`.
    pub fn path_into(&self, id: EntryId, buf: &mut Vec<u8>) {
        self.path_with(id, buf, |s, id| s.name(id), &self.root_path)
    }

    /// Appends the case-folded absolute path of `id` to `buf`.
    pub fn folded_path_into(&self, id: EntryId, buf: &mut Vec<u8>) {
        let root = crate::fold::fold(&self.root_path);
        self.path_with(id, buf, |s, id| s.folded_name(id), &root)
    }

    /// Returns the absolute path of `id`.
    pub fn path(&self, id: EntryId) -> Vec<u8> {
        let mut buf = Vec::new();
        self.path_into(id, &mut buf);
        buf
    }

    fn path_with<'a>(
        &'a self,
        id: EntryId,
        buf: &mut Vec<u8>,
        name: impl Fn(&'a Self, EntryId) -> &'a [u8],
        root: &[u8],
    ) {
        let mut chain = Vec::new();
        let mut cur = id;
        while cur != ROOT && cur != NO_ENTRY {
            chain.push(cur);
            cur = self.parent[cur as usize];
        }
        buf.extend_from_slice(root);
        for &c in chain.iter().rev() {
            if buf.last() != Some(&b'/') {
                buf.push(b'/');
            }
            buf.extend_from_slice(name(self, c));
        }
    }

    /// Number of path components between the volume root and `id`.
    pub fn depth(&self, id: EntryId) -> u32 {
        let mut depth = 0;
        let mut cur = id;
        while cur != ROOT && cur != NO_ENTRY {
            depth += 1;
            cur = self.parent[cur as usize];
        }
        depth
    }

    /// Resolves an absolute path to an entry, if it is inside this volume.
    pub fn resolve(&self, path: &[u8]) -> Option<EntryId> {
        let rel = self.relative(path)?;
        let mut cur = ROOT;
        for comp in rel.split(|&b| b == b'/').filter(|c| !c.is_empty()) {
            cur = self.lookup(cur, comp)?;
        }
        Some(cur)
    }

    /// Strips the volume root from `path`. Returns `None` if `path` is outside.
    pub fn relative<'p>(&self, path: &'p [u8]) -> Option<&'p [u8]> {
        let root = &self.root_path;
        if root.as_slice() == b"/" {
            return path.strip_prefix(b"/");
        }
        let rest = path.strip_prefix(root.as_slice())?;
        match rest.first() {
            None => Some(rest),
            Some(b'/') => Some(&rest[1..]),
            Some(_) => None,
        }
    }

    /// Inserts a new child. The caller guarantees no child of that name exists.
    pub fn insert(&mut self, parent: EntryId, name: &[u8], meta: Meta) -> EntryId {
        debug_assert!(self.is_alive(parent) && self.is_dir(parent));
        debug_assert!(self.lookup(parent, name).is_none(), "duplicate entry");
        let id = self.push_entry(parent, name, meta);
        self.link(parent, id);
        self.hash_insert(id);
        self.generation += 1;
        id
    }

    /// Inserts or updates the child `name` of `parent`.
    ///
    /// If an entry exists with a different kind (file vs. directory) it is
    /// removed together with its subtree and a fresh entry is created.
    /// Returns the id and whether a new entry was created.
    pub fn upsert(&mut self, parent: EntryId, name: &[u8], meta: Meta) -> (EntryId, bool) {
        if let Some(id) = self.lookup(parent, name) {
            if self.is_dir(id) == meta.is_dir() {
                self.set_meta(id, meta);
                return (id, false);
            }
            self.remove(id);
        }
        (self.insert(parent, name, meta), true)
    }

    /// Replaces the metadata of an entry. Returns true if anything changed.
    pub fn set_meta(&mut self, id: EntryId, meta: Meta) -> bool {
        if self.meta(id) == meta {
            return false;
        }
        let i = id as usize;
        self.size[i] = meta.size;
        self.mtime[i] = meta.mtime;
        self.mode[i] = meta.mode;
        self.uid[i] = meta.uid;
        self.gid[i] = meta.gid;
        self.generation += 1;
        true
    }

    /// Removes an entry and its whole subtree. Returns the number removed.
    pub fn remove(&mut self, id: EntryId) -> usize {
        assert_ne!(id, ROOT, "the volume root cannot be removed");
        if !self.is_alive(id) {
            return 0;
        }
        self.unlink(id);
        let mut removed = 0;
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            let mut child = self.first_child[cur as usize];
            while child != NO_ENTRY {
                stack.push(child);
                child = self.next_sibling[child as usize];
            }
            self.unhash(cur);
            let i = cur as usize;
            self.flags[i] |= FLAG_DELETED;
            self.first_child[i] = NO_ENTRY;
            self.garbage_bytes += self.name_len[i] as usize + self.fold_len[i] as usize;
            removed += 1;
        }
        self.live -= removed;
        self.generation += 1;
        removed
    }

    /// Moves and/or renames an entry, keeping its id and subtree.
    ///
    /// An existing entry at the destination is replaced.
    pub fn rename(&mut self, id: EntryId, new_parent: EntryId, new_name: &[u8]) {
        assert_ne!(id, ROOT, "the volume root cannot be moved");
        debug_assert!(self.is_dir(new_parent));
        if self.parent[id as usize] == new_parent && self.name(id) == new_name {
            return;
        }
        if let Some(existing) = self.lookup(new_parent, new_name) {
            if existing != id {
                self.remove(existing);
            }
        }
        self.unhash(id);
        self.unlink(id);
        let i = id as usize;
        if self.name(id) != new_name {
            self.garbage_bytes += self.name_len[i] as usize + self.fold_len[i] as usize;
            let (off, len, foff, flen) = self.push_name(new_name);
            self.name_off[i] = off;
            self.name_len[i] = len;
            self.fold_off[i] = foff;
            self.fold_len[i] = flen;
        }
        self.parent[i] = new_parent;
        self.link(new_parent, id);
        self.hash_insert(id);
        self.generation += 1;
    }

    /// Reconciles the children of `dir` with a fresh directory listing.
    pub fn apply_listing(&mut self, dir: EntryId, listing: &[ListedEntry]) -> ListingOutcome {
        let mut outcome = ListingOutcome::default();
        let mut keep = hashbrown::HashSet::with_capacity(listing.len());
        for entry in listing {
            let existing = self.lookup(dir, &entry.name);
            let id = match existing {
                Some(id) if self.is_dir(id) == entry.meta.is_dir() => {
                    if self.set_meta(id, entry.meta) {
                        outcome.updated += 1;
                    }
                    if entry.meta.is_dir() {
                        outcome.existing_dirs.push(id);
                    }
                    id
                }
                _ => {
                    if let Some(old) = existing {
                        outcome.removed += self.remove(old) as u32;
                    }
                    let id = self.insert(dir, &entry.name, entry.meta);
                    outcome.added += 1;
                    if entry.meta.is_dir() {
                        outcome.new_dirs.push(id);
                    }
                    id
                }
            };
            keep.insert(id);
        }
        let stale: Vec<EntryId> = self.children(dir).filter(|id| !keep.contains(id)).collect();
        for id in stale {
            outcome.removed += self.remove(id) as u32;
        }
        outcome
    }

    /// Bytes of name storage held by deleted or renamed entries.
    pub fn garbage_bytes(&self) -> usize {
        self.garbage_bytes
    }

    /// Fraction of id slots occupied by tombstones.
    pub fn tombstone_ratio(&self) -> f64 {
        1.0 - self.live as f64 / self.len() as f64
    }

    /// Returns true when tombstones or stale name bytes waste enough memory
    /// that [`Index::compact`] is worth its cost.
    pub fn should_compact(&self) -> bool {
        self.len() > 4096
            && (self.tombstone_ratio() > 0.25 || self.garbage_bytes > self.names.len() / 2)
    }

    /// Drops tombstones and unused name bytes, renumbering all entries.
    ///
    /// Returns a map from old to new ids (`NO_ENTRY` for removed entries) so
    /// that callers can translate ids they hold. Bumps [`Index::epoch`].
    pub fn compact(&mut self) -> Vec<EntryId> {
        let mut remap = vec![NO_ENTRY; self.len()];
        let mut next: EntryId = 0;
        for (old, slot) in remap.iter_mut().enumerate() {
            if self.flags[old] & FLAG_DELETED == 0 {
                *slot = next;
                next += 1;
            }
        }
        let map = |id: EntryId| {
            if id == NO_ENTRY {
                NO_ENTRY
            } else {
                remap[id as usize]
            }
        };

        let live = next as usize;
        let mut out = Index::new(&self.root_path, self.meta(ROOT));
        out.clear_columns(live);
        out.names
            .reserve(self.names.len().saturating_sub(self.garbage_bytes / 2));
        out.folded
            .reserve(self.folded.len().saturating_sub(self.garbage_bytes / 2));
        for old in (0..self.len()).filter(|&i| remap[i] != NO_ENTRY) {
            let id = old as EntryId;
            out.parent.push(map(self.parent[old]));
            out.first_child.push(map(self.first_child[old]));
            out.next_sibling.push(map(self.next_sibling[old]));
            out.prev_sibling.push(map(self.prev_sibling[old]));
            out.name_off.push(arena_offset(out.names.len()));
            out.name_len.push(self.name_len[old]);
            out.names.extend_from_slice(self.name(id));
            out.fold_off.push(arena_offset(out.folded.len()));
            out.fold_len.push(self.fold_len[old]);
            out.folded.extend_from_slice(self.folded_name(id));
            out.size.push(self.size[old]);
            out.mtime.push(self.mtime[old]);
            out.mode.push(self.mode[old]);
            out.uid.push(self.uid[old]);
            out.gid.push(self.gid[old]);
            out.flags.push(self.flags[old]);
        }
        out.live = live;
        out.rebuild_lookup();
        out.generation = self.generation + 1;
        out.epoch = self.epoch + 1;
        *self = out;
        remap
    }

    /// Empties every column, keeping room for `capacity` entries.
    pub(crate) fn clear_columns(&mut self, capacity: usize) {
        macro_rules! reset {
            ($($col:ident),*) => {$( self.$col = Vec::with_capacity(capacity); )*};
        }
        reset!(
            parent,
            first_child,
            next_sibling,
            prev_sibling,
            name_off,
            name_len,
            fold_off,
            fold_len,
            size,
            mtime,
            mode,
            uid,
            gid,
            flags
        );
        self.names.clear();
        self.folded.clear();
        self.lookup.clear();
        self.live = 0;
        self.garbage_bytes = 0;
    }

    /// Rebuilds the (parent, name) lookup table from the columns.
    pub(crate) fn rebuild_lookup(&mut self) {
        self.lookup = HashTable::with_capacity(self.live);
        for id in 1..self.len() as EntryId {
            if self.is_alive(id) {
                self.hash_insert(id);
            }
        }
    }

    fn key_hash(&self, parent: EntryId, name: &[u8]) -> u64 {
        self.hasher.hash_one((parent, name))
    }

    fn hash_insert(&mut self, id: EntryId) {
        let hash = self.key_hash(self.parent[id as usize], self.name(id));
        let (parents, names, name_off, name_len) =
            (&self.parent, &self.names, &self.name_off, &self.name_len);
        let hasher = &self.hasher;
        self.lookup.insert_unique(hash, id, |&other| {
            let i = other as usize;
            let off = name_off[i] as usize;
            hasher.hash_one((parents[i], &names[off..off + name_len[i] as usize]))
        });
    }

    fn unhash(&mut self, id: EntryId) {
        let hash = self.key_hash(self.parent[id as usize], self.name(id));
        if let Ok(entry) = self.lookup.find_entry(hash, |&other| other == id) {
            entry.remove();
        }
    }

    fn link(&mut self, parent: EntryId, id: EntryId) {
        let head = self.first_child[parent as usize];
        self.next_sibling[id as usize] = head;
        self.prev_sibling[id as usize] = NO_ENTRY;
        if head != NO_ENTRY {
            self.prev_sibling[head as usize] = id;
        }
        self.first_child[parent as usize] = id;
    }

    fn unlink(&mut self, id: EntryId) {
        let i = id as usize;
        let (prev, next) = (self.prev_sibling[i], self.next_sibling[i]);
        if prev != NO_ENTRY {
            self.next_sibling[prev as usize] = next;
        } else {
            let parent = self.parent[i];
            if parent != NO_ENTRY {
                self.first_child[parent as usize] = next;
            }
        }
        if next != NO_ENTRY {
            self.prev_sibling[next as usize] = prev;
        }
        self.prev_sibling[i] = NO_ENTRY;
        self.next_sibling[i] = NO_ENTRY;
    }

    fn push_name(&mut self, name: &[u8]) -> (u32, u16, u32, u16) {
        let len = u16::try_from(name.len()).expect("filename longer than 65535 bytes");
        let off = arena_offset(self.names.len());
        self.names.extend_from_slice(name);
        let foff = arena_offset(self.folded.len());
        fold_into(name, &mut self.folded);
        let flen = u16::try_from(self.folded.len() - foff as usize)
            .expect("folded filename longer than 65535 bytes");
        (off, len, foff, flen)
    }

    fn push_entry(&mut self, parent: EntryId, name: &[u8], meta: Meta) -> EntryId {
        let id = EntryId::try_from(self.parent.len())
            .ok()
            .filter(|&id| id != NO_ENTRY)
            .expect("volume index is full (2^32 entries)");
        let (off, len, foff, flen) = self.push_name(name);
        self.parent.push(parent);
        self.first_child.push(NO_ENTRY);
        self.next_sibling.push(NO_ENTRY);
        self.prev_sibling.push(NO_ENTRY);
        self.name_off.push(off);
        self.name_len.push(len);
        self.fold_off.push(foff);
        self.fold_len.push(flen);
        self.size.push(meta.size);
        self.mtime.push(meta.mtime);
        self.mode.push(meta.mode);
        self.uid.push(meta.uid);
        self.gid.push(meta.gid);
        self.flags.push(0);
        self.live += 1;
        id
    }
}

/// Iterator over the children of a directory.
pub struct Children<'a> {
    index: &'a Index,
    next: EntryId,
}

impl Iterator for Children<'_> {
    type Item = EntryId;

    fn next(&mut self) -> Option<EntryId> {
        if self.next == NO_ENTRY {
            return None;
        }
        let cur = self.next;
        self.next = self.index.next_sibling[cur as usize];
        Some(cur)
    }
}

/// Extension of a filename without the dot; empty for none and for dotfiles.
pub fn extension(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(0) | None => b"",
        Some(dot) => &name[dot + 1..],
    }
}

fn arena_offset(len: usize) -> u32 {
    u32::try_from(len).expect("name arena exceeds 4 GiB; compact the index")
}

fn normalize_root(path: &[u8]) -> Vec<u8> {
    let mut root = path.to_vec();
    while root.len() > 1 && root.last() == Some(&b'/') {
        root.pop();
    }
    if root.is_empty() {
        root.push(b'/');
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (Index, EntryId, EntryId, EntryId) {
        let mut ix = Index::new(b"/data/", Meta::dir(0));
        let docs = ix.insert(ROOT, b"Docs", Meta::dir(1));
        let a = ix.insert(docs, b"a.txt", Meta::file(10, 2));
        let b = ix.insert(docs, b"B.TXT", Meta::file(20, 3));
        (ix, docs, a, b)
    }

    #[test]
    fn insert_and_lookup() {
        let (ix, docs, a, b) = sample();
        assert_eq!(ix.root_path(), b"/data");
        assert_eq!(ix.lookup(ROOT, b"Docs"), Some(docs));
        assert_eq!(ix.lookup(docs, b"a.txt"), Some(a));
        assert_eq!(ix.lookup(docs, b"b.txt"), None, "lookup is case-sensitive");
        assert_eq!(ix.folded_name(b), b"b.txt");
        assert_eq!(ix.live_count(), 4);
        let mut kids: Vec<_> = ix.children(docs).collect();
        kids.sort();
        assert_eq!(kids, vec![a, b]);
    }

    #[test]
    fn paths_and_resolution() {
        let (ix, docs, a, _) = sample();
        assert_eq!(ix.path(ROOT), b"/data");
        assert_eq!(ix.path(a), b"/data/Docs/a.txt");
        assert_eq!(ix.resolve(b"/data/Docs/a.txt"), Some(a));
        assert_eq!(ix.resolve(b"/data/Docs/"), Some(docs));
        assert_eq!(ix.resolve(b"/data"), Some(ROOT));
        assert_eq!(ix.resolve(b"/database"), None);
        assert_eq!(ix.depth(a), 2);
        let mut folded = Vec::new();
        ix.folded_path_into(docs, &mut folded);
        assert_eq!(folded, b"/data/docs");
    }

    #[test]
    fn root_slash_volume_paths() {
        let mut ix = Index::new(b"/", Meta::dir(0));
        let etc = ix.insert(ROOT, b"etc", Meta::dir(0));
        let f = ix.insert(etc, b"hosts", Meta::file(1, 0));
        assert_eq!(ix.path(f), b"/etc/hosts");
        assert_eq!(ix.resolve(b"/etc/hosts"), Some(f));
    }

    #[test]
    fn remove_subtree() {
        let (mut ix, docs, a, b) = sample();
        let keep = ix.insert(ROOT, b"keep", Meta::file(1, 1));
        assert_eq!(ix.remove(docs), 3);
        assert!(!ix.is_alive(docs) && !ix.is_alive(a) && !ix.is_alive(b));
        assert!(ix.is_alive(keep));
        assert_eq!(ix.lookup(ROOT, b"Docs"), None);
        assert_eq!(ix.children(ROOT).collect::<Vec<_>>(), vec![keep]);
        assert_eq!(ix.live_count(), 2);
        assert_eq!(ix.remove(docs), 0, "removing twice is a no-op");
    }

    #[test]
    fn rename_keeps_id_and_subtree() {
        let (mut ix, docs, a, _) = sample();
        let other = ix.insert(ROOT, b"other", Meta::dir(0));
        ix.rename(docs, other, b"Moved");
        assert_eq!(ix.path(a), b"/data/other/Moved/a.txt");
        assert_eq!(ix.lookup(other, b"Moved"), Some(docs));
        assert_eq!(ix.lookup(ROOT, b"Docs"), None);
        assert_eq!(ix.children(ROOT).collect::<Vec<_>>(), vec![other]);
    }

    #[test]
    fn rename_replaces_destination() {
        let (mut ix, docs, a, b) = sample();
        ix.rename(a, docs, b"B.TXT");
        assert!(!ix.is_alive(b));
        assert_eq!(ix.lookup(docs, b"B.TXT"), Some(a));
        assert_eq!(ix.children(docs).collect::<Vec<_>>(), vec![a]);
    }

    #[test]
    fn upsert_replaces_kind_change() {
        let (mut ix, docs, a, _) = sample();
        let (same, created) = ix.upsert(docs, b"a.txt", Meta::file(99, 9));
        assert_eq!((same, created), (a, false));
        assert_eq!(ix.size(a), 99);
        let (dir, created) = ix.upsert(docs, b"a.txt", Meta::dir(9));
        assert!(created && dir != a && ix.is_dir(dir) && !ix.is_alive(a));
    }

    #[test]
    fn apply_listing_diffs_children() {
        let (mut ix, docs, a, b) = sample();
        let sub = ix.insert(docs, b"sub", Meta::dir(0));
        ix.insert(sub, b"deep", Meta::file(1, 1));
        let listing = vec![
            ListedEntry {
                name: b"a.txt".to_vec(),
                meta: Meta::file(10, 2),
            },
            ListedEntry {
                name: b"B.TXT".to_vec(),
                meta: Meta::file(21, 4),
            },
            ListedEntry {
                name: b"new".to_vec(),
                meta: Meta::dir(5),
            },
        ];
        let out = ix.apply_listing(docs, &listing);
        assert_eq!(out.added, 1);
        assert_eq!(out.updated, 1);
        assert_eq!(out.removed, 2, "sub and its child");
        assert_eq!(out.new_dirs.len(), 1);
        assert!(out.existing_dirs.is_empty());
        assert!(ix.is_alive(a) && ix.is_alive(b) && !ix.is_alive(sub));
        assert_eq!(ix.size(b), 21);
        let unchanged = ix.apply_listing(docs, &listing);
        assert!(!unchanged.changed());
    }

    #[test]
    fn compact_renumbers_and_preserves_tree() {
        let (mut ix, docs, a, b) = sample();
        let gone = ix.insert(ROOT, b"gone", Meta::dir(0));
        ix.insert(gone, b"child", Meta::file(1, 1));
        ix.rename(b, docs, b"renamed.txt");
        ix.remove(gone);
        let epoch = ix.epoch();
        let before: Vec<Vec<u8>> = [docs, a, b].iter().map(|&id| ix.path(id)).collect();

        let remap = ix.compact();

        assert_eq!(ix.epoch(), epoch + 1);
        assert_eq!(ix.len(), 4);
        assert_eq!(ix.live_count(), 4);
        assert_eq!(ix.garbage_bytes(), 0);
        assert_eq!(remap[gone as usize], NO_ENTRY);
        let after: Vec<Vec<u8>> = [docs, a, b]
            .iter()
            .map(|&id| ix.path(remap[id as usize]))
            .collect();
        assert_eq!(before, after);
        let new_docs = remap[docs as usize];
        assert_eq!(ix.lookup(new_docs, b"renamed.txt"), Some(remap[b as usize]));
        assert_eq!(ix.resolve(b"/data/Docs/a.txt"), Some(remap[a as usize]));
        let mut kids: Vec<_> = ix.children(new_docs).collect();
        kids.sort();
        assert_eq!(kids, vec![remap[a as usize], remap[b as usize]]);
        // The compacted index stays fully mutable.
        let c = ix.insert(new_docs, b"c", Meta::file(3, 3));
        assert_eq!(ix.path(c), b"/data/Docs/c");
    }

    #[test]
    fn should_compact_after_mass_deletion() {
        let mut ix = Index::new(b"/", Meta::dir(0));
        let dir = ix.insert(ROOT, b"d", Meta::dir(0));
        for i in 0..5000 {
            ix.insert(dir, format!("f{i}").as_bytes(), Meta::file(0, 0));
        }
        assert!(!ix.should_compact());
        ix.remove(dir);
        assert!(ix.should_compact());
        ix.compact();
        assert_eq!(ix.len(), 1);
        assert!(!ix.should_compact());
    }

    #[test]
    fn generation_tracks_mutations() {
        let (mut ix, _, a, _) = sample();
        let g = ix.generation();
        assert!(!ix.set_meta(a, ix.meta(a)));
        assert_eq!(ix.generation(), g);
        ix.set_meta(a, Meta::file(1, 1));
        assert!(ix.generation() > g);
    }
}
