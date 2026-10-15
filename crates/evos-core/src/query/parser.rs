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
//! term    := ['?'] modifier* (function ':' value | macro: term | text)
//! value   := ['!' | '!='] scalar | scalar (';' scalar)* | '<' value-list '>'
//! ```
//!
//! The parser is lenient: unbalanced brackets, dangling operators and
//! invalid function values never fail, because the query is re-parsed on
//! every keystroke.

use chrono::{DateTime, Local, TimeZone};

use super::ast::{
    Anchor, Attribute, ChildKind, ContentMatch, DateField, DupeKey, Expr, Filter, Kind,
    PatternKind, Property, PropertyMatch, Target, TextMatch,
};
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
    pub match_diacritics: bool,
    /// Terms must match the start of a word.
    pub match_prefix: bool,
    /// Terms must match the end of a word.
    pub match_suffix: bool,
    pub ignore_punctuation: bool,
    pub ignore_white_space: bool,
    /// Reference time for relative dates, in Unix seconds (default: now).
    pub now: Option<i64>,
}

/// Parses a query string.
pub fn parse(input: &str, opts: &ParseOptions) -> Expr {
    let defaults = Modifiers::from_options(opts);
    if opts.regex {
        if input.trim().is_empty() {
            return Expr::All;
        }
        let mods = Modifiers {
            regex: true,
            ..defaults
        };
        return Expr::Filter(Filter::Text(text_match(input, &mods)));
    }
    let tokens = tokenize(input);
    let now = opts
        .now
        .and_then(|secs| Local.timestamp_opt(secs, 0).single())
        .unwrap_or_else(Local::now);
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        defaults,
        now,
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
    /// Modifier state for new terms; `?case:` and friends change it.
    defaults: Modifiers,
    now: DateTime<Local>,
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
                self.parse_term(raw)
            }
            Some(Token::Or) | Some(Token::Close) | None => Expr::All,
        }
    }

    fn parse_term(&mut self, raw: &RawTerm) -> Expr {
        // `?case:` enables a modifier for the rest of the search.
        if let Some(global) = raw
            .text
            .strip_prefix('?')
            .filter(|_| raw.unquoted_prefix > 0)
        {
            let mut mods = self.defaults;
            let mut rest = global;
            while let Some(colon) = rest.find(':') {
                if !apply_modifier(&rest[..colon].to_ascii_lowercase(), &mut mods) {
                    break;
                }
                rest = &rest[colon + 1..];
            }
            if rest.is_empty() {
                self.defaults = Modifiers { kind: None, ..mods };
                return Expr::All;
            }
        }
        parse_term(raw, self.defaults, self.now)
    }
}

/// Modifier state applied to one term.
#[derive(Debug, Clone, Copy)]
struct Modifiers {
    case_sensitive: bool,
    diacritics: bool,
    whole_word: bool,
    prefix: bool,
    suffix: bool,
    path: Option<bool>,
    regex: bool,
    anchor: Anchor,
    ignore_punct: bool,
    ignore_ws: bool,
    kind: Option<Kind>,
}

impl Modifiers {
    fn from_options(opts: &ParseOptions) -> Self {
        Modifiers {
            case_sensitive: opts.match_case,
            diacritics: opts.match_diacritics,
            whole_word: opts.whole_word,
            prefix: opts.match_prefix,
            suffix: opts.match_suffix,
            path: opts.match_path.then_some(true),
            regex: false,
            anchor: Anchor::Anywhere,
            ignore_punct: opts.ignore_punctuation,
            ignore_ws: opts.ignore_white_space,
            kind: None,
        }
    }
}

/// Decodes Everything's character entities (`&sp:`, `&vert:`, `&#169:`).
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let decoded = rest.find(':').and_then(|colon| {
            let name = &rest[1..colon];
            let c = match name {
                "sp" => Some(' '),
                "vert" => Some('|'),
                "excl" => Some('!'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "amp" => Some('&'),
                "apos" => Some('\''),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                    .and_then(char::from_u32),
            };
            c.map(|c| (c, colon + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn parse_term(raw: &RawTerm, defaults: Modifiers, now: DateTime<Local>) -> Expr {
    let mut mods = defaults;
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
            parts.push(function_value(&name, value, &mods, now).unwrap_or_else(|| {
                // Not a valid value: search for the literal text instead.
                Expr::Filter(Filter::Text(text_match(&decode_entities(rest), &mods)))
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
        parts.push(Expr::Filter(Filter::Text(text_match(
            &decode_entities(rest),
            &mods,
        ))));
    }
    Expr::and(parts)
}

fn canonical_function(name: &str) -> Option<&'static str> {
    Some(match name {
        "ext" | "extension" => "ext",
        "filelist" => "filelist",
        "size" => "size",
        "dm" | "datemodified" => "dm",
        "dc" | "datecreated" => "dc",
        "da" | "dateaccessed" => "da",
        "rc" | "recentchange" | "daterecentlychanged" | "drc" => "rc",
        "dr" | "daterun" => "dr",
        "depth" | "parents" => "depth",
        "len" | "length-name" => "len",
        "parent" | "infolder" | "nosubfolders" => "parent",
        "under" | "insubfolder" => "under",
        "root" => "root",
        "empty" => "empty",
        "child" => "child",
        "child-count" | "childcount" => "child-count",
        "child-file-count" | "childfilecount" | "childfiles" => "child-file-count",
        "child-folder-count" | "childfoldercount" | "childfolders" => "child-folder-count",
        "attrib" | "attributes" | "attr" => "attrib",
        "type" => "type",
        "name" => "name",
        "stem" | "namepart" => "stem",
        "location" => "location",
        "count" => "count",
        "dupe" | "dupes" => "dupe",
        "sizedupe" => "sizedupe",
        "namepartdupe" | "stemdupe" => "stemdupe",
        "extdupe" => "extdupe",
        "dmdupe" => "dmdupe",
        "dcdupe" => "dcdupe",
        "run-count" | "runcount" => "run-count",
        "content" | "utf8content" | "ansicontent" | "utf16content" => "content",
        "width" => "width",
        "height" => "height",
        "bitdepth" | "bit-depth" => "bitdepth",
        "length" | "duration" => "length",
        "track" => "track",
        "album" => "album",
        "artist" => "artist",
        "title" => "title",
        "genre" => "genre",
        "comment" => "comment",
        _ => return None,
    })
}

fn is_function(name: &str) -> bool {
    canonical_function(name).is_some()
}

/// Functions whose value is itself a `;`-separated list.
fn takes_list(name: &str) -> bool {
    matches!(
        canonical_function(name),
        Some("ext" | "filelist" | "dupe" | "attrib")
    )
}

/// Splits a group's inner text on a separator, honoring quotes and nested
/// brackets.
fn split_top(text: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut start) = (0i32, false, 0);
    for (i, c) in text.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '<' if !quoted => depth += 1,
            '>' if !quoted => depth -= 1,
            c if c == sep && !quoted && depth == 0 => {
                parts.push(&text[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect()
}

/// Parses a function value with its operators: `<a b>` (AND), `<a|b>` and
/// `a;b` (OR), and `!=a` / `!a` (NOT). `None` means the value is invalid.
fn function_value(name: &str, value: &str, mods: &Modifiers, now: DateTime<Local>) -> Option<Expr> {
    let value = value.trim();
    if let Some(inner) = value.strip_prefix('<').and_then(|v| v.strip_suffix('>')) {
        let alternatives = split_top(inner, '|');
        if alternatives.len() > 1 {
            let parts = alternatives
                .iter()
                .map(|v| function_value(name, v, mods, now))
                .collect::<Option<Vec<_>>>()?;
            return Some(Expr::or(parts));
        }
        let parts = split_top(inner, ' ')
            .iter()
            .map(|v| function_value(name, v.trim_matches('"'), mods, now))
            .collect::<Option<Vec<_>>>()?;
        return Some(Expr::and(parts));
    }
    if let Some(negated) = value.strip_prefix("!=").or_else(|| value.strip_prefix('!')) {
        if !negated.is_empty() {
            return Some(Expr::Not(Box::new(function_value(
                name, negated, mods, now,
            )?)));
        }
    }
    if !takes_list(name) && value.contains(';') {
        let parts = split_top(value, ';')
            .iter()
            .map(|v| function_value(name, v, mods, now))
            .collect::<Option<Vec<_>>>()?;
        return Some(Expr::or(parts));
    }
    function(name, &decode_entities(value), mods, now)
}

fn attributes(value: &str) -> Option<Vec<Attribute>> {
    value
        .chars()
        .filter(|c| !c.is_whitespace())
        .map(|c| {
            Some(match c.to_ascii_uppercase() {
                'A' => Attribute::Archive,
                'D' => Attribute::Directory,
                'H' => Attribute::Hidden,
                'R' => Attribute::ReadOnly,
                'L' => Attribute::ReparsePoint,
                'N' => Attribute::Normal,
                'C' | 'E' | 'I' | 'M' | 'O' | 'P' | 'S' | 'T' | 'U' | 'V' | 'X' => {
                    Attribute::Unsupported
                }
                _ => return None,
            })
        })
        .collect()
}

fn dupe_keys(value: &str, default: DupeKey) -> Option<Vec<DupeKey>> {
    if value.is_empty() {
        return Some(vec![default]);
    }
    value
        .split(';')
        .filter(|k| !k.is_empty())
        .map(|k| {
            Some(match k.trim().to_ascii_lowercase().as_str() {
                "name" | "filename" => DupeKey::Name,
                "size" => DupeKey::Size,
                "stem" | "namepart" => DupeKey::Stem,
                "ext" | "extension" => DupeKey::Extension,
                "dm" | "datemodified" => DupeKey::Modified,
                "dc" | "datecreated" => DupeKey::Created,
                _ => return None,
            })
        })
        .collect()
}

/// Parses a single function value. An empty value yields [`Expr::All`] so a
/// half-typed function does not hide results.
fn function(name: &str, value: &str, mods: &Modifiers, now: DateTime<Local>) -> Option<Expr> {
    let f = |filter| Some(Expr::Filter(filter));
    let name = canonical_function(name)?;
    let numbers = |v: &str| values::parse_bounds(v, 0, u64::MAX, true, values::int_interval);
    let date = |field| {
        let bounds = values::parse_bounds(value, i64::MIN, i64::MAX, false, |v| {
            values::date_interval(v, now)
        })?;
        Some(Expr::Filter(Filter::Date(field, bounds)))
    };
    let text = |target| {
        let mut m = *mods;
        m.path = Some(false);
        let mut t = text_match(value, &m);
        t.target = target;
        t
    };
    // Functions that need no value.
    match name {
        "empty" => return f(Filter::Empty),
        "root" => return f(Filter::Root),
        "dupe" => return f(Filter::Dupe(dupe_keys(value, DupeKey::Name)?)),
        "sizedupe" => return f(Filter::Dupe(vec![DupeKey::Size])),
        "stemdupe" => return f(Filter::Dupe(vec![DupeKey::Stem])),
        "extdupe" => return f(Filter::Dupe(vec![DupeKey::Extension])),
        "dmdupe" => return f(Filter::Dupe(vec![DupeKey::Modified])),
        "dcdupe" => return f(Filter::Dupe(vec![DupeKey::Created])),
        _ => {}
    }
    if value.is_empty() {
        return Some(Expr::All);
    }
    match name {
        "ext" => {
            let exts = value
                .split(';')
                .map(|e| crate::fold::fold_str(e.trim().trim_start_matches('.')))
                .collect::<Vec<_>>();
            f(Filter::Ext(exts))
        }
        "filelist" => {
            let names = value
                .split(';')
                .map(|n| crate::fold::fold_str(n.trim()))
                .filter(|n| !n.is_empty())
                .collect();
            f(Filter::FileList(names))
        }
        "size" => {
            let bounds = values::named_size(value).or_else(|| {
                values::parse_bounds(value, 0, u64::MAX, true, values::size_interval)
            })?;
            f(Filter::Size(bounds))
        }
        "dm" => date(DateField::Modified),
        "dc" => date(DateField::Created),
        "da" => date(DateField::Accessed),
        "rc" => date(DateField::RecentlyChanged),
        "dr" => date(DateField::Run),
        "depth" => f(Filter::Depth(numbers(value)?)),
        "len" => f(Filter::Len(numbers(value)?)),
        "run-count" => f(Filter::RunCount(numbers(value)?)),
        "child-count" => f(Filter::ChildCount(ChildKind::All, numbers(value)?)),
        "child-file-count" => f(Filter::ChildCount(ChildKind::Files, numbers(value)?)),
        "child-folder-count" => f(Filter::ChildCount(ChildKind::Folders, numbers(value)?)),
        "count" => f(Filter::Count(value.trim().parse().ok()?)),
        "parent" => f(Filter::Parent(normalize_dir(value))),
        "under" => f(Filter::Under(normalize_dir(value))),
        "attrib" => f(Filter::Attrib(attributes(value)?)),
        "child" => f(Filter::Child(text(Target::Name))),
        "type" => f(Filter::Type(text(Target::Name))),
        "name" => f(Filter::Text(text(Target::Name))),
        "stem" => f(Filter::Text(text(Target::Stem))),
        "location" => f(Filter::Text(text(Target::Location))),
        "content" => f(Filter::Content(ContentMatch {
            pattern: value.to_owned(),
            regex: mods.regex,
            case_sensitive: mods.case_sensitive,
            whole_word: mods.whole_word,
        })),
        "width" | "height" | "bitdepth" | "track" => {
            let prop = match name {
                "width" => Property::Width,
                "height" => Property::Height,
                "bitdepth" => Property::BitDepth,
                _ => Property::Track,
            };
            f(Filter::Property(
                prop,
                PropertyMatch::Number(numbers(value)?),
            ))
        }
        "length" => {
            let bounds = values::named_duration(value).or_else(|| {
                values::parse_bounds(value, 0, u64::MAX, true, values::duration_interval)
            })?;
            f(Filter::Property(
                Property::Length,
                PropertyMatch::Number(bounds),
            ))
        }
        "album" | "artist" | "title" | "genre" | "comment" => {
            let prop = match name {
                "album" => Property::Album,
                "artist" => Property::Artist,
                "title" => Property::Title,
                "genre" => Property::Genre,
                _ => Property::Comment,
            };
            f(Filter::Property(
                prop,
                PropertyMatch::Text(text(Target::Name)),
            ))
        }
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
        "audio" => crate::types::AUDIO,
        "video" => crate::types::VIDEO,
        "pic" | "picture" | "image" => crate::types::IMAGE,
        "doc" | "document" => crate::types::DOCUMENT,
        "zip" | "compressed" => crate::types::COMPRESSED,
        "exe" | "exec" | "executable" => {
            return Some(Expr::or(vec![
                Expr::Filter(Filter::Executable),
                Expr::Filter(Filter::Ext(
                    crate::types::EXECUTABLE
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

/// Applies a modifier; returns false if `name` is not one.
fn apply_modifier(name: &str, mods: &mut Modifiers) -> bool {
    match name {
        "case" => mods.case_sensitive = true,
        "nocase" => mods.case_sensitive = false,
        "diacritics" => mods.diacritics = true,
        "nodiacritics" => mods.diacritics = false,
        "ww" | "wholeword" | "wholewords" => mods.whole_word = true,
        "noww" | "nowholeword" | "nowholewords" => mods.whole_word = false,
        "prefix" => mods.prefix = true,
        "noprefix" => mods.prefix = false,
        "suffix" => mods.suffix = true,
        "nosuffix" => mods.suffix = false,
        "path" => mods.path = Some(true),
        "nopath" => mods.path = Some(false),
        "regex" => mods.regex = true,
        "noregex" => mods.regex = false,
        "startwith" => mods.anchor = Anchor::Start,
        "endwith" => mods.anchor = Anchor::End,
        "whole" | "wholefilename" | "wfn" | "exact" => mods.anchor = Anchor::Whole,
        "nostartwith" | "noendwith" | "nowhole" | "nowholefilename" | "nowfn" => {
            mods.anchor = Anchor::Anywhere
        }
        "ignorepunc" | "ignorepunctuation" => mods.ignore_punct = true,
        "noignorepunc" | "noignorepunctuation" => mods.ignore_punct = false,
        "ignorews" | "ignorewhitespace" => mods.ignore_ws = true,
        "noignorews" | "noignorewhitespace" => mods.ignore_ws = false,
        "file" | "files" => mods.kind = Some(Kind::File),
        "folder" | "folders" => mods.kind = Some(Kind::Folder),
        // Accepted for compatibility; they change nothing here.
        "ascii" | "utf8" | "highlight" | "nohighlight" | "wildcards" | "nowildcards"
        | "fromdisk" | "nofromdisk" | "tonumber" | "tostring" | "binary" | "hex" => {}
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
        diacritics: mods.diacritics,
        whole_word: mods.whole_word,
        word_prefix: mods.prefix,
        word_suffix: mods.suffix,
        anchor: mods.anchor,
        ignore_punct: mods.ignore_punct,
        ignore_ws: mods.ignore_ws,
    }
}

#[cfg(test)]
mod tests;
