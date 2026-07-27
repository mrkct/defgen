//! The JavaScript backend: one self-contained ES module per schema.
//!
//! # Shape of the output
//!
//! Everything lands in a single `.mjs` — types, constants, codecs and GATT
//! metadata — so a project consumes generated code by copying one file in. The
//! extension is deliberate: a `.js` file is ESM or CommonJS depending on the
//! nearest `package.json`, and a file that ships on its own cannot rely on
//! one, while `.mjs` is a module in Node and in every bundler without any
//! accompanying configuration. Nothing outside the language and the two web
//! globals every runtime has — `TextEncoder`/`TextDecoder` — is used.
//!
//! There is no `.d.ts` alongside it and no build step: every declaration
//! carries a JSDoc block, so a project that runs `tsc --checkJs` (or just opens
//! the file in an editor) gets the same types a TypeScript port would, and a
//! project that does neither gets ordinary JavaScript.
//!
//! # Naming
//!
//! | Schema | JavaScript |
//! |---|---|
//! | `struct Status` | `class Status` |
//! | its codec | `status.encode()` / `Status.decode(data)` |
//! | its size | `Status.SIZE` |
//! | `enum HearingMode`'s `Stereo` | `HearingMode.Stereo` |
//! | field `active_profile` | `status.activeProfile` |
//! | `alias OwnerName`'s codec | `encodeOwnerName` / `decodeOwnerName` |
//!
//! A module-level name is either exported or private, and the ones that are not
//! exported are the ones a caller has no business reaching for. A method cannot
//! be hidden that way — a nested type is packed by its parent, which is a
//! different class — so the plumbing keeps JavaScript's own convention for
//! "internal": `_packFixed`, `_unpackFixed`, `_packTail`, `_unpackTail` and
//! `_tailLen` are leading-underscore, and a schema name shaped like one is
//! escaped so it cannot collide with them.
//!
//! # Representation choices
//!
//! * A `uN`/`iN` value is a `number` while its carrier (§2) fits in 32 bits,
//!   and a `bigint` past that. That is where JavaScript itself draws the line:
//!   `number` is a double, so it holds every `u32` and `i32` exactly but not
//!   every `u64`, and `DataView` switches to `BigInt` at exactly the same
//!   point. Every value is range-checked against its *declared* width on
//!   encode — including a check that it is a whole number at all, since `1.5`
//!   is a perfectly ordinary `number` — and every `iN` is sign-extended from
//!   bit `N-1` on decode.
//! * A `struct` becomes a `class` whose constructor takes an options object in
//!   which every field has a zero default, so `new Status()` and
//!   `new Status({ volume: 3 })` are both usable. Fields are plain mutable
//!   properties.
//! * An `alias` (§3) and a `scaled` type (§4) generate no runtime type —
//!   JavaScript has no type alias — but do generate a `@typedef`, so the domain
//!   name survives for tooling as well as in the codec names.
//! * A plain `enum` becomes a frozen object of named constants, one per
//!   variant, plus a `@typedef` naming the value type. An *open* one (§5) also
//!   gets a `<Name><Else>` class carrying `raw`, and a `<Name>Value` typedef:
//!   an unrecognized wire value decodes to that class, never to a declared
//!   variant, and `instanceof` is what tells them apart. A *closed* one throws
//!   on an unmatched value, on encode as well as decode.
//! * A tagged union (§7) becomes a class hierarchy — an abstract base holding
//!   the codec, one subclass per variant, and, for an open union, a
//!   `<Name><Else>` carrying the unrecognized id together with the undecoded
//!   payload. The base refuses to be constructed directly, so every value is
//!   one of the variants, and decoding dispatches on the id.
//! * A variable-length field (§6.3) is a native `string` or `Array`, as §12
//!   asks. A `string` is encoded and decoded strictly: `TextDecoder` is
//!   constructed with `fatal: true` so malformed input fails rather than
//!   becoming replacement characters, and encode rejects a lone surrogate for
//!   the same reason — `TextEncoder` would silently substitute U+FFFD.
//! * Failures are exceptions, one class per kind under a common `DefgenError`,
//!   which is what a JavaScript caller can `catch` in one clause.
//!
//! # Bit and byte order
//!
//! A container's bits live in a single `bigint`, LSB-first, which is exactly
//! §6's packing rule and makes reading a field a shift and a mask — and a
//! `bigint` has no width ceiling, so the 128-bit maximum (§2) needs no special
//! case. Byte order (§8) then enters in one place only —
//! `DefgenBits.fromBytes` / `toBytes` — because a big-endian container is the
//! very same bit sequence read from the far end of the buffer. Nothing below an
//! entry point decides byte order for itself: it is threaded down as an
//! argument, as far as the variable-length tail, whose elements are each packed
//! as their own byte-multiple unit under the same order.

use super::{Backend, Generated, GeneratedFile, Options, camel, sanitize_stem, screaming, snake};
use crate::ast::{Docs, Endianness, FloatType, Property};
use crate::model::{
    Enum, Field, FieldRole, Model, Scaled, Struct, TypeDef, TypeKind, Union, WireType, carrier_bits,
    int_range,
};

pub struct JavaScriptBackend;

impl Backend for JavaScriptBackend {
    fn name(&self) -> &'static str {
        "javascript"
    }

    fn description(&self) -> &'static str {
        "a single self-contained, JSDoc-typed ES module (ES2022+)"
    }

    fn generate(&self, model: &Model, opts: &Options) -> Generated {
        let file = GeneratedFile {
            name: format!("{}.mjs", sanitize_stem(&opts.stem)),
            contents: Emitter::new(model, opts.source.as_deref()).run(),
        };
        Generated { files: vec![file] }
    }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

/// Every reserved word, including the ones only reserved in strict mode — a
/// module is always strict — and the literals `true`/`false`/`null`. A schema
/// field named `new` is perfectly legal (§1 does not reserve JavaScript's
/// vocabulary), but `this.new = …` is not, so a colliding name gets a trailing
/// `_`.
#[rustfmt::skip]
const JS_KEYWORDS: &[&str] = &[
    "arguments", "await", "break", "case", "catch", "class", "const", "continue", "debugger",
    "default", "delete", "do", "else", "enum", "eval", "export", "extends", "false", "finally",
    "for", "function", "if", "implements", "import", "in", "instanceof", "interface", "let", "new",
    "null", "package", "private", "protected", "public", "return", "static", "super", "switch",
    "this", "throw", "true", "try", "typeof", "var", "void", "while", "with", "yield",
];

/// Names the generated module uses itself, which a schema name has to stay off.
///
/// A property named `encode` would shadow the method that encodes it, and one
/// named `constructor` would replace the class's own — both silently. The
/// module-level names are here for the same reason one step up: a schema type
/// becomes a top-level `class` or `const`, so a type named `TextDecoder` would
/// shadow the global the UTF-8 helpers are built from.
///
/// Everything the emitter introduces itself is either `_`-prefixed or
/// `defgen`-prefixed, and both spellings are escaped by [`ident`] rather than
/// listed here.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    "constructor", "prototype", "encode", "decode", "encodedSize",
    "Array", "BigInt", "Boolean", "Error", "Number", "Object", "String", "Symbol",
    "TextDecoder", "TextEncoder", "Uint8Array", "RangeError", "TypeError",
    "globalThis", "undefined", "NaN", "Infinity",
];

/// A schema name as a JavaScript identifier, escaped where it would collide
/// with the language's or the module's own vocabulary.
fn ident(name: &str) -> String {
    let internal = name.starts_with('_');
    let runtime = name.starts_with("defgen") || name.starts_with("Defgen");
    if JS_KEYWORDS.contains(&name) || RESERVED.contains(&name) || internal || runtime {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// A field, property or parameter name: `active_profile` becomes
/// `activeProfile`, the convention JavaScript shares with Kotlin and Swift
/// (§12).
fn field_ident(name: &str) -> String {
    ident(&camel(name))
}

/// An enum member: `PascalCase`, which is what a frozen object standing in for
/// an `enum` uses in JavaScript and TypeScript alike — `HearingMode.Cinema`,
/// not `HearingMode.CINEMA`, which reads as a loose constant rather than as a
/// member of a set.
fn member_ident(name: &str) -> String {
    let camel = camel(name);
    let mut chars = camel.chars();
    let mut out = String::with_capacity(camel.len());
    if let Some(c) = chars.next() {
        out.extend(c.to_uppercase());
        out.push_str(chars.as_str());
    }
    ident(&out)
}

/// `encode` + `OwnerName` to `encodeOwnerName`.
fn verb(prefix: &str, name: &str) -> String {
    format!("{prefix}{}", member_ident(name))
}

/// `Temperature` to `temperatureFromRaw` and `temperatureToRaw` (§4).
fn scaled_fns(name: &str) -> (String, String) {
    let snake = snake(name);
    (field_ident(&format!("{snake}_from_raw")), field_ident(&format!("{snake}_to_raw")))
}

/// A `///` line as JSDoc content. A block comment ends at the first `*/`, so
/// that is the one sequence a doc comment cannot be allowed to write.
fn escape_doc(text: &str) -> String {
    text.replace("*/", "*\\/")
}

/// The lines of a `///` comment (§1, §12).
fn doc_lines(docs: &Docs) -> Vec<String> {
    docs.iter().map(|d| d.text.clone()).collect()
}

/// Wraps prose the backend wrote itself to `width` columns, indenting every
/// line after the first by `hang` — which is what a JSDoc tag's continuation
/// lines want, and what a plain sentence does not.
///
/// The schema author's own `///` lines are never touched: they were written
/// with the line breaks they have.
fn wrap(text: &str, width: usize, hang: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let indent = if out.is_empty() { 0 } else { hang };
        if !line.is_empty() && indent + line.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        } else if !out.is_empty() {
            line.push_str(&" ".repeat(hang));
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

/// A double-quoted JavaScript string literal.
fn str_lit(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `a` or `an` for a bit count about to be read aloud in a doc comment.
fn article(bits: u32) -> &'static str {
    match bits {
        8 | 11 | 18 | 80..=89 => "an",
        _ => "a",
    }
}

/// A `number` literal JavaScript reads back as the same value. Rust's shortest
/// representation round-trips; its spellings of the non-finite values are just
/// not JavaScript's.
fn float_lit(v: f64) -> String {
    if v.is_nan() {
        return "Number.NaN".to_string();
    }
    if v.is_infinite() {
        return format!("{}Infinity", if v < 0.0 { "-" } else { "" });
    }
    format!("{v:?}")
}

/// Whether a value `bits` wide is carried in a `bigint` rather than a `number`
/// (§2): `number` is a double, so it holds every value up to a 32-bit carrier
/// exactly and not one past it.
fn is_big(bits: u32) -> bool {
    carrier_bits(bits) > 32
}

/// An unsigned wire value as a literal in the carrier a field of `bits` uses.
fn uint_lit(value: u128, bits: u32) -> String {
    if is_big(bits) { format!("{value}n") } else { format!("{value}") }
}

/// The same, in hex — how a wire id reads in the schema and on the wire (§7).
fn hex_lit(value: u128, bits: u32) -> String {
    if is_big(bits) { format!("0x{value:x}n") } else { format!("0x{value:x}") }
}

/// A hex `bigint` literal, for the values that go straight into the bit
/// container and so are never carried in a `number`.
fn big_hex(value: u128) -> String {
    format!("0x{value:x}n")
}

/// A signed bound as a literal in the carrier a field of `bits` uses.
fn int_lit(value: i128, bits: u32) -> String {
    if is_big(bits) { format!("{value}n") } else { format!("{value}") }
}

/// `expr` — a `bigint` straight out of the bit container — as the carrier a
/// field of `bits` is exposed in.
fn from_big(expr: &str, bits: u32) -> String {
    if is_big(bits) { expr.to_string() } else { format!("Number({expr})") }
}

/// The exception hierarchy every generated module carries: one class per way a
/// value can fail to match the schema, under a base a caller can catch whole.
const ERRORS: &[(&str, &str, &str)] = &[
    ("DefgenError", "Error", "Base class for every encode or decode failure this module throws."),
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
    /// A native `string`/`Array` property of the containing class.
    Inline,
    /// A named type that owns the tail, and the methods that handle it.
    Nested,
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Runtime helpers the schema turned out to need. A module-level function
/// nobody calls is something a project's linter complains about, so none is
/// emitted unconditionally — and rather than predicting the need from the
/// model, [`Emitter::run`] emits the declarations *first*, into a buffer, so
/// these flags record what the emitter actually wrote.
#[derive(Default)]
struct Needs {
    /// A signed field, which is sign-extended on decode.
    signed: bool,
    /// An unsigned field, an enum, a union tag: anything range-checked as `uN`.
    unsigned: bool,
    /// A `scaled` type, which needs the rounding helper (§4).
    round: bool,
    /// An array of either kind, whose length check is generic in its element.
    sequences: bool,
    /// A `string`, which needs the UTF-8 helpers (§6.3).
    utf8: bool,
    /// A variable-length root, whose encoding is a prefix followed by a tail.
    concat: bool,
}

struct Emitter<'m> {
    m: &'m Model,
    out: String,
    source: Option<&'m str>,
    needs: Needs,
}

impl<'m> Emitter<'m> {
    fn new(m: &'m Model, source: Option<&'m str>) -> Emitter<'m> {
        Emitter { m, out: String::with_capacity(32 * 1024), source, needs: Needs::default() }
    }

    // -- output primitives --------------------------------------------------

    fn line(&mut self, ind: usize, text: &str) {
        for _ in 0..ind {
            self.out.push_str("  ");
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

    fn banner(&mut self, title: &str) {
        let rule = "-".repeat(74usize.saturating_sub(title.len()));
        self.blank();
        self.line(0, &format!("// {title} {rule}"));
        self.blank();
    }

    /// A JSDoc block: the one-line form where it fits and there are no tags,
    /// the conventional starred form otherwise.
    ///
    /// Returns whether anything was written, so a caller that would follow it
    /// with a blank line can skip that too.
    fn jsdoc(&mut self, ind: usize, lines: &[String]) -> bool {
        let text: Vec<String> = lines.iter().map(|l| escape_doc(l)).collect();
        match text.as_slice() {
            [] => return false,
            [only] if ind * 2 + only.len() + 7 <= 100 => {
                self.line(ind, &format!("/** {only} */"));
                return true;
            }
            _ => {}
        }
        self.line(ind, "/**");
        for l in &text {
            if l.is_empty() {
                self.line(ind, " *");
            } else {
                self.line(ind, &format!(" * {l}"));
            }
        }
        self.line(ind, " */");
        true
    }

    /// A JSDoc block the backend wrote itself, as one line.
    fn note(&mut self, ind: usize, text: &str) {
        let width = 86usize.saturating_sub(ind * 2 + 3);
        let lines = wrap(text, width, 0);
        self.jsdoc(ind, &lines);
    }

    /// A JSDoc block of the backend's own prose and tags, each wrapped — a tag
    /// with a hanging indent, so a continuation line cannot read as a new tag.
    fn block(&mut self, ind: usize, lines: &[String]) {
        let width = 86usize.saturating_sub(ind * 2 + 3);
        let wrapped: Vec<String> = lines
            .iter()
            .flat_map(|l| {
                let hang = if l.starts_with('@') { 2 } else { 0 };
                if l.is_empty() { vec![String::new()] } else { wrap(l, width, hang) }
            })
            .collect();
        self.jsdoc(ind, &wrapped);
    }

    /// The schema's own doc comment, then — after a blank line — whatever the
    /// backend has to say about the representation it chose, then any tags.
    /// Returns whether there was anything to say at all.
    fn docs_with(&mut self, ind: usize, docs: &Docs, notes: &[String], tags: &[String]) -> bool {
        let width = 86usize.saturating_sub(ind * 2 + 3);
        let mut lines = doc_lines(docs);
        if !notes.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.extend(notes.iter().flat_map(|n| wrap(n, width, 0)));
        }
        if !tags.is_empty() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.extend(tags.iter().flat_map(|t| wrap(t, width, 2)));
        }
        self.jsdoc(ind, &lines)
    }

    /// A `throw` of one of the module's errors, whose message is nearly always
    /// too long to sit on the `throw` line.
    fn throw(&mut self, ind: usize, error: &str, message: &str) {
        let one_line = format!("throw new {error}({message});");
        if ind * 2 + one_line.len() <= 100 {
            self.line(ind, &one_line);
            return;
        }
        self.line(ind, &format!("throw new {error}("));
        self.line(ind + 1, message);
        self.line(ind, ");");
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

    /// The JSDoc type a value of `ty` carries. Aliases are deliberately *not*
    /// resolved here: the domain name the author declared is the point of one.
    fn js_type(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) | WireType::Int(n) => {
                if is_big(*n) {
                    "bigint".to_string()
                } else {
                    "number".to_string()
                }
            }
            WireType::Bool => "boolean".to_string(),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                match &def.kind {
                    TypeKind::Enum(e) if e.is_open() => format!("{}Value", ident(&def.name)),
                    _ => ident(&def.name),
                }
            }
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => {
                format!("{}[]", self.js_type(elem))
            }
            WireType::Str { .. } => "string".to_string(),
        }
    }

    /// An expression building a fresh zero value of `ty`, used as a field's
    /// default so that every generated class is constructible with no arguments
    /// at all.
    fn fresh(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) | WireType::Int(n) => uint_lit(0, *n),
            WireType::Bool => "false".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
            WireType::VarArray { .. } => "[]".to_string(),
            WireType::Array { elem, count } => {
                format!("Array.from({{ length: {count} }}, () => {})", self.fresh(elem))
            }
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => self.fresh(&a.target),
                    TypeKind::Scaled(_) => "0".to_string(),
                    // An enum with no variants at all is a compile error (§11),
                    // so the fallback only stands in for an `else`-only one.
                    TypeKind::Enum(e) => match (e.variants.first(), &e.else_arm) {
                        (Some(v), _) => format!("{name}.{}", member_ident(&v.name)),
                        (None, Some(arm)) => format!("new {name}{}()", ident(&arm.name)),
                        (None, None) => "0".to_string(),
                    },
                    TypeKind::Union(u) => match (u.variants.first(), &u.else_arm) {
                        (Some(v), _) => format!("new {name}{}()", ident(&v.name)),
                        (None, Some(arm)) => format!("new {name}{}()", ident(&arm.name)),
                        (None, None) => format!("new {name}()"),
                    },
                    TypeKind::Struct(_) => format!("new {name}()"),
                }
            }
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
            WireType::Named(id) => self.m.get(*id).name.clone(),
            WireType::Array { elem, count } => format!("{}[{count}]", self.wire_str(elem)),
            WireType::VarArray { elem, max } => format!("{}[max: {max}]", self.wire_str(elem)),
            WireType::Str { max } => format!("string(max: {max})"),
        }
    }

    /// Whether a type's codec is a pair of module-level functions rather than a
    /// pair of methods: an `alias`, `scaled` type or `enum` is not a class, so
    /// there is nowhere to hang a method.
    fn has_entry_functions(&self, def: &TypeDef) -> bool {
        def.root && !matches!(def.kind, TypeKind::Struct(_) | TypeKind::Union(_))
    }

    // ---------------------------------------------------------------------
    // Top level
    // ---------------------------------------------------------------------

    /// The declarations are emitted first, into a buffer, and the runtime is
    /// written around them afterwards: which helpers the module needs is then
    /// a record of what was actually emitted rather than a second walk over the
    /// model that could disagree with the first.
    fn run(mut self) -> String {
        let mut body = String::new();
        std::mem::swap(&mut self.out, &mut body);
        self.declarations();
        self.gatt();
        std::mem::swap(&mut self.out, &mut body);

        self.file_header();
        self.runtime();
        self.out.push_str(&body);
        self.out
    }

    fn file_header(&mut self) {
        let from = match self.source {
            Some(path) => format!(" from `{path}`"),
            None => String::new(),
        };
        self.line(0, &format!("// Generated by defgen{from}. Do not edit."));
        self.line(0, "//");
        self.lines(
            0,
            &[
                "// Codecs for this schema's GATT values: LSB-first bit packing (§6), with byte",
                "// order applied once per root container (§8). Encoding produces a `Uint8Array`;",
                "// decoding takes the bytes the transport delivered. Anything the schema does",
                "// not allow throws a `DefgenError` subclass, rather than being quietly",
                "// truncated, wrapped or replaced.",
                "//",
                "// Only a type bound to a characteristic has `encode`/`decode`: byte order is a",
                "// property of the root container, so a type that is only ever nested has no",
                "// byte order of its own to be encoded in (§8).",
                "//",
                "// A standard ES module: no dependencies, no build step. The JSDoc blocks are",
                "// the type declarations — `tsc --checkJs` reads them, and so does an editor.",
            ],
        );
    }

    // ---------------------------------------------------------------------
    // Runtime
    // ---------------------------------------------------------------------

    fn runtime(&mut self) {
        self.banner("Errors");
        for (name, base, doc) in ERRORS {
            self.note(0, doc);
            self.line(0, &format!("export class {name} extends {base} {{"));
            self.note(1, "@param {string} message");
            self.line(1, "constructor(message) {");
            self.line(2, "super(message);");
            self.line(2, &format!("this.name = {};", str_lit(name)));
            self.line(1, "}");
            self.line(0, "}");
            self.blank();
        }

        self.banner("Runtime");
        self.lines(
            0,
            &[
                "/**",
                " * A container's bits as one `bigint`, packed LSB-first from bit 0 (§6).",
                " *",
                " * A `bigint` has no width ceiling, so the 128-bit maximum a field may declare",
                " * (§2) needs no special case. Byte order (§8) enters only where this meets",
                " * bytes: a big-endian container is the very same bit sequence read from the far",
                " * end of the buffer, so byte order is one argument to `fromBytes`/`toBytes`",
                " * rather than something every field has to know about.",
                " */",
                "class DefgenBits {",
                "  /** @param {bigint} [value] */",
                "  constructor(value = 0n) {",
                "    /** @type {bigint} */",
                "    this.value = value;",
                "  }",
                "",
                "  /**",
                "   * The bits of `data`, read in the given byte order.",
                "   *",
                "   * @param {Uint8Array} data",
                "   * @param {boolean} big",
                "   * @returns {DefgenBits}",
                "   */",
                "  static fromBytes(data, big) {",
                "    let value = 0n;",
                "    for (let i = 0; i < data.length; i++) {",
                "      const index = big ? data.length - 1 - i : i;",
                "      value |= BigInt(data[index]) << BigInt(8 * i);",
                "    }",
                "    return new DefgenBits(value);",
                "  }",
                "",
                "  /**",
                "   * Exactly `size` bytes, written in the given byte order.",
                "   *",
                "   * @param {number} size",
                "   * @param {boolean} big",
                "   * @returns {Uint8Array}",
                "   */",
                "  toBytes(size, big) {",
                "    const out = new Uint8Array(size);",
                "    let rest = this.value;",
                "    for (let i = 0; i < size; i++) {",
                "      out[big ? size - 1 - i : i] = Number(rest & 0xffn);",
                "      rest >>= 8n;",
                "    }",
                "    return out;",
                "  }",
                "",
                "  /**",
                "   * The `bits` bits starting at `off`.",
                "   *",
                "   * @param {number} off",
                "   * @param {number} bits",
                "   * @returns {bigint}",
                "   */",
                "  get(off, bits) {",
                "    return (this.value >> BigInt(off)) & ((1n << BigInt(bits)) - 1n);",
                "  }",
                "",
                "  /**",
                "   * Writes the low `bits` bits of `value` at `off`.",
                "   *",
                "   * @param {number} off",
                "   * @param {number} bits",
                "   * @param {bigint} value",
                "   * @returns {void}",
                "   */",
                "  put(off, bits, value) {",
                "    const mask = ((1n << BigInt(bits)) - 1n) << BigInt(off);",
                "    this.value = (this.value & ~mask) | ((value << BigInt(off)) & mask);",
                "  }",
                "}",
            ],
        );

        if self.needs.unsigned || self.needs.signed {
            self.blank();
            self.lines(
                0,
                &[
                    "/**",
                    " * A field value as the `bigint` the bit container speaks in.",
                    " *",
                    " * A JavaScript `number` is a double, so `1.5` and `NaN` reach an integer",
                    " * field as ordinary values rather than as type errors; they are rejected",
                    " * here, where every encoded integer passes through.",
                    " *",
                    " * @param {number|bigint} value",
                    " * @param {string} where",
                    " * @returns {bigint}",
                    " */",
                    "function defgenInt(value, where) {",
                    "  if (typeof value === \"bigint\") return value;",
                    "  if (typeof value === \"number\" && Number.isInteger(value)) return BigInt(value);",
                    "  throw new DefgenRangeError(`${where}: ${String(value)} is not an integer`);",
                    "}",
                ],
            );
        }

        if self.needs.unsigned {
            self.blank();
            self.lines(
                0,
                &[
                    "/**",
                    " * Range-checks a `uN` value: out of range is an error, never a truncation",
                    " * (§2).",
                    " *",
                    " * @param {number|bigint} value",
                    " * @param {number} bits",
                    " * @param {string} where",
                    " * @returns {bigint}",
                    " */",
                    "function defgenCheckUint(value, bits, where) {",
                    "  const raw = defgenInt(value, where);",
                    "  if (raw < 0n || raw >= 1n << BigInt(bits)) {",
                    "    throw new DefgenRangeError(`${where}: ${raw} does not fit in u${bits}`);",
                    "  }",
                    "  return raw;",
                    "}",
                ],
            );
        }

        if self.needs.signed {
            self.blank();
            self.lines(
                0,
                &[
                    "/**",
                    " * Range-checks an `iN` value and returns its two's-complement bits (§2).",
                    " *",
                    " * @param {number|bigint} value",
                    " * @param {number} bits",
                    " * @param {string} where",
                    " * @returns {bigint}",
                    " */",
                    "function defgenCheckInt(value, bits, where) {",
                    "  const raw = defgenInt(value, where);",
                    "  const limit = 1n << BigInt(bits - 1);",
                    "  if (raw < -limit || raw >= limit) {",
                    "    throw new DefgenRangeError(`${where}: ${raw} does not fit in i${bits}`);",
                    "  }",
                    "  return raw & ((1n << BigInt(bits)) - 1n);",
                    "}",
                    "",
                    "/**",
                    " * Sign-extends an `iN` value from bit N-1 (§2).",
                    " *",
                    " * @param {bigint} value",
                    " * @param {number} bits",
                    " * @returns {bigint}",
                    " */",
                    "function defgenSext(value, bits) {",
                    "  const sign = 1n << BigInt(bits - 1);",
                    "  return (value ^ sign) - sign;",
                    "}",
                ],
            );
        }

        if self.needs.sequences {
            self.blank();
            self.lines(
                0,
                &[
                    "/**",
                    " * A fixed-size array carries exactly `count` elements, always (§6.1).",
                    " *",
                    " * @template T",
                    " * @param {T[]} seq",
                    " * @param {number} count",
                    " * @param {string} where",
                    " * @returns {T[]}",
                    " */",
                    "function defgenCheckCount(seq, count, where) {",
                    "  if (!Array.isArray(seq) || seq.length !== count) {",
                    "    const got = Array.isArray(seq) ? seq.length : typeof seq;",
                    "    throw new DefgenRangeError(`${where}: expected exactly ${count} elements, got ${got}`);",
                    "  }",
                    "  return seq;",
                    "}",
                    "",
                    "/**",
                    " * A variable-length array carries at most `limit` elements (§6.3).",
                    " *",
                    " * @template T",
                    " * @param {T[]} seq",
                    " * @param {number} limit",
                    " * @param {string} where",
                    " * @returns {T[]}",
                    " */",
                    "function defgenCheckMax(seq, limit, where) {",
                    "  if (!Array.isArray(seq)) {",
                    "    throw new DefgenRangeError(`${where}: expected an array, got ${typeof seq}`);",
                    "  }",
                    "  if (seq.length > limit) {",
                    "    throw new DefgenRangeError(",
                    "      `${where}: ${seq.length} elements exceeds the maximum of ${limit}`,",
                    "    );",
                    "  }",
                    "  return seq;",
                    "}",
                ],
            );
        }

        if self.needs.utf8 {
            self.blank();
            self.lines(
                0,
                &[
                    "const defgenUtf8Encoder = new TextEncoder();",
                    "",
                    "// `fatal` is what makes decoding strict: malformed input throws instead of",
                    "// becoming U+FFFD, which would turn a transport bug into silently wrong data",
                    "// (§6.3). It rejects truncated sequences, overlong encodings and surrogates.",
                    "const defgenUtf8Decoder = new TextDecoder(\"utf-8\", { fatal: true });",
                    "",
                    "/**",
                    " * A `string` field's bytes, rejecting anything past its `max` (§6.3).",
                    " *",
                    " * An unpaired surrogate is rejected rather than encoded: `TextEncoder` would",
                    " * substitute U+FFFD for it, which is the same silent replacement decoding",
                    " * refuses to do. In a `u`-mode pattern a surrogate pair is one code point, so",
                    " * `\\p{Surrogate}` matches only the unpaired ones.",
                    " *",
                    " * @param {string} text",
                    " * @param {number} limit",
                    " * @param {string} where",
                    " * @returns {Uint8Array}",
                    " */",
                    "function defgenEncodeUtf8(text, limit, where) {",
                    "  if (typeof text !== \"string\") {",
                    "    throw new DefgenUtf8Error(`${where}: expected a string, got ${typeof text}`);",
                    "  }",
                    "  if (/\\p{Surrogate}/u.test(text)) {",
                    "    throw new DefgenUtf8Error(`${where}: the string contains an unpaired surrogate`);",
                    "  }",
                    "  const data = defgenUtf8Encoder.encode(text);",
                    "  if (data.length > limit) {",
                    "    throw new DefgenRangeError(",
                    "      `${where}: ${data.length} bytes exceeds the maximum of ${limit}`,",
                    "    );",
                    "  }",
                    "  return data;",
                    "}",
                    "",
                    "/**",
                    " * Decodes a `string` field, failing on malformed input rather than patching",
                    " * it up with replacement characters (§6.3).",
                    " *",
                    " * @param {Uint8Array} data",
                    " * @param {string} where",
                    " * @returns {string}",
                    " */",
                    "function defgenDecodeUtf8(data, where) {",
                    "  try {",
                    "    return defgenUtf8Decoder.decode(data);",
                    "  } catch {",
                    "    throw new DefgenUtf8Error(`${where}: the bytes are not well-formed UTF-8`);",
                    "  }",
                    "}",
                ],
            );
        }

        if self.needs.concat {
            self.blank();
            self.lines(
                0,
                &[
                    "/**",
                    " * A value's fixed prefix followed by its variable-length tail (§6.3).",
                    " *",
                    " * @param {Uint8Array} prefix",
                    " * @param {Uint8Array} tail",
                    " * @returns {Uint8Array}",
                    " */",
                    "function defgenConcat(prefix, tail) {",
                    "  const out = new Uint8Array(prefix.length + tail.length);",
                    "  out.set(prefix);",
                    "  out.set(tail, prefix.length);",
                    "  return out;",
                    "}",
                ],
            );
        }

        if self.needs.round {
            self.blank();
            self.lines(
                0,
                &[
                    "/**",
                    " * Rounds half away from zero, which is what C's `round()` does.",
                    " *",
                    " * The backends have to agree on a `scaled` value's raw integer down to the",
                    " * last unit (§4, §13), and `Math.round` rounds half *up* — -0.5 to 0, not to",
                    " * -1 — so it would disagree below zero. The C backend carries the mirror",
                    " * image of this function rather than calling `round()` from libm.",
                    " *",
                    " * `Math.trunc` is exact for every finite double, and subtracting that",
                    " * integer part back off is exact too, so the comparison below sees the true",
                    " * remainder. Adding 0.5 first would not: for the double just below 0.5, the",
                    " * addition itself rounds up to 1.",
                    " *",
                    " * @param {number} value",
                    " * @param {string} where",
                    " * @returns {number}",
                    " */",
                    "function defgenRound(value, where) {",
                    "  if (!Number.isFinite(value)) {",
                    "    throw new DefgenRangeError(`${where}: ${value} cannot be rounded to an integer`);",
                    "  }",
                    "  const whole = Math.trunc(value);",
                    "  const remainder = value - whole;",
                    "  if (remainder >= 0.5) return whole + 1;",
                    "  if (remainder <= -0.5) return whole - 1;",
                    "  return whole;",
                    "}",
                ],
            );
        }
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
    }

    // -- alias (§3) ---------------------------------------------------------

    fn declare_alias(&mut self, def: &'m TypeDef, target: &'m WireType) {
        let name = ident(&def.name);
        let ty = self.js_type(target);
        let notes = vec![format!(
            "`{}` (§3): a name for `{}`, with no runtime type of its own.",
            def.name,
            self.wire_str(target)
        )];
        self.docs_with(0, &def.docs, &notes, &[format!("@typedef {{{ty}}} {name}")]);
        self.blank();
    }

    // -- scaled (§4) --------------------------------------------------------

    fn declare_scaled(&mut self, def: &'m TypeDef, s: &'m Scaled) {
        self.needs.round = true;
        let name = ident(&def.name);
        let prefix = screaming(&def.name);
        let raw_ty = if is_big(s.raw_bits) { "bigint" } else { "number" };
        let raw = format!("{}{}", if s.signed { "i" } else { "u" }, s.raw_bits);
        let physical = match s.physical {
            FloatType::F32 => "f32",
            FloatType::F64 => "f64",
        };
        let (min, max) = int_range(s.raw_bits, s.signed);

        let notes = vec![
            format!("`{}` (§4): `physical = raw * scale + offset`, over a `{raw}` wire value.", def.name),
            format!(
                "The schema's physical type is `{physical}`; JavaScript has only the one \
                 number type, so both map to `number`."
            ),
        ];
        self.docs_with(0, &def.docs, &notes, &[format!("@typedef {{number}} {name}")]);
        self.blank();
        self.line(0, &format!("export const {prefix}_SCALE = {};", float_lit(s.scale)));
        self.line(0, &format!("export const {prefix}_OFFSET = {};", float_lit(s.offset)));
        self.blank();

        let (from_raw, to_raw) = scaled_fns(&def.name);
        self.block(
            0,
            &[
                "Decodes the raw wire integer into the physical value (§4).".to_string(),
                String::new(),
                format!("@param {{{raw_ty}}} raw"),
                format!("@returns {{{name}}}"),
            ],
        );
        self.line(0, &format!("export function {from_raw}(raw) {{"));
        let raw_num = if is_big(s.raw_bits) { "Number(raw)" } else { "raw" };
        self.line(1, &format!("return {raw_num} * {prefix}_SCALE + {prefix}_OFFSET;"));
        self.line(0, "}");
        self.blank();

        self.block(
            0,
            &[
                "Rounds `value` to the nearest raw wire integer (§4).".to_string(),
                String::new(),
                format!("Anything outside `{raw}`'s range is an error rather than a wraparound."),
                "The raw integer is reachable this way, so a caller can round-trip a".to_string(),
                "value without going through floating point at all.".to_string(),
                String::new(),
                format!("@param {{{name}}} value"),
                format!("@returns {{{raw_ty}}}"),
            ],
        );
        self.line(0, &format!("export function {to_raw}(value) {{"));
        // `defgenRound` rejects a non-finite value itself, which covers both a
        // NaN or infinity handed in by the caller and one the division produced.
        let rounded = format!("defgenRound((value - {prefix}_OFFSET) / {prefix}_SCALE, \"{}\")", def.name);
        let rounded = if is_big(s.raw_bits) { format!("BigInt({rounded})") } else { rounded };
        self.line(1, &format!("const raw = {rounded};"));
        self.line(
            1,
            &format!("if (raw < {} || raw > {}) {{", int_lit(min, s.raw_bits), uint_lit(max, s.raw_bits)),
        );
        self.throw(
            2,
            "DefgenRangeError",
            &format!("`{}: ${{value}} is out of range for {raw} (raw ${{raw}})`", def.name),
        );
        self.line(1, "}");
        self.line(1, "return raw;");
        self.line(0, "}");
        self.blank();
    }

    // -- plain enum (§5) ----------------------------------------------------

    fn declare_enum(&mut self, def: &'m TypeDef, e: &'m Enum) {
        self.needs.unsigned = true;
        let name = ident(&def.name);
        let bits = e.backing_bits;
        let num_ty = if is_big(bits) { "bigint" } else { "number" };
        let unknown = e.else_arm.as_ref().map(|arm| format!("{name}{}", ident(&arm.name)));
        let value_ty = match &unknown {
            Some(_) => format!("{name}Value"),
            None => name.clone(),
        };

        self.block(
            0,
            &[
                format!("A declared `{}` variant, as it is carried at run time (§5).", def.name),
                String::new(),
                format!("@typedef {{{num_ty}}} {name}"),
            ],
        );
        self.blank();

        let mut notes = vec![
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
        notes.push("The object is frozen, so the set cannot be added to at run time.".to_string());
        self.docs_with(0, &def.docs, &notes, &[]);
        self.line(0, &format!("export const {name} = Object.freeze({{"));
        for v in &e.variants {
            self.jsdoc(1, &doc_lines(&v.docs));
            self.line(1, &format!("{}: {},", member_ident(&v.name), uint_lit(v.value, bits)));
        }
        self.line(0, "});");
        self.blank();

        // -- the fallback variant, and the type covering both cases (§5, §12) --
        if let Some(arm) = &e.else_arm {
            let unknown = format!("{name}{}", ident(&arm.name));
            let raw_ty = if is_big(arm.raw_bits) { "bigint" } else { "number" };
            let notes = vec![format!(
                "A wire value `{name}` does not declare (§5). It keeps the value it was \
                 decoded from, so re-encoding it is lossless."
            )];
            self.docs_with(0, &arm.docs, &notes, &[]);
            self.line(0, &format!("export class {unknown} {{"));
            self.block(
                1,
                &[
                    "The unrecognized wire value.".to_string(),
                    String::new(),
                    format!("@param {{{raw_ty}}} [raw]"),
                ],
            );
            self.line(1, &format!("constructor(raw = {}) {{", uint_lit(0, arm.raw_bits)));
            self.line(2, &format!("/** @type {{{raw_ty}}} */"));
            self.line(2, "this.raw = raw;");
            self.line(2, "Object.freeze(this);");
            self.line(1, "}");
            self.line(0, "}");
            self.blank();

            self.block(
                0,
                &[
                    format!("A declared `{name}` variant, or a value it does not declare (§5)."),
                    String::new(),
                    format!("@typedef {{{name} | {unknown}}} {name}Value"),
                ],
            );
            self.blank();
        }

        // -- the codecs, which are the module's business rather than a caller's --
        self.block(
            0,
            &[
                match &unknown {
                    Some(unknown) => format!("The variant `raw` names, or `{unknown}` (§5)."),
                    None => "The variant `raw` names; an unmatched value is an error (§5).".to_string(),
                },
                String::new(),
                format!("@param {{{num_ty}}} raw"),
                format!("@returns {{{value_ty}}}"),
            ],
        );
        self.line(0, &format!("function defgenDecode{}(raw) {{", member_ident(&def.name)));
        if e.variants.is_empty() {
            match &unknown {
                Some(unknown) => self.line(1, &format!("return new {unknown}(raw);")),
                None => self.throw(
                    1,
                    "DefgenUnknownValueError",
                    &format!("`{}: ${{raw}} matches no declared variant`", def.name),
                ),
            }
        } else {
            self.line(1, "switch (raw) {");
            for v in &e.variants {
                self.line(2, &format!("case {name}.{}:", member_ident(&v.name)));
            }
            self.line(3, "return raw;");
            self.line(2, "default:");
            match &unknown {
                Some(unknown) => self.line(3, &format!("return new {unknown}(raw);")),
                None => self.throw(
                    3,
                    "DefgenUnknownValueError",
                    &format!("`{}: ${{raw}} matches no declared variant`", def.name),
                ),
            }
            self.line(1, "}");
        }
        self.line(0, "}");
        self.blank();

        self.block(
            0,
            &[
                "The wire value `value` encodes to.".to_string(),
                String::new(),
                format!("@param {{{value_ty}}} value"),
                format!("@returns {{{num_ty}}}"),
            ],
        );
        self.line(0, &format!("function defgenEncode{}(value) {{", member_ident(&def.name)));
        match &unknown {
            Some(unknown) => {
                self.line(1, &format!("return value instanceof {unknown} ? value.raw : value;"));
            }
            None => {
                if e.variants.is_empty() {
                    self.throw(
                        1,
                        "DefgenUnknownValueError",
                        &format!("`{}: ${{value}} matches no declared variant`", def.name),
                    );
                } else {
                    self.line(1, "switch (value) {");
                    for v in &e.variants {
                        self.line(2, &format!("case {name}.{}:", member_ident(&v.name)));
                    }
                    self.line(3, "return value;");
                    self.line(2, "default:");
                    self.throw(
                        3,
                        "DefgenUnknownValueError",
                        &format!("`{}: ${{value}} matches no declared variant`", def.name),
                    );
                    self.line(1, "}");
                }
            }
        }
        self.line(0, "}");
        self.blank();
    }

    // -- tagged union (§7) --------------------------------------------------

    fn declare_union(&mut self, def: &'m TypeDef, u: &'m Union) {
        self.needs.unsigned = true;
        let name = ident(&def.name);
        let tag = field_ident(&u.tag_name);
        let (tag_bits, payload_bits) = (u.tag_bits, u.payload_bits);
        let tag_ty = if is_big(tag_bits) { "bigint" } else { "number" };
        let big = def.endian == Endianness::Big;
        let size = def.layout.fixed_bytes();

        // -- the base --
        let mut notes = vec![
            format!(
                "A tagged union (§7): {} {tag_bits}-bit `{}` in the container's low bits, then \
                 {} {payload_bits}-bit payload the id says how to read.",
                article(tag_bits),
                u.tag_name,
                article(payload_bits)
            ),
            format!(
                "Abstract: every value is one of the `{name}…` subclasses below, so a decoded \
                 command is matched with `instanceof`, never by inspecting a tag by hand."
            ),
        ];
        if !u.is_open() {
            notes.push("An id matching no variant is a hard decode error.".to_string());
        }
        self.docs_with(0, &def.docs, &notes, &[]);
        self.line(0, &format!("export class {name} {{"));
        self.note(1, &format!("Width of this union's container, in bits ({}).", def.layout.fixed_bits));
        self.line(1, &format!("static BIT_WIDTH = {};", def.layout.fixed_bits));
        self.blank();
        self.note(1, &format!("Encoded size of a `{}`, in bytes.", def.name));
        self.line(1, &format!("static SIZE = {size};"));
        self.blank();
        self.note(1, "Width of the id in the container's low bits (§7).");
        self.line(1, &format!("static TAG_BITS = {tag_bits};"));
        self.blank();

        self.note(1, "The base is abstract: only a variant can be constructed (§7).");
        self.line(1, "constructor() {");
        self.line(2, &format!("if (new.target === {name}) {{"));
        self.throw(
            3,
            "DefgenError",
            &format!("\"{}: abstract; construct one of its variants (§7)\"", def.name),
        );
        self.line(2, "}");
        self.line(1, "}");
        self.blank();

        if def.root {
            self.encode_method(&def.name, size, big, None);
            // `decode` is inherited by every variant, so it names the base
            // outright: going through `this` would skip the dispatch and read
            // one variant's payload whatever the id on the wire says.
            self.decode_method(&def.name, &name, size, big, Some(&name));
        }

        self.block(
            1,
            &[
                "Packs this variant, id included, at bit `off`. Internal.".to_string(),
                String::new(),
                "@param {DefgenBits} bits".to_string(),
                "@param {number} off".to_string(),
                "@returns {void}".to_string(),
            ],
        );
        self.line(1, "_packFixed(bits, off) {");
        self.throw(2, "DefgenError", &format!("\"{}: abstract\"", def.name));
        self.line(1, "}");
        self.blank();

        self.block(
            1,
            &[
                "Reads the id at `off` and dispatches to the variant it names (§7).".to_string(),
                String::new(),
                "@param {DefgenBits} bits".to_string(),
                "@param {number} off".to_string(),
                format!("@returns {{{name}}}"),
            ],
        );
        self.line(1, "static _unpackFixed(bits, off) {");
        self.line(2, &format!("const tag = {};", from_big(&format!("bits.get(off, {tag_bits})"), tag_bits)));
        self.line(2, "switch (tag) {");
        for v in &u.variants {
            self.line(3, &format!("case {}:", hex_lit(v.id, tag_bits)));
            self.line(4, &format!("return {name}{}._unpackPayload(bits, off);", ident(&v.name)));
        }
        self.line(3, "default:");
        match &u.else_arm {
            Some(arm) => {
                let unknown = format!("{name}{}", ident(&arm.name));
                let mut args = vec![format!("{tag}: tag")];
                if arm.raw_bits > 0 {
                    let get = format!("bits.get({}, {})", Self::off("off", tag_bits), arm.raw_bits);
                    args.push(format!("raw: {}", from_big(&get, arm.raw_bits)));
                }
                self.construct(4, &unknown, &args);
            }
            None => self.throw(
                4,
                "DefgenUnknownValueError",
                &format!("`{}: id ${{tag}} matches no declared variant`", def.name),
            ),
        }
        self.line(2, "}");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();

        // -- one class per variant --
        for v in &u.variants {
            let cls = format!("{name}{}", ident(&v.name));
            self.docs_with(0, &v.docs, &[format!("Wire id `0x{:x}` (§7).", v.id)], &[]);
            self.line(0, &format!("export class {cls} extends {name} {{"));
            self.note(1, "This variant's wire id (§7).");
            self.line(1, &format!("static ID = {};", hex_lit(v.id, tag_bits)));
            self.blank();

            let fields: Vec<&Field> = v.fields.iter().filter(|f| f.is_visible()).collect();
            self.constructor(1, &cls, &fields, true);

            self.block(
                1,
                &[
                    "Packs this variant, id included, at bit `off`. Internal.".to_string(),
                    String::new(),
                    "@param {DefgenBits} bits".to_string(),
                    "@param {number} off".to_string(),
                    "@returns {void}".to_string(),
                ],
            );
            self.line(1, "_packFixed(bits, off) {");
            self.line(2, &format!("bits.put(off, {tag_bits}, {});", big_hex(v.id)));
            for f in &v.fields {
                let Some(fname) = f.name() else { continue };
                let expr = format!("this.{}", field_ident(fname));
                let off = Self::off("off", tag_bits + f.offset_bits);
                let label = format!("{cls}.{fname}");
                self.pack(2, &expr, &f.ty, &off, &label, 0);
            }
            self.line(1, "}");
            self.blank();

            self.block(
                1,
                &[
                    "Reads this variant's payload; the id is already matched. Internal.".to_string(),
                    String::new(),
                    "@param {DefgenBits} bits".to_string(),
                    "@param {number} off".to_string(),
                    format!("@returns {{{cls}}}"),
                ],
            );
            self.line(1, "static _unpackPayload(bits, off) {");
            for f in &v.fields {
                self.padding_check(2, f, tag_bits, &cls);
            }
            let mut args: Vec<String> = Vec::new();
            for f in &v.fields {
                let Some(fname) = f.name() else { continue };
                let off = Self::off("off", tag_bits + f.offset_bits);
                let ty = f.ty.clone();
                args.push(format!("{}: {}", field_ident(fname), self.unpack_expr(&ty, &off, 0)));
            }
            self.construct(2, &cls, &args);
            self.line(1, "}");
            self.line(0, "}");
            self.blank();
        }

        // -- the fallback variant --
        if let Some(arm) = &u.else_arm {
            let cls = format!("{name}{}", ident(&arm.name));
            let raw_ty = if is_big(arm.raw_bits) { "bigint" } else { "number" };
            let notes = vec![format!(
                "An id `{name}` does not declare (§7). Both the id and the undecoded payload \
                 are kept, so re-encoding is lossless and an unknown command can never be \
                 mistaken for a known one."
            )];
            self.docs_with(0, &arm.docs, &notes, &[]);
            self.line(0, &format!("export class {cls} extends {name} {{"));
            let mut tags = vec!["@param {object} [init]".to_string()];
            tags.push(format!("@param {{{tag_ty}}} [init.{tag}] The unrecognized wire id."));
            if arm.raw_bits > 0 {
                tags.push(format!(
                    "@param {{{raw_ty}}} [init.raw] The {} payload bits, undecoded.",
                    arm.raw_bits
                ));
            }
            self.block(1, &tags);
            self.line(1, "constructor(init = {}) {");
            self.line(2, "super();");
            self.line(2, &format!("/** @type {{{tag_ty}}} */"));
            self.line(2, &format!("this.{tag} = init.{tag} ?? {};", uint_lit(0, tag_bits)));
            if arm.raw_bits > 0 {
                self.line(2, &format!("/** @type {{{raw_ty}}} */"));
                self.line(2, &format!("this.raw = init.raw ?? {};", uint_lit(0, arm.raw_bits)));
            }
            self.line(1, "}");
            self.blank();

            self.block(
                1,
                &[
                    "Writes the unrecognized id and its payload back verbatim. Internal.".to_string(),
                    String::new(),
                    "@param {DefgenBits} bits".to_string(),
                    "@param {number} off".to_string(),
                    "@returns {void}".to_string(),
                ],
            );
            self.line(1, "_packFixed(bits, off) {");
            self.line(
                2,
                &format!(
                    "bits.put(off, {tag_bits}, defgenCheckUint(this.{tag}, {tag_bits}, \"{cls}.{}\"));",
                    u.tag_name
                ),
            );
            if arm.raw_bits > 0 {
                let (off, bits) = (Self::off("off", tag_bits), arm.raw_bits);
                self.line(
                    2,
                    &format!("bits.put({off}, {bits}, defgenCheckUint(this.raw, {bits}, \"{cls}.raw\"));"),
                );
            }
            self.line(1, "}");
            self.line(0, "}");
            self.blank();
        }
    }

    // -- struct (§6) --------------------------------------------------------

    fn declare_struct(&mut self, def: &'m TypeDef, s: &'m Struct) {
        let name = ident(&def.name);
        let big = def.endian == Endianness::Big;

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
        self.docs_with(0, &def.docs, &notes, &[]);
        self.line(0, &format!("export class {name} {{"));
        if def.layout.is_variable() {
            self.note(1, "Bytes always present, before the variable-length tail (§6.3).");
            self.line(1, &format!("static FIXED_SIZE = {};", def.layout.fixed_bytes()));
            self.blank();
            self.note(1, "Largest legal encoding — what a receive buffer has to hold.");
            self.line(1, &format!("static MAX_SIZE = {};", def.layout.max_bytes()));
        } else {
            self.note(1, &format!("Width of a `{}` on the wire, in bits.", def.name));
            self.line(1, &format!("static BIT_WIDTH = {};", def.layout.fixed_bits));
            // A nested-only container is free to be sub-byte (§6); only a bound
            // one has to be a whole number of bytes (§10), so a byte size is
            // only stated where it is exact.
            if def.layout.is_byte_aligned() {
                self.blank();
                self.note(1, &format!("Encoded size of a `{}`, in bytes.", def.name));
                self.line(1, &format!("static SIZE = {};", def.layout.fixed_bytes()));
            }
        }
        self.blank();

        let fields: Vec<&Field> = s.fields.iter().filter(|f| f.is_visible()).collect();
        self.constructor(1, &name, &fields, false);

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
                    self.block(
                        1,
                        &[
                            "Bytes this value encodes to as it stands — never padded out (§6.3).".to_string(),
                            String::new(),
                            "@returns {number}".to_string(),
                        ],
                    );
                    self.line(1, "encodedSize() {");
                    self.line(2, &format!("return {name}.FIXED_SIZE + this._tailLen();"));
                    self.line(1, "}");
                    self.blank();
                }
            }
        }

        // -- the fixed part --
        self.block(
            1,
            &[
                "Packs the fixed part into `bits`, at bit `off`. Internal.".to_string(),
                String::new(),
                "@param {DefgenBits} bits".to_string(),
                "@param {number} off".to_string(),
                "@returns {void}".to_string(),
            ],
        );
        self.line(1, "_packFixed(bits, off) {");
        for f in &s.fields {
            let Some(fname) = f.name() else { continue };
            // An inline variable-length field contributes no fixed bits: the
            // tail methods are what write it.
            if matches!(self.tail_kind(&f.ty), Some(TailKind::Inline)) {
                continue;
            }
            let expr = format!("this.{}", field_ident(fname));
            let off = Self::off("off", f.offset_bits);
            let label = format!("{}.{fname}", def.name);
            self.pack(2, &expr, &f.ty, &off, &label, 0);
        }
        self.line(1, "}");
        self.blank();

        self.block(
            1,
            &[
                "Unpacks the fixed part from `bits`, at bit `off`. Internal.".to_string(),
                String::new(),
                "@param {DefgenBits} bits".to_string(),
                "@param {number} off".to_string(),
                format!("@returns {{{name}}}"),
            ],
        );
        self.line(1, "static _unpackFixed(bits, off) {");
        for f in &s.fields {
            self.padding_check(2, f, 0, &def.name);
        }
        let fixed: Vec<&Field> =
            s.fields.iter().filter(|f| !matches!(self.tail_kind(&f.ty), Some(TailKind::Inline))).collect();
        let mut args: Vec<String> = Vec::new();
        for f in fixed {
            let Some(fname) = f.name() else { continue };
            let off = Self::off("off", f.offset_bits);
            let ty = f.ty.clone();
            args.push(format!("{}: {}", field_ident(fname), self.unpack_expr(&ty, &off, 0)));
        }
        self.construct(2, &name, &args);
        self.line(1, "}");

        // -- the variable-length tail --
        if let Some((ty, expr, label)) = self.tail_of_struct(def, s) {
            self.blank();
            self.tail_methods(&ty, &expr, &label);
        }
        self.line(0, "}");
        self.blank();
    }

    /// `return new Name({ … });`, wrapped one property per line when it would
    /// otherwise run long.
    fn construct(&mut self, ind: usize, name: &str, args: &[String]) {
        if args.is_empty() {
            self.line(ind, &format!("return new {name}();"));
            return;
        }
        let one_line = format!("return new {name}({{ {} }});", args.join(", "));
        if ind * 2 + one_line.len() <= 100 {
            self.line(ind, &one_line);
            return;
        }
        self.line(ind, &format!("return new {name}({{"));
        for arg in args {
            self.line(ind + 1, &format!("{arg},"));
        }
        self.line(ind, "});");
    }

    /// The constructor: an options object in which every field has its zero
    /// value as a default, so `new Status()` is a usable value and
    /// `new Status({ volume: 3 })` says only what it means to say.
    ///
    /// A type with no fields at all gets none: the implicit constructor already
    /// does everything this one would, and an `init` nothing reads is exactly
    /// the unused parameter a linter objects to. `has_super` is true for a
    /// tagged-union variant, which has a base class to call.
    fn constructor(&mut self, ind: usize, owner: &str, fields: &[&'m Field], has_super: bool) {
        if fields.is_empty() {
            return;
        }
        let mut tags =
            vec!["@param {object} [init] Field values; every field has a zero default.".to_string()];
        for f in fields {
            let Some(fname) = f.name() else { continue };
            let ty = self.js_type(&f.ty);
            let doc = doc_lines(&f.docs).first().cloned().unwrap_or_default();
            let sep = if doc.is_empty() { "" } else { " " };
            tags.push(format!("@param {{{ty}}} [init.{}]{sep}{doc}", field_ident(fname)));
        }
        self.block(ind, &tags);
        self.line(ind, "constructor(init = {}) {");
        if has_super {
            self.line(ind + 1, "super();");
        }
        for f in fields {
            let Some(fname) = f.name() else { continue };
            self.field_member(ind + 1, f, fname, owner);
        }
        self.line(ind, "}");
        self.blank();
    }

    /// One property assignment, with its doc comment and — for a `reserved`
    /// field — the note that it round-trips rather than belonging to the caller
    /// (§6.2).
    fn field_member(&mut self, ind: usize, f: &'m Field, name: &str, owner: &str) {
        let ty = self.js_type(&f.ty);
        let js_name = field_ident(name);
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
        self.docs_with(ind, &f.docs, &notes, &[format!("@type {{{ty}}}")]);
        self.line(ind, &format!("this.{js_name} = init.{js_name} ?? {};", self.fresh(&f.ty)));
    }

    /// `padding: uN = 0` is validated on decode; bare `padding` is not (§6.2).
    fn padding_check(&mut self, ind: usize, f: &Field, base_bits: u32, owner: &str) {
        let FieldRole::Padding { check_zero: true } = f.role else { return };
        let off = Self::off("off", base_bits + f.offset_bits);
        let bits = f.layout.fixed_bits;
        let (from, to) = (f.offset_bits, f.offset_bits + bits);
        self.line(ind, &format!("if (bits.get({off}, {bits}) !== 0n) {{"));
        self.throw(
            ind + 1,
            "DefgenPaddingError",
            &format!("\"{owner}: padding at bits {from}..{to} is not zero\""),
        );
        self.line(ind, "}");
    }

    // ---------------------------------------------------------------------
    // Entry points (§12)
    // ---------------------------------------------------------------------

    fn encode_method(&mut self, schema_name: &str, size: u64, big: bool, max: Option<u128>) {
        let order = if big { "big" } else { "little" };
        let mut doc = match max {
            None => vec![format!(
                "Encodes this `{schema_name}` into exactly SIZE ({size}) bytes, {order}-endian."
            )],
            Some(max) => vec![
                format!("Encodes this `{schema_name}`, {order}-endian (§8)."),
                String::new(),
                format!("The result is the {size}-byte fixed prefix plus however many bytes"),
                format!("the tail actually needs — FIXED_SIZE to MAX_SIZE ({max}), never"),
                "padded out to the maximum (§6.3).".to_string(),
            ],
        };
        doc.push(String::new());
        doc.push("@returns {Uint8Array}".to_string());
        self.block(1, &doc);
        self.line(1, "encode() {");
        self.line(2, "const bits = new DefgenBits();");
        self.line(2, "this._packFixed(bits, 0);");
        let to_bytes = format!("bits.toBytes({size}, {big})");
        match max {
            None => self.line(2, &format!("return {to_bytes};")),
            Some(_) => {
                self.needs.concat = true;
                self.line(2, &format!("return defgenConcat({to_bytes}, this._packTail({big}));"));
            }
        }
        self.line(1, "}");
        self.blank();
    }

    /// `owner` names the class whose `_unpackFixed` to call, where a static
    /// method inherited by a subclass would otherwise resolve it to that
    /// subclass's.
    fn decode_method(&mut self, schema_name: &str, name: &str, size: u64, big: bool, owner: Option<&str>) {
        let owner = owner.unwrap_or(name);
        self.block(
            1,
            &[
                format!("Decodes exactly {size} bytes; any other length is an error."),
                String::new(),
                "@param {Uint8Array} data".to_string(),
                format!("@returns {{{name}}}"),
            ],
        );
        self.line(1, "static decode(data) {");
        self.line(2, &format!("if (data.length !== {size}) {{"));
        self.throw(
            3,
            "DefgenLengthError",
            &format!("`{schema_name}: expected {size} bytes, got ${{data.length}}`"),
        );
        self.line(2, "}");
        self.line(2, &format!("return {owner}._unpackFixed(DefgenBits.fromBytes(data, {big}), 0);"));
        self.line(1, "}");
        self.blank();
    }

    fn decode_var_method(&mut self, schema_name: &str, name: &str, big: bool) {
        self.block(
            1,
            &[
                "Decodes the bytes the transport delivered.".to_string(),
                String::new(),
                "The tail's length comes from `data.length`, never from the payload".to_string(),
                "itself (§6.3), so a length outside FIXED_SIZE..MAX_SIZE is an error.".to_string(),
                String::new(),
                "@param {Uint8Array} data".to_string(),
                format!("@returns {{{name}}}"),
            ],
        );
        self.line(1, "static decode(data) {");
        self.line(2, &format!("if (data.length < {name}.FIXED_SIZE || data.length > {name}.MAX_SIZE) {{"));
        self.throw(
            3,
            "DefgenLengthError",
            &format!(
                "`{schema_name}: expected ${{{name}.FIXED_SIZE}}..${{{name}.MAX_SIZE}} bytes, \
                 got ${{data.length}}`"
            ),
        );
        self.line(2, "}");
        self.line(
            2,
            &format!("const prefix = DefgenBits.fromBytes(data.subarray(0, {name}.FIXED_SIZE), {big});"),
        );
        self.line(2, &format!("const value = {name}._unpackFixed(prefix, 0);"));
        self.line(2, &format!("value._unpackTail(data.subarray({name}.FIXED_SIZE), {big});"));
        self.line(2, "return value;");
        self.line(1, "}");
        self.blank();
    }

    /// The module-level codec an `alias`, `scaled` or `enum` bound to a
    /// characteristic gets (§10): none of the three is a class, so there is
    /// nowhere to hang a method.
    fn entry_functions(&mut self, def: &'m TypeDef) {
        let name = def.name.clone();
        let prefix = screaming(&def.name);
        let big = def.endian == Endianness::Big;
        let order = def.endian.as_str();
        let size = def.layout.fixed_bytes();
        let ty = self.js_type(&WireType::Named(def.id));
        let target = self.resolve(&WireType::Named(def.id));
        let (encode, decode) = (verb("encode", &def.name), verb("decode", &def.name));

        if def.layout.tail.is_none() {
            self.note(0, &format!("Encoded size of a `{name}`, in bytes."));
            self.line(0, &format!("export const {prefix}_SIZE = {size};"));
            self.blank();

            self.block(
                0,
                &[
                    format!("Encodes a `{name}` into exactly {size} bytes, {order}-endian."),
                    String::new(),
                    format!("@param {{{ty}}} value"),
                    "@returns {Uint8Array}".to_string(),
                ],
            );
            self.line(0, &format!("export function {encode}(value) {{"));
            self.line(1, "const bits = new DefgenBits();");
            self.pack(1, "value", &target, "0", &name, 0);
            self.line(1, &format!("return bits.toBytes({size}, {big});"));
            self.line(0, "}");
            self.blank();

            self.block(
                0,
                &[
                    format!("Decodes exactly {size} bytes into a `{name}`."),
                    String::new(),
                    "@param {Uint8Array} data".to_string(),
                    format!("@returns {{{ty}}}"),
                ],
            );
            self.line(0, &format!("export function {decode}(data) {{"));
            self.line(1, &format!("if (data.length !== {size}) {{"));
            self.throw(
                2,
                "DefgenLengthError",
                &format!("`{name}: expected {size} bytes, got ${{data.length}}`"),
            );
            self.line(1, "}");
            self.line(1, &format!("const bits = DefgenBits.fromBytes(data, {big});"));
            let value = self.unpack_expr(&target, "0", 0);
            self.line(1, &format!("return {value};"));
            self.line(0, "}");
            self.blank();
            return;
        }

        self.note(0, "Bytes always present, before the variable-length tail (§6.3).");
        self.line(0, &format!("export const {prefix}_FIXED_SIZE = {size};"));
        self.blank();
        self.note(0, "Largest legal encoding — what a receive buffer has to hold.");
        self.line(0, &format!("export const {prefix}_MAX_SIZE = {};", def.layout.max_bytes()));
        self.blank();

        self.block(
            0,
            &[
                format!("Encodes a `{name}`, {order}-endian (§8)."),
                String::new(),
                "The encoding is exactly as long as the value is, never padded out to".to_string(),
                "the declared maximum (§6.3).".to_string(),
                String::new(),
                format!("@param {{{ty}}} value"),
                "@returns {Uint8Array}".to_string(),
            ],
        );
        self.line(0, &format!("export function {encode}(value) {{"));
        if size > 0 {
            self.needs.concat = true;
            self.line(1, "const bits = new DefgenBits();");
            self.pack(1, "value", &target, "0", &name, 0);
            self.line(1, &format!("const prefix = bits.toBytes({size}, {big});"));
            let tail = self.pack_tail_body(1, &target, "value", &name, &big.to_string());
            self.line(1, &format!("return defgenConcat(prefix, {tail});"));
        } else {
            let tail = self.pack_tail_body(1, &target, "value", &name, &big.to_string());
            self.line(1, &format!("return {tail};"));
        }
        self.line(0, "}");
        self.blank();

        self.block(
            0,
            &[
                format!("Decodes the bytes the transport delivered into a `{name}` (§6.3)."),
                String::new(),
                "@param {Uint8Array} data".to_string(),
                format!("@returns {{{ty}}}"),
            ],
        );
        self.line(0, &format!("export function {decode}(data) {{"));
        self.line(
            1,
            &format!("if (data.length < {prefix}_FIXED_SIZE || data.length > {prefix}_MAX_SIZE) {{"),
        );
        self.throw(
            2,
            "DefgenLengthError",
            &format!(
                "`{name}: expected ${{{prefix}_FIXED_SIZE}}..${{{prefix}_MAX_SIZE}} bytes, \
                 got ${{data.length}}`"
            ),
        );
        self.line(1, "}");
        self.unpack_alias_tail(1, &target, &name, &prefix, big);
        self.line(0, "}");
        self.blank();
    }

    /// The body of a `decode…` for a variable-length type bound straight to a
    /// characteristic (§6.3), which has no class to hold tail methods.
    fn unpack_alias_tail(&mut self, ind: usize, target: &WireType, name: &str, prefix: &str, big: bool) {
        match target {
            WireType::Str { .. } => {
                self.needs.utf8 = true;
                self.line(ind, &format!("return defgenDecodeUtf8(data, \"{name}\");"));
            }
            WireType::VarArray { elem, .. } => {
                self.needs.sequences = true;
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                let js = self.js_type(elem);
                self.line(ind, &format!("if (data.length % {bytes} !== 0) {{"));
                self.throw(
                    ind + 1,
                    "DefgenLengthError",
                    &format!(
                        "`{name}: ${{data.length}} bytes is not a whole number of {bytes}-byte elements`"
                    ),
                );
                self.line(ind, "}");
                self.line(ind, &format!("/** @type {{{js}[]}} */"));
                self.line(ind, "const values = [];");
                self.line(ind, &format!("for (let i0 = 0; i0 < data.length / {bytes}; i0++) {{"));
                self.line(
                    ind + 1,
                    &format!(
                        "const bits = DefgenBits.fromBytes(data.subarray(i0 * {bytes}, (i0 + 1) * {bytes}), {big});"
                    ),
                );
                let elem_expr = self.unpack_expr(elem, "0", 1);
                self.line(ind + 1, &format!("values.push({elem_expr});"));
                self.line(ind, "}");
                self.line(ind, "return values;");
            }
            // A named type that owns the tail: a variable-length struct.
            WireType::Named(id) => {
                let cls = ident(&self.m.get(*id).name);
                self.line(
                    ind,
                    &format!(
                        "const bits = DefgenBits.fromBytes(data.subarray(0, {prefix}_FIXED_SIZE), {big});"
                    ),
                );
                self.line(ind, &format!("const value = {cls}._unpackFixed(bits, 0);"));
                self.line(ind, &format!("value._unpackTail(data.subarray({prefix}_FIXED_SIZE), {big});"));
                self.line(ind, "return value;");
            }
            _ => self.line(ind, "return data;"),
        }
    }

    // ---------------------------------------------------------------------
    // Value-level emitters
    // ---------------------------------------------------------------------

    /// Emits the statements writing `expr` into `bits` at bit `off`.
    fn pack(&mut self, ind: usize, expr: &str, ty: &WireType, off: &str, label: &str, depth: usize) {
        match ty {
            WireType::UInt(n) => {
                self.needs.unsigned = true;
                self.line(ind, &format!("bits.put({off}, {n}, defgenCheckUint({expr}, {n}, \"{label}\"));"));
            }
            WireType::Int(n) => {
                self.needs.signed = true;
                self.line(ind, &format!("bits.put({off}, {n}, defgenCheckInt({expr}, {n}, \"{label}\"));"));
            }
            WireType::Bool => self.line(ind, &format!("bits.put({off}, 1, {expr} ? 1n : 0n);")),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                match &def.kind {
                    TypeKind::Alias(a) => {
                        let target = a.target.clone();
                        self.pack(ind, expr, &target, off, label, depth);
                    }
                    TypeKind::Scaled(s) => {
                        let check = if s.signed { "defgenCheckInt" } else { "defgenCheckUint" };
                        if s.signed {
                            self.needs.signed = true;
                        } else {
                            self.needs.unsigned = true;
                        }
                        let bits = s.raw_bits;
                        let (_, to_raw) = scaled_fns(&def.name);
                        let raw = format!("{to_raw}({expr})");
                        self.line(
                            ind,
                            &format!("bits.put({off}, {bits}, {check}({raw}, {bits}, \"{label}\"));"),
                        );
                    }
                    TypeKind::Enum(e) => {
                        self.needs.unsigned = true;
                        let bits = e.backing_bits;
                        let raw = format!("defgenEncode{}({expr})", member_ident(&def.name));
                        self.line(
                            ind,
                            &format!("bits.put({off}, {bits}, defgenCheckUint({raw}, {bits}, \"{label}\"));"),
                        );
                    }
                    TypeKind::Struct(_) | TypeKind::Union(_) => {
                        self.line(ind, &format!("{expr}._packFixed(bits, {off});"))
                    }
                }
            }
            WireType::Array { elem, count } => {
                self.needs.sequences = true;
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                let (i, items) = (format!("i{depth}"), format!("items{depth}"));
                // A block of its own, so two arrays in one container do not
                // declare the same `const` twice in the same scope.
                self.line(ind, "{");
                self.line(
                    ind + 1,
                    &format!("const {items} = defgenCheckCount({expr}, {count}, \"{label}\");"),
                );
                self.line(ind + 1, &format!("for (let {i} = 0; {i} < {count}; {i}++) {{"));
                let elem_off = format!("{off} + {i} * {elem_bits}");
                let elem_ty = (**elem).clone();
                self.pack(ind + 2, &format!("{items}[{i}]"), &elem_ty, &elem_off, label, depth + 1);
                self.line(ind + 1, "}");
                self.line(ind, "}");
            }
            // Written by the tail code, never as part of the fixed prefix.
            WireType::VarArray { .. } | WireType::Str { .. } => {}
        }
    }

    /// The expression reading a value of `ty` out of `bits` at bit `off`.
    ///
    /// Every case is a single expression — a fallible decode throws from inside
    /// the helper it calls — which is what lets a decoded value be built in one
    /// constructor call and an array in one `Array.from`.
    fn unpack_expr(&mut self, ty: &WireType, off: &str, depth: usize) -> String {
        match ty {
            WireType::UInt(n) => from_big(&format!("bits.get({off}, {n})"), *n),
            WireType::Int(n) => {
                self.needs.signed = true;
                from_big(&format!("defgenSext(bits.get({off}, {n}), {n})"), *n)
            }
            WireType::Bool => format!("bits.get({off}, 1) !== 0n"),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                match &def.kind {
                    TypeKind::Alias(a) => {
                        let target = a.target.clone();
                        self.unpack_expr(&target, off, depth)
                    }
                    TypeKind::Scaled(s) => {
                        let bits = s.raw_bits;
                        let raw = format!("bits.get({off}, {bits})");
                        let raw = if s.signed {
                            self.needs.signed = true;
                            format!("defgenSext({raw}, {bits})")
                        } else {
                            raw
                        };
                        let (from_raw, _) = scaled_fns(&def.name);
                        format!("{from_raw}({})", from_big(&raw, bits))
                    }
                    TypeKind::Enum(e) => {
                        let bits = e.backing_bits;
                        let raw = from_big(&format!("bits.get({off}, {bits})"), bits);
                        format!("defgenDecode{}({raw})", member_ident(&def.name))
                    }
                    TypeKind::Struct(_) | TypeKind::Union(_) => {
                        format!("{}._unpackFixed(bits, {off})", ident(&def.name))
                    }
                }
            }
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                let i = format!("i{depth}");
                let elem_off = format!("{off} + {i} * {elem_bits}");
                let elem_ty = (**elem).clone();
                let body = self.unpack_expr(&elem_ty, &elem_off, depth + 1);
                format!("Array.from({{ length: {count} }}, (_, {i}) => {body})")
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
    /// in the containing class, or owned by a named type that has tail methods
    /// of its own.
    fn tail_kind(&self, ty: &WireType) -> Option<TailKind> {
        match self.resolve(ty) {
            WireType::Str { .. } | WireType::VarArray { .. } => Some(TailKind::Inline),
            WireType::Named(id) if self.m.get(id).layout.tail.is_some() => Some(TailKind::Nested),
            _ => None,
        }
    }

    /// The trailing field that makes a struct variable-length, as the resolved
    /// type, the property holding it, and the label errors name it by.
    fn tail_of_struct(&self, def: &TypeDef, s: &Struct) -> Option<(WireType, String, String)> {
        if !def.layout.is_variable() {
            return None;
        }
        let f = s.fields.last()?;
        let name = f.name()?;
        self.tail_kind(&f.ty)?;
        Some((self.resolve(&f.ty), format!("this.{}", field_ident(name)), format!("{}.{name}", def.name)))
    }

    /// `_tailLen`, `_packTail` and `_unpackTail` for a type owning a tail.
    fn tail_methods(&mut self, ty: &WireType, expr: &str, label: &str) {
        // -- length --
        self.block(
            1,
            &[
                "Bytes this value's variable-length tail occupies. Internal.".to_string(),
                String::new(),
                "@returns {number}".to_string(),
            ],
        );
        self.line(1, "_tailLen() {");
        let len_expr = match ty {
            WireType::Str { .. } => {
                self.needs.utf8 = true;
                format!("defgenUtf8Encoder.encode({expr}).length")
            }
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                format!("{expr}.length * {bytes}")
            }
            _ => format!("{expr}._tailLen()"),
        };
        self.line(2, &format!("return {len_expr};"));
        self.line(1, "}");
        self.blank();

        // -- pack --
        self.block(
            1,
            &[
                "The variable-length tail, which follows the fixed prefix. Internal.".to_string(),
                String::new(),
                "@param {boolean} big".to_string(),
                "@returns {Uint8Array}".to_string(),
            ],
        );
        self.line(1, "_packTail(big) {");
        let value = self.pack_tail_body(2, ty, expr, label, "big");
        self.line(2, &format!("return {value};"));
        self.line(1, "}");
        self.blank();

        // -- unpack --
        self.block(
            1,
            &[
                "Reads the tail; its length is what the transport delivered (§6.3). Internal.".to_string(),
                String::new(),
                "@param {Uint8Array} data".to_string(),
                "@param {boolean} big".to_string(),
                "@returns {void}".to_string(),
            ],
        );
        self.line(1, "_unpackTail(data, big) {");
        match ty {
            WireType::Str { max } => {
                self.needs.utf8 = true;
                self.line(2, &format!("if (data.length > {max}) {{"));
                self.throw(
                    3,
                    "DefgenLengthError",
                    &format!("`{label}: ${{data.length}} bytes exceeds the maximum of {max}`"),
                );
                self.line(2, "}");
                self.line(2, &format!("{expr} = defgenDecodeUtf8(data, \"{label}\");"));
            }
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                let js = self.js_type(elem);
                // A remainder means the bytes on the wire do not correspond to
                // a whole number of elements, which §6.3 makes a hard error.
                self.line(2, &format!("if (data.length % {bytes} !== 0) {{"));
                self.throw(
                    3,
                    "DefgenLengthError",
                    &format!(
                        "`{label}: ${{data.length}} bytes is not a whole number of {bytes}-byte elements`"
                    ),
                );
                self.line(2, "}");
                self.line(2, &format!("const count = data.length / {bytes};"));
                self.line(2, &format!("if (count > {max}) {{"));
                self.throw(
                    3,
                    "DefgenLengthError",
                    &format!("`{label}: ${{count}} elements exceeds the maximum of {max}`"),
                );
                self.line(2, "}");
                self.line(2, &format!("/** @type {{{js}[]}} */"));
                self.line(2, "const values = [];");
                self.line(2, "for (let i0 = 0; i0 < count; i0++) {");
                self.line(
                    3,
                    &format!(
                        "const bits = DefgenBits.fromBytes(data.subarray(i0 * {bytes}, (i0 + 1) * {bytes}), big);"
                    ),
                );
                let elem_ty = (**elem).clone();
                let elem_expr = self.unpack_expr(&elem_ty, "0", 1);
                self.line(3, &format!("values.push({elem_expr});"));
                self.line(2, "}");
                self.line(2, &format!("{expr} = values;"));
            }
            _ => self.line(2, &format!("{expr}._unpackTail(data, big);")),
        }
        self.line(1, "}");
    }

    /// Emits whatever statements a tail needs and returns the expression for
    /// its bytes. `big` is how the enclosing scope names its byte order — the
    /// `big` parameter of a tail method, a literal in a module-level function.
    fn pack_tail_body(&mut self, ind: usize, ty: &WireType, expr: &str, label: &str, big: &str) -> String {
        match ty {
            WireType::Str { max } => {
                self.needs.utf8 = true;
                format!("defgenEncodeUtf8({expr}, {max}, \"{label}\")")
            }
            WireType::VarArray { elem, max } => {
                self.needs.sequences = true;
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(ind, &format!("const items = defgenCheckMax({expr}, {max}, \"{label}\");"));
                self.line(ind, &format!("const out = new Uint8Array(items.length * {bytes});"));
                self.line(ind, "for (let i0 = 0; i0 < items.length; i0++) {");
                self.line(ind + 1, "const bits = new DefgenBits();");
                let elem_ty = (**elem).clone();
                self.pack(ind + 1, "items[i0]", &elem_ty, "0", label, 1);
                self.line(ind + 1, &format!("out.set(bits.toBytes({bytes}, {big}), i0 * {bytes});"));
                self.line(ind, "}");
                "out".to_string()
            }
            _ => format!("{expr}._packTail({big})"),
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

        self.block(
            0,
            &[
                "GATT characteristic properties, as a bit flag set (§10).".to_string(),
                String::new(),
                "A characteristic's `properties` is the bitwise OR of these.".to_string(),
            ],
        );
        self.line(0, "export const GattProperty = Object.freeze({");
        for (i, p) in Property::ALL.iter().enumerate() {
            self.line(1, &format!("{}: 1 << {i},", screaming(p.as_str())));
        }
        self.line(0, "});");
        self.blank();

        self.block(
            0,
            &[
                "One `characteristic` binding: a UUID, and what may be done with it (§10).".to_string(),
                String::new(),
                "@typedef {object} GattCharacteristic".to_string(),
                "@property {string} name".to_string(),
                "@property {string} uuid".to_string(),
                "@property {number} properties A bitwise OR of `GattProperty` flags.".to_string(),
            ],
        );
        self.blank();
        self.block(
            0,
            &[
                "One `service` declaration, and the characteristics under it (§10).".to_string(),
                String::new(),
                "@typedef {object} GattService".to_string(),
                "@property {string} name".to_string(),
                "@property {string} uuid".to_string(),
                "@property {readonly GattCharacteristic[]} characteristics".to_string(),
            ],
        );
        self.blank();

        for service in &m.services {
            let sprefix = screaming(&service.name);
            self.docs_with(0, &service.docs, &[], &[]);
            self.line(0, &format!("export const {sprefix}_UUID = {};", str_lit(&service.uuid)));
            self.blank();
            for c in &service.characteristics {
                let ty_name = ident(&m.get(c.ty).name);
                let size = match c.layout.tail {
                    None => format!("{} bytes", c.layout.fixed_bytes()),
                    Some(_) => format!("{}..{} bytes", c.layout.fixed_bytes(), c.layout.max_bytes()),
                };
                let notes = vec![format!("Carries a `{ty_name}` ({size}).")];
                self.docs_with(0, &c.docs, &notes, &[]);
                self.line(
                    0,
                    &format!("export const {sprefix}_{}_UUID = {};", screaming(&c.name), str_lit(&c.uuid)),
                );
                self.blank();
            }

            self.note(0, "@type {GattService}");
            self.line(0, &format!("export const {sprefix} = Object.freeze({{"));
            self.line(1, &format!("name: {},", str_lit(&service.name)));
            self.line(1, &format!("uuid: {sprefix}_UUID,"));
            self.line(1, "characteristics: Object.freeze([");
            for c in &service.characteristics {
                let props: Vec<String> =
                    c.properties.iter().map(|p| format!("GattProperty.{}", screaming(p.as_str()))).collect();
                let props = if props.is_empty() { "0".to_string() } else { props.join(" | ") };
                self.line(2, "Object.freeze({");
                self.line(3, &format!("name: {},", str_lit(&c.name)));
                self.line(3, &format!("uuid: {sprefix}_{}_UUID,", screaming(&c.name)));
                self.line(3, &format!("properties: {props},"));
                self.line(2, "}),");
            }
            self.line(1, "]),");
            self.line(0, "});");
            self.blank();
        }

        let names: Vec<String> = m.services.iter().map(|s| screaming(&s.name)).collect();
        self.block(
            0,
            &[
                "Every service this schema declares, in source order.".to_string(),
                String::new(),
                "@type {readonly GattService[]}".to_string(),
            ],
        );
        self.line(0, &format!("export const SERVICES = Object.freeze([{}]);", names.join(", ")));
    }
}
