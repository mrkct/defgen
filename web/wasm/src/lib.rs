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

use defgen::backends::{self, Options};
use defgen::diag::{Diagnostic, Severity};
use defgen::model::{Layout, Model};
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

/// What the CLI prints after writing a file: the resolved version and byte
/// order, every declared type with its size, and the GATT bindings.
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
        ])
    });

    let services = model.services.iter().map(|service| {
        let characteristics = service.characteristics.iter().map(|c| {
            obj(&[
                ("name", s(&c.name)),
                ("uuid", s(&c.uuid)),
                ("type", s(&model.get(c.ty).name)),
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

    obj(&[
        ("version", n(model.version)),
        ("endian", s(model.endian.as_str())),
        ("types", arr(types)),
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
