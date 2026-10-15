//! Reading file content and properties from disk for `content:`, `width:`,
//! `artist:` and friends.
//!
//! Searching must never modify what it searches: files are opened read-only
//! with `O_NOATIME`, so not even their access times change. (The kernel only
//! allows `O_NOATIME` for the file's owner or with CAP_FOWNER; the daemon
//! runs as root. Otherwise the open falls back to a plain read-only open.)

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use crate::store::extension;
use crate::types;

/// Files larger than this are skipped by `content:`.
pub const MAX_CONTENT: u64 = 64 << 20;

/// `O_NOATIME` on Linux (x86, x86-64, ARM, AArch64, RISC-V).
pub const O_NOATIME: i32 = 0o1_000_000;

/// Opens a regular file for reading without updating its access time.
///
/// The daemon reads as root on behalf of users, so the path must lead
/// exactly to the file the index describes: no component may be a
/// symbolic link (a user could otherwise point one at a file they cannot
/// read, or swap a folder for one after it was indexed), and FIFOs and
/// devices are refused rather than read (opening a FIFO would block).
pub fn open_readonly(path: &[u8]) -> io::Result<File> {
    use rustix::fs::{openat2, Mode, OFlags, ResolveFlags, CWD};
    let flags =
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY;
    let path = Path::new(OsStr::from_bytes(path));
    let open = |extra: OFlags| {
        match openat2(
            CWD,
            path,
            flags | extra,
            Mode::empty(),
            ResolveFlags::NO_SYMLINKS,
        ) {
            // Before Linux 5.6 only the final component can be checked.
            Err(rustix::io::Errno::NOSYS) => rustix::fs::open(path, flags | extra, Mode::empty()),
            result => result,
        }
    };
    let fd = match open(OFlags::from_bits_retain(O_NOATIME as u32)) {
        // Not the owner and no CAP_FOWNER: O_NOATIME is not permitted.
        Err(rustix::io::Errno::PERM) => open(OFlags::empty()),
        result => result,
    }?;
    let file = File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// Reads up to `limit` bytes of a file, without updating its access time.
pub fn read_bytes(path: &[u8], limit: u64) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    open_readonly(path)?.take(limit).read_to_end(&mut data)?;
    Ok(data)
}

/// Reads up to `limit` bytes of a file as text (see [`decode_text`]).
pub fn read_content(path: &[u8], limit: u64) -> io::Result<Vec<u8>> {
    Ok(decode_text(read_bytes(path, limit)?))
}

/// UTF-16 text (with a byte order mark) converted to UTF-8; anything else
/// is returned as is.
pub fn decode_text(data: Vec<u8>) -> Vec<u8> {
    let utf16 = |bytes: &[u8], le: bool| {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| {
                if le {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units).into_bytes()
    };
    match data.get(..2) {
        Some([0xff, 0xfe]) => utf16(&data[2..], true),
        Some([0xfe, 0xff]) => utf16(&data[2..], false),
        _ => data,
    }
}

/// Properties read from file headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Props {
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub bit_depth: Option<u64>,
    /// Duration in milliseconds.
    pub length_ms: Option<u64>,
    pub track: Option<u64>,
    pub album: Option<String>,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
}

/// Does the file type carry the properties we read?
pub fn has_props(name: &[u8]) -> bool {
    let ext = String::from_utf8_lossy(extension(name)).to_lowercase();
    [types::IMAGE, types::AUDIO, types::VIDEO]
        .iter()
        .any(|list| list.contains(&ext.as_str()))
}

/// Reads properties from a file's headers.
pub fn read_props(path: &[u8]) -> Props {
    let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
    let ext = String::from_utf8_lossy(extension(name)).to_lowercase();
    let mut props = Props::default();
    let Ok(file) = open_readonly(path) else {
        return props;
    };
    if types::IMAGE.contains(&ext.as_str()) {
        if let Ok(size) = imagesize::reader_size(BufReader::new(file)) {
            props.width = Some(size.width as u64);
            props.height = Some(size.height as u64);
        }
    } else if types::AUDIO.contains(&ext.as_str()) || types::VIDEO.contains(&ext.as_str()) {
        read_media(file, &mut props);
    }
    props
}

fn read_media(file: File, props: &mut Props) {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::tag::Accessor;
    let Ok(probe) = lofty::probe::Probe::new(BufReader::new(file)).guess_file_type() else {
        return;
    };
    let Ok(tagged) = probe.read() else { return };
    let audio = tagged.properties();
    props.length_ms = Some(audio.duration().as_millis() as u64);
    props.bit_depth = audio.bit_depth().map(u64::from);
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        props.track = tag.track().map(u64::from);
        props.album = tag.album().map(|s| s.into_owned());
        props.artist = tag.artist().map(|s| s.into_owned());
        props.title = tag.title().map(|s| s.into_owned());
        props.genre = tag.genre().map(|s| s.into_owned());
        props.comment = tag.comment().map(|s| s.into_owned());
    }
}

type CacheKey = Vec<u8>;
type CacheValue = (i64, u64, Arc<Props>);

fn cache() -> &'static Mutex<HashMap<CacheKey, CacheValue>> {
    static CACHE: OnceLock<Mutex<HashMap<CacheKey, CacheValue>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

const CACHE_LIMIT: usize = 200_000;

/// Properties of a file, cached until its size or mtime change.
pub fn cached_props(path: &[u8], mtime: i64, size: u64) -> Arc<Props> {
    if let Some((m, s, props)) = cache().lock().expect("props cache").get(path) {
        if *m == mtime && *s == size {
            return props.clone();
        }
    }
    let props = Arc::new(read_props(path));
    let mut cache = cache().lock().expect("props cache");
    if cache.len() >= CACHE_LIMIT {
        cache.clear();
    }
    cache.insert(path.to_vec(), (mtime, size, props.clone()));
    props
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    fn b(p: &Path) -> Vec<u8> {
        p.as_os_str().as_bytes().to_vec()
    }

    #[test]
    fn content_reads_utf8_and_utf16() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain.txt");
        std::fs::write(&plain, "שלום world").unwrap();
        assert_eq!(
            read_content(&b(&plain), MAX_CONTENT).unwrap(),
            "שלום world".as_bytes()
        );
        let wide = tmp.path().join("wide.txt");
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend("héllo".encode_utf16().flat_map(|u| u.to_le_bytes()));
        std::fs::write(&wide, bytes).unwrap();
        assert_eq!(
            read_content(&b(&wide), MAX_CONTENT).unwrap(),
            "héllo".as_bytes()
        );
        assert_eq!(read_content(&b(&plain), 4).unwrap().len(), 4, "limit");
    }

    #[test]
    fn image_dimensions() {
        let tmp = tempfile::tempdir().unwrap();
        let png = tmp.path().join("a.png");
        // Minimal PNG header with IHDR 3x2.
        let mut data = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        data.extend(3u32.to_be_bytes());
        data.extend(2u32.to_be_bytes());
        data.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
        std::fs::write(&png, data).unwrap();
        let props = read_props(&b(&png));
        assert_eq!((props.width, props.height), (Some(3), Some(2)));
        assert!(has_props(b"x.PNG") && has_props(b"x.mp3") && !has_props(b"x.txt"));
        let cached = cached_props(&b(&png), 1, 2);
        assert_eq!(cached.width, Some(3));
    }

    #[test]
    fn reading_does_not_touch_access_times() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f.txt");
        std::fs::write(&f, "data").unwrap();
        let old = filetime_atime(&f);
        // Make the access time old enough that relatime would update it.
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86_400);
        File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_accessed(past)
                    .set_modified(past),
            )
            .unwrap();
        let before = filetime_atime(&f);
        assert_ne!(before, old);
        read_content(&b(&f), MAX_CONTENT).unwrap();
        assert_eq!(
            filetime_atime(&f),
            before,
            "O_NOATIME keeps the access time"
        );
    }

    #[test]
    fn only_regular_files_are_read_and_links_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let secret = tmp.path().join("secret");
        std::fs::create_dir(&secret).unwrap();
        std::fs::write(secret.join("key"), "s3cret").unwrap();
        let mine = tmp.path().join("mine");
        std::fs::create_dir(&mine).unwrap();
        std::os::unix::fs::symlink(secret.join("key"), mine.join("link")).unwrap();
        std::os::unix::fs::symlink(&secret, mine.join("dirlink")).unwrap();
        assert!(read_bytes(&b(&secret.join("key")), 100).is_ok());
        assert!(
            read_bytes(&b(&mine.join("link")), 100).is_err(),
            "final link"
        );
        assert!(
            read_bytes(&b(&mine.join("dirlink/key")), 100).is_err(),
            "link in the middle of the path"
        );
        assert!(read_bytes(&b(&secret), 100).is_err(), "a folder");
        let fifo = tmp.path().join("fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        if made {
            // Opening a FIFO for reading would block until a writer comes.
            assert!(read_bytes(&b(&fifo), 100).is_err(), "a FIFO");
        }
    }

    fn filetime_atime(p: &Path) -> std::time::SystemTime {
        std::fs::metadata(p).unwrap().accessed().unwrap()
    }
}
