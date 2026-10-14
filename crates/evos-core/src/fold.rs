//! Case folding for raw filename bytes.
//!
//! Linux filenames are arbitrary byte strings, usually but not necessarily
//! UTF-8. Folding lowercases ASCII byte-wise, lowercases valid UTF-8 with
//! Unicode rules and copies invalid bytes through unchanged, so every name
//! has a well-defined folded form.

/// Appends the case-folded form of `name` to `out`.
pub fn fold_into(name: &[u8], out: &mut Vec<u8>) {
    if name.is_ascii() {
        out.extend(name.iter().map(u8::to_ascii_lowercase));
        return;
    }
    for chunk in name.utf8_chunks() {
        for c in chunk.valid().chars() {
            if c.is_ascii() {
                out.push(c.to_ascii_lowercase() as u8);
            } else {
                let mut buf = [0u8; 4];
                for lower in c.to_lowercase() {
                    out.extend_from_slice(lower.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        out.extend_from_slice(chunk.invalid());
    }
}

/// Returns the case-folded form of `name`.
pub fn fold(name: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len());
    fold_into(name, &mut out);
    out
}

/// Case-folds a query string.
pub fn fold_str(s: &str) -> String {
    // Folding valid UTF-8 always yields valid UTF-8.
    String::from_utf8(fold(s.as_bytes())).expect("folding preserves UTF-8 validity")
}

/// Returns true if `s` contains any character that folding would change.
pub fn has_upper(s: &str) -> bool {
    s.chars().any(|c| c.is_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_lowercased() {
        assert_eq!(fold(b"Hello World.TXT"), b"hello world.txt");
    }

    #[test]
    fn unicode_is_lowercased() {
        assert_eq!(
            fold("ÄÖÜ Straße ΣΑΣ".as_bytes()),
            "äöü straße σασ".as_bytes()
        );
    }

    #[test]
    fn invalid_utf8_passes_through() {
        assert_eq!(fold(b"A\xffB\xc3"), b"a\xffb\xc3");
    }

    #[test]
    fn fold_str_matches_fold() {
        assert_eq!(fold_str("MiXeD É"), "mixed é");
        assert!(has_upper("abC"));
        assert!(!has_upper("abc é"));
    }
}
