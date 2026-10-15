//! Engine tests for the Everything 1.5 search syntax.

use std::sync::atomic::AtomicBool;

use super::*;
use crate::query::{parse, ParseOptions};
use crate::store::Meta;

/// /w/{Résumé.pdf, "My Report (final).docx", שָׁלוֹם.txt, "שלום עולם.md",
///     Москва.jpg, .hidden, link (symlink), ro.txt (read-only),
///     docs/{a.txt, b.txt}, docs2/{a.txt}, empty/, src/{main.rs}}
fn features() -> Index {
    let mut ix = Index::new(b"/w", Meta::dir(0));
    let file = |size: u64| Meta {
        btime: 5,
        atime: 7,
        ..Meta::file(size, 3)
    };
    for (name, size) in [
        ("Résumé.pdf", 10),
        ("My Report (final).docx", 20),
        ("שָׁלוֹם.txt", 30),
        ("שלום עולם.md", 40),
        ("Москва.jpg", 50),
        (".hidden", 1),
    ] {
        ix.insert(ROOT, name.as_bytes(), file(size));
    }
    ix.insert(
        ROOT,
        b"link",
        Meta {
            mode: mode::S_IFLNK | 0o777,
            ..file(4)
        },
    );
    ix.insert(
        ROOT,
        b"ro.txt",
        Meta {
            mode: mode::S_IFREG | 0o444,
            ..file(2)
        },
    );
    let docs = ix.insert(ROOT, b"docs", Meta::dir(1));
    ix.insert(docs, b"a.txt", file(100));
    ix.insert(docs, b"b.txt", file(100));
    let docs2 = ix.insert(ROOT, b"docs2", Meta::dir(1));
    ix.insert(docs2, b"a.txt", file(7));
    ix.insert(ROOT, b"empty", Meta::dir(1));
    let src = ix.insert(ROOT, b"src", Meta::dir(1));
    ix.insert(src, b"main.rs", file(9));
    ix
}

fn run(ix: &Index, q: &str, extra: &dyn Extra) -> Vec<Hit> {
    Query::compile(&parse(q, &ParseOptions::default()))
        .unwrap()
        .run_with(&[(0, ix)], None, extra, &AtomicBool::new(false))
        .unwrap()
}

/// Paths relative to the volume root, sorted.
fn find(ix: &Index, q: &str) -> Vec<String> {
    let mut v: Vec<String> = run(ix, q, &NoExtra)
        .iter()
        .map(|h| {
            String::from_utf8_lossy(&ix.path(h.id))
                .trim_start_matches("/w")
                .to_owned()
        })
        .collect();
    v.sort();
    v
}

#[test]
fn diacritics_are_ignored_by_default() {
    let ix = features();
    assert_eq!(find(&ix, "resume"), ["/Résumé.pdf"]);
    assert_eq!(find(&ix, "RÉSUMÉ"), ["/Résumé.pdf"]);
    assert!(find(&ix, "diacritics:resume").is_empty());
    assert_eq!(find(&ix, "diacritics:résumé"), ["/Résumé.pdf"]);
    assert_eq!(
        find(&ix, "case:Resume"),
        ["/Résumé.pdf"],
        "case: still ignores diacritics"
    );
    assert!(find(&ix, "case:resume").is_empty());
}

#[test]
fn hebrew_and_other_scripts() {
    let ix = features();
    // Niqqud (vowel points) are diacritics: plain letters find both.
    assert_eq!(find(&ix, "שלום"), ["/שָׁלוֹם.txt", "/שלום עולם.md"]);
    assert_eq!(find(&ix, "diacritics:שלום"), ["/שלום עולם.md"]);
    assert_eq!(find(&ix, "\"שלום עולם\""), ["/שלום עולם.md"]);
    assert_eq!(find(&ix, "ww:עולם"), ["/שלום עולם.md"]);
    assert!(find(&ix, "ww:עול").is_empty());
    assert_eq!(find(&ix, "startwith:שלום"), ["/שָׁלוֹם.txt", "/שלום עולם.md"]);
    assert_eq!(find(&ix, "endwith:עולם.md"), ["/שלום עולם.md"]);
    assert_eq!(
        find(&ix, "????.txt"),
        ["/שָׁלוֹם.txt"],
        "? is one Hebrew letter"
    );
    assert_eq!(find(&ix, "len:12"), ["/שלום עולם.md"]);
    assert_eq!(find(&ix, "москва"), ["/Москва.jpg"]);
    assert_eq!(find(&ix, "ext:md;jpg"), ["/Москва.jpg", "/שלום עולם.md"]);
    assert_eq!(
        find(&ix, "/w/שלום"),
        ["/שָׁלוֹם.txt", "/שלום עולם.md"],
        "path matching"
    );
}

#[test]
fn anchors_words_and_normalization() {
    let ix = features();
    assert_eq!(find(&ix, "startwith:my"), ["/My Report (final).docx"]);
    assert!(find(&ix, "startwith:report").is_empty());
    assert_eq!(find(&ix, "endwith:.docx"), ["/My Report (final).docx"]);
    assert_eq!(find(&ix, "wfn:a.txt"), ["/docs/a.txt", "/docs2/a.txt"]);
    assert!(find(&ix, "wfn:a.tx").is_empty());
    assert_eq!(find(&ix, "prefix:rep"), ["/My Report (final).docx"]);
    assert!(find(&ix, "prefix:port").is_empty());
    assert_eq!(find(&ix, "suffix:port"), ["/My Report (final).docx"]);
    assert!(find(&ix, "suffix:rep").is_empty());
    assert_eq!(
        find(&ix, "ignorepunc:ignorews:reportfinal"),
        ["/My Report (final).docx"]
    );
    assert_eq!(
        find(&ix, "ignorepunc:finaldocx"),
        ["/My Report (final).docx"]
    );
    assert!(find(&ix, "reportfinal").is_empty());
    assert_eq!(find(&ix, "ignorews:myreport"), ["/My Report (final).docx"]);
    assert_eq!(
        find(&ix, "&quot:"),
        Vec::<String>::new(),
        "entities decode to literals"
    );
}

#[test]
fn name_parts() {
    let ix = features();
    assert_eq!(
        find(&ix, "stem:a"),
        [
            "/My Report (final).docx",
            "/docs/a.txt",
            "/docs2/a.txt",
            "/src/main.rs"
        ]
    );
    assert!(
        find(&ix, "stem:txt").is_empty(),
        "the extension is not part of the stem"
    );
    assert_eq!(find(&ix, "name:main"), ["/src/main.rs"]);
    assert_eq!(find(&ix, "location:docs2"), ["/docs2/a.txt"]);
    assert_eq!(find(&ix, "location:/w/src"), ["/src/main.rs"]);
}

#[test]
fn structure_and_attributes() {
    let ix = features();
    assert_eq!(
        find(&ix, "filelist:a.txt;MAIN.RS"),
        ["/docs/a.txt", "/docs2/a.txt", "/src/main.rs"]
    );
    assert_eq!(find(&ix, "child:main"), ["/src"]);
    assert_eq!(find(&ix, "child-count:2"), ["/docs"]);
    assert_eq!(find(&ix, "child-file-count:1"), ["/docs2", "/src"]);
    assert_eq!(
        find(&ix, "child-folder-count:0 folder:"),
        ["/docs", "/docs2", "/empty", "/src"]
    );
    assert_eq!(find(&ix, "empty:"), ["/empty"]);
    assert_eq!(find(&ix, "attrib:h"), ["/.hidden"]);
    assert_eq!(find(&ix, "attrib:r"), ["/ro.txt"]);
    assert_eq!(find(&ix, "attrib:l"), ["/link"]);
    assert!(
        find(&ix, "attrib:s").is_empty(),
        "no system attribute on Linux"
    );
    assert_eq!(find(&ix, "attrib:d").len(), 4);
    assert_eq!(find(&ix, "root:"), [""], "the volume root itself");
    assert_eq!(
        attributes_of(&ix, ix.lookup(ROOT, b"ro.txt").unwrap()),
        "RA"
    );
    assert_eq!(
        attributes_of(&ix, ix.lookup(ROOT, b".hidden").unwrap()),
        "HA"
    );
    assert_eq!(attributes_of(&ix, ix.lookup(ROOT, b"docs").unwrap()), "D");
}

#[test]
fn types_and_dates() {
    let ix = features();
    assert_eq!(find(&ix, "type:\"jpeg image\""), ["/Москва.jpg"]);
    assert_eq!(find(&ix, "type:folder").len(), 4);
    assert_eq!(
        find(&ix, "file: dc:1970").len(),
        12,
        "birth times are 5 ns after the epoch"
    );
    assert!(find(&ix, "dc:2000").is_empty());
    assert!(find(&ix, "da:>100").is_empty());
    assert_eq!(
        find(&ix, "rc:today").len(),
        16,
        "everything was just indexed"
    );
    assert_eq!(
        find(&ix, "dc:unknown"),
        ["/docs", "/docs2", "/empty", "/src"],
        "folders have no birth time here"
    );
}

#[test]
fn count_and_dupes() {
    let ix = features();
    let q = Query::compile(&parse("count:3 file:", &ParseOptions::default())).unwrap();
    assert_eq!(q.limit(), Some(3));
    assert_eq!(find(&ix, "dupe:"), ["/docs/a.txt", "/docs2/a.txt"]);
    assert_eq!(find(&ix, "sizedupe: file:"), ["/docs/a.txt", "/docs/b.txt"]);
    assert!(find(&ix, "dupe:name;size").is_empty());
    assert_eq!(find(&ix, "dupe:ext ext:txt").len(), 5);
}

#[test]
fn value_operators() {
    let ix = features();
    assert_eq!(find(&ix, "size:100"), ["/docs/a.txt", "/docs/b.txt"]);
    assert_eq!(find(&ix, "size:!=100 ext:txt").len(), 3);
    assert_eq!(find(&ix, "size:1;2"), ["/.hidden", "/ro.txt"]);
    assert_eq!(
        find(&ix, "size:5-10 file:"),
        ["/Résumé.pdf", "/docs2/a.txt", "/src/main.rs"]
    );
    assert_eq!(
        find(&ix, "ext:<txt|md> size:<30|40>"),
        ["/שָׁלוֹם.txt", "/שלום עולם.md"]
    );
}

struct TestExtra;

impl Extra for TestExtra {
    fn folder_size(&self, _vol: u16, _id: EntryId) -> Option<u64> {
        Some(200)
    }

    fn run_info(&self, _vol: u16, id: EntryId) -> RunInfo {
        RunInfo {
            count: id % 3,
            last: id as i64,
        }
    }
}

#[test]
fn extra_information() {
    let ix = features();
    assert_eq!(
        run(&ix, "size:200 folder:", &TestExtra).len(),
        4,
        "folder sizes"
    );
    assert!(run(&ix, "size:200 folder:", &NoExtra).is_empty());
    assert!(!run(&ix, "run-count:2", &TestExtra).is_empty());
    assert!(run(&ix, "run-count:2", &NoExtra).is_empty());
    assert!(
        !run(&ix, "dr:1970", &TestExtra).is_empty(),
        "run times are tiny test values"
    );
    assert!(run(&ix, "dr:1970", &NoExtra).is_empty());
    let mut hits = run(&ix, "file:", &TestExtra);
    crate::sort::sort_hits_with(
        &mut hits,
        &[(0, &ix)],
        crate::sort::SortSpec {
            key: crate::sort::SortKey::RunCount,
            descending: true,
        },
        &TestExtra,
    );
    assert_eq!(hits[0].id % 3, 2);
}

#[test]
fn content_and_image_properties_from_disk() {
    use std::os::unix::ffi::OsStrExt;
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "Hello שלום World").unwrap();
    std::fs::write(tmp.path().join("other.txt"), "nothing here").unwrap();
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend(1920u32.to_be_bytes());
    png.extend(1080u32.to_be_bytes());
    png.extend([8, 2, 0, 0, 0, 0, 0, 0, 0]);
    std::fs::write(tmp.path().join("photo.png"), png).unwrap();
    let mut ix = Index::new(tmp.path().as_os_str().as_bytes(), Meta::dir(0));
    for name in ["hello.txt", "other.txt", "photo.png"] {
        let len = std::fs::metadata(tmp.path().join(name)).unwrap().len();
        ix.insert(ROOT, name.as_bytes(), Meta::file(len, 1));
    }
    let names = |q: &str| -> Vec<String> {
        let mut v: Vec<String> = run(&ix, q, &NoExtra)
            .iter()
            .map(|h| String::from_utf8_lossy(ix.name(h.id)).into_owned())
            .collect();
        v.sort();
        v
    };
    assert_eq!(names("content:world"), ["hello.txt"]);
    assert_eq!(names("content:שלום"), ["hello.txt"]);
    assert!(names("case:content:world").is_empty());
    assert_eq!(names("content:<hello world>"), ["hello.txt"]);
    assert_eq!(names("regex:content:^nothing"), ["other.txt"]);
    assert_eq!(names("width:1920 height:>1000"), ["photo.png"]);
    assert!(names("width:<100").is_empty());

    struct NoRead;
    impl Extra for NoRead {
        fn can_read(&self, _: u16, _: &Index, _: EntryId) -> bool {
            false
        }
    }
    let unreadable = run(&ix, "content:world", &NoRead);
    assert!(
        unreadable.is_empty(),
        "content is only read when the caller may read the file"
    );
}
