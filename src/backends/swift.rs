//! The Swift backend: one self-contained Swift file per schema.
//!
//! # Shape of the output
//!
//! Everything lands in a single `.swift` file — types, constants and codecs —
//! with no `import` at all: only the standard library is used, including its
//! native `UInt128`/`Int128` (Swift 6+), which is what lets this backend carry
//! every `uN`/`iN` value up to the 128-bit ceiling (§2) in a plain fixed-width
//! integer, the same way the smaller widths are. A project consumes generated
//! code by dropping one file into a target.
//!
//! # Naming
//!
//! | Schema | Swift |
//! |---|---|
//! | `struct Status` | `struct Status { ... }` |
//! | its codec | `Status.encode()` / `Status.decode(data)` |
//! | its size | `Status.size` |
//! | `enum HearingMode`'s `Stereo` | `HearingMode.stereo` (open or closed) |
//! | field `active_profile` | `activeProfile` |
//! | `alias OwnerName`'s codec | `encodeOwnerName` / `decodeOwnerName` |
//!
//! `packFixed`/`unpackFixed`/`packTail`/`unpackTail`/`tailLen` are internal
//! (default, module-only, access): they exist because a nested type is packed
//! by its parent, not because a caller should reach for them.
//!
//! # Representation choices
//!
//! * A `uN`/`iN` value is carried in the smallest of Swift's native
//!   `UInt8`/`UInt16`/`UInt32`/`UInt64`/`UInt128` (unsigned) or their signed
//!   counterparts that holds `N` bits (§2). Every value is range-checked
//!   against its declared width on encode, and every `iN` is sign-extended
//!   from bit `N-1` on decode.
//! * A `struct` becomes a Swift `struct` with `var` (or, for `reserved`,
//!   `let`) properties, every one of them defaulted — so `Status()` is a
//!   usable zero value — using Swift's synthesized memberwise initializer
//!   rather than a hand-written one.
//! * A plain *closed* `enum` (§5) becomes a `RawRepresentable` Swift `enum`
//!   backed by its wire integer — decoding an unmatched value is what
//!   `Swift.init?(rawValue:)` already expresses as failure, so `decode` just
//!   wraps that in a thrown error. An *open* one instead becomes a plain
//!   `enum` with one case per declared variant plus a case carrying the
//!   synthesized `raw` for anything else — a genuine sum type, so "declared,
//!   or not" is answered by which case a value is, not by a side table.
//! * A tagged union (§7) becomes an `enum` the same way: one case per
//!   declared variant, carrying its fields as labeled associated values, plus
//!   — for an open union — a case carrying the unrecognized id together with
//!   the undecoded raw payload.
//! * A `scaled` type (§4) is a `typealias` for its physical `Float`/`Double`,
//!   plus `<name>FromRaw`/`<name>ToRaw` functions that keep the underlying
//!   wire integer reachable for callers that want to round-trip without
//!   floating-point rounding.
//! * A variable-length field (§6.3) is a native `String` or `Array`, as §12
//!   asks. Decode fails on malformed UTF-8 rather than substituting
//!   replacement characters.
//! * Failures are one `DefgenError` enum with a case per kind, so a caller
//!   can catch the lot with one `catch`.
//!
//! # Bit and byte order
//!
//! A container's bits live in a `[UInt8]` buffer sized to its exact byte
//! length; `DefgenBits` maps a logical bit index — LSB-first from bit 0 (§6)
//! — to the physical byte holding it, reversing for a big-endian root (§8).
//! Every read and write goes through it, and every value it hands back or
//! accepts is a `UInt128`, wide enough for any single field (§2) without a
//! narrower/wider split the way a fixed-width-only language like C needs.

use super::{Backend, Generated, GeneratedFile, Options, camel, sanitize_stem, screaming};
use crate::ast::{Docs, Endianness, FloatType, Property};
use crate::model::{
    Enum, Field, FieldRole, Model, Scaled, Struct, TypeDef, TypeKind, Union, WireType, carrier_bits,
};

pub struct SwiftBackend;

impl Backend for SwiftBackend {
    fn name(&self) -> &'static str {
        "swift"
    }

    fn description(&self) -> &'static str {
        "a single self-contained Swift file (Swift 6+)"
    }

    fn generate(&self, model: &Model, opts: &Options) -> Generated {
        let stem = sanitize_stem(&opts.stem);
        let file = GeneratedFile {
            name: format!("{stem}.swift"),
            contents: Emitter::new(model, opts.source.as_deref()).run(),
        };
        Generated { files: vec![file] }
    }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------
//
// [`snake`], [`screaming`] and [`camel`] (`activeProfile`, the property and
// case naming convention this backend, Kotlin's and Java's all share) are
// defined in [`super`] so every backend derives them the same way.

/// Every Swift hard keyword — never legal as a plain identifier. Contextual
/// keywords (`get`, `set`, `didSet`, ...) are deliberately excluded: Swift
/// itself allows those as ordinary identifiers outside the one declaration
/// context that gives them meaning, so escaping them would only be noise. A
/// schema name shaped like a hard keyword is perfectly legal (§1 does not
/// reserve Swift's vocabulary), so a collision gets a trailing `_` rather
/// than a backtick: a uniform rule across every backend is one fewer thing to
/// get wrong per name.
#[rustfmt::skip]
const SWIFT_KEYWORDS: &[&str] = &[
    "associatedtype", "class", "deinit", "enum", "extension", "fileprivate", "func", "import",
    "init", "inout", "internal", "let", "open", "operator", "private", "precedencegroup",
    "protocol", "public", "rethrows", "static", "struct", "subscript", "typealias", "var",
    "break", "case", "catch", "continue", "default", "defer", "do", "else", "fallthrough", "for",
    "guard", "if", "in", "repeat", "return", "switch", "throw", "where", "while",
    "Any", "as", "false", "is", "nil", "self", "Self", "super", "throw", "throws", "true", "try",
];

/// Names the generated file uses for its own runtime, or that a generated
/// type's own members would collide with. A field named `size` or `encode`
/// would otherwise clash with a member the type already carries.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    "encode", "decode", "packFixed", "unpackFixed", "packTail", "unpackTail", "tailLen",
    "encodedSize", "raw", "rawValue",
    "size", "fixedSize", "maxSize", "id",
];

/// A schema name as a Swift identifier, escaped where it would collide with
/// the language's or the generated file's own vocabulary. Used for type names
/// (kept `PascalCase`, as the schema wrote them) and for tagged-union tag
/// names.
fn ident(name: &str) -> String {
    if SWIFT_KEYWORDS.contains(&name) || RESERVED.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

/// A field, case or parameter name as the Swift identifier it becomes:
/// `camel`-cased (§12 — Swift shares this convention with Kotlin and Java),
/// then escaped the same way a type name is.
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

/// The Swift type a `uN`/`iN` value of `bits` width is carried in: the
/// smallest of `UInt8`/`UInt16`/`UInt32`/`UInt64`/`UInt128` (unsigned for
/// `uN`) or their signed counterparts (for `iN`) that holds it. Swift 6's
/// native 128-bit integers mean there is no fallback to an arbitrary-
/// precision type the way a JVM backend needs `BigInteger` past 64 bits.
fn carrier_type(bits: u32, signed: bool) -> &'static str {
    match (signed, carrier_bits(bits)) {
        (false, 8) => "UInt8",
        (false, 16) => "UInt16",
        (false, 32) => "UInt32",
        (false, 64) => "UInt64",
        (false, 128) => "UInt128",
        (true, 8) => "Int8",
        (true, 16) => "Int16",
        (true, 32) => "Int32",
        (true, 64) => "Int64",
        (true, 128) => "Int128",
        _ => unreachable!("carrier_bits only returns 8, 16, 32, 64 or 128"),
    }
}

/// A `Float`/`Double` literal Swift reads back as the same value. Rust's
/// shortest representation round-trips; only the spelling of the non-finite
/// values differs from Rust's, and only `Float` needs a cast (a bare literal
/// defaults to `Double`).
fn float_lit(v: f64, physical: FloatType) -> String {
    let cast = |s: String| if physical == FloatType::F32 { format!("Float({s})") } else { s };
    if v.is_nan() {
        return cast(format!("{}.nan", if physical == FloatType::F32 { "Float" } else { "Double" }));
    }
    if v.is_infinite() {
        let ty = if physical == FloatType::F32 { "Float" } else { "Double" };
        let sign = if v < 0.0 { "-" } else { "" };
        return format!("{sign}{ty}.infinity");
    }
    let s = format!("{v:?}");
    let s = if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") };
    cast(s)
}

// ---------------------------------------------------------------------------
// Variable-length tails
// ---------------------------------------------------------------------------

/// How a field's variable-length tail, if it has one, is laid out — see
/// [`Emitter::tail_kind`].
enum TailKind {
    /// A native `String`/`Array` property of the containing type.
    Inline,
    /// A named type that owns the tail, and the methods that handle it.
    Nested,
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Runtime helpers the schema turned out to need. An unused helper is dead
/// weight in a file meant to be dropped whole into a project, so none of
/// these is unconditional.
#[derive(Default)]
struct Needs {
    /// A `scaled` type, which needs rounding (§4).
    round: bool,
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

    /// Doc comments as `///` (§1, §12). No escaping is needed the way a
    /// block comment (KDoc, Doxygen) requires: a `///` line has no closing
    /// sequence another line's text could prematurely trigger.
    fn docs(&mut self, ind: usize, docs: &Docs) {
        for doc in docs {
            if doc.text.is_empty() {
                self.line(ind, "///");
            } else {
                self.line(ind, &format!("/// {}", doc.text));
            }
        }
    }

    /// A `///` comment the backend wrote itself, wrapped to stay readable.
    fn note(&mut self, ind: usize, text: &str) {
        const WIDTH: usize = 92;
        let budget = WIDTH.saturating_sub(ind * 4);
        if text.len() + 4 <= budget {
            self.line(ind, &format!("/// {text}"));
            return;
        }
        let mut current = String::new();
        for word in text.split_whitespace() {
            if !current.is_empty() && current.len() + 1 + word.len() > budget {
                self.line(ind, &format!("/// {current}"));
                current.clear();
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        if !current.is_empty() {
            self.line(ind, &format!("/// {current}"));
        }
    }

    /// The schema's own doc comment, then — after a blank `///` line —
    /// whatever the backend has to say about the representation it chose.
    fn docs_with(&mut self, ind: usize, docs: &Docs, notes: &[String]) {
        self.docs(ind, docs);
        if !docs.is_empty() && !notes.is_empty() {
            self.line(ind, "///");
        }
        for note in notes {
            self.note(ind, note);
        }
    }

    /// A multi-line `///` comment the backend wrote itself, with no schema
    /// doc comment alongside it.
    fn docstring_note(&mut self, ind: usize, lines: &[String]) {
        self.docs_with(ind, &Docs::new(), lines);
    }

    // -- pre-pass -------------------------------------------------------------

    /// Works out which helpers the schema needs, before emitting the runtime
    /// section that has to declare them.
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
        if let FieldRole::Padding { .. } = f.role {
            return;
        }
        self.scan_type(&f.ty);
    }

    fn scan_type(&mut self, ty: &WireType) {
        match ty {
            WireType::UInt(_) | WireType::Int(_) | WireType::Bool | WireType::Named(_) => {}
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

    /// The Swift type a value of `ty` is held in. Aliases are deliberately
    /// *not* resolved here: the domain name the author declared is the point
    /// of one.
    fn swift_type(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) => carrier_type(*n, false).to_string(),
            WireType::Int(n) => carrier_type(*n, true).to_string(),
            WireType::Bool => "Bool".to_string(),
            WireType::Named(id) => ident(&self.m.get(*id).name),
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => {
                format!("[{}]", self.swift_type(elem))
            }
            WireType::Str { .. } => "String".to_string(),
        }
    }

    /// An expression building a fresh zero value of `ty`, used as a
    /// property's default so every generated type is constructible with no
    /// arguments (via Swift's synthesized memberwise initializer, once every
    /// stored property has a default).
    fn fresh(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(_) | WireType::Int(_) => "0".to_string(),
            WireType::Bool => "false".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
            WireType::VarArray { .. } => "[]".to_string(),
            WireType::Array { elem, count } => {
                format!("[{}](repeating: {}, count: {count})", self.swift_type(elem), self.fresh(elem))
            }
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => self.fresh(&a.target),
                    TypeKind::Scaled(_) => "0.0".to_string(),
                    TypeKind::Enum(e) => match (e.variants.first(), &e.else_arm) {
                        (Some(v), _) => format!("{name}.{}", field_ident(&v.name)),
                        (None, Some(_)) => format!("{name}.unknown(raw: 0)"),
                        (None, None) => "0".to_string(),
                    },
                    TypeKind::Union(u) => match (u.variants.first(), &u.else_arm) {
                        (Some(v), _) if v.fields.iter().any(Field::is_visible) => {
                            let args: Vec<String> = v
                                .fields
                                .iter()
                                .filter(|f| f.is_visible())
                                .filter_map(|f| {
                                    let n = f.name()?;
                                    Some(format!("{}: {}", field_ident(n), self.fresh(&f.ty)))
                                })
                                .collect();
                            format!("{name}.{}({})", field_ident(&v.name), args.join(", "))
                        }
                        (Some(v), _) => format!("{name}.{}", field_ident(&v.name)),
                        (None, Some(_)) => {
                            if u.payload_bits > 0 {
                                format!("{name}.unknown(id: 0, raw: 0)")
                            } else {
                                format!("{name}.unknown(id: 0)")
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
            WireType::Named(id) => self.m.get(*id).name.clone(),
            WireType::Array { elem, count } => format!("{}[{count}]", self.wire_str(elem)),
            WireType::VarArray { elem, max } => format!("{}[max: {max}]", self.wire_str(elem)),
            WireType::Str { max } => format!("string(max: {max})"),
        }
    }

    /// Whether unpacking `ty` can fail — i.e. whether its [`unpack_expr`]
    /// needs a `try`. Only a closed enum, a struct or a tagged union (always,
    /// uniformly — see the module doc) can.
    fn unpack_throws(&self, ty: &WireType) -> bool {
        match ty {
            WireType::UInt(_) | WireType::Int(_) | WireType::Bool => false,
            WireType::Named(id) => match &self.m.get(*id).kind {
                TypeKind::Alias(a) => self.unpack_throws(&a.target),
                TypeKind::Scaled(_) => false,
                TypeKind::Enum(e) => !e.is_open(),
                TypeKind::Union(_) | TypeKind::Struct(_) => true,
            },
            WireType::Array { elem, .. } => self.unpack_throws(elem),
            WireType::VarArray { .. } | WireType::Str { .. } => false,
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
        self.line(0, "//");
        self.line(0, &format!("// Generated by defgen{from}. Do not edit."));
        self.lines(
            0,
            &[
                "//",
                "// Codecs for this schema's GATT values: LSB-first bit packing (§6), with byte",
                "// order applied once per root container (§8). Encoding produces a `[UInt8]`;",
                "// decoding takes the bytes the transport delivered. Anything the schema does",
                "// not allow throws a `DefgenError`, rather than being quietly truncated,",
                "// wrapped or replaced.",
                "//",
                "// Only a type bound to a characteristic has `encode`/`decode`: byte order is a",
                "// property of the root container, so a type that is only ever nested has no",
                "// byte order of its own to be encoded in (§8).",
                "//",
                "// Requires Swift 6 or newer, for native `UInt128`/`Int128`. There are no",
                "// third-party dependencies, and nothing is imported.",
                "//",
            ],
        );
    }

    // ---------------------------------------------------------------------
    // Runtime
    // ---------------------------------------------------------------------

    fn runtime(&mut self) {
        self.banner("Errors");
        self.lines(
            0,
            &[
                "/// Every failure a generated codec can throw (§12): one type, so a caller can",
                "/// catch every kind with a single `catch`.",
                "enum DefgenError: Error, CustomStringConvertible {",
            ],
        );
        for (case, doc) in [
            ("length", "The buffer's length matches no legal encoding of the type (§6.3, §10)."),
            ("range", "A value does not fit the bits its field declares (§2, §4, §6.3)."),
            ("unknownValue", "A closed enum or tagged union met an undeclared value (§5, §7)."),
            ("padding", "A `padding: uN = 0` run was not zero on the wire (§6.2)."),
            ("utf8", "A `string` field's bytes are not well-formed UTF-8 (§6.3)."),
        ] {
            self.note(1, doc);
            self.line(1, &format!("case {case}(String)"));
        }
        self.blank();
        self.line(1, "var description: String {");
        self.line(2, "switch self {");
        self.line(
            2,
            "case .length(let m), .range(let m), .unknownValue(let m), .padding(let m), .utf8(let m):",
        );
        self.line(3, "return m");
        self.line(2, "}");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();

        self.banner("Runtime");
        self.lines(
            0,
            &[
                "/// A container's bytes, addressed as LSB-first bits (§6). Byte order (§8)",
                "/// enters only here — a big-endian container is the same bit sequence read",
                "/// from the far end of the buffer, so byte order is one flag on the container",
                "/// rather than something every field has to know about. Every value this reads",
                "/// or writes is a `UInt128`, wide enough for any single field (§2). Internal",
                "/// (module-only) access, not `private`: a nested type's own `packFixed` takes",
                "/// one as a parameter, so it has to be at least as visible as that method.",
                "final class DefgenBits {",
                "    private(set) var bytes: [UInt8]",
                "    private let big: Bool",
                "",
                "    /// A fresh all-zero container, `size` bytes wide — what a root value packs",
                "    /// itself into.",
                "    init(size: Int, big: Bool) {",
                "        self.bytes = [UInt8](repeating: 0, count: size)",
                "        self.big = big",
                "    }",
                "",
                "    /// The bytes the transport delivered, read in the given byte order.",
                "    init(data: [UInt8], big: Bool) {",
                "        self.bytes = data",
                "        self.big = big",
                "    }",
                "",
                "    private func byteIndex(_ bit: Int) -> Int {",
                "        let i = bit >> 3",
                "        return big ? (bytes.count - 1 - i) : i",
                "    }",
                "",
                "    /// The `bits` bits starting at `off`.",
                "    func get(_ off: Int, _ bits: Int) -> UInt128 {",
                "        var v: UInt128 = 0",
                "        for i in 0..<bits {",
                "            let bit = off + i",
                "            let byte = bytes[byteIndex(bit)]",
                "            v |= UInt128((byte >> (bit & 7)) & 1) << i",
                "        }",
                "        return v",
                "    }",
                "",
                "    /// Writes the low `bits` bits of `value` at `off`.",
                "    func put(_ off: Int, _ bits: Int, _ value: UInt128) {",
                "        for i in 0..<bits {",
                "            let bit = off + i",
                "            let idx = byteIndex(bit)",
                "            let mask: UInt8 = 1 << (bit & 7)",
                "            if (value >> i) & 1 == 1 {",
                "                bytes[idx] |= mask",
                "            } else {",
                "                bytes[idx] &= ~mask",
                "            }",
                "        }",
                "    }",
                "}",
                "",
                "/// Sign-extends an `iN` value from bit N-1 (§2).",
                "private func defgenSext(_ value: UInt128, _ bits: Int) -> Int128 {",
                "    if bits >= 128 { return Int128(bitPattern: value) }",
                "    let sign = UInt128(1) << (bits - 1)",
                "    return Int128(bitPattern: (value ^ sign) &- sign)",
                "}",
                "",
                "/// The unsigned two's-complement wire pattern of an already range-checked `iN`",
                "/// value, masked to `bits` width — the counterpart to `defgenSext`.",
                "private func defgenWirePattern(_ value: Int128, _ bits: Int) -> UInt128 {",
                "    let pattern = UInt128(bitPattern: value)",
                "    return bits >= 128 ? pattern : (pattern & ((UInt128(1) << bits) - 1))",
                "}",
                "",
                "/// Range-checks a `uN` value: out of range is an error, never a truncation (§2).",
                "@discardableResult",
                "private func defgenCheckUInt(_ value: UInt128, _ bits: Int, _ label: String) throws -> UInt128 {",
                "    if bits < 128 && value >= (UInt128(1) << bits) {",
                "        throw DefgenError.range(\"\\(label): \\(value) does not fit in u\\(bits)\")",
                "    }",
                "    return value",
                "}",
                "",
                "/// Range-checks an `iN` value (§2). Returns the value unchanged — call",
                "/// `defgenWirePattern` separately to get the bits a container wants.",
                "@discardableResult",
                "private func defgenCheckInt(_ value: Int128, _ bits: Int, _ label: String) throws -> Int128 {",
                "    if bits < 128 {",
                "        let limit = Int128(1) << (bits - 1)",
                "        if value < -limit || value >= limit {",
                "            throw DefgenError.range(\"\\(label): \\(value) does not fit in i\\(bits)\")",
                "        }",
                "    }",
                "    return value",
                "}",
                "",
                "/// Range-checks an already-rounded signed value against an *unsigned* `uN`",
                "/// range — used by a `scaled` type's raw side, where rounding a physical value",
                "/// can land negative even though the wire type cannot hold one.",
                "private func defgenCheckUIntFromRounded(_ value: Int128, _ bits: Int, _ label: String) throws -> UInt128 {",
                "    if value < 0 || (bits < 128 && value >= (Int128(1) << bits)) {",
                "        throw DefgenError.range(\"\\(label): \\(value) does not fit in u\\(bits)\")",
                "    }",
                "    return UInt128(value)",
                "}",
            ],
        );

        if self.needs.arrays {
            self.blank();
            self.lines(
                0,
                &[
                    "/// A fixed-size array carries exactly `count` elements, always (§6.1).",
                    "@discardableResult",
                    "private func defgenCheckCount<T>(_ seq: [T], _ count: Int, _ label: String) throws -> [T] {",
                    "    guard seq.count == count else {",
                    "        throw DefgenError.range(\"\\(label): expected exactly \\(count) elements, got \\(seq.count)\")",
                    "    }",
                    "    return seq",
                    "}",
                    "",
                    "/// A variable-length array carries at most `limit` elements (§6.3).",
                    "@discardableResult",
                    "private func defgenCheckMax<T>(_ seq: [T], _ limit: Int, _ label: String) throws -> [T] {",
                    "    guard seq.count <= limit else {",
                    "        throw DefgenError.range(\"\\(label): \\(seq.count) elements exceeds the maximum of \\(limit)\")",
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
                    "/// Strict UTF-8 validation for `string` decode (§6.3): rejects overlong",
                    "/// encodings, surrogates and anything above U+10FFFF. The spec requires",
                    "/// failing rather than substituting replacement characters.",
                    "private func defgenValidUtf8(_ bytes: [UInt8]) -> Bool {",
                    "    var i = 0",
                    "    while i < bytes.count {",
                    "        let c = bytes[i]",
                    "        if c < 0x80 { i += 1; continue }",
                    "        let need: Int",
                    "        var cp: UInt32",
                    "        if c & 0xE0 == 0xC0 { need = 1; cp = UInt32(c & 0x1F) }",
                    "        else if c & 0xF0 == 0xE0 { need = 2; cp = UInt32(c & 0x0F) }",
                    "        else if c & 0xF8 == 0xF0 { need = 3; cp = UInt32(c & 0x07) }",
                    "        else { return false }",
                    "        if bytes.count - i - 1 < need { return false }",
                    "        for k in 1...need {",
                    "            let cc = bytes[i + k]",
                    "            if cc & 0xC0 != 0x80 { return false }",
                    "            cp = (cp << 6) | UInt32(cc & 0x3F)",
                    "        }",
                    "        if need == 1 && cp < 0x80 { return false }",
                    "        if need == 2 && cp < 0x800 { return false }",
                    "        if need == 3 && cp < 0x10000 { return false }",
                    "        if cp > 0x10FFFF { return false }",
                    "        if cp >= 0xD800 && cp <= 0xDFFF { return false }",
                    "        i += need + 1",
                    "    }",
                    "    return true",
                    "}",
                    "",
                    "/// A `string` field's bytes, rejecting anything past its `max` (§6.3).",
                    "private func defgenEncodeUtf8(_ text: String, _ limit: Int, _ label: String) throws -> [UInt8] {",
                    "    let data = Array(text.utf8)",
                    "    guard data.count <= limit else {",
                    "        throw DefgenError.range(\"\\(label): \\(data.count) bytes exceeds the maximum of \\(limit)\")",
                    "    }",
                    "    return data",
                    "}",
                    "",
                    "/// Decodes a `string` field. Malformed input fails rather than being patched",
                    "/// up with replacement characters (§6.3).",
                    "private func defgenDecodeUtf8(_ data: [UInt8], _ label: String) throws -> String {",
                    "    guard defgenValidUtf8(data) else {",
                    "        throw DefgenError.utf8(\"\\(label): invalid UTF-8\")",
                    "    }",
                    "    return String(decoding: data, as: UTF8.self)",
                    "}",
                ],
            );
        }

        if self.needs.round {
            self.blank();
            self.lines(
                0,
                &[
                    "/// Rounds half away from zero, which is what every other backend's",
                    "/// `scaled` rounding does too (§4, §13) — Swift's own `.rounded()` supports",
                    "/// this rule directly, as `.toNearestOrAwayFromZero`.",
                    "private func defgenRound(_ value: Double, _ label: String) throws -> Double {",
                    "    guard value.isFinite else {",
                    "        throw DefgenError.range(\"\\(label): \\(value) cannot be rounded to an integer\")",
                    "    }",
                    "    return value.rounded(.toNearestOrAwayFromZero)",
                    "}",
                    "",
                    "/// A rounded `Double` as an exact `Int128` — the shared last step before a",
                    "/// `scaled` type's raw-side range check, whichever signedness it checks",
                    "/// against.",
                    "private func defgenRoundedToInt128(_ rounded: Double, _ label: String) throws -> Int128 {",
                    "    guard let value = Int128(exactly: rounded) else {",
                    "        throw DefgenError.range(\"\\(label): \\(rounded) cannot be rounded to an integer\")",
                    "    }",
                    "    return value",
                    "}",
                ],
            );
        }
    }

    // ---------------------------------------------------------------------
    // Declarations
    // ---------------------------------------------------------------------

    /// Types in source order. §9 forbids forward references, so source order
    /// is already a valid definition order for a file where later types
    /// reference earlier ones by name.
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

    /// Whether `def` is bound to a characteristic (§10) but has no type of
    /// its own to hang `encode`/`decode` off — an alias, a `scaled` type or
    /// an enum, all of which get module-level codec functions instead.
    fn has_entry_functions(&self, def: &TypeDef) -> bool {
        def.root && !matches!(def.kind, TypeKind::Struct(_) | TypeKind::Union(_))
    }

    // -- alias (§3) -----------------------------------------------------------

    fn declare_alias(&mut self, def: &'m TypeDef, target: &'m WireType) {
        let name = ident(&def.name);
        let ty = self.swift_type(target);
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
        self.line(0, &format!("let {prefix}_SCALE: {physical} = {}", float_lit(s.scale, s.physical)));
        self.line(0, &format!("let {prefix}_OFFSET: {physical} = {}", float_lit(s.offset, s.physical)));
        self.blank();

        self.note(0, "Decodes the raw wire integer into the physical value (§4).");
        self.line(0, &format!("func {fnp}FromRaw(_ raw: {raw_ty}) -> {name} {{"));
        self.line(
            1,
            &format!("let physical = Double(raw) * Double({prefix}_SCALE) + Double({prefix}_OFFSET)"),
        );
        let cast = if s.physical == FloatType::F32 { "Float(physical)" } else { "physical" };
        self.line(1, &format!("return {cast}"));
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
        self.line(0, &format!("func {fnp}ToRaw(_ value: {name}) throws -> {raw_ty} {{"));
        self.line(
            1,
            &format!(
                "let rounded = try defgenRound((Double(value) - Double({prefix}_OFFSET)) / Double({prefix}_SCALE), \"{}\")",
                def.name
            ),
        );
        self.line(1, &format!("let asInt = try defgenRoundedToInt128(rounded, \"{}\")", def.name));
        if s.signed {
            self.line(
                1,
                &format!("let checked = try defgenCheckInt(asInt, {}, \"{}\")", s.raw_bits, def.name),
            );
        } else {
            self.line(
                1,
                &format!(
                    "let checked = try defgenCheckUIntFromRounded(asInt, {}, \"{}\")",
                    s.raw_bits, def.name
                ),
            );
        }
        self.line(1, &format!("return {raw_ty}(checked)"));
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
        self.line(0, &format!("enum {name}: {carrier} {{"));
        for v in &e.variants {
            self.docs(1, &v.docs);
            self.line(1, &format!("case {} = {}", field_ident(&v.name), v.value));
        }
        self.blank();
        self.note(1, "The variant `raw` names; an unmatched value is an error (§5).");
        self.line(1, &format!("static func decode(_ raw: {carrier}) throws -> {name} {{"));
        self.line(2, &format!("guard let value = {name}(rawValue: raw) else {{"));
        self.line(
            3,
            &format!("throw DefgenError.unknownValue(\"{}: \\(raw) matches no declared variant\")", def.name),
        );
        self.line(2, "}");
        self.line(2, "return value");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();
    }

    /// An *open* enum (§5) is a plain `enum` with associated values rather
    /// than a `RawRepresentable` one: one case per declared variant, plus a
    /// `case unknown(raw:)` for anything else. This is a genuine sum type —
    /// unlike Python's `IntEnum` + `Union` type alias, `HearingMode` already
    /// covers every wire value with no separate value alias needed, and
    /// matching on it is exhaustive `switch`.
    fn declare_open_enum(&mut self, def: &'m TypeDef, e: &'m Enum) {
        let name = ident(&def.name);
        let bits = e.backing_bits;
        let carrier = carrier_type(bits, false);
        let arm = e.else_arm.as_ref().expect("declare_open_enum called on a closed enum");
        let arm_ident = field_ident(&arm.name);

        let notes = vec![
            format!("{} {bits}-bit wire value (§5).", article(bits).to_uppercase()),
            format!(
                "Open: a value matching none of the variants below decodes to `.{arm_ident}(raw:)` \
                 instead of failing, so decoding this enum never fails."
            ),
        ];
        self.docs_with(0, &def.docs, &notes);
        self.line(0, &format!("enum {name}: Equatable {{"));
        for v in &e.variants {
            self.docs(1, &v.docs);
            self.line(1, &format!("case {}", field_ident(&v.name)));
        }
        self.docs_with(
            1,
            &arm.docs,
            &[format!(
                "A wire value `{name}` does not declare (§5). It keeps the value it was \
                 decoded from, so re-encoding it is lossless."
            )],
        );
        self.line(1, &format!("case {arm_ident}(raw: {carrier})"));
        self.blank();

        self.note(1, "The wire value this case encodes to (§5).");
        self.line(1, &format!("var raw: {carrier} {{"));
        self.line(2, "switch self {");
        for v in &e.variants {
            self.line(3, &format!("case .{}: return {}", field_ident(&v.name), v.value));
        }
        self.line(3, &format!("case .{arm_ident}(let raw): return raw"));
        self.line(2, "}");
        self.line(1, "}");
        self.blank();

        self.note(1, &format!("The variant `raw` names, or `.{arm_ident}` (§5)."));
        self.line(1, &format!("static func decode(_ raw: {carrier}) -> {name} {{"));
        self.line(2, "switch raw {");
        for v in &e.variants {
            self.line(3, &format!("case {}: return .{}", v.value, field_ident(&v.name)));
        }
        self.line(3, &format!("default: return .{arm_ident}(raw: raw)"));
        self.line(2, "}");
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
                "Every value is one of `{name}`'s cases, so a decoded command is matched with \
                 `switch`, never by inspecting a tag by hand."
            ),
        ];
        if !u.is_open() {
            notes.push("An id matching no variant is a hard decode error.".to_string());
        }
        self.docs_with(0, &def.docs, &notes);
        self.line(0, &format!("enum {name}: Equatable {{"));
        for v in &u.variants {
            let visible: Vec<&Field> = v.fields.iter().filter(|f| f.is_visible()).collect();
            self.docs_with(1, &v.docs, &[format!("Wire id `0x{:x}` (§7).", v.id)]);
            if visible.is_empty() {
                self.line(1, &format!("case {}", field_ident(&v.name)));
            } else {
                let params: Vec<String> = visible
                    .iter()
                    .map(|f| format!("{}: {}", field_ident(f.name().unwrap()), self.swift_type(&f.ty)))
                    .collect();
                self.line(1, &format!("case {}({})", field_ident(&v.name), params.join(", ")));
            }
        }
        if let Some(arm) = &u.else_arm {
            let notes = vec![format!(
                "An id `{name}` does not declare (§7). Both the id and the undecoded payload \
                 are kept, so re-encoding is lossless and an unknown command can never be \
                 mistaken for a known one."
            )];
            self.docs_with(1, &arm.docs, &notes);
            if arm.raw_bits > 0 {
                self.line(
                    1,
                    &format!(
                        "case {}(id: {tag_carrier}, raw: {})",
                        field_ident(&arm.name),
                        carrier_type(arm.raw_bits, false)
                    ),
                );
            } else {
                self.line(1, &format!("case {}(id: {tag_carrier})", field_ident(&arm.name)));
            }
        }
        self.blank();

        self.line(1, &format!("static let size: Int = {size}"));
        self.blank();

        // -- packFixed --
        self.note(1, "Packs this value's fixed part into `bits`, at bit `off`.");
        self.line(1, "func packFixed(_ bits: DefgenBits, _ off: Int) throws {");
        self.line(2, "switch self {");
        for v in &u.variants {
            let visible: Vec<&Field> = v.fields.iter().filter(|f| f.is_visible()).collect();
            let label = format!("{}.{}", def.name, v.name);
            if visible.is_empty() {
                self.line(2, &format!("case .{}:", field_ident(&v.name)));
            } else {
                let binds: Vec<String> =
                    visible.iter().map(|f| format!("let {}", field_ident(f.name().unwrap()))).collect();
                self.line(2, &format!("case .{}({}):", field_ident(&v.name), binds.join(", ")));
            }
            self.line(3, &format!("bits.put(off, {tag_bits}, {})", v.id));
            for f in &v.fields {
                let Some(fname) = f.name() else { continue };
                let expr = field_ident(fname);
                let field_off = Self::off("off", tag_bits + f.offset_bits);
                let field_label = format!("{label}.{fname}");
                self.pack(3, &expr, &f.ty, &field_off, &field_label);
            }
        }
        if let Some(arm) = &u.else_arm {
            let label = format!("{}.{}", def.name, arm.name);
            if arm.raw_bits > 0 {
                self.line(2, &format!("case .{}(let id, let raw):", field_ident(&arm.name)));
                self.line(
                    3,
                    &format!(
                        "bits.put(off, {tag_bits}, try defgenCheckUInt(UInt128(id), {tag_bits}, \"{label}\"))"
                    ),
                );
                let raw_off = Self::off("off", tag_bits);
                self.line(
                    3,
                    &format!(
                        "bits.put({raw_off}, {}, try defgenCheckUInt(UInt128(raw), {}, \"{label}.raw\"))",
                        arm.raw_bits, arm.raw_bits
                    ),
                );
            } else {
                self.line(2, &format!("case .{}(let id):", field_ident(&arm.name)));
                self.line(
                    3,
                    &format!(
                        "bits.put(off, {tag_bits}, try defgenCheckUInt(UInt128(id), {tag_bits}, \"{label}\"))"
                    ),
                );
            }
        }
        // A closed union's cases already cover every declared variant
        // exhaustively (§7): there is no `.unknown` case to add a branch for.
        self.line(2, "}");
        self.line(1, "}");
        self.blank();

        // -- unpackFixed --
        self.note(1, "Reads the id at `off` and dispatches to the variant it names (§7).");
        self.line(1, &format!("static func unpackFixed(_ bits: DefgenBits, _ off: Int) throws -> {name} {{"));
        self.line(2, &format!("let tag = {tag_carrier}(bits.get(off, {tag_bits}))"));
        self.line(2, "switch tag {");
        for v in &u.variants {
            let label = format!("{}.{}", def.name, v.name);
            self.line(2, &format!("case {}:", v.id));
            for f in &v.fields {
                self.padding_check(3, f, tag_bits, &label);
            }
            let args: Vec<(String, String)> = v
                .fields
                .iter()
                .filter(|f| f.is_visible())
                .filter_map(|f| {
                    let fname = f.name()?;
                    let field_off = Self::off("off", tag_bits + f.offset_bits);
                    Some((field_ident(fname), self.unpack_expr(&f.ty, &field_off)))
                })
                .collect();
            self.emit_return_call(3, &format!(".{}", field_ident(&v.name)), &args, false);
        }
        match &u.else_arm {
            Some(arm) => {
                self.line(2, "default:");
                if arm.raw_bits > 0 {
                    let raw_off = Self::off("off", tag_bits);
                    let raw_ty = carrier_type(arm.raw_bits, false);
                    self.line(3, &format!("let raw = {raw_ty}(bits.get({raw_off}, {}))", arm.raw_bits));
                    self.emit_return_call(
                        3,
                        &format!(".{}", field_ident(&arm.name)),
                        &[("id".to_string(), "tag".to_string()), ("raw".to_string(), "raw".to_string())],
                        false,
                    );
                } else {
                    self.emit_return_call(
                        3,
                        &format!(".{}", field_ident(&arm.name)),
                        &[("id".to_string(), "tag".to_string())],
                        false,
                    );
                }
            }
            None => {
                self.line(2, "default:");
                self.line(
                    3,
                    &format!(
                        "throw DefgenError.unknownValue(\"{}: id \\(tag) matches no declared variant\")",
                        def.name
                    ),
                );
            }
        }
        self.line(2, "}");
        self.line(1, "}");
        self.blank();

        if def.root {
            self.note(
                1,
                &format!(
                    "Encodes this `{}` into exactly {size} bytes, {}-endian.",
                    def.name,
                    def.endian.as_str()
                ),
            );
            self.line(1, "func encode() throws -> [UInt8] {");
            self.line(2, &format!("let bits = DefgenBits(size: {size}, big: {})", swift_bool(big)));
            self.line(2, "try packFixed(bits, 0)");
            self.line(2, "return bits.bytes");
            self.line(1, "}");
            self.blank();

            self.note(1, &format!("Decodes exactly {size} bytes; any other length is an error."));
            self.line(1, &format!("static func decode(_ data: [UInt8]) throws -> {name} {{"));
            self.line(2, &format!("guard data.count == {size} else {{"));
            self.line(
                3,
                &format!(
                    "throw DefgenError.length(\"{}: expected {size} bytes, got \\(data.count)\")",
                    def.name
                ),
            );
            self.line(2, "}");
            self.line(2, &format!("let bits = DefgenBits(data: data, big: {})", swift_bool(big)));
            self.line(2, "return try unpackFixed(bits, 0)");
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
        self.line(0, &format!("struct {name}: Equatable {{"));

        let visible: Vec<&'m Field> = s.fields.iter().filter(|f| f.is_visible()).collect();
        for f in &visible {
            let fname = f.name().unwrap();
            self.field_doc(1, f);
            let kw = if matches!(f.role, FieldRole::Reserved { .. }) { "let" } else { "var" };
            self.line(1, &format!("{kw} {}: {}", field_ident(fname), self.swift_type(&f.ty)));
        }
        if !visible.is_empty() {
            self.blank();
            // A `let` property is excluded from Swift's synthesized memberwise
            // initializer the moment it carries a default value at its own
            // declaration (the compiler treats it as already initialized) —
            // which is exactly what a `reserved` field needs a default for
            // (§6.2). A hand-written initializer sidesteps that entirely:
            // every property, `let` or `var`, is just a defaulted parameter.
            self.note(1, "Every property defaults to its zero value, so this is usable with no arguments.");
            self.line(1, "init(");
            for (i, f) in visible.iter().enumerate() {
                let fname = field_ident(f.name().unwrap());
                let comma = if i + 1 == visible.len() { "" } else { "," };
                self.line(2, &format!("{fname}: {} = {}{comma}", self.swift_type(&f.ty), self.fresh(&f.ty)));
            }
            self.line(1, ") {");
            for f in &visible {
                let fname = field_ident(f.name().unwrap());
                self.line(2, &format!("self.{fname} = {fname}"));
            }
            self.line(1, "}");
            self.blank();
        }

        match def.layout.tail {
            Some(_) => {
                self.line(1, &format!("static let fixedSize: Int = {}", def.layout.fixed_bytes()));
                self.line(1, &format!("static let maxSize: Int = {}", def.layout.max_bytes()));
            }
            // A nested-only container is free to be sub-byte (§6); only a
            // bound one has to be a whole number of bytes (§10), so a byte
            // size is only stated where it is exact.
            None if def.layout.is_byte_aligned() => {
                self.line(1, &format!("static let size: Int = {}", def.layout.fixed_bytes()));
            }
            None => {}
        }
        self.blank();

        if def.root {
            let size = def.layout.fixed_bytes();
            match def.layout.tail {
                None => {
                    self.note(1, &format!("Decodes exactly {size} bytes; any other length is an error."));
                    self.line(1, &format!("static func decode(_ data: [UInt8]) throws -> {name} {{"));
                    self.line(2, &format!("guard data.count == {size} else {{"));
                    self.line(
                        3,
                        &format!(
                            "throw DefgenError.length(\"{}: expected {size} bytes, got \\(data.count)\")",
                            def.name
                        ),
                    );
                    self.line(2, "}");
                    self.line(2, &format!("let bits = DefgenBits(data: data, big: {})", swift_bool(big)));
                    self.line(2, "return try unpackFixed(bits, 0)");
                    self.line(1, "}");
                }
                Some(_) => {
                    self.docstring_note(
                        1,
                        &[
                            format!(
                                "Decodes the bytes the transport delivered into a `{}` (§6.3).",
                                def.name
                            ),
                            "The tail's length comes from the buffer, never from the payload \
                             itself, so a length outside fixedSize...maxSize is an error."
                                .to_string(),
                        ],
                    );
                    self.line(1, &format!("static func decode(_ data: [UInt8]) throws -> {name} {{"));
                    self.line(2, "guard data.count >= fixedSize && data.count <= maxSize else {");
                    self.raise(
                        3,
                        "length",
                        &format!(
                            "\"{}: expected \\(fixedSize)...\\(maxSize) bytes, got \\(data.count)\"",
                            def.name
                        ),
                    );
                    self.line(2, "}");
                    self.line(
                        2,
                        &format!(
                            "let bits = DefgenBits(data: Array(data[0..<fixedSize]), big: {})",
                            swift_bool(big)
                        ),
                    );
                    self.line(2, "var value = try unpackFixed(bits, 0)");
                    self.line(
                        2,
                        &format!("try value.unpackTail(Array(data[fixedSize...]), {})", swift_bool(big)),
                    );
                    self.line(2, "return value");
                    self.line(1, "}");
                }
            }
            self.blank();
        }

        self.note(1, "Unpacks the fixed part from `bits`, at bit `off`.");
        self.line(1, &format!("static func unpackFixed(_ bits: DefgenBits, _ off: Int) throws -> {name} {{"));
        for f in &s.fields {
            self.padding_check(2, f, 0, &def.name);
        }
        let args: Vec<(String, String)> = s
            .fields
            .iter()
            .filter(|f| !matches!(self.tail_kind(&f.ty), Some(TailKind::Inline)))
            .filter_map(|f| {
                let fname = f.name()?;
                let off = Self::off("off", f.offset_bits);
                Some((field_ident(fname), self.unpack_expr(&f.ty, &off)))
            })
            .collect();
        self.emit_return_call(2, &name, &args, true);
        self.line(1, "}");
        self.blank();

        self.note(1, "Packs the fixed part into `bits`, at bit `off`.");
        self.line(1, "func packFixed(_ bits: DefgenBits, _ off: Int) throws {");
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
                            "Encodes this `{}` into exactly size ({size}) bytes, {}-endian.",
                            def.name,
                            def.endian.as_str()
                        ),
                    );
                    self.line(1, "func encode() throws -> [UInt8] {");
                    self.line(2, &format!("let bits = DefgenBits(size: {size}, big: {})", swift_bool(big)));
                    self.line(2, "try packFixed(bits, 0)");
                    self.line(2, "return bits.bytes");
                    self.line(1, "}");
                }
                Some(_) => {
                    self.docstring_note(
                        1,
                        &[
                            format!("Encodes this `{}`, {}-endian (§8).", def.name, def.endian.as_str()),
                            "The result is exactly as long as the value is — fixedSize to \
                             maxSize, never padded out to the maximum (§6.3)."
                                .to_string(),
                        ],
                    );
                    self.line(1, "func encode() throws -> [UInt8] {");
                    self.line(2, &format!("let bits = DefgenBits(size: {size}, big: {})", swift_bool(big)));
                    self.line(2, "try packFixed(bits, 0)");
                    self.line(2, &format!("return bits.bytes + (try packTail({}))", swift_bool(big)));
                    self.line(1, "}");
                    self.blank();
                    self.note(1, "Bytes this value encodes to as it stands — never padded out (§6.3).");
                    self.line(1, "func encodedSize() -> Int {");
                    self.line(2, &format!("{name}.fixedSize + tailLen()"));
                    self.line(1, "}");
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

    /// One property of a generated `struct`, with its doc comment and — for
    /// a `reserved` field or a variable-length one — the note that explains
    /// it.
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

    /// `padding: uN = 0` is validated on decode; bare `padding` is not (§6.2).
    fn padding_check(&mut self, ind: usize, f: &Field, base_bits: u32, owner: &str) {
        let FieldRole::Padding { check_zero: true } = f.role else { return };
        let off = Self::off("off", base_bits + f.offset_bits);
        let bits = f.layout.fixed_bits;
        let (from, to) = (f.offset_bits, f.offset_bits + bits);
        self.line(ind, &format!("if bits.get({off}, {bits}) != 0 {{"));
        self.raise(ind + 1, "padding", &format!("\"{owner}: padding at bits {from}..{to} is not zero\""));
        self.line(ind, "}");
    }

    /// A `throw` of one of the module's errors.
    fn raise(&mut self, ind: usize, case: &str, message: &str) {
        self.line(ind, &format!("throw DefgenError.{case}({message})"));
    }

    /// Emits `return Callee(label: expr, ...)`, one argument per line. With
    /// no args, emits `return Callee()` when `parens_if_empty` (a struct's
    /// synthesized initializer always needs the call parens) or bare `return
    /// Callee` otherwise (an enum case with no associated values is a plain
    /// value, not an "unapplied function" — `.someCase()` is a type error
    /// where `.someCase` is required).
    fn emit_return_call(
        &mut self,
        ind: usize,
        callee: &str,
        args: &[(String, String)],
        parens_if_empty: bool,
    ) {
        if args.is_empty() {
            let call = if parens_if_empty { format!("{callee}()") } else { callee.to_string() };
            self.line(ind, &format!("return {call}"));
            return;
        }
        self.line(ind, &format!("return {callee}("));
        for (i, (label, expr)) in args.iter().enumerate() {
            let comma = if i + 1 == args.len() { "" } else { "," };
            self.line(ind + 1, &format!("{label}: {expr}{comma}"));
        }
        self.line(ind, ")");
    }

    // ---------------------------------------------------------------------
    // Entry points (§12)
    // ---------------------------------------------------------------------

    /// The module-level codec an `alias`, `scaled` or `enum` bound to a
    /// characteristic gets (§10): none of the three is a type with methods
    /// of its own to hang `encode`/`decode` off.
    fn entry_functions(&mut self, def: &'m TypeDef) {
        let name = ident(&def.name);
        let prefix = screaming(&def.name);
        let big = def.endian == Endianness::Big;
        let order = def.endian.as_str();
        let size = def.layout.fixed_bytes();
        let sw_ty = self.swift_type(&WireType::Named(def.id));
        let target = self.resolve(&WireType::Named(def.id));

        if def.layout.tail.is_none() {
            self.note(0, &format!("Encoded size of a `{}`, in bytes.", def.name));
            self.line(0, &format!("let {prefix}_SIZE: Int = {size}"));
            self.blank();

            self.note(0, &format!("Encodes a `{}` into exactly {size} bytes, {order}-endian.", def.name));
            self.line(0, &format!("func encode{name}(_ value: {sw_ty}) throws -> [UInt8] {{"));
            self.line(1, &format!("let bits = DefgenBits(size: {size}, big: {})", swift_bool(big)));
            self.pack(1, "value", &target, "0", &def.name);
            self.line(1, "return bits.bytes");
            self.line(0, "}");
            self.blank();

            self.note(0, &format!("Decodes exactly {size} bytes into a `{}`.", def.name));
            self.line(0, &format!("func decode{name}(_ data: [UInt8]) throws -> {sw_ty} {{"));
            self.line(1, &format!("guard data.count == {size} else {{"));
            self.raise(2, "length", &format!("\"{}: expected {size} bytes, got \\(data.count)\"", def.name));
            self.line(1, "}");
            self.line(1, &format!("let bits = DefgenBits(data: data, big: {})", swift_bool(big)));
            let value = self.unpack_expr(&target, "0");
            self.line(1, &format!("return {value}"));
            self.line(0, "}");
            self.blank();
            return;
        }

        self.note(0, "Bytes always present, before the variable-length tail (§6.3).");
        self.line(0, &format!("let {prefix}_FIXED_SIZE: Int = {size}"));
        self.note(0, "Largest legal encoding — what a receive buffer must hold.");
        self.line(0, &format!("let {prefix}_MAX_SIZE: Int = {}", def.layout.max_bytes()));
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
        self.line(0, &format!("func encode{name}(_ value: {sw_ty}) throws -> [UInt8] {{"));
        match &target {
            WireType::Str { max } => {
                self.line(1, &format!("return try defgenEncodeUtf8(value, {max}, \"{}\")", def.name));
            }
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(1, "var out: [UInt8] = []");
                self.line(
                    1,
                    &format!("for elemVal in try defgenCheckMax(value, {max}, \"{}\") {{", def.name),
                );
                self.line(2, &format!("let bits = DefgenBits(size: {bytes}, big: {})", swift_bool(big)));
                self.pack(2, "elemVal", elem, "0", &def.name);
                self.line(2, "out += bits.bytes");
                self.line(1, "}");
                self.line(1, "return out");
            }
            WireType::Named(id) => {
                let cls = ident(&self.m.get(*id).name);
                self.line(
                    1,
                    &format!("let bits = DefgenBits(size: {prefix}_FIXED_SIZE, big: {})", swift_bool(big)),
                );
                self.line(1, "try value.packFixed(bits, 0)");
                self.line(1, &format!("return bits.bytes + (try value.packTail({}))", swift_bool(big)));
                let _ = cls;
            }
            _ => self.line(1, "return value"),
        }
        self.line(0, "}");
        self.blank();

        self.note(0, &format!("Decodes the bytes the transport delivered into a `{}` (§6.3).", def.name));
        self.line(0, &format!("func decode{name}(_ data: [UInt8]) throws -> {sw_ty} {{"));
        self.line(
            1,
            &format!("guard data.count >= {prefix}_FIXED_SIZE && data.count <= {prefix}_MAX_SIZE else {{"),
        );
        self.raise(
            2,
            "length",
            &format!(
                "\"{}: expected \\({prefix}_FIXED_SIZE)...\\({prefix}_MAX_SIZE) bytes, got \\(data.count)\"",
                def.name
            ),
        );
        self.line(1, "}");
        self.tail_entry_body(1, &target, &def.name, &prefix, big);
        self.line(0, "}");
        self.blank();
    }

    /// The body of a `decode<Name>` for a variable-length type bound
    /// straight to a characteristic (§6.3), which has no type of its own to
    /// hold tail methods.
    fn tail_entry_body(&mut self, ind: usize, target: &WireType, name: &str, prefix: &str, big: bool) {
        match target {
            WireType::Str { .. } => self.line(ind, &format!("return try defgenDecodeUtf8(data, \"{name}\")")),
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(ind, &format!("guard data.count % {bytes} == 0 else {{"));
                self.raise(
                    ind + 1,
                    "length",
                    &format!(
                        "\"{name}: \\(data.count) bytes is not a whole number of {bytes}-byte elements\""
                    ),
                );
                self.line(ind, "}");
                self.line(ind, &format!("return try (0..<(data.count / {bytes})).map {{ i in"));
                self.line(
                    ind + 1,
                    &format!(
                        "let bits = DefgenBits(data: Array(data[(i * {bytes})..<((i + 1) * {bytes})]), big: {})",
                        swift_bool(big)
                    ),
                );
                let elem_expr = self.unpack_expr(elem, "0");
                self.line(ind + 1, &format!("return {elem_expr}"));
                self.line(ind, "}");
            }
            // A named type that owns the tail: a variable-length struct.
            WireType::Named(id) => {
                let cls = ident(&self.m.get(*id).name);
                self.line(
                    ind,
                    &format!(
                        "let bits = DefgenBits(data: Array(data[0..<{prefix}_FIXED_SIZE]), big: {})",
                        swift_bool(big)
                    ),
                );
                self.line(ind, &format!("var value = try {cls}.unpackFixed(bits, 0)"));
                self.line(
                    ind,
                    &format!(
                        "try value.unpackTail(Array(data[{prefix}_FIXED_SIZE...]), {})",
                        swift_bool(big)
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
                self.line(
                    ind,
                    &format!("bits.put({off}, {n}, try defgenCheckUInt(UInt128({expr}), {n}, \"{label}\"))"),
                );
            }
            WireType::Int(n) => {
                self.line(
                    ind,
                    &format!(
                        "bits.put({off}, {n}, defgenWirePattern(try defgenCheckInt(Int128({expr}), {n}, \"{label}\"), {n}))"
                    ),
                );
            }
            WireType::Bool => {
                self.line(ind, &format!("bits.put({off}, 1, {expr} ? 1 : 0)"));
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
                        let raw_expr = format!("try {}ToRaw({expr})", lower_first(&name));
                        if s.signed {
                            self.line(
                                ind,
                                &format!(
                                    "bits.put({off}, {}, defgenWirePattern(try defgenCheckInt(Int128({raw_expr}), {}, \"{label}\"), {}))",
                                    s.raw_bits, s.raw_bits, s.raw_bits
                                ),
                            );
                        } else {
                            self.line(
                                ind,
                                &format!(
                                    "bits.put({off}, {}, try defgenCheckUInt(UInt128({raw_expr}), {}, \"{label}\"))",
                                    s.raw_bits, s.raw_bits
                                ),
                            );
                        }
                    }
                    TypeKind::Enum(e) => {
                        let raw_access =
                            if e.is_open() { format!("{expr}.raw") } else { format!("{expr}.rawValue") };
                        self.line(
                            ind,
                            &format!("bits.put({off}, {}, UInt128({raw_access}))", e.backing_bits),
                        );
                    }
                    TypeKind::Struct(_) | TypeKind::Union(_) => {
                        self.line(ind, &format!("try {expr}.packFixed(bits, {off})"));
                    }
                }
            }
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                self.line(
                    ind,
                    &format!(
                        "for (elemIdx, elemVal) in (try defgenCheckCount({expr}, {count}, \"{label}\")).enumerated() {{"
                    ),
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

    /// The expression reading a value of `ty` out of `bits` at bit `off`,
    /// with `try` already embedded wherever [`unpack_throws`] says it needs
    /// one.
    fn unpack_expr(&self, ty: &WireType, off: &str) -> String {
        match ty {
            WireType::UInt(n) => format!("{}(bits.get({off}, {n}))", carrier_type(*n, false)),
            WireType::Int(n) => {
                format!("{}(defgenSext(bits.get({off}, {n}), {n}))", carrier_type(*n, true))
            }
            WireType::Bool => format!("(bits.get({off}, 1) != 0)"),
            WireType::Named(id) => {
                let def = self.m.get(*id);
                let name = ident(&def.name);
                match &def.kind {
                    TypeKind::Alias(a) => self.unpack_expr(&a.target, off),
                    TypeKind::Scaled(s) => {
                        let carrier_expr = if s.signed {
                            format!(
                                "{}(defgenSext(bits.get({off}, {}), {}))",
                                carrier_type(s.raw_bits, true),
                                s.raw_bits,
                                s.raw_bits
                            )
                        } else {
                            format!("{}(bits.get({off}, {}))", carrier_type(s.raw_bits, false), s.raw_bits)
                        };
                        format!("{}FromRaw({carrier_expr})", lower_first(&name))
                    }
                    TypeKind::Enum(e) => {
                        let carrier_expr = format!(
                            "{}(bits.get({off}, {}))",
                            carrier_type(e.backing_bits, false),
                            e.backing_bits
                        );
                        if e.is_open() {
                            format!("{name}.decode({carrier_expr})")
                        } else {
                            format!("try {name}.decode({carrier_expr})")
                        }
                    }
                    TypeKind::Struct(_) | TypeKind::Union(_) => {
                        format!("try {name}.unpackFixed(bits, {off})")
                    }
                }
            }
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                let elem_off = format!("({off} + i * {elem_bits})");
                let body = self.unpack_expr(elem, &elem_off);
                if self.unpack_throws(elem) {
                    format!("try (0..<{count}).map {{ i in {body} }}")
                } else {
                    format!("(0..<{count}).map {{ i in {body} }}")
                }
            }
            // Read by the tail code; the fixed part leaves a placeholder.
            WireType::VarArray { .. } => "[]".to_string(),
            WireType::Str { .. } => "\"\"".to_string(),
        }
    }

    // ---------------------------------------------------------------------
    // Variable-length tails (§6.3)
    // ---------------------------------------------------------------------

    /// How a field's variable-length tail, if it has one, is laid out:
    /// inline in the containing struct, or owned by a named type that has
    /// tail methods of its own.
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
        self.note(1, "Bytes this value's variable-length tail occupies.");
        self.line(1, "func tailLen() -> Int {");
        let len_expr = match ty {
            WireType::Str { .. } => format!("{prop}.utf8.count"),
            WireType::VarArray { elem, .. } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                format!("{prop}.count * {bytes}")
            }
            _ => format!("{prop}.tailLen()"),
        };
        self.line(2, &format!("return {len_expr}"));
        self.line(1, "}");
        self.blank();

        self.note(1, "The variable-length tail, which follows the fixed prefix.");
        self.line(1, "func packTail(_ big: Bool) throws -> [UInt8] {");
        match ty {
            WireType::Str { max } => {
                self.line(2, &format!("return try defgenEncodeUtf8({prop}, {max}, \"{label}\")"));
            }
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(2, "var out: [UInt8] = []");
                self.line(2, &format!("for elemVal in try defgenCheckMax({prop}, {max}, \"{label}\") {{"));
                self.line(3, &format!("let bits = DefgenBits(size: {bytes}, big: big)"));
                let elem_ty = (**elem).clone();
                self.pack(3, "elemVal", &elem_ty, "0", label);
                self.line(3, "out += bits.bytes");
                self.line(2, "}");
                self.line(2, "return out");
            }
            _ => self.line(2, &format!("return try {prop}.packTail(big)")),
        }
        self.line(1, "}");
        self.blank();

        self.note(1, "Reads the tail; its length is what the transport delivered (§6.3).");
        self.line(1, "mutating func unpackTail(_ data: [UInt8], _ big: Bool) throws {");
        match ty {
            WireType::Str { max } => {
                self.line(2, &format!("guard data.count <= {max} else {{"));
                self.raise(
                    3,
                    "length",
                    &format!("\"{label}: \\(data.count) bytes exceeds the maximum of {max}\""),
                );
                self.line(2, "}");
                self.line(2, &format!("{prop} = try defgenDecodeUtf8(data, \"{label}\")"));
            }
            WireType::VarArray { elem, max } => {
                let bytes = u64::from(self.m.layout_of(elem).fixed_bits) / 8;
                self.line(2, &format!("guard data.count % {bytes} == 0 else {{"));
                self.raise(
                    3,
                    "length",
                    &format!(
                        "\"{label}: \\(data.count) bytes is not a whole number of {bytes}-byte elements\""
                    ),
                );
                self.line(2, "}");
                self.line(2, &format!("let count = data.count / {bytes}"));
                self.line(2, &format!("guard count <= {max} else {{"));
                self.raise(
                    3,
                    "length",
                    &format!("\"{label}: \\(count) elements exceeds the maximum of {max}\""),
                );
                self.line(2, "}");
                self.line(2, &format!("{prop} = try (0..<count).map {{ i in"));
                self.line(
                    3,
                    &format!(
                        "let bits = DefgenBits(data: Array(data[(i * {bytes})..<((i + 1) * {bytes})]), big: big)"
                    ),
                );
                let elem_expr = self.unpack_expr(elem, "0");
                self.line(3, &format!("return {elem_expr}"));
                self.line(2, "}");
            }
            _ => self.line(2, &format!("try {prop}.unpackTail(data, big)")),
        }
        self.line(1, "}");
        self.blank();
    }

    // ---------------------------------------------------------------------
    // GATT metadata (§10)
    // ---------------------------------------------------------------------

    /// UUIDs and property sets as data. What a program does with them —
    /// which BLE library it hands them to — is deliberately out of scope
    /// (§10).
    fn gatt(&mut self) {
        let m = self.m;
        if m.services.is_empty() {
            return;
        }
        self.banner("GATT bindings");

        self.note(0, "GATT characteristic properties, as a flag set (§10).");
        self.line(0, "enum GattProperty: Hashable {");
        for p in Property::ALL {
            self.line(1, &format!("case {}", field_ident(p.as_str())));
        }
        self.line(0, "}");
        self.blank();

        self.note(0, "One `characteristic` binding: a UUID, and what may be done with it (§10).");
        self.lines(
            0,
            &[
                "struct GattCharacteristic: Equatable {",
                "    let name: String",
                "    let uuid: String",
                "    let properties: Set<GattProperty>",
                "}",
            ],
        );
        self.blank();
        self.note(0, "One `service` declaration, and the characteristics under it (§10).");
        self.lines(
            0,
            &[
                "struct GattService: Equatable {",
                "    let name: String",
                "    let uuid: String",
                "    let characteristics: [GattCharacteristic]",
                "}",
            ],
        );
        self.blank();

        let mut service_names: Vec<String> = Vec::new();
        for service in &m.services {
            let sprefix = screaming(&service.name);
            service_names.push(sprefix.clone());
            self.docs_with(0, &service.docs, &[]);
            self.line(0, &format!("let {sprefix}_UUID: String = \"{}\"", service.uuid));
            for c in &service.characteristics {
                let ty_name = ident(&m.get(c.ty).name);
                let size = match c.layout.tail {
                    None => format!("{} bytes", c.layout.fixed_bytes()),
                    Some(_) => format!("{}..{} bytes", c.layout.fixed_bytes(), c.layout.max_bytes()),
                };
                let notes = vec![format!("Carries a `{ty_name}` ({size}).")];
                self.docs_with(0, &c.docs, &notes);
                self.line(0, &format!("let {sprefix}_{}_UUID: String = \"{}\"", screaming(&c.name), c.uuid));
            }
            self.blank();

            self.line(0, &format!("let {sprefix} = GattService("));
            self.line(1, &format!("name: \"{}\",", service.name));
            self.line(1, &format!("uuid: {sprefix}_UUID,"));
            self.line(1, "characteristics: [");
            for c in &service.characteristics {
                let props: Vec<String> =
                    c.properties.iter().map(|p| format!(".{}", field_ident(p.as_str()))).collect();
                self.line(2, "GattCharacteristic(");
                self.line(3, &format!("name: \"{}\",", c.name));
                self.line(3, &format!("uuid: {sprefix}_{}_UUID,", screaming(&c.name)));
                self.line(3, &format!("properties: [{}]", props.join(", ")));
                self.line(2, "),");
            }
            self.line(1, "]");
            self.line(0, ")");
            self.blank();
        }

        self.note(0, "Every service this schema declares, in source order.");
        self.line(0, &format!("let SERVICES: [GattService] = [{}]", service_names.join(", ")));
    }
}

/// `true`/`false`, spelled the way Swift does — for embedding a root's fixed
/// byte order as a literal rather than threading it as a parameter, wherever
/// the call site is a top-level function rather than an instance method.
fn swift_bool(v: bool) -> &'static str {
    if v { "true" } else { "false" }
}
