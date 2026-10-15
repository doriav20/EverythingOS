use super::*;

fn p(q: &str) -> Expr {
    parse(
        q,
        &ParseOptions {
            now: Some(1_700_000_000),
            ..Default::default()
        },
    )
}

fn sub(s: &str) -> Expr {
    Expr::Filter(Filter::Text(TextMatch::substring(s)))
}

fn text(e: &Expr) -> &TextMatch {
    match e {
        Expr::Filter(Filter::Text(t)) => t,
        other => panic!("expected text filter, got {other:?}"),
    }
}

fn filter(e: &Expr) -> &Filter {
    match e {
        Expr::Filter(f) => f,
        other => panic!("expected filter, got {other:?}"),
    }
}

#[test]
fn empty_query_matches_all() {
    assert_eq!(p(""), Expr::All);
    assert_eq!(p("   "), Expr::All);
    assert_eq!(p("<>"), Expr::All);
}

#[test]
fn juxtaposition_is_and() {
    assert_eq!(p("a b"), Expr::And(vec![sub("a"), sub("b")]));
}

#[test]
fn or_binds_tighter_than_and() {
    assert_eq!(
        p("a b|c"),
        Expr::And(vec![sub("a"), Expr::Or(vec![sub("b"), sub("c")])])
    );
}

#[test]
fn groups_override_precedence() {
    assert_eq!(
        p("<a b>|c"),
        Expr::Or(vec![Expr::And(vec![sub("a"), sub("b")]), sub("c")])
    );
}

#[test]
fn not_applies_to_next_unary() {
    assert_eq!(
        p("!a b"),
        Expr::And(vec![Expr::Not(Box::new(sub("a"))), sub("b")])
    );
    assert_eq!(
        p("!<a|b>"),
        Expr::Not(Box::new(Expr::Or(vec![sub("a"), sub("b")])))
    );
    assert_eq!(p("!!a"), Expr::Not(Box::new(Expr::Not(Box::new(sub("a"))))));
}

#[test]
fn lenient_on_malformed_input() {
    assert_eq!(p("a |"), sub("a"));
    assert_eq!(p("| a"), sub("a"));
    assert_eq!(p("<a"), sub("a"));
    assert_eq!(p("a> b"), Expr::And(vec![sub("a"), sub("b")]));
    assert_eq!(p("a !"), sub("a"));
}

#[test]
fn modifiers() {
    assert!(text(&p("case:Foo")).case_sensitive);
    let t = p("ww:path:foo");
    assert!(text(&t).whole_word);
    assert_eq!(text(&t).target, Target::Path);
    let t = p("regex:^a.*b$");
    assert_eq!(text(&t).kind, PatternKind::Regex);
    assert_eq!(text(&t).pattern, "^a.*b$");
    assert!(text(&p("diacritics:é")).diacritics);
    assert_eq!(text(&p("startwith:ab")).anchor, Anchor::Start);
    assert_eq!(text(&p("endwith:ab")).anchor, Anchor::End);
    assert_eq!(text(&p("wfn:ab.txt")).anchor, Anchor::Whole);
    assert_eq!(text(&p("whole:ab.txt")).anchor, Anchor::Whole);
    assert!(text(&p("prefix:rep")).word_prefix);
    assert!(text(&p("suffix:ort")).word_suffix);
    assert!(text(&p("ignorepunc:q1report")).ignore_punct);
    assert!(text(&p("ignorews:myfile")).ignore_ws);
    assert!(
        text(&p("nocase:case:x")).case_sensitive,
        "the last modifier wins"
    );
    assert!(!text(&p("case:nocase:x")).case_sensitive);
    assert_eq!(p("highlight:abc"), sub("abc"), "accepted no-op modifiers");
}

#[test]
fn global_modifiers_apply_to_the_rest() {
    let e = p("a ?case: B c");
    let Expr::And(parts) = e else { panic!("{e:?}") };
    assert!(!text(&parts[0]).case_sensitive);
    assert!(text(&parts[1]).case_sensitive && text(&parts[2]).case_sensitive);
}

#[test]
fn character_entities() {
    assert_eq!(p("a&sp:b"), sub("a b"));
    assert_eq!(p("x&vert:y&excl:&lt:&gt:&quot:"), sub("x|y!<>\""));
    assert_eq!(p("&#169:&#x5D0:"), sub("©א"));
    assert_eq!(p("R&D"), sub("R&D"), "a lone ampersand is literal");
}

#[test]
fn unknown_prefix_is_literal() {
    assert_eq!(p("foo:bar"), sub("foo:bar"));
}

#[test]
fn quoted_modifier_is_literal() {
    assert_eq!(p("\"case:x\""), sub("case:x"));
}

#[test]
fn kind_modifiers() {
    assert_eq!(p("folder:"), Expr::Filter(Filter::Kind(Kind::Folder)));
    assert_eq!(
        p("file:report"),
        Expr::And(vec![Expr::Filter(Filter::Kind(Kind::File)), sub("report")])
    );
}

#[test]
fn wildcards_and_paths_are_detected() {
    assert_eq!(text(&p("*.rs")).kind, PatternKind::Wildcard);
    assert_eq!(text(&p("src/main")).target, Target::Path);
    assert_eq!(text(&p("nopath:a/b")).target, Target::Name);
}

#[test]
fn options_set_defaults() {
    let opts = ParseOptions {
        match_case: true,
        match_path: true,
        match_diacritics: true,
        match_prefix: true,
        ignore_punctuation: true,
        ..Default::default()
    };
    let Expr::And(parts) = parse("x nocase:y", &opts) else {
        panic!()
    };
    let x = text(&parts[0]);
    assert!(x.case_sensitive && x.diacritics && x.word_prefix && x.ignore_punct);
    assert_eq!(x.target, Target::Path);
    assert!(!text(&parts[1]).case_sensitive);
}

#[test]
fn global_regex_mode_takes_whole_query() {
    let opts = ParseOptions {
        regex: true,
        ..Default::default()
    };
    let e = parse("a b|c", &opts);
    assert_eq!(text(&e).pattern, "a b|c");
    assert_eq!(text(&e).kind, PatternKind::Regex);
}

#[test]
fn ext_and_filelist() {
    assert_eq!(
        p("ext:JPG;.png"),
        Expr::Filter(Filter::Ext(vec!["jpg".into(), "png".into()]))
    );
    assert_eq!(
        p("filelist:A.txt;b.TXT"),
        Expr::Filter(Filter::FileList(vec!["a.txt".into(), "b.txt".into()]))
    );
}

#[test]
fn size_function() {
    let Filter::Size(b) = filter(&p("size:>1mb")).clone() else {
        panic!()
    };
    assert_eq!(b.lo, (1 << 20) + 1);
    let Filter::Size(b) = filter(&p("size:huge")).clone() else {
        panic!()
    };
    assert!(b.contains(100 << 20));
    let Filter::Size(b) = filter(&p("size:10mb-50mb")).clone() else {
        panic!()
    };
    assert!(b.contains(30 << 20) && !b.contains(60 << 20));
}

#[test]
fn value_operators() {
    // != and !value negate.
    assert!(matches!(p("size:!=0"), Expr::Not(_)));
    assert!(matches!(p("ext:!jpg"), Expr::Not(_)));
    // a;b is OR for scalar functions.
    let Expr::Or(parts) = p("size:tiny;huge") else {
        panic!()
    };
    assert_eq!(parts.len(), 2);
    // <a b> is AND, <a|b> is OR.
    let Expr::And(parts) = p("content:<abc 123>") else {
        panic!()
    };
    assert!(matches!(filter(&parts[0]), Filter::Content(c) if c.pattern == "abc"));
    assert!(matches!(filter(&parts[1]), Filter::Content(c) if c.pattern == "123"));
    assert!(matches!(p("name:<foo|bar>"), Expr::Or(_)));
}

#[test]
fn date_functions() {
    for (q, field) in [
        ("dm:today", DateField::Modified),
        ("dc:2020", DateField::Created),
        ("da:lastweek", DateField::Accessed),
        ("rc:today", DateField::RecentlyChanged),
        ("dr:yesterday", DateField::Run),
        ("datecreated:2021-07..2022-06", DateField::Created),
    ] {
        assert!(
            matches!(filter(&p(q)), Filter::Date(f, _) if *f == field),
            "{q}"
        );
    }
    let Filter::Date(_, b) = filter(&p("dm:today")).clone() else {
        panic!()
    };
    assert!(b.contains(1_700_000_000 * 1_000_000_000));
}

#[test]
fn invalid_function_value_falls_back_to_text() {
    assert_eq!(p("size:abc"), sub("size:abc"));
}

#[test]
fn empty_function_value_matches_all() {
    assert_eq!(p("size:"), Expr::All);
    assert_eq!(p("foo dm:"), sub("foo"));
}

#[test]
fn structure_functions() {
    assert_eq!(
        p("folder:empty:"),
        Expr::And(vec![
            Expr::Filter(Filter::Kind(Kind::Folder)),
            Expr::Filter(Filter::Empty)
        ])
    );
    assert_eq!(
        p("parent:/home/me/"),
        Expr::Filter(Filter::Parent("/home/me".into()))
    );
    assert_eq!(p("under:/"), Expr::Filter(Filter::Under("/".into())));
    assert_eq!(p("root:"), Expr::Filter(Filter::Root));
    assert!(matches!(filter(&p("depth:<=2")), Filter::Depth(_)));
    assert!(matches!(filter(&p("len:10")), Filter::Len(_)));
    assert!(matches!(
        filter(&p("child-count:>3")),
        Filter::ChildCount(ChildKind::All, _)
    ));
    assert!(matches!(
        filter(&p("childfiles:0")),
        Filter::ChildCount(ChildKind::Files, _)
    ));
    assert!(matches!(
        filter(&p("child-folder-count:2")),
        Filter::ChildCount(ChildKind::Folders, _)
    ));
    assert!(matches!(filter(&p("child:readme")), Filter::Child(t) if t.pattern == "readme"));
    assert!(matches!(filter(&p("run-count:>1")), Filter::RunCount(_)));
    assert_eq!(p("count:10"), Expr::Filter(Filter::Count(10)));
}

#[test]
fn name_parts() {
    assert_eq!(text(&p("name:foo")).target, Target::Name);
    assert_eq!(text(&p("stem:foo")).target, Target::Stem);
    assert_eq!(text(&p("location:/home")).target, Target::Location);
    assert!(matches!(filter(&p("type:image")), Filter::Type(t) if t.pattern == "image"));
}

#[test]
fn attributes_and_dupes() {
    assert_eq!(
        p("attrib:dh"),
        Expr::Filter(Filter::Attrib(vec![
            Attribute::Directory,
            Attribute::Hidden
        ]))
    );
    assert_eq!(
        p("attrib:s"),
        Expr::Filter(Filter::Attrib(vec![Attribute::Unsupported]))
    );
    assert_eq!(p("dupe:"), Expr::Filter(Filter::Dupe(vec![DupeKey::Name])));
    assert_eq!(
        p("dupe:size;name"),
        Expr::Filter(Filter::Dupe(vec![DupeKey::Size, DupeKey::Name]))
    );
    assert_eq!(
        p("sizedupe:"),
        Expr::Filter(Filter::Dupe(vec![DupeKey::Size]))
    );
}

#[test]
fn content_and_properties() {
    assert!(
        matches!(filter(&p("content:hello")), Filter::Content(c) if c.pattern == "hello" && !c.regex)
    );
    assert!(matches!(filter(&p("regex:content:^foo")), Filter::Content(c) if c.regex));
    assert!(matches!(
        filter(&p("width:>1000")),
        Filter::Property(Property::Width, PropertyMatch::Number(_))
    ));
    assert!(matches!(
        filter(&p("length:>1hour")),
        Filter::Property(Property::Length, PropertyMatch::Number(b)) if b.lo > 3_600_000
    ));
    assert!(matches!(
        filter(&p("length:short")),
        Filter::Property(Property::Length, _)
    ));
    assert!(matches!(
        filter(&p("artist:beatles")),
        Filter::Property(Property::Artist, PropertyMatch::Text(_))
    ));
}

#[test]
fn macros_prefix_text() {
    let Expr::And(parts) = p("pic:cat") else {
        panic!()
    };
    assert!(matches!(filter(&parts[0]), Filter::Ext(exts) if exts.contains(&"jpg".into())));
    assert_eq!(parts[1], sub("cat"));
    assert_eq!(p("image:"), p("pic:"));
    assert!(matches!(p("exe:"), Expr::Or(_)));
}

#[test]
fn hebrew_terms() {
    assert_eq!(p("שלום"), sub("שלום"));
    assert_eq!(
        p("ext:טקסט"),
        Expr::Filter(Filter::Ext(vec!["טקסט".into()]))
    );
    let Expr::And(parts) = p("מסמך !טיוטה") else {
        panic!()
    };
    assert_eq!(parts[0], sub("מסמך"));
}
