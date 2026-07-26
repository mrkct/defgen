//! Code generation.
//!
//! A backend turns a checked [`Model`] into source text. Everything a backend
//! needs has already been resolved by [`crate::check`] — bit offsets, layouts,
//! enum values, byte order — so a backend never re-derives layout, never looks
//! a name up, and never reports an error: [`Backend::generate`] is total.
//!
//! Adding a language means writing one module implementing [`Backend`] and
//! adding it to [`all`]. Nothing else in the compiler, including the CLI's
//! `--backend` flag, needs to change: the flag's accepted values, its help text
//! and its "unknown backend" message all read from the registry below.

pub mod c;
pub mod kotlin;
pub mod python;
pub mod swift;

use crate::model::Model;

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------
//
// Every target language re-cases the schema's identifiers (§13), and they all
// start from the same two conversions, so they live here rather than being
// re-derived — and allowed to drift — once per backend.

/// Reduces a file stem to something usable as an identifier: a file name, an
/// include guard, a module name.
pub fn sanitize_stem(stem: &str) -> String {
    let mut out: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// `HearingMode` to `hearing_mode`.
///
/// A boundary goes before an uppercase letter that follows a lowercase letter
/// or a digit, and before the last uppercase of a run that starts a new word —
/// so `LegacySerial` becomes `legacy_serial` and `UUIDTag` becomes `uuid_tag`
/// rather than `u_u_i_d_tag`.
pub fn snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            if !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            continue;
        }
        if c.is_ascii_uppercase() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + 1).copied();
            let boundary = match prev {
                None | Some('_') => false,
                Some(p) if p.is_ascii_lowercase() || p.is_ascii_digit() => true,
                Some(p) if p.is_ascii_uppercase() => next.is_some_and(|n| n.is_ascii_lowercase()),
                _ => false,
            };
            if boundary && !out.is_empty() && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `HearingMode` to `HEARING_MODE`, the spelling both C's `#define`s and
/// Python's enum members and constants want.
pub fn screaming(name: &str) -> String {
    snake(name).to_ascii_uppercase()
}

/// `active_profile` (or any other casing) to `activeProfile` — the property
/// and function naming convention Kotlin, Swift and Java all share (§13).
/// Goes through [`snake`] first so the word boundaries are found the same way
/// `screaming` finds them, then re-joins with an uppercased first letter on
/// every word but the first.
pub fn camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, word) in snake(name).split('_').filter(|w| !w.is_empty()).enumerate() {
        if i == 0 {
            out.push_str(word);
            continue;
        }
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// One file a backend produced.
///
/// `name` is a bare file name, never a path — where the files land is the
/// caller's decision, not the backend's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedFile {
    pub name: String,
    pub contents: String,
}

/// Everything a backend produced for one schema. A single-file backend (C)
/// returns one entry; a backend that emits a file per type would return many.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Generated {
    pub files: Vec<GeneratedFile>,
}

impl Generated {
    /// The sole file, for a single-file backend. Panics otherwise, so only call
    /// it when you know which backend produced it.
    pub fn single(&self) -> &GeneratedFile {
        assert_eq!(self.files.len(), 1, "expected a single generated file");
        &self.files[0]
    }
}

/// Per-run settings every backend understands. Anything language-specific
/// belongs in that backend, not here.
#[derive(Debug, Clone)]
pub struct Options {
    /// Base name for generated artifacts, normally the schema's file stem.
    /// A backend derives file names, include guards or module names from it.
    pub stem: String,
    /// The schema's path, for the "do not edit" banner. `None` when the source
    /// did not come from a file.
    pub source: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options { stem: "schema".to_string(), source: None }
    }
}

impl Options {
    /// Options for a schema read from `path`.
    pub fn for_path(path: &std::path::Path) -> Options {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        Options {
            stem: stem.filter(|s| !s.is_empty()).unwrap_or_else(|| "schema".to_string()),
            source: Some(path.display().to_string()),
        }
    }
}

/// A code generator for one target language.
pub trait Backend {
    /// The name `--backend` accepts. Lowercase, no spaces.
    fn name(&self) -> &'static str;

    /// One line, for `--help` and the "unknown backend" message.
    fn description(&self) -> &'static str;

    /// Generates source for `model`. Infallible: the model is already valid.
    fn generate(&self, model: &Model, opts: &Options) -> Generated;
}

/// Every backend, in the order they should be listed.
pub fn all() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(c::CBackend),
        Box::new(python::PythonBackend),
        Box::new(kotlin::KotlinBackend),
        Box::new(swift::SwiftBackend),
    ]
}

/// The names `--backend` accepts.
pub fn names() -> Vec<&'static str> {
    all().iter().map(|b| b.name()).collect()
}

/// Looks a backend up by the name `--backend` was given.
pub fn find(name: &str) -> Option<Box<dyn Backend>> {
    all().into_iter().find(|b| b.name() == name)
}
