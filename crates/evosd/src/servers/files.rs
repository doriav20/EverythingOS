//! What the servers show of the file system: entries and folder listings
//! come from the index (never from disk), as the server's "run as"
//! account may see them; downloads read files the way previews do
//! (read-only, no access time change, no symbolic links).

use evos_core::perm::{Caller, Visibility};
use evos_core::{EntryId, Index, ROOT};

use crate::server::Daemon;

/// An entry as listed to a remote user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: Vec<u8>,
    pub is_dir: bool,
    pub size: u64,
    /// Unix nanoseconds.
    pub mtime: i64,
}

fn entry(ix: &Index, id: EntryId) -> Entry {
    Entry {
        name: ix.name(id).to_vec(),
        is_dir: ix.is_dir(id),
        size: ix.size(id),
        mtime: ix.mtime(id),
    }
}

/// Normalizes an absolute path: no `.`, `..`, empty or trailing parts.
pub fn normalize(path: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for p in path.split(|&b| b == b'/') {
        match p {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = Vec::with_capacity(path.len() + 1);
    for p in parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// Runs `f` on the local volume holding `path` (longest root), with the
/// entry's id if it exists and `caller` may see it.
fn with_entry<T>(
    daemon: &Daemon,
    caller: &Caller,
    include: &[String],
    path: &[u8],
    f: impl FnOnce(&Index, u16, EntryId, &Visibility) -> T,
) -> Option<T> {
    let path = normalize(path);
    if !super::included(&path, include) {
        return None;
    }
    let handles = daemon.volumes.volumes();
    let handle = handles
        .iter()
        .filter(|h| h.remote.is_none())
        .filter(|h| h.vol.index.read().relative(&path).is_some())
        .max_by_key(|h| h.vol.root().len())?;
    let ix = handle.vol.index.read();
    let id = ix.resolve(&path)?;
    let vis = Visibility::new(caller.clone(), &[(handle.id, &*ix)]);
    if !vis.is_visible(handle.id, &ix, id) {
        return None;
    }
    Some(f(&ix, handle.id, id, &vis))
}

/// The folders leading from `path` towards indexed volumes that are not
/// inside an indexed folder themselves (`/` → `tmp` for a volume at
/// `/tmp/disk`), as Everything lists drives at the top; `None` if no
/// volume lies below `path`.
fn leading_folders(daemon: &Daemon, include: &[String], path: &[u8]) -> Option<Vec<Entry>> {
    let path = normalize(path);
    let prefix = if path == b"/" {
        b"/".to_vec()
    } else {
        [path.as_slice(), b"/"].concat()
    };
    let handles: Vec<_> = daemon
        .volumes
        .volumes()
        .into_iter()
        .filter(|h| h.remote.is_none())
        .collect();
    // Inside a volume only what the index shows counts: a hidden folder
    // must not give away what is mounted below it.
    if handles
        .iter()
        .any(|h| h.vol.index.read().relative(&path).is_some())
    {
        return None;
    }
    let mut names: Vec<Vec<u8>> = handles
        .iter()
        .filter_map(|h| {
            let root = h.vol.root();
            let rest = root.strip_prefix(prefix.as_slice())?;
            let next = rest
                .split(|&b| b == b'/')
                .next()
                .filter(|n| !n.is_empty())?;
            // Only towards folders the account may see.
            let full = [prefix.as_slice(), next].concat();
            let leads = super::included(&full, include)
                || include.iter().any(|f| {
                    let f = f.trim_end_matches('/').as_bytes();
                    f.starts_with(&full) && f.get(full.len()) == Some(&b'/')
                });
            leads.then(|| next.to_vec())
        })
        .collect();
    if names.is_empty() {
        return None;
    }
    names.sort();
    names.dedup();
    Some(
        names
            .into_iter()
            .map(|name| Entry {
                name,
                is_dir: true,
                size: 0,
                mtime: 0,
            })
            .collect(),
    )
}

/// The entry at `path`.
pub fn lookup(daemon: &Daemon, caller: &Caller, include: &[String], path: &[u8]) -> Option<Entry> {
    with_entry(daemon, caller, include, path, |ix, _, id, _| entry(ix, id)).or_else(|| {
        let path = normalize(path);
        leading_folders(daemon, include, &path)?;
        Some(Entry {
            name: path
                .rsplit(|&b| b == b'/')
                .next()
                .unwrap_or_default()
                .to_vec(),
            is_dir: true,
            size: 0,
            mtime: 0,
        })
    })
}

/// The visible contents of the folder at `path`, sorted by name. The
/// file system root also shows other volumes' roots mounted on it.
pub fn list(
    daemon: &Daemon,
    caller: &Caller,
    include: &[String],
    path: &[u8],
) -> Option<Vec<Entry>> {
    let path = normalize(path);
    let listed = with_entry(daemon, caller, include, &path, |ix, vol, id, vis| {
        if !ix.is_dir(id) {
            return None;
        }
        Some(
            ix.children(id)
                .filter(|&c| vis.is_visible(vol, ix, c))
                .map(|c| entry(ix, c))
                .collect::<Vec<_>>(),
        )
    });
    let mut entries = match listed {
        Some(entries) => entries?,
        None => return leading_folders(daemon, include, &path),
    };
    // Children of a folder outside the include list are listed only if
    // they lead to included folders.
    let prefix = if path == b"/" {
        b"/".to_vec()
    } else {
        [path.as_slice(), b"/"].concat()
    };
    entries.retain(|e| {
        let full = [prefix.as_slice(), &e.name].concat();
        super::included(&full, include)
            || include.iter().any(|f| {
                let f = f.trim_end_matches('/').as_bytes();
                f.starts_with(&full) && f.get(full.len()) == Some(&b'/')
            })
    });
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.dedup_by(|a, b| a.name == b.name);
    Some(entries)
}

/// Opens a file for download if `caller` may read it.
pub fn open(
    daemon: &Daemon,
    caller: &Caller,
    include: &[String],
    path: &[u8],
) -> Result<(std::fs::File, Entry), String> {
    let path = normalize(path);
    let found = with_entry(daemon, caller, include, &path, |ix, vol, id, vis| {
        (
            !ix.is_dir(id) && id != ROOT && vis.can_read(vol, ix, id),
            entry(ix, id),
        )
    });
    match found {
        Some((true, e)) => evos_core::props::open_readonly(&path)
            .map(|f| (f, e))
            .map_err(|err| err.to_string()),
        Some((false, _)) => Err("permission denied".into()),
        None => Err("file not found".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_paths() {
        assert_eq!(normalize(b"/a//b/./c/../d/"), b"/a/b/d");
        assert_eq!(normalize(b"/../.."), b"/");
        assert_eq!(normalize(b""), b"/");
    }
}
