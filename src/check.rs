//! The semantic pass: everything in SPEC.md §11 that needs to look at more
//! than one node.
//!
//! The parser guarantees each declaration is well-formed on its own. This pass
//! establishes the rules that span declarations, and turns the AST into a
//! [`Model`] — see `model.rs` for what that buys downstream. Broadly, it
//! checks that
//!
//! * every name resolves, to something declared *earlier* in the file (§9), so
//!   layouts can be computed in one forward pass and recursion is impossible;
//! * every container's bits add up exactly (§6), and every variable-length
//!   field is the last one, alone, and byte-aligned (§6.3);
//! * enum values and union ids are unique and fit their backing width (§5, §7);
//! * `#[endian(...)]` is only on types that are actually encoded as roots (§8);
//! * GATT bindings name bindable, byte-sized types with plausible UUIDs (§10).
//!
//! Like the parser it reports as much as it can per run: a declaration that
//! fails to resolve is *poisoned* rather than fatal, and only the checks that
//! would cascade off the bad layout are skipped.

use std::collections::{HashMap, HashSet};

use crate::ast::{self, Decl, Ident, MAX_CONTAINER_BITS, MAX_INT_BITS, Schema};
use crate::diag::{Diagnostic, Severity, suggest};
use crate::model::*;
use crate::span::{Span, Spanned};

/// Outcome of checking one schema. `model` is `Some` exactly when no
/// error-level diagnostic was produced, so a backend never sees a schema whose
/// layout is in question. Warnings (the MTU diagnostic, §10) do not suppress it.
#[derive(Debug)]
pub struct Checked {
    pub model: Option<Model>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Checked {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }
}

pub fn check(schema: &Schema) -> Checked {
    let mut checker = Checker::new(schema);
    checker.run();
    let diagnostics = checker.diags;
    let has_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);
    let model = (!has_errors).then_some(Model {
        endian: schema.endian.map_or(ast::Endianness::Little, |e| e.value),
        types: checker.types,
        services: checker.services,
        consts: checker.consts,
    });
    Checked { model, diagnostics }
}

// ---------------------------------------------------------------------------
// The checker
// ---------------------------------------------------------------------------

struct Checker<'a> {
    schema: &'a Schema,
    diags: Vec<Diagnostic>,
    types: Vec<TypeDef>,
    services: Vec<Service>,
    consts: Vec<Const>,
    /// Declaration index of the first declaration of each name. Resolution
    /// compares it against [`Checker::current`] to tell "declared earlier"
    /// from "declared later" (§9) from "not declared at all".
    first_decl: HashMap<&'a str, usize>,
    /// The [`TypeId`] each declaration index produced; `None` for a `service`,
    /// and for a type whose own checking bailed out.
    decl_type: Vec<Option<TypeId>>,
    /// Every declared name, for `did you mean` hints.
    all_names: Vec<&'a str>,
    /// Types whose layout could not be worked out. Anything referring to one
    /// skips its own width checks rather than reporting a bogus bit count.
    poisoned: HashSet<TypeId>,
    /// Set while checking a declaration that referred to something broken.
    tainted: bool,
    /// Index of the declaration being checked.
    current: usize,
    /// `#[endian(...)]` spans, checked against root-ness once every use of
    /// every type is known (§8).
    endian_attrs: Vec<(TypeId, Span)>,
    /// Characteristic and service names/UUIDs already seen, for §11's
    /// duplicate checks.
    char_names: HashMap<String, Span>,
    service_uuids: HashMap<String, Span>,
}

impl<'a> Checker<'a> {
    fn new(schema: &'a Schema) -> Self {
        Checker {
            schema,
            diags: Vec::new(),
            types: Vec::new(),
            services: Vec::new(),
            consts: Vec::new(),
            first_decl: HashMap::new(),
            decl_type: vec![None; schema.decls.len()],
            all_names: Vec::new(),
            poisoned: HashSet::new(),
            tainted: false,
            current: 0,
            endian_attrs: Vec::new(),
            char_names: HashMap::new(),
            service_uuids: HashMap::new(),
        }
    }

    fn run(&mut self) {
        self.collect_names();
        for (index, decl) in self.schema.decls.iter().enumerate() {
            self.current = index;
            self.tainted = false;
            match decl {
                Decl::Service(s) => {
                    let service = self.check_service(s);
                    self.services.push(service);
                }
                Decl::Const(c) => {
                    let cst = self.check_const(c);
                    self.consts.push(cst);
                }
                _ => self.check_type_decl(index, decl),
            }
        }
        self.check_endian_placement();
    }

    fn error(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    // -----------------------------------------------------------------------
    // Declaration names (§11)
    // -----------------------------------------------------------------------

    fn collect_names(&mut self) {
        for (index, decl) in self.schema.decls.iter().enumerate() {
            let name = decl.name();
            self.all_names.push(&name.name);

            if let Some(&first) = self.first_decl.get(name.name.as_str()) {
                let previous = &self.schema.decls[first];
                let d = Diagnostic::error(format!("`{}` is declared more than once", name.name))
                    .primary(name.span, format!("redeclared as a {} here", decl.kind_str()))
                    .secondary(previous.name().span, format!("first declared as a {} here", previous.kind_str()))
                    .note("every declaration in a file shares one namespace, and generated code needs one name per type (§11)")
                    .help("rename one of them");
                self.error(d);
                continue;
            }
            self.first_decl.insert(&name.name, index);

            if let Some(what) = primitive_like(&name.name) {
                let d = Diagnostic::error(format!("`{}` is a built-in type name", name.name))
                    .primary(name.span, format!("cannot declare a {} called `{}`", decl.kind_str(), name.name))
                    .note(format!("{what} is spelled the same way wherever it is used, so a declaration by that name could never be referred to (§2)"))
                    .help("pick a domain name, e.g. `Volume` or `SampleCount`");
                self.error(d);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Type declarations
    // -----------------------------------------------------------------------

    fn check_type_decl(&mut self, index: usize, decl: &'a Decl) {
        let (kind, layout) = match decl {
            Decl::Alias(d) => self.check_alias(d),
            Decl::Scaled(d) => self.check_scaled(d),
            Decl::Enum(d) => self.check_enum(d),
            Decl::Union(d) => self.check_union(d),
            Decl::Struct(d) => self.check_struct(d),
            Decl::Const(_) => unreachable!("constants are checked separately"),
            Decl::Service(_) => unreachable!("services are checked separately"),
        };

        let id = TypeId(self.types.len());
        let attr_endian = match decl {
            Decl::Struct(d) => d.endian(),
            Decl::Union(d) => d.endian(),
            _ => None,
        };
        if let Some(e) = attr_endian {
            self.endian_attrs.push((id, e.span));
        }

        self.types.push(TypeDef {
            id,
            name: decl.name().name.clone(),
            docs: decl.docs().clone(),
            span: decl.span(),
            layout,
            endian: attr_endian
                .map_or_else(|| self.schema.endian.map_or(ast::Endianness::Little, |e| e.value), |e| e.value),
            endian_explicit: attr_endian.is_some(),
            root: false,
            nested: false,
            kind,
        });
        if self.tainted {
            self.poisoned.insert(id);
        }
        // A redeclared name resolves to its first declaration; the duplicate
        // is still checked, just unreachable by name.
        if self.first_decl.get(decl.name().name.as_str()) == Some(&index) {
            self.decl_type[index] = Some(id);
        }
    }

    /// `alias Name = Type;` (§3). An alias has no wire form of its own — it
    /// borrows its target's layout entirely.
    fn check_alias(&mut self, d: &'a ast::AliasDecl) -> (TypeKind, Layout) {
        match self.resolve_field_type(&d.target) {
            Some((target, layout)) => (TypeKind::Alias(Alias { target }), layout),
            None => (TypeKind::Alias(Alias { target: WireType::UInt(8) }), Layout::fixed(8)),
        }
    }

    /// `scaled Name: RawType as f32 (scale: S);` (§4).
    fn check_scaled(&mut self, d: &'a ast::ScaledDecl) -> (TypeKind, Layout) {
        // The parser only accepts `uN`/`iN` here; this keeps the model honest
        // if that ever loosens.
        let (raw_bits, signed) = match d.raw.kind {
            ast::ScalarKind::UInt(n) => (n, false),
            ast::ScalarKind::Int(n) => (n, true),
            _ => {
                let e = Diagnostic::error("a `scaled` declaration's raw type must be `uN` or `iN`")
                    .primary(d.raw.span, "not an integer wire type")
                    .note("`physical = raw * scale + offset` is only defined for an integer raw value (§4)");
                self.error(e);
                self.tainted = true;
                (8, false)
            }
        };

        for (what, value) in [("scale", Some(d.scale)), ("offset", d.offset)]
            .into_iter()
            .filter_map(|(w, v)| v.map(|v| (w, v)))
        {
            if !value.value.is_finite() {
                let e = Diagnostic::error(format!("`{what}` must be a finite number"))
                    .primary(value.span, format!("`{}` is not a usable {what}", value.value))
                    .note("the conversion is evaluated at runtime on the device's numbers, so it has to be an ordinary value (§4)");
                self.error(e);
            }
        }

        let scaled = Scaled {
            raw_bits,
            signed,
            physical: d.physical.value,
            scale: d.scale.value,
            offset: d.offset.map_or(0.0, |o| o.value),
        };
        (TypeKind::Scaled(scaled), Layout::fixed(raw_bits))
    }

    /// `enum Name: uN { ... }` (§5).
    fn check_enum(&mut self, d: &'a ast::EnumDecl) -> (TypeKind, Layout) {
        let bits = d.backing_bits.value;
        let limit = max_value(bits);

        if d.variants.is_empty() {
            let e = Diagnostic::error(format!("enum `{}` declares no variants", d.name.name))
                .primary(d.name.span, "an enum needs at least one named value")
                .note("an enum is a closed set of named values; an empty one can never decode anything (§5)");
            self.error(e);
        }

        let mut names: HashMap<&str, Span> = HashMap::new();
        let mut values: HashMap<u128, (&str, Span)> = HashMap::new();
        let mut variants = Vec::with_capacity(d.variants.len());
        let mut next: u128 = 0;

        for v in &d.variants {
            self.check_variant_name(&mut names, &v.name, "enum", &d.name);

            let (value, span, explicit) = match v.value {
                Some(value) => (value.value, value.span, true),
                None => (next, v.name.span, false),
            };
            next = value.saturating_add(1);

            if value > limit {
                let mut e = Diagnostic::error(format!(
                    "variant `{}` does not fit in the enum's `u{bits}` backing type",
                    v.name.name
                ))
                .primary(span, format!("{value} is outside 0..={limit}"))
                .note(format!("an enum's values are `u{bits}` wire values, so they must fit (§5, §11)"));
                e = if explicit {
                    e.help(format!(
                        "widen the backing type, e.g. `enum {}: u{}`",
                        d.name.name,
                        widen_for(value)
                    ))
                } else {
                    e.note("this value was assigned by the implicit counter, which continues from the last explicit value (§5)")
                        .help("give the variant an explicit value, or widen the backing type")
                };
                self.error(e);
                continue;
            }

            if let Some((other, other_span)) = values.get(&value) {
                let e = Diagnostic::error(format!("enum value {value} is used twice"))
                    .primary(span, format!("`{}` also has value {value}", v.name.name))
                    .secondary(*other_span, format!("`{other}` declared it first"))
                    .note("two names for one wire value make decoding ambiguous (§5, §11)")
                    .help(if explicit {
                        "give this variant a different value"
                    } else {
                        "this value came from the implicit counter; give the variant an explicit value"
                    });
                self.error(e);
                continue;
            }
            values.insert(value, (&v.name.name, span));
            variants.push(EnumVariant {
                docs: v.docs.clone(),
                span: v.span,
                name: v.name.name.clone(),
                value,
            });
        }

        let else_arm = d.else_arm.as_ref().map(|arm| {
            self.check_variant_name(&mut names, &arm.name, "enum", &d.name);
            ElseVariant {
                docs: arm.docs.clone(),
                span: arm.span,
                name: arm.name.name.clone(),
                raw_bits: bits,
                id_bits: None,
            }
        });

        (TypeKind::Enum(Enum { backing_bits: bits, variants, else_arm }), Layout::fixed(bits))
    }

    /// `enum Name(tag: uT): uN { ... }` (§7).
    fn check_union(&mut self, d: &'a ast::UnionDecl) -> (TypeKind, Layout) {
        let tag_bits = d.tag_bits.value;
        let container_bits = d.container_bits.value;

        let payload_bits = match container_bits.checked_sub(tag_bits) {
            Some(p) => p,
            None => {
                let e = Diagnostic::error(format!(
                    "tagged union `{}` has a discriminant wider than its container",
                    d.name.name
                ))
                .primary(d.tag_bits.span, format!("`u{tag_bits}` discriminant"))
                .secondary(d.container_bits.span, format!("in a `u{container_bits}` container"))
                .note(
                    "the discriminant occupies the container's low bits, and the payload takes the rest (§7)",
                )
                .help(format!("widen the container to at least `u{tag_bits}`"));
                self.error(e);
                self.tainted = true;
                0
            }
        };

        if d.variants.is_empty() {
            let e = Diagnostic::error(format!("tagged union `{}` declares no variants", d.name.name))
                .primary(d.name.span, "a tagged union needs at least one variant")
                .note("without a variant there is no id a payload could ever be interpreted for (§7)");
            self.error(e);
        }

        let mut names: HashMap<&str, Span> = HashMap::new();
        let mut ids: HashMap<u128, (&str, Span)> = HashMap::new();
        let id_limit = max_value(tag_bits);
        let mut variants = Vec::with_capacity(d.variants.len());

        for v in &d.variants {
            self.check_variant_name(&mut names, &v.name, "tagged union", &d.name);

            if v.id.value > id_limit {
                let e = Diagnostic::error(format!(
                    "variant id {:#x} does not fit in the `u{tag_bits}` discriminant",
                    v.id.value
                ))
                .primary(v.id.span, format!("outside 0..={id_limit}"))
                .note(format!("the discriminant is `u{tag_bits}`, so every id must fit in it (§7, §11)"))
                .help(format!(
                    "widen the discriminant, e.g. `{}(id: u{})`",
                    d.name.name,
                    widen_for(v.id.value)
                ));
                self.error(e);
            } else if let Some((other, other_span)) = ids.get(&v.id.value) {
                let e = Diagnostic::error(format!("variant id {:#x} is used twice", v.id.value))
                    .primary(v.id.span, format!("`{}` also has this id", v.name.name))
                    .secondary(*other_span, format!("`{other}` declared it first"))
                    .note("an id is a wire contract: two variants sharing one makes decoding ambiguous (§7, §11)");
                self.error(e);
            } else {
                ids.insert(v.id.value, (&v.name.name, v.id.span));
            }

            let owner = format!("variant `{}` of tagged union `{}`", v.name.name, d.name.name);
            let walk = self.walk_fields(&v.fields, &owner, VarFields::Rejected, tag_bits);

            if walk.ok && walk.bits > payload_bits {
                let e = Diagnostic::error(format!(
                    "variant `{}` needs {} bits, more than the {payload_bits}-bit payload",
                    v.name.name, walk.bits
                ))
                .primary(v.span, format!("payload is {} bits", walk.bits))
                .secondary(
                    d.container_bits.span,
                    format!("`u{container_bits}` container minus the `u{tag_bits}` discriminant leaves {payload_bits} bits"),
                )
                .note("a variant's fields must fit the payload region; unused trailing bits are implicit padding, but there is no room to overflow into (§7, §11)")
                .help("shrink the variant's fields, or widen the container");
                self.error(e);
            }

            variants.push(UnionVariant {
                docs: v.docs.clone(),
                span: v.span,
                name: v.name.name.clone(),
                id: v.id.value,
                fields: walk.fields,
                used_bits: walk.bits,
            });
        }

        let else_arm = d.else_arm.as_ref().map(|arm| {
            self.check_variant_name(&mut names, &arm.name, "tagged union", &d.name);
            ElseVariant {
                docs: arm.docs.clone(),
                span: arm.span,
                name: arm.name.name.clone(),
                raw_bits: payload_bits,
                id_bits: Some(tag_bits),
            }
        });

        // The fallback variant carries `raw: u(N - T)`, so the payload region
        // has to be a width a value can actually have (§7).
        if let Some(arm) = d.else_arm.as_ref()
            && !self.tainted
        {
            if payload_bits == 0 {
                let e = Diagnostic::error(format!(
                    "the `else` arm of `{}` has no payload bits to capture",
                    d.name.name
                ))
                .primary(arm.span, "needs at least one payload bit")
                .secondary(
                    d.container_bits.span,
                    format!("the `u{tag_bits}` discriminant fills the whole `u{container_bits}` container"),
                )
                .note("an open union's fallback variant carries `{ id: uT, raw: u(N - T) }`, and `raw` cannot be zero bits wide (§7)")
                .help("widen the container, or drop the `else` arm to make the union closed");
                self.error(e);
            } else if payload_bits > MAX_INT_BITS {
                let e = Diagnostic::error(format!(
                    "the `else` arm of `{}` would have to capture {payload_bits} bits in one value",
                    d.name.name
                ))
                .primary(arm.span, format!("`raw` would be `u{payload_bits}`"))
                .secondary(
                    d.container_bits.span,
                    format!("`u{container_bits}` container minus the `u{tag_bits}` discriminant"),
                )
                .note(format!("the fallback variant carries `raw: u(N - T)`, and that is an ordinary value: it has to fit a native integer, so at most {MAX_INT_BITS} bits (§2, §7)"))
                .help("narrow the container, or drop the `else` arm to make the union closed");
                self.error(e);
            }
        }

        let union = Union {
            tag_name: d.tag_name.name.clone(),
            tag_bits,
            container_bits,
            payload_bits,
            variants,
            else_arm,
        };
        (TypeKind::Union(union), Layout::fixed(container_bits))
    }

    /// `struct Name[: uN] { ... }` (§6, §6.3).
    fn check_struct(&mut self, d: &'a ast::StructDecl) -> (TypeKind, Layout) {
        let owner = format!("struct `{}`", d.name.name);
        let walk = self.walk_fields(&d.fields, &owner, VarFields::Allowed, 0);
        let layout = Layout { fixed_bits: walk.bits, tail: walk.tail };

        match (&d.width_bits, walk.tail) {
            // Fixed-width: the bits must add up exactly (§6).
            (Some(width), None) => {
                if walk.ok && !self.tainted && width.value != walk.bits {
                    self.report_exact_fit(d, *width, walk.bits);
                }
            }
            // A declared width plus a variable-length field is contradictory.
            (Some(width), Some(_)) => {
                let e = Diagnostic::error(format!(
                    "struct `{}` declares a width but ends in a variable-length field",
                    d.name.name
                ))
                .primary(width.span, format!("declared `u{}` here", width.value))
                .secondary(walk.var_span.expect("a tail comes from a field"), "this field's size depends on the buffer")
                .note("a struct either declares an exact bit width and is fully fixed, or omits it and ends in one variable-length field — nothing in between (§6, §6.3)")
                .help(format!("remove the `: u{}`", width.value));
                self.error(e);
            }
            // No declared width and nothing variable: the width is missing.
            (None, None) => {
                if walk.ok && !self.tainted {
                    let bits = walk.bits;
                    let mut e = Diagnostic::error(format!(
                        "struct `{}` has no declared width and does not end in a variable-length field",
                        d.name.name
                    ))
                    .primary(d.name.span, "expected `: uN` after the name")
                    .note("omitting `: uN` is how a struct opts into one trailing variable-length field; a fully fixed struct must state its container width (§6, §6.3)");
                    e = if bits > 0 && bits <= MAX_CONTAINER_BITS {
                        e.help(format!(
                            "write `struct {}: u{bits} {{ ... }}` — its fields add up to {bits} bits",
                            d.name.name
                        ))
                    } else {
                        e.help("add the container width, or make the last field a `string(max: N)` / `Type[max: N]`")
                    };
                    self.error(e);
                }
            }
            (None, Some(_)) => {}
        }

        (
            TypeKind::Struct(Struct { declared_bits: d.width_bits.map(|w| w.value), fields: walk.fields }),
            layout,
        )
    }

    fn report_exact_fit(&mut self, d: &ast::StructDecl, width: Spanned<u32>, actual: u32) {
        let declared = width.value;
        let (verb, delta) = if actual < declared {
            ("does not fill", declared - actual)
        } else {
            ("overflows", actual - declared)
        };
        let mut e = Diagnostic::error(format!(
            "struct `{}` {verb} its `u{declared}` container",
            d.name.name
        ))
        .primary(width.span, format!("declares {declared} bits"))
        .secondary(
            d.fields.last().map_or(d.name.span, |f| f.span),
            format!("its fields add up to {actual} bits"),
        )
        .note("every container's declared bit width must be exactly accounted for by its fields; there is no implicit trailing padding (§0, §6)");
        e = if actual < declared {
            e.help(format!("add `padding: u{delta},` for the {delta} unaccounted bit{}", plural(delta)))
        } else if actual <= MAX_CONTAINER_BITS {
            e.help(format!("widen the container to `u{actual}`, or remove {delta} bit{}", plural(delta)))
        } else {
            e.help(format!(
                "remove {delta} bit{} — a container cannot be wider than `u{MAX_CONTAINER_BITS}`",
                plural(delta)
            ))
        };
        self.error(e);
    }

    /// `const Name: uN|iN = <literal>;` (§3.1). No `TypeId` is produced: a
    /// constant has no wire form, so nothing can resolve to it as a type.
    fn check_const(&mut self, d: &'a ast::ConstDecl) -> Const {
        let (bits, signed) = match d.ty.kind {
            ast::ScalarKind::UInt(n) => (n, false),
            ast::ScalarKind::Int(n) => (n, true),
            _ => unreachable!("the parser only accepts `uN`/`iN` here"),
        };

        let lit = d.value.value;
        let (min, max) = int_range(bits, signed);
        // `min` is always <= 0 (§2), so its magnitude is exactly the largest
        // legal negative value's absolute value.
        let in_range =
            if lit.negative { signed && lit.magnitude <= min.unsigned_abs() } else { lit.magnitude <= max };

        if !in_range {
            let ty_str = format!("{}{bits}", if signed { "i" } else { "u" });
            let value_str =
                if lit.negative { format!("-{}", lit.magnitude) } else { lit.magnitude.to_string() };
            let mut e = Diagnostic::error(format!(
                "constant `{}` does not fit in its declared type `{ty_str}`",
                d.name.name
            ))
            .primary(d.value.span, format!("{value_str} does not fit in {ty_str}"))
            .note(format!("a `{ty_str}` value must be in {min}..={max} (§2, §3.1)"));
            e = if !signed && lit.negative {
                e.help("`uN` has no sign; use an `iN` type for a negative constant")
            } else {
                e.help("widen the declared type so it can hold this exact value")
            };
            self.error(e);
        }

        Const {
            docs: d.docs.clone(),
            span: d.span,
            name: d.name.name.clone(),
            bits,
            signed,
            magnitude: lit.magnitude,
            negative: lit.negative,
        }
    }

    fn check_variant_name(
        &mut self,
        seen: &mut HashMap<&'a str, Span>,
        name: &'a Ident,
        what: &str,
        owner: &Ident,
    ) {
        if let Some(first) = seen.get(name.name.as_str()) {
            let e =
                Diagnostic::error(format!("duplicate variant `{}` in {what} `{}`", name.name, owner.name))
                    .primary(name.span, "declared twice")
                    .secondary(*first, "first declared here")
                    .note(
                        "variants become distinct cases in generated code, so their names must differ (§11)",
                    );
            self.error(e);
        } else {
            seen.insert(&name.name, name.span);
        }
    }

    // -----------------------------------------------------------------------
    // Fields and layout (§6, §6.2, §6.3)
    // -----------------------------------------------------------------------

    /// Places a container's fields, LSB-first in declaration order with no
    /// gaps (§6), enforcing the variable-length rules of §6.3 along the way.
    /// `base_offset` is where `fields` starts within its enclosing container
    /// (0 for a struct's own fields, the tag width for a tagged-union
    /// variant's payload), so the byte-crossing diagnostic can reason about
    /// absolute position.
    fn walk_fields(
        &mut self,
        fields: &'a [ast::Field],
        owner: &str,
        var: VarFields,
        base_offset: u32,
    ) -> Walk {
        let mut walk = Walk::default();
        let mut names: HashMap<&str, Span> = HashMap::new();
        let mut offset: u64 = 0;
        let mut trailing_reported = false;

        for field in fields {
            if let Some(name) = field.name()
                && let Some(first) = names.insert(&name.name, name.span)
            {
                let e = Diagnostic::error(format!("duplicate field `{}` in {owner}", name.name))
                    .primary(name.span, "declared twice")
                    .secondary(first, "first declared here")
                    .note("field names become member names in generated code, so they must be unique within a container (§11)");
                self.error(e);
            }

            let Some((ty, layout)) = self.resolve_field(field) else {
                walk.ok = false;
                continue;
            };

            if layout.is_variable() {
                match var {
                    VarFields::Rejected => {
                        let e = Diagnostic::error(format!("{owner} cannot contain a variable-length field"))
                            .primary(field.span, "this field's size depends on the buffer")
                            .note("tagged unions are fully fixed-width in v1: a variant's payload region has a compile-time size (§6.3, §7, §14)")
                            .help("model the variable-length value as its own characteristic instead");
                        self.error(e);
                        walk.ok = false;
                        continue;
                    }
                    VarFields::Allowed => {}
                }
                if let Some(first) = walk.var_span {
                    let e = Diagnostic::error(format!("{owner} has more than one variable-length field"))
                        .primary(field.span, "a second variable-length field")
                        .secondary(first, "the first one is here")
                        .note("only the last field's length can be derived from the buffer, so a container may contribute at most one (§6.3)")
                        .help("move one of them into its own characteristic");
                    self.error(e);
                    walk.ok = false;
                    continue;
                }
                if !offset.is_multiple_of(8) {
                    let e = Diagnostic::error(format!(
                        "the fixed fields before a variable-length field must fill whole bytes, but they occupy {offset} bits"
                    ))
                    .primary(field.span, format!("this field would start {} bit{} into a byte", offset % 8, plural((offset % 8) as u32)))
                    .note("the runtime length is computed as `buffer_length - fixed_prefix_bytes`, which only makes sense at a byte boundary (§6.3)")
                    .help(format!("add `padding: u{},` before it", 8 - offset % 8));
                    self.error(e);
                    walk.ok = false;
                }
                walk.var_span = Some(field.span);
                walk.tail = layout.tail;
            } else {
                if let Some(first) = walk.var_span
                    && !trailing_reported
                {
                    trailing_reported = true;
                    let e = Diagnostic::error(format!("the variable-length field of {owner} is not the last field"))
                        .primary(first, "this field's length comes from the end of the buffer")
                        .secondary(field.span, "so nothing may follow it")
                        .note("every offset before the variable-length field is compile-time known; anything after it would not be (§0, §6.3)")
                        .help("move the variable-length field to the end");
                    self.error(e);
                    walk.ok = false;
                }
                self.check_byte_crossing(field, u64::from(base_offset) + offset, layout.fixed_bits);
            }

            offset += u64::from(layout.fixed_bits);
            if offset > u64::from(u32::MAX) {
                let e = Diagnostic::error(format!("{owner} is too large"))
                    .primary(field.span, "the fields before and including this one exceed 2^32 bits")
                    .note("defgen tracks bit offsets in 32 bits; a BLE payload is orders of magnitude smaller than that");
                self.error(e);
                walk.ok = false;
                break;
            }

            walk.fields.push(Field {
                docs: field.docs.clone(),
                span: field.span,
                role: field_role(field),
                ty,
                offset_bits: (offset - u64::from(layout.fixed_bits)) as u32,
                layout,
            });
        }

        walk.bits = offset as u32;
        walk
    }

    /// Flags a named field that starts mid-byte and spans into the next byte
    /// — legal (defgen bit-packs LSB-first with no alignment requirement,
    /// §6), but often an unintentional miscount, so it gets a non-fatal
    /// diagnostic rather than being silently accepted. `padding` is exempt:
    /// it carries no value, so crossing a byte has no bug signal.
    fn check_byte_crossing(&mut self, field: &'a ast::Field, start: u64, width: u32) {
        let Some(name) = field.name() else { return };
        if width == 0 {
            return;
        }
        let end = start + u64::from(width) - 1;
        if start.is_multiple_of(8) || start / 8 == end / 8 {
            return;
        }
        let into = start % 8;
        let w = Diagnostic::warning(format!("field `{}` crosses a byte boundary", name.name))
            .primary(
                field.span,
                format!("starts {into} bit{} into byte {}, ends in byte {}", plural(into as u32), start / 8, end / 8),
            )
            .note("defgen bit-packs fields LSB-first with no alignment requirement, so this is legal — some real wire formats split fields exactly this way (e.g. BLE's own `Appearance` characteristic, a 10-bit/6-bit split of one 16-bit value) (§6)")
            .help("if this wasn't intentional, reorder the fields or insert explicit padding so it starts at a byte boundary");
        self.error(w);
    }

    /// The resolved type and size of one field, including `padding`/`reserved`
    /// which are always plain unsigned bit runs (§6.2).
    fn resolve_field(&mut self, field: &'a ast::Field) -> Option<(WireType, Layout)> {
        match &field.kind {
            ast::FieldKind::Value { ty, .. } => self.resolve_field_type(ty),
            ast::FieldKind::Padding { bits, .. } | ast::FieldKind::Reserved { bits, .. } => {
                Some((WireType::UInt(bits.value), Layout::fixed(bits.value)))
            }
        }
    }

    fn resolve_field_type(&mut self, ty: &'a ast::FieldType) -> Option<(WireType, Layout)> {
        match &ty.kind {
            ast::FieldTypeKind::Scalar(s) => {
                let wire = self.resolve_scalar(s)?;
                let layout = self.scalar_layout(&wire);
                Some((wire, layout))
            }
            ast::FieldTypeKind::FixedArray { elem, count } => {
                let wire = self.resolve_scalar(elem)?;
                let elem_layout = self.scalar_layout(&wire);
                if elem_layout.is_variable() {
                    self.reject_variable_element(elem, "an array element", "(§6.1)");
                    return None;
                }
                if count.value == 0 {
                    let e = Diagnostic::error("an array length must be positive")
                        .primary(count.span, "zero elements")
                        .note("a zero-length array occupies no bits and has nothing to encode (§6.1)")
                        .help("give the array a real length, or remove the field");
                    self.error(e);
                    return None;
                }
                let bits = u128::from(elem_layout.fixed_bits) * u128::from(count.value);
                if bits > u128::from(u32::MAX) {
                    let e = Diagnostic::error("array is too large")
                        .primary(ty.span, format!("{bits} bits"))
                        .note("defgen tracks bit offsets in 32 bits; a BLE payload is orders of magnitude smaller than that");
                    self.error(e);
                    return None;
                }
                Some((
                    WireType::Array { elem: Box::new(wire), count: count.value },
                    Layout::fixed(bits as u32),
                ))
            }
            ast::FieldTypeKind::VarArray { elem, max } => {
                let wire = self.resolve_scalar(elem)?;
                let elem_layout = self.scalar_layout(&wire);
                if elem_layout.is_variable() {
                    self.reject_variable_element(elem, "the element type of a `[max: N]` array", "(§6.3)");
                    return None;
                }
                let elem_bits = elem_layout.fixed_bits;
                if elem_bits == 0 || !elem_bits.is_multiple_of(8) {
                    let e = Diagnostic::error(format!(
                        "a `[max: N]` element type must be a whole number of bytes, but this one is {elem_bits} bits"
                    ))
                    .primary(elem.span, format!("{elem_bits} bits"))
                    .note("decoding divides the remaining bytes by the element width to recover the count, which only works if elements are whole bytes (§6.3)")
                    .help("use a byte-multiple element type, e.g. `u8`, `u16`, or a struct declared with a byte-multiple width");
                    self.error(e);
                    return None;
                }
                Some((
                    WireType::VarArray { elem: Box::new(wire), max: max.value },
                    Layout { fixed_bits: 0, tail: Some(Tail { elem_bits, max_elems: max.value }) },
                ))
            }
            ast::FieldTypeKind::Str { max } => Some((
                WireType::Str { max: max.value },
                Layout { fixed_bits: 0, tail: Some(Tail { elem_bits: 8, max_elems: max.value }) },
            )),
        }
    }

    fn reject_variable_element(&mut self, elem: &ast::ScalarType, what: &str, section: &str) {
        let name = elem.named().map_or_else(|| "this type".to_string(), |i| format!("`{}`", i.name));
        let e = Diagnostic::error(format!("{name} is variable-length, so it cannot be {what}"))
            .primary(elem.span, "its size depends on the buffer")
            .note(format!("only the outermost trailing field may be variable-length; an element's width has to be known to place the next one {section}"))
            .help("use a fixed-width element type");
        self.error(e);
    }

    fn resolve_scalar(&mut self, scalar: &'a ast::ScalarType) -> Option<WireType> {
        match &scalar.kind {
            ast::ScalarKind::UInt(n) => Some(WireType::UInt(*n)),
            ast::ScalarKind::Int(n) => Some(WireType::Int(*n)),
            ast::ScalarKind::Bool => Some(WireType::Bool),
            ast::ScalarKind::Named(ident) => {
                let id = self.lookup(ident, Use::Field)?;
                self.mark_nested(id);
                Some(WireType::Named(id))
            }
        }
    }

    /// The size of a scalar. Composite sizes come from
    /// [`Checker::resolve_field_type`], which computes them as it validates.
    fn scalar_layout(&self, wire: &WireType) -> Layout {
        match wire {
            WireType::UInt(n) | WireType::Int(n) => Layout::fixed(*n),
            WireType::Bool => Layout::fixed(1),
            WireType::Named(id) => self.types[id.0].layout,
            // `resolve_scalar` never produces these.
            WireType::Array { .. } | WireType::VarArray { .. } | WireType::Str { .. } => Layout::fixed(0),
        }
    }

    // -----------------------------------------------------------------------
    // Name resolution (§9, §11)
    // -----------------------------------------------------------------------

    fn lookup(&mut self, ident: &Ident, using: Use) -> Option<TypeId> {
        match self.first_decl.get(ident.name.as_str()).copied() {
            Some(index) if index < self.current => match self.decl_type[index] {
                Some(id) => {
                    if self.poisoned.contains(&id) {
                        self.tainted = true;
                    }
                    Some(id)
                }
                // A `service`, a `const`, or a type declaration that bailed out.
                None => {
                    match &self.schema.decls[index] {
                        Decl::Service(_) => {
                            let e = Diagnostic::error(format!("`{}` is a service, not a type", ident.name))
                                .primary(ident.span, format!("cannot use a service as {}", using.as_str()))
                                .secondary(self.schema.decls[index].name().span, "declared here")
                                .note("a service groups characteristic bindings; it has no wire representation of its own (§10)");
                            self.error(e);
                        }
                        Decl::Const(_) => {
                            let e = Diagnostic::error(format!("`{}` is a constant, not a type", ident.name))
                                .primary(ident.span, format!("cannot use a constant as {}", using.as_str()))
                                .secondary(self.schema.decls[index].name().span, "declared here")
                                .note("a `const` is a plain value for generated code to use directly; it has no wire representation and cannot appear as a field or characteristic type (§3.1)");
                            self.error(e);
                        }
                        _ => {}
                    }
                    self.tainted = true;
                    None
                }
            },
            Some(index) if index == self.current => {
                let e = Diagnostic::error(format!("`{}` cannot contain itself", ident.name))
                    .primary(ident.span, "recursive type reference")
                    .note("a container's layout is computed at compile time, so it cannot embed itself directly or transitively (§11)")
                    .help("hold the nested value in a separate characteristic instead");
                self.error(e);
                self.tainted = true;
                None
            }
            Some(index) => {
                let decl = &self.schema.decls[index];
                let e = Diagnostic::error(format!("`{}` is used before it is declared", ident.name))
                    .primary(ident.span, "declared later in the file")
                    .secondary(decl.name().span, format!("this {} is declared here", decl.kind_str()))
                    .note("declarations are resolved in order so every layout is known by the time it is used; forward references are not allowed (§9)")
                    .help(format!("move the `{}` declaration above this one", ident.name));
                self.error(e);
                self.tainted = true;
                None
            }
            None => {
                let mut e = Diagnostic::error(format!("unknown type `{}`", ident.name))
                    .primary(ident.span, "not declared in this file")
                    .note("a type is either a primitive (`uN`, `iN`, `bool`) or something declared earlier in this file (§2, §9)");
                if let Some(s) = suggest(&ident.name, &self.all_names) {
                    e = e.help(format!("did you mean `{s}`?"));
                }
                self.error(e);
                self.tainted = true;
                None
            }
        }
    }

    /// Records that a type is embedded in another container, following alias
    /// indirection: using an alias nested uses its target nested too (§8).
    fn mark_nested(&mut self, id: TypeId) {
        self.mark(id, |t| t.nested = true);
    }

    fn mark_root(&mut self, id: TypeId) {
        self.mark(id, |t| t.root = true);
    }

    fn mark(&mut self, id: TypeId, set: impl Fn(&mut TypeDef)) {
        let mut next = Some(id);
        // Ids strictly decrease along an alias chain (§9), so this terminates.
        while let Some(id) = next {
            let def = &mut self.types[id.0];
            set(def);
            next = match &def.kind {
                TypeKind::Alias(alias) => alias.target.named(),
                _ => None,
            };
        }
    }

    /// `#[endian(...)]` is meaningful only on a container that is encoded as a
    /// root; on a type that is only ever nested it is a compile error (§8).
    fn check_endian_placement(&mut self) {
        for (id, span) in std::mem::take(&mut self.endian_attrs) {
            let def = &self.types[id.0];
            if def.nested && !def.root {
                let (name, decl_span) = (def.name.clone(), def.span);
                let e = Diagnostic::error(format!(
                    "`{name}` is only ever used as a nested field, so `#[endian(...)]` has no meaning on it"
                ))
                .primary(span, "no root container to apply this to")
                .secondary(decl_span, format!("`{name}` is declared here"))
                .note("bit-packing flattens a nested type into its parent's single contiguous bit sequence, and byte order is applied once, to that sequence, by the root container's setting (§8)")
                .help(format!("put the attribute on the container that is bound to a characteristic, or bind `{name}` to one"));
                self.error(e);
            }
        }
    }

    // -----------------------------------------------------------------------
    // GATT metadata (§10)
    // -----------------------------------------------------------------------

    fn check_service(&mut self, d: &'a ast::ServiceDecl) -> Service {
        self.check_uuid(&d.uuid, &format!("service `{}`", d.name.name));
        if let Some(first) = self.service_uuids.insert(normalize_uuid(&d.uuid.value), d.uuid.span) {
            let e = Diagnostic::error("two services share one UUID")
                .primary(d.uuid.span, "declared again here")
                .secondary(first, "first used here")
                .note("a service UUID identifies the service on the peripheral; two declarations of it describe the same service twice (§10)");
            self.error(e);
        }

        let mut characteristics = Vec::with_capacity(d.characteristics.len());
        let mut uuids: HashMap<String, Span> = HashMap::new();

        for c in &d.characteristics {
            if let Some(first) = self.char_names.insert(c.name.name.clone(), c.name.span) {
                let e = Diagnostic::error(format!("characteristic `{}` is declared more than once", c.name.name))
                    .primary(c.name.span, "declared again here")
                    .secondary(first, "first declared here")
                    .note("characteristic names are unique across the file: each becomes one named binding in generated code (§11)");
                self.error(e);
            }
            self.check_uuid(&c.uuid, &format!("characteristic `{}`", c.name.name));
            if let Some(first) = uuids.insert(normalize_uuid(&c.uuid.value), c.uuid.span) {
                let e = Diagnostic::error(format!(
                    "two characteristics of service `{}` share one UUID",
                    d.name.name
                ))
                .primary(c.uuid.span, "declared again here")
                .secondary(first, "first used here")
                .note("a client discovers characteristics by UUID within a service, so duplicates inside one service are indistinguishable (§10)");
                self.error(e);
            }
            self.check_properties(c);

            let Some(ty) = self.lookup(&c.ty, Use::Characteristic) else {
                continue;
            };
            if !self.check_bindable(c, ty) {
                continue;
            }
            self.mark_root(ty);

            let def = &self.types[ty.0];
            let layout = def.layout;
            let endian = def.endian;
            self.check_binding_size(c, ty);

            characteristics.push(Characteristic {
                docs: c.docs.clone(),
                span: c.span,
                name: c.name.name.clone(),
                uuid: c.uuid.value.clone(),
                properties: c.properties.iter().map(|p| p.value).collect(),
                ty,
                layout,
                endian,
            });
        }

        Service {
            docs: d.docs.clone(),
            span: d.span,
            name: d.name.name.clone(),
            uuid: d.uuid.value.clone(),
            characteristics,
        }
    }

    fn check_properties(&mut self, c: &ast::Characteristic) {
        let mut seen: HashMap<ast::Property, Span> = HashMap::new();
        for p in &c.properties {
            if let Some(first) = seen.insert(p.value, p.span) {
                let e = Diagnostic::error(format!("duplicate property `{}`", p.value.as_str()))
                    .primary(p.span, "listed twice")
                    .secondary(first, "first listed here")
                    .note("the property list is a set: repeating an entry says nothing extra (§10)");
                self.error(e);
            }
        }
    }

    /// A characteristic binds a `struct`, tagged-union `enum`, or `alias` (§10).
    fn check_bindable(&mut self, c: &'a ast::Characteristic, ty: TypeId) -> bool {
        let def = &self.types[ty.0];
        match def.kind {
            TypeKind::Struct(_) | TypeKind::Union(_) | TypeKind::Alias(_) => true,
            TypeKind::Enum(_) | TypeKind::Scaled(_) => {
                let (kind, name, decl_span) = (def.kind_str(), def.name.clone(), def.span);
                let e = Diagnostic::error(format!(
                    "characteristic `{}` cannot bind the {kind} `{name}`",
                    c.name.name
                ))
                .primary(c.ty.span, format!("`{name}` is a {kind}"))
                .secondary(decl_span, "declared here")
                .note("a characteristic binds a `struct`, a tagged-union `enum`, or an `alias` (§10)")
                .help(format!("declare `alias {name}Value = {name};` and bind that, or wrap it in a struct"));
                self.error(e);
                false
            }
        }
    }

    /// A bound container is what the transport actually carries, so it has to
    /// be a whole number of bytes (§10); §6.3 already guarantees the fixed
    /// prefix of a variable-length type is.
    fn check_binding_size(&mut self, c: &'a ast::Characteristic, ty: TypeId) {
        let def = &self.types[ty.0];
        let (layout, name, decl_span) = (def.layout, def.name.clone(), def.span);
        if self.poisoned.contains(&ty) {
            return;
        }

        if !layout.is_byte_aligned() {
            let bits = layout.fixed_bits;
            let e = Diagnostic::error(format!(
                "characteristic `{}` binds `{name}`, which is {bits} bits — not a whole number of bytes",
                c.name.name
            ))
            .primary(c.ty.span, format!("`{name}` is {bits} bits wide"))
            .secondary(decl_span, "declared here")
            .note("a characteristic's value is a byte buffer framed by ATT, so a bound container's width must be a multiple of 8 (§10, §12)")
            .help(format!("pad `{name}` out to {} bits", bits.next_multiple_of(8)));
            self.error(e);
            return;
        }

        let bytes = layout.max_bytes();
        if bytes > u128::from(DEFAULT_ATT_PAYLOAD_BYTES) {
            let size =
                if layout.is_variable() { format!("up to {bytes} bytes") } else { format!("{bytes} bytes") };
            let w = Diagnostic::warning(format!(
                "characteristic `{}` is {size}, more than the {DEFAULT_ATT_PAYLOAD_BYTES}-byte default ATT payload",
                c.name.name
            ))
            .primary(c.ty.span, format!("`{name}` encodes to {size}"))
            .note("without MTU negotiation an ATT packet carries 20 bytes of value; this is not an error because the MTU is negotiable at runtime (§10)")
            .help("make sure the client negotiates a larger MTU, or split the value across characteristics");
            self.error(w);
        }
    }

    fn check_uuid(&mut self, uuid: &Spanned<String>, owner: &str) {
        if is_uuid(&uuid.value) {
            return;
        }
        let e = Diagnostic::error(format!("{owner} has a malformed UUID"))
            .primary(uuid.span, format!("`{}` is not a GATT UUID", uuid.value))
            .note("a GATT UUID is 16-bit (`180a`), 32-bit (`0000180a`) or the 128-bit form `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`, in hexadecimal (§10)")
            .help("write it as `\"7d8f0001-3c1a-4e8a-9b5a-000000000000\"`");
        self.error(e);
    }
}

/// Where a name is being used, for the "not a type" message.
#[derive(Clone, Copy)]
enum Use {
    Field,
    Characteristic,
}

impl Use {
    fn as_str(self) -> &'static str {
        match self {
            Use::Field => "a field type",
            Use::Characteristic => "a characteristic's value type",
        }
    }
}

/// Whether the container being walked may hold a variable-length field: a
/// struct may (§6.3), a tagged-union variant may not (§7, §14).
#[derive(Clone, Copy, PartialEq, Eq)]
enum VarFields {
    Allowed,
    Rejected,
}

/// The result of placing one container's fields.
struct Walk {
    fields: Vec<Field>,
    /// Total fixed bits, including a variable-length field's own fixed prefix.
    bits: u32,
    tail: Option<Tail>,
    /// Span of the field that contributed the tail.
    var_span: Option<Span>,
    /// False if a field failed to resolve or broke a placement rule, in which
    /// case width checks on the container are skipped to avoid cascades.
    ok: bool,
}

impl Default for Walk {
    fn default() -> Self {
        Walk { fields: Vec::new(), bits: 0, tail: None, var_span: None, ok: true }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn field_role(field: &ast::Field) -> FieldRole {
    match &field.kind {
        ast::FieldKind::Value { name, .. } => FieldRole::Value { name: name.name.clone() },
        ast::FieldKind::Padding { check_zero, .. } => FieldRole::Padding { check_zero: *check_zero },
        ast::FieldKind::Reserved { name, .. } => FieldRole::Reserved { name: name.name.clone() },
    }
}

/// The largest value an `N`-bit unsigned field can hold.
fn max_value(bits: u32) -> u128 {
    if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 }
}

/// The narrowest `uN` that could hold `value`, for "widen it to" hints.
fn widen_for(value: u128) -> u32 {
    (128 - value.leading_zeros()).max(1)
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Whether a declaration name collides with a type spelling the language
/// already understands (§2).
fn primitive_like(name: &str) -> Option<&'static str> {
    match name {
        "bool" => Some("`bool`"),
        "f32" | "f64" => Some("a physical type"),
        "string" => Some("`string`"),
        _ => {
            let (prefix, digits) = name.split_at(1);
            let integer = matches!(prefix, "u" | "i")
                && !digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit());
            integer.then_some("an integer type such as `u8`")
        }
    }
}

/// Whether a string is one of the three GATT UUID forms: 16-bit, 32-bit, or
/// the canonical 128-bit spelling (§10).
fn is_uuid(s: &str) -> bool {
    let hex = |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_hexdigit());
    match s.len() {
        4 => hex(s, 4),
        8 => hex(s, 8),
        36 => {
            let parts: Vec<&str> = s.split('-').collect();
            parts.len() == 5 && parts.iter().zip([8, 4, 4, 4, 12]).all(|(p, n)| hex(p, n))
        }
        _ => false,
    }
}

/// Case-insensitive key for duplicate-UUID checks.
fn normalize_uuid(s: &str) -> String {
    s.to_ascii_lowercase()
}
