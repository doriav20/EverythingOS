//! `*` / `?` wildcard matching over raw bytes.
//!
//! The whole text must match the pattern. In names `*` matches any sequence
//! and `?` exactly one character (one UTF-8 sequence, or one byte of
//! invalid UTF-8). The common shapes `*.ext`, `prefix*` and `*infix*`
//! compile to fast paths.
//!
//! In paths, as in Everything 1.5, `*` and `?` do not match the separator
//! while `**` matches anything, so `/home/*/notes.txt` only looks one level
//! deep and `/home/**/notes.txt` at any depth.

use memchr::memmem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Byte(u8),
    AnyChar,
    Star,
}

#[derive(Debug, Clone)]
enum Plan {
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    Contains(Box<memmem::Finder<'static>>),
    General(Vec<Tok>),
    /// Separator-aware path pattern.
    Path(regex::bytes::Regex),
}

/// A compiled wildcard pattern.
#[derive(Debug, Clone)]
pub struct Wildcard {
    plan: Plan,
}

impl Wildcard {
    pub fn new(pattern: &[u8]) -> Self {
        let toks = tokenize(pattern);
        Wildcard {
            plan: plan(pattern, toks),
        }
    }

    pub fn is_match(&self, text: &[u8]) -> bool {
        match &self.plan {
            Plan::Path(re) => re.is_match(text),
            Plan::Exact(lit) => text == lit.as_slice(),
            Plan::Prefix(lit) => text.starts_with(lit),
            Plan::Suffix(lit) => text.ends_with(lit),
            Plan::Contains(finder) => finder.find(text).is_some(),
            Plan::General(toks) => general_match(toks, text),
        }
    }

    /// A pattern for full paths: `*` and `?` stop at `/`, `**` does not.
    pub fn new_path(pattern: &[u8]) -> Self {
        let mut re = String::from("(?s)^");
        let mut i = 0;
        while i < pattern.len() {
            match pattern[i] {
                b'*' if pattern.get(i + 1) == Some(&b'*') => {
                    re.push_str(".*");
                    while pattern.get(i + 1) == Some(&b'*') {
                        i += 1;
                    }
                }
                b'*' => re.push_str("[^/]*"),
                b'?' => re.push_str("[^/]"),
                _ => {
                    let end = pattern[i..]
                        .iter()
                        .position(|&b| b == b'*' || b == b'?')
                        .map_or(pattern.len(), |p| i + p);
                    re.push_str(&regex::escape(&String::from_utf8_lossy(&pattern[i..end])));
                    i = end;
                    continue;
                }
            }
            i += 1;
        }
        re.push('$');
        let re = regex::bytes::Regex::new(&re).expect("escaped wildcard pattern is a valid regex");
        Wildcard {
            plan: Plan::Path(re),
        }
    }

    /// True if the pattern matches any text (`*`, `**`, ...).
    pub fn matches_everything(&self) -> bool {
        matches!(&self.plan, Plan::Prefix(lit) | Plan::Suffix(lit) if lit.is_empty())
    }
}

fn tokenize(pattern: &[u8]) -> Vec<Tok> {
    let mut toks = Vec::with_capacity(pattern.len());
    for &b in pattern {
        let tok = match b {
            b'*' => Tok::Star,
            b'?' => Tok::AnyChar,
            b => Tok::Byte(b),
        };
        if tok == Tok::Star && toks.last() == Some(&Tok::Star) {
            continue; // `**` is the same as `*`
        }
        toks.push(tok);
    }
    toks
}

fn plan(pattern: &[u8], toks: Vec<Tok>) -> Plan {
    if toks.contains(&Tok::AnyChar) {
        return Plan::General(toks);
    }
    let stars: Vec<usize> = toks
        .iter()
        .enumerate()
        .filter(|(_, t)| **t == Tok::Star)
        .map(|(i, _)| i)
        .collect();
    let literal = |range: &[Tok]| -> Vec<u8> {
        range
            .iter()
            .map(|t| {
                if let Tok::Byte(b) = t {
                    *b
                } else {
                    unreachable!()
                }
            })
            .collect()
    };
    let n = toks.len();
    match stars.as_slice() {
        [] => Plan::Exact(pattern.to_vec()),
        [0] => Plan::Suffix(literal(&toks[1..])),
        [s] if *s == n - 1 => Plan::Prefix(literal(&toks[..n - 1])),
        [0, e] if *e == n - 1 => Plan::Contains(Box::new(
            memmem::Finder::new(&literal(&toks[1..n - 1])).into_owned(),
        )),
        _ => Plan::General(toks),
    }
}

fn char_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1, // continuation or invalid byte: treat as one character
    }
}

fn general_match(pat: &[Tok], text: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while t < text.len() {
        match pat.get(p) {
            Some(Tok::Star) => {
                backtrack = Some((p, t));
                p += 1;
                continue;
            }
            Some(Tok::Byte(b)) if text[t] == *b => {
                p += 1;
                t += 1;
                continue;
            }
            Some(Tok::AnyChar) => {
                let len = char_len(text[t]);
                if t + len <= text.len() {
                    p += 1;
                    t += len;
                    continue;
                }
            }
            _ => {}
        }
        // Mismatch: let the last star absorb one more character.
        match backtrack {
            Some((sp, st)) => {
                let st = st + char_len(text[st]).min(text.len() - st);
                backtrack = Some((sp, st));
                p = sp + 1;
                t = st;
            }
            None => return false,
        }
    }
    pat[p..].iter().all(|t| *t == Tok::Star)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, t: &str) -> bool {
        Wildcard::new(p.as_bytes()).is_match(t.as_bytes())
    }

    #[test]
    fn fast_paths() {
        assert!(m("*.rs", "main.rs"));
        assert!(!m("*.rs", "main.rsx"));
        assert!(m("read*", "readme.md"));
        assert!(!m("read*", "unread"));
        assert!(m("*port*", "report.pdf"));
        assert!(m("exact", "exact"));
        assert!(!m("exact", "exactly"));
        assert!(m("*", ""));
        assert!(m("**", "anything"));
        assert!(Wildcard::new(b"**").matches_everything());
    }

    #[test]
    fn question_mark_matches_one_char() {
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(!m("a?c", "abbc"));
        assert!(m("?", "é"), "one UTF-8 character");
        assert!(!m("??", "é"));
        assert!(m("caf?", "café"));
    }

    #[test]
    fn general_patterns() {
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(m("a*b*c", "abc"));
        assert!(!m("a*b*c", "acb"));
        assert!(m("*a*?", "xxaYz"));
        assert!(m("IMG_*.j?g", "IMG_0001.jpg"));
        assert!(!m("IMG_*.j?g", "IMG_0001.png"));
        assert!(m("*/src/*.rs", "/home/me/src/lib.rs"));
        assert!(m("*a*a*a*a*b", "aaaaaaaaaaaaaaaaaab"));
        assert!(!m("*a*a*a*a*b", "aaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn path_stars_respect_separators() {
        let p =
            |pat: &str, text: &str| Wildcard::new_path(pat.as_bytes()).is_match(text.as_bytes());
        assert!(p("/home/*/notes.txt", "/home/me/notes.txt"));
        assert!(!p("/home/*/notes.txt", "/home/me/deep/notes.txt"));
        assert!(p("/home/**/notes.txt", "/home/me/deep/notes.txt"));
        assert!(p("**/src/*.rs", "/x/y/src/main.rs"));
        assert!(!p("**/src/*.rs", "/x/src/sub/main.rs"));
        assert!(p("/d/?.txt", "/d/ש.txt"), "? is one Hebrew letter");
        assert!(!p("/d?x", "/d/x"), "? never matches the separator");
        assert!(
            p("/a/(1)+[2].txt", "/a/(1)+[2].txt"),
            "regex characters are literal"
        );
    }

    #[test]
    fn invalid_utf8_text() {
        assert!(Wildcard::new(b"a?b").is_match(b"a\xffb"));
        assert!(Wildcard::new(b"*b").is_match(b"\xff\xfeb"));
    }
}
