//! The Everything-style query language.

mod ast;
mod lexer;
mod parser;
mod values;

pub use ast::{Bounds, Expr, Filter, Kind, PatternKind, Target, TextMatch};
pub use parser::{parse, ParseOptions};
