//! The checked model: the schema as it looks once every rule in SPEC.md §11
//! has been enforced.
//!
//! Where [`crate::ast`] records what the author *wrote*, this records what the
//! schema *means*. Everything the AST deliberately leaves open is closed here:
//!
//! * names are resolved to [`TypeId`]s, so nothing downstream does lookups;
//! * every type has a [`Layout`] — its exact bit width, or its fixed prefix
//!   plus a bounded variable tail (§6.3);
//! * every field carries its LSB-first [`Field::offset_bits`] within its
//!   container (§6), so a backend never re-derives bit positions;
//! * implicit enum numbering is resolved to actual values (§5);
//! * byte order is resolved per type from the file default and any
//!   `#[endian(...)]` override (§8).
//!
//! A [`Model`] only ever exists for a schema that produced no errors, so its
//! invariants are guaranteed and its accessors are total: a backend can index,
//! add and divide without re-checking anything. The invariants are stated on
//! each field; the checker in [`crate::check`] is what establishes them.

use crate::ast::{Docs, Endianness, FloatType, Property};
use crate::span::Span;

/// Default ATT payload size, in bytes, without MTU negotiation (§10).
pub const DEFAULT_ATT_PAYLOAD_BYTES: u64 = 20;

// ---------------------------------------------------------------------------
// Sizes
// ---------------------------------------------------------------------------

/// The on-wire size of a type or field.
///
/// A fixed-width type is `fixed_bits` with no `tail`. A variable-length type
/// (§6.3) is a fixed prefix — always a whole number of bytes — followed by
/// between 0 and `max_elems` elements whose count comes from the buffer
/// length, never from the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Bits that are always present. Byte-aligned whenever `tail` is `Some`.
    pub fixed_bits: u32,
    /// The trailing variable-length part, if any.
    pub tail: Option<Tail>,
}

/// The variable-length tail of a [`Layout`] (§6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    /// Width of one element: 8 for `string(max: N)`, the element width for
    /// `Type[max: N]`. Always a positive multiple of 8.
    pub elem_bits: u32,
    /// Upper bound on the element count — `N` in both spellings.
    pub max_elems: u64,
}

impl Tail {
    /// The largest number of bits this tail can occupy.
    pub fn max_bits(self) -> u128 {
        u128::from(self.elem_bits) * u128::from(self.max_elems)
    }
}

impl Layout {
    pub const fn fixed(bits: u32) -> Layout {
        Layout { fixed_bits: bits, tail: None }
    }

    pub fn is_variable(self) -> bool {
        self.tail.is_some()
    }

    /// Bits always present, whatever the buffer holds.
    pub fn min_bits(self) -> u32 {
        self.fixed_bits
    }

    /// Bits in the largest legal encoding.
    pub fn max_bits(self) -> u128 {
        u128::from(self.fixed_bits) + self.tail.map_or(0, Tail::max_bits)
    }

    /// The fixed prefix in bytes. Exact for a fixed-width type; for a
    /// variable-length one this is the part every encoding starts with.
    pub fn fixed_bytes(self) -> u64 {
        u64::from(self.fixed_bits).div_ceil(8)
    }

    /// Bytes in the largest legal encoding — what the MTU diagnostic (§10) and
    /// a backend sizing a receive buffer both want.
    pub fn max_bytes(self) -> u128 {
        self.max_bits().div_ceil(8)
    }

    pub fn is_byte_aligned(self) -> bool {
        self.fixed_bits.is_multiple_of(8)
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Index of a [`TypeDef`] in [`Model::types`]. Declarations keep their source
/// order, so a smaller id was always declared earlier (§9 forbids forward
/// references, so a type's id is always greater than those it depends on).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(pub usize);

/// A resolved field/element type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireType {
    /// `uN`.
    UInt(u32),
    /// `iN`, two's complement.
    Int(u32),
    /// `bool` — one bit (§2). Kept distinct from `UInt(1)` so backends can
    /// expose it as a native boolean.
    Bool,
    /// An `alias`, `scaled`, `enum`, tagged union or `struct`. The name is
    /// preserved (rather than substituted away) so generated code can keep the
    /// domain type the author declared.
    Named(TypeId),
    /// `Type[N]` — exactly `count` fixed-width elements (§6.1).
    Array { elem: Box<WireType>, count: u64 },
    /// `Type[max: N]` — 0..=`max` elements, count derived from the buffer (§6.3).
    VarArray { elem: Box<WireType>, max: u64 },
    /// `string(max: N)` — UTF-8, at most `max` bytes (§6.3).
    Str { max: u64 },
}

/// The native integer a `uN`/`iN` value is carried in: the smallest of 8, 16,
/// 32, 64 or 128 bits that holds `bits` (§2).
///
/// A schema is free to declare any width up to 128 — bit-packed BLE payloads
/// are full of 4-, 12- and 48-bit values — so every backend needs this same
/// rounding, and gets it from here rather than reinventing it.
pub fn carrier_bits(bits: u32) -> u32 {
    match bits {
        0..=8 => 8,
        9..=16 => 16,
        17..=32 => 32,
        33..=64 => 64,
        _ => 128,
    }
}

impl WireType {
    pub fn is_variable(&self) -> bool {
        matches!(self, WireType::VarArray { .. } | WireType::Str { .. })
    }

    /// Width of the native integer this value is carried in, for the scalar
    /// types that are carried in one (§2). `None` for anything a backend
    /// represents as a composite: a named type, an array, a string.
    pub fn carrier_bits(&self) -> Option<u32> {
        match self {
            WireType::UInt(n) | WireType::Int(n) => Some(carrier_bits(*n)),
            WireType::Bool => Some(8),
            _ => None,
        }
    }

    /// Whether the value is signed, and so sign-extended from bit `N-1` on
    /// decode (§2).
    pub fn is_signed(&self) -> bool {
        matches!(self, WireType::Int(_))
    }

    /// The declared type this refers to, directly or as an array element.
    pub fn named(&self) -> Option<TypeId> {
        match self {
            WireType::Named(id) => Some(*id),
            WireType::Array { elem, .. } | WireType::VarArray { elem, .. } => elem.named(),
            _ => None,
        }
    }
}

/// One declared type, with everything the checker worked out about it.
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub id: TypeId,
    pub name: String,
    pub docs: Docs,
    pub span: Span,
    /// Wire size (§6.3: `layout.tail` is `Some` for a variable-length type).
    pub layout: Layout,
    /// Byte order to use when this type is encoded as a root value: its own
    /// `#[endian(...)]` if it has one, otherwise the file default (§8).
    pub endian: Endianness,
    /// Whether that byte order was written on the declaration itself.
    pub endian_explicit: bool,
    /// Bound to at least one characteristic (§10) — i.e. encoded on its own.
    /// Both this and [`TypeDef::nested`] can be true.
    pub root: bool,
    /// Used as the type of a field of some other container (§8's nested case).
    pub nested: bool,
    pub kind: TypeKind,
}

impl TypeDef {
    /// What this is called in diagnostics.
    pub fn kind_str(&self) -> &'static str {
        self.kind.kind_str()
    }

    pub fn as_struct(&self) -> Option<&Struct> {
        match &self.kind {
            TypeKind::Struct(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_union(&self) -> Option<&Union> {
        match &self.kind {
            TypeKind::Union(u) => Some(u),
            _ => None,
        }
    }

    pub fn as_enum(&self) -> Option<&Enum> {
        match &self.kind {
            TypeKind::Enum(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TypeKind {
    Alias(Alias),
    Scaled(Scaled),
    Enum(Enum),
    Union(Union),
    Struct(Struct),
}

impl TypeKind {
    pub fn kind_str(&self) -> &'static str {
        match self {
            TypeKind::Alias(_) => "alias",
            TypeKind::Scaled(_) => "scaled type",
            TypeKind::Enum(_) => "enum",
            TypeKind::Union(_) => "tagged union",
            TypeKind::Struct(_) => "struct",
        }
    }
}

/// `alias Name = Type;` (§3) — a name for another type, with no wire presence
/// of its own.
#[derive(Debug, Clone)]
pub struct Alias {
    pub target: WireType,
}

/// `scaled Name: RawType as f32 (scale: S, offset: O);` (§4).
#[derive(Debug, Clone)]
pub struct Scaled {
    pub raw_bits: u32,
    pub signed: bool,
    pub physical: FloatType,
    pub scale: f64,
    /// Defaults to `0.0` when the declaration omits it (§4).
    pub offset: f64,
}

impl Scaled {
    /// The raw integer's inclusive range, for the encode-side range check (§4).
    pub fn raw_range(&self) -> (i128, u128) {
        int_range(self.raw_bits, self.signed)
    }
}

/// The inclusive range a `uN`/`iN` value may take, as `(min, max)` (§2).
/// Encoding a value outside it is a hard error, never a truncation.
///
/// `min` is always `<= 0` and `max` always `>= 0`, which is what lets one
/// signature cover every width up to 128 without either bound overflowing —
/// `u128`'s maximum does not fit an `i128`, nor `i128`'s minimum a `u128`.
pub fn int_range(bits: u32, signed: bool) -> (i128, u128) {
    match (signed, bits) {
        (_, 0) => (0, 0),
        (true, 128) => (i128::MIN, i128::MAX as u128),
        (true, n) => (-(1i128 << (n - 1)), (1u128 << (n - 1)) - 1),
        (false, 128) => (0, u128::MAX),
        (false, n) => (0, (1u128 << n) - 1),
    }
}

/// `enum Name: uN { ... }` (§5).
#[derive(Debug, Clone)]
pub struct Enum {
    pub backing_bits: u32,
    /// In declaration order, with implicit numbering already resolved.
    pub variants: Vec<EnumVariant>,
    /// `else Unknown` — present iff the enum is open, in which case decoding
    /// never fails (§5).
    pub else_arm: Option<ElseVariant>,
}

impl Enum {
    pub fn is_open(&self) -> bool {
        self.else_arm.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub docs: Docs,
    pub span: Span,
    pub name: String,
    /// The wire value, explicit or assigned by the implicit counter (§5).
    pub value: u128,
}

/// The synthesized fallback variant of an open enum or tagged union (§5, §7).
#[derive(Debug, Clone)]
pub struct ElseVariant {
    pub docs: Docs,
    pub span: Span,
    pub name: String,
    /// Width of the `raw` field it carries: the enum's backing width, or a
    /// union's payload width.
    pub raw_bits: u32,
    /// Width of the `id` field it carries — `Some` only for a union (§7).
    pub id_bits: Option<u32>,
}

/// `enum Name(tag: uT): uN { ... }` (§7).
#[derive(Debug, Clone)]
pub struct Union {
    /// Name of the discriminant field, e.g. `id` in `enum Command(id: u16)`.
    pub tag_name: String,
    /// Width of the discriminant, which occupies the container's low bits.
    pub tag_bits: u32,
    /// Total container width.
    pub container_bits: u32,
    /// `container_bits - tag_bits`: the budget every variant's payload fits in.
    pub payload_bits: u32,
    pub variants: Vec<UnionVariant>,
    pub else_arm: Option<ElseVariant>,
}

impl Union {
    pub fn is_open(&self) -> bool {
        self.else_arm.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct UnionVariant {
    pub docs: Docs,
    pub span: Span,
    pub name: String,
    /// Always explicit in the schema (§7).
    pub id: u128,
    /// Payload fields, packed from the first bit above the tag. Offsets are
    /// relative to the start of the payload region, not the container.
    pub fields: Vec<Field>,
    /// Bits this variant's fields actually use; the rest of the payload region
    /// is implicit padding (§6.2, §7).
    pub used_bits: u32,
}

/// `struct Name[: uN] { ... }` (§6).
#[derive(Debug, Clone)]
pub struct Struct {
    /// The `: uN` the author wrote, `None` for a variable-length struct (§6.3).
    /// When `Some`, it equals `layout.fixed_bits` — the exact-fit rule.
    pub declared_bits: Option<u32>,
    pub fields: Vec<Field>,
}

/// One field of a struct or tagged-union variant, placed in its container.
#[derive(Debug, Clone)]
pub struct Field {
    pub docs: Docs,
    pub span: Span,
    pub role: FieldRole,
    pub ty: WireType,
    /// Bit offset within the container: fields are packed in declaration order
    /// from the front of the container with no gaps, and this is not
    /// configurable (§6). Which end of the container "the front" is depends on
    /// its byte order, which is applied when the bits meet bytes (§8), not
    /// here.
    pub offset_bits: u32,
    pub layout: Layout,
}

impl Field {
    /// `None` for `padding`, which is anonymous and may repeat (§6.2).
    pub fn name(&self) -> Option<&str> {
        match &self.role {
            FieldRole::Value { name } | FieldRole::Reserved { name } => Some(name),
            FieldRole::Padding { .. } => None,
        }
    }

    /// Whether the field is part of the type a caller sees (§6.2).
    pub fn is_visible(&self) -> bool {
        !matches!(self.role, FieldRole::Padding { .. })
    }
}

/// How a field is treated on encode and decode (§6.2).
#[derive(Debug, Clone)]
pub enum FieldRole {
    /// A normal, caller-visible field.
    Value { name: String },
    /// `padding: uN` — written as zero, and on decode either ignored or, for
    /// the `= 0` form, required to be zero.
    Padding { check_zero: bool },
    /// `reserved name: uN` — captured on decode, written back unchanged on
    /// encode, exposed read-only.
    Reserved { name: String },
}

// ---------------------------------------------------------------------------
// Constants (§3.1)
// ---------------------------------------------------------------------------

/// `const Name: uN|iN = <literal>;` (§3.1) — a named integer value with no
/// wire representation: nothing resolves to it, and it carries no
/// [`TypeId`], so a backend reads [`Model::consts`] directly rather than
/// finding these mixed into [`Model::types`].
#[derive(Debug, Clone)]
pub struct Const {
    pub docs: Docs,
    pub span: Span,
    pub name: String,
    pub bits: u32,
    pub signed: bool,
    /// The literal's absolute value; `negative` says which side of zero.
    /// Kept apart because the widest legal magnitude (2^128 - 1, a `u128`
    /// constant) does not fit in `i128`.
    pub magnitude: u128,
    pub negative: bool,
}

impl Const {
    /// The constant as a signed value. Only meaningful when `signed`; a
    /// checked model's signed constant always fits (§2, §3.1 both enforce
    /// the same range check an enum value gets), so this never truncates.
    pub fn as_i128(&self) -> i128 {
        if self.negative {
            if self.magnitude == 1u128 << 127 { i128::MIN } else { -(self.magnitude as i128) }
        } else {
            self.magnitude as i128
        }
    }
}

// ---------------------------------------------------------------------------
// GATT metadata (§10)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Service {
    pub docs: Docs,
    pub span: Span,
    pub name: String,
    pub uuid: String,
    pub characteristics: Vec<Characteristic>,
}

#[derive(Debug, Clone)]
pub struct Characteristic {
    pub docs: Docs,
    pub span: Span,
    pub name: String,
    pub uuid: String,
    pub properties: Vec<Property>,
    /// The bound value type, which is a root container (§8).
    pub ty: TypeId,
    /// That type's layout, copied here because it is what the transport sees.
    pub layout: Layout,
    /// Byte order this characteristic's value is encoded in (§8).
    pub endian: Endianness,
}

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// A fully checked schema.
#[derive(Debug, Clone)]
pub struct Model {
    /// The file-level default byte order (§8).
    pub endian: Endianness,
    /// Every declared type, in source order.
    pub types: Vec<TypeDef>,
    pub services: Vec<Service>,
    /// Every `const` declaration, in source order (§3.1).
    pub consts: Vec<Const>,
}

impl Model {
    pub fn get(&self, id: TypeId) -> &TypeDef {
        &self.types[id.0]
    }

    pub fn find(&self, name: &str) -> Option<&TypeDef> {
        self.types.iter().find(|t| t.name == name)
    }

    /// The size of any resolved type. Total: the checker has already rejected
    /// the cases that could overflow.
    pub fn layout_of(&self, ty: &WireType) -> Layout {
        match ty {
            WireType::UInt(n) | WireType::Int(n) => Layout::fixed(*n),
            WireType::Bool => Layout::fixed(1),
            WireType::Named(id) => self.get(*id).layout,
            WireType::Array { elem, count } => {
                let elem_bits = u128::from(self.layout_of(elem).fixed_bits);
                Layout::fixed((elem_bits * u128::from(*count)).min(u128::from(u32::MAX)) as u32)
            }
            WireType::VarArray { elem, max } => Layout {
                fixed_bits: 0,
                tail: Some(Tail { elem_bits: self.layout_of(elem).fixed_bits, max_elems: *max }),
            },
            WireType::Str { max } => {
                Layout { fixed_bits: 0, tail: Some(Tail { elem_bits: 8, max_elems: *max }) }
            }
        }
    }

    /// Follows `alias` indirection to the type that actually defines the
    /// layout. An alias of a variable-length type (`alias N = string(max: 8)`)
    /// defines its own, so it is returned unchanged.
    pub fn underlying(&self, id: TypeId) -> TypeId {
        let mut id = id;
        // Alias chains are acyclic: §9 bans forward references, so each step
        // strictly decreases the id.
        while let TypeKind::Alias(alias) = &self.get(id).kind {
            match alias.target {
                WireType::Named(next) => id = next,
                _ => break,
            }
        }
        id
    }

    /// Types bound to at least one characteristic — the ones a backend emits
    /// encode/decode entry points for (§12).
    pub fn roots(&self) -> impl Iterator<Item = &TypeDef> {
        self.types.iter().filter(|t| t.root)
    }

    pub fn characteristics(&self) -> impl Iterator<Item = (&Service, &Characteristic)> {
        self.services.iter().flat_map(|s| s.characteristics.iter().map(move |c| (s, c)))
    }
}
