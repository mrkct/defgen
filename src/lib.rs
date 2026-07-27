//! `defgen` — compiles a `.defs` BLE GATT schema into C, Java, Kotlin, Python
//! and Swift codecs. See `SPEC.md` for the language and `GRAMMAR.ebnf` for its
//! grammar.
//!
//! The front end is three stages:
//!
//! 1. [`lexer::lex`] — source text to tokens, with `///` doc comments kept.
//! 2. [`parser::parse`] — tokens to an [`ast::Schema`], reporting every syntax
//!    error it can as a [`diag::Diagnostic`].
//! 3. [`check::check`] — an [`ast::Schema`] to a [`model::Model`], enforcing
//!    every cross-node rule in SPEC.md §11 (exact-fit widths, name resolution,
//!    variable-length placement, duplicate ids) and resolving layouts.
//!
//! [`compile`] runs all three. Code generation is the fourth: a
//! [`backends::Backend`] consumes a [`model::Model`] and emits source text.
//! Backends are registered in [`backends::all`], which is what the CLI's
//! `--backend` flag reads its accepted values from.

pub mod ast;
pub mod backends;
pub mod check;
pub mod diag;
pub mod lexer;
pub mod model;
pub mod parser;
pub mod span;

pub use ast::Schema;
pub use backends::{Backend, Generated, GeneratedFile};
pub use check::{Checked, check};
pub use diag::Diagnostic;
pub use model::Model;
pub use parser::{Parsed, parse};

use diag::Severity;

/// Outcome of the whole front end. `model` is `Some` exactly when nothing
/// error-level was reported; warnings (§10's MTU diagnostic) still allow it.
#[derive(Debug)]
pub struct Compiled {
    pub model: Option<Model>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Compiled {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }
}

/// Parses and checks one schema. Checking only runs on a schema that parsed
/// cleanly — resolving names in a tree with holes in it would report errors
/// the author never made.
pub fn compile(src: &str) -> Compiled {
    let parsed = parse(src);
    let Some(schema) = parsed.schema else {
        return Compiled { model: None, diagnostics: parsed.diagnostics };
    };
    let checked = check(&schema);
    let mut diagnostics = parsed.diagnostics;
    diagnostics.extend(checked.diagnostics);
    Compiled { model: checked.model, diagnostics }
}
