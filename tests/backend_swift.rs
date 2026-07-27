//! The Swift backend.
//!
//! Two layers of test live here. The cheap ones read the generated text and
//! assert on what it declares — these always run. The expensive one hands the
//! generated file to a real Swift compiler together with
//! `tests/examples/swift_conformance.swift` and runs the result, which is
//! what actually pins the wire format down: a codegen bug that produces valid
//! Swift saying the wrong thing is invisible to string matching.
//!
//! The conformance fixture asserts the very same byte strings as its C,
//! Python and Kotlin counterparts. That is the point of §13 — several
//! backends, one wire format — so if these ever disagree, one of the
//! backends is wrong.
//!
//! Swift requires that a file with genuine top-level *statements* (as
//! opposed to declarations) be named `main.swift` when compiled alongside
//! other files, so the fixture is staged under that name; the generated
//! schema file — pure declarations — can keep its own name.
//!
//! Anything needing `swiftc` is skipped, loudly, when there isn't one, so
//! this file still does useful work on a machine without a Swift toolchain.

use std::path::{Path, PathBuf};
use std::process::Command;

use defgen::backends::{self, Backend, Options};
use defgen::diag::Severity;
use defgen::model::Model;

const EXAMPLE: &str = include_str!("examples/commands.defs");

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn model_of(src: &str) -> Model {
    let compiled = defgen::compile(src);
    let errors: Vec<String> = compiled
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.render_plain("test.defs", src))
        .collect();
    assert!(errors.is_empty(), "schema did not check:\n{}", errors.join("\n"));
    compiled.model.expect("model")
}

/// Generates a file from `src`, as the CLI would for a file named `stem`.
fn file_of(src: &str, stem: &str) -> String {
    generate(src, stem).1
}

/// The `(file name, contents)` the backend produced.
fn generate(src: &str, stem: &str) -> (String, String) {
    let model = model_of(src);
    let opts = Options { stem: stem.to_string(), source: Some(format!("{stem}.defs")) };
    let generated = backends::swift::SwiftBackend.generate(&model, &opts);
    assert_eq!(generated.files.len(), 1, "the Swift backend emits exactly one file");
    let file = generated.single();
    (file.name.clone(), file.contents.clone())
}

fn example_file() -> String {
    file_of(EXAMPLE, "commands")
}

/// A schema with just enough header to be legal, so a test can be one struct.
fn schema(body: &str) -> String {
    format!("endian: little;\n---\n{body}")
}

fn assert_contains(file: &str, needle: &str) {
    assert!(file.contains(needle), "generated file is missing `{needle}`:\n{file}");
}

/// The indented body of a top-level `struct`/`enum` named `name`, so a test
/// can say "this type declares this member" without a match somewhere else
/// in the file counting. Brace-counting rather than indentation, since a
/// nested closure (an array's `.map { ... }`) balances its own braces before
/// the declaration's own closing brace is reached.
fn type_body(file: &str, name: &str) -> String {
    let markers = [
        format!("struct {name} {{"),
        format!("struct {name}: "),
        format!("enum {name} {{"),
        format!("enum {name}: "),
    ];
    let lines: Vec<&str> = file.lines().collect();
    let start = lines
        .iter()
        .position(|l| markers.iter().any(|m| l.contains(m.as_str())))
        .unwrap_or_else(|| panic!("generated file declares no `{name}`"));

    let mut depth = 0i32;
    let mut out: Vec<&str> = Vec::new();
    for line in &lines[start..] {
        for c in line.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        out.push(line);
        if depth <= 0 {
            break;
        }
    }
    out.join("\n")
}

// ---------------------------------------------------------------------------
// Swift toolchain
// ---------------------------------------------------------------------------

/// The Swift compiler to test against, or `None` if this machine has none.
fn swiftc() -> Option<String> {
    let candidates = std::env::var("SWIFTC").into_iter().chain(["swiftc".into()]);
    candidates.into_iter().find(|k| Command::new(k).arg("--version").output().is_ok())
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("swift_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Type-checks a generated file on its own, so a schema feature that never
/// appears in the worked example still has to produce Swift a compiler
/// accepts. `-warnings-as-errors` is deliberate: generated code that warns is
/// generated code nobody wants in their build. `-typecheck` alone (no
/// linking) is enough, and sidesteps needing an entry point for a file that,
/// on its own, is pure declarations.
fn assert_compiles(file: &str, name: &str) {
    let Some(swiftc) = swiftc() else {
        eprintln!("skipping `{name}`: no swiftc found");
        return;
    };
    let dir = scratch(name);
    let src = dir.join("schema.swift");
    std::fs::write(&src, file).unwrap();

    let out = Command::new(&swiftc)
        .args(["-typecheck", "-warnings-as-errors"])
        .arg(&src)
        .output()
        .expect("failed to run swiftc");
    assert!(
        out.status.success(),
        "generated file does not compile cleanly:\n{}\n--- file ---\n{file}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Registry (SPEC.md §12 — one backend per target language)
// ---------------------------------------------------------------------------

#[test]
fn the_swift_backend_is_registered_under_swift() {
    assert!(backends::names().contains(&"swift"));
    let backend = backends::find("swift").expect("`swift` resolves to a backend");
    assert_eq!(backend.name(), "swift");
    assert!(!backend.description().is_empty());
    assert!(backends::find("Swift").is_none(), "backend names are matched exactly");
    assert!(backends::find("sw").is_none(), "and only under the name they registered");
}

#[test]
fn every_registered_backend_has_a_unique_name() {
    let mut names = backends::names();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), total, "two backends share a name");
}

#[test]
fn the_file_stem_drives_the_file_name() {
    let (name, _) = generate(EXAMPLE, "commands");
    assert_eq!(name, "commands.swift");

    // A stem that is not a legal Swift file name still has to produce one.
    assert_eq!(generate(EXAMPLE, "my-schema.v2").0, "my_schema_v2.swift");
    assert_eq!(generate(EXAMPLE, "2fast").0, "_2fast.swift");
}

#[test]
fn generation_is_deterministic() {
    assert_eq!(example_file(), example_file());
}

// ---------------------------------------------------------------------------
// File shape
// ---------------------------------------------------------------------------

#[test]
fn the_file_is_self_contained() {
    let file = example_file();
    assert!(!file.contains("import "), "the generated file needs no import at all — not even Foundation");
}

#[test]
fn doc_comments_become_triple_slash() {
    let file = example_file();
    assert_contains(&file, "/// Playback volume. The device only has 4 bits of resolution.");
    assert_contains(&file, "/// Reusable 3-axis orientation reading");
}

#[test]
fn rounding_helpers_only_appear_when_a_scaled_type_needs_them() {
    let file = example_file();
    assert_contains(&file, "private func defgenRound(_ value: Double, _ label: String) throws -> Double {");

    let plain = file_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!plain.contains("defgenRound"), "no `scaled` type means no rounding helper");
}

// ---------------------------------------------------------------------------
// Type mapping (§2, §3, §4, §5, §6, §7)
// ---------------------------------------------------------------------------

#[test]
fn every_integer_width_is_the_smallest_carrier_that_holds_it() {
    // §2: unsigned for `uN`, signed for `iN`, widened up to the next native
    // fixed-width size — 8, 16, 32, 64, 128 — Swift 6's own integer types the
    // whole way, with no arbitrary-precision fallback needed.
    let src = schema(
        "struct Widths: u256 {\n\
             a: u1,\n\
             b: u4,\n\
             c: u9,\n\
             e: u33,\n\
             f: i8,\n\
             h: i48,\n\
             padding: u153,\n\
         }",
    );
    let file = file_of(&src, "widths");
    assert_contains(&file, "var a: UInt8");
    assert_contains(&file, "var b: UInt8");
    assert_contains(&file, "var c: UInt16");
    assert_contains(&file, "var e: UInt64");
    assert_contains(&file, "var f: Int8");
    assert_contains(&file, "var h: Int64");
    // Every property defaults to zero via the hand-written initializer —
    // never inline on the property itself, since a `let` property with an
    // inline default is excluded from Swift's own memberwise init, which
    // `reserved` fields need not to happen (see the `reserved` test below).
    assert_contains(&file, "a: UInt8 = 0,");
    assert_contains(&file, "h: Int64 = 0");
    assert_contains(&file, "try defgenCheckUInt(UInt128(a), 1, \"Widths.a\")");
    assert_contains(&file, "try defgenCheckUInt(UInt128(c), 9, \"Widths.c\")");
    assert_contains(&file, "defgenWirePattern(try defgenCheckInt(Int128(f), 8, \"Widths.f\"), 8)");
    // A signed value is sign-extended back out of its declared width, never
    // read as the unsigned bit pattern.
    assert_contains(&file, "f: Int8(defgenSext(bits.get((off + 47), 8), 8))");
    assert_contains(&file, "h: Int64(defgenSext(bits.get((off + 55), 48), 48))");
    assert_compiles(&file, "widths");
}

#[test]
fn values_up_to_128_bits_use_native_int128() {
    // §2: 128 bits is the ceiling; Swift 6 has a native type that reaches it,
    // with no BigInteger-style fallback needed.
    let src = schema(
        "struct Wide: u128 {\n\
             id: u128,\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Wide;\n\
         }",
    );
    let file = file_of(&src, "wide");
    assert_contains(&file, "var id_: UInt128");
    assert_contains(&file, "id_: UInt128 = 0");
    assert_contains(&file, "static let size: Int = 16");
    assert_compiles(&file, "int128");
}

#[test]
fn a_signed_value_at_128_bits_keeps_its_sign_in_int128() {
    let src = schema("struct Wide: u96 { v: i96, }");
    let file = file_of(&src, "wide96");
    assert_contains(&file, "var v: Int128");
    assert_contains(&file, "v: Int128 = 0");
    assert_contains(&file, "defgenWirePattern(try defgenCheckInt(Int128(v), 96, \"Wide.v\"), 96)");
    assert_contains(&file, "v: Int128(defgenSext(bits.get(off, 96), 96))");
    assert_compiles(&file, "wide_signed");
}

#[test]
fn an_alias_keeps_the_name_the_author_declared() {
    // §3: an alias generates no runtime type, but the domain name survives.
    let file = example_file();
    assert_contains(&file, "typealias Volume = UInt8");
    assert_contains(&file, "var volume: Volume");
    assert_contains(&file, "volume: Volume = 0");
}

/// A raw `f32`/`f64` field (§2) is carried as `Float`/`Double` and packed
/// through its IEEE-754 `bitPattern`, the same way `scaled` already reaches
/// the wire. No `swiftc` in this environment to compile-check it, so this
/// only pins the shape; the C, Python and Java backends carry the executable
/// version of the same round trip (§13).
#[test]
fn raw_floats_are_carried_as_float_and_double() {
    let src = schema("struct Floats: u96 { a: f32, b: f64, }");
    let file = file_of(&src, "floats");
    assert_contains(&file, "var a: Float");
    assert_contains(&file, "var b: Double");
    assert_contains(&file, "a: Float = 0.0,");
    assert_contains(&file, "b: Double = 0.0");
    assert_contains(&file, "UInt128(a.bitPattern)");
    assert_contains(&file, "UInt128(b.bitPattern)");
    assert_contains(&file, "Float(bitPattern: UInt32(bits.get(off, 32)))");
    assert_contains(&file, "Double(bitPattern: UInt64(bits.get((off + 32), 64)))");
}

#[test]
fn a_scaled_type_exposes_both_representations() {
    // §4: the physical value to callers, the raw integer for exact round trips.
    let file = example_file();
    assert_contains(&file, "typealias Temperature = Float");
    assert_contains(&file, "let TEMPERATURE_SCALE: Float = Float(0.01)");
    assert_contains(&file, "let TEMPERATURE_OFFSET: Float = Float(0.0)");
    assert_contains(&file, "func temperatureFromRaw(_ raw: Int16) -> Temperature");
    assert_contains(&file, "func temperatureToRaw(_ value: Temperature) throws -> Int16");
    assert_contains(&file, "DefgenError.range");
}

#[test]
fn constants_become_top_level_lets() {
    // §3.1: no wire form, no codec — just a named value.
    let file = file_of(
        &schema("const MaxRetries: u8 = 5;\nconst MinTemperature: i16 = -40;\nconst Big: u128 = 5;"),
        "s",
    );
    assert_contains(&file, "let MAX_RETRIES: UInt8 = 5");
    assert_contains(&file, "let MIN_TEMPERATURE: Int16 = -40");
    // Swift 6's native 128-bit integers need no arbitrary-precision fallback.
    assert_contains(&file, "let BIG: UInt128 = 5");
}

#[test]
fn scaled_rounding_rounds_half_away_from_zero() {
    // §4, §13: every backend has to agree on a raw integer down to the last
    // unit — Swift's own `.rounded(_:)` supports this rule directly, unlike
    // the JVM's `Math.round` (rounds half *up*) or a hand-rolled algorithm.
    let file = example_file();
    assert_contains(&file, "value.rounded(.toNearestOrAwayFromZero)");
}

#[test]
fn a_closed_enum_is_rawrepresentable_and_rejects_an_unmatched_value() {
    // §5, §12: a closed enum needs no sum-type case for "unknown" at all —
    // `init?(rawValue:)` already expresses "no such variant" as failure.
    let src = schema("enum Mode: u8 { A = 0, Bravo = 1, }\nstruct S: u8 { m: Mode, }");
    let file = file_of(&src, "closed");
    assert_contains(&file, "enum Mode: UInt8 {");
    assert_contains(&file, "case a = 0");
    assert_contains(&file, "case bravo = 1");
    assert_contains(&file, "guard let value = Mode(rawValue: raw) else {");
    assert_contains(&file, "throw DefgenError.unknownValue(\"Mode: \\(raw) matches no declared variant\")");
    assert!(!file.contains("case unknown("), "a closed enum needs no fallback case");
    assert_compiles(&file, "closed_enum");
}

#[test]
fn an_implicitly_numbered_enum_gets_the_values_the_checker_resolved() {
    // §5: the counter advances from the last explicit value it saw.
    let src = schema(
        "enum E: u8 { A, B = 7, C, D, }\nalias Bound = E;\nservice S(uuid: \"180a\") {\n    characteristic C(uuid: \"2a00\", properties: [read]): Bound;\n}",
    );
    let body = type_body(&file_of(&src, "e"), "E");
    for member in ["case a = 0", "case b = 7", "case c = 8", "case d = 9"] {
        assert_contains(&body, member);
    }
}

#[test]
fn an_open_enum_covers_every_wire_value_as_a_sum_type() {
    // §5, §12: the unknown case carries the raw value, so a decoded value can
    // never be confused with a declared one, and matching on it is
    // exhaustive `switch` with no separate "is it known?" check needed.
    let file = example_file();
    assert_contains(&file, "enum HearingMode: Equatable {");
    assert_contains(&file, "case default_");
    assert_contains(&file, "case stereo");
    assert_contains(&file, "case unknown(raw: UInt8)");
    let body = type_body(&file, "HearingMode");
    assert_contains(&body, "default: return .unknown(raw: raw)");
    assert!(!body.contains("throw DefgenError.unknownValue"), "an open enum must not reject any wire value");
    // A value it keeps has to be re-encodable, or the round trip is lossy.
    assert_contains(&body, "case .unknown(let raw): return raw");
}

#[test]
fn a_tagged_union_becomes_an_enum_with_one_case_per_variant() {
    // §7, §12: cases carry their fields as labeled associated values, so a
    // decoded command is matched with `switch`/`case let`, never by reading a
    // tag by hand.
    let file = example_file();
    assert_contains(&file, "enum Command: Equatable {");
    assert_contains(&file, "case setVolume(volume: Volume)");
    assert_contains(&file, "case triggerFactoryReset");
    assert_contains(&file, "case unknown(id: UInt16, raw: UInt64)");
    let body = type_body(&file, "Command");
    assert_contains(&body, "case .triggerFactoryReset:");
    assert_contains(&body, "bits.put(off, 16, 65535)");
    // A payload-less variant's decode returns the bare case, never `case()`.
    assert_contains(&body, "return .triggerFactoryReset\n");
    assert!(!body.contains("triggerFactoryReset()"), "a case with no associated values is not a function");
}

#[test]
fn a_closed_union_rejects_an_unrecognized_id() {
    // §7: without an `else` arm, an unknown id is a hard error, and the
    // switch needs no fallback case of its own to add.
    let src = schema(
        "enum Cmd(id: u8): u16 {\n\
             A(0x01) { x: u8 }\n\
             B(0x02)\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [write]): Cmd;\n\
         }",
    );
    let file = file_of(&src, "closed_union");
    assert!(!file.contains("case unknown("), "a closed union has no fallback case");
    assert_contains(&file, "throw DefgenError.unknownValue(");
    assert_compiles(&file, "closed_union");
}

#[test]
fn a_struct_of_only_padding_still_produces_a_legal_type() {
    // §6.2 allows a container of pure padding, and Swift has no trouble with
    // a struct that declares no properties at all.
    let src = schema("struct Blank: u8 { padding: u8, }");
    let file = file_of(&src, "blank");
    let body = type_body(&file, "Blank");
    assert_contains(&body, "static let size: Int = 1");
    assert_contains(&body, "func packFixed(_ bits: DefgenBits, _ off: Int) throws {");
    assert_contains(&body, "return Blank()");
    assert_compiles(&file, "padding_only");
}

#[test]
fn field_names_that_collide_with_swift_keywords_are_escaped() {
    // §1 does not reserve Swift's vocabulary, so the backend has to cope. The
    // schema's own spelling still has to appear in error messages, since that
    // is the name the author would go looking for.
    let src = schema("struct S: u32 { class: u8, self: u8, default: u8, is: u8, }");
    let file = file_of(&src, "keywords");
    for name in ["class_", "self_", "default_", "is_"] {
        assert_contains(&file, &format!("var {name}: UInt8"));
    }
    assert_contains(&file, "\"S.class\"");
    assert_compiles(&file, "keyword_fields");
}

#[test]
fn field_names_that_would_shadow_the_generated_api_are_escaped() {
    let src = schema(
        "struct S: u40 { encode: u8, size: u8, raw: u8, rawValue: u8, tailLen: u8, }\n\
         service Svc(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): S;\n\
         }",
    );
    let file = file_of(&src, "shadow");
    for name in ["encode_", "size_", "raw_", "rawValue_", "tailLen_"] {
        assert_contains(&file, &format!("var {name}: UInt8"));
    }
    assert_contains(&type_body(&file, "S"), "func encode() throws -> [UInt8]");
    assert_compiles(&file, "shadowing_fields");
}

#[test]
fn names_are_recased_without_mangling_acronyms() {
    let src = schema("struct HTTPProxyID: u8 { x: u8, }");
    let file = file_of(&src, "acronym");
    // A type keeps the author's spelling; only casing derived from a field
    // name (camelCase) goes through the conversion.
    assert_contains(&file, "struct HTTPProxyID: Equatable {");
    assert_compiles(&file, "acronym");

    let src = schema(
        "alias HTTPProxyID = u8;\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): HTTPProxyID;\n\
         }",
    );
    let file = file_of(&src, "acronym2");
    assert_contains(&file, "func encodeHTTPProxyID(");
    assert_contains(&file, "let HTTP_PROXY_ID_SIZE: Int = 1");
}

#[test]
fn active_profile_becomes_camel_case() {
    // §12: `snake_case` fields become camelCase properties in Swift.
    assert_contains(&example_file(), "var activeProfile: UInt8");
    assert_contains(&example_file(), "activeProfile: UInt8 = 0");
}

// ---------------------------------------------------------------------------
// Codec surface (§12)
// ---------------------------------------------------------------------------

#[test]
fn every_root_type_gets_an_encode_and_a_decode() {
    let file = example_file();
    let model = model_of(EXAMPLE);
    let roots: Vec<&defgen::model::TypeDef> = model.roots().collect();
    assert!(roots.iter().any(|t| t.name == "Status") && roots.iter().any(|t| t.name == "OwnerName"));

    for root in roots {
        match root.kind {
            defgen::model::TypeKind::Struct(_) | defgen::model::TypeKind::Union(_) => {
                let body = type_body(&file, &root.name);
                assert_contains(&body, "func encode() throws -> [UInt8]");
                assert_contains(
                    &file,
                    &format!("static func decode(_ data: [UInt8]) throws -> {}", root.name),
                );
            }
            _ => {
                assert_contains(&file, &format!("func encode{}(", root.name));
                assert_contains(&file, &format!("func decode{}(", root.name));
            }
        }
    }
}

#[test]
fn a_type_that_is_only_ever_nested_gets_no_entry_points() {
    // §8, §10: byte order is a property of the root container, so a type
    // that is only ever nested has no byte order of its own to encode in.
    let file = example_file();
    let body = type_body(&file, "Orientation");
    assert_contains(&body, "func packFixed(_ bits: DefgenBits, _ off: Int) throws {");
    assert!(!body.contains("func encode()"), "a nested-only type needs no entry point");
}

#[test]
fn sizes_are_exposed_as_static_constants() {
    let file = example_file();
    assert_contains(&type_body(&file, "Status"), "static let size: Int = 8");
    assert_contains(&type_body(&file, "Orientation"), "static let size: Int = 3");
    assert_contains(&type_body(&file, "LegacySerial"), "static let size: Int = 4");
    // §6.3: a variable-length type has a prefix and a maximum instead.
    let label = type_body(&file, "DiagnosticLabel");
    assert_contains(&label, "static let fixedSize: Int = 1");
    assert_contains(&label, "static let maxSize: Int = 25");
    assert!(!label.contains("static let size:"), "a varying size is not one number");
    assert_contains(&label, "func encodedSize() -> Int");
    // An alias has no type, so its sizes are top-level constants.
    assert_contains(&file, "let OWNER_NAME_FIXED_SIZE: Int = 0");
    assert_contains(&file, "let OWNER_NAME_MAX_SIZE: Int = 32");
}

#[test]
fn byte_order_is_resolved_per_root_container() {
    // §8: the file default, and the one struct that overrides it. Byte order
    // reaches the wire in exactly one place — where `DefgenBits` is built.
    let file = example_file();
    assert_contains(&type_body(&file, "Status"), "DefgenBits(size: 8, big: false)");
    assert_contains(&type_body(&file, "LegacySerial"), "DefgenBits(size: 4, big: true)");
    assert_contains(&type_body(&file, "LegacySerial"), "DefgenBits(data: data, big: true)");
}

#[test]
fn a_variable_length_field_is_a_string_or_an_array() {
    // §12: Swift gets the idiomatic container, not C's buffer-plus-length.
    let file = example_file();
    assert_contains(&file, "var label: String");
    assert_contains(&file, "label: String = \"\"");
    assert_contains(&file, "var samples: [Temperature]");
    assert!(!file.contains("labelLen"), "a Swift container carries its own length");
    assert_contains(&type_body(&file, "DiagnosticLabel"), "DiagnosticLabel.fixedSize + tailLen()");
}

#[test]
fn a_fixed_array_is_checked_for_its_exact_count() {
    // §6.1: a fixed array carries exactly its declared count, always.
    let file = example_file();
    assert_contains(&file, "defgenCheckCount(samples, 4, \"TemperatureLog.samples\")");
    assert_contains(&file, "defgenCheckCount(points, 2, \"MotionPath.points\")");
}

#[test]
fn declared_padding_is_validated_and_bare_padding_is_not() {
    // §6.2: `padding: uN = 0` is a claim about the wire; bare padding is not.
    let file = example_file();
    assert_contains(&file, "throw DefgenError.padding(\"MotionPath: padding at bits 48..64 is not zero\")");
    assert_eq!(
        file.matches("DefgenError.padding(\"MotionPath").count(),
        1,
        "the example declares exactly one `padding = 0` run"
    );
    // Reserved bits are neither: they are carried through untouched (§6.2).
    // The default lives in the hand-written initializer, never inline on the
    // property: a `let` property with an inline default would be excluded
    // from Swift's synthesized memberwise init entirely, which is exactly
    // what a `reserved` field must not happen to, since decode needs to set
    // it.
    assert_contains(&file, "let flags: UInt8");
    assert!(!file.contains("let flags: UInt8 = 0"), "a `let` field must not carry an inline default");
    assert_contains(&file, "flags: UInt8 = 0");
    assert_contains(&file, "flags: UInt8(bits.get((off + 60), 4))");
}

#[test]
fn an_alias_of_a_variable_length_type_is_bindable_on_its_own() {
    // §6.3: "one characteristic is just a name" needs no wrapping struct.
    let file = example_file();
    assert_contains(&file, "typealias OwnerName = String");
    assert_contains(&file, "func encodeOwnerName(_ value: OwnerName) throws -> [UInt8]");
    assert_contains(&file, "func decodeOwnerName(_ data: [UInt8]) throws -> OwnerName");
    assert!(!file.contains("struct OwnerName"), "an alias generates no runtime type");
    assert_contains(&file, "defgenDecodeUtf8(data, \"OwnerName\")");
}

#[test]
fn utf8_is_validated_rather_than_patched_up() {
    // §6.3: malformed input fails; it is never replaced with U+FFFD, which
    // would turn a transport bug into silently wrong data.
    let file = example_file();
    assert_contains(&file, "defgenValidUtf8");
    assert_contains(&file, "throw DefgenError.utf8");
}

#[test]
fn every_failure_is_a_defgen_error() {
    // §12: one enum with a case per kind, so a caller can catch the lot with
    // one `catch let error as DefgenError`.
    let file = example_file();
    assert_contains(&file, "enum DefgenError: Error, CustomStringConvertible {");
    for case in ["length", "range", "unknownValue", "padding", "utf8"] {
        assert_contains(&file, &format!("case {case}(String)"));
    }
}

#[test]
fn gatt_metadata_becomes_top_level_constants() {
    // §10: UUIDs and properties, with what to do with them left to the caller.
    let file = example_file();
    assert_contains(&file, "let HEARING_AID_CONTROL_UUID: String = \"7d8f0000-3c1a-4e8a-9b5a-000000000000\"");
    assert_contains(
        &file,
        "let HEARING_AID_CONTROL_STATUS_CHAR_UUID: String = \"7d8f0001-3c1a-4e8a-9b5a-000000000000\"",
    );
    assert_contains(&file, "enum GattProperty: Hashable {");
    assert_contains(&file, "properties: [.read, .notify]");
    assert_contains(&file, "let SERVICES: [GattService] = [HEARING_AID_CONTROL]");
}

#[test]
fn a_schema_with_no_services_emits_no_gatt_section() {
    let file = file_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!file.contains("GattProperty"));
    assert!(!file.contains("SERVICES"));
    assert_compiles(&file, "no_services");
}

// ---------------------------------------------------------------------------
// Features the worked example does not reach
// ---------------------------------------------------------------------------

#[test]
fn a_variable_length_array_tail_compiles() {
    // §6.3: the element count comes from the buffer length, so the emitter
    // has to divide rather than read a prefix.
    let src = schema(
        "struct Reading: u16 {\n\
             value: u16,\n\
         }\n\
         struct Batch {\n\
             kind: u8,\n\
             readings: Reading[max: 8],\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Batch;\n\
         }",
    );
    let file = file_of(&src, "batch");
    assert_contains(&file, "var readings: [Reading]");
    assert_contains(&file, "readings: [Reading] = []");
    assert_contains(&file, "defgenCheckMax(readings, 8, \"Batch.readings\")");
    assert_contains(&file, "guard data.count % 2 == 0 else {");
    assert_contains(&file, "let count = data.count / 2");
    assert_contains(&file, "guard count <= 8 else {");
    assert_contains(&file, "return readings.count * 2");
    // The naming bug that shadowing must avoid: a locally-named `DefgenBits`
    // inside the element loop must actually be called `bits`, since `pack`
    // and `unpack_expr` always emit that literal identifier.
    assert!(!file.contains("elemBits"), "the per-element container must be named `bits`, not `elemBits`");
    assert_compiles(&file, "var_array_tail");
}

#[test]
fn a_nested_variable_length_struct_delegates_its_tail() {
    // §6.3: variable-ness propagates to the container, still trailing.
    let src = schema(
        "struct Inner {\n\
             a: u8,\n\
             text: string(max: 4),\n\
         }\n\
         struct Outer {\n\
             b: u8,\n\
             inner: Inner,\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Outer;\n\
         }",
    );
    let file = file_of(&src, "nested");
    let outer = type_body(&file, "Outer");
    assert_contains(&outer, "static let fixedSize: Int = 2");
    assert_contains(&outer, "static let maxSize: Int = 6");
    assert_contains(&outer, "return inner.tailLen()");
    assert_contains(&outer, "return try inner.packTail(big)");
    assert_contains(&outer, "try inner.unpackTail(data, big)");
    assert!(!type_body(&file, "Inner").contains("func encode()"));
    assert_compiles(&file, "nested_var_struct");
}

#[test]
fn a_big_endian_variable_length_root_reaches_its_tail() {
    // §8 with §6.3: byte order covers the fixed prefix, and is handed to the
    // tail as well, since a tail of multi-byte elements needs it too.
    let src = schema(
        "#[endian(big)]\n\
         struct Note {\n\
             code: u16,\n\
             text: string(max: 8),\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Note;\n\
         }",
    );
    let file = file_of(&src, "note");
    let body = type_body(&file, "Note");
    assert_contains(&body, "DefgenBits(size: 2, big: true)");
    assert_contains(&body, "DefgenBits(data: Array(data[0..<fixedSize]), big: true)");
    assert_contains(&body, "try value.unpackTail(Array(data[fixedSize...]), true)");
    assert_compiles(&file, "big_endian_var");
}

#[test]
fn an_enum_bound_through_an_alias_gets_its_own_entry_points() {
    // §3 with §10: binding `Bound` makes `E` a root too — and a root needs a codec.
    let src = schema(
        "enum E: u8 { A = 1, B = 2, }\n\
         alias Bound = E;\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Bound;\n\
         }",
    );
    let file = file_of(&src, "bound");
    assert_contains(&file, "func encodeE(");
    assert_contains(&file, "func decodeE(");
    assert_contains(&file, "let E_SIZE: Int = 1");
    assert_compiles(&file, "alias_bound_enum");
}

#[test]
fn a_fixed_array_of_a_closed_enum_uses_rawvalue_with_no_range_check() {
    // A closed enum's `rawValue` is always one of its declared, compile-time
    // valid cases, so packing it needs no runtime range check the way an
    // ordinary `uN` field does.
    let src = schema(
        "enum Mode: u8 { A = 1, B = 2, }\n\
         struct S: u32 { modes: Mode[4], }\n\
         service Svc(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): S;\n\
         }",
    );
    let file = file_of(&src, "enumarray");
    assert_contains(&file, "var modes: [Mode]");
    assert_contains(&file, "modes: [Mode] = [Mode](repeating: Mode.a, count: 4)");
    assert_contains(&file, "UInt128(elemVal.rawValue)");
    assert_compiles(&file, "enum_array");
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// Code generation is total: any model the checker accepts must produce a
/// file rather than a panic.
///
/// Deleting one line at a time from the worked example is a cheap way to
/// reach model shapes nobody wrote on purpose — a service with no
/// characteristics, a union whose `else` arm is gone, a struct that lost its
/// only field — which is exactly where an emitter that assumes "there is
/// always a last field" breaks.
#[test]
fn generating_never_panics_on_a_schema_that_checked() {
    let lines: Vec<&str> = EXAMPLE.lines().collect();
    let mut generated = 0;
    for skip in 0..lines.len() {
        let mutated: String =
            lines.iter().enumerate().filter(|(i, _)| *i != skip).map(|(_, l)| format!("{l}\n")).collect();

        let compiled = defgen::compile(&mutated);
        let Some(model) = compiled.model else { continue };
        if compiled.diagnostics.iter().any(|d| d.severity == Severity::Error) {
            continue;
        }
        let file = backends::swift::SwiftBackend.generate(&model, &Options::default());
        let contents = &file.single().contents;
        assert!(!contents.is_empty(), "line {skip} produced an empty file");
        assert_eq!(
            contents.matches('{').count(),
            contents.matches('}').count(),
            "line {skip} produced unbalanced braces"
        );
        generated += 1;
    }
    assert!(generated > 10, "only {generated} mutations checked clean; the test is not exercising much");
}

// ---------------------------------------------------------------------------
// Conformance: the generated file, compiled and run
// ---------------------------------------------------------------------------

/// Compiles the worked example's generated file together with the
/// hand-written conformance fixture and runs the result. The fixture's byte
/// strings are derived from SPEC.md by hand, so this is the test that would
/// catch the emitter and the spec disagreeing.
#[test]
fn the_generated_file_round_trips_the_worked_example() {
    let Some(swiftc) = swiftc() else {
        eprintln!("skipping the Swift conformance run: no swiftc found");
        return;
    };
    let dir = scratch("conformance");
    let schema_path = dir.join("commands.swift");
    std::fs::write(&schema_path, example_file()).unwrap();

    // Swift only allows top-level *statements* (as opposed to declarations)
    // in a file named `main.swift` when compiling several files together, so
    // the fixture — which has real top-level statements (`try main()`,
    // `exit(...)`) — is staged under that name; the schema file is pure
    // declarations and keeps its own name.
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples/swift_conformance.swift");
    let fixture_path = dir.join("main.swift");
    std::fs::copy(&fixture, &fixture_path).expect("failed to stage the fixture");

    let exe = dir.join("conformance");
    let build = Command::new(&swiftc)
        .args(["-warnings-as-errors", "-o"])
        .arg(&exe)
        .arg(&schema_path)
        .arg(&fixture_path)
        .output()
        .expect("failed to run swiftc");
    assert!(
        build.status.success(),
        "the conformance fixture did not build:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = Command::new(&exe).output().expect("failed to run the conformance fixture");
    assert!(
        run.status.success(),
        "the generated codecs disagree with the spec:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
