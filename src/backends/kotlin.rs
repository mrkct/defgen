//! The Kotlin backend: one self-contained Kotlin file per schema (JVM target).
//!
//! # Shape of the output
//!
//! Everything lands in a single `.kt` file with no package declaration — types,
//! constants, codecs and GATT metadata — so a project consumes generated code
//! by dropping one file into a source set. Only the Kotlin/JVM standard
//! library is used, plus `java.math.BigInteger` (and `BigDecimal`, only where
//! a `scaled` type needs rounding): both ship with the JDK, so nothing extra
//! needs to be on the classpath. The floor is Kotlin 1.9: `entries` on an
//! `enum class` is 1.9, and unsigned integer types are 1.5.
//!
//! # Naming
//!
//! | Schema | Kotlin |
//! |---|---|
//! | `struct Status` | `data class Status` |
//! | its codec | `Status.encode()` / `Status.decode(data)` |
//! | its size | `Status.SIZE` |
//! | `enum HearingMode`'s `Stereo` | `HearingMode.Stereo` (open) or `HearingMode.STEREO` (closed) |
//! | field `active_profile` | `activeProfile` |
//! | `alias OwnerName`'s codec | `encodeOwnerName` / `decodeOwnerName` |
//!
//! A member named `packFixed`/`unpackFixed`/`packTail`/`unpackTail`/`tailLen`
//! is `internal`: it exists because a nested type is packed by its parent, not
//! because a caller should reach for it.
//!
//! # Representation choices
//!
//! * A `uN`/`iN` value is carried in the smallest Kotlin integer type that
//!   holds `N` bits (§2): `UByte`/`UShort`/`UInt`/`ULong` for `uN`, the signed
//!   counterparts for `iN`, and `BigInteger` once `N` passes 64 — the JVM has
//!   no 128-bit integer, and `BigInteger` is what the standard library offers
//!   in its place. Every value is range-checked against its declared width on
//!   encode, and every `iN` is sign-extended from bit `N-1` on decode.
//! * A `struct` becomes a mutable `data class`, so `Status()` is a usable zero
//!   value with every field defaulted. Unlike a Python dataclass, a Kotlin
//!   default-argument *expression* is evaluated fresh at every call site that
//!   omits it, so there is no shared-mutable-default hazard to route around —
//!   a nested struct or an array field can default straight to `Orientation()`
//!   or `List(4) { 0 }` with nothing extra.
//! * A plain *closed* `enum` (§5) becomes a Kotlin `enum class` carrying its
//!   wire value as a `raw` property; decoding an unmatched value is a hard
//!   error. An *open* one becomes a `sealed class` instead — one nested
//!   `object` per declared variant plus a nested `data class Unknown`
//!   carrying `raw` for anything else — so the "declared, or not" question
//!   Python answers with a `Union` type alias is instead the sealed
//!   hierarchy itself: `HearingMode` already covers every wire value with no
//!   separate value alias needed, and matching on it is exhaustive `when`.
//! * A tagged union (§7) becomes a sealed class the same way: an abstract base
//!   holding the codec, one nested variant per declared id (an `object` where
//!   it carries no payload, a `data class` otherwise), and — for an open
//!   union — a nested `data class Unknown` carrying the unrecognized id
//!   together with the undecoded raw payload.
//! * A `scaled` type (§4) is a `typealias` for its physical `Float`/`Double`,
//!   plus `<name>FromRaw`/`<name>ToRaw` functions that keep the underlying
//!   wire integer reachable for callers that want to round-trip without
//!   floating-point rounding.
//! * A variable-length field (§6.3) is a native `String` or `List`, as §12
//!   asks. Decode fails on malformed UTF-8 rather than substituting
//!   replacement characters.
//! * Failures are exceptions, one class per kind under a common sealed
//!   `DefgenError`, so a caller can catch the lot with one `catch`.
//!
//! # Bit and byte order
//!
//! A container's bits live in one `BigInteger`, LSB-first from bit 0 — the
//! same design as the Python backend's `_Bits`, chosen for the same reason:
//! `BigInteger` is exactly "an integer with no width ceiling", which is what a
//! container up to 4096 bits (§6) needs, and it is already on the classpath.
//! Byte order (§8) enters in one place only — `DefgenBits.fromBytes`/`toBytes`
//! — because a big-endian container is exactly the same bit sequence read
//! from the far end of the buffer. A scalar field narrower than its carrier
//! (a `u12` in a `UShort`) is range-checked and converted through
//! `BigInteger` too, which sidesteps every JVM signed/unsigned conversion
//! pitfall at the cost of a `BigInteger` per field access — a price this
//! backend is happy to pay for never getting a sign-extension bug wrong.

use super::{Backend, Generated, GeneratedFile, Options, camel, sanitize_stem, screaming};
use crate::ast::{Docs, Endianness, FloatType, Property};
use crate::model::{
    Const, Enum, Field, FieldRole, Model, Scaled, Struct, TypeDef, TypeKind, Union, WireType, carrier_bits,
};

pub struct KotlinBackend;

impl Backend for KotlinBackend {
    fn name(&self) -> &'static str {
        "kotlin"
    }

    fn description(&self) -> &'static str {
        "a single self-contained Kotlin file (JVM, Kotlin 1.9+)"
    }

    fn generate(&self, model: &Model, opts: &Options) -> Generated {
        let stem = sanitize_stem(&opts.stem);
        let file = GeneratedFile {
            name: format!("{stem}.kt"),
            contents: Emitter::new(model, opts.source.as_deref()).run(),
        };
        Generated { files: vec![file] }
    }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------
//
// [`snake`], [`screaming`] and [`camel`] (`activeProfile`, the property-naming
// convention this backend, Swift's and Java's all share) are defined in
// [`super`] so every backend derives them the same way.

/// Every Kotlin hard keyword — the ones that are never legal as an identifier,
/// with or without backticks. A schema name shaped like one is perfectly
/// legal (§1 does not reserve Kotlin's vocabulary), so a collision gets a
/// trailing `_` rather than being backtick-escaped: backticks would work for
/// most of these, but not all, and a uniform rule is one fewer thing to get
/// wrong per name.
#[rustfmt::skip]
const KT_KEYWORDS: &[&str] = &[
    "as", "break", "class", "continue", "do", "else", "false", "for", "fun", "if", "in",
    "interface", "is", "null", "object", "package", "return", "super", "this", "throw", "true",
    "try", "typealias", "typeof", "val", "var", "when", "while",
];

/// Names the generated file uses for its own runtime, or that a `data class`
/// would collide with via its compiler-synthesized members. A field named
/// `copy` or `encode` would otherwise clash with a method the same class
/// carries; a type named `Companion` would clash with the one every
/// `companion object` already introduces.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    "encode", "decode", "packFixed", "unpackFixed", "packTail", "unpackTail", "tailLen",
    "encodedSize", "raw",
    "SIZE", "FIXED_SIZE", "MAX_SIZE", "ID",
    "copy", "equals", "hashCode", "toString", "Companion",
];

/// A schema name as a Kotlin identifier, escaped where it would collide with
/// the language's or the generated file's own vocabulary.
fn ident(name: &str) -> String {
    if KT_KEYWORDS.contains(&name) || RESERVED.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// A field name as the Kotlin property it becomes: `camel`-cased, then
/// escaped the same way a type name is.
fn field_ident(name: &str) -> String {
    ident(&camel(name))
}

/// `Temperature` to `temperature` — the one-letter change that turns a
/// `PascalCase` schema name into the camelCase prefix a generated function
/// name (`temperatureFromRaw`) wants, without touching the rest of the word
/// the way a full `camel()` re-split would.
fn lower_first(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) => c.to_lowercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// A `///` line as KDoc content: the one sequence that would close the
/// comment early is escaped.
fn escape_doc(text: &str) -> String {
    text.replace("*/", "*\\/")
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

/// The Kotlin type a `uN`/`iN` value of `bits` width is carried in: the
/// smallest of `UByte`/`UShort`/`UInt`/`ULong` (unsigned for `uN`) or
/// `Byte`/`Short`/`Int`/`Long` (signed for `iN`) that holds it, or
/// `BigInteger` once the carrier passes 64 bits — the JVM has nothing
/// narrower that reaches 128.
fn carrier_type(bits: u32, signed: bool) -> &'static str {
    match (signed, carrier_bits(bits)) {
        (false, 8) => "UByte",
        (false, 16) => "UShort",
        (false, 32) => "UInt",
        (false, 64) => "ULong",
        (true, 8) => "Byte",
        (true, 16) => "Short",
        (true, 32) => "Int",
        (true, 64) => "Long",
        (_, 128) => "BigInteger",
        _ => unreachable!("carrier_bits only returns 8, 16, 32, 64 or 128"),
    }
}

/// A literal of `carrier_type(bits, false)` naming exactly `value` — every
/// enum variant value and tagged-union id is one of these. `UByte`/`UShort`
/// have no literal suffix of their own, so those go through an `Int` literal
/// (always in range: both are narrower than `Int`) and an explicit
/// conversion; `UInt`/`ULong` have `u`/`uL` suffixes wide enough for their
/// full range.
fn uint_lit(value: u128, bits: u32) -> String {
    match carrier_bits(bits) {
        8 => format!("{value}.toUByte()"),
        16 => format!("{value}.toUShort()"),
        32 => format!("{value}u"),
        64 => format!("{value}uL"),
        128 => format!("BigInteger(\"{value}\")"),
        _ => unreachable!(),
    }
}

/// `const val` if `uint_lit(_, bits)` produces a literal kotlinc accepts as a
/// compile-time constant, `val` otherwise. Only `UInt`/`ULong` have a literal
/// suffix (`u`/`uL`); `UByte`/`UShort` go through a `.toUByte()`/`.toUShort()`
/// call, which kotlinc rejects in a `const val` initializer despite being
/// foldable, and `BigInteger` isn't a `const`-eligible type at all.
fn uint_val_keyword(bits: u32) -> &'static str {
    match carrier_bits(bits) {
        32 | 64 => "const val",
        _ => "val",
    }
}

/// `value` as a bare `BigInteger` literal, however wide — used where a
/// constant (a tagged-union id, most often) is written straight into a
/// `DefgenBits`, with no typed carrier variable in between. `BigInteger`'s
/// string constructor is exact at any width, unlike a Kotlin integer literal
/// suffix, which tops out at `ULong`.
fn bigint_lit(value: u128) -> String {
    format!("BigInteger(\"{value}\")")
}

/// A literal of `carrier_type(bits, true)` naming exactly `value` — used by a
/// signed `const` declaration (§3.1), the only place a compile-time signed
/// value shows up outside a fixed-carrier field. `Byte`/`Short` have no
/// literal suffix of their own, so those go through an `Int` literal (always
/// in range) and an explicit conversion, mirroring `uint_lit`; a negative one
/// needs parentheses first, since `.toByte()`/`.toShort()` binds tighter than
/// unary minus (`-40.toShort()` is `-(40.toShort())`, not what is wanted).
fn int_lit(value: i128, bits: u32) -> String {
    match carrier_bits(bits) {
        8 => format!("({value}).toByte()"),
        16 => format!("({value}).toShort()"),
        32 => format!("{value}"),
        64 => format!("{value}L"),
        128 => format!("BigInteger(\"{value}\")"),
        _ => unreachable!(),
    }
}

/// `const val` if `int_lit(_, bits)` produces a literal kotlinc accepts as a
/// compile-time constant, `val` otherwise — the signed counterpart of
/// [`uint_val_keyword`].
fn int_val_keyword(bits: u32) -> &'static str {
    match carrier_bits(bits) {
        32 | 64 => "const val",
        _ => "val",
    }
}

/// The expression converting a value already in `expr` (of
/// `carrier_type(bits, signed)`) into the `BigInteger` the bit container
/// speaks. `UByte`/`UShort`/`UInt`/`ULong` all render their `toString()` as an
/// unsigned decimal, which is a safe, width-independent way to reach
/// `BigInteger` without a manual sign-bit dance for the 64-bit case (a plain
/// `Long` round trip would misread a `ULong` above `Long.MAX_VALUE`).
fn to_bigint(expr: &str, bits: u32, signed: bool) -> String {
    match carrier_bits(bits) {
        128 => expr.to_string(),
        // Already a `Long`; `.toLong()` here would be a redundant conversion,
        // which kotlinc warns on and the conformance build treats as an error.
        64 if signed => format!("BigInteger.valueOf({expr})"),
        _ if signed => format!("BigInteger.valueOf({expr}.toLong())"),
        _ => format!("BigInteger({expr}.toString())"),
    }
}

/// The expression converting a `BigInteger` already known to be in
/// `expr`'s legal range down to `carrier_type(bits, signed)`. For the
/// unsigned carriers this relies on `BigInteger.toInt()`/`toLong()` and the
/// stdlib's `Int.toUInt()`-style reinterpreting conversions: `expr` is always
/// non-negative and below `2^carrier_bits`, so truncating to the carrier's own
/// width is exact, not lossy.
fn from_bigint(expr: &str, bits: u32, signed: bool) -> String {
    match (signed, carrier_bits(bits)) {
        (false, 8) => format!("{expr}.toInt().toUByte()"),
        (false, 16) => format!("{expr}.toInt().toUShort()"),
        (false, 32) => format!("{expr}.toInt().toUInt()"),
        (false, 64) => format!("{expr}.toLong().toULong()"),
        (true, 8) => format!("{expr}.toByte()"),
        (true, 16) => format!("{expr}.toShort()"),
        (true, 32) => format!("{expr}.toInt()"),
        (true, 64) => format!("{expr}.toLong()"),
        (_, 128) => expr.to_string(),
        _ => unreachable!(),
    }
}

/// A `Float`/`Double` literal Kotlin reads back as the same value. Rust's
/// shortest representation round-trips; only the suffix and the spelling of
/// the non-finite values differ from Rust's.
fn float_lit(v: f64, physical: FloatType) -> String {
    let suffix = if physical == FloatType::F32 { "f" } else { "" };
    if v.is_nan() {
        return format!("Float.NaN{}", if physical == FloatType::F32 { "" } else { ".toDouble()" })
            .replace("Float.NaN.toDouble()", "Double.NaN");
    }
    if v.is_infinite() {
        let sign = if v < 0.0 { "NEGATIVE" } else { "POSITIVE" };
        let ty = if physical == FloatType::F32 { "Float" } else { "Double" };
        return format!("{ty}.{sign}_INFINITY");
    }
    let s = format!("{v:?}");
    let s = if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") };
    format!("{s}{suffix}")
}

/// `expr`'s IEEE-754 bit pattern as a Kotlin integer, plus its width. Widening
/// the `Int` `Float.toRawBits()` returns into the `Long` `BigInteger.valueOf`
/// wants sign-extends it, but `DefgenBits.put` masks to the low `bits` bits
/// before storing, so the sign-extended high bits are discarded and the
/// pattern survives exactly (§2, §8).
fn float_raw_bits_expr(f: FloatType, expr: &str) -> (String, u32) {
    match f {
        FloatType::F32 => (format!("{expr}.toRawBits().toLong()"), 32),
        FloatType::F64 => (format!("{expr}.toRawBits()"), 64),
    }
}

/// The reverse of [`float_raw_bits_expr`]: reinterprets `bits`'s wire pattern
/// (a non-negative `BigInteger`) as `f`. `toInt`/`toLong` keep only the low
/// 32/64 bits, which is exactly the pattern to reinterpret.
fn float_from_bits_expr(f: FloatType, bits: &str) -> String {
    match f {
        FloatType::F32 => format!("Float.fromBits({bits}.toInt())"),
        FloatType::F64 => format!("Double.fromBits({bits}.toLong())"),
    }
}

// ---------------------------------------------------------------------------
// Variable-length tails
// ---------------------------------------------------------------------------

/// How a field's variable-length tail, if it has one, is laid out — see
/// [`Emitter::tail_kind`].
enum TailKind {
    /// A native `String`/`List` property of the containing class.
    Inline,
    /// A named type that owns the tail, and the methods that handle it.
    Nested,
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Imports and runtime helpers the schema turned out to need. An unused
/// import or helper is dead weight in a file meant to be dropped whole into a
/// project, so none of these is unconditional.
#[derive(Default)]
struct Needs {
    /// A `scaled` type, which needs `BigDecimal` for exact rounding (§4).
    round: bool,
    /// A `padding: uN = 0` field, which needs the all-zero check (§6.2).
    zero_check: bool,
    /// A fixed or variable-length array, whose length check is generic.
    arrays: bool,
    /// A `string`, which needs the UTF-8 helpers (§6.3).
    utf8: bool,
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

    fn banner(&mut self, title: &str) {
        let rule = "-".repeat(76usize.saturating_sub(title.len()));
        self.blank();
        self.line(0, &format!("// {title} {rule}"));
        self.blank();
    }

    /// Doc comments as KDoc (§1, §12).
    fn docs(&mut self, ind: usize, docs: &Docs) {
        if docs.is_empty() {
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
        self.line(ind, " */");
    }

    /// A KDoc comment the backend wrote itself, wrapped to stay readable.
    fn note(&mut self, ind: usize, text: &str) {
        const WIDTH: usize = 92;
        let budget = WIDTH.saturating_sub(ind * 4);
        if text.len() + 6 <= budget {
            self.line(ind, &format!("/** {text} */"));
            return;
        }
        self.line(ind, "/**");
        let mut current = String::new();
        for word in text.split_whitespace() {
            if !current.is_empty() && current.len() + 1 + word.len() + 3 > budget {
                self.line(ind, &format!(" * {current}"));
                current.clear();
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() {
            self.line(ind, &format!(" * {current}"));
        }
        self.line(ind, " */");
    }

    /// The schema's own doc comment, then — after a blank KDoc line — whatever
    /// the backend has to say about the representation it chose.
    fn docs_with(&mut self, ind: usize, docs: &Docs, notes: &[String]) {
        if docs.is_empty() && notes.is_empty() {
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
            self.line(ind, &format!(" * {note}"));
        }
        self.line(ind, " */");
    }

    // -- pre-pass -------------------------------------------------------------

    /// Works out which imports and helpers the schema needs, before emitting
    /// the header that has to declare them.
    fn scan(&mut self) {
        let m = self.m;
        for def in &m.types {
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
        if let FieldRole::Padding { check_zero } = f.role {
            self.needs.zero_check |= check_zero;
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
            WireType::Str { .. } => self.needs.utf8 = true,
            WireType::Array { elem, .. } => {
                self.needs.arrays = true;
                self.scan_type(elem);
            }
            WireType::VarArray { elem, .. } => {
                self.needs.arrays = true;
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

    /// The Kotlin type a value of `ty` is held in. Aliases are deliberately
    /// *not* resolved here: the domain name the author declared is the point
    /// of one.
    fn kt_type(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) => carrier_type(*n, false).to_string(),
            WireType::Int(n) => carrier_type(*n, true).to_string(),
            WireType::Bool => "Boolean".to_string(),
            WireType::Float(FloatType::F32) => "Float".to_string(),
            WireType::Float(FloatType::F64) => "Double".to_string(),
            WireType::Named(id) => ident(&self.m.get(*id).name),
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => {
                format!("List<{}>", self.kt_type(elem))
            }
            WireType::Str { .. } => "String".to_string(),
        }
    }

    /// An expression building a fresh zero value of `ty`, used as a
    /// property's default so every generated class is constructible with no
    /// arguments. Unlike Python's `default_factory` dance, a Kotlin default
    /// argument is just an expression re-evaluated at each call site that
    /// omits it, so a mutable-looking default (a `List`, a nested class) needs
    /// no extra machinery to stay unshared between instances.
    fn fresh(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) => uint_lit(0, *n),
            WireType::Int(n) => {
                if carrier_bits(*n) == 128 {
                    "BigInteger.ZERO".to_string()
                } else {
                    "0".to_string()
                }
            }
            WireType::Bool => "false".to_string(),
            WireType::Float(FloatType::F32) => "0.0f".to_string(),
            WireType::Float(FloatType::F64) => "0.0".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
            WireType::VarArray { .. } => "emptyList()".to_string(),
            WireType::Array { elem, count } => format!("List({count}) {{ {} }}", self.fresh(elem)),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => self.fresh(&a.target),
                    TypeKind::Scaled(s) => match s.physical {
                        FloatType::F32 => "0.0f".to_string(),
                        FloatType::F64 => "0.0".to_string(),
                    },
                    TypeKind::Enum(e) => match (e.variants.first(), &e.else_arm) {
                        (Some(v), _) if e.is_open() => format!("{name}.{}", ident(&v.name)),
                        (Some(v), _) => format!("{name}.{}", screaming(&v.name)),
                        (None, Some(_)) => format!("{name}.Unknown({})", uint_lit(0, e.backing_bits)),
                        (None, None) => "0".to_string(),
                    },
                    TypeKind::Union(u) => match (u.variants.first(), &u.else_arm) {
                        (Some(v), _) if v.fields.iter().any(Field::is_visible) => {
                            format!("{name}.{}()", ident(&v.name))
                        }
                        (Some(v), _) => format!("{name}.{}", ident(&v.name)),
                        (None, Some(_)) => {
                            let tag = uint_lit(0, u.tag_bits);
                            if u.payload_bits > 0 {
                                format!("{name}.Unknown({tag}, {})", uint_lit(0, u.payload_bits))
                            } else {
                                format!("{name}.Unknown({tag})")
                            }
                        }
                        (None, None) => format!("{name}()"),
                    },
                    TypeKind::Struct(_) => format!("{name}()"),
                }
            }
        }
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
                " * order applied once per root container (§8). Encoding produces a `ByteArray`;",
                " * decoding takes the bytes the transport delivered. Anything the schema does",
                " * not allow throws a `DefgenError` subclass, rather than being quietly",
                " * truncated, wrapped or replaced.",
                " *",
                " * Only a type bound to a characteristic has `encode`/`decode`: byte order is a",
                " * property of the root container, so a type that is only ever nested has no",
                " * byte order of its own to be encoded in (§8).",
                " *",
                " * Requires Kotlin 1.9 or newer, targeting the JVM. There are no third-party",
                " * dependencies.",
                " */",
                "",
                "import java.math.BigInteger",
            ],
        );
        if self.needs.round {
            self.line(0, "import java.math.BigDecimal");
        }
    }

    // ---------------------------------------------------------------------
    // Runtime
    // ---------------------------------------------------------------------

    fn runtime(&mut self) {
        self.banner("Errors");
        self.line(0, "sealed class DefgenError(message: String) : Exception(message)");
        self.blank();
        for (name, doc) in [
            ("DefgenLengthError", "The buffer's length matches no legal encoding of the type (§6.3, §10)."),
            ("DefgenRangeError", "A value does not fit the bits its field declares (§2, §4, §6.3)."),
            ("DefgenUnknownValueError", "A closed enum or tagged union met an undeclared value (§5, §7)."),
            ("DefgenPaddingError", "A `padding: uN = 0` run was not zero on the wire (§6.2)."),
            ("DefgenUtf8Error", "A `string` field's bytes are not well-formed UTF-8 (§6.3)."),
        ] {
            self.note(0, doc);
            self.line(0, &format!("class {name}(message: String) : DefgenError(message)"));
            self.blank();
        }

        self.banner("Runtime");
        self.lines(
            0,
            &[
                "/**",
                " * A container's bits as one arbitrary-width integer, packed LSB-first from bit",
                " * 0 (§6). `value` is always non-negative.",
                " *",
                " * Byte order (§8) enters only where this meets a `ByteArray`: a big-endian",
                " * container is the very same bit sequence read from the far end of the buffer,",
                " * so byte order is one argument to `fromBytes`/`toBytes` rather than something",
                " * every field has to know about.",
                " */",
                "internal class DefgenBits(var value: BigInteger = BigInteger.ZERO) {",
                "    companion object {",
                "        /** The bits of `data`, read in the given byte order. */",
                "        fun fromBytes(data: ByteArray, big: Boolean): DefgenBits {",
                "            val bytes = if (big) data else data.reversedArray()",
                "            return DefgenBits(BigInteger(1, bytes))",
                "        }",
                "    }",
                "",
                "    /** Exactly `size` bytes, written in the given byte order. */",
                "    fun toBytes(size: Int, big: Boolean): ByteArray {",
                "        val magnitude = value.toByteArray()",
                "        val out = ByteArray(size)",
                "        val copyLen = minOf(magnitude.size, size)",
                "        System.arraycopy(magnitude, magnitude.size - copyLen, out, size - copyLen, copyLen)",
                "        return if (big) out else out.reversedArray()",
                "    }",
                "",
                "    /** The `bits` bits starting at `off`. */",
                "    fun get(off: Int, bits: Int): BigInteger {",
                "        val mask = BigInteger.ONE.shiftLeft(bits).subtract(BigInteger.ONE)",
                "        return value.shiftRight(off).and(mask)",
                "    }",
                "",
                "    /** Writes the low `bits` bits of `v` at `off`. */",
                "    fun put(off: Int, bits: Int, v: BigInteger) {",
                "        val mask = BigInteger.ONE.shiftLeft(bits).subtract(BigInteger.ONE)",
                "        value = value.andNot(mask.shiftLeft(off)).or(v.and(mask).shiftLeft(off))",
                "    }",
                "}",
                "",
                "/** Sign-extends an `iN` value from bit N-1 (§2). */",
                "private fun defgenSext(value: BigInteger, bits: Int): BigInteger {",
                "    val sign = BigInteger.ONE.shiftLeft(bits - 1)",
                "    return value.xor(sign).subtract(sign)",
                "}",
                "",
                "/** Range-checks a `uN` value: out of range is an error, never a truncation (§2). */",
                "private fun defgenCheckUInt(value: BigInteger, bits: Int, where_: String): BigInteger {",
                "    val limit = BigInteger.ONE.shiftLeft(bits)",
                "    if (value.signum() < 0 || value >= limit) {",
                "        throw DefgenRangeError(\"$where_: $value does not fit in u$bits\")",
                "    }",
                "    return value",
                "}",
                "",
                "/** Range-checks an `iN` value and returns its two's-complement bits (§2). */",
                "private fun defgenCheckInt(value: BigInteger, bits: Int, where_: String): BigInteger {",
                "    val limit = BigInteger.ONE.shiftLeft(bits - 1)",
                "    if (value < limit.negate() || value >= limit) {",
                "        throw DefgenRangeError(\"$where_: $value does not fit in i$bits\")",
                "    }",
                "    return if (value.signum() < 0) value.add(BigInteger.ONE.shiftLeft(bits)) else value",
                "}",
            ],
        );

        if self.needs.arrays {
            self.blank();
            self.lines(
                0,
                &[
                    "/** A fixed-size array carries exactly `count` elements, always (§6.1). */",
                    "private fun <T> defgenCheckCount(seq: List<T>, count: Int, where_: String): List<T> {",
                    "    if (seq.size != count) {",
                    "        throw DefgenRangeError(\"$where_: expected exactly $count elements, got ${seq.size}\")",
                    "    }",
                    "    return seq",
                    "}",
                    "",
                    "/** A variable-length array carries at most `limit` elements (§6.3). */",
                    "private fun <T> defgenCheckMax(seq: List<T>, limit: Int, where_: String): List<T> {",
                    "    if (seq.size > limit) {",
                    "        throw DefgenRangeError(\"$where_: ${seq.size} elements exceeds the maximum of $limit\")",
                    "    }",
                    "    return seq",
                    "}",
                ],
            );
        }

        if self.needs.utf8 {
            self.blank();
            self.lines(
                0,
                &[
                    "/** A `string` field's bytes, rejecting anything past its `max` (§6.3). */",
                    "private fun defgenEncodeUtf8(text: String, limit: Int, where_: String): ByteArray {",
                    "    val data = text.toByteArray(Charsets.UTF_8)",
                    "    if (data.size > limit) {",
                    "        throw DefgenRangeError(\"$where_: ${data.size} bytes exceeds the maximum of $limit\")",
                    "    }",
                    "    return data",
                    "}",
                    "",
                    "/**",
                    " * Decodes a `string` field. Malformed input fails rather than being patched up",
                    " * with replacement characters (§6.3).",
                    " */",
                    "private fun defgenDecodeUtf8(data: ByteArray, where_: String): String {",
                    "    val decoder = Charsets.UTF_8.newDecoder()",
                    "        .onMalformedInput(java.nio.charset.CodingErrorAction.REPORT)",
                    "        .onUnmappableCharacter(java.nio.charset.CodingErrorAction.REPORT)",
                    "    return try {",
                    "        decoder.decode(java.nio.ByteBuffer.wrap(data)).toString()",
                    "    } catch (exc: java.nio.charset.CharacterCodingException) {",
                    "        throw DefgenUtf8Error(\"$where_: ${exc.message}\")",
                    "    }",
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
                    " * Rounds half away from zero, which is what C's `round()` does. Every backend",
                    " * has to agree on a `scaled` value's raw integer down to the last unit (§4,",
                    " * §13): Kotlin has no built-in \"round half away from zero\", and the obvious",
                    " * `Math.round` rounds half *up*, which disagrees on every negative tie.",
                    " */",
                    "private fun defgenRound(value: Double, where_: String): BigInteger {",
                    "    if (value - value != 0.0) {",
                    "        // Infinite or NaN: every other double subtracts to zero. No integer",
                    "        // represents either, so this is where an out-of-range `scaled` division",
                    "        // lands, rather than in an arithmetic exception from `BigDecimal`.",
                    "        throw DefgenRangeError(\"$where_: $value cannot be rounded to an integer\")",
                    "    }",
                    "    val whole = BigDecimal(value).toBigInteger()",
                    "    val remainder = value - whole.toDouble()",
                    "    return when {",
                    "        remainder >= 0.5 -> whole.add(BigInteger.ONE)",
                    "        remainder <= -0.5 -> whole.subtract(BigInteger.ONE)",
                    "        else -> whole",
                    "    }",
                    "}",
                ],
            );
        }
    }

    // ---------------------------------------------------------------------
    // Declarations
    // ---------------------------------------------------------------------

    /// Types in source order. §9 forbids forward references, so source order
    /// is already a valid definition order for a file where `typealias` and
    /// top-level `fun`s reference each other by name.
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

    // -- const (§3.1) -----------------------------------------------------------

    fn declare_const(&mut self, c: &Const) {
        let ty = carrier_type(c.bits, c.signed);
        let keyword = if c.signed { int_val_keyword(c.bits) } else { uint_val_keyword(c.bits) };
        let value = if c.signed { int_lit(c.as_i128(), c.bits) } else { uint_lit(c.magnitude, c.bits) };
        self.docs_with(0, &c.docs, &[]);
        self.line(0, &format!("{keyword} {}: {ty} = {value}", screaming(&c.name)));
        self.blank();
    }

    /// Whether `def` is bound to a characteristic (§10) but has no class of
    /// its own to hang `encode`/`decode` off — an alias, a `scaled` type or an
    /// enum, all of which are module-level codec functions instead.
    fn has_entry_functions(&self, def: &TypeDef) -> bool {
        def.root && !matches!(def.kind, TypeKind::Struct(_) | TypeKind::Union(_))
    }

    // -- alias (§3) -----------------------------------------------------------

    fn declare_alias(&mut self, def: &'m TypeDef, target: &'m WireType) {
        let name = ident(&def.name);
        let ty = self.kt_type(target);
        let notes = vec![format!(
            "`{}` (§3): a name for `{}`, with no runtime type of its own.",
            def.name,
            self.wire_str(target)
        )];
        self.docs_with(0, &def.docs, &notes);
        self.line(0, &format!("typealias {name} = {ty}"));
        self.blank();
    }

    // -- scaled (§4) ------------------------------------------------------------

    fn declare_scaled(&mut self, def: &'m TypeDef, s: &'m Scaled) {
        let name = ident(&def.name);
        let prefix = screaming(&def.name);
        let fnp = lower_first(&name);
        let physical = match s.physical {
            FloatType::F32 => "Float",
            FloatType::F64 => "Double",
        };
        let raw_ty = carrier_type(s.raw_bits, s.signed);
        let raw_wire = format!("{}{}", if s.signed { "i" } else { "u" }, s.raw_bits);

        let notes = vec![
            format!(
                "`{}` (§4): `physical = raw * scale + offset`, over a `{raw_wire}` wire value.",
                def.name
            ),
            format!(
                "The raw wire integer is reachable via `{fnp}ToRaw`/`{fnp}FromRaw`, for callers that want to round-trip without floating-point rounding."
            ),
        ];
        self.docs_with(0, &def.docs, &notes);
        self.line(0, &format!("typealias {name} = {physical}"));
        self.blank();
        self.line(0, &format!("const val {prefix}_SCALE: {physical} = {}", float_lit(s.scale, s.physical)));
        self.line(0, &format!("const val {prefix}_OFFSET: {physical} = {}", float_lit(s.offset, s.physical)));
        self.blank();

        self.note(0, "Decodes the raw wire integer into the physical value (§4).");
        self.line(0, &format!("fun {fnp}FromRaw(raw: {raw_ty}): {name} {{"));
        let as_bigint = to_bigint("raw", s.raw_bits, s.signed);
        let as_bigint = if s.signed { format!("defgenSext({as_bigint}, {})", s.raw_bits) } else { as_bigint };
        self.line(1, &format!("val physical = {as_bigint}.toDouble() * {prefix}_SCALE + {prefix}_OFFSET"));
        let cast = if s.physical == FloatType::F32 { ".toFloat()" } else { "" };
        self.line(1, &format!("return physical{cast}"));
        self.line(0, "}");
        self.blank();

        self.docs_with(
            0,
            &Docs::new(),
            &[
                "Rounds `value` to the nearest raw wire integer (§4).".to_string(),
                format!("Anything outside `{raw_wire}`'s range is an error rather than a wraparound."),
            ],
        );
        self.line(0, &format!("fun {fnp}ToRaw(value: {name}): {raw_ty} {{"));
        self.line(
            1,
            &format!(
                "val raw = defgenRound((value.toDouble() - {prefix}_OFFSET) / {prefix}_SCALE, \"{}\")",
                def.name
            ),
        );
        let check = if s.signed { "defgenCheckInt" } else { "defgenCheckUInt" };
        self.line(1, &format!("val checked = {check}(raw, {}, \"{}\")", s.raw_bits, def.name));
        self.line(1, &format!("return {}", from_bigint("checked", s.raw_bits, s.signed)));
        self.line(0, "}");
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
        let name = ident(&def.name);
        let bits = e.backing_bits;
        let carrier = carrier_type(bits, false);

        let notes = vec![
            format!("{} {bits}-bit wire value (§5).", article(bits).to_uppercase()),
            "Closed: a value matching none of the variants below is a hard error, on encode \
             as well as on decode."
                .to_string(),
        ];
        self.docs_with(0, &def.docs, &notes);
        self.line(0, &format!("enum class {name}(val raw: {carrier}) {{"));
        for (i, v) in e.variants.iter().enumerate() {
            self.docs(1, &v.docs);
            let sep = if i + 1 == e.variants.len() { ";" } else { "," };
            self.line(1, &format!("{}({}){sep}", screaming(&v.name), uint_lit(v.value, bits)));
        }
        self.blank();
        self.line(1, "companion object {");
        self.note(2, "The variant `raw` names; an unmatched value is an error (§5).");
        self.line(2, &format!("internal fun decode(raw: {carrier}): {name} ="));
        self.line(
            3,
            &format!(
                "entries.firstOrNull {{ it.raw == raw }} ?: throw DefgenUnknownValueError(\"{}: \
                 $raw matches no declared variant\")",
                def.name
            ),
        );
        self.blank();
        self.note(2, "The wire value `value` encodes to.");
        self.line(2, &format!("internal fun encode(value: {name}): {carrier} = value.raw"));
        self.line(1, "}");
        self.line(0, "}");
        self.blank();
    }

    /// An *open* enum (§5) is a sealed class rather than an `enum class`: a
    /// nested `object` per declared variant, plus a nested `data class
    /// Unknown` for anything else. Unlike Python's `IntEnum` + `Union` type
    /// alias, the sealed class already answers "declared, or not" on its
    /// own — `HearingMode` is the type of every wire value, known or not.
    fn declare_open_enum(&mut self, def: &'m TypeDef, e: &'m Enum) {
        let name = ident(&def.name);
        let bits = e.backing_bits;
        let carrier = carrier_type(bits, false);
        let arm = e.else_arm.as_ref().expect("declare_open_enum called on a closed enum");

        let notes = vec![
            format!("{} {bits}-bit wire value (§5).", article(bits).to_uppercase()),
            format!(
                "Open: a value matching none of the variants below decodes to `{name}.{}` \
                 instead of failing, so decoding this enum never fails.",
                arm.name
            ),
        ];
        self.docs_with(0, &def.docs, &notes);
        self.line(0, &format!("sealed class {name} {{"));
        self.line(1, &format!("abstract val raw: {carrier}"));
        self.blank();
        for v in &e.variants {
            self.docs(1, &v.docs);
            self.line(1, &format!("object {} : {name}() {{", ident(&v.name)));
            self.line(2, &format!("override val raw: {carrier} = {}", uint_lit(v.value, bits)));
            self.line(1, "}");
        }
        self.blank();
        self.docs_with(
            1,
            &arm.docs,
            &[format!(
                "A wire value `{name}` does not declare (§5). It keeps the value it was \
                 decoded from, so re-encoding it is lossless."
            )],
        );
        self.line(1, &format!("data class {}(override val raw: {carrier}) : {name}()", ident(&arm.name)));
        self.blank();
        self.line(1, "companion object {");
        self.note(2, &format!("The variant `raw` names, or `{name}.{}` (§5).", arm.name));
        self.line(2, &format!("internal fun decode(raw: {carrier}): {name} = when (raw) {{"));
        for v in &e.variants {
            self.line(3, &format!("{} -> {}", uint_lit(v.value, bits), ident(&v.name)));
        }
        self.line(3, &format!("else -> {}(raw)", ident(&arm.name)));
        self.line(2, "}");
        self.blank();
        self.note(2, "The wire value `value` encodes to.");
        self.line(2, &format!("internal fun encode(value: {name}): {carrier} = value.raw"));
        self.line(1, "}");
        self.line(0, "}");
        self.blank();
    }

    // -- tagged union (§7) -----------------------------------------------------

    fn declare_union(&mut self, def: &'m TypeDef, u: &'m Union) {
        let name = ident(&def.name);
        let tag_carrier = carrier_type(u.tag_bits, false);
        let (tag_bits, payload_bits) = (u.tag_bits, u.payload_bits);
        let big = def.endian == Endianness::Big;
        let size = def.layout.fixed_bytes();

        let mut notes = vec![
            format!(
                "A tagged union (§7): {} {tag_bits}-bit `{}` in the container's low bits, then \
                 {} {payload_bits}-bit payload the id says how to read.",
                article(tag_bits),
                u.tag_name,
                article(payload_bits)
            ),
            format!(
                "Sealed: every value is one of `{name}`'s nested variants, so a decoded command \
                 is matched with `is`/`when`, never by inspecting a tag by hand."
            ),
        ];
        if !u.is_open() {
            notes.push("An id matching no variant is a hard decode error.".to_string());
        }
        self.docs_with(0, &def.docs, &notes);
        self.line(0, &format!("sealed class {name} {{"));
        self.line(1, "internal abstract fun packFixed(bits: DefgenBits, off: Int)");
        self.blank();

        if def.root {
            self.note(
                1,
                &format!(
                    "Encodes this `{}` into exactly SIZE ({size}) bytes, {}-endian.",
                    def.name,
                    def.endian.as_str()
                ),
            );
            self.line(1, "fun encode(): ByteArray {");
            self.line(2, "val bits = DefgenBits()");
            self.line(2, "packFixed(bits, 0)");
            self.line(2, &format!("return bits.toBytes({size}, big = {big})"));
            self.line(1, "}");
            self.blank();
        }

        self.line(1, "companion object {");
        // A tagged union's layout never has a tail (§7: the payload is a fixed
        // number of bits chosen by the container width), so unlike a struct
        // this is always a plain SIZE, never a FIXED_SIZE/MAX_SIZE pair.
        // A nested-only union is free to be sub-byte (§6); only a bound one has
        // to be a whole number of bytes (§10), so a byte size is only stated
        // where it is exact.
        if def.layout.is_byte_aligned() {
            self.line(2, &format!("const val SIZE: Int = {}", def.layout.fixed_bytes()));
            self.blank();
        }
        if def.root {
            self.note(2, &format!("Decodes exactly {size} bytes; any other length is an error."));
            self.line(2, &format!("fun decode(data: ByteArray): {name} {{"));
            self.line(3, &format!("if (data.size != {size}) {{"));
            self.raise(
                4,
                "DefgenLengthError",
                &format!("\"{}: expected {size} bytes, got ${{data.size}}\"", def.name),
            );
            self.line(3, "}");
            self.line(3, &format!("return unpackFixed(DefgenBits.fromBytes(data, big = {big}), 0)"));
            self.line(2, "}");
            self.blank();
        }
        self.note(2, "Reads the id at `off` and dispatches to the variant it names (§7). Internal.");
        self.line(2, &format!("internal fun unpackFixed(bits: DefgenBits, off: Int): {name} {{"));
        let tag_expr = from_bigint(&format!("bits.get(off, {tag_bits})"), tag_bits, false);
        self.line(3, &format!("val tag = {tag_expr}"));
        self.line(3, "return when (tag) {");
        for v in &u.variants {
            self.line(
                4,
                &format!("{} -> {}.unpackPayload(bits, off)", uint_lit(v.id, tag_bits), ident(&v.name)),
            );
        }
        match &u.else_arm {
            Some(arm) => {
                let unknown = ident(&arm.name);
                let raw = if arm.raw_bits > 0 {
                    let raw_off = Self::off("off", tag_bits);
                    let raw_expr =
                        from_bigint(&format!("bits.get({raw_off}, {})", arm.raw_bits), arm.raw_bits, false);
                    format!(", {raw_expr}")
                } else {
                    String::new()
                };
                self.line(4, &format!("else -> {unknown}(tag{raw})"));
            }
            None => {
                self.line(4, "else -> throw DefgenUnknownValueError(");
                self.line(5, &format!("\"{}: id $tag matches no declared variant\"", def.name));
                self.line(4, ")");
            }
        }
        self.line(3, "}");
        self.line(2, "}");
        self.line(1, "}");
        self.blank();

        // -- one nested class per variant --
        for v in &u.variants {
            let cls = ident(&v.name);
            let visible: Vec<&Field> = v.fields.iter().filter(|f| f.is_visible()).collect();
            self.docs_with(1, &v.docs, &[format!("Wire id `0x{:x}` (§7).", v.id)]);
            if visible.is_empty() {
                self.line(1, &format!("object {cls} : {name}() {{"));
                self.line(
                    2,
                    &format!(
                        "{} ID: {tag_carrier} = {}",
                        uint_val_keyword(tag_bits),
                        uint_lit(v.id, tag_bits)
                    ),
                );
                self.blank();
                self.line(2, "override fun packFixed(bits: DefgenBits, off: Int) {");
                self.line(3, &format!("bits.put(off, {tag_bits}, {})", bigint_lit(v.id)));
                self.line(2, "}");
                self.blank();
                self.note(2, "Reads this variant's payload; the id is already matched. Internal.");
                self.line(
                    2,
                    &format!("internal fun unpackPayload(bits: DefgenBits, off: Int): {cls} = this"),
                );
                self.line(1, "}");
            } else {
                let params: Vec<String> = visible
                    .iter()
                    .map(|f| format!("var {}: {}", field_ident(f.name().unwrap()), self.kt_type(&f.ty)))
                    .collect();
                self.line(1, &format!("data class {cls}({}) : {name}() {{", params.join(", ")));
                self.line(2, "companion object {");
                self.line(
                    3,
                    &format!(
                        "{} ID: {tag_carrier} = {}",
                        uint_val_keyword(tag_bits),
                        uint_lit(v.id, tag_bits)
                    ),
                );
                self.blank();
                self.note(3, "Reads this variant's payload; the id is already matched. Internal.");
                self.line(3, &format!("internal fun unpackPayload(bits: DefgenBits, off: Int): {cls} {{"));
                for f in &v.fields {
                    self.padding_check(4, f, tag_bits, &format!("{}.{cls}", def.name));
                }
                let args: Vec<String> = visible
                    .iter()
                    .map(|f| {
                        let off = Self::off("off", tag_bits + f.offset_bits);
                        format!("{} = {}", field_ident(f.name().unwrap()), self.unpack_expr(&f.ty, &off))
                    })
                    .collect();
                self.line(4, &format!("return {cls}({})", args.join(", ")));
                self.line(3, "}");
                self.line(2, "}");
                self.blank();

                self.line(2, "override fun packFixed(bits: DefgenBits, off: Int) {");
                self.line(3, &format!("bits.put(off, {tag_bits}, {})", bigint_lit(v.id)));
                for f in &v.fields {
                    let Some(fname) = f.name() else { continue };
                    let expr = field_ident(fname);
                    let off = Self::off("off", tag_bits + f.offset_bits);
                    let label = format!("{}.{cls}.{fname}", def.name);
                    self.pack(3, &expr, &f.ty, &off, &label);
                }
                self.line(2, "}");
                self.line(1, "}");
            }
            self.blank();
        }

        // -- the fallback variant --
        if let Some(arm) = &u.else_arm {
            let cls = ident(&arm.name);
            let notes = vec![format!(
                "An id `{name}` does not declare (§7). Both the id and the undecoded payload \
                 are kept, so re-encoding is lossless and an unknown command can never be \
                 mistaken for a known one."
            )];
            self.docs_with(1, &arm.docs, &notes);
            let mut params = vec![format!("val {}: {tag_carrier}", field_ident(&u.tag_name))];
            if arm.raw_bits > 0 {
                params.push(format!("val raw: {}", carrier_type(arm.raw_bits, false)));
            }
            self.line(1, &format!("data class {cls}({}) : {name}() {{", params.join(", ")));
            self.line(2, "override fun packFixed(bits: DefgenBits, off: Int) {");
            let tag_prop = field_ident(&u.tag_name);
            let tag_big = to_bigint(&tag_prop, tag_bits, false);
            self.line(
                3,
                &format!(
                    "bits.put(off, {tag_bits}, defgenCheckUInt({tag_big}, {tag_bits}, \"{}.{cls}.{}\"))",
                    def.name, u.tag_name
                ),
            );
            if arm.raw_bits > 0 {
                let raw_off = Self::off("off", tag_bits);
                let raw_big = to_bigint("raw", arm.raw_bits, false);
                self.line(
                    3,
                    &format!(
                        "bits.put({raw_off}, {}, defgenCheckUInt({raw_big}, {}, \"{}.{cls}.raw\"))",
                        arm.raw_bits, arm.raw_bits, def.name
                    ),
                );
            }
            self.line(2, "}");
            self.line(1, "}");
            self.blank();
        }
        self.line(0, "}");
        self.blank();
    }

    // -- struct (§6) ------------------------------------------------------------

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
        self.docs_with(0, &def.docs, &notes);

        let visible: Vec<&'m Field> = s.fields.iter().filter(|f| f.is_visible()).collect();
        if visible.is_empty() {
            self.line(0, &format!("class {name} {{"));
        } else {
            self.line(0, &format!("data class {name}("));
            for (i, f) in visible.iter().enumerate() {
                let fname = f.name().unwrap();
                self.field_doc(1, f);
                let kw = if matches!(f.role, FieldRole::Reserved { .. }) { "val" } else { "var" };
                let comma = if i + 1 == visible.len() { "" } else { "," };
                self.line(
                    1,
                    &format!(
                        "{kw} {}: {}{}{comma}",
                        field_ident(fname),
                        self.kt_type(&f.ty),
                        self.default_clause(&f.ty)
                    ),
                );
            }
            self.line(0, ") {");
        }
        self.blank();

        self.line(1, "companion object {");
        match def.layout.tail {
            Some(_) => {
                self.line(2, &format!("const val FIXED_SIZE: Int = {}", def.layout.fixed_bytes()));
                self.line(2, &format!("const val MAX_SIZE: Int = {}", def.layout.max_bytes()));
            }
            // A nested-only container is free to be sub-byte (§6); only a bound
            // one has to be a whole number of bytes (§10), so a byte size is
            // only stated where it is exact.
            None if def.layout.is_byte_aligned() => {
                self.line(2, &format!("const val SIZE: Int = {}", def.layout.fixed_bytes()));
            }
            None => {}
        }
        self.blank();

        if def.root {
            let size = def.layout.fixed_bytes();
            match def.layout.tail {
                None => {
                    self.note(2, &format!("Decodes exactly {size} bytes; any other length is an error."));
                    self.line(2, &format!("fun decode(data: ByteArray): {name} {{"));
                    self.line(3, &format!("if (data.size != {size}) {{"));
                    self.raise(
                        4,
                        "DefgenLengthError",
                        &format!("\"{}: expected {size} bytes, got ${{data.size}}\"", def.name),
                    );
                    self.line(3, "}");
                    self.line(3, &format!("return unpackFixed(DefgenBits.fromBytes(data, big = {big}), 0)"));
                    self.line(2, "}");
                }
                Some(_) => {
                    self.docstring_note(
                        2,
                        &[
                            "Decodes the bytes the transport delivered.".to_string(),
                            "The tail's length comes from `data.size`, never from the payload \
                             itself (§6.3), so a length outside FIXED_SIZE..MAX_SIZE is an error."
                                .to_string(),
                        ],
                    );
                    self.line(2, &format!("fun decode(data: ByteArray): {name} {{"));
                    self.line(3, "if (data.size < FIXED_SIZE || data.size > MAX_SIZE) {");
                    self.raise(
                        4,
                        "DefgenLengthError",
                        &format!(
                            "\"{}: expected $FIXED_SIZE..$MAX_SIZE bytes, got ${{data.size}}\"",
                            def.name
                        ),
                    );
                    self.line(3, "}");
                    self.line(
                        3,
                        &format!(
                            "val value = unpackFixed(DefgenBits.fromBytes(data.copyOfRange(0, \
                             FIXED_SIZE), big = {big}), 0)"
                        ),
                    );
                    self.line(
                        3,
                        &format!("value.unpackTail(data.copyOfRange(FIXED_SIZE, data.size), {big})"),
                    );
                    self.line(3, "return value");
                    self.line(2, "}");
                }
            }
            self.blank();
        }

        self.note(2, "Unpacks the fixed part from `bits`, at bit `off`. Internal.");
        self.line(2, &format!("internal fun unpackFixed(bits: DefgenBits, off: Int): {name} {{"));
        for f in &s.fields {
            self.padding_check(3, f, 0, &def.name);
        }
        let args: Vec<String> = s
            .fields
            .iter()
            .filter(|f| !matches!(self.tail_kind(&f.ty), Some(TailKind::Inline)))
            .filter_map(|f| {
                let fname = f.name()?;
                let off = Self::off("off", f.offset_bits);
                Some(format!("{} = {}", field_ident(fname), self.unpack_expr(&f.ty, &off)))
            })
            .collect();
        if args.is_empty() {
            self.line(3, &format!("return {name}()"));
        } else {
            self.line(3, &format!("return {name}("));
            for a in &args {
                self.line(4, &format!("{a},"));
            }
            self.line(3, ")");
        }
        self.line(2, "}");
        self.line(1, "}");
        self.blank();

        self.note(1, "Packs the fixed part into `bits`, at bit `off`. Internal.");
        self.line(1, "internal fun packFixed(bits: DefgenBits, off: Int) {");
        for f in &s.fields {
            let Some(fname) = f.name() else { continue };
            // An inline variable-length field contributes no fixed bits: the
            // tail methods are what write it.
            if matches!(self.tail_kind(&f.ty), Some(TailKind::Inline)) {
                continue;
            }
            let expr = field_ident(fname);
            let off = Self::off("off", f.offset_bits);
            let label = format!("{}.{fname}", def.name);
            self.pack(2, &expr, &f.ty, &off, &label);
        }
        self.line(1, "}");
        self.blank();

        if def.root {
            let size = def.layout.fixed_bytes();
            match def.layout.tail {
                None => {
                    self.note(
                        1,
                        &format!(
                            "Encodes this `{}` into exactly SIZE ({size}) bytes, {}-endian.",
                            def.name,
                            def.endian.as_str()
                        ),
                    );
                    self.line(1, "fun encode(): ByteArray {");
                    self.line(2, "val bits = DefgenBits()");
                    self.line(2, "packFixed(bits, 0)");
                    self.line(2, &format!("return bits.toBytes({size}, big = {big})"));
                    self.line(1, "}");
                }
                Some(_) => {
                    self.docstring_note(
                        1,
                        &[
                            format!("Encodes this `{}`, {}-endian (§8).", def.name, def.endian.as_str()),
                            "The result is the fixed prefix plus however many bytes the tail \
                             actually needs — FIXED_SIZE to MAX_SIZE, never padded out to the \
                             maximum (§6.3)."
                                .to_string(),
                        ],
                    );
                    self.line(1, "fun encode(): ByteArray {");
                    self.line(2, "val bits = DefgenBits()");
                    self.line(2, "packFixed(bits, 0)");
                    self.line(2, &format!("val prefix = bits.toBytes({size}, big = {big})"));
                    self.line(2, &format!("return prefix + packTail({big})"));
                    self.line(1, "}");
                    self.blank();
                    self.note(1, "Bytes this value encodes to as it stands — never padded out (§6.3).");
                    self.line(1, "fun encodedSize(): Int = FIXED_SIZE + tailLen()");
                }
            }
            self.blank();
        }

        if let Some((ty, prop, label)) = self.tail_of_struct(def, s) {
            self.tail_methods(&ty, &prop, &label);
        }

        self.line(0, "}");
        self.blank();
    }

    /// One property of a generated `data class`, with its doc comment and —
    /// for a `reserved` field or a variable-length one — the note that
    /// explains it.
    fn field_doc(&mut self, ind: usize, f: &Field) {
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
        self.docs_with(ind, &f.docs, &notes);
    }

    fn default_clause(&self, ty: &WireType) -> String {
        format!(" = {}", self.fresh(ty))
    }

    /// A multi-line KDoc the backend wrote itself, with no schema doc comment
    /// alongside it.
    fn docstring_note(&mut self, ind: usize, lines: &[String]) {
        self.docs_with(ind, &Docs::new(), lines);
    }

    /// `padding: uN = 0` is validated on decode; bare `padding` is not (§6.2).
    fn padding_check(&mut self, ind: usize, f: &Field, base_bits: u32, owner: &str) {
        let FieldRole::Padding { check_zero: true } = f.role else { return };
        let off = Self::off("off", base_bits + f.offset_bits);
        let bits = f.layout.fixed_bits;
        let (from, to) = (f.offset_bits, f.offset_bits + bits);
        self.line(ind, &format!("if (bits.get({off}, {bits}) != BigInteger.ZERO) {{"));
        self.raise(
            ind + 1,
            "DefgenPaddingError",
            &format!("\"{owner}: padding at bits {from}..{to} is not zero\""),
        );
        self.line(ind, "}");
    }

    /// A `throw` of one of the module's errors.
    fn raise(&mut self, ind: usize, error: &str, message: &str) {
        self.line(ind, &format!("throw {error}({message})"));
    }

    // ---------------------------------------------------------------------
    // Entry points (§12)
    // ---------------------------------------------------------------------

    /// The module-level codec an `alias`, `scaled` or `enum` bound to a
    /// characteristic gets (§10): none of the three is a class, so there is
    /// nowhere to hang a method.
    fn entry_functions(&mut self, def: &'m TypeDef) {
        let name = ident(&def.name);
        let prefix = screaming(&def.name);
        let big = def.endian == Endianness::Big;
        let order = def.endian.as_str();
        let size = def.layout.fixed_bytes();
        let kt_ty = self.kt_type(&WireType::Named(def.id));
        let target = self.resolve(&WireType::Named(def.id));

        if def.layout.tail.is_none() {
            self.note(0, &format!("Encoded size of a `{}`, in bytes.", def.name));
            self.line(0, &format!("const val {prefix}_SIZE: Int = {size}"));
            self.blank();

            self.note(0, &format!("Encodes a `{}` into exactly {size} bytes, {order}-endian.", def.name));
            self.line(0, &format!("fun encode{name}(value: {kt_ty}): ByteArray {{"));
            self.line(1, "val bits = DefgenBits()");
            self.pack(1, "value", &target, "0", &def.name);
            self.line(1, &format!("return bits.toBytes({size}, big = {big})"));
            self.line(0, "}");
            self.blank();

            self.note(0, &format!("Decodes exactly {size} bytes into a `{}`.", def.name));
            self.line(0, &format!("fun decode{name}(data: ByteArray): {kt_ty} {{"));
            self.line(1, &format!("if (data.size != {size}) {{"));
            self.raise(
                2,
                "DefgenLengthError",
                &format!("\"{}: expected {size} bytes, got ${{data.size}}\"", def.name),
            );
            self.line(1, "}");
            self.line(1, &format!("val bits = DefgenBits.fromBytes(data, big = {big})"));
            let value = self.unpack_expr(&target, "0");
            self.line(1, &format!("return {value}"));
            self.line(0, "}");
            self.blank();
            return;
        }

        self.note(0, "Bytes always present, before the variable-length tail (§6.3).");
        self.line(0, &format!("const val {prefix}_FIXED_SIZE: Int = {size}"));
        self.note(0, "Largest legal encoding — what a receive buffer must hold.");
        self.line(0, &format!("const val {prefix}_MAX_SIZE: Int = {}", def.layout.max_bytes()));
        self.blank();

        self.docstring_note(
            0,
            &[
                format!("Encodes a `{}`, {order}-endian (§8).", def.name),
                "The encoding is exactly as long as the value is, never padded out to the \
                 declared maximum (§6.3)."
                    .to_string(),
            ],
        );
        self.line(0, &format!("fun encode{name}(value: {kt_ty}): ByteArray {{"));
        self.line(1, "val bits = DefgenBits()");
        self.pack(1, "value", &target, "0", &def.name);
        self.line(1, &format!("val prefix = bits.toBytes({size}, big = {big})"));
        let tail = self.pack_tail_body(1, &target, "value", &def.name, kt_bool(big));
        self.line(1, &format!("return prefix + {tail}"));
        self.line(0, "}");
        self.blank();

        self.note(0, &format!("Decodes the bytes the transport delivered into a `{}` (§6.3).", def.name));
        self.line(0, &format!("fun decode{name}(data: ByteArray): {kt_ty} {{"));
        self.line(1, &format!("if (data.size < {prefix}_FIXED_SIZE || data.size > {prefix}_MAX_SIZE) {{"));
        self.raise(
            2,
            "DefgenLengthError",
            &format!(
                "\"{}: expected ${{{prefix}_FIXED_SIZE}}..${{{prefix}_MAX_SIZE}} bytes, got \
                 ${{data.size}}\"",
                def.name
            ),
        );
        self.line(1, "}");
        self.unpack_alias_tail(1, &target, &def.name, &prefix, big);
        self.line(0, "}");
        self.blank();
    }

    /// The body of a `decode<Name>` for a variable-length type bound straight
    /// to a characteristic (§6.3), which has no class to hold tail methods.
    fn unpack_alias_tail(&mut self, ind: usize, target: &WireType, name: &str, prefix: &str, big: bool) {
        match target {
            WireType::Str { .. } => self.line(ind, &format!("return defgenDecodeUtf8(data, \"{name}\")")),
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(ind, &format!("if (data.size % {bytes} != 0) {{"));
                self.raise(
                    ind + 1,
                    "DefgenLengthError",
                    &format!(
                        "\"{name}: ${{data.size}} bytes is not a whole number of {bytes}-byte \
                         elements\""
                    ),
                );
                self.line(ind, "}");
                self.line(ind, &format!("return List(data.size / {bytes}) {{ i ->"));
                self.line(
                    ind + 1,
                    &format!(
                        "val bits = DefgenBits.fromBytes(data.copyOfRange(i * {bytes}, (i + 1) * \
                         {bytes}), {})",
                        kt_bool(big)
                    ),
                );
                let elem_expr = self.unpack_expr(elem, "0");
                self.line(ind + 1, &elem_expr);
                self.line(ind, "}");
            }
            // A named type that owns the tail: a variable-length struct.
            WireType::Named(id) => {
                let cls = ident(&self.m.get(*id).name);
                self.line(
                    ind,
                    &format!(
                        "val value = {cls}.unpackFixed(DefgenBits.fromBytes(data.copyOfRange(0, \
                         {prefix}_FIXED_SIZE), {}), 0)",
                        kt_bool(big)
                    ),
                );
                self.line(
                    ind,
                    &format!(
                        "value.unpackTail(data.copyOfRange({prefix}_FIXED_SIZE, data.size), {})",
                        kt_bool(big)
                    ),
                );
                self.line(ind, "return value");
            }
            _ => self.line(ind, "return data"),
        }
    }

    // ---------------------------------------------------------------------
    // Value-level emitters
    // ---------------------------------------------------------------------

    /// Emits the statement(s) writing `expr` into `bits` at bit `off`.
    fn pack(&mut self, ind: usize, expr: &str, ty: &WireType, off: &str, label: &str) {
        match ty {
            WireType::UInt(n) => {
                let big = to_bigint(expr, *n, false);
                self.line(ind, &format!("bits.put({off}, {n}, defgenCheckUInt({big}, {n}, \"{label}\"))"));
            }
            WireType::Int(n) => {
                let big = to_bigint(expr, *n, true);
                self.line(ind, &format!("bits.put({off}, {n}, defgenCheckInt({big}, {n}, \"{label}\"))"));
            }
            WireType::Bool => {
                self.line(
                    ind,
                    &format!("bits.put({off}, 1, if ({expr}) BigInteger.ONE else BigInteger.ZERO)"),
                );
            }
            WireType::Float(f) => {
                let (raw, bits) = float_raw_bits_expr(*f, expr);
                self.line(ind, &format!("bits.put({off}, {bits}, BigInteger.valueOf({raw}))"));
            }
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => {
                        let target = a.target.clone();
                        self.pack(ind, expr, &target, off, label);
                    }
                    TypeKind::Scaled(s) => {
                        let raw_expr = format!("{}ToRaw({expr})", lower_first(&name));
                        let big = to_bigint(&raw_expr, s.raw_bits, s.signed);
                        let check = if s.signed { "defgenCheckInt" } else { "defgenCheckUInt" };
                        self.line(
                            ind,
                            &format!(
                                "bits.put({off}, {}, {check}({big}, {}, \"{label}\"))",
                                s.raw_bits, s.raw_bits
                            ),
                        );
                    }
                    TypeKind::Enum(e) => {
                        let raw_expr = format!("{name}.encode({expr})");
                        let big = to_bigint(&raw_expr, e.backing_bits, false);
                        self.line(
                            ind,
                            &format!(
                                "bits.put({off}, {}, defgenCheckUInt({big}, {}, \"{label}\"))",
                                e.backing_bits, e.backing_bits
                            ),
                        );
                    }
                    TypeKind::Struct(_) | TypeKind::Union(_) => {
                        self.line(ind, &format!("{expr}.packFixed(bits, {off})"));
                    }
                }
            }
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                self.line(
                    ind,
                    &format!("for ((elemIdx, elemVal) in defgenCheckCount({expr}, {count}, \"{label}\").withIndex()) {{"),
                );
                let elem_off = format!("({off} + elemIdx * {elem_bits})");
                let elem_ty = (**elem).clone();
                self.pack(ind + 1, "elemVal", &elem_ty, &elem_off, label);
                self.line(ind, "}");
            }
            // Written by the tail code, never as part of the fixed prefix.
            WireType::VarArray { .. } | WireType::Str { .. } => {}
        }
    }

    /// The expression reading a value of `ty` out of `bits` at bit `off`.
    ///
    /// Every case is a single expression — a fallible decode throws from
    /// inside the helper it calls — which is what lets a decoded value be
    /// built in one constructor call and an array be a `List` literal.
    fn unpack_expr(&self, ty: &WireType, off: &str) -> String {
        match ty {
            WireType::UInt(n) => from_bigint(&format!("bits.get({off}, {n})"), *n, false),
            WireType::Int(n) => {
                let raw = format!("bits.get({off}, {n})");
                let sext = format!("defgenSext({raw}, {n})");
                from_bigint(&sext, *n, true)
            }
            WireType::Bool => format!("(bits.get({off}, 1) != BigInteger.ZERO)"),
            WireType::Float(f) => float_from_bits_expr(*f, &format!("bits.get({off}, {})", f.bits())),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
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
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                let elem_off = format!("({off} + i * {elem_bits})");
                let body = self.unpack_expr(elem, &elem_off);
                format!("List({count}) {{ i -> {body} }}")
            }
            // Read by the tail code; the fixed part leaves a placeholder.
            WireType::VarArray { .. } => "emptyList()".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
        }
    }

    // ---------------------------------------------------------------------
    // Variable-length tails (§6.3)
    // ---------------------------------------------------------------------

    /// How a field's variable-length tail, if it has one, is laid out: inline
    /// in the containing class, or owned by a named type that has tail
    /// methods of its own.
    fn tail_kind(&self, ty: &WireType) -> Option<TailKind> {
        match self.resolve(ty) {
            WireType::Str { .. } | WireType::VarArray { .. } => Some(TailKind::Inline),
            WireType::Named(id) if self.m.get(id).layout.tail.is_some() => Some(TailKind::Nested),
            _ => None,
        }
    }

    /// The trailing field that makes a struct variable-length, as the
    /// resolved type, the property holding it, and the label errors name it
    /// by.
    fn tail_of_struct(&self, def: &TypeDef, s: &Struct) -> Option<(WireType, String, String)> {
        if !def.layout.is_variable() {
            return None;
        }
        let f = s.fields.last()?;
        let name = f.name()?;
        self.tail_kind(&f.ty)?;
        Some((self.resolve(&f.ty), field_ident(name), format!("{}.{name}", def.name)))
    }

    /// `tailLen`, `packTail` and `unpackTail` for a type owning a tail.
    fn tail_methods(&mut self, ty: &WireType, prop: &str, label: &str) {
        self.note(1, "Bytes this value's variable-length tail occupies. Internal.");
        self.line(1, "internal fun tailLen(): Int {");
        let len_expr = match ty {
            WireType::Str { .. } => format!("{prop}.toByteArray(Charsets.UTF_8).size"),
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                format!("{prop}.size * {bytes}")
            }
            _ => format!("{prop}.tailLen()"),
        };
        self.line(2, &format!("return {len_expr}"));
        self.line(1, "}");
        self.blank();

        self.note(1, "The variable-length tail, which follows the fixed prefix. Internal.");
        self.line(1, "internal fun packTail(big: Boolean): ByteArray {");
        let value = self.pack_tail_body(2, ty, prop, label, "big");
        self.line(2, &format!("return {value}"));
        self.line(1, "}");
        self.blank();

        self.note(1, "Reads the tail; its length is what the transport delivered (§6.3). Internal.");
        self.line(1, "internal fun unpackTail(data: ByteArray, big: Boolean) {");
        match ty {
            WireType::Str { max } => {
                self.line(2, &format!("if (data.size > {max}) {{"));
                self.raise(
                    3,
                    "DefgenLengthError",
                    &format!("\"{label}: ${{data.size}} bytes exceeds the maximum of {max}\""),
                );
                self.line(2, "}");
                self.line(2, &format!("{prop} = defgenDecodeUtf8(data, \"{label}\")"));
            }
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(2, &format!("if (data.size % {bytes} != 0) {{"));
                self.raise(
                    3,
                    "DefgenLengthError",
                    &format!(
                        "\"{label}: ${{data.size}} bytes is not a whole number of {bytes}-byte \
                         elements\""
                    ),
                );
                self.line(2, "}");
                self.line(2, &format!("val count = data.size / {bytes}"));
                self.line(2, &format!("if (count > {max}) {{"));
                self.raise(
                    3,
                    "DefgenLengthError",
                    &format!("\"{label}: $count elements exceeds the maximum of {max}\""),
                );
                self.line(2, "}");
                self.line(2, &format!("{prop} = List(count) {{ i ->"));
                self.line(
                    3,
                    &format!(
                        "val bits = DefgenBits.fromBytes(data.copyOfRange(i * {bytes}, (i + 1) * \
                         {bytes}), big)"
                    ),
                );
                let elem_expr = self.unpack_expr(elem, "0");
                self.line(3, &elem_expr);
                self.line(2, "}");
            }
            _ => self.line(2, &format!("{prop}.unpackTail(data, big)")),
        }
        self.line(1, "}");
        self.blank();
    }

    /// Emits whatever statements a tail needs and returns the expression for
    /// its bytes. `big` is how the enclosing scope names its byte order — the
    /// `big` parameter of a tail method, or a literal in a top-level function.
    fn pack_tail_body(&mut self, ind: usize, ty: &WireType, prop: &str, label: &str, big: &str) -> String {
        match ty {
            WireType::Str { max } => format!("defgenEncodeUtf8({prop}, {max}, \"{label}\")"),
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(ind, "val out = java.io.ByteArrayOutputStream()");
                self.line(ind, &format!("for (elemVal in defgenCheckMax({prop}, {max}, \"{label}\")) {{"));
                self.line(ind + 1, "val bits = DefgenBits()");
                let elem_ty = (**elem).clone();
                self.pack(ind + 1, "elemVal", &elem_ty, "0", label);
                self.line(ind + 1, &format!("out.write(bits.toBytes({bytes}, big = {big}))"));
                self.line(ind, "}");
                "out.toByteArray()".to_string()
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
        self.banner("GATT bindings");

        self.note(0, "GATT characteristic properties, as a flag set (§10).");
        self.line(0, "enum class GattProperty {");
        for (i, p) in Property::ALL.iter().enumerate() {
            let sep = if i + 1 == Property::ALL.len() { "" } else { "," };
            self.line(1, &format!("{}{sep}", screaming(p.as_str())));
        }
        self.line(0, "}");
        self.blank();

        self.note(0, "One `characteristic` binding: a UUID, and what may be done with it (§10).");
        self.line(
            0,
            "data class GattCharacteristic(val name: String, val uuid: String, val properties: \
             Set<GattProperty>)",
        );
        self.blank();
        self.note(0, "One `service` declaration, and the characteristics under it (§10).");
        self.line(
            0,
            "data class GattService(val name: String, val uuid: String, val characteristics: \
             List<GattCharacteristic>)",
        );
        self.blank();

        for service in &m.services {
            let sprefix = screaming(&service.name);
            self.docs_with(0, &service.docs, &[]);
            self.line(0, &format!("const val {sprefix}_UUID: String = \"{}\"", service.uuid));
            for c in &service.characteristics {
                let ty_name = ident(&m.get(c.ty).name);
                let size = match c.layout.tail {
                    None => format!("{} bytes", c.layout.fixed_bytes()),
                    Some(_) => format!("{}..{} bytes", c.layout.fixed_bytes(), c.layout.max_bytes()),
                };
                let notes = vec![format!("Carries a `{ty_name}` ({size}).")];
                self.docs_with(0, &c.docs, &notes);
                self.line(
                    0,
                    &format!("const val {sprefix}_{}_UUID: String = \"{}\"", screaming(&c.name), c.uuid),
                );
            }
            self.blank();

            self.line(0, &format!("val {sprefix}: GattService = GattService("));
            self.line(1, &format!("name = \"{}\",", service.name));
            self.line(1, &format!("uuid = {sprefix}_UUID,"));
            self.line(1, "characteristics = listOf(");
            for c in &service.characteristics {
                let props: Vec<String> =
                    c.properties.iter().map(|p| format!("GattProperty.{}", screaming(p.as_str()))).collect();
                self.line(2, "GattCharacteristic(");
                self.line(3, &format!("name = \"{}\",", c.name));
                self.line(3, &format!("uuid = {sprefix}_{}_UUID,", screaming(&c.name)));
                self.line(3, &format!("properties = setOf({}),", props.join(", ")));
                self.line(2, "),");
            }
            self.line(1, "),");
            self.line(0, ")");
            self.blank();
        }

        let names: Vec<String> = m.services.iter().map(|s| screaming(&s.name)).collect();
        self.note(0, "Every service this schema declares, in source order.");
        self.line(0, &format!("val SERVICES: List<GattService> = listOf({})", names.join(", ")));
    }
}

/// `true`/`false`, spelled the way Kotlin does — for embedding a root's fixed
/// byte order as a literal rather than threading it as a parameter, wherever
/// the call site is a top-level function rather than an instance method.
fn kt_bool(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}
