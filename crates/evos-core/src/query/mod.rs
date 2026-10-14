//! The Everything-style query language.

mod ast;
mod lexer;
mod parser;

pub use ast::{Expr, Filter, Kind, PatternKind, Target, TextMatch};
pub use parser::{parse, ParseOptions};
