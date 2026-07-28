//! The C backend: one self-contained C99 header per schema.
//!
//! # Shape of the output
//!
//! Everything lands in a single `#include`-able header — types, constants and
//! `static inline` codec functions — so a project consumes generated code by
//! copying one file in. There is no companion `.c`, no build-system step and no
//! runtime library, and nothing to link against: the only dependencies are
//! `<stdint.h>`, `<stdbool.h>`, `<stddef.h>` and `<string.h>`. A `scaled` type
//! (SPEC.md §4) needs rounding, but it gets it from a generated helper rather
//! than from `<math.h>`, so generated code never drags in libm — which on a
//! bare-metal BLE target is a dependency worth not having.
//!
//! # Naming
//!
//! The schema owns the header's namespace, so declared names carry over
//! directly and are only re-cased to C convention (§12):
//!
//! | Schema | C |
//! |---|---|
//! | `struct Status` | `typedef struct { … } Status;` |
//! | its codec | `status_encode` / `status_decode` |
//! | its size | `STATUS_SIZE` |
//! | `enum HearingMode`'s `Stereo` | `HEARING_MODE_STEREO` |
//! | field `active_profile` | `active_profile` |
//!
//! Functions with a `__` in the middle (`status__pack_fixed`) are internal:
//! they exist because a nested type is packed by its parent, not because a
//! caller should reach for them.
//!
//! # Representation choices
//!
//! * A `uN`/`iN` value is carried in the smallest stdint type that holds `N`
//!   bits (§2), so `u12` is a `uint16_t`. `u65`..`u128` need `unsigned
//!   __int128`; the header `#error`s on a compiler without it, and only when
//!   the schema actually uses such a width.
//! * A plain `enum` becomes a `typedef` of its backing integer plus `#define`d
//!   constants, not a C `enum`: a C `enum`'s underlying type is
//!   implementation-defined, which is exactly the kind of per-toolchain drift
//!   a wire format cannot afford. An *open* enum (§5) needs no separate
//!   `Unknown` case, because an unmatched wire value is already distinct from
//!   every declared one and keeps its raw value; `<enum>_is_known` tests for
//!   it. A *closed* enum's decode rejects an unmatched value.
//! * A tagged union (§7) becomes a struct holding the discriminant plus a C
//!   `union` of per-variant payload structs. An open union's fallback carries
//!   its undecoded `raw` payload in a `payload.<else>` member, with the
//!   unrecognized id left in the discriminant field.
//! * C has no growable string or array, so a variable-length field (§6.3) is a
//!   fixed-capacity buffer of `max` elements plus a `<field>_len` count, as
//!   §12 requires. Nothing here allocates. An `alias` of a variable-length
//!   type gets that pair wrapped in a value of its own (`{ data, len }`), since
//!   §6.3 lets such an alias be bound straight to a characteristic and it then
//!   needs to be one addressable thing.
//! * A `scaled` type (§4) is a `typedef` of its physical `float`/`double`, with
//!   `<name>_from_raw` / `<name>_to_raw` exposing the underlying integer for
//!   callers that want to round-trip without floating-point rounding.
//!
//! # Bit and byte order
//!
//! Fields occupy the container in declaration order, packed with no gaps, and
//! byte order (§8) decides which end of the container they fill from — LSB-first
//! for a little-endian container, MSB-first for a big-endian one (§6). Both live
//! in one place: `defgen__start` mirrors a value's offset for a big-endian
//! container and `defgen__byte` maps the resulting bit to a physical byte. Every
//! read and write goes through them, so a container's byte order is a single
//! `int big` argument threaded down from the root entry point rather than
//! something each generated function decides for itself.
//!
//! For a variable-length root (§6.3) that mapping covers the *fixed prefix*; the
//! trailing elements follow it in order, each packed as its own byte-multiple
//! unit under the same byte order.

use super::{Backend, Generated, GeneratedFile, Options, sanitize_stem, screaming, snake};
use crate::ast::{Docs, Endianness, FloatType, Property};
use crate::model::{
    Const, ElseVariant, Enum, Field, FieldRole, Model, Scaled, Struct, TypeDef, TypeId, TypeKind, Union,
    WireType, carrier_bits, int_range,
};

pub struct CBackend;

impl Backend for CBackend {
    fn name(&self) -> &'static str {
        "c"
    }

    fn description(&self) -> &'static str {
        "a single self-contained C99 header"
    }

    fn generate(&self, model: &Model, opts: &Options) -> Generated {
        let stem = sanitize_stem(&opts.stem);
        let file = GeneratedFile {
            name: format!("{stem}.h"),
            contents: Emitter::new(model, &stem, opts.source.as_deref()).run(),
        };
        Generated { files: vec![file] }
    }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------
//
// [`snake`] (`hearing_mode`, the prefix every generated function for a type is
// built from) and [`screaming`] (`HEARING_MODE`, the prefix for `#define`d
// constants) are shared with the other backends; see [`super`].

/// Every C99 keyword, plus the C11/C23 additions and `bool`/`true`/`false` from
/// `<stdbool.h>`. A schema field named `default` is perfectly legal — §1 does
/// not reserve C's vocabulary — so a colliding name gets a trailing `_`.
#[rustfmt::skip]
const C_KEYWORDS: &[&str] = &[
    "alignas", "alignof", "auto", "bool", "break", "case", "char", "const", "constexpr",
    "continue", "default", "do", "double", "else", "enum", "extern", "false", "float", "for",
    "goto", "if", "inline", "int", "long", "nullptr", "register", "restrict", "return", "short",
    "signed", "sizeof", "static", "static_assert", "struct", "switch", "thread_local", "true",
    "typedef", "typeof", "union", "unsigned", "void", "volatile", "while",
];

/// A schema name as a C identifier, escaped if it collides with a keyword.
fn ident(name: &str) -> String {
    if C_KEYWORDS.contains(&name) { format!("{name}_") } else { name.to_string() }
}

/// A GATT UUID (validated by `check::is_uuid` into one of the three hex
/// forms, §10) as a C brace initializer, in wire order — the little-endian
/// byte order BLE stacks (Zephyr, the nRF SDKs, BlueZ) actually transmit and
/// expect, which is the reverse of the order the UUID is written in.
fn uuid_bytes(uuid: &str) -> String {
    let hex: String = uuid.chars().filter(|c| *c != '-').collect();
    let mut bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    bytes.reverse();
    let body: Vec<String> = bytes.iter().map(|b| format!("0x{b:02x}")).collect();
    format!("{{ {} }}", body.join(", "))
}

/// Whether `code` uses `name` as an identifier rather than merely containing
/// its letters — so `sizeof(*v)` does not count as a use of `size`.
fn mentions(code: &str, name: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    code.match_indices(name).any(|(i, _)| {
        let before = code[..i].chars().next_back().is_none_or(|c| !is_word(c));
        let after = code[i + name.len()..].chars().next().is_none_or(|c| !is_word(c));
        before && after
    })
}

// ---------------------------------------------------------------------------
// Variable-length tails
// ---------------------------------------------------------------------------

/// Where a value's variable-length tail (§6.3) actually lives.
///
/// `member` and `len_member` are C expressions relative to the `v` a generated
/// function receives — `v->label` / `v->label_len` for a struct field, or
/// `v->data` / `v->len` for an alias bound straight to a characteristic.
enum Tail {
    /// `string(max: N)` written inline as a field.
    Str { member: String, len_member: String, max: u64 },
    /// `Type[max: N]` written inline as a field.
    Arr { member: String, len_member: String, elem: WireType, max: u64, elem_bytes: u64 },
    /// A named type that owns a tail of its own — a variable-length struct, or
    /// an alias of a variable-length type. It has its own tail functions, so
    /// this case is pure delegation.
    Nested { id: TypeId, member: String },
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// Runtime helpers the schema turned out to need. Each costs a dependency or a
/// portability caveat, so none of them is emitted unconditionally.
#[derive(Default)]
struct Needs {
    /// A value wider than 64 bits, which forces `unsigned __int128`.
    wide: bool,
    /// A `string`, which needs the UTF-8 validator (§6.3).
    utf8: bool,
    /// A `scaled` type, which needs the rounding helper (§4).
    round: bool,
    /// A `padding: uN = 0` field, which needs the all-zero check (§6.2).
    zero_check: bool,
}

/// Where a generated statement reads and writes bits: the buffer expression,
/// its byte length (which fixes the big-endian byte mapping) and the byte-order
/// flag. Threading these as expressions rather than fixed names is what lets
/// one emitter serve a root container, a nested struct and a lone tail element
/// alike.
struct Ctx {
    buf: String,
    size: String,
    big: String,
}

impl Ctx {
    /// The parameters every `__pack_fixed`/`__unpack_fixed` receives.
    fn params() -> Ctx {
        Ctx { buf: "buf".into(), size: "size".into(), big: "big".into() }
    }

    /// One element of a variable-length tail, packed as its own byte-multiple
    /// unit at `out + i * bytes`.
    fn tail_elem(base: &str, index: &str, bytes: u64) -> Ctx {
        Ctx { buf: format!("({base} + {index} * {bytes}u)"), size: format!("{bytes}u"), big: "big".into() }
    }
}

struct Emitter<'m> {
    m: &'m Model,
    out: String,
    guard: String,
    source: Option<&'m str>,
    needs: Needs,
    /// Nesting depth of emitted `for` loops, so array loop variables are unique.
    depth: usize,
    /// Counter for temporaries, so nested blocks never shadow one another.
    tmp: usize,
}

impl<'m> Emitter<'m> {
    fn new(m: &'m Model, stem: &str, source: Option<&'m str>) -> Emitter<'m> {
        let mut e = Emitter {
            m,
            out: String::with_capacity(32 * 1024),
            guard: format!("{}_H", stem.to_ascii_uppercase()),
            source,
            needs: Needs::default(),
            depth: 0,
            tmp: 0,
        };
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
            self.line(ind, text);
        }
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }

    /// Runs `f` and returns what it emitted instead of appending it, so a
    /// function's body can be inspected — is it empty? — before its signature
    /// is written.
    fn capture(&mut self, f: impl FnOnce(&mut Self)) -> String {
        let saved = std::mem::take(&mut self.out);
        f(self);
        std::mem::replace(&mut self.out, saved)
    }

    fn banner(&mut self, title: &str) {
        let rule = "-".repeat(68usize.saturating_sub(title.len()));
        self.blank();
        self.line(0, &format!("/* {title} {rule} */"));
        self.blank();
    }

    /// Doc comments as Doxygen (§1, §12).
    fn docs(&mut self, ind: usize, docs: &Docs) {
        if docs.is_empty() {
            return;
        }
        self.line(ind, "/**");
        for doc in docs {
            // A `*/` inside a doc comment would close the block early.
            let text = doc.text.replace("*/", "*\\/");
            if text.is_empty() {
                self.line(ind, " *");
            } else {
                self.line(ind, &format!(" * {text}"));
            }
        }
        self.line(ind, " */");
    }

    /// A Doxygen comment the backend itself wrote, wrapped to stay readable —
    /// several of these explain a representation choice and run well past one
    /// line.
    fn note(&mut self, ind: usize, text: &str) {
        const WIDTH: usize = 78;
        let budget = WIDTH.saturating_sub(ind * 4);
        if text.len() + 7 <= budget {
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

    fn tmp(&mut self, base: &str) -> String {
        self.tmp += 1;
        format!("{base}{}", self.tmp)
    }

    // -- pre-pass -----------------------------------------------------------

    /// Works out which runtime helpers the schema needs, before emitting the
    /// prelude that has to contain them.
    fn scan(&mut self) {
        let m = self.m;
        for def in &m.types {
            match &def.kind {
                TypeKind::Alias(a) => self.scan_type(&a.target),
                TypeKind::Scaled(s) => {
                    self.needs.round = true;
                    self.needs.wide |= carrier_bits(s.raw_bits) > 64;
                }
                TypeKind::Enum(e) => {
                    self.needs.wide |= carrier_bits(e.backing_bits) > 64;
                    self.scan_else(e.else_arm.as_ref());
                }
                TypeKind::Union(u) => {
                    self.needs.wide |= carrier_bits(u.tag_bits) > 64;
                    self.scan_else(u.else_arm.as_ref());
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
        for c in &m.consts {
            self.needs.wide |= carrier_bits(c.bits) > 64;
        }
    }

    fn scan_else(&mut self, arm: Option<&ElseVariant>) {
        if let Some(arm) = arm {
            self.needs.wide |= carrier_bits(arm.raw_bits) > 64;
            self.needs.wide |= arm.id_bits.is_some_and(|b| carrier_bits(b) > 64);
        }
    }

    fn scan_field(&mut self, f: &Field) {
        // A `padding` run is a gap, never decoded into a value (§2, §6.2), so
        // it does not drag in the 128-bit helpers however wide it is.
        if let FieldRole::Padding { check_zero } = f.role {
            self.needs.zero_check |= check_zero;
            return;
        }
        self.scan_type(&f.ty);
    }

    fn scan_type(&mut self, ty: &WireType) {
        match ty {
            WireType::UInt(n) | WireType::Int(n) => self.needs.wide |= carrier_bits(*n) > 64,
            WireType::Bool | WireType::Named(_) => {}
            WireType::Str { .. } => self.needs.utf8 = true,
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => self.scan_type(elem),
        }
    }

    // -- type mapping -------------------------------------------------------

    /// The stdint type a `uN`/`iN` value is carried in (§2).
    fn int_type(bits: u32, signed: bool) -> String {
        match (signed, carrier_bits(bits)) {
            (false, 128) => "defgen_u128".to_string(),
            (true, 128) => "defgen_i128".to_string(),
            (false, c) => format!("uint{c}_t"),
            (true, c) => format!("int{c}_t"),
        }
    }

    /// The C type a value of `ty` is held in. Arrays and inline
    /// variable-length types are declared through [`Emitter::member_lines`]
    /// instead, since their C spelling wraps the member name.
    fn c_type(&self, ty: &WireType) -> String {
        match ty {
            WireType::UInt(n) => Self::int_type(*n, false),
            WireType::Int(n) => Self::int_type(*n, true),
            WireType::Bool => "bool".to_string(),
            WireType::Named(id) => ident(&self.m.get(*id).name),
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => self.c_type(elem),
            WireType::Str { .. } => "char".to_string(),
        }
    }

    /// The declaration lines for one struct member. An inline variable-length
    /// field becomes a fixed-capacity buffer plus an explicit length (§12).
    fn member_lines(&self, ty: &WireType, name: &str) -> Vec<String> {
        match ty {
            WireType::Array { elem, count } => {
                vec![format!("{} {name}[{count}];", self.c_type(elem))]
            }
            WireType::Str { max } => {
                vec![format!("char {name}[{max}];"), format!("size_t {name}_len;")]
            }
            WireType::VarArray { elem, max } => {
                vec![format!("{} {name}[{max}];", self.c_type(elem)), format!("size_t {name}_len;")]
            }
            _ => vec![format!("{} {name};", self.c_type(ty))],
        }
    }

    // -- literals -----------------------------------------------------------

    /// An unsigned constant of any declared width, as a C expression. A value
    /// above 64 bits has no single-token spelling, so it is assembled from two
    /// halves.
    ///
    /// `hex` is for tagged-union ids, which §7 has authors write in hex because
    /// they are wire contracts; matching that makes the header diffable against
    /// the schema by eye.
    fn uint_lit(value: u128, carrier: u32, hex: bool) -> String {
        let write = |v: u64| if hex { format!("0x{v:x}") } else { v.to_string() };
        if carrier <= 64 {
            format!("UINT64_C({})", write(value as u64))
        } else if value <= u128::from(u64::MAX) {
            format!("((defgen_u128)UINT64_C({}))", write(value as u64))
        } else {
            let (hi, lo) = ((value >> 64) as u64, value as u64);
            format!("((((defgen_u128)UINT64_C({})) << 64) | UINT64_C({}))", write(hi), write(lo))
        }
    }

    /// A signed constant of any declared width, as a C expression (§3.1).
    /// Above 64 bits, `__int128` has no literal of its own either, so the
    /// value is round-tripped through the same unsigned assembly `uint_lit`
    /// uses: the bit pattern is identical either way, two's complement (§2).
    fn int_lit(value: i128, carrier: u32) -> String {
        if carrier <= 64 {
            format!("INT64_C({value})")
        } else {
            format!("((defgen_i128){})", Self::uint_lit(value as u128, carrier, false))
        }
    }

    /// `2^bits - 1`, the encode-side upper bound for a `uN` value (§2).
    fn umax_expr(bits: u32) -> String {
        if bits < 64 {
            format!("UINT64_C({})", (1u128 << bits) - 1)
        } else {
            format!("((((defgen_u128)1) << {bits}) - 1)")
        }
    }

    /// The inclusive `iN` bounds, as C expressions (§2).
    fn int_bounds(bits: u32) -> (String, String) {
        if bits < 64 {
            let (min, max) = int_range(bits, true);
            (format!("(-INT64_C({}))", -min), format!("INT64_C({max})"))
        } else {
            (
                format!("(-(((defgen_i128)1) << {}))", bits - 1),
                format!("((((defgen_i128)1) << {}) - 1)", bits - 1),
            )
        }
    }

    /// A `double` literal that round-trips. Rust's shortest representation is
    /// already exact; it just may lack the `.` that makes C read it as one.
    fn float_lit(v: f64) -> String {
        let s = format!("{v:?}");
        if s.contains(['.', 'e', 'E', 'n', 'i']) { s } else { format!("{s}.0") }
    }

    /// A power of two as an exact hex-float literal — the bound a `scaled`
    /// value's rounded result is checked against (§4). Written this way because
    /// whether a decimal literal for, say, `2^63` is exactly representable as a
    /// `double` is not obvious by inspection, and a bound that is off by one
    /// ulp silently accepts an out-of-range value.
    fn pow2_lit(exp: u32) -> String {
        format!("0x1p{exp}")
    }

    fn offset(base: &str, delta: u32) -> String {
        if delta == 0 { base.to_string() } else { format!("{base} + {delta}u") }
    }

    // ---------------------------------------------------------------------
    // Top level
    // ---------------------------------------------------------------------

    fn run(mut self) -> String {
        self.file_header();
        self.prelude();
        self.declarations();
        self.implementations();
        self.gatt();
        self.blank();
        let guard = self.guard.clone();
        self.lines(0, &["#ifdef __cplusplus", "} /* extern \"C\" */", "#endif"]);
        self.blank();
        self.line(0, &format!("#endif /* {guard} */"));
        self.out
    }

    fn file_header(&mut self) {
        let from = match self.source {
            Some(path) => format!(" from `{path}`"),
            None => String::new(),
        };
        let guard = self.guard.clone();
        self.line(0, "/*");
        self.line(0, &format!(" * Generated by defgen{from}. Do not edit."));
        self.lines(
            0,
            &[
                " *",
                " * Codecs for this schema's GATT values: fields in declaration order (§6),",
                " * byte order applied per root container (§8). Every function is `static",
                " * inline`, so this",
                " * header is the whole dependency — there is no companion .c file.",
                " *",
                " * Sizes are byte counts. Every codec returns `defgen_err_t`; on anything",
                " * but DEFGEN_OK the output value is unspecified.",
                " */",
            ],
        );
        self.line(0, &format!("#ifndef {guard}"));
        self.line(0, &format!("#define {guard}"));
        self.blank();
        self.lines(
            0,
            &["#include <stdbool.h>", "#include <stddef.h>", "#include <stdint.h>", "#include <string.h>"],
        );
        self.blank();
        self.lines(0, &["#ifdef __cplusplus", "extern \"C\" {", "#endif"]);
    }

    fn prelude(&mut self) {
        self.banner("Runtime");

        if self.needs.wide {
            self.lines(
                0,
                &[
                    "/* This schema declares a value wider than 64 bits, so generated code needs",
                    "   a 128-bit integer type. ISO C has none, so `__extension__` keeps the",
                    "   compiler's own extension from tripping -pedantic in the including",
                    "   project's build. */",
                    "#if defined(__SIZEOF_INT128__)",
                    "__extension__ typedef unsigned __int128 defgen_u128;",
                    "__extension__ typedef __int128 defgen_i128;",
                    "#else",
                    "#error \"this schema needs 128-bit integers; no __int128 on this compiler\"",
                    "#endif",
                ],
            );
            self.blank();
        }

        self.note(0, "Outcome of an encode or decode. `DEFGEN_OK` is zero.");
        self.line(0, "typedef enum {");
        self.line(1, "DEFGEN_OK = 0,");
        self.note(1, "The caller's buffer is smaller than the encoded value.");
        self.line(1, "DEFGEN_ERR_BUFFER_TOO_SMALL,");
        self.note(1, "The received length matches no legal encoding of this type.");
        self.line(1, "DEFGEN_ERR_LENGTH,");
        self.note(1, "A value does not fit the bits its field declares (§2, §4, §6.3).");
        self.line(1, "DEFGEN_ERR_RANGE,");
        self.note(1, "A closed enum or tagged union met an undeclared value (§5, §7).");
        self.line(1, "DEFGEN_ERR_UNKNOWN_VALUE,");
        self.note(1, "A `padding: uN = 0` run was not zero on the wire (§6.2).");
        self.line(1, "DEFGEN_ERR_PADDING,");
        self.note(1, "A `string` field's bytes are not well-formed UTF-8 (§6.3).");
        self.line(1, "DEFGEN_ERR_UTF8");
        self.line(0, "} defgen_err_t;");
        self.blank();

        self.note(0, "A short, stable description of an error code.");
        self.line(0, "static inline const char *defgen_err_str(defgen_err_t err) {");
        self.line(1, "switch (err) {");
        for (code, text) in [
            ("DEFGEN_OK", "ok"),
            ("DEFGEN_ERR_BUFFER_TOO_SMALL", "buffer too small"),
            ("DEFGEN_ERR_LENGTH", "wrong length"),
            ("DEFGEN_ERR_RANGE", "value out of range"),
            ("DEFGEN_ERR_UNKNOWN_VALUE", "unknown value"),
            ("DEFGEN_ERR_PADDING", "non-zero padding"),
            ("DEFGEN_ERR_UTF8", "invalid UTF-8"),
        ] {
            self.line(2, &format!("case {code}: return \"{text}\";"));
        }
        self.line(2, "default: return \"unknown error\";");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();

        self.lines(
            0,
            &[
                "/* Where an `n`-bit value declared at offset `off` sits in the container's",
                "   bits, and which byte of the buffer holds each of them. Byte order (§8) is",
                "   applied in these two functions and nowhere else.",
                "",
                "   Fields always occupy the container in declaration order, first field",
                "   first; byte order chooses which end of the container they fill from.",
                "   Little-endian fills from the least-significant end, so a field's own bit i",
                "   lands at container bit `off + i`; big-endian fills from the",
                "   most-significant end, which is the mirror image — and mirroring the",
                "   offsets is exactly what puts the first field in the first byte once the",
                "   container is written out most-significant byte first. */",
                "static inline uint32_t defgen__start(size_t size, int big, uint32_t off, uint32_t n) {",
                "    return big ? ((uint32_t)size * 8u - off - n) : off;",
                "}",
                "",
                "static inline size_t defgen__byte(size_t size, int big, uint32_t bit) {",
                "    size_t i = (size_t)(bit >> 3);",
                "    return big ? (size - (size_t)1 - i) : i;",
                "}",
                "",
                "/* Reads `n` bits (n <= 64) of the value declared at `off`, LSB-first. */",
                "static inline uint64_t defgen__get(const uint8_t *buf, size_t size, int big,",
                "                                   uint32_t off, uint32_t n) {",
                "    uint64_t v = 0;",
                "    uint32_t start = defgen__start(size, big, off, n);",
                "    uint32_t i;",
                "    for (i = 0; i < n; i++) {",
                "        uint32_t bit = start + i;",
                "        size_t b = defgen__byte(size, big, bit);",
                "        v |= (uint64_t)((buf[b] >> (bit & 7u)) & 1u) << i;",
                "    }",
                "    return v;",
                "}",
                "",
                "/* Writes the low `n` bits (n <= 64) of `val` into the value declared at `off`,",
                "   LSB-first. Clears as well as sets, so it never assumes a zeroed buffer. */",
                "static inline void defgen__put(uint8_t *buf, size_t size, int big,",
                "                               uint32_t off, uint32_t n, uint64_t val) {",
                "    uint32_t start = defgen__start(size, big, off, n);",
                "    uint32_t i;",
                "    for (i = 0; i < n; i++) {",
                "        uint32_t bit = start + i;",
                "        size_t b = defgen__byte(size, big, bit);",
                "        uint8_t mask = (uint8_t)(1u << (bit & 7u));",
                "        if ((val >> i) & 1u) buf[b] = (uint8_t)(buf[b] | mask);",
                "        else buf[b] = (uint8_t)(buf[b] & (uint8_t)~mask);",
                "    }",
                "}",
                "",
                "/* Sign-extends an `iN` value from bit N-1 (§2). */",
                "static inline int64_t defgen__sext(uint64_t v, uint32_t n) {",
                "    uint64_t m;",
                "    if (n >= 64u) return (int64_t)v;",
                "    m = (uint64_t)1 << (n - 1u);",
                "    return (int64_t)((v ^ m) - m);",
                "}",
            ],
        );

        if self.needs.wide {
            self.blank();
            self.lines(
                0,
                &[
                    "/* The same three, for the 65..128-bit values this schema declares. */",
                    "static inline defgen_u128 defgen__get_wide(const uint8_t *buf, size_t size,",
                    "                                           int big, uint32_t off, uint32_t n) {",
                    "    defgen_u128 v = 0;",
                    "    uint32_t start = defgen__start(size, big, off, n);",
                    "    uint32_t i;",
                    "    for (i = 0; i < n; i++) {",
                    "        uint32_t bit = start + i;",
                    "        size_t b = defgen__byte(size, big, bit);",
                    "        v |= (defgen_u128)((buf[b] >> (bit & 7u)) & 1u) << i;",
                    "    }",
                    "    return v;",
                    "}",
                    "",
                    "static inline void defgen__put_wide(uint8_t *buf, size_t size, int big,",
                    "                                    uint32_t off, uint32_t n, defgen_u128 val) {",
                    "    uint32_t start = defgen__start(size, big, off, n);",
                    "    uint32_t i;",
                    "    for (i = 0; i < n; i++) {",
                    "        uint32_t bit = start + i;",
                    "        size_t b = defgen__byte(size, big, bit);",
                    "        uint8_t mask = (uint8_t)(1u << (bit & 7u));",
                    "        if ((uint32_t)((val >> i) & 1u)) buf[b] = (uint8_t)(buf[b] | mask);",
                    "        else buf[b] = (uint8_t)(buf[b] & (uint8_t)~mask);",
                    "    }",
                    "}",
                    "",
                    "static inline defgen_i128 defgen__sext_wide(defgen_u128 v, uint32_t n) {",
                    "    defgen_u128 m;",
                    "    if (n >= 128u) return (defgen_i128)v;",
                    "    m = (defgen_u128)1 << (n - 1u);",
                    "    return (defgen_i128)((v ^ m) - m);",
                    "}",
                ],
            );
        }

        if self.needs.round {
            self.blank();
            self.lines(
                0,
                &[
                    "/* Rounds half away from zero: exactly what C's `round()` does, without",
                    "   pulling in <math.h> and libm for the one operation a `scaled` type needs",
                    "   (§4). Every backend has to agree on a scaled value's raw integer down to",
                    "   the last unit (§13), so this is `round()`'s behaviour and not merely a",
                    "   near-miss.",
                    "",
                    "   The obvious `(int64_t)(v + 0.5)` is *not* that: adding 0.5 to the double",
                    "   just below 0.5 rounds up to 1.0, so it rounds 0.49999999999999994 to 1,",
                    "   and it is undefined behaviour once v exceeds the integer type. Taking the",
                    "   integer part first and comparing the remainder avoids both. */",
                    "static inline double defgen__round(double v) {",
                    "    double a = v < 0 ? -v : v;",
                    "    double w;",
                    "    /* At 2^52 and above every double is already an integer, so there is",
                    "       nothing to do — and this is where NaN and the infinities land too,",
                    "       since the comparison against them is false. */",
                    "    if (!(a < 4503599627370496.0)) return v;",
                    "    w = (double)(uint64_t)a; /* a < 2^52, so the conversion is in range */",
                    "    if (a - w >= 0.5) w += 1.0; /* exact: w is a's integer part */",
                    "    return v < 0 ? -w : w;",
                    "}",
                ],
            );
        }

        if self.needs.zero_check {
            self.blank();
            self.lines(
                0,
                &[
                    "/* Whether a `padding: uN = 0` run is all zero (§6.2). Bit-at-a-time, so a",
                    "   run of any width, byte-aligned or not, is one call. */",
                    "static inline int defgen__bits_zero(const uint8_t *buf, size_t size, int big,",
                    "                                    uint32_t off, uint32_t n) {",
                    "    uint32_t start = defgen__start(size, big, off, n);",
                    "    uint32_t i;",
                    "    for (i = 0; i < n; i++) {",
                    "        uint32_t bit = start + i;",
                    "        size_t b = defgen__byte(size, big, bit);",
                    "        if ((buf[b] >> (bit & 7u)) & 1u) return 0;",
                    "    }",
                    "    return 1;",
                    "}",
                ],
            );
        }

        if self.needs.utf8 {
            self.blank();
            self.lines(
                0,
                &[
                    "/* Strict UTF-8 validation for `string` decode (§6.3): rejects overlong",
                    "   encodings, surrogates and anything above U+10FFFF. The spec requires",
                    "   failing rather than substituting replacement characters. */",
                    "static inline int defgen__utf8_valid(const uint8_t *s, size_t n) {",
                    "    size_t i = 0;",
                    "    while (i < n) {",
                    "        uint8_t c = s[i];",
                    "        size_t need, k;",
                    "        uint32_t cp;",
                    "        if (c < 0x80u) { i += 1; continue; }",
                    "        if ((c & 0xE0u) == 0xC0u) { need = 1; cp = (uint32_t)(c & 0x1Fu); }",
                    "        else if ((c & 0xF0u) == 0xE0u) { need = 2; cp = (uint32_t)(c & 0x0Fu); }",
                    "        else if ((c & 0xF8u) == 0xF0u) { need = 3; cp = (uint32_t)(c & 0x07u); }",
                    "        else return 0;",
                    "        if (n - i - 1 < need) return 0;",
                    "        for (k = 1; k <= need; k++) {",
                    "            uint8_t cc = s[i + k];",
                    "            if ((cc & 0xC0u) != 0x80u) return 0;",
                    "            cp = (cp << 6) | (uint32_t)(cc & 0x3Fu);",
                    "        }",
                    "        if (need == 1 && cp < 0x80u) return 0;",
                    "        if (need == 2 && cp < 0x800u) return 0;",
                    "        if (need == 3 && cp < 0x10000u) return 0;",
                    "        if (cp > 0x10FFFFu) return 0;",
                    "        if (cp >= 0xD800u && cp <= 0xDFFFu) return 0;",
                    "        i += need + 1;",
                    "    }",
                    "    return 1;",
                    "}",
                ],
            );
        }
    }

    // ---------------------------------------------------------------------
    // Declarations
    // ---------------------------------------------------------------------

    /// Types and constants, in source order. §9 forbids forward references, so
    /// source order is already a valid definition order for C.
    fn declarations(&mut self) {
        self.banner("Types");
        let m = self.m;
        for def in &m.types {
            self.declare(def);
            self.blank();
        }
        if !m.consts.is_empty() {
            self.banner("Named constants");
            for c in &m.consts {
                self.declare_const(c);
                self.blank();
            }
        }
    }

    /// `const Name: uN|iN = <literal>;` (§3.1) — a `#define`, the same idiom
    /// an enum variant's value already gets.
    fn declare_const(&mut self, c: &Const) {
        self.docs(0, &c.docs);
        let ty = Self::int_type(c.bits, c.signed);
        let carrier = carrier_bits(c.bits);
        let value = if c.signed {
            Self::int_lit(c.as_i128(), carrier)
        } else {
            Self::uint_lit(c.magnitude, carrier, false)
        };
        self.line(0, &format!("#define {} (({ty}){value})", screaming(&c.name)));
    }

    fn declare(&mut self, def: &'m TypeDef) {
        let name = ident(&def.name);
        self.docs(0, &def.docs);
        match &def.kind {
            TypeKind::Alias(a) => self.declare_alias(&name, &a.target),
            TypeKind::Scaled(s) => self.declare_scaled(&name, s),
            TypeKind::Enum(e) => self.declare_enum(def, &name, e),
            TypeKind::Union(u) => self.declare_union(def, &name, u),
            TypeKind::Struct(s) => self.declare_struct(&name, s),
        }
        self.size_constants(def, &name);
    }

    fn declare_alias(&mut self, name: &str, target: &WireType) {
        match target {
            // A variable-length alias can be bound straight to a characteristic
            // (§6.3), so it needs a value of its own to hold the buffer and its
            // length — the pair an inline variable-length field expands to,
            // wrapped so it is one addressable thing.
            WireType::Str { max } => {
                self.note(0, "Variable-length (§6.3): the first `len` bytes of `data` are live.");
                self.line(0, "typedef struct {");
                self.line(1, &format!("char data[{max}];"));
                self.line(1, "size_t len;");
                self.line(0, &format!("}} {name};"));
            }
            WireType::VarArray { elem, max } => {
                let elem_ty = self.c_type(elem);
                self.note(0, "Variable-length (§6.3): the first `len` elements of `data` are live.");
                self.line(0, "typedef struct {");
                self.line(1, &format!("{elem_ty} data[{max}];"));
                self.line(1, "size_t len;");
                self.line(0, &format!("}} {name};"));
            }
            WireType::Array { elem, count } => {
                let elem_ty = self.c_type(elem);
                self.line(0, &format!("typedef {elem_ty} {name}[{count}];"));
            }
            _ => {
                let target_ty = self.c_type(target);
                self.line(0, &format!("typedef {target_ty} {name};"));
            }
        }
    }

    fn declare_scaled(&mut self, name: &str, s: &Scaled) {
        let physical = match s.physical {
            FloatType::F32 => "float",
            FloatType::F64 => "double",
        };
        let raw = Self::int_type(s.raw_bits, s.signed);
        let prefix = screaming(name);
        self.line(0, &format!("typedef {physical} {name};"));
        self.note(0, "physical = raw * SCALE + OFFSET (§4).");
        self.line(0, &format!("#define {prefix}_SCALE {}", Self::float_lit(s.scale)));
        self.line(0, &format!("#define {prefix}_OFFSET {}", Self::float_lit(s.offset)));
        self.note(0, "The underlying wire integer, for round-tripping without rounding (§4).");
        self.line(0, &format!("typedef {raw} {name}Raw;"));
    }

    fn declare_enum(&mut self, def: &TypeDef, name: &str, e: &Enum) {
        let carrier = carrier_bits(e.backing_bits);
        let backing = Self::int_type(e.backing_bits, false);
        let bits = e.backing_bits;
        self.line(0, &format!("typedef {backing} {name}; /* {bits} bits on the wire */"));
        let prefix = screaming(&def.name);
        for v in &e.variants {
            self.docs(0, &v.docs);
            let value = Self::uint_lit(v.value, carrier, false);
            self.line(0, &format!("#define {prefix}_{} (({name}){value})", screaming(&v.name)));
        }
        if let Some(arm) = &e.else_arm {
            let fnp = snake(&def.name);
            self.docs(0, &arm.docs);
            self.note(
                0,
                &format!(
                    "Open enum (§5): a wire value outside the set above is `{}`. It keeps that \
                     value, which no declared variant can collide with; test for it with \
                     {fnp}_is_known().",
                    arm.name
                ),
            );
        }
    }

    fn declare_union(&mut self, def: &TypeDef, name: &str, u: &Union) {
        let prefix = screaming(&def.name);
        let tag_type = Self::int_type(u.tag_bits, false);
        let tag_carrier = carrier_bits(u.tag_bits);
        let tag = ident(&u.tag_name);

        for v in &u.variants {
            self.docs(0, &v.docs);
            let id = Self::uint_lit(v.id, tag_carrier, true);
            self.line(0, &format!("#define {prefix}_{} (({tag_type}){id})", screaming(&v.name)));
        }

        self.line(0, "typedef struct {");
        self.note(1, &format!("Discriminant: one of the {prefix}_* ids above."));
        self.line(1, &format!("{tag_type} {tag};"));

        let has_payload =
            u.variants.iter().any(|v| v.fields.iter().any(Field::is_visible)) || u.else_arm.is_some();
        if has_payload {
            self.note(1, "The live member is the one the discriminant selects.");
            self.line(1, "union {");
            for v in &u.variants {
                if !v.fields.iter().any(Field::is_visible) {
                    continue;
                }
                self.docs(2, &v.docs);
                self.line(2, "struct {");
                for f in &v.fields {
                    if let Some(fname) = f.name() {
                        self.field_member(3, f, &ident(fname));
                    }
                }
                self.line(2, &format!("}} {};", ident(&snake(&v.name))));
            }
            if let Some(arm) = &u.else_arm {
                let raw_ty = Self::int_type(arm.raw_bits, false);
                let raw_bits = arm.raw_bits;
                self.docs(2, &arm.docs);
                self.note(
                    2,
                    &format!(
                        "Open union (§7): an unrecognized id lands here, left in `{tag}`, with \
                         the undecoded payload in `raw`."
                    ),
                );
                self.line(2, "struct {");
                self.line(3, &format!("{raw_ty} raw; /* {raw_bits} bits */"));
                self.line(2, &format!("}} {};", ident(&snake(&arm.name))));
            }
            self.line(1, "} payload;");
        }
        self.line(0, &format!("}} {name};"));
    }

    fn declare_struct(&mut self, name: &str, s: &Struct) {
        self.line(0, "typedef struct {");
        let mut any = false;
        for f in &s.fields {
            let Some(fname) = f.name() else { continue };
            any = true;
            self.field_member(1, f, &ident(fname));
        }
        if !any {
            self.note(1, "This container is entirely padding, and C forbids an empty struct.");
            self.line(1, "char _unused;");
        }
        self.line(0, &format!("}} {name};"));
    }

    /// One member of a generated struct, with its doc comment and, for a
    /// `reserved` field, the note that it round-trips rather than belonging to
    /// the caller (§6.2).
    fn field_member(&mut self, ind: usize, f: &Field, name: &str) {
        self.docs(ind, &f.docs);
        if let FieldRole::Reserved { .. } = f.role {
            self.note(ind, "Reserved (§6.2): captured on decode, written back unchanged.");
        }
        for line in self.member_lines(&f.ty, name) {
            self.line(ind, &line);
        }
    }

    /// `<TYPE>_SIZE` for a fixed type, or the prefix/maximum pair for a
    /// variable-length one (§6.3).
    fn size_constants(&mut self, def: &TypeDef, name: &str) {
        if !self.emits_codec(def) {
            return;
        }
        let prefix = screaming(&def.name);
        let layout = def.layout;
        match layout.tail {
            None => {
                self.note(0, &format!("Encoded size of `{name}`, in bytes."));
                self.line(0, &format!("#define {prefix}_SIZE {}u", layout.fixed_bytes()));
            }
            Some(_) => {
                self.note(0, "Bytes always present, before the variable-length tail (§6.3).");
                self.line(0, &format!("#define {prefix}_FIXED_SIZE {}u", layout.fixed_bytes()));
                self.note(0, "Largest legal encoding — what a receive buffer must hold.");
                self.line(0, &format!("#define {prefix}_MAX_SIZE {}u", layout.max_bytes()));
            }
        }
    }

    // ---------------------------------------------------------------------
    // Implementations
    // ---------------------------------------------------------------------

    /// Whether a type needs packing functions of its own.
    ///
    /// Structs and tagged unions always do — they may be nested anywhere — as
    /// does anything owning a variable-length tail, since its container
    /// delegates to it. Everything else only does when it is bound to a
    /// characteristic: an alias, a `scaled` type and an enum are otherwise
    /// packed inline at their use site.
    fn emits_codec(&self, def: &TypeDef) -> bool {
        matches!(def.kind, TypeKind::Struct(_) | TypeKind::Union(_)) || def.layout.tail.is_some() || def.root
    }

    fn implementations(&mut self) {
        self.banner("Codecs");
        let m = self.m;
        for def in &m.types {
            match &def.kind {
                TypeKind::Enum(e) => self.enum_helpers(def, e),
                TypeKind::Scaled(s) => self.scaled_helpers(def, s),
                TypeKind::Union(u) => self.union_helpers(def, u),
                _ => {}
            }
            if self.emits_codec(def) {
                self.pack_fixed_fn(def);
                self.unpack_fixed_fn(def);
                if def.layout.tail.is_some() {
                    self.tail_fns(def);
                }
            }
            if def.root {
                self.entry_points(def);
            }
        }
    }

    // -- per-kind helpers ---------------------------------------------------

    fn enum_helpers(&mut self, def: &TypeDef, e: &Enum) {
        let name = ident(&def.name);
        let fnp = snake(&def.name);
        let prefix = screaming(&def.name);

        self.note(0, &format!("Whether `v` is one of `{name}`'s declared variants (§5)."));
        self.line(0, &format!("static inline bool {fnp}_is_known({name} v) {{"));
        self.line(1, "switch (v) {");
        for v in &e.variants {
            self.line(2, &format!("case {prefix}_{}:", screaming(&v.name)));
        }
        self.line(3, "return true;");
        self.line(2, "default: return false;");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();

        self.note(0, "The variant's schema name, or NULL if `v` matches none.");
        self.line(0, &format!("static inline const char *{fnp}_name({name} v) {{"));
        self.line(1, "switch (v) {");
        for v in &e.variants {
            self.line(2, &format!("case {prefix}_{}: return \"{}\";", screaming(&v.name), v.name));
        }
        self.line(2, "default: return NULL;");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();
    }

    fn union_helpers(&mut self, def: &TypeDef, u: &Union) {
        let name = ident(&def.name);
        let fnp = snake(&def.name);
        let prefix = screaming(&def.name);
        let tag_type = Self::int_type(u.tag_bits, false);
        let tag = ident(&u.tag_name);

        self.note(0, &format!("Whether `{tag}` names a declared variant of `{name}` (§7)."));
        self.line(0, &format!("static inline bool {fnp}_is_known({tag_type} {tag}) {{"));
        self.line(1, &format!("switch ({tag}) {{"));
        for v in &u.variants {
            self.line(2, &format!("case {prefix}_{}:", screaming(&v.name)));
        }
        self.line(3, "return true;");
        self.line(2, "default: return false;");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();

        self.note(0, "The variant's schema name, or NULL if the id matches none.");
        self.line(0, &format!("static inline const char *{fnp}_name({tag_type} {tag}) {{"));
        self.line(1, &format!("switch ({tag}) {{"));
        for v in &u.variants {
            self.line(2, &format!("case {prefix}_{}: return \"{}\";", screaming(&v.name), v.name));
        }
        self.line(2, "default: return NULL;");
        self.line(1, "}");
        self.line(0, "}");
        self.blank();
    }

    fn scaled_helpers(&mut self, def: &TypeDef, s: &Scaled) {
        let name = ident(&def.name);
        let fnp = snake(&def.name);
        let prefix = screaming(&def.name);
        let raw = format!("{name}Raw");

        self.note(0, "Decodes the raw wire integer into the physical value (§4).");
        self.line(0, &format!("static inline {name} {fnp}_from_raw({raw} raw) {{"));
        self.line(1, &format!("return ({name})((double)raw * {prefix}_SCALE + {prefix}_OFFSET);"));
        self.line(0, "}");
        self.blank();

        // The bound is checked before the cast, because converting an
        // out-of-range double to an integer is undefined behaviour, not a
        // wraparound. Powers of two are exact as doubles at every width the
        // spec allows, so `r < 2^k` stands in faithfully for `r <= 2^k - 1`
        // even where `2^k - 1` itself is not exactly representable.
        let (lo, hi) = if s.signed {
            (format!("-{}", Self::pow2_lit(s.raw_bits - 1)), Self::pow2_lit(s.raw_bits - 1))
        } else {
            ("0.0".to_string(), Self::pow2_lit(s.raw_bits))
        };
        self.note(0, "Rounds to the nearest raw value, rejecting anything out of range (§4).");
        self.line(0, &format!("static inline defgen_err_t {fnp}_to_raw({name} v, {raw} *raw) {{"));
        self.line(1, &format!("double r = defgen__round(((double)v - {prefix}_OFFSET) / {prefix}_SCALE);"));
        self.line(1, &format!("if (!(r >= {lo} && r < {hi})) return DEFGEN_ERR_RANGE;"));
        self.line(1, &format!("*raw = ({raw})r;"));
        self.line(1, "return DEFGEN_OK;");
        self.line(0, "}");
        self.blank();
    }

    // -- fixed-part packing -------------------------------------------------

    fn pack_fixed_fn(&mut self, def: &'m TypeDef) {
        let name = ident(&def.name);
        let fnp = snake(&def.name);
        let body = self.capture(|e| {
            let ctx = Ctx::params();
            match &def.kind {
                TypeKind::Struct(s) => {
                    for f in &s.fields {
                        e.pack_field(1, &ctx, f);
                    }
                }
                TypeKind::Union(u) => e.pack_union(1, &ctx, def, u),
                _ => e.pack(1, &ctx, "(*v)", &WireType::Named(def.id), "off"),
            }
        });
        self.note(0, "Packs the fixed part into an in-progress buffer. Internal.");
        self.line(
            0,
            &format!(
                "static inline defgen_err_t {fnp}__pack_fixed(const {name} *v, uint8_t *buf, \
                 size_t size, int big, uint32_t off) {{"
            ),
        );
        self.body(&body);
        self.line(1, "return DEFGEN_OK;");
        self.line(0, "}");
        self.blank();
    }

    fn unpack_fixed_fn(&mut self, def: &'m TypeDef) {
        let name = ident(&def.name);
        let fnp = snake(&def.name);
        let body = self.capture(|e| {
            let ctx = Ctx::params();
            match &def.kind {
                TypeKind::Struct(s) => {
                    // Zeroing first means a variable-length field's unused
                    // capacity is never stale and a nested union's inactive
                    // members read as zero rather than as whatever was there.
                    e.line(1, "memset(v, 0, sizeof(*v));");
                    for f in &s.fields {
                        e.unpack_field(1, &ctx, f);
                    }
                }
                TypeKind::Union(u) => {
                    e.line(1, "memset(v, 0, sizeof(*v));");
                    e.unpack_union(1, &ctx, def, u);
                }
                _ => e.unpack(1, &ctx, "(*v)", &WireType::Named(def.id), "off"),
            }
        });
        self.note(0, "Unpacks the fixed part from a received buffer. Internal.");
        self.line(
            0,
            &format!(
                "static inline defgen_err_t {fnp}__unpack_fixed({name} *v, const uint8_t *buf, \
                 size_t size, int big, uint32_t off) {{"
            ),
        );
        self.body(&body);
        self.line(1, "return DEFGEN_OK;");
        self.line(0, "}");
        self.blank();
    }

    /// Writes a captured function body, prefixed by a `(void)` cast for every
    /// parameter it never touches.
    ///
    /// The signature is the same for every type so that callers do not have to
    /// care what kind they are packing, which leaves parameters genuinely
    /// unused for a type made only of padding, or only of a variable-length
    /// tail. Silencing that here means a project can build generated code under
    /// `-Wextra -Werror`.
    fn body(&mut self, body: &str) {
        let unused: Vec<String> = ["v", "buf", "size", "big", "off"]
            .into_iter()
            .filter(|p| !mentions(body, p))
            .map(|p| format!("(void){p};"))
            .collect();
        if !unused.is_empty() {
            self.line(1, &unused.join(" "));
        }
        self.out.push_str(body);
    }

    fn pack_field(&mut self, ind: usize, ctx: &Ctx, f: &'m Field) {
        // `padding` is written as zero (§6.2) and the entry point zeroes the
        // whole buffer before packing, so there is nothing to emit for it.
        let Some(name) = f.name() else { return };
        let expr = format!("v->{}", ident(name));
        let off = Self::offset("off", f.offset_bits);
        match self.tail_shape(&f.ty) {
            // An inline variable-length field contributes no fixed bits; the
            // tail functions are what write it.
            Some(TailShape::Inline) => {}
            // A named variable-length type still has a fixed prefix of its own.
            Some(TailShape::Nested) | None => self.pack(ind, ctx, &expr, &f.ty, &off),
        }
    }

    fn unpack_field(&mut self, ind: usize, ctx: &Ctx, f: &'m Field) {
        if let FieldRole::Padding { check_zero } = f.role {
            if check_zero {
                let off = Self::offset("off", f.offset_bits);
                let bits = f.layout.fixed_bits;
                self.line(
                    ind,
                    &format!(
                        "if (!defgen__bits_zero({}, {}, {}, {off}, {bits}u)) return \
                         DEFGEN_ERR_PADDING;",
                        ctx.buf, ctx.size, ctx.big
                    ),
                );
            }
            return;
        }
        let Some(name) = f.name() else { return };
        let expr = format!("v->{}", ident(name));
        let off = Self::offset("off", f.offset_bits);
        match self.tail_shape(&f.ty) {
            Some(TailShape::Inline) => {}
            Some(TailShape::Nested) | None => self.unpack(ind, ctx, &expr, &f.ty, &off),
        }
    }

    fn pack_union(&mut self, ind: usize, ctx: &Ctx, def: &'m TypeDef, u: &'m Union) {
        let prefix = screaming(&def.name);
        let tag = ident(&u.tag_name);
        self.pack_int(ind, ctx, &format!("v->{tag}"), u.tag_bits, false, "off");
        self.line(ind, &format!("switch (v->{tag}) {{"));
        for v in &u.variants {
            self.line(ind, &format!("case {prefix}_{}:", screaming(&v.name)));
            let member = ident(&snake(&v.name));
            for f in &v.fields {
                let Some(name) = f.name() else { continue };
                let off = Self::offset("off", u.tag_bits + f.offset_bits);
                let expr = format!("v->payload.{member}.{}", ident(name));
                self.pack(ind + 1, ctx, &expr, &f.ty, &off);
            }
            self.line(ind + 1, "break;");
        }
        self.line(ind, "default:");
        match &u.else_arm {
            Some(arm) => {
                let member = ident(&snake(&arm.name));
                let off = Self::offset("off", u.tag_bits);
                let expr = format!("v->payload.{member}.raw");
                self.pack_int(ind + 1, ctx, &expr, arm.raw_bits, false, &off);
                self.line(ind + 1, "break;");
            }
            // A closed union has no representation for an unrecognized id, so
            // one cannot be produced by accident (§7).
            None => self.line(ind + 1, "return DEFGEN_ERR_UNKNOWN_VALUE;"),
        }
        self.line(ind, "}");
    }

    fn unpack_union(&mut self, ind: usize, ctx: &Ctx, def: &'m TypeDef, u: &'m Union) {
        let prefix = screaming(&def.name);
        let tag = ident(&u.tag_name);
        self.unpack_int(ind, ctx, &format!("v->{tag}"), u.tag_bits, false, "off", None);
        self.line(ind, &format!("switch (v->{tag}) {{"));
        for v in &u.variants {
            self.line(ind, &format!("case {prefix}_{}:", screaming(&v.name)));
            let member = ident(&snake(&v.name));
            for f in &v.fields {
                match &f.role {
                    FieldRole::Padding { check_zero } => {
                        if *check_zero {
                            let off = Self::offset("off", u.tag_bits + f.offset_bits);
                            let bits = f.layout.fixed_bits;
                            self.line(
                                ind + 1,
                                &format!(
                                    "if (!defgen__bits_zero({}, {}, {}, {off}, {bits}u)) return \
                                     DEFGEN_ERR_PADDING;",
                                    ctx.buf, ctx.size, ctx.big
                                ),
                            );
                        }
                    }
                    FieldRole::Value { name } | FieldRole::Reserved { name } => {
                        let off = Self::offset("off", u.tag_bits + f.offset_bits);
                        let expr = format!("v->payload.{member}.{}", ident(name));
                        self.unpack(ind + 1, ctx, &expr, &f.ty, &off);
                    }
                }
            }
            self.line(ind + 1, "break;");
        }
        self.line(ind, "default:");
        match &u.else_arm {
            Some(arm) => {
                let member = ident(&snake(&arm.name));
                let off = Self::offset("off", u.tag_bits);
                let expr = format!("v->payload.{member}.raw");
                self.unpack_int(ind + 1, ctx, &expr, arm.raw_bits, false, &off, None);
                self.line(ind + 1, "break;");
            }
            None => self.line(ind + 1, "return DEFGEN_ERR_UNKNOWN_VALUE;"),
        }
        self.line(ind, "}");
    }

    // -- the value-level emitters ------------------------------------------

    /// Emits the statements that write `expr` into the buffer at bit `off`.
    fn pack(&mut self, ind: usize, ctx: &Ctx, expr: &str, ty: &WireType, off: &str) {
        match ty {
            WireType::UInt(n) => self.pack_int(ind, ctx, expr, *n, false, off),
            WireType::Int(n) => self.pack_int(ind, ctx, expr, *n, true, off),
            WireType::Bool => self.line(
                ind,
                &format!(
                    "defgen__put({}, {}, {}, {off}, 1u, ({expr}) ? UINT64_C(1) : UINT64_C(0));",
                    ctx.buf, ctx.size, ctx.big
                ),
            ),
            WireType::Named(id) => self.pack_named(ind, ctx, expr, *id, off),
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                self.array_loop(ind, *count, elem_bits, off, |e, ind, i, elem_off| {
                    e.pack(ind, ctx, &format!("({expr})[{i}]"), elem, &elem_off);
                });
            }
            // Unreachable: a variable-length field is written by the tail
            // functions, never as part of the fixed prefix.
            WireType::VarArray { .. } | WireType::Str { .. } => {}
        }
    }

    fn unpack(&mut self, ind: usize, ctx: &Ctx, expr: &str, ty: &WireType, off: &str) {
        match ty {
            WireType::UInt(n) => self.unpack_int(ind, ctx, expr, *n, false, off, None),
            WireType::Int(n) => self.unpack_int(ind, ctx, expr, *n, true, off, None),
            WireType::Bool => self.line(
                ind,
                &format!("{expr} = defgen__get({}, {}, {}, {off}, 1u) != 0;", ctx.buf, ctx.size, ctx.big),
            ),
            WireType::Named(id) => self.unpack_named(ind, ctx, expr, *id, off),
            WireType::Array { elem, count } => {
                let elem_bits = self.m.layout_of(elem).fixed_bits;
                self.array_loop(ind, *count, elem_bits, off, |e, ind, i, elem_off| {
                    e.unpack(ind, ctx, &format!("({expr})[{i}]"), elem, &elem_off);
                });
            }
            WireType::VarArray { .. } | WireType::Str { .. } => {}
        }
    }

    /// A `for` over a fixed-size array (§6.1), with a loop variable unique to
    /// its nesting depth and the element's bit offset derived from the index.
    fn array_loop(
        &mut self,
        ind: usize,
        count: u64,
        elem_bits: u32,
        off: &str,
        body: impl FnOnce(&mut Self, usize, &str, String),
    ) {
        let i = format!("i{}", self.depth);
        let elem_off = format!("{off} + (uint32_t)({i} * {elem_bits}u)");
        self.line(ind, &format!("{{ size_t {i}; for ({i} = 0; {i} < {count}u; {i}++) {{"));
        self.depth += 1;
        body(self, ind + 1, &i, elem_off);
        self.depth -= 1;
        self.line(ind, "} }");
    }

    fn pack_int(&mut self, ind: usize, ctx: &Ctx, expr: &str, bits: u32, signed: bool, off: &str) {
        let carrier = carrier_bits(bits);
        // Only worth checking when the carrier is wider than the field: at an
        // exact fit no value of the C type can be out of range (§2).
        if carrier > bits {
            if signed {
                let (lo, hi) = Self::int_bounds(bits);
                self.line(ind, &format!("if (({expr}) < {lo} || ({expr}) > {hi}) return DEFGEN_ERR_RANGE;"));
            } else {
                let hi = Self::umax_expr(bits);
                self.line(ind, &format!("if (({expr}) > {hi}) return DEFGEN_ERR_RANGE;"));
            }
        }
        let (put, cast) = match (carrier > 64, signed) {
            (true, true) => ("defgen__put_wide", "(defgen_u128)(defgen_i128)"),
            (true, false) => ("defgen__put_wide", "(defgen_u128)"),
            (false, true) => ("defgen__put", "(uint64_t)(int64_t)"),
            (false, false) => ("defgen__put", "(uint64_t)"),
        };
        self.line(
            ind,
            &format!("{put}({}, {}, {}, {off}, {bits}u, {cast}({expr}));", ctx.buf, ctx.size, ctx.big),
        );
    }

    /// Reads `bits` bits into `expr`. `as_type` overrides the destination's C
    /// type, which an enum needs (its own typedef, not `uintN_t`).
    #[allow(clippy::too_many_arguments)]
    fn unpack_int(
        &mut self,
        ind: usize,
        ctx: &Ctx,
        expr: &str,
        bits: u32,
        signed: bool,
        off: &str,
        as_type: Option<&str>,
    ) {
        let carrier = carrier_bits(bits);
        let ty = as_type.map(str::to_string).unwrap_or_else(|| Self::int_type(bits, signed));
        let get = if carrier > 64 { "defgen__get_wide" } else { "defgen__get" };
        let read = format!("{get}({}, {}, {}, {off}, {bits}u)", ctx.buf, ctx.size, ctx.big);
        if signed && carrier > bits {
            let sext = if carrier > 64 { "defgen__sext_wide" } else { "defgen__sext" };
            self.line(ind, &format!("{expr} = ({ty}){sext}({read}, {bits}u);"));
        } else {
            self.line(ind, &format!("{expr} = ({ty})({read});"));
        }
    }

    fn pack_named(&mut self, ind: usize, ctx: &Ctx, expr: &str, id: TypeId, off: &str) {
        let def = self.m.get(id);
        let fnp = snake(&def.name);
        match &def.kind {
            TypeKind::Alias(a) => self.pack(ind, ctx, expr, &a.target, off),
            TypeKind::Scaled(s) => {
                let raw = self.tmp("raw");
                let err = self.tmp("e");
                let raw_ty = format!("{}Raw", ident(&def.name));
                let (bits, signed) = (s.raw_bits, s.signed);
                self.line(ind, "{");
                self.line(ind + 1, &format!("{raw_ty} {raw};"));
                self.line(ind + 1, &format!("defgen_err_t {err} = {fnp}_to_raw({expr}, &{raw});"));
                self.line(ind + 1, &format!("if ({err} != DEFGEN_OK) return {err};"));
                self.pack_int(ind + 1, ctx, &raw, bits, signed, off);
                self.line(ind, "}");
            }
            TypeKind::Enum(e) => {
                // A closed enum has no representation for an undeclared value,
                // so writing one is an error rather than something that quietly
                // reaches the wire (§5).
                if !e.is_open() {
                    self.line(ind, &format!("if (!{fnp}_is_known({expr})) return DEFGEN_ERR_UNKNOWN_VALUE;"));
                }
                self.pack_int(ind, ctx, expr, e.backing_bits, false, off);
            }
            TypeKind::Union(_) | TypeKind::Struct(_) => {
                let err = self.tmp("e");
                self.line(ind, "{");
                self.line(
                    ind + 1,
                    &format!(
                        "defgen_err_t {err} = {fnp}__pack_fixed(&({expr}), {}, {}, {}, {off});",
                        ctx.buf, ctx.size, ctx.big
                    ),
                );
                self.line(ind + 1, &format!("if ({err} != DEFGEN_OK) return {err};"));
                self.line(ind, "}");
            }
        }
    }

    fn unpack_named(&mut self, ind: usize, ctx: &Ctx, expr: &str, id: TypeId, off: &str) {
        let def = self.m.get(id);
        let fnp = snake(&def.name);
        let name = ident(&def.name);
        match &def.kind {
            TypeKind::Alias(a) => self.unpack(ind, ctx, expr, &a.target, off),
            TypeKind::Scaled(s) => {
                let raw = self.tmp("raw");
                let raw_ty = format!("{name}Raw");
                let (bits, signed) = (s.raw_bits, s.signed);
                self.line(ind, "{");
                self.line(ind + 1, &format!("{raw_ty} {raw};"));
                self.unpack_int(ind + 1, ctx, &raw, bits, signed, off, Some(&raw_ty));
                self.line(ind + 1, &format!("{expr} = {fnp}_from_raw({raw});"));
                self.line(ind, "}");
            }
            TypeKind::Enum(e) => {
                self.unpack_int(ind, ctx, expr, e.backing_bits, false, off, Some(&name));
                if !e.is_open() {
                    self.line(ind, &format!("if (!{fnp}_is_known({expr})) return DEFGEN_ERR_UNKNOWN_VALUE;"));
                }
            }
            TypeKind::Union(_) | TypeKind::Struct(_) => {
                let err = self.tmp("e");
                self.line(ind, "{");
                self.line(
                    ind + 1,
                    &format!(
                        "defgen_err_t {err} = {fnp}__unpack_fixed(&({expr}), {}, {}, {}, {off});",
                        ctx.buf, ctx.size, ctx.big
                    ),
                );
                self.line(ind + 1, &format!("if ({err} != DEFGEN_OK) return {err};"));
                self.line(ind, "}");
            }
        }
    }

    // -- variable-length tails ---------------------------------------------

    /// How a field's variable-length tail, if it has one, is laid out in C:
    /// flattened into the parent (an inline `string`/`Type[max: N]`) or owned
    /// by a named type that has tail functions of its own.
    fn tail_shape(&self, ty: &WireType) -> Option<TailShape> {
        match ty {
            WireType::Str { .. } | WireType::VarArray { .. } => Some(TailShape::Inline),
            WireType::Named(id) if self.m.get(*id).layout.tail.is_some() => Some(TailShape::Nested),
            _ => None,
        }
    }

    /// The tail of a whole type: the trailing field of a struct, or the target
    /// of an alias that can be bound straight to a characteristic (§6.3).
    fn tail_of_type(&self, def: &TypeDef) -> Option<Tail> {
        match &def.kind {
            TypeKind::Struct(s) => {
                let f = s.fields.last()?;
                let name = ident(f.name()?);
                self.tail_from(&f.ty, &format!("v->{name}"), &format!("v->{name}_len"))
            }
            TypeKind::Alias(a) => self.tail_from(&a.target, "v->data", "v->len"),
            _ => None,
        }
    }

    fn tail_from(&self, ty: &WireType, member: &str, len_member: &str) -> Option<Tail> {
        match ty {
            WireType::Str { max } => {
                Some(Tail::Str { member: member.to_string(), len_member: len_member.to_string(), max: *max })
            }
            WireType::VarArray { elem, max } => Some(Tail::Arr {
                member: member.to_string(),
                len_member: len_member.to_string(),
                elem: (**elem).clone(),
                max: *max,
                elem_bytes: u64::from(self.m.layout_of(elem).fixed_bits) / 8,
            }),
            WireType::Named(id) if self.m.get(*id).layout.tail.is_some() => {
                Some(Tail::Nested { id: *id, member: member.to_string() })
            }
            _ => None,
        }
    }

    fn tail_fns(&mut self, def: &'m TypeDef) {
        let Some(tail) = self.tail_of_type(def) else { return };
        let name = ident(&def.name);
        let fnp = snake(&def.name);
        let child = match &tail {
            Tail::Nested { id, .. } => snake(&self.m.get(*id).name),
            _ => String::new(),
        };

        // -- length --
        self.note(0, "Bytes this value's variable-length tail occupies. Internal.");
        self.line(0, &format!("static inline size_t {fnp}__tail_len(const {name} *v) {{"));
        match &tail {
            Tail::Str { len_member, .. } => self.line(1, &format!("return {len_member};")),
            Tail::Arr { len_member, elem_bytes, .. } => {
                self.line(1, &format!("return {len_member} * {elem_bytes}u;"))
            }
            Tail::Nested { member, .. } => self.line(1, &format!("return {child}__tail_len(&{member});")),
        }
        self.line(0, "}");
        self.blank();

        // -- pack --
        self.note(0, "Writes the variable-length tail after the fixed prefix. Internal.");
        self.line(
            0,
            &format!(
                "static inline defgen_err_t {fnp}__pack_tail(const {name} *v, uint8_t *out, \
                 size_t cap, int big) {{"
            ),
        );
        match &tail {
            Tail::Str { member, len_member, max } => {
                self.line(1, "(void)big;");
                self.line(1, &format!("if ({len_member} > {max}u) return DEFGEN_ERR_RANGE;"));
                self.line(1, &format!("if (cap < {len_member}) return DEFGEN_ERR_BUFFER_TOO_SMALL;"));
                self.line(1, &format!("memcpy(out, {member}, {len_member});"));
            }
            Tail::Arr { member, len_member, elem, max, elem_bytes } => {
                let (member, len_member, elem, max, bytes) =
                    (member.clone(), len_member.clone(), elem.clone(), *max, *elem_bytes);
                self.line(1, &format!("if ({len_member} > {max}u) return DEFGEN_ERR_RANGE;"));
                self.line(
                    1,
                    &format!("if (cap < {len_member} * {bytes}u) return DEFGEN_ERR_BUFFER_TOO_SMALL;"),
                );
                self.line(1, &format!("memset(out, 0, {len_member} * {bytes}u);"));
                let i = format!("i{}", self.depth);
                let ctx = Ctx::tail_elem("out", &i, bytes);
                self.line(1, &format!("{{ size_t {i}; for ({i} = 0; {i} < {len_member}; {i}++) {{"));
                self.depth += 1;
                self.pack(2, &ctx, &format!("{member}[{i}]"), &elem, "0u");
                self.depth -= 1;
                self.line(1, "} }");
            }
            Tail::Nested { member, .. } => {
                self.line(1, &format!("return {child}__pack_tail(&{member}, out, cap, big);"))
            }
        }
        self.line(1, "return DEFGEN_OK;");
        self.line(0, "}");
        self.blank();

        // -- unpack --
        self.note(0, "Reads the variable-length tail; `len` is what the transport delivered.");
        self.line(
            0,
            &format!(
                "static inline defgen_err_t {fnp}__unpack_tail({name} *v, const uint8_t *in, \
                 size_t len, int big) {{"
            ),
        );
        match &tail {
            Tail::Str { member, len_member, max } => {
                self.line(1, "(void)big;");
                self.line(1, &format!("if (len > {max}u) return DEFGEN_ERR_LENGTH;"));
                self.line(1, "if (!defgen__utf8_valid(in, len)) return DEFGEN_ERR_UTF8;");
                self.line(1, &format!("memcpy({member}, in, len);"));
                self.line(1, &format!("memset({member} + len, 0, sizeof({member}) - len);"));
                self.line(1, &format!("{len_member} = len;"));
            }
            Tail::Arr { member, len_member, elem, max, elem_bytes } => {
                let (member, len_member, elem, max, bytes) =
                    (member.clone(), len_member.clone(), elem.clone(), *max, *elem_bytes);
                // A remainder means the bytes on the wire do not correspond to
                // a whole number of elements, which §6.3 makes a hard error.
                self.line(1, &format!("if (len % {bytes}u != 0) return DEFGEN_ERR_LENGTH;"));
                self.line(1, &format!("if (len / {bytes}u > {max}u) return DEFGEN_ERR_LENGTH;"));
                self.line(1, &format!("memset({member}, 0, sizeof({member}));"));
                self.line(1, &format!("{len_member} = len / {bytes}u;"));
                let i = format!("i{}", self.depth);
                let ctx = Ctx::tail_elem("in", &i, bytes);
                self.line(1, &format!("{{ size_t {i}; for ({i} = 0; {i} < {len_member}; {i}++) {{"));
                self.depth += 1;
                self.unpack(2, &ctx, &format!("{member}[{i}]"), &elem, "0u");
                self.depth -= 1;
                self.line(1, "} }");
            }
            Tail::Nested { member, .. } => {
                self.line(1, &format!("return {child}__unpack_tail(&{member}, in, len, big);"))
            }
        }
        self.line(1, "return DEFGEN_OK;");
        self.line(0, "}");
        self.blank();
    }

    // -- public entry points ------------------------------------------------

    /// The encode/decode pair a characteristic-bound type gets (§12). This is
    /// the only place byte order becomes a concrete value: everything below
    /// takes it as a parameter.
    fn entry_points(&mut self, def: &'m TypeDef) {
        let name = ident(&def.name);
        let fnp = snake(&def.name);
        let prefix = screaming(&def.name);
        let big = i32::from(def.endian == Endianness::Big);
        let order = def.endian.as_str();

        if def.layout.tail.is_none() {
            self.note(
                0,
                &format!(
                    "Encodes a `{name}` into exactly {prefix}_SIZE bytes, {order}-endian. `len`, \
                     if non-NULL, receives the byte count."
                ),
            );
            self.line(
                0,
                &format!(
                    "static inline defgen_err_t {fnp}_encode(const {name} *v, uint8_t *buf, \
                     size_t cap, size_t *len) {{"
                ),
            );
            self.line(1, "defgen_err_t err;");
            self.line(1, &format!("if (cap < {prefix}_SIZE) return DEFGEN_ERR_BUFFER_TOO_SMALL;"));
            self.line(1, &format!("memset(buf, 0, {prefix}_SIZE);"));
            self.line(1, &format!("err = {fnp}__pack_fixed(v, buf, {prefix}_SIZE, {big}, 0u);"));
            self.line(1, "if (err != DEFGEN_OK) return err;");
            self.line(1, &format!("if (len) *len = {prefix}_SIZE;"));
            self.line(1, "return DEFGEN_OK;");
            self.line(0, "}");
            self.blank();

            self.note(
                0,
                &format!(
                    "Decodes exactly {prefix}_SIZE bytes into `v`; any other length is \
                     DEFGEN_ERR_LENGTH."
                ),
            );
            self.line(
                0,
                &format!(
                    "static inline defgen_err_t {fnp}_decode({name} *v, const uint8_t *buf, \
                     size_t len) {{"
                ),
            );
            self.line(1, &format!("if (len != {prefix}_SIZE) return DEFGEN_ERR_LENGTH;"));
            self.line(1, &format!("return {fnp}__unpack_fixed(v, buf, {prefix}_SIZE, {big}, 0u);"));
            self.line(0, "}");
            self.blank();
            return;
        }

        self.note(0, "Bytes this value encodes to as it currently stands (§6.3).");
        self.line(0, &format!("static inline size_t {fnp}_size(const {name} *v) {{"));
        self.line(1, &format!("return {prefix}_FIXED_SIZE + {fnp}__tail_len(v);"));
        self.line(0, "}");
        self.blank();

        self.note(
            0,
            &format!(
                "Encodes a `{name}`, {order}-endian, producing exactly {fnp}_size(v) bytes — \
                 never padded out to {prefix}_MAX_SIZE (§6.3)."
            ),
        );
        self.line(
            0,
            &format!(
                "static inline defgen_err_t {fnp}_encode(const {name} *v, uint8_t *buf, \
                 size_t cap, size_t *len) {{"
            ),
        );
        self.line(1, "defgen_err_t err;");
        self.line(1, &format!("size_t total = {fnp}_size(v);"));
        self.line(1, &format!("if (total > {prefix}_MAX_SIZE) return DEFGEN_ERR_RANGE;"));
        self.line(1, "if (cap < total) return DEFGEN_ERR_BUFFER_TOO_SMALL;");
        self.line(1, &format!("memset(buf, 0, {prefix}_FIXED_SIZE);"));
        self.line(1, &format!("err = {fnp}__pack_fixed(v, buf, {prefix}_FIXED_SIZE, {big}, 0u);"));
        self.line(1, "if (err != DEFGEN_OK) return err;");
        self.line(
            1,
            &format!(
                "err = {fnp}__pack_tail(v, buf + {prefix}_FIXED_SIZE, total - {prefix}_FIXED_SIZE, \
                 {big});"
            ),
        );
        self.line(1, "if (err != DEFGEN_OK) return err;");
        self.line(1, "if (len) *len = total;");
        self.line(1, "return DEFGEN_OK;");
        self.line(0, "}");
        self.blank();

        self.note(
            0,
            "Decodes the `len` bytes the transport delivered. The tail's element count comes \
             from `len`, never from the payload (§6.3).",
        );
        self.line(
            0,
            &format!(
                "static inline defgen_err_t {fnp}_decode({name} *v, const uint8_t *buf, \
                 size_t len) {{"
            ),
        );
        self.line(1, "defgen_err_t err;");
        // A type that is nothing but a tail has no minimum, and `len < 0` on a
        // size_t would warn under -Wtype-limits.
        if def.layout.fixed_bytes() > 0 {
            self.line(1, &format!("if (len < {prefix}_FIXED_SIZE) return DEFGEN_ERR_LENGTH;"));
        }
        self.line(1, &format!("if (len > {prefix}_MAX_SIZE) return DEFGEN_ERR_LENGTH;"));
        self.line(1, &format!("err = {fnp}__unpack_fixed(v, buf, {prefix}_FIXED_SIZE, {big}, 0u);"));
        self.line(1, "if (err != DEFGEN_OK) return err;");
        self.line(
            1,
            &format!(
                "return {fnp}__unpack_tail(v, buf + {prefix}_FIXED_SIZE, len - {prefix}_FIXED_SIZE, \
                 {big});"
            ),
        );
        self.line(0, "}");
        self.blank();
    }

    // ---------------------------------------------------------------------
    // GATT metadata (§10)
    // ---------------------------------------------------------------------

    /// UUIDs and property sets as constants. What a program does with them —
    /// which BLE stack it hands them to — is deliberately out of scope (§10).
    fn gatt(&mut self) {
        if self.m.services.is_empty() {
            return;
        }
        self.banner("GATT bindings");

        self.note(0, "GATT characteristic properties, as a bit set (§10).");
        self.line(0, "typedef enum {");
        for (i, p) in Property::ALL.iter().enumerate() {
            self.line(1, &format!("DEFGEN_PROP_{} = 1u << {i},", screaming(p.as_str())));
        }
        self.line(0, "} defgen_prop_t;");
        self.blank();

        self.note(
            0,
            "A GATT UUID as a brace initializer, in wire order (little-endian, the reverse \
             of how the UUID above is written) — e.g. `uint8_t u[] = FOO_UUID_BYTES;`.",
        );
        self.blank();

        let services = self.m.services.clone();
        for service in &services {
            let sprefix = screaming(&service.name);
            self.docs(0, &service.docs);
            self.line(0, &format!("#define {sprefix}_SERVICE_UUID \"{}\"", service.uuid));
            self.line(0, &format!("#define {sprefix}_SERVICE_UUID_BYTES {}", uuid_bytes(&service.uuid)));
            for c in &service.characteristics {
                let cprefix = format!("{sprefix}_{}", screaming(&c.name));
                let ty_name = ident(&self.m.get(c.ty).name);
                let size = match c.layout.tail {
                    None => format!("{} bytes", c.layout.fixed_bytes()),
                    Some(_) => {
                        format!("{}..{} bytes", c.layout.fixed_bytes(), c.layout.max_bytes())
                    }
                };
                self.docs(0, &c.docs);
                self.note(0, &format!("Carries a `{ty_name}` ({size})."));
                self.line(0, &format!("#define {cprefix}_UUID \"{}\"", c.uuid));
                self.line(0, &format!("#define {cprefix}_UUID_BYTES {}", uuid_bytes(&c.uuid)));
                let flags: Vec<String> =
                    c.properties.iter().map(|p| format!("DEFGEN_PROP_{}", screaming(p.as_str()))).collect();
                let flags = if flags.is_empty() { "0u".to_string() } else { flags.join(" | ") };
                self.line(0, &format!("#define {cprefix}_PROPERTIES ({flags})"));
            }
            self.blank();
        }
    }
}

/// How a variable-length field is spelled in C — see [`Emitter::tail_shape`].
enum TailShape {
    /// Flattened into the containing struct as a buffer plus a length.
    Inline,
    /// A named type that owns the tail, and the functions that handle it.
    Nested,
}
