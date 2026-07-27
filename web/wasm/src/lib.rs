//! WebAssembly entry points for the browser playground.
//!
//! `defgen`'s front end and its backends are plain computation over a string —
//! no I/O, no clock, no environment — so the compiler itself needs nothing
//! added to run in a browser. What this crate adds is the boundary: a handful
//! of `extern "C"` functions and a way to move strings across it.
//!
//! # ABI
//!
//! Arguments are `(pointer, length)` pairs of UTF-8 bytes in the module's
//! linear memory. The caller allocates them with [`defgen_alloc`] and releases
//! them with [`defgen_free`].
//!
//! Every function that returns a string returns a pointer to a buffer holding
//! the payload length as a little-endian `u32` followed by that many bytes of
//! UTF-8 — one return value, because a wasm function has only one. The caller
//! owns that buffer and frees the whole `4 + length` block, again with
//! [`defgen_free`].
//!
//! All buffers on this boundary are allocated with an alignment of 1 and freed
//! with the length they were allocated with, so both sides agree on the layout
//! without either having to carry it around.
//!
//! The payload is JSON in every case; `site/defgen.js` is the other half.

mod json;

use std::alloc::{Layout as AllocLayout, alloc, dealloc};

use defgen::backends::javascript::{field_ident, ident, member_ident};
use defgen::backends::{self, Options};
use defgen::diag::{Diagnostic, Severity};
use defgen::model::{
    Enum, Field, FieldRole, Layout, Model, Scaled, Struct, TypeDef, TypeId, TypeKind, Union, WireType,
    carrier_bits, int_range,
};
use defgen::span::line_col;

use json::{arr, b, n, null, obj, s};

/// Include the parsed syntax tree in the result, as the CLI's `--ast` does.
const FLAG_AST: u32 = 1 << 0;
/// Include the checked model in the result, as the CLI's `--model` does.
const FLAG_MODEL: u32 = 1 << 1;

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// Allocates `len` bytes for the caller to write an argument into, or to
/// receive a result in. Returns a dangling-but-valid pointer for `len == 0`,
/// which the allocator itself refuses to produce.
#[unsafe(no_mangle)]
pub extern "C" fn defgen_alloc(len: usize) -> *mut u8 {
    let Some(layout) = layout_for(len) else {
        return std::ptr::dangling_mut();
    };
    // SAFETY: `layout_for` returns `None` for the zero size `alloc` forbids.
    let ptr = unsafe { alloc(layout) };
    if ptr.is_null() {
        std::alloc::handle_alloc_error(layout);
    }
    ptr
}

/// Releases a buffer from [`defgen_alloc`], or one returned by any of the
/// entry points below.
///
/// # Safety
///
/// `ptr` must have come from this module's allocator and `len` must be the
/// length it was allocated with — for a returned buffer, `4 + payload length`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn defgen_free(ptr: *mut u8, len: usize) {
    if let Some(layout) = layout_for(len) {
        // SAFETY: the caller's contract is exactly `dealloc`'s.
        unsafe { dealloc(ptr, layout) };
    }
}

fn layout_for(len: usize) -> Option<AllocLayout> {
    (len > 0).then(|| AllocLayout::from_size_align(len, 1).expect("a byte layout is always valid"))
}

/// Moves `text` into a length-prefixed buffer and hands it to the caller.
fn into_result(text: &str) -> *mut u8 {
    let bytes = text.as_bytes();
    let ptr = defgen_alloc(4 + bytes.len());
    // SAFETY: `ptr` owns `4 + bytes.len()` bytes, which is exactly what the
    // two writes cover, and neither source can overlap a fresh allocation.
    unsafe {
        std::ptr::copy_nonoverlapping((bytes.len() as u32).to_le_bytes().as_ptr(), ptr, 4);
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(4), bytes.len());
    }
    ptr
}

/// Borrows an argument as a string.
///
/// # Safety
///
/// `ptr` must point at `len` readable bytes that outlive the call. They must
/// be UTF-8; anything else is read as empty, which is what a caller that is
/// not `TextEncoder` deserves.
unsafe fn str_arg<'a>(ptr: *const u8, len: usize) -> &'a str {
    if len == 0 {
        return "";
    }
    // SAFETY: the caller guarantees the range is readable for the call.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes).unwrap_or("")
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// The backends `defgen_compile` accepts, as
/// `[{"name": …, "description": …}]` — the same registry, in the same order,
/// that the CLI's `--backend` reads its accepted values from. The page builds
/// its backend picker from this rather than hard-coding a list that a new
/// backend would silently leave out of date.
#[unsafe(no_mangle)]
pub extern "C" fn defgen_backends() -> *mut u8 {
    let backends = backends::all()
        .into_iter()
        .map(|backend| obj(&[("name", s(backend.name())), ("description", s(backend.description()))]));
    into_result(&arr(backends))
}

/// Compiles `src` with the named backend.
///
/// `stem` is the schema's base name — what the CLI takes from the file name,
/// and what backends derive module names, include guards and generated file
/// names from. `flags` is a bitmask of [`FLAG_AST`] and [`FLAG_MODEL`].
///
/// The result is one JSON object; see [`compile_json`] for its shape.
///
/// # Safety
///
/// Every pointer/length pair must satisfy [`str_arg`]'s contract.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn defgen_compile(
    src: *const u8,
    src_len: usize,
    backend: *const u8,
    backend_len: usize,
    stem: *const u8,
    stem_len: usize,
    flags: u32,
) -> *mut u8 {
    // SAFETY: forwarded from this function's own contract.
    let (src, backend, stem) =
        unsafe { (str_arg(src, src_len), str_arg(backend, backend_len), str_arg(stem, stem_len)) };
    into_result(&compile_json(src, backend, stem, flags))
}

/// Everything one call produced, before it is rendered.
#[derive(Default)]
struct Outcome {
    /// A model was produced and code generated. Warnings (§10's MTU
    /// diagnostic) leave this true, exactly as they do not stop the CLI from
    /// writing a file.
    ok: bool,
    /// A problem with the request itself rather than with the schema.
    error: Option<String>,
    diagnostics: Vec<Diagnostic>,
    /// Encoded `{name, contents}` objects.
    files: Vec<String>,
    summary: Option<String>,
    ast: Option<String>,
    model: Option<String>,
}

impl Outcome {
    /// The JSON the page receives:
    ///
    /// ```json
    /// {
    ///   "ok": true,
    ///   "error": null,
    ///   "diagnostics": [{"severity": "error", "message": …, "line": …,
    ///                    "column": …, "rendered": …}],
    ///   "files": [{"name": …, "contents": …}],
    ///   "summary": {…},             // present whenever "ok"
    ///   "ast": null, "model": null  // the dumps, when asked for
    /// }
    /// ```
    fn render(self, src: &str, filename: &str) -> String {
        let mut sorted: Vec<&Diagnostic> = self.diagnostics.iter().collect();
        sorted.sort_by_key(|d| (d.span().start, d.span().end));
        let diagnostics = sorted.into_iter().map(|d| diagnostic(d, src, filename));

        obj(&[
            ("ok", b(self.ok)),
            ("error", self.error.as_deref().map_or_else(null, s)),
            ("diagnostics", arr(diagnostics)),
            ("files", arr(self.files)),
            ("summary", self.summary.unwrap_or_else(null)),
            ("ast", self.ast.as_deref().map_or_else(null, s)),
            ("model", self.model.as_deref().map_or_else(null, s)),
        ])
    }
}

fn compile_json(src: &str, backend_name: &str, stem: &str, flags: u32) -> String {
    // An empty stem would give C an include guard of `__H` and Python a module
    // called `_`; the CLI defaults the same way for a path without a stem.
    let stem = if stem.trim().is_empty() { "schema" } else { stem.trim() };
    let filename = format!("{stem}.defs");
    let mut outcome = Outcome::default();

    // The backend is resolved before any work, so an unknown name fails
    // immediately rather than after a schema has been parsed and checked.
    let Some(backend) = backends::find(backend_name) else {
        outcome.error = Some(format!("unknown backend `{backend_name}`"));
        return outcome.render(src, &filename);
    };

    let parsed = defgen::parse(src);
    outcome.diagnostics = parsed.diagnostics;
    let Some(schema) = parsed.schema else {
        return outcome.render(src, &filename);
    };
    if flags & FLAG_AST != 0 {
        outcome.ast = Some(format!("{schema:#?}"));
    }

    let checked = defgen::check(&schema);
    outcome.diagnostics.extend(checked.diagnostics);
    let Some(model) = checked.model else {
        return outcome.render(src, &filename);
    };

    let opts = Options { stem: stem.to_string(), source: Some(filename.clone()) };
    let generated = backend.generate(&model, &opts);

    outcome.ok = true;
    outcome.files = generated
        .files
        .iter()
        .map(|file| obj(&[("name", s(&file.name)), ("contents", s(&file.contents))]))
        .collect();
    outcome.summary = Some(summary(&model));
    if flags & FLAG_MODEL != 0 {
        outcome.model = Some(format!("{model:#?}"));
    }
    outcome.render(src, &filename)
}

/// One diagnostic, both as parts the page can lay out itself and as the
/// rendered block the CLI would print. Colour is off: the page styles the
/// block, and ANSI escapes are not something a `<pre>` knows what to do with.
fn diagnostic(d: &Diagnostic, src: &str, filename: &str) -> String {
    let (line, column) = line_col(src, d.span().start);
    obj(&[
        (
            "severity",
            s(match d.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            }),
        ),
        ("message", s(&d.message)),
        ("line", n(line)),
        ("column", n(column)),
        ("rendered", s(&d.render(filename, src, false))),
    ])
}

// ---------------------------------------------------------------------------
// Schema summary
// ---------------------------------------------------------------------------

/// What the CLI prints after writing a file: the resolved byte order, every
/// declared type with its size, and the GATT bindings.
///
/// Sizes come across as numbers rather than as the CLI's phrasing, because the
/// page lays them out as table cells rather than as a line of text.
fn summary(model: &Model) -> String {
    let types = model.types.iter().map(|ty| {
        obj(&[
            ("kind", s(ty.kind_str())),
            ("name", s(&ty.name)),
            ("root", b(ty.root)),
            ("nested", b(ty.nested)),
            ("endian", s(ty.endian.as_str())),
            // A nested type is encoded in its container's byte order (§8), so
            // a type's own is only worth showing where the schema states it.
            ("endianExplicit", b(ty.endian_explicit)),
            ("size", size(ty.layout)),
            // The Device tab's form generator and value renderer recurse over
            // this instead of the table above: it is the same information,
            // shaped for building inputs and reading decoded instances rather
            // than for a row in a table.
            ("shape", shape(model, ty)),
        ])
    });

    let services = model.services.iter().map(|service| {
        let characteristics = service.characteristics.iter().map(|c| {
            obj(&[
                ("name", s(&c.name)),
                ("uuid", s(&c.uuid)),
                ("type", s(&model.get(c.ty).name)),
                // The bound type, as the same `{kind: "named", ...}` shape a
                // struct field of this type would get — so the Device tab
                // walks a characteristic's value with the very same recursive
                // form/decoder it uses for a nested field, root or not.
                ("valueType", wire_type(model, &WireType::Named(c.ty))),
                // How to actually invoke that codec at the root: a struct or
                // union exposes it as instance/class methods, but a root
                // alias, scaled type or enum has no class at all ([`shape`],
                // `entry_functions` in backends::javascript) — only the two
                // free functions this names.
                ("jsCodec", codec(model, c.ty)),
                ("endian", s(c.endian.as_str())),
                ("properties", arr(c.properties.iter().map(|p| s(p.as_str())))),
                ("size", size(c.layout)),
            ])
        });
        obj(&[
            ("name", s(&service.name)),
            ("uuid", s(&service.uuid)),
            ("characteristics", arr(characteristics)),
        ])
    });

    // A `const` (§3.1) has no layout/root/nested/endian of its own — just a
    // name, a type, and a value — so it gets its own flat row shape rather
    // than reusing the type table's columns.
    let consts = model.consts.iter().map(|c| {
        let value = if c.negative { format!("-{}", c.magnitude) } else { c.magnitude.to_string() };
        obj(&[
            ("name", s(&c.name)),
            ("type", s(&format!("{}{}", if c.signed { "i" } else { "u" }, c.bits))),
            ("value", big(value)),
        ])
    });

    obj(&[
        ("endian", s(model.endian.as_str())),
        ("types", arr(types)),
        ("consts", arr(consts)),
        ("services", arr(services)),
    ])
}

/// A layout as `{fixedBits, maxBytes, variable}`. A variable-length type's
/// fixed prefix is always byte-aligned (§6.3), so `fixedBits` is only ever
/// a non-multiple of 8 for a fixed-width type, where it is the whole size.
fn size(layout: Layout) -> String {
    obj(&[
        ("fixedBits", n(layout.fixed_bits)),
        // Saturating keeps the number inside what JSON can carry losslessly
        // into a double; reaching it needs an array of 2^53 elements, which
        // §6 rejects long before it gets here.
        ("maxBytes", n(layout.max_bytes().min(u128::from(1u64 << 53)))),
        ("variable", b(layout.is_variable())),
    ])
}

// ---------------------------------------------------------------------------
// Type shape (Device tab)
// ---------------------------------------------------------------------------
//
// A recursive description of a declared type, for building a form that writes
// a value and for reading one back off a decoded instance — the two things
// the summary's flat `{kind, size}` row can't drive. Every generated-code
// identifier in here (`jsName`/`jsType`) is computed with the same functions
// `backends::javascript` itself used, so a name the module actually exports
// is never re-derived, only looked up.

/// A `u128`/`i128` as a JSON string rather than a bare number: both bounds of
/// a 128-bit field are routinely past `Number.MAX_SAFE_INTEGER`, and the
/// JavaScript backend itself only starts using `bigint` past 32 bits (see its
/// module doc) — a plain JSON number here would silently round exactly the
/// values that matter most for a range check.
fn big(value: impl std::fmt::Display) -> String {
    s(&value.to_string())
}

fn shape(model: &Model, def: &TypeDef) -> String {
    let name = ident(&def.name);
    match &def.kind {
        TypeKind::Alias(a) => obj(&[("form", s("alias")), ("target", wire_type(model, &a.target))]),
        TypeKind::Scaled(sc) => scaled_shape(sc),
        TypeKind::Enum(e) => enum_shape(&name, e),
        TypeKind::Union(u) => union_shape(model, &name, u),
        TypeKind::Struct(st) => struct_shape(model, st),
    }
}

/// A [`WireType`] as `{kind, ...}` — `uint`/`int` (with a range, §2), `bool`,
/// `named` (a pointer by name into the same `types` array), `array` (fixed
/// count, §6.1), `vararray` (a bound, §6.3) or `string` (a byte bound, §6.3).
fn wire_type(model: &Model, ty: &WireType) -> String {
    match ty {
        WireType::UInt(bits) => {
            let (_, max) = int_range(*bits, false);
            obj(&[
                ("kind", s("uint")),
                ("bits", n(*bits)),
                ("carrierBits", n(carrier_bits(*bits))),
                ("min", big(0)),
                ("max", big(max)),
            ])
        }
        WireType::Int(bits) => {
            let (min, max) = int_range(*bits, true);
            obj(&[
                ("kind", s("int")),
                ("bits", n(*bits)),
                ("carrierBits", n(carrier_bits(*bits))),
                ("min", big(min)),
                ("max", big(max)),
            ])
        }
        WireType::Bool => obj(&[("kind", s("bool"))]),
        WireType::Named(id) => {
            let target = model.get(*id);
            obj(&[("kind", s("named")), ("name", s(&target.name)), ("jsName", s(&ident(&target.name)))])
        }
        WireType::Array { elem, count } => {
            obj(&[("kind", s("array")), ("elem", wire_type(model, elem)), ("count", n(*count))])
        }
        WireType::VarArray { elem, max } => {
            obj(&[("kind", s("vararray")), ("elem", wire_type(model, elem)), ("max", n(*max))])
        }
        WireType::Str { max } => obj(&[("kind", s("string")), ("max", n(*max))]),
    }
}

/// A visible field (§6.2 drops `padding`, which has no property to set) as
/// `{name, jsName, reserved, type}`.
fn field_shape(model: &Model, field: &Field) -> Option<String> {
    let (name, reserved) = match &field.role {
        FieldRole::Value { name } => (name, false),
        FieldRole::Reserved { name } => (name, true),
        FieldRole::Padding { .. } => return None,
    };
    Some(obj(&[
        ("name", s(name)),
        ("jsName", s(&field_ident(name))),
        // Written back unchanged on encode rather than freely settable
        // (model.rs's `FieldRole::Reserved`) — the form disables it by
        // default, pre-filled from the last decode rather than left at 0.
        ("reserved", b(reserved)),
        ("type", wire_type(model, &field.ty)),
    ]))
}

fn fields_shape(model: &Model, fields: &[Field]) -> String {
    arr(fields.iter().filter_map(|f| field_shape(model, f)))
}

fn scaled_shape(sc: &Scaled) -> String {
    let (raw_min, raw_max) = int_range(sc.raw_bits, sc.signed);
    obj(&[
        ("form", s("scaled")),
        ("rawBits", n(sc.raw_bits)),
        ("signed", b(sc.signed)),
        ("carrierBits", n(carrier_bits(sc.raw_bits))),
        ("physical", s(sc.physical.as_str())),
        ("scale", n(sc.scale)),
        ("offset", n(sc.offset)),
        // The *raw* integer's range (§4) — a field of this type holds the
        // scaled `f32`/`f64` physical value, but validating an edit needs the
        // pre-conversion bound the same way encode does, and floating-point
        // scale/offset make deriving it from the physical value lossy.
        ("rawMin", big(raw_min)),
        ("rawMax", big(raw_max)),
    ])
}

/// `type_js_name` is the enum's own class name — an open enum's synthesized
/// "unknown" class is `<Enum><Arm>` (§5), the exact concatenation
/// `backends::javascript::declare_enum` builds, so it is spelled out here
/// rather than left for the consumer to reassemble.
fn enum_shape(type_js_name: &str, e: &Enum) -> String {
    let variants = e.variants.iter().map(|v| {
        obj(&[("name", s(&v.name)), ("jsName", s(&member_ident(&v.name))), ("value", big(v.value))])
    });
    let else_arm = e.else_arm.as_ref().map(|arm| {
        obj(&[("name", s(&arm.name)), ("jsName", s(&format!("{type_js_name}{}", ident(&arm.name))))])
    });
    obj(&[
        ("form", s("enum")),
        ("backingBits", n(e.backing_bits)),
        // Whether a variant's value (and an "other" raw entry) is a `number`
        // or a `bigint` in the generated module — the same `> 32` carrier
        // cutoff `backends::javascript::is_big` uses, spelled out here so the
        // Device tab never has to re-decide it from `backingBits` itself.
        ("carrierBits", n(carrier_bits(e.backing_bits))),
        ("variants", arr(variants)),
        ("elseArm", else_arm.unwrap_or_else(null)),
    ])
}

/// `type_js_name` is the union's own class name; each variant's class is
/// `<Union><Variant>` (§7), again spelled out here rather than reassembled by
/// the consumer. An open union's "unknown" class carries the tag under the
/// union's own tag property name, plus `raw` when the payload region is
/// non-empty — exactly the two arguments `declare_union` passes it.
fn union_shape(model: &Model, type_js_name: &str, u: &Union) -> String {
    let variants = u.variants.iter().map(|v| {
        obj(&[
            ("name", s(&v.name)),
            ("jsName", s(&format!("{type_js_name}{}", ident(&v.name)))),
            ("id", big(v.id)),
            ("fields", fields_shape(model, &v.fields)),
        ])
    });
    let else_arm = u.else_arm.as_ref().map(|arm| {
        obj(&[
            ("name", s(&arm.name)),
            ("jsName", s(&format!("{type_js_name}{}", ident(&arm.name)))),
            ("rawBits", n(arm.raw_bits)),
            ("rawCarrierBits", n(carrier_bits(arm.raw_bits))),
        ])
    });
    obj(&[
        ("form", s("union")),
        ("tagName", s(&u.tag_name)),
        ("tagJsName", s(&field_ident(&u.tag_name))),
        ("tagBits", n(u.tag_bits)),
        ("tagCarrierBits", n(carrier_bits(u.tag_bits))),
        ("variants", arr(variants)),
        ("elseArm", else_arm.unwrap_or_else(null)),
    ])
}

fn struct_shape(model: &Model, st: &Struct) -> String {
    obj(&[("form", s("struct")), ("fields", fields_shape(model, &st.fields))])
}

/// How a characteristic's root value is encoded/decoded: `{kind: "class"}`
/// for a `struct`/tagged `union` (`value.encode()` / `Class.decode(bytes)`),
/// or `{kind: "functions", encode, decode}` naming the free functions a root
/// `alias`, `scaled` or `enum` gets instead — mirrors
/// `backends::javascript::Emitter::has_entry_functions` exactly, since a type
/// only has one of these two shapes depending on the very same condition.
fn codec(model: &Model, id: TypeId) -> String {
    let def = model.get(id);
    match &def.kind {
        TypeKind::Struct(_) | TypeKind::Union(_) => obj(&[("kind", s("class"))]),
        _ => obj(&[
            ("kind", s("functions")),
            ("encode", s(&format!("encode{}", member_ident(&def.name)))),
            ("decode", s(&format!("decode{}", member_ident(&def.name)))),
        ]),
    }
}
