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

use super::ast::{Expr, Filter, Kind, PatternKind, Target, TextMatch};
use super::lexer::{tokenize, RawTerm, Token};

/// Search-wide defaults, typically toggled in the user interface.
#[derive(Debug, Clone, Default)]
pub struct ParseOptions {
    pub match_case: bool,
    pub whole_word: bool,
    pub match_path: bool,
    /// Treat the whole query as one regular expression.
    pub regex: bool,
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
    while let Some(colon) = rest[..unquoted].find(':') {
        if !apply_modifier(&rest[..colon], &mut mods) {
            break;
        }
        rest = &rest[colon + 1..];
        unquoted -= colon + 1;
    }

    let mut parts = Vec::new();
    if let Some(kind) = mods.kind {
        parts.push(Expr::Filter(Filter::Kind(kind)));
    }
    if !rest.is_empty() {
        parts.push(Expr::Filter(Filter::Text(text_match(rest, &mods))));
    }
    Expr::and(parts)
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
