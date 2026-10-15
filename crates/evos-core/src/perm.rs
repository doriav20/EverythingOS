//! Per-user result filtering.
//!
//! The daemon indexes everything as root, so results must be filtered for
//! the user asking. An entry is visible when the user could list its parent
//! folder: every ancestor inside the volume grants search (`x`) permission
//! and the parent also grants read (`r`) permission. Decisions are memoized
//! per folder, so the check costs one array load for most entries.
//!
//! POSIX ACLs are not evaluated; only the mode bits are.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::store::{EntryId, Index, ROOT};

/// The user on whose behalf a search runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller {
    pub uid: u32,
    /// Primary and supplementary groups.
    pub gids: Vec<u32>,
}

impl Caller {
    pub fn root() -> Self {
        Caller {
            uid: 0,
            gids: vec![0],
        }
    }

    pub fn is_root(&self) -> bool {
        self.uid == 0
    }

    fn allows(&self, mode: u32, uid: u32, gid: u32, bit: u32) -> bool {
        let shift = if self.uid == uid {
            6
        } else if self.gids.contains(&gid) {
            3
        } else {
            0
        };
        (mode >> shift) & bit != 0
    }
}

const UNKNOWN: u8 = 0;
const LISTABLE: u8 = 1;
const REACHABLE: u8 = 2;
const BLOCKED: u8 = 3;

const R: u32 = 4;
const X: u32 = 1;

/// Visibility decisions for one caller over a set of volumes.
pub struct Visibility {
    caller: Caller,
    memos: Vec<Vec<AtomicU8>>,
}

impl Visibility {
    pub fn new(caller: Caller, volumes: &[(u16, &Index)]) -> Self {
        let len = volumes
            .iter()
            .map(|(v, _)| *v as usize + 1)
            .max()
            .unwrap_or(0);
        let mut memos: Vec<Vec<AtomicU8>> = (0..len).map(|_| Vec::new()).collect();
        if !caller.is_root() {
            for &(vol, ix) in volumes {
                memos[vol as usize] = (0..ix.len()).map(|_| AtomicU8::new(UNKNOWN)).collect();
            }
        }
        Visibility { caller, memos }
    }

    /// Can the caller see this entry?
    pub fn is_visible(&self, vol: u16, index: &Index, id: EntryId) -> bool {
        if self.caller.is_root() {
            return true;
        }
        let Some(memo) = self
            .memos
            .get(vol as usize)
            .filter(|m| m.len() == index.len())
        else {
            return false;
        };
        id == ROOT || self.dir_state(memo, index, index.parent(id)) == LISTABLE
    }

    /// Could the caller open this file for reading? (It must be visible,
    /// and its mode must grant the caller read permission.)
    pub fn can_read(&self, vol: u16, index: &Index, id: EntryId) -> bool {
        if self.caller.is_root() {
            return true;
        }
        let meta = index.meta(id);
        self.is_visible(vol, index, id) && self.caller.allows(meta.mode, meta.uid, meta.gid, R)
    }

    fn dir_state(&self, memo: &[AtomicU8], index: &Index, dir: EntryId) -> u8 {
        let mut chain = Vec::new();
        let mut cur = dir;
        // Find the nearest ancestor with a known state.
        let mut state = loop {
            let known = memo[cur as usize].load(Ordering::Relaxed);
            if known != UNKNOWN {
                break known;
            }
            chain.push(cur);
            if cur == ROOT {
                break LISTABLE; // the volume root is reachable by assumption
            }
            cur = index.parent(cur);
        };
        for &d in chain.iter().rev() {
            let meta = index.meta(d);
            state = if state == BLOCKED || !self.caller.allows(meta.mode, meta.uid, meta.gid, X) {
                BLOCKED
            } else if self.caller.allows(meta.mode, meta.uid, meta.gid, R) {
                LISTABLE
            } else {
                REACHABLE
            };
            memo[d as usize].store(state, Ordering::Relaxed);
        }
        state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{mode, Meta};

    fn dir(perm: u32, uid: u32, gid: u32) -> Meta {
        Meta {
            mode: mode::S_IFDIR | perm,
            uid,
            gid,
            ..Default::default()
        }
    }

    #[test]
    fn mode_bits_are_honored() {
        let mut ix = Index::new(b"/", dir(0o755, 0, 0));
        let home = ix.insert(ROOT, b"home", dir(0o755, 0, 0));
        let alice = ix.insert(home, b"alice", dir(0o700, 1000, 1000));
        let secret = ix.insert(alice, b"secret.txt", Meta::file(1, 0));
        let shared = ix.insert(home, b"shared", dir(0o750, 0, 100));
        let doc = ix.insert(shared, b"doc", Meta::file(1, 0));
        let dropbox = ix.insert(home, b"dropbox", dir(0o711, 0, 0));
        let hidden = ix.insert(dropbox, b"inside", Meta::file(1, 0));
        let vols = [(0, &ix)];

        let alice_user = Visibility::new(
            Caller {
                uid: 1000,
                gids: vec![1000],
            },
            &vols,
        );
        assert!(alice_user.is_visible(0, &ix, secret));
        assert!(!alice_user.is_visible(0, &ix, doc), "not in group 100");
        assert!(
            alice_user.is_visible(0, &ix, shared),
            "the folder itself is listed in /home"
        );

        let bob = Visibility::new(
            Caller {
                uid: 1001,
                gids: vec![1001, 100],
            },
            &vols,
        );
        assert!(!bob.is_visible(0, &ix, secret));
        assert!(
            bob.is_visible(0, &ix, alice),
            "the folder name itself is visible"
        );
        assert!(bob.is_visible(0, &ix, doc), "via supplementary group");
        assert!(
            !bob.is_visible(0, &ix, hidden),
            "x without r hides contents"
        );

        let root = Visibility::new(Caller::root(), &vols);
        assert!(root.is_visible(0, &ix, secret) && root.is_visible(0, &ix, hidden));
    }

    #[test]
    fn blocked_ancestor_hides_whole_subtree() {
        let mut ix = Index::new(b"/", dir(0o755, 0, 0));
        let private = ix.insert(ROOT, b"private", dir(0o700, 0, 0));
        let open = ix.insert(private, b"open", dir(0o777, 0, 0));
        let f = ix.insert(open, b"f", Meta::file(1, 0));
        let vis = Visibility::new(
            Caller {
                uid: 5,
                gids: vec![5],
            },
            &[(0, &ix)],
        );
        assert!(!vis.is_visible(0, &ix, f));
        assert!(!vis.is_visible(0, &ix, open));
        assert!(vis.is_visible(0, &ix, private));
    }

    #[test]
    fn unknown_volume_is_invisible() {
        let ix = Index::new(b"/", dir(0o755, 0, 0));
        let vis = Visibility::new(
            Caller {
                uid: 5,
                gids: vec![5],
            },
            &[],
        );
        assert!(!vis.is_visible(3, &ix, ROOT));
    }

    #[test]
    fn reading_needs_visibility_and_the_read_bit() {
        let mut ix = Index::new(b"/", dir(0o755, 0, 0));
        let open = ix.insert(ROOT, b"open.txt", Meta::file(1, 0));
        let mut locked = Meta::file(1, 0);
        locked.mode = mode::S_IFREG | 0o600;
        let locked = ix.insert(ROOT, b"locked.txt", locked);
        let private = ix.insert(ROOT, b"private", dir(0o700, 0, 0));
        let inside = ix.insert(private, b"inside.txt", Meta::file(1, 0));
        let vols = [(0, &ix)];
        let user = Visibility::new(
            Caller {
                uid: 5,
                gids: vec![5],
            },
            &vols,
        );
        assert!(user.can_read(0, &ix, open));
        assert!(!user.can_read(0, &ix, locked), "mode 0600 of root");
        assert!(!user.can_read(0, &ix, inside), "inside a 0700 folder");
        assert!(Visibility::new(Caller::root(), &vols).can_read(0, &ix, locked));
    }
}
