//! Text normalization for matching raw filename bytes.
//!
//! Linux filenames are arbitrary byte strings, usually but not necessarily
//! UTF-8. Normalization works on valid UTF-8 sequences and copies invalid
//! bytes through unchanged, so every name has a well-defined form.
//!
//! By default searches, like Everything's, ignore both case and diacritics:
//! "resume" finds "Résumé" and "שלום" finds "שָׁלוֹם" (Hebrew vowel points
//! are combining marks). The index stores names in that folded form; the
//! `case:` and `diacritics:` modifiers and the punctuation/white-space
//! options use [`normalize_into`] on the fly.

use unicode_normalization::char::{decompose_canonical, is_combining_mark};

/// How text is normalized before comparison.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Normalize {
    /// Keep letter case.
    pub case: bool,
    /// Keep diacritical marks.
    pub diacritics: bool,
    /// Drop punctuation.
    pub ignore_punct: bool,
    /// Drop white space.
    pub ignore_ws: bool,
}

impl Normalize {
    /// The form names are stored in: case and diacritics folded.
    pub const FOLDED: Normalize = Normalize {
        case: false,
        diacritics: false,
        ignore_punct: false,
        ignore_ws: false,
    };

    pub fn is_folded(&self) -> bool {
        *self == Self::FOLDED
    }
}

/// Letters without a canonical decomposition that still carry a
/// diacritic-like stroke, mapped to their base letter.
fn strip_extra(c: char) -> Option<char> {
    Some(match c {
        'ł' => 'l',
        'Ł' => 'L',
        'ø' => 'o',
        'Ø' => 'O',
        'đ' => 'd',
        'Đ' => 'D',
        'ħ' => 'h',
        'Ħ' => 'H',
        'ı' => 'i',
        _ => return None,
    })
}

/// Is `c` dropped by `ignorepunc:`?
pub fn is_punct(c: char) -> bool {
    !c.is_alphanumeric() && !c.is_whitespace() && !is_combining_mark(c)
}

fn push_char(out: &mut Vec<u8>, c: char) {
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

fn push_normalized(c: char, opts: &Normalize, keep: &[char], out: &mut Vec<u8>) {
    if keep.contains(&c) {
        return push_char(out, c);
    }
    if (opts.ignore_ws && c.is_whitespace()) || (opts.ignore_punct && is_punct(c)) {
        return;
    }
    let mut emit = |c: char| {
        if opts.case {
            push_char(out, c);
        } else if c.is_ascii() {
            out.push(c.to_ascii_lowercase() as u8);
        } else {
            for lower in c.to_lowercase() {
                push_char(out, lower);
            }
        }
    };
    if opts.diacritics || c.is_ascii() {
        return emit(c);
    }
    if let Some(base) = strip_extra(c) {
        return emit(base);
    }
    decompose_canonical(c, |d| {
        if !is_combining_mark(d) {
            emit(d);
        }
    });
}

/// Appends the normalized form of `text` to `out`. Characters in `keep`
/// (e.g. wildcard symbols in a pattern) are never dropped or changed.
pub fn normalize_into(text: &[u8], opts: &Normalize, keep: &[char], out: &mut Vec<u8>) {
    let plain = !opts.ignore_punct && !opts.ignore_ws;
    if text.is_ascii() && plain {
        if opts.case {
            out.extend_from_slice(text);
        } else {
            out.extend(text.iter().map(u8::to_ascii_lowercase));
        }
        return;
    }
    for chunk in text.utf8_chunks() {
        for c in chunk.valid().chars() {
            push_normalized(c, opts, keep, out);
        }
        out.extend_from_slice(chunk.invalid());
    }
}

/// Returns the normalized form of `text`.
pub fn normalize(text: &[u8], opts: &Normalize) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    normalize_into(text, opts, &[], &mut out);
    out
}

/// Appends the folded (case- and diacritic-insensitive) form of `name`.
pub fn fold_into(name: &[u8], out: &mut Vec<u8>) {
    normalize_into(name, &Normalize::FOLDED, &[], out);
}

/// Returns the folded form of `name`.
pub fn fold(name: &[u8]) -> Vec<u8> {
    normalize(name, &Normalize::FOLDED)
}

/// Folds a query string.
pub fn fold_str(s: &str) -> String {
    // Normalizing valid UTF-8 always yields valid UTF-8.
    String::from_utf8(fold(s.as_bytes())).expect("folding preserves UTF-8 validity")
}

/// Returns true if `s` contains any character that case folding would change.
pub fn has_upper(s: &str) -> bool {
    s.chars().any(|c| c.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(text: &str, opts: Normalize) -> String {
        String::from_utf8(normalize(text.as_bytes(), &opts)).unwrap()
    }

    #[test]
    fn ascii_is_lowercased() {
        assert_eq!(fold(b"Hello World.TXT"), b"hello world.txt");
    }

    #[test]
    fn unicode_is_lowercased_and_diacritics_dropped() {
        assert_eq!(fold_str("ÄÖÜ Straße Résumé ΣΑΣ"), "aou straße resume σασ");
        assert_eq!(fold_str("Łódź Øresund"), "lodz oresund");
    }

    #[test]
    fn hebrew_vowel_points_are_diacritics() {
        // "shalom" with niqqud folds to the plain consonants.
        assert_eq!(fold_str("שָׁלוֹם"), "שלום");
        assert_eq!(fold_str("קובץ.txt"), "קובץ.txt");
        // Arabic harakat, too.
        assert_eq!(fold_str("مُحَمَّد"), "محمد");
    }

    #[test]
    fn other_scripts_pass_through() {
        assert_eq!(fold_str("Москва"), "москва");
        assert_eq!(fold_str("東京タワー"), "東京タワー");
    }

    #[test]
    fn modes() {
        let case = Normalize {
            case: true,
            ..Default::default()
        };
        assert_eq!(n("Résumé", case), "Resume");
        let diacritics = Normalize {
            diacritics: true,
            ..Default::default()
        };
        assert_eq!(n("Résumé", diacritics), "résumé");
        let both = Normalize {
            case: true,
            diacritics: true,
            ..Default::default()
        };
        assert_eq!(n("Résumé", both), "Résumé");
        let punct = Normalize {
            ignore_punct: true,
            ..Default::default()
        };
        assert_eq!(n("Q1-Report_(final).pdf", punct), "q1reportfinalpdf");
        let ws = Normalize {
            ignore_ws: true,
            ..Default::default()
        };
        assert_eq!(n("My  Holiday Photos", ws), "myholidayphotos");
    }

    #[test]
    fn kept_characters_survive() {
        let punct = Normalize {
            ignore_punct: true,
            ..Default::default()
        };
        let mut out = Vec::new();
        normalize_into(b"*q1-report?", &punct, &['*', '?'], &mut out);
        assert_eq!(out, b"*q1report?");
    }

    #[test]
    fn invalid_utf8_passes_through() {
        assert_eq!(fold(b"A\xffB\xc3"), b"a\xffb\xc3");
    }

    #[test]
    fn has_upper_detects_case() {
        assert!(has_upper("abC"));
        assert!(!has_upper("abc é שלום"));
    }
}
