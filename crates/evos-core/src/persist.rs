//! Index snapshots on disk.
//!
//! Layout (all integers little-endian):
//!
//! ```text
//! "EVOSIDX\0"  u32 version
//! header:  u32 len, root path, u64 generation, u64 epoch, u64 live,
//!          u64 garbage bytes, u32 len, caller-defined extra bytes
//! columns: u32 count, then per column:
//!          u64 raw len, u64 compressed len, u32 crc32, zstd bytes
//! "EVOSEND\0"
//! ```
//!
//! Columns are compressed in parallel. Loading validates every offset and
//! id so a corrupt file is rejected instead of causing panics later, then
//! rebuilds the lookup table, which is not stored.

use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use rayon::prelude::*;

use crate::store::{Index, Meta, NO_ENTRY};

const MAGIC: &[u8; 8] = b"EVOSIDX\0";
const END: &[u8; 8] = b"EVOSEND\0";
const VERSION: u32 = 1;
const COLUMNS: u32 = 16;
const ZSTD_LEVEL: i32 = 1;

/// Error loading a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("unsupported snapshot version {0}")]
    Version(u32),
    #[error("corrupt snapshot: {0}")]
    Corrupt(&'static str),
}

trait Scalar: Copy + Send + Sync {
    const SIZE: usize;
    fn put(self, out: &mut Vec<u8>);
    fn get(bytes: &[u8]) -> Self;
}

macro_rules! scalar {
    ($($t:ty),*) => {$(
        impl Scalar for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn put(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_le_bytes());
            }
            fn get(bytes: &[u8]) -> Self {
                <$t>::from_le_bytes(bytes.try_into().expect("exact chunk"))
            }
        }
    )*};
}
scalar!(u8, u16, u32, u64, i64);

fn encode<T: Scalar>(col: &[T]) -> Vec<u8> {
    let mut out = Vec::with_capacity(col.len() * T::SIZE);
    for &v in col {
        v.put(&mut out);
    }
    out
}

fn decode<T: Scalar>(bytes: &[u8]) -> Result<Vec<T>, SnapshotError> {
    if bytes.len() % T::SIZE != 0 {
        return Err(SnapshotError::Corrupt("column length"));
    }
    Ok(bytes.chunks_exact(T::SIZE).map(T::get).collect())
}

/// Writes a snapshot of `index`, with `extra` stored verbatim in the header.
pub fn write_snapshot(index: &Index, extra: &[u8], w: impl Write) -> io::Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, w);
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    write_blob(&mut w, index.root_path())?;
    w.write_all(&index.generation.to_le_bytes())?;
    w.write_all(&index.epoch.to_le_bytes())?;
    w.write_all(&(index.live as u64).to_le_bytes())?;
    w.write_all(&(index.garbage_bytes as u64).to_le_bytes())?;
    write_blob(&mut w, extra)?;

    let raw: Vec<Vec<u8>> = vec![
        encode(&index.parent),
        encode(&index.first_child),
        encode(&index.next_sibling),
        encode(&index.prev_sibling),
        encode(&index.name_off),
        encode(&index.name_len),
        encode(&index.fold_off),
        encode(&index.fold_len),
        encode(&index.size),
        encode(&index.mtime),
        encode(&index.mode),
        encode(&index.uid),
        encode(&index.gid),
        encode(&index.flags),
        index.names.clone(),
        index.folded.clone(),
    ];
    debug_assert_eq!(raw.len(), COLUMNS as usize);
    let compressed: Vec<io::Result<Vec<u8>>> = raw
        .par_iter()
        .map(|col| zstd::bulk::compress(col, ZSTD_LEVEL))
        .collect();

    w.write_all(&COLUMNS.to_le_bytes())?;
    for (col, comp) in raw.iter().zip(compressed) {
        let comp = comp?;
        w.write_all(&(col.len() as u64).to_le_bytes())?;
        w.write_all(&(comp.len() as u64).to_le_bytes())?;
        w.write_all(&crc32fast::hash(&comp).to_le_bytes())?;
        w.write_all(&comp)?;
    }
    w.write_all(END)?;
    w.flush()
}

/// Reads a snapshot written by [`write_snapshot`].
pub fn read_snapshot(r: impl Read) -> Result<(Index, Vec<u8>), SnapshotError> {
    let mut r = BufReader::with_capacity(1 << 20, r);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(SnapshotError::Corrupt("bad magic"));
    }
    let version = read_u32(&mut r)?;
    if version != VERSION {
        return Err(SnapshotError::Version(version));
    }
    let root = read_blob(&mut r)?;
    let generation = read_u64(&mut r)?;
    let epoch = read_u64(&mut r)?;
    let live = read_u64(&mut r)? as usize;
    let garbage = read_u64(&mut r)? as usize;
    let extra = read_blob(&mut r)?;

    if read_u32(&mut r)? != COLUMNS {
        return Err(SnapshotError::Corrupt("column count"));
    }
    let mut compressed = Vec::with_capacity(COLUMNS as usize);
    for _ in 0..COLUMNS {
        let raw_len = read_u64(&mut r)? as usize;
        let comp_len = read_u64(&mut r)? as usize;
        let crc = read_u32(&mut r)?;
        let comp = read_exact_vec(&mut r, comp_len)?;
        if crc32fast::hash(&comp) != crc {
            return Err(SnapshotError::Corrupt("checksum mismatch"));
        }
        compressed.push((raw_len, comp));
    }
    let mut end = [0u8; 8];
    r.read_exact(&mut end)?;
    if &end != END {
        return Err(SnapshotError::Corrupt("missing end marker"));
    }
    let raw: Vec<Vec<u8>> = compressed
        .par_iter()
        .map(|(raw_len, comp)| {
            let col = zstd::bulk::decompress(comp, *raw_len)
                .map_err(|_| SnapshotError::Corrupt("zstd stream"))?;
            if col.len() != *raw_len {
                return Err(SnapshotError::Corrupt("column size"));
            }
            Ok(col)
        })
        .collect::<Result<_, _>>()?;

    let mut index = Index::new(&root, Meta::default());
    index.clear_columns(0);
    let mut cols = raw.into_iter();
    let mut next = || cols.next().expect("column count checked");
    index.parent = decode(&next())?;
    index.first_child = decode(&next())?;
    index.next_sibling = decode(&next())?;
    index.prev_sibling = decode(&next())?;
    index.name_off = decode(&next())?;
    index.name_len = decode(&next())?;
    index.fold_off = decode(&next())?;
    index.fold_len = decode(&next())?;
    index.size = decode(&next())?;
    index.mtime = decode(&next())?;
    index.mode = decode(&next())?;
    index.uid = decode(&next())?;
    index.gid = decode(&next())?;
    index.flags = decode(&next())?;
    index.names = next();
    index.folded = next();
    validate(&index)?;
    index.live = live;
    index.garbage_bytes = garbage;
    index.generation = generation;
    index.epoch = epoch;
    index.rebuild_lookup();
    Ok((index, extra))
}

/// Saves a snapshot atomically: write to a temporary file, fsync, rename.
pub fn save(index: &Index, extra: &[u8], path: &Path) -> io::Result<()> {
    let tmp = path.with_extension("idx.tmp");
    {
        let file = File::create(&tmp)?;
        write_snapshot(index, extra, &file)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        // Persist the rename itself.
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Loads a snapshot saved with [`save`].
pub fn load(path: &Path) -> Result<(Index, Vec<u8>), SnapshotError> {
    read_snapshot(File::open(path)?)
}

fn validate(ix: &Index) -> Result<(), SnapshotError> {
    let n = ix.parent.len();
    let lens = [
        ix.first_child.len(),
        ix.next_sibling.len(),
        ix.prev_sibling.len(),
        ix.name_off.len(),
        ix.name_len.len(),
        ix.fold_off.len(),
        ix.fold_len.len(),
        ix.size.len(),
        ix.mtime.len(),
        ix.mode.len(),
        ix.uid.len(),
        ix.gid.len(),
        ix.flags.len(),
    ];
    if n == 0 || lens.iter().any(|&l| l != n) {
        return Err(SnapshotError::Corrupt("inconsistent column lengths"));
    }
    let id_ok = |id: u32| id == NO_ENTRY || (id as usize) < n;
    for i in 0..n {
        if !(id_ok(ix.parent[i])
            && id_ok(ix.first_child[i])
            && id_ok(ix.next_sibling[i])
            && id_ok(ix.prev_sibling[i]))
        {
            return Err(SnapshotError::Corrupt("entry id out of range"));
        }
        if ix.name_off[i] as usize + ix.name_len[i] as usize > ix.names.len()
            || ix.fold_off[i] as usize + ix.fold_len[i] as usize > ix.folded.len()
        {
            return Err(SnapshotError::Corrupt("name offset out of range"));
        }
    }
    if ix.parent[0] != NO_ENTRY {
        return Err(SnapshotError::Corrupt("root has a parent"));
    }
    Ok(())
}

fn write_blob(w: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "header field too large"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)
}

fn read_blob(r: &mut impl Read) -> Result<Vec<u8>, SnapshotError> {
    let len = read_u32(r)? as usize;
    read_exact_vec(r, len)
}

fn read_exact_vec(r: &mut impl Read, len: usize) -> Result<Vec<u8>, SnapshotError> {
    let mut buf = Vec::new();
    r.take(len as u64).read_to_end(&mut buf)?;
    if buf.len() != len {
        return Err(SnapshotError::Corrupt("truncated"));
    }
    Ok(buf)
}

fn read_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ROOT;

    fn sample() -> Index {
        let mut ix = Index::new(b"/srv/data", Meta::dir(7));
        let d = ix.insert(ROOT, b"Dir", Meta::dir(1));
        ix.insert(d, "Ünïcode.TXT".as_bytes(), Meta::file(42, 5));
        let gone = ix.insert(d, b"gone", Meta::file(1, 1));
        ix.insert(ROOT, b"raw\xffname", Meta::file(9, 9));
        ix.remove(gone);
        ix
    }

    #[test]
    fn roundtrip_preserves_everything() {
        let ix = sample();
        let mut buf = Vec::new();
        write_snapshot(&ix, b"scanner-state", &mut buf).unwrap();
        let (back, extra) = read_snapshot(buf.as_slice()).unwrap();
        assert_eq!(extra, b"scanner-state");
        assert_eq!(back.root_path(), ix.root_path());
        assert_eq!(back.len(), ix.len());
        assert_eq!(back.live_count(), ix.live_count());
        assert_eq!(back.generation(), ix.generation());
        assert_eq!(back.garbage_bytes(), ix.garbage_bytes());
        for id in 0..ix.len() as u32 {
            assert_eq!(back.is_alive(id), ix.is_alive(id));
            if ix.is_alive(id) {
                assert_eq!(back.path(id), ix.path(id));
                assert_eq!(back.meta(id), ix.meta(id));
                assert_eq!(back.folded_name(id), ix.folded_name(id));
            }
        }
        let d = back.lookup(ROOT, b"Dir").expect("lookup table rebuilt");
        assert!(back.lookup(d, "Ünïcode.TXT".as_bytes()).is_some());
        assert!(back.lookup(d, b"gone").is_none());
    }

    #[test]
    fn rejects_corruption() {
        let mut buf = Vec::new();
        write_snapshot(&sample(), b"", &mut buf).unwrap();

        let mut bad = buf.clone();
        bad[0] = b'X';
        assert!(matches!(
            read_snapshot(bad.as_slice()),
            Err(SnapshotError::Corrupt(_))
        ));

        let mut bad = buf.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        assert!(read_snapshot(bad.as_slice()).is_err());

        let truncated = &buf[..buf.len() - 20];
        assert!(read_snapshot(truncated).is_err());

        let mut bad = buf.clone();
        bad[8] = 99;
        assert!(matches!(
            read_snapshot(bad.as_slice()),
            Err(SnapshotError::Version(_))
        ));
    }

    #[test]
    fn save_and_load_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vol.idx");
        save(&sample(), b"x", &path).unwrap();
        assert!(!path.with_extension("idx.tmp").exists());
        let (ix, extra) = load(&path).unwrap();
        assert_eq!(extra, b"x");
        assert_eq!(ix.live_count(), sample().live_count());
    }
}
