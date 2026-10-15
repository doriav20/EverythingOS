//! Tokenizer for the Everything-style query language.
//!
//! Whitespace separates terms, `|` is OR, a leading `!` is NOT and `<` `>`
//! group (parentheses are common in filenames, so they stay literal).
//! Double quotes protect spaces and operators and may appear anywhere in a
//! term. Inside function terms such as `size:>1mb` a comparison operator
//! directly after the colon is part of the value, not a group bracket.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Token {
    Term(RawTerm),
    Or,
    Not,
    Open,
    Close,
}

/// A term with quotes removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawTerm {
    pub text: String,
    /// Length in bytes of the leading part of `text` that was not quoted.
    /// Modifiers such as `case:` are only recognized there.
    pub unquoted_prefix: usize,
}

pub(crate) fn tokenize(input: &str) -> Vec<Token> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '|' => {
                tokens.push(Token::Or);
                i += 1;
            }
            '<' => {
                tokens.push(Token::Open);
                i += 1;
            }
            '>' => {
                tokens.push(Token::Close);
                i += 1;
            }
            '!' => {
                tokens.push(Token::Not);
                i += 1;
            }
            _ => {
                let (term, next) = read_term(&chars, i);
                tokens.push(Token::Term(term));
                i = next;
            }
        }
    }
    tokens
}

fn read_term(chars: &[char], start: usize) -> (RawTerm, usize) {
    let mut text = String::new();
    let mut unquoted_prefix = None;
    let mut in_quotes = false;
    let mut saw_colon = false;
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if c == '"' {
                in_quotes = false;
            } else {
                text.push(c);
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_quotes = true;
                unquoted_prefix.get_or_insert(text.len());
            }
            c if c.is_whitespace() || c == '|' => break,
            '<' if text.ends_with(':') && starts_value_group(chars, i) => {
                // `content:<abc 123>`, `size:<1mb|>1gb>`: a grouped function
                // value, kept verbatim (spaces included) for the parser.
                let end = group_end(chars, i).expect("checked by starts_value_group");
                text.extend(&chars[i..=end]);
                i = end + 1;
                continue;
            }
            '<' | '>' => {
                // `size:>10mb`, `dm:<=2024`: operator right after the colon
                // (or another operator) belongs to the function value.
                let prev = text.chars().last();
                let is_operator = saw_colon && matches!(prev, Some(':' | '<' | '>' | '='));
                if !is_operator {
                    break;
                }
                text.push(c);
            }
            ':' => {
                saw_colon = true;
                text.push(c);
            }
            _ => text.push(c),
        }
        i += 1;
    }
    let unquoted_prefix = unquoted_prefix.unwrap_or(text.len());
    (
        RawTerm {
            text,
            unquoted_prefix,
        },
        i,
    )
}

/// Index of the `>` closing the group opened at `open`, if any.
fn group_end(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut in_quotes = false;
    for (i, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '"' => in_quotes = !in_quotes,
            '<' if !in_quotes => depth += 1,
            '>' if !in_quotes => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Does the `<` at `i` (right after a function colon) open a value group
/// rather than a comparison like `<5mb`, `<=2024` or `<today`?
fn starts_value_group(chars: &[char], i: usize) -> bool {
    let Some(&next) = chars.get(i + 1) else {
        return false;
    };
    let comparison = matches!(next, '=' | '<' | '>' | '!');
    if comparison {
        return false;
    }
    // A group needs its closing bracket, and must contain more than one
    // value (space or `|`) to be distinguishable from `dm:<today`.
    group_end(chars, i).is_some_and(|end| chars[i + 1..end].iter().any(|c| *c == ' ' || *c == '|'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_function_values() {
        assert_eq!(
            tokenize("content:<abc 123> x"),
            vec![term("content:<abc 123>"), term("x")]
        );
        assert_eq!(tokenize("size:<tiny|huge>"), vec![term("size:<tiny|huge>")]);
        assert_eq!(tokenize("dm:<today"), vec![term("dm:<today")]);
        assert_eq!(
            tokenize("<dm:<today>"),
            vec![Token::Open, term("dm:<today"), Token::Close]
        );
    }

    fn term(text: &str) -> Token {
        Token::Term(RawTerm {
            text: text.into(),
            unquoted_prefix: text.len(),
        })
    }

    #[test]
    fn splits_terms_and_operators() {
        assert_eq!(
            tokenize("foo  bar|baz !qux"),
            vec![
                term("foo"),
                term("bar"),
                Token::Or,
                term("baz"),
                Token::Not,
                term("qux")
            ]
        );
    }

    #[test]
    fn groups() {
        assert_eq!(
            tokenize("<a|b> c"),
            vec![
                Token::Open,
                term("a"),
                Token::Or,
                term("b"),
                Token::Close,
                term("c")
            ]
        );
    }

    #[test]
    fn quotes_protect_spaces_and_operators() {
        assert_eq!(
            tokenize(r#""my file|x" path:"/a b/<c>""#),
            vec![
                Token::Term(RawTerm {
                    text: "my file|x".into(),
                    unquoted_prefix: 0
                }),
                Token::Term(RawTerm {
                    text: "path:/a b/<c>".into(),
                    unquoted_prefix: 5
                }),
            ]
        );
    }

    #[test]
    fn comparison_operators_in_functions() {
        assert_eq!(tokenize("size:>10mb"), vec![term("size:>10mb")]);
        assert_eq!(tokenize("dm:<=2024"), vec![term("dm:<=2024")]);
        assert_eq!(
            tokenize("<size:>1mb>"),
            vec![Token::Open, term("size:>1mb"), Token::Close]
        );
    }

    #[test]
    fn bang_inside_term_is_literal() {
        assert_eq!(tokenize("hey!you"), vec![term("hey!you")]);
    }

    #[test]
    fn parentheses_are_literal() {
        assert_eq!(tokenize("photo(1).jpg"), vec![term("photo(1).jpg")]);
    }
}
