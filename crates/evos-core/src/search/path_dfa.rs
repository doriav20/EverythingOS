//! Substring matching over full paths without building the paths.
//!
//! A path is the concatenation of its ancestors' names, so the state of a
//! KMP automaton after reading a directory's path is a function of its
//! parent's state and its own name. States are memoized per directory, which
//! turns a path substring search over N entries into one automaton run over
//! each entry's own name.

use std::sync::atomic::{AtomicU16, Ordering};

use crate::store::{EntryId, Index, ROOT};

/// Longest supported needle; longer needles exceed `PATH_MAX` anyway.
pub(crate) const MAX_NEEDLE: usize = 4096;

/// A KMP automaton whose accepting state is absorbing.
pub(crate) struct Dfa {
    table: Vec<u16>,
    accept: u16,
}

impl Dfa {
    pub fn new(needle: &[u8]) -> Self {
        assert!(!needle.is_empty() && needle.len() <= MAX_NEEDLE);
        let m = needle.len();
        let mut table = vec![0u16; (m + 1) * 256];
        table[needle[0] as usize] = 1;
        let mut restart = 0usize;
        for state in 1..=m {
            for byte in 0..256 {
                table[state * 256 + byte] = table[restart * 256 + byte];
            }
            if state < m {
                table[state * 256 + needle[state] as usize] = state as u16 + 1;
                restart = table[restart * 256 + needle[state] as usize] as usize;
            }
        }
        // Once matched, stay matched.
        for byte in 0..256 {
            table[m * 256 + byte] = m as u16;
        }
        Dfa {
            table,
            accept: m as u16,
        }
    }

    #[inline]
    pub fn run(&self, mut state: u16, bytes: &[u8]) -> u16 {
        for &b in bytes {
            if state == self.accept {
                break;
            }
            state = self.table[state as usize * 256 + b as usize];
        }
        state
    }

    pub fn accepts(&self, state: u16) -> bool {
        state == self.accept
    }
}

const UNKNOWN: u16 = u16::MAX;

/// Per-volume memo of automaton states after each directory's path.
pub(crate) struct PathMemo {
    states: Vec<AtomicU16>,
    folded: bool,
    root_state: u16,
    /// The root path is `/`, so children are not preceded by another `/`.
    root_is_slash: bool,
}

impl PathMemo {
    pub fn new(dfa: &Dfa, index: &Index, folded: bool) -> Self {
        let root = index.root_path();
        let root_bytes = if folded {
            crate::fold::fold(root)
        } else {
            root.to_vec()
        };
        PathMemo {
            states: (0..index.len()).map(|_| AtomicU16::new(UNKNOWN)).collect(),
            folded,
            root_state: dfa.run(0, &root_bytes),
            root_is_slash: root == b"/",
        }
    }

    fn name<'a>(&self, index: &'a Index, id: EntryId) -> &'a [u8] {
        if self.folded {
            index.folded_name(id)
        } else {
            index.name(id)
        }
    }

    /// State after reading the full path of `id`.
    pub fn state(&self, dfa: &Dfa, index: &Index, id: EntryId) -> u16 {
        if id == ROOT {
            return self.root_state;
        }
        if !index.is_dir(id) {
            let parent = index.parent(id);
            let s = self.dir_state(dfa, index, parent);
            return self.step(dfa, index, parent, s, id);
        }
        self.dir_state(dfa, index, id)
    }

    fn dir_state(&self, dfa: &Dfa, index: &Index, dir: EntryId) -> u16 {
        if dir == ROOT {
            return self.root_state;
        }
        let known = self.states[dir as usize].load(Ordering::Relaxed);
        if known != UNKNOWN {
            return known;
        }
        // Walk up to the nearest known ancestor, then compute downwards.
        let mut chain = vec![dir];
        let mut cur = index.parent(dir);
        let mut state = loop {
            if cur == ROOT {
                break self.root_state;
            }
            let s = self.states[cur as usize].load(Ordering::Relaxed);
            if s != UNKNOWN {
                break s;
            }
            chain.push(cur);
            cur = index.parent(cur);
        };
        for &d in chain.iter().rev() {
            state = self.step(dfa, index, index.parent(d), state, d);
            self.states[d as usize].store(state, Ordering::Relaxed);
        }
        state
    }

    fn step(&self, dfa: &Dfa, index: &Index, parent: EntryId, state: u16, id: EntryId) -> u16 {
        let state = if parent == ROOT && self.root_is_slash {
            state
        } else {
            dfa.run(state, b"/")
        };
        dfa.run(state, self.name(index, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Meta;

    fn contains(needle: &str, hay: &str) -> bool {
        let dfa = Dfa::new(needle.as_bytes());
        dfa.accepts(dfa.run(0, hay.as_bytes()))
    }

    #[test]
    fn dfa_finds_substrings() {
        assert!(contains("abab", "xxababyy"));
        assert!(contains("aab", "aaab"));
        assert!(!contains("abc", "ababab"));
        assert!(contains("a", "a"));
        assert!(contains("src/main", "/home/src/main.rs"));
    }

    #[test]
    fn memo_matches_full_paths() {
        for root in [&b"/"[..], b"/mnt/Share"] {
            let mut ix = Index::new(root, Meta::dir(0));
            let a = ix.insert(ROOT, b"Alpha", Meta::dir(0));
            let b = ix.insert(a, b"beta", Meta::dir(0));
            let f = ix.insert(b, b"file.txt", Meta::file(0, 0));
            for needle in [
                "alpha/beta",
                "/alpha",
                "beta/file",
                "a/b",
                "share/alpha",
                "zzz",
            ] {
                let dfa = Dfa::new(needle.as_bytes());
                let memo = PathMemo::new(&dfa, &ix, true);
                for id in [a, b, f] {
                    let mut path = Vec::new();
                    ix.folded_path_into(id, &mut path);
                    let expected = memchr::memmem::find(&path, needle.as_bytes()).is_some();
                    let got = dfa.accepts(memo.state(&dfa, &ix, id));
                    assert_eq!(
                        got,
                        expected,
                        "needle {needle:?} path {:?}",
                        String::from_utf8_lossy(&path)
                    );
                }
            }
        }
    }
}
