//! Property tests: the optimized engine must agree with a naive reference
//! implementation on random trees, random mutations and random queries, and
//! the store must keep its structural invariants through compaction and
//! snapshot round trips.

use std::collections::BTreeSet;
use std::sync::atomic::AtomicBool;

use evos_core::fold::fold_str;
use evos_core::query::{parse, Expr, Filter, Kind, ParseOptions, PatternKind, Target, TextMatch};
use evos_core::search::Query;
use evos_core::{persist, EntryId, Index, Meta, ROOT};
use proptest::prelude::*;
use regex::Regex;

#[derive(Debug, Clone)]
enum Op {
    Insert {
        parent: usize,
        name: String,
        dir: bool,
    },
    Remove {
        target: usize,
    },
    Rename {
        target: usize,
        parent: usize,
        name: String,
    },
}

fn name_strategy() -> impl Strategy<Value = String> {
    proptest::string::string_regex("[aAbBé._]{1,5}").unwrap()
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (any::<usize>(), name_strategy(), any::<bool>())
            .prop_map(|(parent, name, dir)| Op::Insert { parent, name, dir }),
        1 => any::<usize>().prop_map(|target| Op::Remove { target }),
        1 => (any::<usize>(), any::<usize>(), name_strategy())
            .prop_map(|(target, parent, name)| Op::Rename { target, parent, name }),
    ]
}

fn build(ops: &[Op]) -> Index {
    let mut ix = Index::new(b"/r", Meta::dir(0));
    for op in ops {
        let live: Vec<EntryId> = ix.live_ids().collect();
        let dirs: Vec<EntryId> = live.iter().copied().filter(|&id| ix.is_dir(id)).collect();
        match op {
            Op::Insert { parent, name, dir } => {
                let parent = dirs[parent % dirs.len()];
                let meta = if *dir {
                    Meta::dir(1)
                } else {
                    Meta::file(name.len() as u64, 2)
                };
                ix.upsert(parent, name.as_bytes(), meta);
            }
            Op::Remove { target } => {
                let id = live[target % live.len()];
                if id != ROOT {
                    ix.remove(id);
                }
            }
            Op::Rename {
                target,
                parent,
                name,
            } => {
                let id = live[target % live.len()];
                let parent = dirs[parent % dirs.len()];
                if id != ROOT {
                    // Impossible moves are refused by the store itself.
                    ix.rename(id, parent, name.as_bytes());
                }
            }
        }
    }
    ix
}

fn term_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        name_strategy(),
        "[aAbB]{1,2}".prop_map(|s| format!("case:{s}")),
        "[ab]{1,2}".prop_map(|s| format!("*{s}*")),
        "[ab]".prop_map(|s| format!("*.{s}")),
        "[ab]".prop_map(|s| format!("{s}?")),
        "[ab]{1,2}".prop_map(|s| format!("{s}/")),
        "[ab]".prop_map(|s| format!("/{s}")),
        "[ab]".prop_map(|s| format!("ext:{s}")),
        Just("folder:".to_owned()),
        Just("file:".to_owned()),
        Just("empty:".to_owned()),
    ]
}

fn atom_strategy() -> impl Strategy<Value = String> {
    (any::<bool>(), term_strategy()).prop_map(|(neg, t)| if neg { format!("!{t}") } else { t })
}

fn query_strategy() -> impl Strategy<Value = String> {
    let group = proptest::collection::vec(atom_strategy(), 1..3)
        .prop_map(|parts| format!("<{}>", parts.join("|")));
    proptest::collection::vec(prop_oneof![3 => atom_strategy(), 1 => group], 0..4)
        .prop_map(|parts| parts.join(" "))
}

/// Reference evaluation directly on strings.
fn reference(expr: &Expr, ix: &Index, id: EntryId) -> bool {
    match expr {
        Expr::All => true,
        Expr::And(v) => v.iter().all(|e| reference(e, ix, id)),
        Expr::Or(v) => v.iter().any(|e| reference(e, ix, id)),
        Expr::Not(e) => !reference(e, ix, id),
        Expr::Filter(f) => match f {
            Filter::Text(t) => text_reference(t, ix, id),
            Filter::Kind(Kind::File) => !ix.is_dir(id),
            Filter::Kind(Kind::Folder) => ix.is_dir(id),
            Filter::Ext(exts) => {
                let name = String::from_utf8_lossy(ix.name(id)).to_lowercase();
                let ext = match name.rfind('.') {
                    Some(0) | None => String::new(),
                    Some(i) => name[i + 1..].to_owned(),
                };
                !ix.is_dir(id) && exts.contains(&ext)
            }
            Filter::Empty => ix.is_dir(id) && ix.children(id).next().is_none(),
            other => panic!("reference does not model {other:?}"),
        },
    }
}

fn text_reference(t: &TextMatch, ix: &Index, id: EntryId) -> bool {
    let raw = match t.target {
        Target::Name => String::from_utf8(ix.name(id).to_vec()).unwrap(),
        Target::Path => String::from_utf8(ix.path(id)).unwrap(),
    };
    let (hay, pat) = if t.case_sensitive {
        (raw, t.pattern.clone())
    } else {
        (fold_str(&raw), fold_str(&t.pattern))
    };
    match t.kind {
        PatternKind::Substring => hay.contains(&pat),
        PatternKind::Wildcard => {
            let re: String = pat
                .chars()
                .map(|c| match c {
                    '*' => ".*".to_owned(),
                    '?' => ".".to_owned(),
                    c => regex::escape(&c.to_string()),
                })
                .collect();
            Regex::new(&format!("(?s)^{re}$")).unwrap().is_match(&hay)
        }
        PatternKind::Regex => unreachable!("not generated"),
    }
}

fn engine(ix: &Index, expr: &Expr) -> BTreeSet<EntryId> {
    let q = Query::compile(expr).unwrap();
    let hits = q.run(&[(0, ix)], None, &AtomicBool::new(false)).unwrap();
    hits.into_iter().map(|h| h.id).collect()
}

fn check_invariants(ix: &Index) {
    let mut count = 0;
    for id in ix.live_ids() {
        count += 1;
        if id == ROOT {
            continue;
        }
        let parent = ix.parent(id);
        assert!(
            ix.is_alive(parent) && ix.is_dir(parent),
            "parent of {id} must be a live folder"
        );
        assert!(
            ix.children(parent).any(|c| c == id),
            "{id} missing from parent's children"
        );
        assert_eq!(ix.lookup(parent, ix.name(id)), Some(id), "lookup of {id}");
        assert_eq!(ix.resolve(&ix.path(id)), Some(id), "resolve of {id}");
    }
    assert_eq!(count, ix.live_count());
    for id in ix.live_ids().filter(|&id| ix.is_dir(id)) {
        for c in ix.children(id) {
            assert!(
                ix.is_alive(c) && ix.parent(c) == id,
                "stale child {c} of {id}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn engine_matches_reference(
        ops in proptest::collection::vec(op_strategy(), 1..60),
        query in query_strategy(),
    ) {
        let ix = build(&ops);
        let expr = parse(&query, &ParseOptions::default());
        let expected: BTreeSet<EntryId> =
            ix.live_ids().filter(|&id| id != ROOT && reference(&expr, &ix, id)).collect();
        prop_assert_eq!(engine(&ix, &expr), expected, "query {:?} expr {:?}", query, expr);
    }

    #[test]
    fn store_invariants_hold(ops in proptest::collection::vec(op_strategy(), 1..80)) {
        let mut ix = build(&ops);
        check_invariants(&ix);
        ix.compact();
        check_invariants(&ix);
    }

    #[test]
    fn compaction_and_snapshots_preserve_results(
        ops in proptest::collection::vec(op_strategy(), 1..60),
        query in query_strategy(),
    ) {
        let mut ix = build(&ops);
        let expr = parse(&query, &ParseOptions::default());
        let paths = |ix: &Index, ids: BTreeSet<EntryId>| -> BTreeSet<Vec<u8>> {
            ids.into_iter().map(|id| ix.path(id)).collect()
        };
        let before = paths(&ix, engine(&ix, &expr));

        ix.compact();
        prop_assert_eq!(&paths(&ix, engine(&ix, &expr)), &before);

        let mut buf = Vec::new();
        persist::write_snapshot(&ix, b"", &mut buf).unwrap();
        let (loaded, _) = persist::read_snapshot(buf.as_slice()).unwrap();
        check_invariants(&loaded);
        prop_assert_eq!(&paths(&loaded, engine(&loaded, &expr)), &before);
    }
}
