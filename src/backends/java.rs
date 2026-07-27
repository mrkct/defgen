//! The Java backend: one self-contained Java file per schema (Java 17+).
//!
//! # Shape of the output
//!
//! Java allows one public top-level type per file, so everything the schema
//! produces lands *inside* one final class named after the schema file —
//! `commands.defs` becomes `Commands.java`, holding `Commands.Status`,
//! `Commands.Command.SetVolume` and so on. That keeps the "drop one file into a
//! source set" property the other backends have, and it means two schemas
//! generated into the same package cannot collide: their runtimes, their error
//! classes and their types all live under their own outer class.
//!
//! There is no `package` declaration — where the file lands is the consuming
//! project's decision — and nothing outside the JDK is used: `BigInteger`
//! carries a container's bits, `BigDecimal` rounds a `scaled` value, and
//! `java.util`'s immutable collections carry arrays. The floor is Java 17:
//! records and sealed interfaces are what a struct and a tagged union map onto.
//!
//! # Naming
//!
//! | Schema | Java |
//! |---|---|
//! | `struct Status` | `record Status` |
//! | its codec | `status.encode()` / `Status.decode(data)` |
//! | its size | `Status.SIZE` |
//! | `enum HearingMode`'s `Stereo` | `HearingMode.Stereo` (open) or `HearingMode.STEREO` (closed) |
//! | field `active_profile` | `activeProfile()` |
//! | `alias OwnerName`'s codec | `encodeOwnerName` / `decodeOwnerName` |
//!
//! A member with no access modifier — `packFixed`, `unpackFixed`, `packTail`,
//! `unpackTail`, `tailLen` — is package-private: it exists because a nested
//! type is packed by its parent, not because a caller should reach for it.
//! Java gives an interface's members no such choice, so a tagged union's
//! plumbing is `public` in name only: it speaks in `DefgenBits`, which *is*
//! package-private, so no caller outside the file's package can even name the
//! argument type.
//!
//! # Representation choices
//!
//! * Java has no unsigned integer types, so — as §2 permits — a `uN` value is
//!   **widened** rather than wrapped: it is carried in the smallest signed Java
//!   integer that holds every value in `0..2^N - 1`. `u8` is a `short`, `u16`
//!   an `int`, `u32` a `long`, and `u64` a `BigInteger`, because no Java
//!   primitive holds `2^64 - 1`. The alternative — a `byte` holding `-56` for a
//!   `u8` of 200 — would put a number on a caller's field that is not the
//!   number on the wire, which is exactly the sort of quiet mismatch §0 exists
//!   to avoid. An `iN` needs no widening: it is a `byte`/`short`/`int`/`long`,
//!   or a `BigInteger` past 64 bits.
//! * Java has no type alias, so an `alias` (§3) and a `scaled` type (§4)
//!   generate no type of their own: `Volume` is a `short` like any other `u4`,
//!   `Temperature` is a `float`. The name survives on their constants and
//!   codecs — `TEMPERATURE_SCALE`, `temperatureFromRaw`, `encodeOwnerName` —
//!   and in the comment that says which wire type it stands for.
//! * A `struct` becomes a `record`: immutable, with `equals`/`hashCode`/
//!   `toString` supplied by the language. A no-argument constructor is
//!   generated alongside the canonical one, so `new Status()` is a usable zero
//!   value. Immutability is why decode reads a variable-length tail *before*
//!   constructing (§6.3): there is no half-built value to fill in afterwards.
//! * A plain *closed* `enum` (§5) becomes a Java `enum` carrying its wire value
//!   as a `raw()` accessor; decoding an unmatched value is a hard error. An
//!   *open* one becomes a `sealed interface` instead — one nested record per
//!   declared variant plus a nested `Unknown` carrying `raw` — so the type
//!   already covers every wire value, known or not, and `instanceof` patterns
//!   match it exhaustively.
//! * A tagged union (§7) is a `sealed interface` the same way: one nested
//!   record per declared id, and — for an open union — an `Unknown` carrying
//!   the unrecognized id together with the undecoded raw payload.
//! * A fixed or variable-length array (§6.1, §6.3) is an immutable `List` of
//!   the boxed element type, and a `string` is a `String`, as §12 asks. Decode
//!   fails on malformed UTF-8 rather than substituting replacement characters.
//! * Failures are **checked** exceptions under a common sealed `DefgenError`:
//!   malformed wire data is exactly the recoverable, caller's-problem condition
//!   checked exceptions are for, and one sealed base means a caller can still
//!   catch the lot with one `catch`.
//!
//! # Bit and byte order
//!
//! A container's bits live in one `BigInteger`, LSB-first from bit 0 — the same
//! design as the Kotlin and Python backends, chosen for the same reason:
//! `BigInteger` is exactly "an integer with no width ceiling", which is what a
//! container up to 4096 bits (§6) needs, and it is already in the JDK. Byte
//! order (§8) enters in one place only — `DefgenBits.fromBytes`/`toBytes` —
//! because a big-endian container is the very same bit sequence read from the
//! far end of the buffer. Every field, however narrow, is range-checked and
//! converted through `BigInteger`, which sidesteps every signed/unsigned
//! conversion pitfall the JVM has, at the cost of a `BigInteger` per field
//! access — a price this backend is happy to pay for never getting a
//! sign-extension bug wrong.

use super::{Backend, Generated, GeneratedFile, Options, camel, sanitize_stem, screaming};
use crate::ast::{Docs, Endianness, FloatType, Property};
use crate::model::{
    Const, Enum, Field, FieldRole, Model, Scaled, Struct, TypeDef, TypeKind, Union, UnionVariant, WireType,
    carrier_bits,
};

pub struct JavaBackend;

impl Backend for JavaBackend {
    fn name(&self) -> &'static str {
        "java"
    }

    fn description(&self) -> &'static str {
        "a single self-contained Java file (Java 17+)"
    }

    fn generate(&self, model: &Model, opts: &Options) -> Generated {
        let outer = class_name(&opts.stem, model);
        let file = GeneratedFile {
            name: format!("{outer}.java"),
            contents: Emitter::new(model, opts.source.as_deref(), outer).run(),
        };
        Generated { files: vec![file] }
    }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------
//
// [`snake`], [`screaming`] and [`camel`] (`activeProfile`, the member-naming
// convention this backend, Kotlin's and Swift's all share) are defined in
// [`super`] so every backend derives them the same way.

/// Every Java keyword, the three literals, and the contextual keywords a type
/// name could be confused with. A schema name shaped like one is perfectly
/// legal (§1 does not reserve Java's vocabulary), so a collision gets a
/// trailing `_`.
#[rustfmt::skip]
const JAVA_KEYWORDS: &[&str] = &[
    "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const",
    "continue", "default", "do", "double", "else", "enum", "extends", "final", "finally", "float",
    "for", "goto", "if", "implements", "import", "instanceof", "int", "interface", "long", "native",
    "new", "package", "private", "protected", "public", "return", "short", "static", "strictfp",
    "super", "switch", "synchronized", "this", "throw", "throws", "transient", "try", "void",
    "volatile", "while", "true", "false", "null",
    "record", "sealed", "permits", "var", "yield",
];

/// Names the generated file uses for its own runtime, for a member every
/// generated type carries, or for a local a generated method body declares. A
/// field named `encode` would collide with the method that encodes it; one
/// named `bits` would shadow the parameter its own packing statement reads; and
/// a *record component* may not be named after an `Object` method at all —
/// that is a compile error, not a shadowing warning.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    "encode", "decode", "packFixed", "unpackFixed", "unpackPayload", "packTail", "unpackTail",
    "tailLen", "encodedSize", "raw",
    "SIZE", "FIXED_SIZE", "MAX_SIZE", "ID",
    "clone", "equals", "finalize", "getClass", "hashCode", "notify", "notifyAll", "toString", "wait",
    "bits", "off", "data", "value", "big", "prefix", "tail", "out", "elems", "count", "checked",
    "physical", "magnitude", "i",
    "DefgenBits", "DefgenError", "GattProperty", "GattService", "GattCharacteristic",
    "SERVICES",
];

/// The schema's file stem as the one public class everything nests inside:
/// `commands` becomes `Commands`, `my-schema.v2` becomes `MySchemaV2`. Java
/// requires the file name to match the public class it holds, so this names the
/// file too.
///
/// A nested type may not repeat its enclosing class's simple name, and
/// `status.defs` declaring a `struct Status` is an ordinary thing to write — so
/// where the two collide it is the wrapper that yields, not the schema's own
/// name. The wrapper is an artifact of Java's one-public-class-per-file rule;
/// the type name is the author's.
fn class_name(stem: &str, model: &Model) -> String {
    let mut out = String::with_capacity(stem.len());
    for word in sanitize_stem(stem).split('_') {
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    let taken = |name: &str| model.types.iter().any(|t| t.name == name);
    if taken(&out) {
        out.push_str("Schema");
    }
    // Every step makes the name longer, and the schema declares finitely many.
    while taken(&out) {
        out.push('_');
    }
    out
}

/// `Temperature` to `temperature` — the one-letter change that turns a
/// `PascalCase` schema name into the camelCase prefix a generated method name
/// (`temperatureFromRaw`) wants, without touching the rest of the word the way
/// a full `camel()` re-split would.
fn lower_first(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A `///` line as Javadoc content. Javadoc is HTML, so the three characters
/// that would otherwise be read as markup are escaped, as is the one sequence
/// that would close the comment early — `*&#47;` renders as `*/` rather than
/// ending it.
fn escape_doc(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace("*/", "*&#47;")
}

/// `a` or `an` for a bit count read aloud in a doc comment.
fn article(bits: u32) -> &'static str {
    match bits {
        8 | 11 | 18 | 80..=89 => "an",
        _ => "a",
    }
}

// ---------------------------------------------------------------------------
// Carrier types (§2)
// ---------------------------------------------------------------------------

/// The Java type a `uN`/`iN` value of `bits` width is carried in.
///
/// Java has no unsigned integers, so a `uN` is widened to the smallest signed
/// type that holds all of `0..2^N - 1` — one bit more than the value itself
/// needs — which lands `u8` in a `short` and `u64` in a `BigInteger`. An `iN`
/// is already signed and takes the smallest type that holds `N` bits.
fn carrier_type(bits: u32, signed: bool) -> &'static str {
    let width = if signed { carrier_bits(bits) } else { carrier_bits(bits.saturating_add(1)) };
    match width {
        8 => "byte",
        16 => "short",
        32 => "int",
        64 => "long",
        _ => "BigInteger",
    }
}

/// The boxed form of a primitive carrier, for the element type of a `List`.
fn boxed(ty: &str) -> &str {
    match ty {
        "byte" => "Byte",
        "short" => "Short",
        "int" => "Integer",
        "long" => "Long",
        "float" => "Float",
        "double" => "Double",
        "boolean" => "Boolean",
        other => other,
    }
}

/// A zero-or-other literal of `carrier_type(bits, signed)`. `byte` and `short`
/// have no literal form of their own, so those go through an `int` literal and
/// a cast, which the compiler folds; `BigInteger` goes through a constant or
/// its exact string constructor, since no Java literal reaches 128 bits.
fn int_lit(value: i128, bits: u32, signed: bool) -> String {
    match carrier_type(bits, signed) {
        "byte" => format!("(byte) {value}"),
        "short" => format!("(short) {value}"),
        "int" => format!("{value}"),
        "long" => format!("{value}L"),
        _ if value == 0 => "BigInteger.ZERO".to_string(),
        _ => format!("new BigInteger(\"{value}\")"),
    }
}

/// An unsigned wire value — an enum variant's value, a tagged-union id — as a
/// literal of the carrier it is held in.
fn uint_lit(value: u128, bits: u32) -> String {
    match carrier_type(bits, false) {
        "byte" => format!("(byte) {value}"),
        "short" => format!("(short) {value}"),
        "int" => format!("{value}"),
        "long" => format!("{value}L"),
        _ if value == 0 => "BigInteger.ZERO".to_string(),
        _ => format!("new BigInteger(\"{value}\")"),
    }
}

/// `value` as a bare `BigInteger`, however wide — used where a constant (a
/// tagged-union id, most often) is written straight into a `DefgenBits`, with
/// no typed carrier in between. Always the string constructor, even for 0 and
/// 1: a wire id reads as the number it is, not as `BigInteger.ONE`.
fn bigint_lit(value: u128) -> String {
    format!("new BigInteger(\"{value}\")")
}

/// The expression converting a value already in `expr` (of
/// `carrier_type(bits, signed)`) into the `BigInteger` the bit container
/// speaks. Widening (§2) is what makes this a one-liner: every carrier holds
/// the value's true magnitude, so there is no sign bit to undo first.
fn to_bigint(expr: &str, bits: u32, signed: bool) -> String {
    match carrier_type(bits, signed) {
        "BigInteger" => expr.to_string(),
        _ => format!("BigInteger.valueOf({expr})"),
    }
}

/// The expression narrowing a `BigInteger` already known to be in range down to
/// `carrier_type(bits, signed)`.
fn from_bigint(expr: &str, bits: u32, signed: bool) -> String {
    match carrier_type(bits, signed) {
        "byte" => format!("{expr}.byteValue()"),
        "short" => format!("{expr}.shortValue()"),
        "int" => format!("{expr}.intValue()"),
        "long" => format!("{expr}.longValue()"),
        _ => expr.to_string(),
    }
}

/// `expr == <literal>`, spelled the way the carrier needs: a `BigInteger`
/// compares with `equals`, every primitive carrier with `==`.
fn eq_lit(expr: &str, value: u128, bits: u32) -> String {
    match carrier_type(bits, false) {
        "BigInteger" => format!("{expr}.equals({})", uint_lit(value, bits)),
        _ => format!("{expr} == {}", uint_lit(value, bits)),
    }
}

/// The same comparison where both sides are expressions rather than one being a
/// literal — the closed enum's lookup.
fn eq_expr(lhs: &str, rhs: &str, bits: u32) -> String {
    match carrier_type(bits, false) {
        "BigInteger" => format!("{lhs}.equals({rhs})"),
        _ => format!("{lhs} == {rhs}"),
    }
}

/// A `float`/`double` literal Java reads back as the same value. Rust's
/// shortest representation round-trips; only the suffix and the spelling of the
/// non-finite values differ from Rust's.
fn float_lit(v: f64, physical: FloatType) -> String {
    let single = physical == FloatType::F32;
    let ty = if single { "Float" } else { "Double" };
    if v.is_nan() {
        return format!("{ty}.NaN");
    }
    if v.is_infinite() {
        let sign = if v < 0.0 { "NEGATIVE" } else { "POSITIVE" };
        return format!("{ty}.{sign}_INFINITY");
    }
    let s = format!("{v:?}");
    let s = if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") };
    format!("{s}{}", if single { "f" } else { "" })
}

/// The Java type a raw `f32`/`f64` wire value is held in (§2).
fn float_java_type(f: FloatType) -> &'static str {
    match f {
        FloatType::F32 => "float",
        FloatType::F64 => "double",
    }
}

/// `expr`'s IEEE-754 bit pattern as a Java integer, plus its width. Widening
/// the `int` `floatToRawIntBits` returns into the `long` `BigInteger.valueOf`
/// wants sign-extends it, but `DefgenBits.put` masks to the low `bits` bits
/// before storing, so the sign-extended high bits are discarded and the
/// pattern survives exactly (§2, §8).
fn float_raw_bits_expr(f: FloatType, expr: &str) -> (String, u32) {
    match f {
        FloatType::F32 => (format!("Float.floatToRawIntBits({expr})"), 32),
        FloatType::F64 => (format!("Double.doubleToRawLongBits({expr})"), 64),
    }
}

/// The reverse of [`float_raw_bits_expr`]: reinterprets `bits`'s wire pattern
/// (a non-negative `BigInteger`) as `f`. `intValue`/`longValue` keep only the
/// low 32/64 bits, which is exactly the pattern to reinterpret.
fn float_from_bits_expr(f: FloatType, bits: &str) -> String {
    match f {
        FloatType::F32 => format!("Float.intBitsToFloat({bits}.intValue())"),
        FloatType::F64 => format!("Double.longBitsToDouble({bits}.longValue())"),
    }
}

/// `true`/`false` — for embedding a root's fixed byte order as a literal rather
/// than threading it as a parameter, wherever the call site is a static method
/// rather than an instance one.
fn java_bool(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}

/// Greedily wraps `text` to `budget` columns, for a doc comment the backend
/// wrote itself. A schema's own `///` lines are never re-wrapped: those are the
/// author's, and where they break is the author's choice.
fn wrap(text: &str, budget: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if !current.is_empty() && current.len() + 1 + word.len() > budget {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

// ---------------------------------------------------------------------------
// Variable-length tails
// ---------------------------------------------------------------------------

/// How a field's variable-length tail, if it has one, is laid out — see
/// [`Emitter::tail_kind`].
enum TailKind {
    /// A native `String`/`List` component of the containing record.
    Inline,
    /// A named type that owns the tail, and the methods that handle it.
    Nested,
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Imports and runtime helpers the schema turned out to need. An unused import
/// or helper is dead weight in a file meant to be dropped whole into a project,
/// so none of these is unconditional.
#[derive(Default)]
struct Needs {
    /// A `scaled` type, which needs `BigDecimal` for exact rounding (§4).
    round: bool,
    /// A fixed-size array (§6.1): an exact-count check, and a filled default.
    fixed_arrays: bool,
    /// A `Type[max: N]` (§6.3): a maximum-count check.
    var_arrays: bool,
    /// A `string`, which needs the UTF-8 helpers (§6.3).
    strings: bool,
    /// A variable-length type at all, whose encoding is a prefix plus a tail.
    tails: bool,
}

impl Needs {
    fn arrays(&self) -> bool {
        self.fixed_arrays || self.var_arrays
    }
}

struct Emitter<'m> {
    m: &'m Model,
    out: String,
    source: Option<&'m str>,
    /// The one public class everything nests inside, which also names the file.
    outer: String,
    needs: Needs,
}

impl<'m> Emitter<'m> {
    fn new(m: &'m Model, source: Option<&'m str>, outer: String) -> Emitter<'m> {
        let mut e =
            Emitter { m, out: String::with_capacity(48 * 1024), source, outer, needs: Needs::default() };
        e.scan();
        e
    }

    // -- output primitives ---------------------------------------------------

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

    fn banner(&mut self, ind: usize, title: &str) {
        let rule = "-".repeat(72usize.saturating_sub(title.len() + ind * 4));
        self.blank();
        self.line(ind, &format!("// {title} {rule}"));
        self.blank();
    }

    /// A schema name as a Java identifier, escaped where it would collide with
    /// the language's vocabulary or the generated file's own. The outer class
    /// is a backstop only — [`class_name`] already picks a wrapper no
    /// declaration is named after, since a nested type may not repeat its
    /// enclosing class's simple name.
    fn ident(&self, name: &str) -> String {
        if JAVA_KEYWORDS.contains(&name) || RESERVED.contains(&name) || name == self.outer {
            format!("{name}_")
        } else {
            name.to_string()
        }
    }

    /// A field name as the Java member it becomes: `camel`-cased, then escaped
    /// the same way a type name is.
    fn field_ident(&self, name: &str) -> String {
        self.ident(&camel(name))
    }

    /// Doc comments as Javadoc (§1, §12).
    fn docs(&mut self, ind: usize, docs: &Docs) {
        self.docs_with(ind, docs, &[]);
    }

    /// A Javadoc comment the backend wrote itself, wrapped to stay readable.
    fn note(&mut self, ind: usize, text: &str) {
        let budget = WIDTH.saturating_sub(ind * 4);
        if text.len() + 7 <= budget {
            self.line(ind, &format!("/** {text} */"));
            return;
        }
        let wrapped = wrap(text, budget.saturating_sub(3));
        self.line(ind, "/**");
        for line in wrapped {
            self.line(ind, &format!(" * {line}"));
        }
        self.line(ind, " */");
    }

    /// The schema's own doc comment, then — after a blank Javadoc line —
    /// whatever the backend has to say about the representation it chose.
    fn docs_with(&mut self, ind: usize, docs: &Docs, notes: &[String]) {
        self.docs_full(ind, docs, notes, &[]);
    }

    /// The same, plus the `@param` tags a record's components are documented
    /// with — Javadoc reads a component's documentation from nowhere else.
    fn docs_full(&mut self, ind: usize, docs: &Docs, notes: &[String], params: &[String]) {
        if docs.is_empty() && notes.is_empty() && params.is_empty() {
            return;
        }
        let budget = WIDTH.saturating_sub(ind * 4 + 3);
        // One short sentence of the backend's own is a one-liner, not a block.
        if docs.is_empty() && params.is_empty() && notes.len() == 1 {
            let note = notes[0].clone();
            self.note(ind, &note);
            return;
        }
        self.line(ind, "/**");
        for doc in docs {
            let text = escape_doc(&doc.text);
            if text.is_empty() {
                self.line(ind, " *");
            } else {
                self.line(ind, &format!(" * {text}"));
            }
        }
        if !docs.is_empty() && !notes.is_empty() {
            self.line(ind, " *");
        }
        for note in notes {
            for line in wrap(note, budget) {
                self.line(ind, &format!(" * {line}"));
            }
        }
        let had_prose = !docs.is_empty() || !notes.is_empty();
        if !params.is_empty() && had_prose {
            self.line(ind, " *");
        }
        for param in params {
            for (i, line) in wrap(param, budget).into_iter().enumerate() {
                let indent = if i == 0 { "" } else { "        " };
                self.line(ind, &format!(" * {indent}{line}"));
            }
        }
        self.line(ind, " */");
    }

    /// A note with no declaration to attach to — an `alias`, which generates no
    /// Java type at all (§3), so there is nothing for Javadoc to document.
    fn comment(&mut self, ind: usize, docs: &Docs, notes: &[String]) {
        let budget = WIDTH.saturating_sub(ind * 4 + 3);
        for doc in docs {
            self.line(ind, &format!("// {}", doc.text));
        }
        if !docs.is_empty() && !notes.is_empty() {
            self.line(ind, "//");
        }
        for note in notes {
            for line in wrap(note, budget) {
                self.line(ind, &format!("// {line}"));
            }
        }
    }

    // -- pre-pass -------------------------------------------------------------

    /// Works out which imports and helpers the schema needs, before emitting
    /// the header that has to declare them.
    fn scan(&mut self) {
        let m = self.m;
        for def in &m.types {
            self.needs.tails |= def.layout.is_variable();
            match &def.kind {
                TypeKind::Alias(a) => self.scan_type(&a.target),
                TypeKind::Scaled(_) => self.needs.round = true,
                TypeKind::Enum(_) => {}
                TypeKind::Union(u) => {
                    for f in u.variants.iter().flat_map(|v| &v.fields) {
                        self.scan_field(f);
                    }
                }
                TypeKind::Struct(s) => {
                    for f in &s.fields {
                        self.scan_field(f);
                    }
                }
            }
        }
    }

    fn scan_field(&mut self, f: &Field) {
        if matches!(f.role, FieldRole::Padding { .. }) {
            return;
        }
        self.scan_type(&f.ty);
    }

    fn scan_type(&mut self, ty: &WireType) {
        match ty {
            WireType::UInt(_)
            | WireType::Int(_)
            | WireType::Bool
            | WireType::Float(_)
            | WireType::Named(_) => {}
            WireType::Str { .. } => self.needs.strings = true,
            WireType::Array { elem, .. } => {
                self.needs.fixed_arrays = true;
                self.scan_type(elem);
            }
            WireType::VarArray { elem, .. } => {
                self.needs.var_arrays = true;
                self.scan_type(elem);
            }
        }
    }

    // -- type mapping ----------------------------------------------------------

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

    /// The Java type a value of `ty` is held in. Unlike the Kotlin backend,
    /// which keeps an `alias` as a `typealias`, this resolves them away: Java
    /// has no type alias for the domain name to survive in.
    fn java_type(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) => carrier_type(*n, false).to_string(),
            WireType::Int(n) => carrier_type(*n, true).to_string(),
            WireType::Bool => "boolean".to_string(),
            WireType::Float(f) => float_java_type(*f).to_string(),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                match &def.kind {
                    TypeKind::Alias(a) => self.java_type(&a.target),
                    TypeKind::Scaled(s) => match s.physical {
                        FloatType::F32 => "float".to_string(),
                        FloatType::F64 => "double".to_string(),
                    },
                    _ => self.ident(&def.name),
                }
            }
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => {
                format!("List<{}>", boxed(&self.java_type(elem)))
            }
            WireType::Str { .. } => "String".to_string(),
        }
    }

    /// An expression building a zero value of `ty`, used by the no-argument
    /// constructor every generated record carries. Nothing here is mutable —
    /// records, boxed numbers and the results of `List.of`/`nCopies` are all
    /// immutable — so sharing one instance between values is safe, unlike the
    /// mutable-default hazard a Python dataclass has to route around.
    fn fresh(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) => int_lit(0, *n, false),
            WireType::Int(n) => int_lit(0, *n, true),
            WireType::Bool => "false".to_string(),
            WireType::Float(FloatType::F32) => "0.0f".to_string(),
            WireType::Float(FloatType::F64) => "0.0".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
            WireType::VarArray { .. } => "List.of()".to_string(),
            WireType::Array { elem, count } => {
                format!("Collections.nCopies({count}, {})", self.fresh(elem))
            }
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = self.ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => self.fresh(&a.target),
                    TypeKind::Scaled(s) => match s.physical {
                        FloatType::F32 => "0.0f".to_string(),
                        FloatType::F64 => "0.0".to_string(),
                    },
                    TypeKind::Enum(e) => match (e.variants.first(), &e.else_arm) {
                        (Some(v), _) if e.is_open() => format!("new {name}.{}()", self.ident(&v.name)),
                        (Some(v), _) => format!("{name}.{}", screaming(&v.name)),
                        (None, Some(arm)) => format!(
                            "new {name}.{}({})",
                            self.ident(&arm.name),
                            int_lit(0, e.backing_bits, false)
                        ),
                        // An enum with neither variants nor an `else` arm is
                        // rejected by §11; a mutated schema can still reach
                        // codegen with one, and it has no value to name.
                        (None, None) => "null".to_string(),
                    },
                    TypeKind::Union(u) => match (u.variants.first(), &u.else_arm) {
                        (Some(v), _) => {
                            format!("new {name}.{}({})", self.ident(&v.name), self.fresh_fields(v))
                        }
                        (None, Some(arm)) => {
                            let tag = int_lit(0, u.tag_bits, false);
                            if arm.raw_bits > 0 {
                                format!(
                                    "new {name}.{}({tag}, {})",
                                    self.ident(&arm.name),
                                    int_lit(0, arm.raw_bits, false)
                                )
                            } else {
                                format!("new {name}.{}({tag})", self.ident(&arm.name))
                            }
                        }
                        (None, None) => "null".to_string(),
                    },
                    TypeKind::Struct(_) => format!("new {name}()"),
                }
            }
        }
    }

    /// The zero value of every visible field of a union variant, as an argument
    /// list: a record only has a no-argument constructor of its own when it has
    /// no components at all.
    fn fresh_fields(&self, v: &UnionVariant) -> String {
        let args: Vec<String> =
            v.fields.iter().filter(|f| f.is_visible()).map(|f| self.fresh(&f.ty)).collect();
        args.join(", ")
    }

    fn off(base: &str, delta: u32) -> String {
        if delta == 0 { base.to_string() } else { format!("({base} + {delta})") }
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
        self.line(0, "}");
        self.out
    }

    fn file_header(&mut self) {
        let from = match self.source {
            Some(path) => format!(" from `{path}`"),
            None => String::new(),
        };
        self.line(0, "/**");
        self.line(0, &format!(" * Generated by defgen{from}. Do not edit."));
        self.lines(
            0,
            &[
                " *",
                " * Codecs for this schema's GATT values: LSB-first bit packing (§6), with byte",
                " * order applied once per root container (§8). Encoding produces a byte array;",
                " * decoding takes the bytes the transport delivered. Anything the schema does not",
                " * allow throws a DefgenError subclass, rather than being quietly truncated,",
                " * wrapped or replaced.",
                " *",
                " * Only a type bound to a characteristic has encode/decode: byte order is a",
                " * property of the root container, so a type that is only ever nested has no byte",
                " * order of its own to be encoded in (§8).",
                " *",
                " * Requires Java 17 or newer. There are no third-party dependencies.",
                " */",
                "",
                "import java.math.BigInteger;",
            ],
        );
        if self.needs.round {
            self.line(0, "import java.math.BigDecimal;");
        }
        if self.needs.arrays() {
            self.line(0, "import java.util.ArrayList;");
        }
        if self.needs.tails {
            self.line(0, "import java.util.Arrays;");
        }
        if self.needs.fixed_arrays {
            self.line(0, "import java.util.Collections;");
        }
        if self.needs.arrays() || !self.m.services.is_empty() {
            self.line(0, "import java.util.List;");
        }
        if !self.m.services.is_empty() {
            self.line(0, "import java.util.Set;");
        }
        self.blank();

        let outer = self.outer.clone();
        self.note(0, "Every type, codec and binding this schema declares.");
        self.line(0, &format!("public final class {outer} {{"));
        self.blank();
        self.note(1, "A namespace, not a value: there is nothing here to construct.");
        self.line(1, &format!("private {outer}() {{"));
        self.line(1, "}");
    }

    // ---------------------------------------------------------------------
    // Runtime
    // ---------------------------------------------------------------------

    fn runtime(&mut self) {
        self.banner(1, "Errors");
        self.note(
            1,
            "Anything the schema does not allow, on either side of the wire. Sealed, so one \
             `catch` covers every failure a codec can report; checked, because malformed wire data \
             is the caller's to handle, not a bug to crash on.",
        );
        self.lines(
            1,
            &[
                "public abstract static sealed class DefgenError extends Exception {",
                "    private static final long serialVersionUID = 1L;",
                "",
                "    DefgenError(String message) {",
                "        super(message);",
                "    }",
                "}",
            ],
        );
        self.blank();
        for (name, doc) in [
            ("DefgenLengthError", "The buffer's length matches no legal encoding of the type (§6.3, §10)."),
            ("DefgenRangeError", "A value does not fit the bits its field declares (§2, §4, §6.3)."),
            ("DefgenUnknownValueError", "A closed enum or tagged union met an undeclared value (§5, §7)."),
            ("DefgenPaddingError", "A `padding: uN = 0` run was not zero on the wire (§6.2)."),
            ("DefgenUtf8Error", "A `string` field's bytes are not well-formed UTF-8 (§6.3)."),
        ] {
            self.note(1, doc);
            self.line(1, &format!("public static final class {name} extends DefgenError {{"));
            self.line(2, "private static final long serialVersionUID = 1L;");
            self.blank();
            self.line(2, &format!("{name}(String message) {{"));
            self.line(3, "super(message);");
            self.line(2, "}");
            self.line(1, "}");
            self.blank();
        }

        self.banner(1, "Runtime");
        self.lines(
            1,
            &[
                "/**",
                " * A container's bits as one arbitrary-width integer, packed LSB-first from bit 0",
                " * (§6). The value is always non-negative.",
                " *",
                " * Byte order (§8) enters only where this meets a byte array: a big-endian",
                " * container is the very same bit sequence read from the far end of the buffer, so",
                " * byte order is one argument to fromBytes/toBytes rather than something every",
                " * field has to know about.",
                " *",
                " * Package-private, which is what makes every member that speaks in one internal:",
                " * a caller outside this file's package cannot name the type, so it cannot call",
                " * them however public they are declared.",
                " */",
                "static final class DefgenBits {",
                "    private BigInteger value;",
                "",
                "    DefgenBits() {",
                "        this.value = BigInteger.ZERO;",
                "    }",
                "",
                "    private DefgenBits(BigInteger value) {",
                "        this.value = value;",
                "    }",
                "",
                "    /** The bits of `data`, read in the given byte order. */",
                "    static DefgenBits fromBytes(byte[] data, boolean big) {",
                "        return new DefgenBits(new BigInteger(1, big ? data : reversed(data)));",
                "    }",
                "",
                "    /** Exactly `size` bytes, written in the given byte order. */",
                "    byte[] toBytes(int size, boolean big) {",
                "        byte[] magnitude = value.toByteArray();",
                "        byte[] out = new byte[size];",
                "        int copyLen = Math.min(magnitude.length, size);",
                "        System.arraycopy(magnitude, magnitude.length - copyLen, out, size - copyLen, copyLen);",
                "        return big ? out : reversed(out);",
                "    }",
                "",
                "    /** The `bits` bits starting at `off`. */",
                "    BigInteger get(int off, int bits) {",
                "        BigInteger mask = BigInteger.ONE.shiftLeft(bits).subtract(BigInteger.ONE);",
                "        return value.shiftRight(off).and(mask);",
                "    }",
                "",
                "    /** Writes the low `bits` bits of `v` at `off`. */",
                "    void put(int off, int bits, BigInteger v) {",
                "        BigInteger mask = BigInteger.ONE.shiftLeft(bits).subtract(BigInteger.ONE);",
                "        value = value.andNot(mask.shiftLeft(off)).or(v.and(mask).shiftLeft(off));",
                "    }",
                "",
                "    private static byte[] reversed(byte[] data) {",
                "        byte[] out = new byte[data.length];",
                "        for (int i = 0; i < data.length; i++) {",
                "            out[i] = data[data.length - 1 - i];",
                "        }",
                "        return out;",
                "    }",
                "}",
                "",
                "/** Sign-extends an `iN` value from bit N-1 (§2). */",
                "static BigInteger defgenSext(BigInteger value, int bits) {",
                "    BigInteger sign = BigInteger.ONE.shiftLeft(bits - 1);",
                "    return value.xor(sign).subtract(sign);",
                "}",
                "",
                "/** Range-checks a `uN` value: out of range is an error, never a truncation (§2). */",
                "static BigInteger defgenCheckUInt(BigInteger value, int bits, String where)",
                "        throws DefgenRangeError {",
                "    BigInteger limit = BigInteger.ONE.shiftLeft(bits);",
                "    if (value.signum() < 0 || value.compareTo(limit) >= 0) {",
                "        throw new DefgenRangeError(where + \": \" + value + \" does not fit in u\" + bits);",
                "    }",
                "    return value;",
                "}",
                "",
                "/** Range-checks an `iN` value and returns its two's-complement bits (§2). */",
                "static BigInteger defgenCheckInt(BigInteger value, int bits, String where)",
                "        throws DefgenRangeError {",
                "    BigInteger limit = BigInteger.ONE.shiftLeft(bits - 1);",
                "    if (value.compareTo(limit.negate()) < 0 || value.compareTo(limit) >= 0) {",
                "        throw new DefgenRangeError(where + \": \" + value + \" does not fit in i\" + bits);",
                "    }",
                "    return value.signum() < 0 ? value.add(BigInteger.ONE.shiftLeft(bits)) : value;",
                "}",
            ],
        );

        if self.needs.fixed_arrays {
            self.blank();
            self.lines(
                1,
                &[
                    "/** A fixed-size array carries exactly `count` elements, always (§6.1). */",
                    "static <T> List<T> defgenCheckCount(List<T> seq, int count, String where)",
                    "        throws DefgenRangeError {",
                    "    if (seq.size() != count) {",
                    "        throw new DefgenRangeError(",
                    "                where + \": expected exactly \" + count + \" elements, got \" + seq.size());",
                    "    }",
                    "    return seq;",
                    "}",
                ],
            );
        }

        if self.needs.var_arrays {
            self.blank();
            self.lines(
                1,
                &[
                    "/** A variable-length array carries at most `limit` elements (§6.3). */",
                    "static <T> List<T> defgenCheckMax(List<T> seq, int limit, String where)",
                    "        throws DefgenRangeError {",
                    "    if (seq.size() > limit) {",
                    "        throw new DefgenRangeError(",
                    "                where + \": \" + seq.size() + \" elements exceeds the maximum of \" + limit);",
                    "    }",
                    "    return seq;",
                    "}",
                ],
            );
        }

        if self.needs.tails {
            self.blank();
            self.lines(
                1,
                &[
                    "/** A fixed prefix, then however many bytes the tail actually needs (§6.3). */",
                    "static byte[] defgenConcat(byte[] head, byte[] tail) {",
                    "    byte[] out = new byte[head.length + tail.length];",
                    "    System.arraycopy(head, 0, out, 0, head.length);",
                    "    System.arraycopy(tail, 0, out, head.length, tail.length);",
                    "    return out;",
                    "}",
                ],
            );
        }

        if self.needs.strings {
            self.blank();
            self.lines(
                1,
                &[
                    "/** A `string` field's bytes, rejecting anything past its `max` (§6.3). */",
                    "static byte[] defgenEncodeUtf8(String text, int limit, String where)",
                    "        throws DefgenRangeError {",
                    "    byte[] data = text.getBytes(java.nio.charset.StandardCharsets.UTF_8);",
                    "    if (data.length > limit) {",
                    "        throw new DefgenRangeError(",
                    "                where + \": \" + data.length + \" bytes exceeds the maximum of \" + limit);",
                    "    }",
                    "    return data;",
                    "}",
                    "",
                    "/**",
                    " * Decodes a `string` field. Malformed input fails rather than being patched up",
                    " * with replacement characters (§6.3) — which is exactly what the",
                    " * String(byte[], Charset) constructor would do instead.",
                    " */",
                    "static String defgenDecodeUtf8(byte[] data, String where) throws DefgenUtf8Error {",
                    "    java.nio.charset.CharsetDecoder decoder = java.nio.charset.StandardCharsets.UTF_8",
                    "            .newDecoder()",
                    "            .onMalformedInput(java.nio.charset.CodingErrorAction.REPORT)",
                    "            .onUnmappableCharacter(java.nio.charset.CodingErrorAction.REPORT);",
                    "    try {",
                    "        return decoder.decode(java.nio.ByteBuffer.wrap(data)).toString();",
                    "    } catch (java.nio.charset.CharacterCodingException exc) {",
                    "        throw new DefgenUtf8Error(where + \": \" + exc.getMessage());",
                    "    }",
                    "}",
                ],
            );
        }

        if self.needs.round {
            self.blank();
            self.lines(
                1,
                &[
                    "/**",
                    " * Rounds half away from zero, which is what C's round() does. Every backend has",
                    " * to agree on a `scaled` value's raw integer down to the last unit (§4, §13):",
                    " * Java has no built-in \"round half away from zero\", and the obvious Math.round",
                    " * rounds half *up*, which disagrees on every negative tie.",
                    " */",
                    "static BigInteger defgenRound(double value, String where) throws DefgenRangeError {",
                    "    if (value - value != 0.0) {",
                    "        // Infinite or NaN: every other double subtracts to zero. No integer",
                    "        // represents either, so this is where an out-of-range `scaled` division",
                    "        // lands, rather than in an arithmetic exception from BigDecimal.",
                    "        throw new DefgenRangeError(where + \": \" + value + \" cannot be rounded\");",
                    "    }",
                    "    BigInteger whole = new BigDecimal(value).toBigInteger();",
                    "    double remainder = value - whole.doubleValue();",
                    "    if (remainder >= 0.5) {",
                    "        return whole.add(BigInteger.ONE);",
                    "    }",
                    "    if (remainder <= -0.5) {",
                    "        return whole.subtract(BigInteger.ONE);",
                    "    }",
                    "    return whole;",
                    "}",
                ],
            );
        }
    }

    // ---------------------------------------------------------------------
    // Declarations
    // ---------------------------------------------------------------------

    /// Types in source order. §9 forbids forward references, so source order is
    /// already a valid declaration order — which matters here, since a
    /// `static final` field may only read one declared before it.
    fn declarations(&mut self) {
        self.banner(1, "Types");
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
            self.banner(1, "Constants");
            for c in &m.consts {
                self.declare_const(c);
            }
        }
    }

    // -- const (§3.1) -----------------------------------------------------------

    fn declare_const(&mut self, c: &Const) {
        let ty = carrier_type(c.bits, c.signed);
        let value = if c.signed { int_lit(c.as_i128(), c.bits, true) } else { uint_lit(c.magnitude, c.bits) };
        self.comment(1, &c.docs, &[]);
        self.line(1, &format!("public static final {ty} {} = {value};", screaming(&c.name)));
        self.blank();
    }

    /// Whether `def` is bound to a characteristic (§10) but has no type of its
    /// own to hang `encode`/`decode` off — an alias, a `scaled` type or an
    /// enum, all of which get static codec methods instead.
    fn has_entry_functions(&self, def: &TypeDef) -> bool {
        def.root && !matches!(def.kind, TypeKind::Struct(_) | TypeKind::Union(_))
    }

    // -- alias (§3) -----------------------------------------------------------

    /// An alias declares nothing: Java has no type alias, so what survives is
    /// the comment saying what the name stands for — plus, if it is bound to a
    /// characteristic, its own codec (§10).
    fn declare_alias(&mut self, def: &'m TypeDef, target: &'m WireType) {
        let notes = vec![format!(
            "`{}` (§3): a name for `{}`. Java has no type alias, so a value of it is held in a \
             `{}`, like any other `{}`.",
            def.name,
            self.wire_str(target),
            self.java_type(target),
            self.wire_str(target),
        )];
        self.comment(1, &def.docs, &notes);
        self.blank();
    }

    // -- scaled (§4) ------------------------------------------------------------

    fn declare_scaled(&mut self, def: &'m TypeDef, s: &'m Scaled) {
        let prefix = screaming(&def.name);
        let fnp = lower_first(&self.ident(&def.name));
        let physical = match s.physical {
            FloatType::F32 => "float",
            FloatType::F64 => "double",
        };
        let raw_ty = carrier_type(s.raw_bits, s.signed);
        let raw_wire = format!("{}{}", if s.signed { "i" } else { "u" }, s.raw_bits);

        let notes = vec![
            format!(
                "`{}` (§4): `physical = raw * scale + offset`, over a `{raw_wire}` wire value. Java \
                 has no type alias, so the physical value is held in a `{physical}`.",
                def.name
            ),
            format!(
                "The raw wire integer stays reachable through {fnp}ToRaw/{fnp}FromRaw, for callers \
                 that want to round-trip without floating-point rounding."
            ),
        ];
        self.comment(1, &def.docs, &notes);
        self.line(
            1,
            &format!("public static final {physical} {prefix}_SCALE = {};", float_lit(s.scale, s.physical)),
        );
        self.line(
            1,
            &format!("public static final {physical} {prefix}_OFFSET = {};", float_lit(s.offset, s.physical)),
        );
        self.blank();

        self.note(1, "Decodes the raw wire integer into the physical value (§4).");
        self.line(1, &format!("public static {physical} {fnp}FromRaw({raw_ty} raw) {{"));
        let as_bigint = to_bigint("raw", s.raw_bits, s.signed);
        let as_bigint = if s.signed { format!("defgenSext({as_bigint}, {})", s.raw_bits) } else { as_bigint };
        self.line(
            2,
            &format!("double physical = {as_bigint}.doubleValue() * {prefix}_SCALE + {prefix}_OFFSET;"),
        );
        let cast = if s.physical == FloatType::F32 { "(float) " } else { "" };
        self.line(2, &format!("return {cast}physical;"));
        self.line(1, "}");
        self.blank();

        self.docs_with(
            1,
            &Docs::new(),
            &[
                "Rounds `value` to the nearest raw wire integer (§4).".to_string(),
                format!("Anything outside `{raw_wire}`'s range is an error rather than a wraparound."),
            ],
        );
        self.line(1, &format!("public static {raw_ty} {fnp}ToRaw({physical} value) throws DefgenError {{"));
        // The division happens in `double` whatever the physical type is, so a
        // `float` is widened first. Saying so where the value is *already* a
        // `double` would be a redundant cast, which javac warns about.
        let widen = if s.physical == FloatType::F32 { "(double) " } else { "" };
        self.line(
            2,
            &format!(
                "BigInteger raw = defgenRound(({widen}value - {prefix}_OFFSET) / {prefix}_SCALE, \"{}\");",
                def.name
            ),
        );
        let check = if s.signed { "defgenCheckInt" } else { "defgenCheckUInt" };
        self.line(2, &format!("BigInteger checked = {check}(raw, {}, \"{}\");", s.raw_bits, def.name));
        self.line(2, &format!("return {};", from_bigint("checked", s.raw_bits, s.signed)));
        self.line(1, "}");
        self.blank();
    }

    // -- plain enum (§5) --------------------------------------------------------

    fn declare_enum(&mut self, def: &'m TypeDef, e: &'m Enum) {
        if e.is_open() {
            self.declare_open_enum(def, e);
        } else {
            self.declare_closed_enum(def, e);
        }
    }

    fn declare_closed_enum(&mut self, def: &'m TypeDef, e: &'m Enum) {
        let name = self.ident(&def.name);
        let bits = e.backing_bits;
        let carrier = carrier_type(bits, false);

        let notes = vec![
            format!("{} {bits}-bit wire value (§5).", article(bits).to_uppercase()),
            "Closed: a value matching none of the variants below is a hard error, on encode as \
             well as on decode."
                .to_string(),
        ];
        self.docs_with(1, &def.docs, &notes);
        self.line(1, &format!("public enum {name} {{"));
        // §11 rejects an enum with no variants, but a mutated schema can still
        // reach codegen with one, and `;` alone is how Java spells an empty
        // constant list ahead of a body.
        if e.variants.is_empty() {
            self.line(2, ";");
        }
        for (i, v) in e.variants.iter().enumerate() {
            self.docs(2, &v.docs);
            let sep = if i + 1 == e.variants.len() { ";" } else { "," };
            self.line(2, &format!("{}({}){sep}", screaming(&v.name), uint_lit(v.value, bits)));
        }
        self.blank();
        self.line(2, &format!("private final {carrier} raw;"));
        self.blank();
        self.line(2, &format!("{name}({carrier} raw) {{"));
        self.line(3, "this.raw = raw;");
        self.line(2, "}");
        self.blank();
        self.note(2, "The wire value this variant is spelled as (§5).");
        self.line(2, &format!("public {carrier} raw() {{"));
        self.line(3, "return raw;");
        self.line(2, "}");
        self.blank();
        self.note(2, "The variant `raw` names; an unmatched value is an error (§5).");
        self.line(2, &format!("static {name} decode({carrier} raw) throws DefgenError {{"));
        self.line(3, &format!("for ({name} variant : values()) {{"));
        self.line(4, &format!("if ({}) {{", eq_expr("variant.raw", "raw", bits)));
        self.line(5, "return variant;");
        self.line(4, "}");
        self.line(3, "}");
        self.raise(
            3,
            "DefgenUnknownValueError",
            &format!("\"{}: \" + raw + \" matches no declared variant\"", def.name),
        );
        self.line(2, "}");
        self.blank();
        self.note(2, "The wire value `value` encodes to.");
        self.line(2, &format!("static {carrier} encode({name} value) {{"));
        self.line(3, "return value.raw;");
        self.line(2, "}");
        self.line(1, "}");
        self.blank();
    }

    /// An *open* enum (§5) is a sealed interface rather than an `enum`: a
    /// nested record per declared variant, plus a nested `Unknown` for anything
    /// else. The hierarchy already answers "declared, or not" on its own —
    /// `HearingMode` is the type of every wire value, known or not — and being
    /// records, two `Default`s are equal to one another, the way two enum
    /// constants would be.
    fn declare_open_enum(&mut self, def: &'m TypeDef, e: &'m Enum) {
        let name = self.ident(&def.name);
        let bits = e.backing_bits;
        let carrier = carrier_type(bits, false);
        let arm = e.else_arm.as_ref().expect("declare_open_enum called on a closed enum");
        let unknown = self.ident(&arm.name);

        let notes = vec![
            format!("{} {bits}-bit wire value (§5).", article(bits).to_uppercase()),
            format!(
                "Open: a value matching none of the variants below decodes to `{name}.{}` instead \
                 of failing, so decoding this enum never fails.",
                arm.name
            ),
        ];
        self.docs_with(1, &def.docs, &notes);
        self.line(1, &format!("public sealed interface {name} {{"));
        self.note(2, "The wire value this variant carries.");
        self.line(2, &format!("{carrier} raw();"));
        self.blank();
        for v in &e.variants {
            self.docs(2, &v.docs);
            self.line(2, &format!("record {}() implements {name} {{", self.ident(&v.name)));
            self.line(3, "@Override");
            self.line(3, &format!("public {carrier} raw() {{"));
            self.line(4, &format!("return {};", uint_lit(v.value, bits)));
            self.line(3, "}");
            self.line(2, "}");
            self.blank();
        }
        self.docs_with(
            2,
            &arm.docs,
            &[format!(
                "A wire value `{name}` does not declare (§5). It keeps the value it was decoded \
                 from, so re-encoding it is lossless."
            )],
        );
        self.line(2, &format!("record {unknown}({carrier} raw) implements {name} {{"));
        self.line(2, "}");
        self.blank();
        self.note(2, &format!("The variant `raw` names, or `{name}.{}` (§5).", arm.name));
        self.line(2, &format!("public static {name} decode({carrier} raw) {{"));
        for v in &e.variants {
            self.line(3, &format!("if ({}) {{", eq_lit("raw", v.value, bits)));
            self.line(4, &format!("return new {}();", self.ident(&v.name)));
            self.line(3, "}");
        }
        self.line(3, &format!("return new {unknown}(raw);"));
        self.line(2, "}");
        self.blank();
        self.note(2, "The wire value `value` encodes to.");
        self.line(2, &format!("public static {carrier} encode({name} value) {{"));
        self.line(3, "return value.raw();");
        self.line(2, "}");
        self.line(1, "}");
        self.blank();
    }

    // -- tagged union (§7) -----------------------------------------------------

    fn declare_union(&mut self, def: &'m TypeDef, u: &'m Union) {
        let name = self.ident(&def.name);
        let tag_carrier = carrier_type(u.tag_bits, false);
        let (tag_bits, payload_bits) = (u.tag_bits, u.payload_bits);
        let big = java_bool(def.endian == Endianness::Big);
        let size = def.layout.fixed_bytes();

        let mut notes = vec![
            format!(
                "A tagged union (§7): {} {tag_bits}-bit `{}` in the container's low bits, then {} \
                 {payload_bits}-bit payload the id says how to read.",
                article(tag_bits),
                u.tag_name,
                article(payload_bits)
            ),
            format!(
                "Sealed: every value is one of `{name}`'s nested records, so a decoded command is \
                 matched with `instanceof`, never by inspecting a tag by hand."
            ),
        ];
        if !u.is_open() {
            notes.push("An id matching no variant is a hard decode error.".to_string());
        }
        self.docs_with(1, &def.docs, &notes);
        self.line(1, &format!("public sealed interface {name} {{"));

        // A tagged union's layout never has a tail (§7: the payload is a fixed
        // number of bits chosen by the container width), so unlike a struct
        // this is always a plain SIZE, never a FIXED_SIZE/MAX_SIZE pair. A
        // nested-only union is free to be sub-byte (§6); only a bound one has
        // to be a whole number of bytes (§10), so a byte size is only stated
        // where it is exact.
        if def.layout.is_byte_aligned() {
            self.note(2, &format!("Encoded size of a `{}`, in bytes.", def.name));
            self.line(2, &format!("int SIZE = {size};"));
            self.blank();
        }

        self.note(2, "Writes this value's bits into `bits`, at bit `off`. Internal.");
        self.line(2, "void packFixed(DefgenBits bits, int off) throws DefgenError;");
        self.blank();

        if def.root {
            self.note(
                2,
                &format!(
                    "Encodes this `{}` into exactly SIZE ({size}) bytes, {}-endian.",
                    def.name,
                    def.endian.as_str()
                ),
            );
            self.line(2, "default byte[] encode() throws DefgenError {");
            self.line(3, "DefgenBits bits = new DefgenBits();");
            self.line(3, "packFixed(bits, 0);");
            self.line(3, &format!("return bits.toBytes({size}, {big});"));
            self.line(2, "}");
            self.blank();

            self.note(2, &format!("Decodes exactly {size} bytes; any other length is an error."));
            self.line(2, &format!("static {name} decode(byte[] data) throws DefgenError {{"));
            self.line(3, &format!("if (data.length != {size}) {{"));
            self.raise(
                4,
                "DefgenLengthError",
                &format!("\"{}: expected {size} bytes, got \" + data.length", def.name),
            );
            self.line(3, "}");
            self.line(3, &format!("return unpackFixed(DefgenBits.fromBytes(data, {big}), 0);"));
            self.line(2, "}");
            self.blank();
        }

        self.note(2, "Reads the id at `off` and dispatches to the variant it names (§7). Internal.");
        self.line(2, &format!("static {name} unpackFixed(DefgenBits bits, int off) throws DefgenError {{"));
        let tag_expr = from_bigint(&format!("bits.get(off, {tag_bits})"), tag_bits, false);
        self.line(3, &format!("{tag_carrier} tag = {tag_expr};"));
        for v in &u.variants {
            self.line(3, &format!("if ({}) {{", eq_lit("tag", v.id, tag_bits)));
            self.line(4, &format!("return {}.unpackPayload(bits, off);", self.ident(&v.name)));
            self.line(3, "}");
        }
        match &u.else_arm {
            Some(arm) => {
                let unknown = self.ident(&arm.name);
                if arm.raw_bits > 0 {
                    let raw_off = Self::off("off", tag_bits);
                    let raw_expr =
                        from_bigint(&format!("bits.get({raw_off}, {})", arm.raw_bits), arm.raw_bits, false);
                    self.line(3, &format!("return new {unknown}(tag, {raw_expr});"));
                } else {
                    self.line(3, &format!("return new {unknown}(tag);"));
                }
            }
            None => self.raise(
                3,
                "DefgenUnknownValueError",
                &format!("\"{}: id \" + tag + \" matches no declared variant\"", def.name),
            ),
        }
        self.line(2, "}");
        self.blank();

        // -- one nested record per variant --
        for v in &u.variants {
            let cls = self.ident(&v.name);
            let visible: Vec<&Field> = v.fields.iter().filter(|f| f.is_visible()).collect();
            let params: Vec<String> = visible
                .iter()
                .map(|f| format!("{} {}", self.java_type(&f.ty), self.field_ident(f.name().unwrap())))
                .collect();
            let param_docs: Vec<String> =
                visible.iter().map(|f| self.param_doc(f)).filter(|d| !d.is_empty()).collect();
            self.docs_full(2, &v.docs, &[format!("Wire id `0x{:x}` (§7).", v.id)], &param_docs);
            self.line(2, &format!("record {cls}({}) implements {name} {{", params.join(", ")));
            self.note(3, "This variant's wire id — a property of the type, never of a value.");
            self.line(3, &format!("public static final {tag_carrier} ID = {};", uint_lit(v.id, tag_bits)));
            self.blank();

            self.line(3, "@Override");
            self.line(3, "public void packFixed(DefgenBits bits, int off) throws DefgenError {");
            self.line(4, &format!("bits.put(off, {tag_bits}, {});", bigint_lit(v.id)));
            for f in &v.fields {
                let Some(fname) = f.name() else { continue };
                let expr = self.field_ident(fname);
                let off = Self::off("off", tag_bits + f.offset_bits);
                let label = format!("{}.{cls}.{fname}", def.name);
                self.pack(4, &expr, &f.ty, &off, &label);
            }
            self.line(3, "}");
            self.blank();

            self.note(3, "Reads this variant's payload; the id is already matched. Internal.");
            self.line(
                3,
                &format!("static {cls} unpackPayload(DefgenBits bits, int off) throws DefgenError {{"),
            );
            for f in &v.fields {
                self.padding_check(4, f, tag_bits, &format!("{}.{cls}", def.name));
            }
            let mut args: Vec<String> = Vec::new();
            for f in &visible {
                let fname = self.field_ident(f.name().unwrap());
                let off = Self::off("off", tag_bits + f.offset_bits);
                let ty = f.ty.clone();
                args.push(self.unpack_local(4, &fname, &ty, &off));
            }
            self.call(4, &format!("return new {cls}("), &args, ");");
            self.line(3, "}");
            self.line(2, "}");
            self.blank();
        }

        // -- the fallback variant --
        if let Some(arm) = &u.else_arm {
            let cls = self.ident(&arm.name);
            let tag_field = self.field_ident(&u.tag_name);
            let notes = vec![format!(
                "An id `{name}` does not declare (§7). Both the id and the undecoded payload are \
                 kept, so re-encoding is lossless and an unknown command can never be mistaken for \
                 a known one."
            )];
            let mut params = vec![format!("{tag_carrier} {tag_field}")];
            if arm.raw_bits > 0 {
                params.push(format!("{} raw", carrier_type(arm.raw_bits, false)));
            }
            self.docs_with(2, &arm.docs, &notes);
            self.line(2, &format!("record {cls}({}) implements {name} {{", params.join(", ")));
            self.line(3, "@Override");
            self.line(3, "public void packFixed(DefgenBits bits, int off) throws DefgenError {");
            let tag_big = to_bigint(&tag_field, tag_bits, false);
            self.put(
                4,
                "off",
                tag_bits,
                &format!("defgenCheckUInt({tag_big}, {tag_bits}, \"{}.{cls}.{}\")", def.name, u.tag_name),
            );
            if arm.raw_bits > 0 {
                let raw_off = Self::off("off", tag_bits);
                let raw_big = to_bigint("raw", arm.raw_bits, false);
                let bits = arm.raw_bits;
                self.put(
                    4,
                    &raw_off,
                    bits,
                    &format!("defgenCheckUInt({raw_big}, {bits}, \"{}.{cls}.raw\")", def.name),
                );
            }
            self.line(3, "}");
            self.line(2, "}");
            self.blank();
        }
        self.line(1, "}");
        self.blank();
    }

    // -- struct (§6) ------------------------------------------------------------

    fn declare_struct(&mut self, def: &'m TypeDef, s: &'m Struct) {
        let name = self.ident(&def.name);
        let big = java_bool(def.endian == Endianness::Big);
        let visible: Vec<&'m Field> = s.fields.iter().filter(|f| f.is_visible()).collect();
        let tail = self.tail_of_struct(def, s);

        let mut notes: Vec<String> = Vec::new();
        if let Some(t) = def.layout.tail {
            notes.push(format!(
                "Variable-length (§6.3): a {}-byte fixed prefix, then up to {} trailing element(s). \
                 The length is whatever the transport delivers — nothing in the payload states it.",
                def.layout.fixed_bytes(),
                t.max_elems
            ));
        }
        let param_docs: Vec<String> =
            visible.iter().map(|f| self.param_doc(f)).filter(|d| !d.is_empty()).collect();
        self.docs_full(1, &def.docs, &notes, &param_docs);

        if visible.is_empty() {
            self.line(1, &format!("public record {name}() {{"));
        } else {
            self.line(1, &format!("public record {name}("));
            for (i, f) in visible.iter().enumerate() {
                let close = if i + 1 == visible.len() { ") {" } else { "," };
                let ty = self.java_type(&f.ty);
                let fname = self.field_ident(f.name().unwrap());
                self.line(3, &format!("{ty} {fname}{close}"));
            }
        }

        match def.layout.tail {
            Some(_) => {
                self.note(2, "Bytes always present, before the variable-length tail (§6.3).");
                self.line(2, &format!("public static final int FIXED_SIZE = {};", def.layout.fixed_bytes()));
                self.note(2, "Largest legal encoding — what a receive buffer must hold.");
                self.line(2, &format!("public static final int MAX_SIZE = {};", def.layout.max_bytes()));
                self.blank();
            }
            // A nested-only container is free to be sub-byte (§6); only a bound
            // one has to be a whole number of bytes (§10), so a byte size is
            // only stated where it is exact.
            None if def.layout.is_byte_aligned() => {
                self.note(2, &format!("Encoded size of a `{}`, in bytes.", def.name));
                self.line(2, &format!("public static final int SIZE = {};", def.layout.fixed_bytes()));
                self.blank();
            }
            None => {}
        }

        // A record with no components already has a no-argument constructor.
        if !visible.is_empty() {
            self.note(2, "The zero value: every component at its default.");
            self.line(2, &format!("public {name}() {{"));
            let args: Vec<String> = visible.iter().map(|f| self.fresh(&f.ty)).collect();
            self.call(3, "this(", &args, ");");
            self.line(2, "}");
            self.blank();
        }

        if def.root {
            let size = def.layout.fixed_bytes();
            match &tail {
                None => {
                    self.note(
                        2,
                        &format!(
                            "Encodes this `{}` into exactly SIZE ({size}) bytes, {}-endian.",
                            def.name,
                            def.endian.as_str()
                        ),
                    );
                    self.line(2, "public byte[] encode() throws DefgenError {");
                    self.line(3, "DefgenBits bits = new DefgenBits();");
                    self.line(3, "packFixed(bits, 0);");
                    self.line(3, &format!("return bits.toBytes({size}, {big});"));
                    self.line(2, "}");
                    self.blank();

                    self.note(2, &format!("Decodes exactly {size} bytes; any other length is an error."));
                    self.line(2, &format!("public static {name} decode(byte[] data) throws DefgenError {{"));
                    self.line(3, &format!("if (data.length != {size}) {{"));
                    self.raise(
                        4,
                        "DefgenLengthError",
                        &format!("\"{}: expected {size} bytes, got \" + data.length", def.name),
                    );
                    self.line(3, "}");
                    self.line(3, &format!("return unpackFixed(DefgenBits.fromBytes(data, {big}), 0);"));
                    self.line(2, "}");
                    self.blank();
                }
                Some((ty, _, _)) => {
                    let tail_ty = self.tail_type(ty).unwrap_or_else(|| "byte[]".to_string());
                    self.docs_note(
                        2,
                        &[
                            format!("Encodes this `{}`, {}-endian (§8).", def.name, def.endian.as_str()),
                            "The result is the fixed prefix plus however many bytes the tail actually \
                             needs — FIXED_SIZE to MAX_SIZE, never padded out to the maximum (§6.3)."
                                .to_string(),
                        ],
                    );
                    self.line(2, "public byte[] encode() throws DefgenError {");
                    self.line(3, "DefgenBits bits = new DefgenBits();");
                    self.line(3, "packFixed(bits, 0);");
                    self.line(
                        3,
                        &format!("return defgenConcat(bits.toBytes(FIXED_SIZE, {big}), packTail({big}));"),
                    );
                    self.line(2, "}");
                    self.blank();

                    self.note(2, "Bytes this value encodes to as it stands — never padded out (§6.3).");
                    self.line(2, "public int encodedSize() {");
                    self.line(3, "return FIXED_SIZE + tailLen();");
                    self.line(2, "}");
                    self.blank();

                    self.docs_note(
                        2,
                        &[
                            "Decodes the bytes the transport delivered.".to_string(),
                            "The tail's length comes from the buffer, never from the payload itself \
                             (§6.3), so a length outside FIXED_SIZE..MAX_SIZE is an error."
                                .to_string(),
                        ],
                    );
                    self.line(2, &format!("public static {name} decode(byte[] data) throws DefgenError {{"));
                    self.line(3, "if (data.length < FIXED_SIZE || data.length > MAX_SIZE) {");
                    self.raise(
                        4,
                        "DefgenLengthError",
                        &format!(
                            "\"{}: expected \" + FIXED_SIZE + \"..\" + MAX_SIZE + \" bytes, got \" + data.length",
                            def.name
                        ),
                    );
                    self.line(3, "}");
                    // The tail is read first: a record is built in one call, so
                    // there is no half-built value to fill in afterwards.
                    self.line(
                        3,
                        &format!(
                            "{tail_ty} tail = unpackTail(Arrays.copyOfRange(data, FIXED_SIZE, data.length), \
                             {big});"
                        ),
                    );
                    self.line(
                        3,
                        &format!(
                            "return unpackFixed(DefgenBits.fromBytes(Arrays.copyOfRange(data, 0, \
                             FIXED_SIZE), {big}), 0, tail);"
                        ),
                    );
                    self.line(2, "}");
                    self.blank();
                }
            }
        }

        self.note(2, "Packs the fixed part into `bits`, at bit `off`. Internal.");
        self.line(2, "void packFixed(DefgenBits bits, int off) throws DefgenError {");
        for f in &s.fields {
            let Some(fname) = f.name() else { continue };
            // An inline variable-length field contributes no fixed bits: the
            // tail methods are what write it.
            if matches!(self.tail_kind(&f.ty), Some(TailKind::Inline)) {
                continue;
            }
            let expr = self.field_ident(fname);
            let off = Self::off("off", f.offset_bits);
            let label = format!("{}.{fname}", def.name);
            self.pack(3, &expr, &f.ty, &off, &label);
        }
        self.line(2, "}");
        self.blank();

        self.note(2, "Unpacks the fixed part from `bits`, at bit `off`. Internal.");
        let signature = match &tail {
            Some((ty, _, _)) => {
                let tail_ty = self.tail_type(ty).unwrap_or_else(|| "byte[]".to_string());
                format!(
                    "static {name} unpackFixed(DefgenBits bits, int off, {tail_ty} tail) throws DefgenError {{"
                )
            }
            None => format!("static {name} unpackFixed(DefgenBits bits, int off) throws DefgenError {{"),
        };
        self.line(2, &signature);
        for f in &s.fields {
            self.padding_check(3, f, 0, &def.name);
        }
        let mut args: Vec<String> = Vec::new();
        for f in &visible {
            let fname = self.field_ident(f.name().unwrap());
            let off = Self::off("off", f.offset_bits);
            match self.tail_kind(&f.ty) {
                // Already decoded, before this call: see `decode`.
                Some(TailKind::Inline) => args.push("tail".to_string()),
                Some(TailKind::Nested) => {
                    let cls = self.java_type(&f.ty);
                    args.push(format!("{cls}.unpackFixed(bits, {off}, tail)"));
                }
                None => {
                    let ty = f.ty.clone();
                    args.push(self.unpack_local(3, &fname, &ty, &off));
                }
            }
        }
        self.call(3, &format!("return new {name}("), &args, ");");
        self.line(2, "}");

        if let Some((ty, prop, label)) = tail {
            self.blank();
            self.tail_methods(&ty, &prop, &label);
        }

        self.line(1, "}");
        self.blank();
    }

    /// One `@param` line of a record's Javadoc: the component's own doc
    /// comment, plus whatever the backend has to say about how it is
    /// represented.
    fn param_doc(&self, f: &Field) -> String {
        let Some(name) = f.name() else { return String::new() };
        let mut parts: Vec<String> = f.docs.iter().map(|d| escape_doc(&d.text)).collect();
        if matches!(f.role, FieldRole::Reserved { .. }) {
            parts.push(
                "Reserved (§6.2): captured on decode and written back unchanged, so a \
                 decode-then-relay round trip does not clobber it."
                    .to_string(),
            );
        }
        match &f.ty {
            WireType::Array { count, .. } => {
                parts.push(format!("Exactly {count} elements (§6.1); any other count fails to encode."));
            }
            WireType::VarArray { max, .. } => parts.push(format!("At most {max} elements (§6.3).")),
            WireType::Str { max } => {
                parts.push(format!("At most {max} UTF-8 bytes — `max` bounds bytes, not characters (§6.3)."));
            }
            _ => {}
        }
        if parts.is_empty() {
            return String::new();
        }
        format!("@param {} {}", self.field_ident(name), parts.join(" "))
    }

    /// A multi-line Javadoc the backend wrote itself, with no schema doc
    /// comment alongside it.
    fn docs_note(&mut self, ind: usize, lines: &[String]) {
        self.docs_with(ind, &Docs::new(), lines);
    }

    /// `padding: uN = 0` is validated on decode; bare `padding` is not (§6.2).
    fn padding_check(&mut self, ind: usize, f: &Field, base_bits: u32, owner: &str) {
        let FieldRole::Padding { check_zero: true } = f.role else { return };
        let off = Self::off("off", base_bits + f.offset_bits);
        let bits = f.layout.fixed_bits;
        let (from, to) = (f.offset_bits, f.offset_bits + bits);
        self.line(ind, &format!("if (bits.get({off}, {bits}).signum() != 0) {{"));
        self.raise(
            ind + 1,
            "DefgenPaddingError",
            &format!("\"{owner}: padding at bits {from}..{to} is not zero\""),
        );
        self.line(ind, "}");
    }

    /// A `throw` of one of the file's errors, wrapped where the message pushes
    /// the statement past a readable line length.
    fn raise(&mut self, ind: usize, error: &str, message: &str) {
        let text = format!("throw new {error}({message});");
        if text.len() + ind * 4 <= WIDTH {
            self.line(ind, &text);
            return;
        }
        self.line(ind, &format!("throw new {error}("));
        self.line(ind + 2, &format!("{message});"));
    }

    // ---------------------------------------------------------------------
    // Entry points (§12)
    // ---------------------------------------------------------------------

    /// The static codec an `alias`, `scaled` or `enum` bound to a
    /// characteristic gets (§10): the first two are not types here at all, and
    /// an `enum`'s own `decode` speaks in wire values rather than bytes.
    fn entry_functions(&mut self, def: &'m TypeDef) {
        let name = self.ident(&def.name);
        let prefix = screaming(&def.name);
        let big = java_bool(def.endian == Endianness::Big);
        let order = def.endian.as_str();
        let size = def.layout.fixed_bytes();
        let java_ty = self.java_type(&WireType::Named(def.id));
        let target = self.resolve(&WireType::Named(def.id));

        if def.layout.tail.is_none() {
            self.note(1, &format!("Encoded size of a `{}`, in bytes.", def.name));
            self.line(1, &format!("public static final int {prefix}_SIZE = {size};"));
            self.blank();

            self.note(1, &format!("Encodes a `{}` into exactly {size} bytes, {order}-endian.", def.name));
            self.line(
                1,
                &format!("public static byte[] encode{name}({java_ty} value) throws DefgenError {{"),
            );
            self.line(2, "DefgenBits bits = new DefgenBits();");
            self.pack(2, "value", &target, "0", &def.name);
            self.line(2, &format!("return bits.toBytes({size}, {big});"));
            self.line(1, "}");
            self.blank();

            self.note(1, &format!("Decodes exactly {size} bytes into a `{}`.", def.name));
            self.line(1, &format!("public static {java_ty} decode{name}(byte[] data) throws DefgenError {{"));
            self.line(2, &format!("if (data.length != {size}) {{"));
            self.raise(
                3,
                "DefgenLengthError",
                &format!("\"{}: expected {size} bytes, got \" + data.length", def.name),
            );
            self.line(2, "}");
            self.line(2, &format!("DefgenBits bits = DefgenBits.fromBytes(data, {big});"));
            let value = self.unpack_local(2, "value", &target, "0");
            self.line(2, &format!("return {value};"));
            self.line(1, "}");
            self.blank();
            return;
        }

        self.note(1, "Bytes always present, before the variable-length tail (§6.3).");
        self.line(1, &format!("public static final int {prefix}_FIXED_SIZE = {size};"));
        self.note(1, "Largest legal encoding — what a receive buffer must hold.");
        self.line(1, &format!("public static final int {prefix}_MAX_SIZE = {};", def.layout.max_bytes()));
        self.blank();

        self.docs_note(
            1,
            &[
                format!("Encodes a `{}`, {order}-endian (§8).", def.name),
                "The encoding is exactly as long as the value is, never padded out to the declared \
                 maximum (§6.3)."
                    .to_string(),
            ],
        );
        self.line(1, &format!("public static byte[] encode{name}({java_ty} value) throws DefgenError {{"));
        if size > 0 {
            self.line(2, "DefgenBits bits = new DefgenBits();");
            self.pack(2, "value", &target, "0", &def.name);
            self.line(2, &format!("byte[] prefix = bits.toBytes({size}, {big});"));
            let tail = self.pack_tail_body(2, &target, "value", &def.name, big);
            self.line(2, &format!("return defgenConcat(prefix, {tail});"));
        } else {
            // Nothing precedes the tail, so the tail *is* the encoding.
            let tail = self.pack_tail_body(2, &target, "value", &def.name, big);
            self.line(2, &format!("return {tail};"));
        }
        self.line(1, "}");
        self.blank();

        self.note(1, &format!("Decodes the bytes the transport delivered into a `{}` (§6.3).", def.name));
        self.line(1, &format!("public static {java_ty} decode{name}(byte[] data) throws DefgenError {{"));
        self.line(
            2,
            &format!("if (data.length < {prefix}_FIXED_SIZE || data.length > {prefix}_MAX_SIZE) {{"),
        );
        self.raise(
            3,
            "DefgenLengthError",
            &format!(
                "\"{}: expected \" + {prefix}_FIXED_SIZE + \"..\" + {prefix}_MAX_SIZE + \" bytes, got \" \
                 + data.length",
                def.name
            ),
        );
        self.line(2, "}");
        self.unpack_alias_tail(2, &target, &def.name, &prefix, big);
        self.line(1, "}");
        self.blank();
    }

    /// The body of a `decode<Name>` for a variable-length type bound straight
    /// to a characteristic (§6.3), which has no type of its own to hold tail
    /// methods.
    fn unpack_alias_tail(&mut self, ind: usize, target: &WireType, name: &str, prefix: &str, big: &str) {
        match target {
            WireType::Str { .. } => self.line(ind, &format!("return defgenDecodeUtf8(data, \"{name}\");")),
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(ind, &format!("if (data.length % {bytes} != 0) {{"));
                self.raise(
                    ind + 1,
                    "DefgenLengthError",
                    &format!(
                        "\"{name}: \" + data.length + \" bytes is not a whole number of {bytes}-byte \
                         elements\""
                    ),
                );
                self.line(ind, "}");
                self.line(ind, &format!("int count = data.length / {bytes};"));
                let elem_ty = boxed(&self.java_type(elem)).to_string();
                self.line(ind, &format!("List<{elem_ty}> out = new ArrayList<>(count);"));
                self.line(ind, "for (int i = 0; i < count; i++) {");
                self.line(
                    ind + 1,
                    &format!(
                        "DefgenBits bits = DefgenBits.fromBytes(Arrays.copyOfRange(data, i * {bytes}, \
                         (i + 1) * {bytes}), {big});"
                    ),
                );
                let elem_expr = self.unpack_expr(elem, "0");
                self.line(ind + 1, &format!("out.add({elem_expr});"));
                self.line(ind, "}");
                self.line(ind, "return List.copyOf(out);");
            }
            // A named type that owns the tail: a variable-length struct.
            WireType::Named(id) => {
                let cls = self.ident(&self.m.get(*id).name);
                let tail_ty = self.tail_type(target).unwrap_or_else(|| "byte[]".to_string());
                self.line(
                    ind,
                    &format!(
                        "{tail_ty} tail = {cls}.unpackTail(Arrays.copyOfRange(data, {prefix}_FIXED_SIZE, \
                         data.length), {big});"
                    ),
                );
                self.line(
                    ind,
                    &format!(
                        "return {cls}.unpackFixed(DefgenBits.fromBytes(Arrays.copyOfRange(data, 0, \
                         {prefix}_FIXED_SIZE), {big}), 0, tail);"
                    ),
                );
            }
            _ => self.line(ind, "return data;"),
        }
    }

    // ---------------------------------------------------------------------
    // Value-level emitters
    // ---------------------------------------------------------------------

    /// Emits the statement(s) writing `expr` into `bits` at bit `off`.
    fn pack(&mut self, ind: usize, expr: &str, ty: &WireType, off: &str, label: &str) {
        match ty {
            WireType::UInt(n) => {
                let (big, n) = (to_bigint(expr, *n, false), *n);
                self.put(ind, off, n, &format!("defgenCheckUInt({big}, {n}, \"{label}\")"));
            }
            WireType::Int(n) => {
                let (big, n) = (to_bigint(expr, *n, true), *n);
                self.put(ind, off, n, &format!("defgenCheckInt({big}, {n}, \"{label}\")"));
            }
            WireType::Bool => {
                self.put(ind, off, 1, &format!("{expr} ? BigInteger.ONE : BigInteger.ZERO"));
            }
            WireType::Float(f) => {
                let (raw, bits) = float_raw_bits_expr(*f, expr);
                self.put(ind, off, bits, &format!("BigInteger.valueOf({raw})"));
            }
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = self.ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => {
                        let target = a.target.clone();
                        self.pack(ind, expr, &target, off, label);
                    }
                    TypeKind::Scaled(s) => {
                        let raw_expr = format!("{}ToRaw({expr})", lower_first(&name));
                        let big = to_bigint(&raw_expr, s.raw_bits, s.signed);
                        let check = if s.signed { "defgenCheckInt" } else { "defgenCheckUInt" };
                        let bits = s.raw_bits;
                        self.put(ind, off, bits, &format!("{check}({big}, {bits}, \"{label}\")"));
                    }
                    TypeKind::Enum(e) => {
                        let raw_expr = format!("{name}.encode({expr})");
                        let big = to_bigint(&raw_expr, e.backing_bits, false);
                        let bits = e.backing_bits;
                        self.put(ind, off, bits, &format!("defgenCheckUInt({big}, {bits}, \"{label}\")"));
                    }
                    TypeKind::Struct(_) | TypeKind::Union(_) => {
                        self.line(ind, &format!("{expr}.packFixed(bits, {off});"));
                    }
                }
            }
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                let elem_ty = boxed(&self.java_type(elem)).to_string();
                // A block, so two array fields in one container do not fight
                // over the name of the checked list.
                self.line(ind, "{");
                self.line(
                    ind + 1,
                    &format!("List<{elem_ty}> elems = defgenCheckCount({expr}, {count}, \"{label}\");"),
                );
                self.line(ind + 1, &format!("for (int i = 0; i < {count}; i++) {{"));
                let elem_off = format!("({off} + i * {elem_bits})");
                let elem_ty = (**elem).clone();
                self.pack(ind + 2, "elems.get(i)", &elem_ty, &elem_off, label);
                self.line(ind + 1, "}");
                self.line(ind, "}");
            }
            // Written by the tail code, never as part of the fixed prefix.
            WireType::VarArray { .. } | WireType::Str { .. } => {}
        }
    }

    /// A call whose arguments the emitter built one at a time — a record's
    /// constructor, most often. One line where it fits, one argument per line
    /// where it does not: a seven-component record decoded in one expression is
    /// otherwise a single unreadable line.
    fn call(&mut self, ind: usize, open: &str, args: &[String], close: &str) {
        let joined = args.join(", ");
        if ind * 4 + open.len() + joined.len() + close.len() <= WIDTH {
            self.line(ind, &format!("{open}{joined}{close}"));
            return;
        }
        self.line(ind, open);
        for (i, arg) in args.iter().enumerate() {
            let end = if i + 1 == args.len() { close } else { "," };
            self.line(ind + 2, &format!("{arg}{end}"));
        }
    }

    /// One `bits.put`, wrapped where the value expression is long enough to
    /// push the statement past a readable line length.
    fn put(&mut self, ind: usize, off: &str, bits: u32, value: &str) {
        let text = format!("bits.put({off}, {bits}, {value});");
        if text.len() + ind * 4 <= WIDTH {
            self.line(ind, &text);
            return;
        }
        self.line(ind, &format!("bits.put({off}, {bits},"));
        self.line(ind + 2, &format!("{value});"));
    }

    /// Emits whatever statements decoding a value of `ty` needs, and returns
    /// the expression naming it: a local's name for an array, the reading
    /// expression itself for everything else.
    fn unpack_local(&mut self, ind: usize, name: &str, ty: &WireType, off: &str) -> String {
        match self.resolve(ty) {
            WireType::Array { ref elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                let elem_ty = boxed(&self.java_type(elem)).to_string();
                self.line(ind, &format!("List<{elem_ty}> {name} = new ArrayList<>({count});"));
                self.line(ind, &format!("for (int i = 0; i < {count}; i++) {{"));
                let elem_off = format!("({off} + i * {elem_bits})");
                let elem_expr = self.unpack_expr(elem, &elem_off);
                self.line(ind + 1, &format!("{name}.add({elem_expr});"));
                self.line(ind, "}");
                format!("List.copyOf({name})")
            }
            _ => self.unpack_expr(ty, off),
        }
    }

    /// The expression reading one value of `ty` out of `bits` at bit `off`.
    ///
    /// Every case is a single expression — a fallible decode throws from inside
    /// the helper it calls — which is what lets a decoded record be built in one
    /// constructor call.
    fn unpack_expr(&self, ty: &WireType, off: &str) -> String {
        match ty {
            WireType::UInt(n) => from_bigint(&format!("bits.get({off}, {n})"), *n, false),
            WireType::Int(n) => {
                let raw = format!("bits.get({off}, {n})");
                from_bigint(&format!("defgenSext({raw}, {n})"), *n, true)
            }
            WireType::Bool => format!("bits.get({off}, 1).signum() != 0"),
            WireType::Float(f) => float_from_bits_expr(*f, &format!("bits.get({off}, {})", f.bits())),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = self.ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => self.unpack_expr(&a.target, off),
                    TypeKind::Scaled(s) => {
                        let raw = format!("bits.get({off}, {})", s.raw_bits);
                        let raw = if s.signed { format!("defgenSext({raw}, {})", s.raw_bits) } else { raw };
                        let carrier_expr = from_bigint(&raw, s.raw_bits, s.signed);
                        format!("{}FromRaw({carrier_expr})", lower_first(&name))
                    }
                    TypeKind::Enum(e) => {
                        let carrier_expr = from_bigint(
                            &format!("bits.get({off}, {})", e.backing_bits),
                            e.backing_bits,
                            false,
                        );
                        format!("{name}.decode({carrier_expr})")
                    }
                    TypeKind::Struct(_) | TypeKind::Union(_) => format!("{name}.unpackFixed(bits, {off})"),
                }
            }
            // An array is decoded by `unpack_local`, which has statements to
            // spend; a tail is decoded by the tail methods.
            WireType::Array { .. } | WireType::VarArray { .. } => "List.of()".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
        }
    }

    // ---------------------------------------------------------------------
    // Variable-length tails (§6.3)
    // ---------------------------------------------------------------------

    /// How a field's variable-length tail, if it has one, is laid out: inline
    /// in the containing record, or owned by a named type that has tail methods
    /// of its own.
    fn tail_kind(&self, ty: &WireType) -> Option<TailKind> {
        match self.resolve(ty) {
            WireType::Str { .. } | WireType::VarArray { .. } => Some(TailKind::Inline),
            WireType::Named(id) if self.m.get(id).layout.tail.is_some() => Some(TailKind::Nested),
            _ => None,
        }
    }

    /// The Java type a tail is decoded into: a `String`, a `List`, or —
    /// following a nested variable-length struct down — whatever that struct's
    /// own trailing field decodes into.
    fn tail_type(&self, ty: &WireType) -> Option<String> {
        match self.resolve(ty) {
            WireType::Str { .. } => Some("String".to_string()),
            WireType::VarArray { ref elem, .. } => Some(format!("List<{}>", boxed(&self.java_type(elem)))),
            WireType::Named(id) => {
                let s = self.m.get(id).as_struct()?;
                self.tail_type(&s.fields.last()?.ty)
            }
            _ => None,
        }
    }

    /// The trailing field that makes a struct variable-length, as the resolved
    /// type, the component holding it, and the label errors name it by.
    fn tail_of_struct(&self, def: &TypeDef, s: &'m Struct) -> Option<(WireType, String, String)> {
        if !def.layout.is_variable() {
            return None;
        }
        let f = s.fields.last()?;
        let name = f.name()?;
        self.tail_kind(&f.ty)?;
        Some((self.resolve(&f.ty), self.field_ident(name), format!("{}.{name}", def.name)))
    }

    /// `tailLen`, `packTail` and `unpackTail` for a type owning a tail.
    fn tail_methods(&mut self, ty: &WireType, prop: &str, label: &str) {
        self.note(2, "Bytes this value's variable-length tail occupies. Internal.");
        self.line(2, "int tailLen() {");
        let len_expr = match ty {
            WireType::Str { .. } => {
                format!("{prop}.getBytes(java.nio.charset.StandardCharsets.UTF_8).length")
            }
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                format!("{prop}.size() * {bytes}")
            }
            _ => format!("{prop}.tailLen()"),
        };
        self.line(3, &format!("return {len_expr};"));
        self.line(2, "}");
        self.blank();

        self.note(2, "The variable-length tail, which follows the fixed prefix. Internal.");
        self.line(2, "byte[] packTail(boolean big) throws DefgenError {");
        let value = self.pack_tail_body(3, ty, prop, label, "big");
        self.line(3, &format!("return {value};"));
        self.line(2, "}");
        self.blank();

        self.note(2, "Reads the tail; its length is what the transport delivered (§6.3). Internal.");
        let tail_ty = self.tail_type(ty).unwrap_or_else(|| "byte[]".to_string());
        self.line(2, &format!("static {tail_ty} unpackTail(byte[] data, boolean big) throws DefgenError {{"));
        match ty {
            WireType::Str { max } => {
                self.line(3, &format!("if (data.length > {max}) {{"));
                self.raise(
                    4,
                    "DefgenLengthError",
                    &format!("\"{label}: \" + data.length + \" bytes exceeds the maximum of {max}\""),
                );
                self.line(3, "}");
                self.line(3, &format!("return defgenDecodeUtf8(data, \"{label}\");"));
            }
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(3, &format!("if (data.length % {bytes} != 0) {{"));
                self.raise(
                    4,
                    "DefgenLengthError",
                    &format!(
                        "\"{label}: \" + data.length + \" bytes is not a whole number of {bytes}-byte \
                         elements\""
                    ),
                );
                self.line(3, "}");
                self.line(3, &format!("int count = data.length / {bytes};"));
                self.line(3, &format!("if (count > {max}) {{"));
                self.raise(
                    4,
                    "DefgenLengthError",
                    &format!("\"{label}: \" + count + \" elements exceeds the maximum of {max}\""),
                );
                self.line(3, "}");
                let elem_ty = boxed(&self.java_type(elem)).to_string();
                self.line(3, &format!("List<{elem_ty}> out = new ArrayList<>(count);"));
                self.line(3, "for (int i = 0; i < count; i++) {");
                self.line(
                    4,
                    &format!(
                        "DefgenBits bits = DefgenBits.fromBytes(Arrays.copyOfRange(data, i * {bytes}, \
                         (i + 1) * {bytes}), big);"
                    ),
                );
                let elem_expr = self.unpack_expr(elem, "0");
                self.line(4, &format!("out.add({elem_expr});"));
                self.line(3, "}");
                self.line(3, "return List.copyOf(out);");
            }
            WireType::Named(id) => {
                let cls = self.ident(&self.m.get(*id).name);
                self.line(3, &format!("return {cls}.unpackTail(data, big);"));
            }
            _ => self.line(3, "return data;"),
        }
        self.line(2, "}");
    }

    /// Emits whatever statements a tail needs and returns the expression for its
    /// bytes. `big` is how the enclosing scope names its byte order — the `big`
    /// parameter of a tail method, or a literal in a static one.
    fn pack_tail_body(&mut self, ind: usize, ty: &WireType, prop: &str, label: &str, big: &str) -> String {
        match ty {
            WireType::Str { max } => format!("defgenEncodeUtf8({prop}, {max}, \"{label}\")"),
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                let elem_ty = boxed(&self.java_type(elem)).to_string();
                self.line(
                    ind,
                    &format!("List<{elem_ty}> elems = defgenCheckMax({prop}, {max}, \"{label}\");"),
                );
                self.line(ind, &format!("byte[] out = new byte[elems.size() * {bytes}];"));
                self.line(ind, "for (int i = 0; i < elems.size(); i++) {");
                self.line(ind + 1, "DefgenBits bits = new DefgenBits();");
                let elem_ty = (**elem).clone();
                self.pack(ind + 1, "elems.get(i)", &elem_ty, "0", label);
                self.line(
                    ind + 1,
                    &format!("System.arraycopy(bits.toBytes({bytes}, {big}), 0, out, i * {bytes}, {bytes});"),
                );
                self.line(ind, "}");
                "out".to_string()
            }
            _ => format!("{prop}.packTail({big})"),
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
        self.banner(1, "GATT bindings");

        self.note(1, "GATT characteristic properties, as a flag set (§10).");
        self.line(1, "public enum GattProperty {");
        for (i, p) in Property::ALL.iter().enumerate() {
            let sep = if i + 1 == Property::ALL.len() { "" } else { "," };
            self.line(2, &format!("{}{sep}", screaming(p.as_str())));
        }
        self.line(1, "}");
        self.blank();

        self.note(1, "One `characteristic` binding: a UUID, and what may be done with it (§10).");
        self.line(
            1,
            "public record GattCharacteristic(String name, String uuid, Set<GattProperty> properties) {",
        );
        self.line(1, "}");
        self.blank();
        self.note(1, "One `service` declaration, and the characteristics under it (§10).");
        self.line(
            1,
            "public record GattService(String name, String uuid, List<GattCharacteristic> characteristics) {",
        );
        self.line(1, "}");
        self.blank();

        for service in &m.services {
            let sprefix = screaming(&service.name);
            self.docs(1, &service.docs);
            self.line(1, &format!("public static final String {sprefix}_UUID = \"{}\";", service.uuid));
            for c in &service.characteristics {
                let ty_name = self.ident(&m.get(c.ty).name);
                let size = match c.layout.tail {
                    None => format!("{} bytes", c.layout.fixed_bytes()),
                    Some(_) => format!("{}..{} bytes", c.layout.fixed_bytes(), c.layout.max_bytes()),
                };
                let notes = vec![format!("Carries a `{ty_name}` ({size}).")];
                self.docs_with(1, &c.docs, &notes);
                self.line(
                    1,
                    &format!(
                        "public static final String {sprefix}_{}_UUID = \"{}\";",
                        screaming(&c.name),
                        c.uuid
                    ),
                );
            }
            self.blank();

            self.line(1, &format!("public static final GattService {sprefix} = new GattService("));
            self.line(3, &format!("\"{}\",", service.name));
            self.line(3, &format!("{sprefix}_UUID,"));
            if service.characteristics.is_empty() {
                self.line(3, "List.of());");
            } else {
                self.line(3, "List.of(");
                for (i, c) in service.characteristics.iter().enumerate() {
                    let props: Vec<String> = c
                        .properties
                        .iter()
                        .map(|p| format!("GattProperty.{}", screaming(p.as_str())))
                        .collect();
                    // The last argument closes the `Set.of`, the
                    // `GattCharacteristic`, the `List.of` and the
                    // `GattService` in one go.
                    let close = if i + 1 == service.characteristics.len() { ")));" } else { ")," };
                    self.line(4, "new GattCharacteristic(");
                    self.line(6, &format!("\"{}\",", c.name));
                    self.line(6, &format!("{sprefix}_{}_UUID,", screaming(&c.name)));
                    self.line(6, &format!("Set.of({}){close}", props.join(", ")));
                }
            }
            self.blank();
        }

        let names: Vec<String> = m.services.iter().map(|s| screaming(&s.name)).collect();
        self.note(1, "Every service this schema declares, in source order.");
        self.line(
            1,
            &format!("public static final List<GattService> SERVICES = List.of({});", names.join(", ")),
        );
    }
}

/// How wide a generated line is allowed to get before the emitter wraps it.
const WIDTH: usize = 100;
