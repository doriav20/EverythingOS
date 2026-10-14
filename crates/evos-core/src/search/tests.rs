use super::*;
use crate::query::{parse, ParseOptions};
use crate::store::Meta;

const DAY: i64 = 86_400 * 1_000_000_000;
const NOW: i64 = 1_700_000_000;

/// /home/Alice/{Photos/{IMG_0001.JPG, cat.png, empty/}, notes.txt, run.sh*}
/// /home/bob/{Report Final.pdf, src/{main.rs, lib.rs}}
fn tree() -> Index {
    let old = (NOW - 400 * 86_400) * 1_000_000_000;
    let new = NOW * 1_000_000_000 - DAY / 2;
    let mut ix = Index::new(b"/home", Meta::dir(0));
    let alice = ix.insert(ROOT, b"Alice", Meta::dir(old));
    let photos = ix.insert(alice, b"Photos", Meta::dir(old));
    ix.insert(photos, b"IMG_0001.JPG", Meta::file(3 << 20, old));
    ix.insert(photos, b"cat.png", Meta::file(200 << 10, new));
    ix.insert(photos, b"empty", Meta::dir(old));
    ix.insert(alice, b"notes.txt", Meta::file(120, new));
    let mut exec = Meta::file(50, old);
    exec.mode = mode::S_IFREG | 0o755;
    ix.insert(alice, b"run.sh", exec);
    let bob = ix.insert(ROOT, b"bob", Meta::dir(old));
    ix.insert(bob, b"Report Final.pdf", Meta::file(2 << 20, old));
    let src = ix.insert(bob, b"src", Meta::dir(new));
    ix.insert(src, b"main.rs", Meta::file(900, new));
    ix.insert(src, b"lib.rs", Meta::file(400, new));
    ix
}

fn search_with(ix: &Index, q: &str, opts: &ParseOptions) -> Vec<String> {
    let query = Query::compile(&parse(q, opts)).unwrap();
    let hits = query
        .run(&[(0, ix)], None, &AtomicBool::new(false))
        .unwrap();
    let mut names: Vec<String> = hits
        .iter()
        .map(|h| String::from_utf8_lossy(&ix.path(h.id)).into_owned())
        .collect();
    names.sort();
    names
}

fn search(ix: &Index, q: &str) -> Vec<String> {
    search_with(
        ix,
        q,
        &ParseOptions {
            now: Some(NOW),
            ..Default::default()
        },
    )
}

fn names(paths: &[String]) -> Vec<&str> {
    let mut v: Vec<&str> = paths
        .iter()
        .map(|p| p.rsplit('/').next().unwrap())
        .collect();
    v.sort();
    v
}

#[test]
fn empty_query_returns_everything_but_the_root() {
    let ix = tree();
    assert_eq!(search(&ix, "").len(), ix.live_count() - 1);
}

#[test]
fn substring_is_case_insensitive_by_default() {
    let ix = tree();
    assert_eq!(names(&search(&ix, "img")), vec!["IMG_0001.JPG"]);
    assert_eq!(names(&search(&ix, "case:img")), Vec::<&str>::new());
    assert_eq!(names(&search(&ix, "case:IMG")), vec!["IMG_0001.JPG"]);
}

#[test]
fn boolean_operators() {
    let ix = tree();
    assert_eq!(names(&search(&ix, "rs !main")), vec!["lib.rs"]);
    assert_eq!(
        names(&search(&ix, "cat|notes")),
        vec!["cat.png", "notes.txt"]
    );
    assert_eq!(names(&search(&ix, "<cat|notes> png")), vec!["cat.png"]);
}

#[test]
fn quoted_phrase() {
    let ix = tree();
    assert_eq!(
        names(&search(&ix, "\"report final\"")),
        vec!["Report Final.pdf"]
    );
    assert!(search(&ix, "\"report  final\"").is_empty());
}

#[test]
fn whole_word() {
    let ix = tree();
    assert_eq!(names(&search(&ix, "ww:final")), vec!["Report Final.pdf"]);
    assert!(search(&ix, "ww:fin").is_empty());
}

#[test]
fn wildcards_match_whole_name() {
    let ix = tree();
    assert_eq!(names(&search(&ix, "*.rs")), vec!["lib.rs", "main.rs"]);
    assert_eq!(names(&search(&ix, "img_????.jpg")), vec!["IMG_0001.JPG"]);
    assert!(search(&ix, "*.r").is_empty());
}

#[test]
fn path_substring() {
    let ix = tree();
    assert_eq!(
        names(&search(&ix, "bob/src")),
        vec!["lib.rs", "main.rs", "src"]
    );
    assert_eq!(names(&search(&ix, "alice/photos/c")), vec!["cat.png"]);
    assert_eq!(search(&ix, "path:home").len(), ix.live_count() - 1);
    assert!(search(&ix, "/nope/").is_empty());
}

#[test]
fn path_wildcards_and_regex() {
    let ix = tree();
    assert_eq!(names(&search(&ix, "*/src/*.rs")), vec!["lib.rs", "main.rs"]);
    assert_eq!(names(&search(&ix, "/home/*/notes.txt")), vec!["notes.txt"]);
    assert_eq!(
        names(&search(&ix, r#"path:regex:"alice/.*\.(txt|sh)$""#)),
        vec!["notes.txt", "run.sh"]
    );
}

#[test]
fn regex_on_names() {
    let ix = tree();
    assert_eq!(
        names(&search(&ix, "regex:^[a-z]+\\.rs$")),
        vec!["lib.rs", "main.rs"]
    );
    let err = Query::compile(&parse("regex:(", &ParseOptions::default()));
    assert!(matches!(err, Err(QueryError::Regex(_))));
}

#[test]
fn kind_filters() {
    let ix = tree();
    assert_eq!(
        names(&search(&ix, "folder:")),
        vec!["Alice", "Photos", "bob", "empty", "src"]
    );
    assert_eq!(
        names(&search(&ix, "file:s")),
        vec!["lib.rs", "main.rs", "notes.txt", "run.sh"]
    );
}

#[test]
fn extension_size_and_date() {
    let ix = tree();
    assert_eq!(
        names(&search(&ix, "ext:jpg;PNG")),
        vec!["IMG_0001.JPG", "cat.png"]
    );
    assert_eq!(
        names(&search(&ix, "size:>=1mb")),
        vec!["IMG_0001.JPG", "Report Final.pdf"]
    );
    assert_eq!(names(&search(&ix, "size:medium")), vec!["cat.png"]);
    assert_eq!(
        names(&search(&ix, "file: dm:last2days")),
        vec!["cat.png", "lib.rs", "main.rs", "notes.txt"]
    );
    assert_eq!(names(&search(&ix, "pic:")), vec!["IMG_0001.JPG", "cat.png"]);
}

#[test]
fn structure_filters() {
    let ix = tree();
    assert_eq!(
        names(&search(&ix, "parent:/home/alice")),
        vec!["Photos", "notes.txt", "run.sh"]
    );
    assert_eq!(
        names(&search(&ix, "under:/home/bob")),
        vec!["Report Final.pdf", "lib.rs", "main.rs", "src"]
    );
    assert_eq!(search(&ix, "under:/").len(), ix.live_count() - 1);
    assert_eq!(names(&search(&ix, "depth:2")), vec!["Alice", "bob"]);
    assert_eq!(
        names(&search(&ix, "len:<=6 file:")),
        vec!["lib.rs", "run.sh"]
    );
    assert_eq!(names(&search(&ix, "empty:")), vec!["empty"]);
    assert_eq!(names(&search(&ix, "exe:")), vec!["run.sh"]);
}

#[test]
fn global_options() {
    let ix = tree();
    let opts = ParseOptions {
        match_path: true,
        ..Default::default()
    };
    assert_eq!(
        names(&search_with(&ix, "photos", &opts)),
        vec!["IMG_0001.JPG", "Photos", "cat.png", "empty"]
    );
    let opts = ParseOptions {
        regex: true,
        ..Default::default()
    };
    assert_eq!(
        names(&search_with(&ix, "^(lib|main)\\.rs$", &opts)),
        vec!["lib.rs", "main.rs"]
    );
}

#[test]
fn deleted_entries_are_not_found() {
    let mut ix = tree();
    let bob = ix.lookup(ROOT, b"bob").unwrap();
    ix.remove(bob);
    assert!(search(&ix, "rs").is_empty());
}

#[test]
fn entry_filter_is_applied() {
    let ix = tree();
    let query = Query::compile(&parse("rs", &ParseOptions::default())).unwrap();
    let only_lib = |_: u16, ix: &Index, id: EntryId| ix.name(id) == b"lib.rs";
    let hits = query
        .run(&[(0, &ix)], Some(&only_lib), &AtomicBool::new(false))
        .unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn refinement_matches_full_search() {
    let ix = tree();
    let cancel = AtomicBool::new(false);
    let broad = Query::compile(&parse("s", &ParseOptions::default())).unwrap();
    let narrow = Query::compile(&parse("src", &ParseOptions::default())).unwrap();
    let prev = broad.run(&[(0, &ix)], None, &cancel).unwrap();
    let mut refined = narrow
        .run_within(&[(0, &ix)], &prev, None, &cancel)
        .unwrap();
    let mut full = narrow.run(&[(0, &ix)], None, &cancel).unwrap();
    refined.sort();
    full.sort();
    assert_eq!(refined, full);
}

#[test]
fn multiple_volumes_and_cancellation() {
    let a = tree();
    let mut b = Index::new(b"/mnt/share", Meta::dir(0));
    b.insert(ROOT, b"main.rs", Meta::file(1, 0));
    let query = Query::compile(&parse("main.rs", &ParseOptions::default())).unwrap();
    let hits = query
        .run(&[(3, &a), (7, &b)], None, &AtomicBool::new(false))
        .unwrap();
    let mut vols: Vec<u16> = hits.iter().map(|h| h.vol).collect();
    vols.sort();
    assert_eq!(vols, vec![3, 7]);
    assert!(query
        .run(&[(3, &a)], None, &AtomicBool::new(true))
        .is_none());
}
