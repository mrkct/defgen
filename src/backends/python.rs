//! The Python backend: one self-contained, fully type-hinted module per schema.
//!
//! # Shape of the output
//!
//! Everything lands in a single importable `.py` — types, constants, codecs and
//! GATT metadata — so a project consumes generated code by copying one file in.
//! Only the standard library is used — `dataclasses`, `enum` and `typing`, and
//! not even `math`, since a `scaled` type's rounding is a generated helper
//! rather than a library call (§4). Every function, parameter and attribute
//! carries an annotation, so the module reads correctly to a type checker with
//! no stub file alongside it. The floor is Python 3.10:
//! `slots=True` dataclasses and `X | Y` in a `TypeAlias` are both 3.10.
//!
//! # Naming
//!
//! | Schema | Python |
//! |---|---|
//! | `struct Status` | `@dataclass class Status` |
//! | its codec | `Status.encode()` / `Status.decode(data)` |
//! | its size | `Status.SIZE` |
//! | `enum HearingMode`'s `Stereo` | `HearingMode.STEREO` |
//! | field `active_profile` | `active_profile` |
//! | `alias OwnerName`'s codec | `encode_owner_name` / `decode_owner_name` |
//!
//! Names starting with `_` are internal: `_pack_fixed` exists because a nested
//! type is packed by its parent, not because a caller should reach for it.
//!
//! # Representation choices
//!
//! * A `uN`/`iN` value is an `int`. Python has no fixed-width integers, so §2's
//!   carrier width is invisible here — but the rules that come with it are not:
//!   every value is range-checked against its declared width on encode, and
//!   every `iN` is sign-extended from bit `N-1` on decode.
//! * A `struct` becomes a mutable `@dataclass(slots=True)` in which every field
//!   has a default, so `Status()` is a usable zero value.
//! * A plain `enum` becomes an `enum.IntEnum`. An *open* one (§5) also gets a
//!   frozen `<Name><Else>` dataclass carrying `raw`, and a `<Name>Value` union
//!   alias: an unrecognized wire value is a distinct variant, never coerced
//!   into a declared one. A *closed* one raises on an unmatched value, on
//!   encode as well as decode.
//! * A tagged union (§7) becomes a sealed class hierarchy — an abstract base
//!   holding the codec, one dataclass per variant, and, for an open union, an
//!   `<Name><Else>` variant carrying the unrecognized id together with the
//!   undecoded `raw` payload. Decoding dispatches through a private id table.
//! * A `scaled` type (§4) is a `float` alias plus `<name>_from_raw` /
//!   `<name>_to_raw`, which keeps the underlying integer reachable for callers
//!   that want to round-trip without floating-point rounding. `f32` and `f64`
//!   both map to `float`, Python's only binary float.
//! * A variable-length field (§6.3) is a native `str` or `list`, as §12 asks.
//!   Decode fails on malformed UTF-8 rather than substituting replacement
//!   characters — which `bytes.decode("utf-8")` already does, being strict by
//!   default.
//! * Failures are exceptions, one class per kind under a common `DefgenError`,
//!   since a `Result`-shaped return would be foreign here.
//!
//! # Bit and byte order
//!
//! A container's bits live in a single `int`, which makes reading a field a
//! shift and a mask. Byte order (§8) enters in one place only — `_Bits._shift`,
//! which mirrors a value's declared offset for a big-endian container, since
//! §6 has such a container filled from its most-significant end rather than its
//! least-significant one. Nothing below an entry point decides byte order for
//! itself: it is threaded down as an argument, as far as the variable-length
//! tail, whose elements are each packed as their own byte-multiple unit under
//! the same order.

use super::{Backend, Generated, GeneratedFile, Options, sanitize_stem, screaming, snake};
use crate::ast::{Docs, Endianness, FloatType, Property};
use crate::model::{
    Const, Enum, Field, FieldRole, Model, Scaled, Struct, TypeDef, TypeKind, Union, WireType, int_range,
};

pub struct PythonBackend;

impl Backend for PythonBackend {
    fn name(&self) -> &'static str {
        "python"
    }

    fn description(&self) -> &'static str {
        "a single self-contained, type-hinted module (Python 3.10+)"
    }

    fn generate(&self, model: &Model, opts: &Options) -> Generated {
        let stem = sanitize_stem(&opts.stem).to_ascii_lowercase();
        let file = GeneratedFile {
            name: format!("{stem}.py"),
            contents: Emitter::new(model, opts.source.as_deref()).run(),
        };
        Generated { files: vec![file] }
    }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// Every Python keyword. A schema field named `class` is perfectly legal — §1
/// does not reserve Python's vocabulary — but `self.class` is a syntax error,
/// so a colliding name gets a trailing `_`.
#[rustfmt::skip]
const PY_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Names the generated module uses itself, which a schema name has to stay off.
///
/// A class attribute shadows the module-level name for the rest of the class
/// body, so a field named `field` would break the `field(default_factory=...)`
/// call declaring the next one; a field named `encode` would replace the codec
/// method outright. Both are avoided the same way a keyword is.
///
/// The builtins are here for the same reason one step up: a schema type becomes
/// a module-level `class`, so a type named `int` or `bytes` would shadow the
/// builtin for the whole module — including `_Bits`, which is built out of
/// `int.from_bytes`. These are the names the generated runtime actually
/// references; adding to it is cheap, and missing one is not.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    "field", "dataclass", "enum", "self", "cls",
    "encode", "decode", "encoded_size",
    "SIZE", "BIT_WIDTH", "FIXED_SIZE", "MAX_SIZE", "TAG_BITS", "ID",
    // Builtins the runtime and the generated codecs call by name.
    "bool", "bytes", "bytearray", "classmethod", "dict", "enumerate", "float",
    "int", "isinstance", "len", "list", "range", "staticmethod", "str",
    "tuple", "type",
    "Exception", "NotImplementedError", "UnicodeDecodeError",
    "UnicodeEncodeError", "ValueError",
];

/// A schema name as a Python identifier, escaped where it would collide with
/// the language's or the module's own vocabulary.
///
/// Every local and parameter the emitter introduces is `_`-prefixed (`_bits`,
/// `_off`, `_i0`), so a schema name shaped like one is escaped too.
fn ident(name: &str) -> String {
    let internal = name.starts_with('_') && name.len() > 1;
    if PY_KEYWORDS.contains(&name) || RESERVED.contains(&name) || internal {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// A `///` line as docstring content: the two sequences that could close the
/// docstring early, or swallow the character after them, are escaped.
fn escape_doc(text: &str) -> String {
    text.replace('\\', "\\\\").replace("\"\"\"", "\\\"\\\"\\\"")
}

/// The lines of a `///` comment (§1, §12).
fn doc_lines(docs: &Docs) -> Vec<String> {
    docs.iter().map(|d| d.text.clone()).collect()
}

/// `a` or `an` for a bit count about to be read aloud in a docstring — "an
/// 8-bit wire value", not "a 8-bit" one. Widths run 1..=128 (§2), so the only
/// numbers that begin with a vowel sound are 8, 11, 18 and the eighties.
fn article(bits: u32) -> &'static str {
    match bits {
        8 | 11 | 18 | 80..=89 => "an",
        _ => "a",
    }
}

/// A `float` literal Python reads back as the same value. Rust's shortest
/// representation round-trips; its spellings of the non-finite values are just
/// not Python's.
fn float_lit(v: f64) -> String {
    if v.is_nan() {
        return "float(\"nan\")".to_string();
    }
    if v.is_infinite() {
        return format!("float(\"{}inf\")", if v < 0.0 { "-" } else { "" });
    }
    let s = format!("{v:?}");
    if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") }
}

/// `True`/`False`, spelled the way Python does.
fn py_bool(v: bool) -> &'static str {
    if v { "True" } else { "False" }
}

/// The `struct` format codes for `f`'s IEEE-754 pattern — itself, and the
/// same-width unsigned integer `_bits.put`/`.get` speaks — plus its bit
/// width. Little-endian throughout: the pattern round-trips through
/// `struct.pack`/`unpack` purely to reinterpret bits, so the byte order used
/// there is arbitrary as long as packing and unpacking agree, and `<` avoids
/// the padding a native/`@`/`=` mode could insert.
fn float_struct_fmts(f: FloatType) -> (&'static str, &'static str, u32) {
    match f {
        FloatType::F32 => ("f", "I", 32),
        FloatType::F64 => ("d", "Q", 64),
    }
}

/// The exception hierarchy every generated module carries: one class per way a
/// value can fail to match the schema, under a base a caller can catch whole.
const ERRORS: &[(&str, &str, &str)] = &[
    ("DefgenError", "Exception", "Base class for every encode or decode failure this module raises."),
    (
        "DefgenLengthError",
        "DefgenError",
        "The buffer's length matches no legal encoding of the type (§6.3, §10).",
    ),
    ("DefgenRangeError", "DefgenError", "A value does not fit the bits its field declares (§2, §4, §6.3)."),
    (
        "DefgenUnknownValueError",
        "DefgenError",
        "A closed enum or tagged union met an undeclared value (§5, §7).",
    ),
    ("DefgenPaddingError", "DefgenError", "A `padding: uN = 0` run was not zero on the wire (§6.2)."),
    ("DefgenUtf8Error", "DefgenError", "A `string` field's bytes are not well-formed UTF-8 (§6.3)."),
];

/// How a variable-length field is spelled — see [`Emitter::tail_kind`].
enum TailKind {
    /// A native `str`/`list` attribute of the containing dataclass.
    Inline,
    /// A named type that owns the tail, and the methods that handle it.
    Nested,
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Imports and runtime helpers the schema turned out to need. An unused import
/// is something a project's linter complains about, so none is unconditional.
#[derive(Default)]
struct Needs {
    /// An `enum.IntEnum`, or the GATT property flags.
    enums: bool,
    /// A `scaled` type, which needs the rounding helper (§4).
    round: bool,
    /// A `@dataclass`: a struct, a union variant, an open enum's fallback, or
    /// the GATT metadata records.
    dataclass: bool,
    /// A field whose default has to be built fresh for each instance.
    default_factory: bool,
    /// An `alias`, `scaled` or open enum — each declares a `TypeAlias`.
    type_alias: bool,
    /// A struct or union, which carry their sizes as `ClassVar`s.
    class_var: bool,
    /// An array of either kind, whose length check is generic in its element.
    sequences: bool,
    /// A `string`, which needs the UTF-8 helpers (§6.3).
    utf8: bool,
    /// A raw `f32`/`f64` field, which needs the `struct` module (§2).
    raw_float: bool,
}

struct Emitter<'m> {
    m: &'m Model,
    out: String,
    source: Option<&'m str>,
    needs: Needs,
}

impl<'m> Emitter<'m> {
    fn new(m: &'m Model, source: Option<&'m str>) -> Emitter<'m> {
        let mut e = Emitter { m, out: String::with_capacity(32 * 1024), source, needs: Needs::default() };
        e.scan();
        e
    }

    // -- output primitives --------------------------------------------------

    fn line(&mut self, ind: usize, text: &str) {
        for _ in 0..ind {
            self.out.push_str("    ");
        }
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn lines(&mut self, ind: usize, texts: &[&str]) {
        for text in texts {
            if text.is_empty() { self.blank() } else { self.line(ind, text) }
        }
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }

    /// Two blank lines, which is what PEP 8 puts between top-level definitions.
    fn gap(&mut self) {
        self.blank();
        self.blank();
    }

    fn banner(&mut self, title: &str) {
        let rule = "-".repeat(72usize.saturating_sub(title.len()));
        self.blank();
        self.line(0, &format!("# {title} {rule}"));
        self.blank();
    }

    /// A call, wrapped one argument per line when it would otherwise run long.
    /// `lead` is what precedes the open paren — `return cls`, for instance.
    fn call(&mut self, ind: usize, lead: &str, args: &[String]) {
        const WIDTH: usize = 98;
        let one_line = format!("{lead}({})", args.join(", "));
        if ind * 4 + one_line.len() <= WIDTH {
            self.line(ind, &one_line);
            return;
        }
        self.line(ind, &format!("{lead}("));
        for arg in args {
            self.line(ind + 1, &format!("{arg},"));
        }
        self.line(ind, ")");
    }

    /// A `raise` of one of the module's errors, whose message is nearly always
    /// too long to sit on the `raise` line.
    fn raise(&mut self, ind: usize, error: &str, message: &str) {
        let one_line = format!("raise {error}({message})");
        if ind * 4 + one_line.len() <= 98 {
            self.line(ind, &one_line);
            return;
        }
        self.line(ind, &format!("raise {error}("));
        self.line(ind + 1, message);
        self.line(ind, ")");
    }

    /// A docstring: one line where it fits, the conventional multi-line form
    /// otherwise. The closing `"""` sits on its own line there, so a doc ending
    /// in a quote cannot run into it.
    ///
    /// Returns whether anything was written, so a caller that would follow it
    /// with a blank line can skip that too and not open a class body on one.
    fn docstring(&mut self, ind: usize, lines: &[String]) -> bool {
        if lines.is_empty() {
            return false;
        }
        let text: Vec<String> = lines.iter().map(|l| escape_doc(l)).collect();
        if text.len() == 1 && !text[0].ends_with('"') && ind * 4 + text[0].len() + 6 <= 98 {
            self.line(ind, &format!("\"\"\"{}\"\"\"", text[0]));
            return true;
        }
        self.line(ind, &format!("\"\"\"{}", text[0]));
        for l in &text[1..] {
            if l.is_empty() {
                self.blank();
            } else {
                self.line(ind, l);
            }
        }
        self.line(ind, "\"\"\"");
        true
    }

    /// A docstring the backend wrote itself, as one line.
    fn note(&mut self, ind: usize, text: &str) {
        self.docstring(ind, &[text.to_string()]);
    }

    /// The schema's own doc comment, then — after a blank line — whatever the
    /// backend has to say about the representation it chose. Returns whether
    /// there was anything to say at all.
    fn docs_with(&mut self, ind: usize, docs: &Docs, notes: &[String]) -> bool {
        let mut lines = doc_lines(docs);
        if !notes.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.extend(notes.iter().cloned());
        }
        self.docstring(ind, &lines)
    }

    // -- pre-pass -----------------------------------------------------------

    /// Works out which imports and helpers the schema needs, before emitting
    /// the header that has to declare them.
    fn scan(&mut self) {
        self.needs.enums = !self.m.services.is_empty();
        self.needs.dataclass = !self.m.services.is_empty();
        let m = self.m;
        for def in &m.types {
            match &def.kind {
                TypeKind::Alias(a) => {
                    self.needs.type_alias = true;
                    self.scan_type(&a.target);
                }
                TypeKind::Scaled(_) => {
                    self.needs.type_alias = true;
                    self.needs.round = true;
                }
                TypeKind::Enum(e) => {
                    self.needs.enums = true;
                    if e.is_open() {
                        self.needs.dataclass = true;
                        self.needs.type_alias = true;
                    }
                }
                TypeKind::Union(u) => {
                    self.needs.dataclass = true;
                    self.needs.class_var = true;
                    for f in u.variants.iter().flat_map(|v| &v.fields) {
                        self.scan_field(f);
                    }
                }
                TypeKind::Struct(s) => {
                    self.needs.dataclass = true;
                    self.needs.class_var = true;
                    for f in &s.fields {
                        self.scan_field(f);
                    }
                }
            }
        }
    }

    fn scan_field(&mut self, f: &Field) {
        // A `padding` run is a gap, never a value (§6.2), so it needs nothing.
        if matches!(f.role, FieldRole::Padding { .. }) {
            return;
        }
        self.needs.default_factory |= self.needs_factory(&f.ty);
        self.scan_type(&f.ty);
    }

    fn scan_type(&mut self, ty: &WireType) {
        match ty {
            WireType::UInt(_) | WireType::Int(_) | WireType::Bool | WireType::Named(_) => {}
            WireType::Float(_) => self.needs.raw_float = true,
            WireType::Str { .. } => self.needs.utf8 = true,
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => {
                self.needs.sequences = true;
                self.scan_type(elem);
            }
        }
    }

    // -- type mapping -------------------------------------------------------

    /// Follows `alias` indirection down to the type that actually says how a
    /// value is laid out. An alias is a compile-time name (§3), with no
    /// representation of its own to dispatch on.
    fn resolve(&self, ty: &WireType) -> WireType {
        match ty {
            WireType::Named(id) => match &self.m.get(*id).kind {
                TypeKind::Alias(a) => self.resolve(&a.target),
                _ => ty.clone(),
            },
            _ => ty.clone(),
        }
    }

    /// The annotation a value of `ty` carries. Aliases are deliberately *not*
    /// resolved here: the domain name the author declared is the point of one.
    fn py_type(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(_) | WireType::Int(_) => "int".to_string(),
            WireType::Bool => "bool".to_string(),
            WireType::Float(_) => "float".to_string(),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                match &def.kind {
                    TypeKind::Enum(e) if e.is_open() => format!("{}Value", ident(&def.name)),
                    _ => ident(&def.name),
                }
            }
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => {
                format!("list[{}]", self.py_type(elem))
            }
            WireType::Str { .. } => "str".to_string(),
        }
    }

    /// An expression building a fresh zero value of `ty`, used as a field's
    /// default so that every generated dataclass is constructible with no
    /// arguments at all.
    fn fresh(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(_) | WireType::Int(_) => "0".to_string(),
            WireType::Bool => "False".to_string(),
            WireType::Float(_) => "0.0".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
            WireType::VarArray { .. } => "[]".to_string(),
            WireType::Array { elem, count } => format!("[{} for _ in range({count})]", self.fresh(elem)),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => self.fresh(&a.target),
                    TypeKind::Scaled(_) => "0.0".to_string(),
                    // An enum with no variants at all is a compile error (§11),
                    // so the fallback only stands in for an `else`-only one.
                    TypeKind::Enum(e) => match (e.variants.first(), &e.else_arm) {
                        (Some(v), _) => format!("{name}.{}", screaming(&v.name)),
                        (None, Some(arm)) => format!("{name}{}()", ident(&arm.name)),
                        (None, None) => "0".to_string(),
                    },
                    TypeKind::Union(u) => match (u.variants.first(), &u.else_arm) {
                        (Some(v), _) => format!("{name}{}()", ident(&v.name)),
                        (None, Some(arm)) => format!("{name}{}()", ident(&arm.name)),
                        (None, None) => format!("{name}()"),
                    },
                    TypeKind::Struct(_) => format!("{name}()"),
                }
            }
        }
    }

    /// Whether `ty`'s zero value is mutable, and so has to be built per
    /// instance rather than shared between every default-constructed value.
    fn needs_factory(&self, ty: &WireType) -> bool {
        match ty {
            WireType::Array { .. } | WireType::VarArray { .. } => true,
            WireType::Named(id) => match &self.m.get(*id).kind {
                TypeKind::Alias(a) => self.needs_factory(&a.target),
                TypeKind::Struct(_) | TypeKind::Union(_) => true,
                _ => false,
            },
            _ => false,
        }
    }

    /// The ` = …` a dataclass field is declared with.
    fn default_clause(&self, ty: &WireType) -> String {
        let fresh = self.fresh(ty);
        if !self.needs_factory(ty) {
            return format!(" = {fresh}");
        }
        // `default_factory` wants a callable, so a bare constructor is handed
        // over as one and everything else is wrapped.
        if fresh == "[]" {
            return " = field(default_factory=list)".to_string();
        }
        match fresh.strip_suffix("()") {
            Some(ctor) if !ctor.contains(['(', ')', ' ', '[']) => {
                format!(" = field(default_factory={ctor})")
            }
            _ => format!(" = field(default_factory=lambda: {fresh})"),
        }
    }

    fn off(base: &str, delta: u32) -> String {
        if delta == 0 { base.to_string() } else { format!("{base} + {delta}") }
    }

    /// A wire type as the schema spells it, for a doc comment.
    fn wire_str(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) => format!("u{n}"),
            WireType::Int(n) => format!("i{n}"),
            WireType::Bool => "bool".to_string(),
            WireType::Float(f) => f.as_str().to_string(),
            WireType::Named(id) => self.m.get(*id).name.clone(),
            WireType::Array { elem, count } => format!("{}[{count}]", self.wire_str(elem)),
            WireType::VarArray { elem, max } => format!("{}[max: {max}]", self.wire_str(elem)),
            WireType::Str { max } => format!("string(max: {max})"),
        }
    }

    // ---------------------------------------------------------------------
    // Top level
    // ---------------------------------------------------------------------

    fn run(mut self) -> String {
        self.file_header();
        self.runtime();
        self.declarations();
        self.gatt();
        self.out
    }

    fn file_header(&mut self) {
        let from = match self.source {
            Some(path) => format!(" from `{path}`"),
            None => String::new(),
        };
        self.line(0, &format!("\"\"\"Generated by defgen{from}. Do not edit."));
        self.blank();
        self.lines(
            0,
            &[
                "Codecs for this schema's GATT values: fields in declaration order (§6),",
                "with byte order applied once per root container (§8). Encoding produces `bytes`;",
                "decoding takes the bytes the transport delivered. Anything the schema does",
                "not allow raises a `DefgenError` subclass, rather than being quietly",
                "truncated, wrapped or replaced.",
                "",
                "Only a type bound to a characteristic has `encode`/`decode`: byte order is a",
                "property of the root container, so a type that is only ever nested has no",
                "byte order of its own to be encoded in (§8).",
                "",
                "Requires Python 3.10 or newer. There are no third-party dependencies.",
                "\"\"\"",
                "",
                "from __future__ import annotations",
                "",
            ],
        );

        if self.needs.enums {
            self.line(0, "import enum");
        }
        if self.needs.raw_float {
            self.line(0, "import struct");
        }
        if self.needs.dataclass {
            let names = if self.needs.default_factory { "dataclass, field" } else { "dataclass" };
            self.line(0, &format!("from dataclasses import {names}"));
        }
        let mut typing: Vec<&str> = Vec::new();
        if self.needs.class_var {
            typing.push("ClassVar");
        }
        typing.push("Final");
        if self.needs.type_alias {
            typing.push("TypeAlias");
        }
        if self.needs.sequences {
            typing.push("TypeVar");
        }
        self.line(0, &format!("from typing import {}", typing.join(", ")));

        self.blank();
        let names = self.public_names();
        self.line(0, "__all__ = [");
        for name in names {
            self.line(1, &format!("\"{name}\","));
        }
        self.line(0, "]");
    }

    /// Everything a `from … import *` should bring in, in the order the module
    /// defines it.
    fn public_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        names.extend(ERRORS.iter().map(|(name, _, _)| (*name).to_string()));
        names.extend(self.m.consts.iter().map(|c| screaming(&c.name)));

        for def in &self.m.types {
            let name = ident(&def.name);
            names.push(name.clone());
            match &def.kind {
                TypeKind::Scaled(_) => {
                    let prefix = screaming(&def.name);
                    let fnp = snake(&def.name);
                    names.push(format!("{prefix}_SCALE"));
                    names.push(format!("{prefix}_OFFSET"));
                    names.push(format!("{fnp}_from_raw"));
                    names.push(format!("{fnp}_to_raw"));
                }
                TypeKind::Enum(e) => {
                    if let Some(arm) = &e.else_arm {
                        names.push(format!("{name}{}", ident(&arm.name)));
                        names.push(format!("{name}Value"));
                    }
                }
                TypeKind::Union(u) => {
                    names.extend(u.variants.iter().map(|v| format!("{name}{}", ident(&v.name))));
                    if let Some(arm) = &u.else_arm {
                        names.push(format!("{name}{}", ident(&arm.name)));
                    }
                }
                _ => {}
            }
            // A struct or union carries its codec as methods; anything else
            // bound to a characteristic gets module-level functions instead.
            if self.has_entry_functions(def) {
                let prefix = screaming(&def.name);
                let fnp = snake(&def.name);
                if def.layout.is_variable() {
                    names.push(format!("{prefix}_FIXED_SIZE"));
                    names.push(format!("{prefix}_MAX_SIZE"));
                } else {
                    names.push(format!("{prefix}_SIZE"));
                }
                names.push(format!("encode_{fnp}"));
                names.push(format!("decode_{fnp}"));
            }
        }

        if !self.m.services.is_empty() {
            names.extend(["GattProperty", "GattCharacteristic", "GattService"].map(str::to_string));
            for service in &self.m.services {
                let sprefix = screaming(&service.name);
                names.push(format!("{sprefix}_UUID"));
                for c in &service.characteristics {
                    names.push(format!("{sprefix}_{}_UUID", screaming(&c.name)));
                }
                names.push(sprefix);
            }
            names.push("SERVICES".to_string());
        }
        names
    }

    fn has_entry_functions(&self, def: &TypeDef) -> bool {
        def.root && !matches!(def.kind, TypeKind::Struct(_) | TypeKind::Union(_))
    }

    // ---------------------------------------------------------------------
    // Runtime
    // ---------------------------------------------------------------------

    fn runtime(&mut self) {
        self.banner("Errors");
        for (name, base, doc) in ERRORS {
            self.line(0, &format!("class {name}({base}):"));
            self.note(1, doc);
            self.gap();
        }

        self.banner("Runtime");
        if self.needs.sequences {
            self.line(0, "_T = TypeVar(\"_T\")");
            self.gap();
        }

        self.lines(
            0,
            &[
                "class _Bits:",
                "    \"\"\"A container of `size` bytes, held as one integer while it is packed.",
                "",
                "    Fields occupy the container in declaration order, first field first, and",
                "    byte order (§8) chooses which end of it they fill from: little-endian",
                "    from the least-significant end, big-endian from the most-significant one",
                "    (§6). `_shift` is the whole of that difference — every field is written",
                "    the same way otherwise, and the container is handed to `int.to_bytes` in",
                "    its own byte order at the end.",
                "    \"\"\"",
                "",
                "    __slots__ = (\"value\", \"size\", \"big\")",
                "",
                "    value: int",
                "    size: int",
                "    big: bool",
                "",
                "    def __init__(self, size: int, big: bool, value: int = 0) -> None:",
                "        self.value = value",
                "        self.size = size",
                "        self.big = big",
                "",
                "    @classmethod",
                "    def from_bytes(cls, data: bytes, big: bool) -> _Bits:",
                "        \"\"\"The bits of `data`, read in the given byte order.\"\"\"",
                "        return cls(len(data), big, int.from_bytes(data, \"big\" if big else \"little\"))",
                "",
                "    def to_bytes(self) -> bytes:",
                "        \"\"\"Exactly `size` bytes, written in the container's byte order.\"\"\"",
                "        return self.value.to_bytes(self.size, \"big\" if self.big else \"little\")",
                "",
                "    def _shift(self, off: int, bits: int) -> int:",
                "        \"\"\"Where a `bits`-wide value declared at `off` sits in `value`.\"\"\"",
                "        return self.size * 8 - off - bits if self.big else off",
                "",
                "    def get(self, off: int, bits: int) -> int:",
                "        \"\"\"The `bits` bits of the value declared at `off`.\"\"\"",
                "        return (self.value >> self._shift(off, bits)) & ((1 << bits) - 1)",
                "",
                "    def put(self, off: int, bits: int, value: int) -> None:",
                "        \"\"\"Writes the low `bits` bits of `value` into the value at `off`.\"\"\"",
                "        shift = self._shift(off, bits)",
                "        mask = ((1 << bits) - 1) << shift",
                "        self.value = (self.value & ~mask) | ((value << shift) & mask)",
                "",
                "",
                "def _sext(value: int, bits: int) -> int:",
                "    \"\"\"Sign-extends an `iN` value from bit N-1 (§2).\"\"\"",
                "    sign = 1 << (bits - 1)",
                "    return (value ^ sign) - sign",
                "",
                "",
                "def _check_uint(value: int, bits: int, where: str) -> int:",
                "    \"\"\"Range-checks a `uN` value: out of range is an error, never a",
                "    truncation (§2).",
                "    \"\"\"",
                "    if not 0 <= value < (1 << bits):",
                "        raise DefgenRangeError(f\"{where}: {value} does not fit in u{bits}\")",
                "    return value",
                "",
                "",
                "def _check_int(value: int, bits: int, where: str) -> int:",
                "    \"\"\"Range-checks an `iN` value and returns its two's-complement bits (§2).\"\"\"",
                "    limit = 1 << (bits - 1)",
                "    if not -limit <= value < limit:",
                "        raise DefgenRangeError(f\"{where}: {value} does not fit in i{bits}\")",
                "    return value & ((1 << bits) - 1)",
            ],
        );

        if self.needs.sequences {
            self.gap();
            self.lines(
                0,
                &[
                    "def _check_count(seq: list[_T], count: int, where: str) -> list[_T]:",
                    "    \"\"\"A fixed-size array carries exactly `count` elements, always (§6.1).\"\"\"",
                    "    if len(seq) != count:",
                    "        raise DefgenRangeError(",
                    "            f\"{where}: expected exactly {count} elements, got {len(seq)}\"",
                    "        )",
                    "    return seq",
                    "",
                    "",
                    "def _check_max(seq: list[_T], limit: int, where: str) -> list[_T]:",
                    "    \"\"\"A variable-length array carries at most `limit` elements (§6.3).\"\"\"",
                    "    if len(seq) > limit:",
                    "        raise DefgenRangeError(",
                    "            f\"{where}: {len(seq)} elements exceeds the maximum of {limit}\"",
                    "        )",
                    "    return seq",
                ],
            );
        }

        if self.needs.utf8 {
            self.gap();
            self.lines(
                0,
                &[
                    "def _encode_utf8(text: str, limit: int, where: str) -> bytes:",
                    "    \"\"\"A `string` field's bytes, rejecting anything past its `max` (§6.3).\"\"\"",
                    "    try:",
                    "        data = text.encode(\"utf-8\")",
                    "    except UnicodeEncodeError as exc:",
                    "        raise DefgenUtf8Error(f\"{where}: {exc}\") from None",
                    "    if len(data) > limit:",
                    "        raise DefgenRangeError(",
                    "            f\"{where}: {len(data)} bytes exceeds the maximum of {limit}\"",
                    "        )",
                    "    return data",
                    "",
                    "",
                    "def _decode_utf8(data: bytes, where: str) -> str:",
                    "    \"\"\"Decodes a `string` field. Malformed input fails rather than being",
                    "    patched up with replacement characters (§6.3): `bytes.decode` is",
                    "    strict by default, and rejects surrogates and overlong encodings.",
                    "    \"\"\"",
                    "    try:",
                    "        return data.decode(\"utf-8\")",
                    "    except UnicodeDecodeError as exc:",
                    "        raise DefgenUtf8Error(f\"{where}: {exc}\") from None",
                ],
            );
        }

        if self.needs.round {
            self.gap();
            self.lines(
                0,
                &[
                    "def _round(value: float, where: str) -> int:",
                    "    \"\"\"Rounds half away from zero, which is what C's `round()` does.",
                    "",
                    "    The backends have to agree on a `scaled` value's raw integer down to",
                    "    the last unit (§4, §13), and Python's own `round()` rounds half to",
                    "    even — 0.5 to 0, not to 1. The C backend carries the mirror image of",
                    "    this function rather than calling `round()` from libm.",
                    "",
                    "    `int()` truncates toward zero and is exact for every finite float, and",
                    "    subtracting that integer part back off is exact too, so the comparison",
                    "    below sees the true remainder. Adding 0.5 first would not: for the",
                    "    double just below 0.5, the addition itself rounds up to 1.0.",
                    "    \"\"\"",
                    "    if value - value != 0.0:",
                    "        # Infinite or NaN — every other float subtracts to zero. No integer",
                    "        # represents either, so this is where a scale that overflowed the",
                    "        # division lands, rather than in an `OverflowError` from `int()`.",
                    "        raise DefgenRangeError(f\"{where}: {value} cannot be rounded to an integer\")",
                    "    whole = int(value)",
                    "    remainder = value - whole",
                    "    if remainder >= 0.5:",
                    "        return whole + 1",
                    "    if remainder <= -0.5:",
                    "        return whole - 1",
                    "    return whole",
                ],
            );
        }
        self.blank();
    }

    // ---------------------------------------------------------------------
    // Declarations
    // ---------------------------------------------------------------------

    /// Types in source order. §9 forbids forward references, so source order is
    /// already a valid definition order for a module executed top to bottom.
    fn declarations(&mut self) {
        self.banner("Types");
        let m = self.m;
        for def in &m.types {
            match &def.kind {
                TypeKind::Alias(a) => self.declare_alias(def, &a.target),
                TypeKind::Scaled(s) => self.declare_scaled(def, s),
                TypeKind::Enum(e) => self.declare_enum(def, e),
                TypeKind::Union(u) => self.declare_union(def, u),
                TypeKind::Struct(s) => self.declare_struct(def, s),
            }
            if self.has_entry_functions(def) {
                self.entry_functions(def);
            }
        }
        if !m.consts.is_empty() {
            self.banner("Constants");
            for c in &m.consts {
                self.declare_const(c);
            }
        }
    }

    // -- const (§3.1) ---------------------------------------------------------

    fn declare_const(&mut self, c: &'m Const) {
        let value = if c.negative { format!("-{}", c.magnitude) } else { c.magnitude.to_string() };
        self.line(0, &format!("{}: Final[int] = {value}", screaming(&c.name)));
        self.docs_with(0, &c.docs, &[]);
        self.gap();
    }

    // -- alias (§3) ---------------------------------------------------------

    fn declare_alias(&mut self, def: &'m TypeDef, target: &'m WireType) {
        let name = ident(&def.name);
        let ty = self.py_type(target);
        let notes = vec![format!(
            "`{}` (§3): a name for `{}`, with no runtime type of its own.",
            def.name,
            self.wire_str(target)
        )];
        self.line(0, &format!("{name}: TypeAlias = {ty}"));
        self.docs_with(0, &def.docs, &notes);
        self.gap();
    }

    // -- scaled (§4) --------------------------------------------------------

    fn declare_scaled(&mut self, def: &'m TypeDef, s: &'m Scaled) {
        let name = ident(&def.name);
        let prefix = screaming(&def.name);
        let fnp = snake(&def.name);
        let raw = format!("{}{}", if s.signed { "i" } else { "u" }, s.raw_bits);
        let physical = match s.physical {
            FloatType::F32 => "f32",
            FloatType::F64 => "f64",
        };
        let (min, max) = int_range(s.raw_bits, s.signed);

        self.line(0, &format!("{name}: TypeAlias = float"));
        let notes = vec![
            format!("`{}` (§4): `physical = raw * scale + offset`, over a `{raw}` wire value.", def.name),
            format!(
                "The schema's physical type is `{physical}`; Python has only the one binary \
                 float, so both map to `float`."
            ),
        ];
        self.docs_with(0, &def.docs, &notes);
        self.blank();
        self.line(0, &format!("{prefix}_SCALE: Final = {}", float_lit(s.scale)));
        self.line(0, &format!("{prefix}_OFFSET: Final = {}", float_lit(s.offset)));
        self.gap();

        self.line(0, &format!("def {fnp}_from_raw(raw: int) -> {name}:"));
        self.note(1, "Decodes the raw wire integer into the physical value (§4).");
        self.line(1, &format!("return raw * {prefix}_SCALE + {prefix}_OFFSET"));
        self.gap();

        self.line(0, &format!("def {fnp}_to_raw(value: {name}) -> int:"));
        self.docstring(
            1,
            &[
                "Rounds `value` to the nearest raw wire integer (§4).".to_string(),
                String::new(),
                format!("Anything outside `{raw}`'s range is an error rather than a wraparound."),
                "The raw integer is reachable this way, so a caller can round-trip a".to_string(),
                "value without going through floating point at all.".to_string(),
            ],
        );
        // `_round` rejects a non-finite value itself, which covers both a NaN
        // or infinity handed in by the caller and one the division produced.
        self.line(1, &format!("raw = _round((value - {prefix}_OFFSET) / {prefix}_SCALE, \"{}\")", def.name));
        self.line(1, &format!("if not {min} <= raw <= {max}:"));
        self.raise(
            2,
            "DefgenRangeError",
            &format!("f\"{}: {{value}} is out of range for {raw} (raw {{raw}})\"", def.name),
        );
        self.line(1, "return raw");
        self.gap();
    }

    // -- plain enum (§5) ----------------------------------------------------

    fn declare_enum(&mut self, def: &'m TypeDef, e: &'m Enum) {
        let name = ident(&def.name);
        let bits = e.backing_bits;
        let unknown = e.else_arm.as_ref().map(|arm| format!("{name}{}", ident(&arm.name)));
        let value_ty = match &unknown {
            Some(_) => format!("{name}Value"),
            None => name.clone(),
        };

        self.line(0, &format!("class {name}(enum.IntEnum):"));
        let notes = vec![
            format!("{} {bits}-bit wire value (§5).", article(bits).to_uppercase()),
            match &unknown {
                Some(unknown) => format!(
                    "Open: a value matching none of the variants below decodes to `{unknown}` \
                     instead of failing, so decoding this enum never fails."
                ),
                None => "Closed: a value matching none of the variants below is a hard error, \
                         on encode as well as on decode."
                    .to_string(),
            },
        ];
        self.docs_with(1, &def.docs, &notes);
        self.blank();
        for v in &e.variants {
            self.line(1, &format!("{} = {}", screaming(&v.name), v.value));
            self.docstring(1, &doc_lines(&v.docs));
        }
        if e.variants.is_empty() {
            self.line(1, "pass");
        }
        self.blank();

        self.line(1, "@staticmethod");
        self.line(1, &format!("def _decode(raw: int) -> {value_ty}:"));
        match &unknown {
            Some(unknown) => {
                self.note(2, &format!("The variant `raw` names, or `{unknown}` (§5)."));
                self.line(2, "try:");
                self.line(3, &format!("return {name}(raw)"));
                self.line(2, "except ValueError:");
                self.line(3, &format!("return {unknown}(raw=raw)"));
            }
            None => {
                self.note(2, "The variant `raw` names; an unmatched value is an error (§5).");
                self.line(2, "try:");
                self.line(3, &format!("return {name}(raw)"));
                self.line(2, "except ValueError:");
                self.raise(
                    3,
                    "DefgenUnknownValueError",
                    &format!("f\"{}: {{raw}} matches no declared variant\"", def.name),
                );
                // `from None` keeps the traceback about the schema rather than
                // about `enum`'s own lookup failing.
                self.replace_last("        )", "        ) from None");
                self.replace_last(")\n", ") from None\n");
            }
        }
        self.blank();

        self.line(1, "@staticmethod");
        self.line(1, &format!("def _encode(value: {value_ty}) -> int:"));
        self.note(2, "The wire value `value` encodes to.");
        match &unknown {
            Some(unknown) => {
                self.line(2, &format!("if isinstance(value, {unknown}):"));
                self.line(3, "return value.raw");
                self.line(2, "return int(value)");
            }
            None => {
                self.line(2, "try:");
                self.line(3, &format!("return int({name}(value))"));
                self.line(2, "except ValueError:");
                self.raise(
                    3,
                    "DefgenUnknownValueError",
                    &format!("f\"{}: {{value}} matches no declared variant\"", def.name),
                );
                self.replace_last(")\n", ") from None\n");
            }
        }
        self.gap();

        // The fallback variant, and the alias letting a field hold either it or
        // a declared variant (§5, §12).
        if let Some(arm) = &e.else_arm {
            let unknown = format!("{name}{}", ident(&arm.name));
            let notes = vec![format!(
                "A wire value `{name}` does not declare (§5). It keeps the value it was \
                 decoded from, so re-encoding it is lossless."
            )];
            self.line(0, "@dataclass(frozen=True, slots=True)");
            self.line(0, &format!("class {unknown}:"));
            self.docs_with(1, &arm.docs, &notes);
            self.blank();
            self.line(1, "raw: int = 0");
            self.gap();
            self.line(0, &format!("{name}Value: TypeAlias = {name} | {unknown}"));
            self.note(0, &format!("A declared `{name}` variant, or a value it does not declare (§5)."));
            self.gap();
        }
    }

    /// Rewrites the tail of the output. Only `raise … from None` uses this: the
    /// clause hangs off whichever of [`Emitter::raise`]'s two layouts was
    /// chosen, and reproducing that choice at every call site would be worse.
    fn replace_last(&mut self, suffix: &str, replacement: &str) {
        if self.out.ends_with(suffix) {
            let keep = self.out.len() - suffix.len();
            self.out.truncate(keep);
            self.out.push_str(replacement);
        }
    }

    // -- tagged union (§7) --------------------------------------------------

    fn declare_union(&mut self, def: &'m TypeDef, u: &'m Union) {
        let name = ident(&def.name);
        let tag = ident(&u.tag_name);
        let table = format!("_{}_BY_ID", screaming(&def.name));
        let (tag_bits, payload_bits) = (u.tag_bits, u.payload_bits);
        let big = def.endian == Endianness::Big;
        let size = def.layout.fixed_bytes();

        // -- the base --
        self.line(0, &format!("class {name}:"));
        let mut notes = vec![
            format!(
                "A tagged union (§7): {} {tag_bits}-bit `{}` in the container's low bits, then \
                 {} {payload_bits}-bit payload the id says how to read.",
                article(tag_bits),
                u.tag_name,
                article(payload_bits)
            ),
            format!(
                "Sealed: every value is one of the `{name}…` classes below, so a decoded \
                 command is matched with `isinstance`, never by inspecting a tag by hand."
            ),
        ];
        if !u.is_open() {
            notes.push("An id matching no variant is a hard decode error.".to_string());
        }
        self.docs_with(1, &def.docs, &notes);
        self.blank();
        self.line(1, "__slots__ = ()");
        self.blank();
        self.line(1, &format!("BIT_WIDTH: ClassVar[int] = {}", def.layout.fixed_bits));
        self.line(1, &format!("SIZE: ClassVar[int] = {size}"));
        self.line(1, &format!("TAG_BITS: ClassVar[int] = {tag_bits}"));
        self.blank();

        if def.root {
            self.encode_method(&def.name, size, big, None);
            // `decode` is inherited by every variant, so it names the base
            // outright: going through `cls` would skip the dispatch and read
            // one variant's payload whatever the id on the wire says.
            self.decode_method(&def.name, &name, size, big, Some(&name));
        }

        self.line(1, "def _pack_fixed(self, _bits: _Bits, _off: int) -> None:");
        self.note(2, "Packs this variant, id included, at bit `_off`. Internal.");
        self.line(2, "raise NotImplementedError");
        self.blank();

        self.line(1, "@classmethod");
        self.line(1, &format!("def _unpack_fixed(cls, _bits: _Bits, _off: int) -> {name}:"));
        self.note(2, "Reads the id at `_off` and dispatches to the variant it names (§7).");
        self.line(2, &format!("_tag = _bits.get(_off, {tag_bits})"));
        self.line(2, &format!("_variant = {table}.get(_tag)"));
        self.line(2, "if _variant is None:");
        match &u.else_arm {
            Some(arm) => {
                let unknown = format!("{name}{}", ident(&arm.name));
                let raw = if arm.raw_bits > 0 {
                    format!(", raw=_bits.get({}, {})", Self::off("_off", tag_bits), arm.raw_bits)
                } else {
                    String::new()
                };
                self.line(3, &format!("return {unknown}({tag}=_tag{raw})"));
            }
            None => self.raise(
                3,
                "DefgenUnknownValueError",
                &format!("f\"{}: id {{_tag}} matches no declared variant\"", def.name),
            ),
        }
        self.line(2, "return _variant._unpack_payload(_bits, _off)");
        self.blank();

        self.line(1, "@classmethod");
        self.line(1, &format!("def _unpack_payload(cls, _bits: _Bits, _off: int) -> {name}:"));
        self.note(2, "Reads this variant's payload; the id is already matched. Internal.");
        self.line(2, "raise NotImplementedError");
        self.gap();

        // -- one dataclass per variant --
        for v in &u.variants {
            let cls = format!("{name}{}", ident(&v.name));
            self.line(0, "@dataclass(slots=True)");
            self.line(0, &format!("class {cls}({name}):"));
            self.docs_with(1, &v.docs, &[format!("Wire id `0x{:x}` (§7).", v.id)]);
            self.blank();
            self.line(1, &format!("ID: ClassVar[int] = 0x{:x}", v.id));
            self.blank();
            let mut any = false;
            for f in &v.fields {
                if let Some(fname) = f.name() {
                    any = true;
                    self.field_member(1, f, fname, &cls);
                }
            }
            if any {
                self.blank();
            }

            self.line(1, "def _pack_fixed(self, _bits: _Bits, _off: int) -> None:");
            self.note(2, "Packs this variant, id included, at bit `_off`. Internal.");
            self.line(2, &format!("_bits.put(_off, {tag_bits}, 0x{:x})", v.id));
            for f in &v.fields {
                let Some(fname) = f.name() else { continue };
                let expr = format!("self.{}", ident(fname));
                let off = Self::off("_off", tag_bits + f.offset_bits);
                let label = format!("{cls}.{fname}");
                self.pack(2, &expr, &f.ty, &off, &label, 0);
            }
            self.blank();

            self.line(1, "@classmethod");
            self.line(1, &format!("def _unpack_payload(cls, _bits: _Bits, _off: int) -> {cls}:"));
            self.note(2, "Reads this variant's payload; the id is already matched. Internal.");
            for f in &v.fields {
                self.padding_check(2, f, tag_bits, &cls);
            }
            let args: Vec<String> = v
                .fields
                .iter()
                .filter_map(|f| {
                    let fname = f.name()?;
                    let off = Self::off("_off", tag_bits + f.offset_bits);
                    Some(format!("{}={}", ident(fname), self.unpack_expr(&f.ty, &off, 0)))
                })
                .collect();
            self.call(2, "return cls", &args);
            self.gap();
        }

        // -- the fallback variant --
        if let Some(arm) = &u.else_arm {
            let cls = format!("{name}{}", ident(&arm.name));
            let notes = vec![format!(
                "An id `{name}` does not declare (§7). Both the id and the undecoded payload \
                 are kept, so re-encoding is lossless and an unknown command can never be \
                 mistaken for a known one."
            )];
            self.line(0, "@dataclass(slots=True)");
            self.line(0, &format!("class {cls}({name}):"));
            self.docs_with(1, &arm.docs, &notes);
            self.blank();
            self.line(1, &format!("{tag}: int = 0"));
            self.note(1, "The unrecognized wire id.");
            if arm.raw_bits > 0 {
                self.line(1, "raw: int = 0");
                self.note(1, &format!("The {} payload bits, undecoded.", arm.raw_bits));
            }
            self.blank();
            self.line(1, "def _pack_fixed(self, _bits: _Bits, _off: int) -> None:");
            self.note(2, "Writes the unrecognized id and its payload back verbatim. Internal.");
            self.line(
                2,
                &format!(
                    "_bits.put(_off, {tag_bits}, _check_uint(self.{tag}, {tag_bits}, \"{cls}.{}\"))",
                    u.tag_name
                ),
            );
            if arm.raw_bits > 0 {
                let (off, bits) = (Self::off("_off", tag_bits), arm.raw_bits);
                self.line(
                    2,
                    &format!("_bits.put({off}, {bits}, _check_uint(self.raw, {bits}, \"{cls}.raw\"))"),
                );
            }
            self.gap();
        }

        // -- the dispatch table --
        self.line(0, &format!("{table}: Final[dict[int, type[{name}]]] = {{"));
        for v in &u.variants {
            self.line(1, &format!("0x{:x}: {name}{},", v.id, ident(&v.name)));
        }
        self.line(0, "}");
        self.gap();
    }

    // -- struct (§6) --------------------------------------------------------

    fn declare_struct(&mut self, def: &'m TypeDef, s: &'m Struct) {
        let name = ident(&def.name);
        let big = def.endian == Endianness::Big;

        self.line(0, "@dataclass(slots=True)");
        self.line(0, &format!("class {name}:"));
        let mut notes: Vec<String> = Vec::new();
        if let Some(tail) = def.layout.tail {
            notes.push(format!(
                "Variable-length (§6.3): a {}-byte fixed prefix, then up to {} trailing \
                 element(s). The length is whatever the transport delivers — nothing in the \
                 payload states it.",
                def.layout.fixed_bytes(),
                tail.max_elems
            ));
        }
        if self.docs_with(1, &def.docs, &notes) {
            self.blank();
        }
        if def.layout.is_variable() {
            self.line(1, &format!("FIXED_SIZE: ClassVar[int] = {}", def.layout.fixed_bytes()));
            self.line(1, &format!("MAX_SIZE: ClassVar[int] = {}", def.layout.max_bytes()));
        } else {
            self.line(1, &format!("BIT_WIDTH: ClassVar[int] = {}", def.layout.fixed_bits));
            // A nested-only container is free to be sub-byte (§6); only a bound
            // one has to be a whole number of bytes (§10), so a byte size is
            // only stated where it is exact.
            if def.layout.is_byte_aligned() {
                self.line(1, &format!("SIZE: ClassVar[int] = {}", def.layout.fixed_bytes()));
            }
        }
        self.blank();

        let mut any = false;
        for f in &s.fields {
            if let Some(fname) = f.name() {
                any = true;
                self.field_member(1, f, fname, &def.name);
            }
        }
        if any {
            self.blank();
        }

        if def.root {
            let size = def.layout.fixed_bytes();
            match def.layout.tail {
                None => {
                    self.encode_method(&def.name, size, big, None);
                    self.decode_method(&def.name, &name, size, big, None);
                }
                Some(_) => {
                    self.encode_method(&def.name, size, big, Some(def.layout.max_bytes()));
                    self.decode_var_method(&def.name, &name, big);
                    self.line(1, "def encoded_size(self) -> int:");
                    self.note(2, "Bytes this value encodes to as it stands — never padded out (§6.3).");
                    self.line(2, "return self.FIXED_SIZE + self._tail_len()");
                    self.blank();
                }
            }
        }

        // -- the fixed part --
        self.line(1, "def _pack_fixed(self, _bits: _Bits, _off: int) -> None:");
        self.note(2, "Packs the fixed part into `_bits`, at bit `_off`. Internal.");
        for f in &s.fields {
            let Some(fname) = f.name() else { continue };
            // An inline variable-length field contributes no fixed bits: the
            // tail methods are what write it.
            if matches!(self.tail_kind(&f.ty), Some(TailKind::Inline)) {
                continue;
            }
            let expr = format!("self.{}", ident(fname));
            let off = Self::off("_off", f.offset_bits);
            let label = format!("{}.{fname}", def.name);
            self.pack(2, &expr, &f.ty, &off, &label, 0);
        }
        self.blank();

        self.line(1, "@classmethod");
        self.line(1, &format!("def _unpack_fixed(cls, _bits: _Bits, _off: int) -> {name}:"));
        self.note(2, "Unpacks the fixed part from `_bits`, at bit `_off`. Internal.");
        for f in &s.fields {
            self.padding_check(2, f, 0, &def.name);
        }
        let args: Vec<String> = s
            .fields
            .iter()
            .filter(|f| !matches!(self.tail_kind(&f.ty), Some(TailKind::Inline)))
            .filter_map(|f| {
                let fname = f.name()?;
                let off = Self::off("_off", f.offset_bits);
                Some(format!("{}={}", ident(fname), self.unpack_expr(&f.ty, &off, 0)))
            })
            .collect();
        self.call(2, "return cls", &args);
        self.blank();

        // -- the variable-length tail --
        if let Some((ty, expr, label)) = self.tail_of_struct(def, s) {
            self.tail_methods(&ty, &expr, &label);
        }
        self.blank();
    }

    /// One dataclass attribute, with its doc comment and — for a `reserved`
    /// field — the note that it round-trips rather than belonging to the caller
    /// (§6.2). Python documents an attribute with a string right below it,
    /// which is the form tooling reads.
    fn field_member(&mut self, ind: usize, f: &'m Field, name: &str, owner: &str) {
        let ty = self.py_type(&f.ty);
        let default = self.default_clause(&f.ty);
        let mut notes: Vec<String> = Vec::new();
        if matches!(f.role, FieldRole::Reserved { .. }) {
            notes.push(
                "Reserved (§6.2): captured on decode and written back unchanged, so a \
                 decode-then-relay round trip does not clobber it."
                    .to_string(),
            );
        }
        if let WireType::Array { count, .. } = &f.ty {
            notes.push(format!("Exactly {count} elements (§6.1); any other count fails to encode."));
        }
        if let WireType::VarArray { max, .. } = &f.ty {
            notes.push(format!("At most {max} elements (§6.3)."));
        }
        if let WireType::Str { max } = &f.ty {
            notes.push(format!("At most {max} UTF-8 bytes — `max` bounds bytes, not characters (§6.3)."));
        }
        let _ = owner;
        self.line(ind, &format!("{}: {ty}{default}", ident(name)));
        self.docs_with(ind, &f.docs, &notes);
    }

    /// `padding: uN = 0` is validated on decode; bare `padding` is not (§6.2).
    fn padding_check(&mut self, ind: usize, f: &Field, base_bits: u32, owner: &str) {
        let FieldRole::Padding { check_zero: true } = f.role else { return };
        let off = Self::off("_off", base_bits + f.offset_bits);
        let bits = f.layout.fixed_bits;
        let (from, to) = (f.offset_bits, f.offset_bits + bits);
        self.line(ind, &format!("if _bits.get({off}, {bits}) != 0:"));
        self.raise(
            ind + 1,
            "DefgenPaddingError",
            &format!("\"{owner}: padding at bits {from}..{to} is not zero\""),
        );
    }

    // ---------------------------------------------------------------------
    // Entry points (§12)
    // ---------------------------------------------------------------------

    fn encode_method(&mut self, schema_name: &str, size: u64, big: bool, max: Option<u128>) {
        let order = if big { "big" } else { "little" };
        self.line(1, "def encode(self) -> bytes:");
        match max {
            None => self.note(
                2,
                &format!("Encodes this `{schema_name}` into exactly SIZE ({size}) bytes, {order}-endian."),
            ),
            Some(max) => {
                self.docstring(
                    2,
                    &[
                        format!("Encodes this `{schema_name}`, {order}-endian (§8)."),
                        String::new(),
                        format!("The result is the {size}-byte fixed prefix plus however many bytes"),
                        format!("the tail actually needs — FIXED_SIZE to MAX_SIZE ({max}), never"),
                        "padded out to the maximum (§6.3).".to_string(),
                    ],
                );
            }
        }
        self.line(2, &format!("_bits = _Bits({size}, big={})", py_bool(big)));
        self.line(2, "self._pack_fixed(_bits, 0)");
        let to_bytes = "_bits.to_bytes()".to_string();
        match max {
            None => self.line(2, &format!("return {to_bytes}")),
            Some(_) => self.line(2, &format!("return {to_bytes} + self._pack_tail({})", py_bool(big))),
        }
        self.blank();
    }

    /// `owner` names the class whose `_unpack_fixed` to call, where inheriting
    /// `decode` would otherwise resolve it to a subclass's.
    fn decode_method(&mut self, schema_name: &str, name: &str, size: u64, big: bool, owner: Option<&str>) {
        let owner = owner.unwrap_or("cls");
        self.line(1, "@classmethod");
        self.line(1, &format!("def decode(cls, data: bytes) -> {name}:"));
        self.note(2, &format!("Decodes exactly {size} bytes; any other length is an error."));
        self.line(2, &format!("if len(data) != {size}:"));
        self.raise(
            3,
            "DefgenLengthError",
            &format!("f\"{schema_name}: expected {size} bytes, got {{len(data)}}\""),
        );
        self.line(
            2,
            &format!("return {owner}._unpack_fixed(_Bits.from_bytes(data, big={}), 0)", py_bool(big)),
        );
        self.blank();
    }

    fn decode_var_method(&mut self, schema_name: &str, name: &str, big: bool) {
        self.line(1, "@classmethod");
        self.line(1, &format!("def decode(cls, data: bytes) -> {name}:"));
        self.docstring(
            2,
            &[
                "Decodes the bytes the transport delivered.".to_string(),
                String::new(),
                "The tail's length comes from `len(data)`, never from the payload".to_string(),
                "itself (§6.3), so a length outside FIXED_SIZE..MAX_SIZE is an error.".to_string(),
            ],
        );
        self.line(2, "if not cls.FIXED_SIZE <= len(data) <= cls.MAX_SIZE:");
        self.raise(
            3,
            "DefgenLengthError",
            &format!(
                "f\"{schema_name}: expected {{cls.FIXED_SIZE}}..{{cls.MAX_SIZE}} bytes, got {{len(data)}}\""
            ),
        );
        self.line(
            2,
            &format!(
                "value = cls._unpack_fixed(_Bits.from_bytes(data[: cls.FIXED_SIZE], big={}), 0)",
                py_bool(big)
            ),
        );
        self.line(2, &format!("value._unpack_tail(data[cls.FIXED_SIZE :], {})", py_bool(big)));
        self.line(2, "return value");
        self.blank();
    }

    /// The module-level codec an `alias`, `scaled` or `enum` bound to a
    /// characteristic gets (§10): none of the three is a class, so there is
    /// nowhere to hang a method.
    fn entry_functions(&mut self, def: &'m TypeDef) {
        let name = def.name.clone();
        let fnp = snake(&def.name);
        let prefix = screaming(&def.name);
        let big = def.endian == Endianness::Big;
        let order = def.endian.as_str();
        let size = def.layout.fixed_bytes();
        let ty = self.py_type(&WireType::Named(def.id));
        let target = self.resolve(&WireType::Named(def.id));

        if def.layout.tail.is_none() {
            self.line(0, &format!("{prefix}_SIZE: Final = {size}"));
            self.note(0, &format!("Encoded size of a `{name}`, in bytes."));
            self.gap();

            self.line(0, &format!("def encode_{fnp}(value: {ty}) -> bytes:"));
            self.note(1, &format!("Encodes a `{name}` into exactly {size} bytes, {order}-endian."));
            self.line(1, &format!("_bits = _Bits({size}, big={})", py_bool(big)));
            self.pack(1, "value", &target, "0", &name, 0);
            self.line(1, "return _bits.to_bytes()");
            self.gap();

            self.line(0, &format!("def decode_{fnp}(data: bytes) -> {ty}:"));
            self.note(1, &format!("Decodes exactly {size} bytes into a `{name}`."));
            self.line(1, &format!("if len(data) != {size}:"));
            self.raise(
                2,
                "DefgenLengthError",
                &format!("f\"{name}: expected {size} bytes, got {{len(data)}}\""),
            );
            self.line(1, &format!("_bits = _Bits.from_bytes(data, big={})", py_bool(big)));
            let value = self.unpack_expr(&target, "0", 0);
            self.line(1, &format!("return {value}"));
            self.gap();
            return;
        }

        self.line(0, &format!("{prefix}_FIXED_SIZE: Final = {size}"));
        self.note(0, "Bytes always present, before the variable-length tail (§6.3).");
        self.line(0, &format!("{prefix}_MAX_SIZE: Final = {}", def.layout.max_bytes()));
        self.note(0, "Largest legal encoding — what a receive buffer has to hold.");
        self.gap();

        self.line(0, &format!("def encode_{fnp}(value: {ty}) -> bytes:"));
        self.docstring(
            1,
            &[
                format!("Encodes a `{name}`, {order}-endian (§8)."),
                String::new(),
                "The encoding is exactly as long as the value is, never padded out to".to_string(),
                "the declared maximum (§6.3).".to_string(),
            ],
        );
        if size > 0 {
            self.line(1, &format!("_bits = _Bits({size}, big={})", py_bool(big)));
            self.pack(1, "value", &target, "0", &name, 0);
            self.line(1, "_prefix = _bits.to_bytes()");
        } else {
            self.line(1, "_prefix = b\"\"");
        }
        let tail = self.pack_tail_body(1, &target, "value", &name, py_bool(big));
        self.line(1, &format!("return _prefix + {tail}"));
        self.gap();

        self.line(0, &format!("def decode_{fnp}(data: bytes) -> {ty}:"));
        self.note(1, &format!("Decodes the bytes the transport delivered into a `{name}` (§6.3)."));
        self.line(1, &format!("if not {prefix}_FIXED_SIZE <= len(data) <= {prefix}_MAX_SIZE:"));
        self.raise(
            2,
            "DefgenLengthError",
            &format!(
                "f\"{name}: expected {{{prefix}_FIXED_SIZE}}..{{{prefix}_MAX_SIZE}} bytes, got {{len(data)}}\""
            ),
        );
        self.unpack_alias_tail(1, &target, &name, &prefix, big);
        self.gap();
    }

    /// The body of a `decode_…` for a variable-length type bound straight to a
    /// characteristic (§6.3), which has no dataclass to hold tail methods.
    fn unpack_alias_tail(&mut self, ind: usize, target: &WireType, name: &str, prefix: &str, big: bool) {
        match target {
            WireType::Str { .. } => self.line(ind, &format!("return _decode_utf8(data, \"{name}\")")),
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                let py = self.py_type(elem);
                self.line(ind, &format!("if len(data) % {bytes} != 0:"));
                self.raise(
                    ind + 1,
                    "DefgenLengthError",
                    &format!(
                        "f\"{name}: {{len(data)}} bytes is not a whole number of {bytes}-byte elements\""
                    ),
                );
                self.line(ind, &format!("_values: list[{py}] = []"));
                self.line(ind, &format!("for _i0 in range(len(data) // {bytes}):"));
                self.line(
                    ind + 1,
                    &format!(
                        "_bits = _Bits.from_bytes(data[_i0 * {bytes} : (_i0 + 1) * {bytes}], big={})",
                        py_bool(big)
                    ),
                );
                let elem_expr = self.unpack_expr(elem, "0", 1);
                self.line(ind + 1, &format!("_values.append({elem_expr})"));
                self.line(ind, "return _values");
            }
            // A named type that owns the tail: a variable-length struct.
            WireType::Named(id) => {
                let cls = ident(&self.m.get(*id).name);
                self.line(
                    ind,
                    &format!(
                        "value = {cls}._unpack_fixed(_Bits.from_bytes(data[:{prefix}_FIXED_SIZE], \
                         big={}), 0)",
                        py_bool(big)
                    ),
                );
                self.line(ind, &format!("value._unpack_tail(data[{prefix}_FIXED_SIZE:], {})", py_bool(big)));
                self.line(ind, "return value");
            }
            _ => self.line(ind, "return data"),
        }
    }

    // ---------------------------------------------------------------------
    // Value-level emitters
    // ---------------------------------------------------------------------

    /// Emits the statements writing `expr` into `_bits` at bit `off`.
    fn pack(&mut self, ind: usize, expr: &str, ty: &WireType, off: &str, label: &str, depth: usize) {
        match ty {
            WireType::UInt(n) => {
                self.line(ind, &format!("_bits.put({off}, {n}, _check_uint({expr}, {n}, \"{label}\"))"))
            }
            WireType::Int(n) => {
                self.line(ind, &format!("_bits.put({off}, {n}, _check_int({expr}, {n}, \"{label}\"))"))
            }
            WireType::Bool => self.line(ind, &format!("_bits.put({off}, 1, 1 if {expr} else 0)")),
            WireType::Float(f) => {
                let (float_fmt, uint_fmt, bits) = float_struct_fmts(*f);
                let raw = format!("struct.unpack(\"<{uint_fmt}\", struct.pack(\"<{float_fmt}\", {expr}))[0]");
                self.line(ind, &format!("_bits.put({off}, {bits}, {raw})"));
            }
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => {
                        let target = a.target.clone();
                        self.pack(ind, expr, &target, off, label, depth);
                    }
                    TypeKind::Scaled(s) => {
                        let check = if s.signed { "_check_int" } else { "_check_uint" };
                        let bits = s.raw_bits;
                        let raw = format!("{}_to_raw({expr})", snake(&def.name));
                        self.line(
                            ind,
                            &format!("_bits.put({off}, {bits}, {check}({raw}, {bits}, \"{label}\"))"),
                        );
                    }
                    TypeKind::Enum(e) => {
                        let bits = e.backing_bits;
                        let raw = format!("{name}._encode({expr})");
                        self.line(
                            ind,
                            &format!("_bits.put({off}, {bits}, _check_uint({raw}, {bits}, \"{label}\"))"),
                        );
                    }
                    TypeKind::Struct(_) | TypeKind::Union(_) => {
                        self.line(ind, &format!("{expr}._pack_fixed(_bits, {off})"))
                    }
                }
            }
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                let (i, v) = (format!("_i{depth}"), format!("_v{depth}"));
                self.line(
                    ind,
                    &format!("for {i}, {v} in enumerate(_check_count({expr}, {count}, \"{label}\")):"),
                );
                let elem_off = format!("{off} + {i} * {elem_bits}");
                let elem = (**elem).clone();
                self.pack(ind + 1, &v, &elem, &elem_off, label, depth + 1);
            }
            // Written by the tail code, never as part of the fixed prefix.
            WireType::VarArray { .. } | WireType::Str { .. } => {}
        }
    }

    /// The expression reading a value of `ty` out of `_bits` at bit `off`.
    ///
    /// Every case is a single expression — a fallible decode raises from inside
    /// the helper it calls — which is what lets a decoded struct be built in one
    /// constructor call and an array be a comprehension.
    fn unpack_expr(&self, ty: &WireType, off: &str, depth: usize) -> String {
        match ty {
            WireType::UInt(n) => format!("_bits.get({off}, {n})"),
            WireType::Int(n) => format!("_sext(_bits.get({off}, {n}), {n})"),
            WireType::Bool => format!("_bits.get({off}, 1) != 0"),
            WireType::Float(f) => {
                let (float_fmt, uint_fmt, bits) = float_struct_fmts(*f);
                format!(
                    "struct.unpack(\"<{float_fmt}\", struct.pack(\"<{uint_fmt}\", _bits.get({off}, {bits})))[0]"
                )
            }
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => self.unpack_expr(&a.target, off, depth),
                    TypeKind::Scaled(s) => {
                        let (bits, raw) = (s.raw_bits, format!("_bits.get({off}, {})", s.raw_bits));
                        let raw = if s.signed { format!("_sext({raw}, {bits})") } else { raw };
                        format!("{}_from_raw({raw})", snake(&def.name))
                    }
                    TypeKind::Enum(e) => format!("{name}._decode(_bits.get({off}, {}))", e.backing_bits),
                    TypeKind::Struct(_) | TypeKind::Union(_) => {
                        format!("{name}._unpack_fixed(_bits, {off})")
                    }
                }
            }
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                let i = format!("_i{depth}");
                let elem_off = format!("{off} + {i} * {elem_bits}");
                let body = self.unpack_expr(elem, &elem_off, depth + 1);
                format!("[{body} for {i} in range({count})]")
            }
            // Read by the tail code; the fixed part leaves a placeholder.
            WireType::VarArray { .. } => "[]".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
        }
    }

    // ---------------------------------------------------------------------
    // Variable-length tails (§6.3)
    // ---------------------------------------------------------------------

    /// How a field's variable-length tail, if it has one, is laid out: inline
    /// in the containing dataclass, or owned by a named type that has tail
    /// methods of its own.
    fn tail_kind(&self, ty: &WireType) -> Option<TailKind> {
        match self.resolve(ty) {
            WireType::Str { .. } | WireType::VarArray { .. } => Some(TailKind::Inline),
            WireType::Named(id) if self.m.get(id).layout.tail.is_some() => Some(TailKind::Nested),
            _ => None,
        }
    }

    /// The trailing field that makes a struct variable-length, as the resolved
    /// type, the attribute holding it, and the label errors name it by.
    fn tail_of_struct(&self, def: &TypeDef, s: &Struct) -> Option<(WireType, String, String)> {
        if !def.layout.is_variable() {
            return None;
        }
        let f = s.fields.last()?;
        let name = f.name()?;
        self.tail_kind(&f.ty)?;
        Some((self.resolve(&f.ty), format!("self.{}", ident(name)), format!("{}.{name}", def.name)))
    }

    /// `_tail_len`, `_pack_tail` and `_unpack_tail` for a type owning a tail.
    fn tail_methods(&mut self, ty: &WireType, expr: &str, label: &str) {
        // -- length --
        self.line(1, "def _tail_len(self) -> int:");
        self.note(2, "Bytes this value's variable-length tail occupies. Internal.");
        let len_expr = match ty {
            WireType::Str { .. } => format!("len({expr}.encode(\"utf-8\"))"),
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                format!("len({expr}) * {bytes}")
            }
            _ => format!("{expr}._tail_len()"),
        };
        self.line(2, &format!("return {len_expr}"));
        self.blank();

        // -- pack --
        self.line(1, "def _pack_tail(self, _big: bool) -> bytes:");
        self.note(2, "The variable-length tail, which follows the fixed prefix. Internal.");
        let value = self.pack_tail_body(2, ty, expr, label, "_big");
        self.line(2, &format!("return {value}"));
        self.blank();

        // -- unpack --
        self.line(1, "def _unpack_tail(self, _data: bytes, _big: bool) -> None:");
        self.note(2, "Reads the tail; its length is what the transport delivered (§6.3). Internal.");
        match ty {
            WireType::Str { max } => {
                self.line(2, &format!("if len(_data) > {max}:"));
                self.raise(
                    3,
                    "DefgenLengthError",
                    &format!("f\"{label}: {{len(_data)}} bytes exceeds the maximum of {max}\""),
                );
                self.line(2, &format!("{expr} = _decode_utf8(_data, \"{label}\")"));
            }
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                let py = self.py_type(elem);
                // A remainder means the bytes on the wire do not correspond to
                // a whole number of elements, which §6.3 makes a hard error.
                self.line(2, &format!("if len(_data) % {bytes} != 0:"));
                self.raise(
                    3,
                    "DefgenLengthError",
                    &format!(
                        "f\"{label}: {{len(_data)}} bytes is not a whole number of {bytes}-byte elements\""
                    ),
                );
                self.line(2, &format!("_count = len(_data) // {bytes}"));
                self.line(2, &format!("if _count > {max}:"));
                self.raise(
                    3,
                    "DefgenLengthError",
                    &format!("f\"{label}: {{_count}} elements exceeds the maximum of {max}\""),
                );
                self.line(2, &format!("_values: list[{py}] = []"));
                self.line(2, "for _i0 in range(_count):");
                self.line(
                    3,
                    &format!("_bits = _Bits.from_bytes(_data[_i0 * {bytes} : (_i0 + 1) * {bytes}], _big)"),
                );
                let elem_expr = self.unpack_expr(elem, "0", 1);
                self.line(3, &format!("_values.append({elem_expr})"));
                self.line(2, &format!("{expr} = _values"));
            }
            _ => self.line(2, &format!("{expr}._unpack_tail(_data, _big)")),
        }
        self.blank();
    }

    /// Emits whatever statements a tail needs and returns the expression for
    /// its bytes. `big` is how the enclosing scope names its byte order — the
    /// `_big` parameter of a tail method, a literal in a module-level function.
    fn pack_tail_body(&mut self, ind: usize, ty: &WireType, expr: &str, label: &str, big: &str) -> String {
        match ty {
            WireType::Str { max } => format!("_encode_utf8({expr}, {max}, \"{label}\")"),
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(ind, "_out = bytearray()");
                self.line(ind, &format!("for _v0 in _check_max({expr}, {max}, \"{label}\"):"));
                self.line(ind + 1, &format!("_bits = _Bits({bytes}, big={big})"));
                let elem = (**elem).clone();
                self.pack(ind + 1, "_v0", &elem, "0", label, 1);
                self.line(ind + 1, "_out += _bits.to_bytes()");
                "bytes(_out)".to_string()
            }
            _ => format!("{expr}._pack_tail({big})"),
        }
    }

    // ---------------------------------------------------------------------
    // GATT metadata (§10)
    // ---------------------------------------------------------------------

    /// UUIDs and property sets as data. What a program does with them — which
    /// BLE library it hands them to — is deliberately out of scope (§10).
    fn gatt(&mut self) {
        let m = self.m;
        if m.services.is_empty() {
            return;
        }
        self.banner("GATT bindings");

        self.line(0, "class GattProperty(enum.Flag):");
        self.note(1, "GATT characteristic properties, as a flag set (§10).");
        self.blank();
        for p in Property::ALL {
            self.line(1, &format!("{} = enum.auto()", screaming(p.as_str())));
        }
        self.gap();

        self.line(0, "@dataclass(frozen=True, slots=True)");
        self.line(0, "class GattCharacteristic:");
        self.note(1, "One `characteristic` binding: a UUID, and what may be done with it (§10).");
        self.blank();
        self.lines(1, &["name: str", "uuid: str", "properties: GattProperty"]);
        self.gap();

        self.line(0, "@dataclass(frozen=True, slots=True)");
        self.line(0, "class GattService:");
        self.note(1, "One `service` declaration, and the characteristics under it (§10).");
        self.blank();
        self.lines(1, &["name: str", "uuid: str", "characteristics: tuple[GattCharacteristic, ...]"]);
        self.gap();

        for service in &m.services {
            let sprefix = screaming(&service.name);
            self.line(0, &format!("{sprefix}_UUID: Final = \"{}\"", service.uuid));
            self.docs_with(0, &service.docs, &[]);
            for c in &service.characteristics {
                let ty_name = ident(&m.get(c.ty).name);
                let size = match c.layout.tail {
                    None => format!("{} bytes", c.layout.fixed_bytes()),
                    Some(_) => format!("{}..{} bytes", c.layout.fixed_bytes(), c.layout.max_bytes()),
                };
                self.line(0, &format!("{sprefix}_{}_UUID: Final = \"{}\"", screaming(&c.name), c.uuid));
                let notes = vec![format!("Carries a `{ty_name}` ({size}).")];
                self.docs_with(0, &c.docs, &notes);
            }
            self.blank();

            self.line(0, &format!("{sprefix}: Final = GattService("));
            self.line(1, &format!("name=\"{}\",", service.name));
            self.line(1, &format!("uuid={sprefix}_UUID,"));
            self.line(1, "characteristics=(");
            for c in &service.characteristics {
                let props: Vec<String> =
                    c.properties.iter().map(|p| format!("GattProperty.{}", screaming(p.as_str()))).collect();
                let props = if props.is_empty() { "GattProperty(0)".to_string() } else { props.join(" | ") };
                self.line(2, "GattCharacteristic(");
                self.line(3, &format!("name=\"{}\",", c.name));
                self.line(3, &format!("uuid={sprefix}_{}_UUID,", screaming(&c.name)));
                self.line(3, &format!("properties={props},"));
                self.line(2, "),");
            }
            self.line(1, "),");
            self.line(0, ")");
            self.gap();
        }

        let names: Vec<String> = m.services.iter().map(|s| screaming(&s.name)).collect();
        // A one-element tuple needs its trailing comma to be a tuple at all.
        let trailing = if names.len() == 1 { "," } else { "" };
        self.line(0, &format!("SERVICES: Final[tuple[GattService, ...]] = ({}{trailing})", names.join(", ")));
        self.note(0, "Every service this schema declares, in source order.");
    }
}
