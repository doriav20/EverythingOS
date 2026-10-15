//! Sorting of search results.
//!
//! Sorting is a parallel comparison sort over the result set. Every order
//! breaks ties by name and finally by `(volume, id)`, so the same results
//! always come out in the same order.

use std::cmp::Ordering;

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::search::{attributes_of, Extra, Hit, NoExtra};
use crate::store::{extension, EntryId, Index};

/// Column to sort results by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum SortKey {
    #[default]
    Name,
    /// Folder path, then name.
    Path,
    Size,
    Modified,
    Extension,
    /// Type description ("PNG Image").
    Type,
    Created,
    Accessed,
    Attributes,
    RecentlyChanged,
    RunCount,
    DateRun,
}

/// Sort column and direction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SortSpec {
    pub key: SortKey,
    pub descending: bool,
}

struct Volumes<'a> {
    by_id: Vec<Option<&'a Index>>,
}

impl<'a> Volumes<'a> {
    fn new(volumes: &[(u16, &'a Index)]) -> Self {
        let len = volumes
            .iter()
            .map(|(v, _)| *v as usize + 1)
            .max()
            .unwrap_or(0);
        let mut by_id = vec![None; len];
        for &(v, ix) in volumes {
            by_id[v as usize] = Some(ix);
        }
        Volumes { by_id }
    }

    fn get(&self, vol: u16) -> &'a Index {
        self.by_id[vol as usize].expect("hit refers to an unknown volume")
    }
}

/// Sorts hits in place.
pub fn sort_hits(hits: &mut [Hit], volumes: &[(u16, &Index)], spec: SortSpec) {
    sort_hits_with(hits, volumes, spec, &NoExtra)
}

/// Sorts hits in place, with folder sizes and run history from `extra`.
pub fn sort_hits_with(
    hits: &mut [Hit],
    volumes: &[(u16, &Index)],
    spec: SortSpec,
    extra: &dyn Extra,
) {
    let vols = Volumes::new(volumes);
    let name = |h: &Hit| vols.get(h.vol).folded_name(h.id);
    let by_name = |a: &Hit, b: &Hit| {
        name(a)
            .cmp(name(b))
            .then_with(|| vols.get(a.vol).name(a.id).cmp(vols.get(b.vol).name(b.id)))
            .then_with(|| a.cmp(b))
    };
    let dir = |o: Ordering| if spec.descending { o.reverse() } else { o };

    match spec.key {
        SortKey::Name => hits.par_sort_unstable_by(|a, b| dir(by_name(a, b))),
        SortKey::Size => hits.par_sort_unstable_by(|a, b| {
            let size = |h: &Hit| {
                let ix = vols.get(h.vol);
                // Folders without a known size sort before files.
                if ix.is_dir(h.id) {
                    extra.folder_size(h.vol, h.id)
                } else {
                    Some(ix.size(h.id))
                }
            };
            dir(size(a).cmp(&size(b)).then_with(|| by_name(a, b)))
        }),
        SortKey::Modified
        | SortKey::Created
        | SortKey::Accessed
        | SortKey::RecentlyChanged
        | SortKey::RunCount
        | SortKey::DateRun => hits.par_sort_unstable_by(|a, b| {
            let value = |h: &Hit| {
                let ix = vols.get(h.vol);
                match spec.key {
                    SortKey::Modified => ix.mtime(h.id),
                    SortKey::Created => ix.btime(h.id),
                    SortKey::Accessed => ix.atime(h.id),
                    SortKey::RecentlyChanged => ix.changed(h.id),
                    SortKey::RunCount => extra.run_info(h.vol, h.id).count as i64,
                    _ => extra.run_info(h.vol, h.id).last,
                }
            };
            dir(value(a).cmp(&value(b)).then_with(|| by_name(a, b)))
        }),
        SortKey::Type | SortKey::Attributes => {
            let text = |h: &Hit| {
                let ix = vols.get(h.vol);
                if spec.key == SortKey::Type {
                    crate::types::type_name(
                        ix.name(h.id),
                        ix.is_dir(h.id),
                        ix.mode(h.id) & 0o111 != 0,
                    )
                } else {
                    attributes_of(ix, h.id)
                }
            };
            let mut keyed: Vec<(String, Hit)> = hits.par_iter().map(|h| (text(h), *h)).collect();
            keyed.par_sort_unstable_by(|(ka, a), (kb, b)| {
                dir(ka.cmp(kb).then_with(|| by_name(a, b)))
            });
            for (dst, (_, hit)) in hits.iter_mut().zip(keyed) {
                *dst = hit;
            }
        }
        SortKey::Extension => hits.par_sort_unstable_by(|a, b| {
            let ext = |h: &Hit| {
                let ix = vols.get(h.vol);
                if ix.is_dir(h.id) {
                    &b""[..]
                } else {
                    extension(ix.folded_name(h.id))
                }
            };
            dir(ext(a).cmp(ext(b)).then_with(|| by_name(a, b)))
        }),
        SortKey::Path => sort_by_path(hits, &vols, spec.descending, by_name),
    }
}

/// Sorts by folded parent path. Each distinct parent's path is built once.
fn sort_by_path(
    hits: &mut [Hit],
    vols: &Volumes<'_>,
    descending: bool,
    by_name: impl Fn(&Hit, &Hit) -> Ordering + Sync,
) {
    let mut slots: HashMap<(u16, EntryId), u32> = HashMap::new();
    let mut paths: Vec<Vec<u8>> = Vec::new();
    let mut keyed: Vec<(u32, Hit)> = Vec::with_capacity(hits.len());
    for &hit in hits.iter() {
        let ix = vols.get(hit.vol);
        let parent = ix.parent(hit.id);
        let slot = *slots.entry((hit.vol, parent)).or_insert_with(|| {
            let mut path = Vec::new();
            ix.folded_path_into(parent, &mut path);
            paths.push(path);
            (paths.len() - 1) as u32
        });
        keyed.push((slot, hit));
    }
    keyed.par_sort_unstable_by(|(sa, a), (sb, b)| {
        let o = paths[*sa as usize]
            .cmp(&paths[*sb as usize])
            .then_with(|| by_name(a, b));
        if descending {
            o.reverse()
        } else {
            o
        }
    });
    for (dst, (_, hit)) in hits.iter_mut().zip(keyed) {
        *dst = hit;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Meta, ROOT};

    fn setup() -> (Index, Index) {
        let mut a = Index::new(b"/", Meta::dir(0));
        let docs = a.insert(ROOT, b"docs", Meta::dir(5));
        a.insert(docs, b"b.txt", Meta::file(300, 1));
        a.insert(docs, b"A.md", Meta::file(100, 3));
        a.insert(ROOT, b"c.txt", Meta::file(200, 2));
        let mut b = Index::new(b"/mnt/x", Meta::dir(0));
        b.insert(ROOT, b"a.txt", Meta::file(50, 4));
        (a, b)
    }

    fn sorted(spec: SortSpec) -> Vec<String> {
        let (a, b) = setup();
        let vols = [(0u16, &a), (1u16, &b)];
        let mut hits: Vec<Hit> = a
            .live_ids()
            .skip(1)
            .map(|id| Hit { vol: 0, id })
            .chain(b.live_ids().skip(1).map(|id| Hit { vol: 1, id }))
            .collect();
        sort_hits(&mut hits, &vols, spec);
        hits.iter()
            .map(|h| {
                let ix = if h.vol == 0 { &a } else { &b };
                String::from_utf8(ix.path(h.id)).unwrap()
            })
            .collect()
    }

    #[test]
    fn by_name_is_case_insensitive() {
        let got = sorted(SortSpec {
            key: SortKey::Name,
            descending: false,
        });
        assert_eq!(
            got,
            [
                "/docs/A.md",
                "/mnt/x/a.txt",
                "/docs/b.txt",
                "/c.txt",
                "/docs"
            ]
        );
    }

    #[test]
    fn by_name_descending() {
        let got = sorted(SortSpec {
            key: SortKey::Name,
            descending: true,
        });
        assert_eq!(
            got,
            [
                "/docs",
                "/c.txt",
                "/docs/b.txt",
                "/mnt/x/a.txt",
                "/docs/A.md"
            ]
        );
    }

    #[test]
    fn by_path_groups_folders() {
        let got = sorted(SortSpec {
            key: SortKey::Path,
            descending: false,
        });
        assert_eq!(
            got,
            [
                "/c.txt",
                "/docs",
                "/docs/A.md",
                "/docs/b.txt",
                "/mnt/x/a.txt"
            ]
        );
    }

    #[test]
    fn by_size_puts_folders_first() {
        let got = sorted(SortSpec {
            key: SortKey::Size,
            descending: false,
        });
        assert_eq!(
            got,
            [
                "/docs",
                "/mnt/x/a.txt",
                "/docs/A.md",
                "/c.txt",
                "/docs/b.txt"
            ]
        );
    }

    #[test]
    fn by_modified_and_extension() {
        let got = sorted(SortSpec {
            key: SortKey::Modified,
            descending: true,
        });
        assert_eq!(got[0], "/docs");
        let got = sorted(SortSpec {
            key: SortKey::Extension,
            descending: false,
        });
        assert_eq!(
            got,
            [
                "/docs",
                "/docs/A.md",
                "/mnt/x/a.txt",
                "/docs/b.txt",
                "/c.txt"
            ]
        );
    }
}
