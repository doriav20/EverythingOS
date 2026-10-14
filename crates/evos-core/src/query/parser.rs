//! Recursive-descent parser for the query language.
//!
//! Grammar (as in Everything, OR binds tighter than the implicit AND):
//!
//! ```text
//! query   := and EOF
//! and     := or+
//! or      := unary ('|' unary)*
//! unary   := '!' unary | primary
//! primary := '<' and '>' | term
//! ```
//!
//! The parser is lenient: unbalanced brackets and dangling operators are
//! ignored rather than reported, because the query is re-parsed on every
//! keystroke.

use chrono::{Local, TimeZone};

use super::ast::{Expr, Filter, Kind, PatternKind, Target, TextMatch};
use super::lexer::{tokenize, RawTerm, Token};
use super::values;

/// Search-wide defaults, typically toggled in the user interface.
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    /// Treat the whole query as one regular expression.
    pub regex: bool,
    /// Reference time for relative dates, in Unix seconds (default: now).
    pub now: Option<i64>,
}

/// Parses a query string.
pub fn parse(input: &str, opts: &ParseOptions) -> Expr {
    if opts.regex {
        if input.trim().is_empty() {
            return Expr::All;
        }
        return Expr::Filter(Filter::Text(TextMatch {
            pattern: input.to_owned(),
            kind: PatternKind::Regex,
            target: if opts.match_path {
                Target::Path
            } else {
                Target::Name
            },
            case_sensitive: opts.match_case,
            whole_word: false,
        }));
    }
    let tokens = tokenize(input);
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        opts,
    };
    let mut parts = Vec::new();
    while parser.peek().is_some() {
        if parser.peek() == Some(&Token::Close) {
            parser.pos += 1; // stray closing bracket
            continue;
        }
        parts.push(parser.parse_and());
    }
    Expr::and(parts)
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    opts: &'a ParseOptions,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    fn parse_and(&mut self) -> Expr {
        let mut parts = Vec::new();
        while let Some(tok) = self.peek() {
            match tok {
                Token::Close => break,
                Token::Or => self.pos += 1, // dangling OR
                _ => parts.push(self.parse_or()),
            }
        }
        Expr::and(parts)
    }

    fn parse_or(&mut self) -> Expr {
        let mut parts = vec![self.parse_unary()];
        while self.peek() == Some(&Token::Or) {
            self.pos += 1;
            match self.peek() {
                None | Some(Token::Close) | Some(Token::Or) => break,
                _ => parts.push(self.parse_unary()),
            }
        }
        Expr::or(parts)
    }

    fn parse_unary(&mut self) -> Expr {
        match self.peek() {
            Some(Token::Not) => {
                self.pos += 1;
                match self.peek() {
                    None | Some(Token::Close) | Some(Token::Or) => Expr::All,
                    _ => match self.parse_unary() {
                        Expr::All => Expr::All,
                        e => Expr::Not(Box::new(e)),
                    },
                }
            }
            Some(Token::Open) => {
                self.pos += 1;
                let inner = self.parse_and();
                if self.peek() == Some(&Token::Close) {
                    self.pos += 1;
                }
                inner
            }
            Some(Token::Term(raw)) => {
                self.pos += 1;
                parse_term(raw, self.opts)
            }
            Some(Token::Or) | Some(Token::Close) | None => Expr::All,
        }
    }
}

/// Modifier state applied to one term.
#[derive(Debug, Clone, Copy)]
struct Modifiers {
    case_sensitive: bool,
    whole_word: bool,
    path: Option<bool>,
    regex: bool,
    kind: Option<Kind>,
}

fn parse_term(raw: &RawTerm, opts: &ParseOptions) -> Expr {
    let mut mods = Modifiers {
        case_sensitive: opts.match_case,
        whole_word: opts.whole_word,
        path: opts.match_path.then_some(true),
        regex: false,
        kind: None,
    };
    let mut rest = raw.text.as_str();
    let mut unquoted = raw.unquoted_prefix;
    let mut parts = Vec::new();
    while let Some(colon) = rest[..unquoted].find(':') {
        let name = rest[..colon].to_ascii_lowercase();
        let value = &rest[colon + 1..];
        if apply_modifier(&name, &mut mods) {
            // Modifiers prefix the rest of the term.
        } else if let Some(filter) = macro_filter(&name) {
            parts.push(filter);
        } else if is_function(&name) {
            if let Some(kind) = mods.kind {
                parts.push(Expr::Filter(Filter::Kind(kind)));
            }
            parts.push(function(&name, value, opts).unwrap_or_else(|| {
                // Not a valid value: search for the literal text instead.
                Expr::Filter(Filter::Text(text_match(rest, &mods)))
            }));
            return Expr::and(parts);
        } else {
            break;
        }
        rest = value;
        unquoted -= colon + 1;
    }

    if let Some(kind) = mods.kind {
        parts.push(Expr::Filter(Filter::Kind(kind)));
    }
    if !rest.is_empty() {
        parts.push(Expr::Filter(Filter::Text(text_match(rest, &mods))));
    }
    Expr::and(parts)
}

fn is_function(name: &str) -> bool {
    matches!(
        name,
        "ext"
            | "size"
            | "dm"
            | "datemodified"
            | "depth"
            | "len"
            | "parent"
            | "infolder"
            | "under"
            | "empty"
    )
}

/// Parses a function term. `None` means the value is invalid. An empty
/// value yields [`Expr::All`] so a half-typed function does not hide results.
fn function(name: &str, value: &str, opts: &ParseOptions) -> Option<Expr> {
    let f = |filter| Some(Expr::Filter(filter));
    if name == "empty" {
        return f(Filter::Empty);
    }
    if value.is_empty() {
        return Some(Expr::All);
    }
    match name {
        "ext" => {
            let exts = value
                .split(';')
                .map(|e| crate::fold::fold_str(e.trim_start_matches('.')))
                .filter(|e| !e.is_empty())
                .collect();
            f(Filter::Ext(exts))
        }
        "size" => {
            let bounds = values::named_size(value)
                .or_else(|| values::parse_bounds(value, 0, u64::MAX, values::size_interval))?;
            f(Filter::Size(bounds))
        }
        "dm" | "datemodified" => {
            let now = opts
                .now
                .and_then(|secs| Local.timestamp_opt(secs, 0).single())
                .unwrap_or_else(Local::now);
            let bounds =
                values::parse_bounds(value, i64::MIN, i64::MAX, |v| values::date_interval(v, now))?;
            f(Filter::Modified(bounds))
        }
        "depth" => f(Filter::Depth(values::parse_bounds(
            value,
            0,
            u64::MAX,
            values::int_interval,
        )?)),
        "len" => f(Filter::Len(values::parse_bounds(
            value,
            0,
            u64::MAX,
            values::int_interval,
        )?)),
        "parent" | "infolder" => f(Filter::Parent(normalize_dir(value))),
        "under" => f(Filter::Under(normalize_dir(value))),
        _ => None,
    }
}

fn normalize_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Type macros, which filter like modifiers and may prefix more text.
fn macro_filter(name: &str) -> Option<Expr> {
    let exts: &[&str] = match name {
        "audio" => &[
            "aac", "ac3", "aif", "aifc", "aiff", "amr", "ape", "au", "flac", "m4a", "m4b", "mid",
            "midi", "mka", "mp2", "mp3", "mpa", "oga", "ogg", "opus", "ra", "wav", "wma", "wv",
        ],
        "video" => &[
            "3g2", "3gp", "avi", "flv", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "mts",
            "ogm", "ogv", "rm", "ts", "vob", "webm", "wmv",
        ],
        "pic" | "picture" | "image" => &[
            "avif", "bmp", "gif", "heic", "heif", "ico", "jpeg", "jpg", "jxl", "png", "psd", "raw",
            "svg", "tif", "tiff", "webp", "xcf",
        ],
        "doc" | "document" => &[
            "c", "chm", "cpp", "csv", "doc", "docx", "epub", "h", "htm", "html", "json", "md",
            "odp", "ods", "odt", "pdf", "ppt", "pptx", "rtf", "tex", "txt", "xls", "xlsx", "xml",
        ],
        "zip" | "compressed" => &[
            "7z", "ace", "arj", "bz2", "cab", "deb", "gz", "iso", "lz", "lz4", "lzma", "rar",
            "rpm", "tar", "tbz2", "tgz", "txz", "xz", "zip", "zst",
        ],
        "exe" | "exec" | "executable" => {
            return Some(Expr::or(vec![
                Expr::Filter(Filter::Executable),
                Expr::Filter(Filter::Ext(
                    ["appimage", "run", "sh"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                )),
            ]));
        }
        _ => return None,
    };
    Some(Expr::Filter(Filter::Ext(
        exts.iter().map(|s| s.to_string()).collect(),
    )))
}

fn apply_modifier(name: &str, mods: &mut Modifiers) -> bool {
    match name.to_ascii_lowercase().as_str() {
        "case" => mods.case_sensitive = true,
        "nocase" => mods.case_sensitive = false,
        "ww" | "wholeword" | "wholewords" => mods.whole_word = true,
        "noww" | "nowholeword" | "nowholewords" => mods.whole_word = false,
        "path" => mods.path = Some(true),
        "nopath" => mods.path = Some(false),
        "regex" => mods.regex = true,
        "noregex" => mods.regex = false,
        "file" | "files" => mods.kind = Some(Kind::File),
        "folder" | "folders" => mods.kind = Some(Kind::Folder),
        _ => return false,
    }
    true
}

fn text_match(pattern: &str, mods: &Modifiers) -> TextMatch {
    let kind = if mods.regex {
        PatternKind::Regex
    } else if pattern.contains(['*', '?']) {
        PatternKind::Wildcard
    } else {
        PatternKind::Substring
    };
    let target = match mods.path {
        Some(true) => Target::Path,
        Some(false) => Target::Name,
        None if pattern.contains('/') => Target::Path,
        None => Target::Name,
    };
    TextMatch {
        pattern: pattern.to_owned(),
        kind,
        target,
        case_sensitive: mods.case_sensitive,
        whole_word: mods.whole_word,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(q: &str) -> Expr {
        parse(q, &ParseOptions::default())
    }

    fn sub(s: &str) -> Expr {
        Expr::Filter(Filter::Text(TextMatch {
            pattern: s.into(),
            kind: PatternKind::Substring,
            target: Target::Name,
            case_sensitive: false,
            whole_word: false,
        }))
    }

    fn text(e: &Expr) -> &TextMatch {
        match e {
            Expr::Filter(Filter::Text(t)) => t,
            other => panic!("expected text filter, got {other:?}"),
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
        let t = p("case:Foo");
        assert!(text(&t).case_sensitive);
        let t = p("ww:path:foo");
        assert!(text(&t).whole_word);
        assert_eq!(text(&t).target, Target::Path);
        let t = p("regex:^a.*b$");
        assert_eq!(text(&t).kind, PatternKind::Regex);
        assert_eq!(text(&t).pattern, "^a.*b$");
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
            ..Default::default()
        };
        let e = parse("x nocase:y", &opts);
        let Expr::And(parts) = e else { panic!() };
        assert!(text(&parts[0]).case_sensitive);
        assert_eq!(text(&parts[0]).target, Target::Path);
        assert!(!text(&parts[1]).case_sensitive);
    }

    fn filter(e: &Expr) -> &Filter {
        match e {
            Expr::Filter(f) => f,
            other => panic!("expected filter, got {other:?}"),
        }
    }

    #[test]
    fn ext_function() {
        assert_eq!(
            p("ext:JPG;.png;"),
            Expr::Filter(Filter::Ext(vec!["jpg".into(), "png".into()]))
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
    }

    #[test]
    fn date_function_uses_reference_time() {
        let opts = ParseOptions {
            now: Some(1_700_000_000),
            ..Default::default()
        };
        let Expr::Filter(Filter::Modified(b)) = parse("dm:today", &opts) else {
            panic!()
        };
        assert!(b.contains(1_700_000_000 * 1_000_000_000));
        assert!(!b.contains((1_700_000_000 - 2 * 86_400) * 1_000_000_000));
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
    fn functions_combine_with_modifiers() {
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
        assert!(matches!(filter(&p("depth:<=2")), Filter::Depth(_)));
        assert!(matches!(filter(&p("len:10")), Filter::Len(_)));
    }

    #[test]
    fn macros_prefix_text() {
        let e = p("pic:cat");
        let Expr::And(parts) = e else { panic!() };
        assert!(matches!(filter(&parts[0]), Filter::Ext(exts) if exts.contains(&"jpg".into())));
        assert_eq!(parts[1], sub("cat"));
        assert!(matches!(p("exe:"), Expr::Or(_)));
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
}
