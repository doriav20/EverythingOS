//! The Everything-style query language.

mod ast;
mod lexer;
mod parser;
mod values;

pub use ast::{
    Anchor, Attribute, Bounds, ChildKind, ContentMatch, DateField, DupeKey, Expr, Filter, Kind,
    PatternKind, Property, PropertyMatch, Target, TextMatch,
};
pub use parser::{decode_entities, parse, ParseOptions};
pub use values::UNKNOWN_DATE;

/// The literal text a result is expected to contain, for highlighting:
/// plain and wildcard name or path terms outside negations. Wildcard
/// patterns contribute their literal pieces; path terms also contribute
/// their last component, which is what the Name column shows.
pub fn highlight_terms(expr: &Expr) -> Vec<String> {
    fn walk(expr: &Expr, out: &mut Vec<String>) {
        match expr {
            Expr::And(parts) | Expr::Or(parts) => parts.iter().for_each(|p| walk(p, out)),
            Expr::Filter(Filter::Text(t)) if t.target != Target::Location => {
                let pieces: Vec<&str> = match t.kind {
                    PatternKind::Substring => vec![t.pattern.as_str()],
                    PatternKind::Wildcard => t.pattern.split(['*', '?']).collect(),
                    PatternKind::Regex => Vec::new(),
                };
                for piece in pieces {
                    let last = piece.rsplit('/').next().unwrap_or(piece);
                    for term in [piece, last] {
                        if !term.is_empty() && !out.iter().any(|t| t == term) {
                            out.push(term.to_owned());
                        }
                    }
                }
            }
            Expr::All | Expr::Not(_) | Expr::Filter(_) => {}
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_terms_skip_negations_and_functions() {
        let terms = |q: &str| highlight_terms(&parse(q, &ParseOptions::default()));
        assert_eq!(terms("foo !bar ext:txt"), vec!["foo"]);
        assert_eq!(
            terms("*.tar.gz report|draft"),
            vec![".tar.gz", "report", "draft"]
        );
        assert_eq!(terms("home/docs"), vec!["home/docs", "docs"]);
        assert_eq!(terms("regex:^a.*b$"), Vec::<String>::new());
        assert_eq!(terms("שלום"), vec!["שלום"]);
    }
}
