//! Copies of an index on another machine, kept current with deltas.
//!
//! A replica has the same entry ids as its source. Ids are only ever
//! appended within an epoch, and every change stamps the entry's
//! "recently changed" time, so a replica that knows how many slots it has
//! and when it last synced can be brought up to date with just the entries
//! appended or changed since. Compaction starts a new epoch; replicas of an
//! older epoch need a full copy.
//!
//! Entries a client may not see are sent as nameless tombstones, which
//! keeps ids aligned without revealing anything about them.

use std::io::{self, Read, Write};

use super::{now_ns, EntryId, Index, Meta, FLAG_DELETED, NO_ENTRY, ROOT};

/// One entry of a delta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaEntry {
    pub id: EntryId,
    pub alive: bool,
    pub parent: EntryId,
    pub name: Vec<u8>,
    pub meta: Meta,
    pub changed: i64,
}

/// What a replica needs to catch up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    pub epoch: u64,
    /// Slots of the source; the replica grows to this many.
    pub len: u32,
    /// The source's clock when the delta was taken; the replica asks for
    /// changes since this time next.
    pub now: i64,
    pub entries: Vec<DeltaEntry>,
}

/// Entries kept by a filter, plus every folder above them (the replica
/// needs the structure, not necessarily the right to list it).
fn kept_with_ancestors(index: &Index, keep: &dyn Fn(EntryId) -> bool) -> Vec<bool> {
    let mut kept = vec![false; index.len()];
    kept[ROOT as usize] = true;
    for id in 1..index.len() as EntryId {
        if !index.is_alive(id) || kept[id as usize] || !keep(id) {
            continue;
        }
        let mut cur = id;
        while cur != NO_ENTRY && !kept[cur as usize] {
            kept[cur as usize] = true;
            cur = index.parent[cur as usize];
        }
    }
    kept
}

impl Index {
    /// Moves the index to another root path (a replica shown under a
    /// local mount location).
    pub fn set_root_path(&mut self, path: &[u8]) {
        self.root_path = super::normalize_root(path);
        self.generation += 1;
    }

    /// A copy with the same ids in which only the entries `keep` accepts
    /// (and the folders above them) are alive; all others are tombstones
    /// without names.
    pub fn filtered_copy(&self, keep: impl Fn(EntryId) -> bool) -> Index {
        let kept = kept_with_ancestors(self, &keep);
        let mut out = Index::new(&self.root_path, self.meta(ROOT));
        out.epoch = self.epoch;
        out.changed[ROOT as usize] = self.changed[ROOT as usize];
        for id in 1..self.len() as EntryId {
            let i = id as usize;
            if kept[i] {
                out.push_entry(self.parent[i], self.name(id), self.meta(id));
                out.changed[i] = self.changed[i];
            } else {
                out.push_entry(NO_ENTRY, b"", Meta::default());
                out.flags[i] = FLAG_DELETED;
                out.live -= 1;
            }
        }
        for id in 1..out.len() as EntryId {
            if out.is_alive(id) {
                out.link(out.parent[id as usize], id);
                out.hash_insert(id);
            }
        }
        out.generation = self.generation;
        out
    }

    /// What a replica with `len` slots, last synced at `since` (the
    /// source's clock), needs to catch up. Entries `keep` rejects are sent
    /// as tombstones.
    pub fn delta(&self, len: usize, since: i64, keep: impl Fn(EntryId) -> bool) -> Delta {
        let now = now_ns();
        let kept = kept_with_ancestors(self, &keep);
        let entries = (1..self.len() as EntryId)
            .filter(|&id| id as usize >= len || self.changed[id as usize] >= since)
            .map(|id| {
                let i = id as usize;
                if kept[i] {
                    DeltaEntry {
                        id,
                        alive: true,
                        parent: self.parent[i],
                        name: self.name(id).to_vec(),
                        meta: self.meta(id),
                        changed: self.changed[i],
                    }
                } else {
                    DeltaEntry {
                        id,
                        alive: false,
                        parent: NO_ENTRY,
                        name: Vec::new(),
                        meta: Meta::default(),
                        changed: self.changed[i],
                    }
                }
            })
            .collect();
        Delta {
            epoch: self.epoch,
            len: self.len() as u32,
            now,
            entries,
        }
    }

    /// Applies a delta from the source of this replica.
    pub fn apply_delta(&mut self, delta: &Delta) {
        // Grow first: an entry may move under a folder appended later.
        while self.len() < delta.len as usize {
            self.push_entry(NO_ENTRY, b"", Meta::default());
            let i = self.len() - 1;
            self.flags[i] = FLAG_DELETED;
            self.live -= 1;
        }
        for e in &delta.entries {
            self.mirror(e);
        }
        self.generation += 1;
    }

    /// Makes one slot match the source.
    fn mirror(&mut self, e: &DeltaEntry) {
        let i = e.id as usize;
        if i == ROOT as usize || i >= self.len() {
            return;
        }
        if self.is_alive(e.id) {
            self.unhash(e.id);
            self.unlink(e.id);
            self.live -= 1;
        }
        self.garbage_bytes += self.name_len[i] as usize + self.fold_len[i] as usize;
        if !e.alive {
            self.flags[i] = FLAG_DELETED;
            self.parent[i] = NO_ENTRY;
            self.changed[i] = e.changed;
            return;
        }
        let (off, len, foff, flen) = self.push_name(&e.name);
        self.name_off[i] = off;
        self.name_len[i] = len;
        self.fold_off[i] = foff;
        self.fold_len[i] = flen;
        self.parent[i] = e.parent;
        self.size[i] = e.meta.size;
        self.mtime[i] = e.meta.mtime;
        self.mode[i] = e.meta.mode;
        self.uid[i] = e.meta.uid;
        self.gid[i] = e.meta.gid;
        self.btime[i] = e.meta.btime;
        self.atime[i] = e.meta.atime;
        self.changed[i] = e.changed;
        self.flags[i] = 0;
        self.live += 1;
        self.link(e.parent, e.id);
        self.hash_insert(e.id);
    }
}

impl Delta {
    /// Compact binary form (zstd-compressed).
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        let mut raw = Vec::with_capacity(64 + self.entries.len() * 64);
        raw.extend_from_slice(&self.epoch.to_le_bytes());
        raw.extend_from_slice(&self.len.to_le_bytes());
        raw.extend_from_slice(&self.now.to_le_bytes());
        raw.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for e in &self.entries {
            raw.extend_from_slice(&e.id.to_le_bytes());
            raw.push(e.alive as u8);
            raw.extend_from_slice(&e.parent.to_le_bytes());
            raw.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            raw.extend_from_slice(&e.name);
            let m = &e.meta;
            raw.extend_from_slice(&m.size.to_le_bytes());
            raw.extend_from_slice(&m.mtime.to_le_bytes());
            raw.extend_from_slice(&m.mode.to_le_bytes());
            raw.extend_from_slice(&m.uid.to_le_bytes());
            raw.extend_from_slice(&m.gid.to_le_bytes());
            raw.extend_from_slice(&m.btime.to_le_bytes());
            raw.extend_from_slice(&m.atime.to_le_bytes());
            raw.extend_from_slice(&e.changed.to_le_bytes());
        }
        let mut out = Vec::new();
        let mut enc = zstd::Encoder::new(&mut out, 1)?;
        enc.write_all(&raw)?;
        enc.finish()?;
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Delta> {
        let mut raw = Vec::new();
        zstd::Decoder::new(bytes)?.read_to_end(&mut raw)?;
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "truncated delta");
        let mut at = 0usize;
        let mut take = |n: usize| -> io::Result<&[u8]> {
            let s = raw.get(at..at + n).ok_or_else(bad)?;
            at += n;
            Ok(s)
        };
        macro_rules! num {
            ($t:ty) => {
                <$t>::from_le_bytes(take(std::mem::size_of::<$t>())?.try_into().unwrap())
            };
        }
        let epoch = num!(u64);
        let len = num!(u32);
        let now = num!(i64);
        let count = num!(u32) as usize;
        let mut entries = Vec::with_capacity(count.min(1 << 20));
        for _ in 0..count {
            let id = num!(u32);
            let alive = take(1)?[0] != 0;
            let parent = num!(u32);
            let name_len = num!(u16) as usize;
            let name = take(name_len)?.to_vec();
            let meta = Meta {
                size: num!(u64),
                mtime: num!(i64),
                mode: num!(u32),
                uid: num!(u32),
                gid: num!(u32),
                btime: num!(i64),
                atime: num!(i64),
            };
            let changed = num!(i64);
            entries.push(DeltaEntry {
                id,
                alive,
                parent,
                name,
                meta,
                changed,
            });
        }
        Ok(Delta {
            epoch,
            len,
            now,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(ix: &Index) -> Vec<String> {
        let mut v: Vec<String> = ix
            .live_ids()
            .filter(|&id| id != ROOT)
            .map(|id| String::from_utf8_lossy(&ix.path(id)).into_owned())
            .collect();
        v.sort();
        v
    }

    fn source() -> Index {
        let mut ix = Index::new(b"/srv", Meta::dir(0));
        let docs = ix.insert(ROOT, b"docs", Meta::dir(0));
        ix.insert(docs, "שלום.txt".as_bytes(), Meta::file(5, 1));
        let secret = ix.insert(ROOT, b"secret", Meta::dir(0));
        ix.insert(secret, b"keys.txt", Meta::file(9, 1));
        ix
    }

    #[test]
    fn filtered_copies_keep_ids_and_hide_the_rest() {
        let ix = source();
        let secret = ix.lookup(ROOT, b"secret").unwrap();
        let copy = ix.filtered_copy(|id| !ix.is_within(id, secret));
        assert_eq!(copy.len(), ix.len());
        assert_eq!(paths(&copy), ["/srv/docs", "/srv/docs/שלום.txt"]);
        assert!(!copy.is_alive(secret));
        assert_eq!(copy.name(secret), b"", "no names of hidden entries");
        let docs = copy.lookup(ROOT, b"docs").unwrap();
        assert!(copy.lookup(docs, "שלום.txt".as_bytes()).is_some());
        assert_eq!(copy.folded_name(docs), crate::fold::fold(b"docs"));
    }

    #[test]
    fn deltas_bring_a_replica_up_to_date() {
        let mut ix = source();
        let mut replica = ix.filtered_copy(|_| true);
        replica.set_root_path(b"/net/server");
        let since = now_ns();
        std::thread::sleep(std::time::Duration::from_millis(2));

        // Additions, a move under a newer folder, a change and a removal.
        let docs = ix.lookup(ROOT, b"docs").unwrap();
        let file = ix.lookup(docs, "שלום.txt".as_bytes()).unwrap();
        let new_dir = ix.insert(ROOT, b"new", Meta::dir(0));
        ix.insert(new_dir, b"added.txt", Meta::file(1, 1));
        ix.rename(file, new_dir, b"moved.txt");
        ix.set_meta(docs, Meta::dir(42));
        let secret = ix.lookup(ROOT, b"secret").unwrap();
        ix.remove(secret);

        let delta = ix.delta(replica.len(), since, |_| true);
        let delta = Delta::decode(&delta.encode().unwrap()).unwrap();
        replica.apply_delta(&delta);
        assert_eq!(
            paths(&replica),
            [
                "/net/server/docs",
                "/net/server/new",
                "/net/server/new/added.txt",
                "/net/server/new/moved.txt"
            ]
        );
        assert_eq!(replica.live_count(), ix.live_count());
        assert_eq!(replica.mtime(docs), 42);
        assert!(replica.lookup(docs, "שלום.txt".as_bytes()).is_none());

        // Nothing changed: an empty delta.
        let next = ix.delta(replica.len(), delta.now, |_| true);
        assert!(next.entries.is_empty());
    }

    #[test]
    fn hidden_entries_stay_hidden_in_deltas() {
        let mut ix = source();
        let secret = ix.lookup(ROOT, b"secret").unwrap();
        let keep = |ix: &Index, id| !ix.is_within(id, secret);
        let mut replica = ix.filtered_copy(|id| keep(&ix, id));
        let since = now_ns();
        ix.insert(secret, b"more-keys.txt", Meta::file(1, 1));
        let delta = ix.delta(replica.len(), since, |id| keep(&ix, id));
        assert!(delta.entries.iter().all(|e| !e.alive && e.name.is_empty()));
        replica.apply_delta(&delta);
        assert_eq!(paths(&replica), ["/srv/docs", "/srv/docs/שלום.txt"]);
    }
}
