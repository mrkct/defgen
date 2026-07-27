//! Abstract syntax tree.
//!
//! The AST is a faithful, span-carrying record of what the schema author wrote:
//! the parser resolves closed syntactic sets (attribute names, GATT properties,
//! integer widths) into typed values, but performs no layout, no name
//! resolution, and no width arithmetic. Everything in SPEC.md §11 that needs to
//! look at more than one node — exact-fit widths, duplicate ids, undeclared or
//! recursive type references, `#[endian]` on a nested-only type — is left to a
//! later semantic pass over this tree.

use crate::span::{Span, Spanned};

/// Widest primitive integer a field may be declared with (§2). This is a limit
/// on *values*: a field is decoded into a native integer, and 128 bits is the
/// widest any backend has one for.
pub const MAX_INT_BITS: u32 = 128;

/// Widest bit count a container may declare, and the widest run of `padding`
/// (§6, §6.2). Neither is a value — a container width is the size of a buffer
/// and padding is a gap — so the primitive limit does not apply to them. 4096
/// bits is 512 bytes: the largest value ATT can carry, negotiated MTU and all.
pub const MAX_CONTAINER_BITS: u32 = 4096;

/// An identifier as written, with its span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

impl Ident {
    pub fn new(name: impl Into<String>, span: Span) -> Self {
        Ident { name: name.into(), span }
    }
}

/// One `///` line. Kept line-by-line so backends can re-wrap or re-prefix them
/// for their target language's doc-comment syntax (SPEC.md §12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    pub text: String,
    pub span: Span,
}

pub type Docs = Vec<Doc>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endianness {
    Little,
    Big,
}

impl Endianness {
    pub fn as_str(self) -> &'static str {
        match self {
            Endianness::Little => "little",
            Endianness::Big => "big",
        }
    }
}

// ---------------------------------------------------------------------------
// File
// ---------------------------------------------------------------------------

/// A whole `.defs` file: the optional header pragma plus the declarations
/// below the `---` (§1.1).
#[derive(Debug, Clone)]
pub struct Schema {
    /// The `endian` pragma, if the file declared a header. `None` means the
    /// file had no header at all, and the default byte order (little) applies.
    pub endian: Option<Spanned<Endianness>>,
    /// Span of the `---` separator, if the file had one.
    pub separator: Option<Span>,
    pub decls: Vec<Decl>,
}

impl Schema {
    /// Looks a declaration up by the name it declares.
    pub fn decl(&self, name: &str) -> Option<&Decl> {
        self.decls.iter().find(|d| d.name().name == name)
    }

    pub fn structs(&self) -> impl Iterator<Item = &StructDecl> {
        self.decls.iter().filter_map(|d| match d {
            Decl::Struct(s) => Some(s),
            _ => None,
        })
    }

    pub fn services(&self) -> impl Iterator<Item = &ServiceDecl> {
        self.decls.iter().filter_map(|d| match d {
            Decl::Service(s) => Some(s),
            _ => None,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Decl {
    Alias(AliasDecl),
    Scaled(ScaledDecl),
    Enum(EnumDecl),
    Union(UnionDecl),
    Struct(StructDecl),
    Const(ConstDecl),
    Service(ServiceDecl),
}

impl Decl {
    pub fn name(&self) -> &Ident {
        match self {
            Decl::Alias(d) => &d.name,
            Decl::Scaled(d) => &d.name,
            Decl::Enum(d) => &d.name,
            Decl::Union(d) => &d.name,
            Decl::Struct(d) => &d.name,
            Decl::Const(d) => &d.name,
            Decl::Service(d) => &d.name,
        }
    }

    pub fn span(&self) -> Span {
        match self {
            Decl::Alias(d) => d.span,
            Decl::Scaled(d) => d.span,
            Decl::Enum(d) => d.span,
            Decl::Union(d) => d.span,
            Decl::Struct(d) => d.span,
            Decl::Const(d) => d.span,
            Decl::Service(d) => d.span,
        }
    }

    pub fn docs(&self) -> &Docs {
        match self {
            Decl::Alias(d) => &d.docs,
            Decl::Scaled(d) => &d.docs,
            Decl::Enum(d) => &d.docs,
            Decl::Union(d) => &d.docs,
            Decl::Struct(d) => &d.docs,
            Decl::Const(d) => &d.docs,
            Decl::Service(d) => &d.docs,
        }
    }

    /// What this declaration is called in diagnostics.
    pub fn kind_str(&self) -> &'static str {
        match self {
            Decl::Alias(_) => "alias",
            Decl::Scaled(_) => "scaled type",
            Decl::Enum(_) => "enum",
            Decl::Union(_) => "tagged union",
            Decl::Struct(_) => "struct",
            Decl::Const(_) => "constant",
            Decl::Service(_) => "service",
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A type usable as a field, array element, alias target or `scaled` raw type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarType {
    pub kind: ScalarKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScalarKind {
    /// `uN`, `1 <= N <= 128`.
    UInt(u32),
    /// `iN`, `2 <= N <= 128`.
    Int(u32),
    /// `bool` — sugar for `u1` (§2).
    Bool,
    /// `f32`/`f64` — an IEEE-754 value carried directly on the wire, byte
    /// order following the same rule as any other multi-byte scalar (§2, §8).
    Float(FloatType),
    /// A declared `alias`/`scaled`/`enum`/tagged-union/`struct` name. Resolved
    /// by the semantic pass, not here.
    Named(Ident),
}

impl ScalarType {
    /// On-wire width, when it is knowable without resolving names.
    pub fn intrinsic_bits(&self) -> Option<u32> {
        match &self.kind {
            ScalarKind::UInt(n) | ScalarKind::Int(n) => Some(*n),
            ScalarKind::Bool => Some(1),
            ScalarKind::Float(f) => Some(f.bits()),
            ScalarKind::Named(_) => None,
        }
    }

    pub fn named(&self) -> Option<&Ident> {
        match &self.kind {
            ScalarKind::Named(i) => Some(i),
            _ => None,
        }
    }
}

/// The full type of a field: a scalar, an array of scalars, or a
/// variable-length string (§6.1, §6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldType {
    pub kind: FieldTypeKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldTypeKind {
    Scalar(ScalarType),
    /// `Type[N]` — exactly `N` elements, no length prefix (§6.1).
    FixedArray {
        elem: ScalarType,
        count: Spanned<u64>,
    },
    /// `Type[max: N]` — 0..=N elements, length derived from the buffer (§6.3).
    VarArray {
        elem: ScalarType,
        max: Spanned<u64>,
    },
    /// `string(max: N)` — UTF-8, at most `N` bytes (§6.3).
    Str {
        max: Spanned<u64>,
    },
}

impl FieldType {
    /// Whether this type's on-wire size depends on the received buffer. Note a
    /// `Scalar(Named(..))` may *also* be variable-length once resolved (a
    /// variable-length struct or a `string` alias); only the semantic pass can
    /// tell (§6.3).
    pub fn is_intrinsically_variable(&self) -> bool {
        matches!(self.kind, FieldTypeKind::VarArray { .. } | FieldTypeKind::Str { .. })
    }
}

// ---------------------------------------------------------------------------
// Attributes (§1.2)
// ---------------------------------------------------------------------------

/// A resolved `#[...]` attribute. v1 recognizes only `endian(little|big)`; an
/// unknown name is rejected at parse time, so there is no "unknown" variant.
#[derive(Debug, Clone)]
pub struct Attr {
    pub kind: AttrKind,
    /// Span of the whole `#[...]`.
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum AttrKind {
    Endian(Spanned<Endianness>),
}

impl Attr {
    pub fn endian(&self) -> Option<Spanned<Endianness>> {
        match &self.kind {
            AttrKind::Endian(e) => Some(*e),
        }
    }
}

// ---------------------------------------------------------------------------
// Declarations
// ---------------------------------------------------------------------------

/// `alias Name = Type;` (§3)
#[derive(Debug, Clone)]
pub struct AliasDecl {
    pub docs: Docs,
    pub name: Ident,
    pub target: FieldType,
    pub span: Span,
}

/// `scaled Name: RawType as f32 (scale: S, offset: O);` (§4)
#[derive(Debug, Clone)]
pub struct ScaledDecl {
    pub docs: Docs,
    pub name: Ident,
    /// Always `UInt`/`Int`: checked at parse time (§11).
    pub raw: ScalarType,
    pub physical: Spanned<FloatType>,
    pub scale: Spanned<f64>,
    pub offset: Option<Spanned<f64>>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatType {
    F32,
    F64,
}

impl FloatType {
    pub fn as_str(self) -> &'static str {
        match self {
            FloatType::F32 => "f32",
            FloatType::F64 => "f64",
        }
    }

    /// On-wire/in-memory width: 32 for `f32`, 64 for `f64`.
    pub fn bits(self) -> u32 {
        match self {
            FloatType::F32 => 32,
            FloatType::F64 => 64,
        }
    }
}

/// `enum Name: uN { ... }` (§5)
#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub docs: Docs,
    pub name: Ident,
    /// Backing width in bits; the type is always unsigned.
    pub backing_bits: Spanned<u32>,
    pub variants: Vec<EnumVariant>,
    /// `else Unknown` — present iff the enum is open (§5).
    pub else_arm: Option<ElseArm>,
    pub span: Span,
}

impl EnumDecl {
    pub fn is_open(&self) -> bool {
        self.else_arm.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub docs: Docs,
    pub name: Ident,
    /// `None` means "one past the previous variant" (§5); assigning the actual
    /// numbers is the semantic pass's job.
    pub value: Option<Spanned<u128>>,
    pub span: Span,
}

/// `else Unknown` — the fallback arm of an open enum or union.
#[derive(Debug, Clone)]
pub struct ElseArm {
    pub docs: Docs,
    pub name: Ident,
    pub span: Span,
}

/// `enum Name(tag: uT): uN { Variant(id) { ... } ... }` (§7)
#[derive(Debug, Clone)]
pub struct UnionDecl {
    pub docs: Docs,
    pub attrs: Vec<Attr>,
    pub name: Ident,
    /// Name of the discriminant field, e.g. `id` in `enum Command(id: u16)`.
    pub tag_name: Ident,
    pub tag_bits: Spanned<u32>,
    pub container_bits: Spanned<u32>,
    pub variants: Vec<UnionVariant>,
    pub else_arm: Option<ElseArm>,
    pub span: Span,
}

impl UnionDecl {
    pub fn is_open(&self) -> bool {
        self.else_arm.is_some()
    }

    /// Bits available to a variant's payload (§7).
    pub fn payload_bits(&self) -> Option<u32> {
        self.container_bits.value.checked_sub(self.tag_bits.value)
    }

    /// The `#[endian(...)]` override, if any (§8).
    pub fn endian(&self) -> Option<Spanned<Endianness>> {
        self.attrs.iter().find_map(|a| a.endian())
    }
}

#[derive(Debug, Clone)]
pub struct UnionVariant {
    pub docs: Docs,
    pub name: Ident,
    /// Always explicit for unions — never auto-numbered (§7).
    pub id: Spanned<u128>,
    pub fields: Vec<Field>,
    /// `false` for a payload-less variant written without braces.
    pub has_payload_block: bool,
    pub span: Span,
}

/// `struct Name[: uN] { fields }` (§6)
#[derive(Debug, Clone)]
pub struct StructDecl {
    pub docs: Docs,
    pub attrs: Vec<Attr>,
    pub name: Ident,
    /// `Some` for a fixed-width struct, `None` for a variable-length one
    /// (§6.3). Which one is legal depends on the fields — a semantic check.
    pub width_bits: Option<Spanned<u32>>,
    pub fields: Vec<Field>,
    pub span: Span,
}

impl StructDecl {
    pub fn endian(&self) -> Option<Spanned<Endianness>> {
        self.attrs.iter().find_map(|a| a.endian())
    }

    /// Whether the struct declared itself variable-length by omitting `: uN`.
    pub fn declared_variable(&self) -> bool {
        self.width_bits.is_none()
    }
}

/// `const Name: uN|iN = <literal>;` — a named integer constant with no wire
/// presence of its own: a plain value threaded straight into generated code
/// (protocol limits, retry counts, and the like), never read or written by
/// any codec.
#[derive(Debug, Clone)]
pub struct ConstDecl {
    pub docs: Docs,
    pub name: Ident,
    /// Always `UInt`/`Int`: checked at parse time.
    pub ty: ScalarType,
    pub value: Spanned<ConstLit>,
    pub span: Span,
}

/// An integer literal that may be negative, kept as magnitude plus sign since
/// the widest legal magnitude (2^128 - 1, for a `u128` constant) does not fit
/// in `i128` (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConstLit {
    pub magnitude: u128,
    pub negative: bool,
}

/// One field of a struct or tagged-union variant.
#[derive(Debug, Clone)]
pub struct Field {
    pub docs: Docs,
    pub kind: FieldKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum FieldKind {
    /// `name: Type` — a normal, caller-visible field.
    Value { name: Ident, ty: FieldType },
    /// `padding: uN` / `padding: uN = 0` (§6.2). Not exposed to callers;
    /// `check_zero` is the `= 0` form, which fails decode on non-zero bits.
    Padding { keyword: Span, bits: Spanned<u32>, check_zero: bool },
    /// `reserved name: uN` (§6.2) — captured on decode, written back on
    /// encode, exposed read-only.
    Reserved { name: Ident, bits: Spanned<u32> },
}

impl Field {
    /// The field's name, for duplicate-name checking and codegen. `padding`
    /// has none: it is anonymous and may repeat within one container.
    pub fn name(&self) -> Option<&Ident> {
        match &self.kind {
            FieldKind::Value { name, .. } | FieldKind::Reserved { name, .. } => Some(name),
            FieldKind::Padding { .. } => None,
        }
    }

    /// Width in bits, when knowable without resolving named types.
    pub fn intrinsic_bits(&self) -> Option<u32> {
        match &self.kind {
            FieldKind::Padding { bits, .. } | FieldKind::Reserved { bits, .. } => Some(bits.value),
            FieldKind::Value { ty, .. } => match &ty.kind {
                FieldTypeKind::Scalar(s) => s.intrinsic_bits(),
                FieldTypeKind::FixedArray { elem, count } => elem
                    .intrinsic_bits()
                    .and_then(|b| u32::try_from(count.value).ok().and_then(|c| b.checked_mul(c))),
                FieldTypeKind::VarArray { .. } | FieldTypeKind::Str { .. } => None,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// GATT metadata (§10)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ServiceDecl {
    pub docs: Docs,
    pub name: Ident,
    pub uuid: Spanned<String>,
    pub characteristics: Vec<Characteristic>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Characteristic {
    pub docs: Docs,
    pub name: Ident,
    pub uuid: Spanned<String>,
    pub properties: Vec<Spanned<Property>>,
    /// The declared type bound to this characteristic; resolved later.
    pub ty: Ident,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Property {
    Read,
    Write,
    WriteWithoutResponse,
    Notify,
    Indicate,
}

impl Property {
    pub const ALL: [Property; 5] = [
        Property::Read,
        Property::Write,
        Property::WriteWithoutResponse,
        Property::Notify,
        Property::Indicate,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Property::Read => "read",
            Property::Write => "write",
            Property::WriteWithoutResponse => "write_without_response",
            Property::Notify => "notify",
            Property::Indicate => "indicate",
        }
    }

    pub fn from_name(s: &str) -> Option<Property> {
        Property::ALL.into_iter().find(|p| p.as_str() == s)
    }
}
