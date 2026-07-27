//! The Kotlin backend.
//!
//! Two layers of test live here. The cheap ones read the generated text and
//! assert on what it declares — these always run. The expensive one hands the
//! generated file to a real Kotlin compiler together with
//! `tests/examples/kotlin_conformance.kt` and runs the result, which is what
//! actually pins the wire format down: a codegen bug that produces valid
//! Kotlin saying the wrong thing is invisible to string matching.
//!
//! The conformance fixture asserts the very same byte strings as its C and
//! Python counterparts. That is the point of §13 — several backends, one wire
//! format — so if these ever disagree, one of the backends is wrong.
//!
//! Anything needing `kotlinc` is skipped, loudly, when there isn't one, so
//! this file still does useful work on a machine without a Kotlin toolchain.

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
    let generated = backends::kotlin::KotlinBackend.generate(&model, &opts);
    assert_eq!(generated.files.len(), 1, "the Kotlin backend emits exactly one file");
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

/// The indented body of a top-level or nested `class`/`object`/`sealed class`
/// named `name`, so a test can say "this class declares this member" without
/// a match somewhere else in the file counting.
fn class_body(file: &str, name: &str) -> String {
    // `.contains` rather than `starts_with`, since the declaration itself may
    // be prefixed with `data `/`sealed `/`enum ` — and brace-counting rather
    // than indentation, since a nested variant's own body is indented deeper
    // than its enclosing sealed class.
    let markers = [format!("class {name}("), format!("class {name} {{"), format!("object {name} ")];
    let lines: Vec<&str> = file.lines().collect();
    let start = lines
        .iter()
        .position(|l| markers.iter().any(|m| l.contains(m.as_str())))
        .unwrap_or_else(|| panic!("generated file declares no `{name}`"));

    // Where the class body actually opens: either the marker line ends with
    // `{` itself (a single-line header — `object`/`sealed class`/a no-field
    // `class`), or it opens a multi-line constructor, in which case the body
    // starts at the `) {` that closes it. A default-value lambda
    // (`List(4) { 0.0f }`) balances its own braces earlier in the constructor
    // and must not be mistaken for that opening brace.
    let body_start = if lines[start].trim_end().ends_with('{') {
        start
    } else {
        lines[start..]
            .iter()
            .position(|l| l.trim() == ") {")
            .map(|i| start + i)
            .unwrap_or_else(|| panic!("`{name}`'s constructor never closes"))
    };

    let mut depth = 0i32;
    let mut out: Vec<&str> = lines[start..body_start].to_vec();
    for line in &lines[body_start..] {
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
// Kotlin toolchain
// ---------------------------------------------------------------------------

/// The Kotlin compiler to test against, or `None` if this machine has none.
fn kotlinc() -> Option<String> {
    let candidates = std::env::var("KOTLINC").into_iter().chain(["kotlinc".into()]);
    candidates.into_iter().find(|k| Command::new(k).arg("-version").output().is_ok())
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("kotlin_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Compiles a generated file on its own, so a schema feature that never
/// appears in the worked example still has to produce Kotlin a compiler
/// accepts. `-Werror` is deliberate: generated code that warns is generated
/// code nobody wants in their build.
fn assert_compiles(file: &str, name: &str) {
    let Some(kotlinc) = kotlinc() else {
        eprintln!("skipping `{name}`: no kotlinc found");
        return;
    };
    let dir = scratch(name);
    let src = dir.join("schema.kt");
    std::fs::write(&src, file).unwrap();

    let out = Command::new(&kotlinc)
        .args(["-Werror", "-d"])
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("failed to run kotlinc");
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
fn the_kotlin_backend_is_registered_under_kotlin() {
    assert!(backends::names().contains(&"kotlin"));
    let backend = backends::find("kotlin").expect("`kotlin` resolves to a backend");
    assert_eq!(backend.name(), "kotlin");
    assert!(!backend.description().is_empty());
    assert!(backends::find("Kotlin").is_none(), "backend names are matched exactly");
    assert!(backends::find("kt").is_none(), "and only under the name they registered");
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
    assert_eq!(name, "commands.kt");

    // A stem that is not a legal Kotlin file name still has to produce one.
    assert_eq!(generate(EXAMPLE, "my-schema.v2").0, "my_schema_v2.kt");
    assert_eq!(generate(EXAMPLE, "2fast").0, "_2fast.kt");
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
    assert_contains(&file, "import java.math.BigInteger");
    assert!(!file.contains("package "), "a generated file has no package declaration");
    assert!(!file.contains("import kotlinx"), "and pulls in nothing beyond the JDK");
}

#[test]
fn rounding_only_imports_bigdecimal_when_a_scaled_type_needs_it() {
    let file = example_file();
    assert_contains(&file, "import java.math.BigDecimal");
    assert_contains(&file, "private fun defgenRound(value: Double, where_: String): BigInteger {");

    let plain = file_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!plain.contains("BigDecimal"), "no `scaled` type means no rounding helper");
    assert!(!plain.contains("defgenRound"));
}

#[test]
fn doc_comments_become_kdoc() {
    let file = example_file();
    assert_contains(&file, " * Playback volume. The device only has 4 bits of resolution.");
    assert_contains(&file, " * Reusable 3-axis orientation reading");
}

#[test]
fn a_doc_comment_cannot_close_its_own_kdoc() {
    let src = schema("/// ends a comment */ right here\nstruct S: u8 { x: u8, }");
    let file = file_of(&src, "s");
    assert!(!file.contains("*/ right here"), "`*/` inside a doc comment must be escaped");
    assert_compiles(&file, "doc_comment_escape");
}

// ---------------------------------------------------------------------------
// Type mapping (§2, §3, §4, §5, §6, §7)
// ---------------------------------------------------------------------------

#[test]
fn every_integer_width_is_the_smallest_carrier_that_holds_it() {
    // §2: unsigned for `uN`, signed for `iN`, widened up to the next native
    // size — 8, 16, 32, 64 — and to `BigInteger` only past 64.
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
    assert_contains(&file, "var a: UByte");
    assert_contains(&file, "var b: UByte");
    assert_contains(&file, "var c: UShort");
    assert_contains(&file, "var e: ULong");
    assert_contains(&file, "var f: Byte");
    assert_contains(&file, "var h: Long");
    assert_contains(&file, "defgenCheckUInt(BigInteger(a.toString()), 1, \"Widths.a\")");
    assert_contains(&file, "defgenCheckUInt(BigInteger(c.toString()), 9, \"Widths.c\")");
    assert_contains(&file, "defgenCheckInt(BigInteger.valueOf(f.toLong()), 8, \"Widths.f\")");
    // A signed value is sign-extended back out of its declared width, never
    // read as the unsigned bit pattern.
    assert_contains(&file, "f = defgenSext(bits.get((off + 47), 8), 8).toByte()");
    assert_contains(&file, "h = defgenSext(bits.get((off + 55), 48), 48).toLong()");
    assert_compiles(&file, "widths");
}

#[test]
fn values_wider_than_64_bits_use_biginteger() {
    // §2: 128 bits is the ceiling; the JVM has nothing narrower than
    // `BigInteger` that reaches it.
    let src = schema(
        "struct Wide: u128 {\n\
             id: u128,\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Wide;\n\
         }",
    );
    let file = file_of(&src, "wide");
    assert_contains(&file, "var id: BigInteger = BigInteger(\"0\")");
    assert_contains(&file, "const val SIZE: Int = 16");
    assert_compiles(&file, "int128");
}

#[test]
fn a_signed_value_wider_than_64_bits_keeps_its_sign_in_biginteger() {
    // A `BigInteger` carrier holds a negative value directly, so the default,
    // the pack and the unpack all skip the narrower-carrier round trip that a
    // 64-bit-or-under `iN` goes through.
    let src = schema("struct Wide: u96 { v: i96, }");
    let file = file_of(&src, "wide96");
    assert_contains(&file, "var v: BigInteger = BigInteger.ZERO");
    assert_contains(&file, "defgenCheckInt(v, 96, \"Wide.v\")");
    assert_contains(&file, "v = defgenSext(bits.get(off, 96), 96)");
    assert_compiles(&file, "wide_signed");
}

#[test]
fn an_alias_keeps_the_name_the_author_declared() {
    // §3: an alias generates no runtime type, but the domain name survives.
    let file = example_file();
    assert_contains(&file, "typealias Volume = UByte");
    assert_contains(&file, "var volume: Volume = 0.toUByte()");
}

#[test]
fn a_scaled_type_exposes_both_representations() {
    // §4: the physical value to callers, the raw integer for exact round trips.
    let file = example_file();
    assert_contains(&file, "typealias Temperature = Float");
    assert_contains(&file, "const val TEMPERATURE_SCALE: Float = 0.01f");
    assert_contains(&file, "const val TEMPERATURE_OFFSET: Float = 0.0f");
    assert_contains(&file, "fun temperatureFromRaw(raw: Short): Temperature");
    assert_contains(&file, "fun temperatureToRaw(value: Temperature): Short");
    assert_contains(&file, "DefgenRangeError");
}

#[test]
fn scaled_rounding_rounds_half_away_from_zero() {
    // §4, §13: the backends have to agree on a raw integer down to the last
    // unit, and the JVM's own `Math.round` rounds half *up*, which disagrees
    // with a negative tie. `defgenRound` is `private`, so this schema-level
    // sweep is the only thing pinning its shape down on the Rust side; the
    // Kotlin conformance fixture pins its *behavior* down through the public
    // `temperatureToRaw`.
    let file = example_file();
    assert_contains(&file, "private fun defgenRound(value: Double, where_: String): BigInteger {");
    assert!(!file.contains("= Math.round("), "Math.round rounds half up, which would disagree with C/Python");
    assert_contains(&file, "remainder >= 0.5 -> whole.add(BigInteger.ONE)");
    assert_contains(&file, "remainder <= -0.5 -> whole.subtract(BigInteger.ONE)");
}

#[test]
fn enum_variants_become_screaming_snake_members_when_closed() {
    // §12: casing is converted to the target language's convention — but only
    // for a closed enum's `enum class` members; an open enum's nested
    // `object`s keep the schema's own PascalCase spelling (checked below).
    let src = schema("enum Mode: u8 { A = 0, Bravo = 1, }\nstruct S: u8 { m: Mode, }");
    let body = class_body(&file_of(&src, "closed"), "Mode");
    assert_contains(&body, "A(0.toUByte())");
    assert_contains(&body, "BRAVO(1.toUByte())");
}

#[test]
fn constants_become_top_level_vals() {
    // §3.1: no wire form, no codec — just a named value.
    let file = file_of(&schema("const MaxRetries: u8 = 5;\nconst MinTemperature: i16 = -40;"), "s");
    assert_contains(&file, "val MAX_RETRIES: UByte = 5.toUByte()");
    assert_contains(&file, "val MIN_TEMPERATURE: Short = (-40).toShort()");
}

#[test]
fn an_implicitly_numbered_enum_gets_the_values_the_checker_resolved() {
    // §5: the counter advances from the last explicit value it saw.
    let src = schema(
        "enum E: u8 { A, B = 7, C, D, }\nalias Bound = E;\nservice S(uuid: \"180a\") {\n    characteristic C(uuid: \"2a00\", properties: [read]): Bound;\n}",
    );
    let body = class_body(&file_of(&src, "e"), "E");
    for member in ["A(0.toUByte())", "B(7.toUByte())", "C(8.toUByte())", "D(9.toUByte())"] {
        assert_contains(&body, member);
    }
}

#[test]
fn a_closed_enum_is_an_enum_class_that_rejects_an_unmatched_value() {
    // §5, §12: decoding an undeclared value must be fallible, and — unlike an
    // open enum — a closed one needs no sealed hierarchy at all.
    let src = schema("enum Mode: u8 { A = 0, B = 1, }\nstruct S: u8 { m: Mode, }");
    let file = file_of(&src, "closed");
    assert_contains(&file, "enum class Mode(val raw: UByte) {");
    assert_contains(&file, "throw DefgenUnknownValueError(\"Mode: $raw matches no declared variant\")");
    assert!(!file.contains("sealed class Mode"), "a closed enum needs no sealed hierarchy");
    assert!(!file.contains("class ModeUnknown"), "nor a fallback class");
    assert_compiles(&file, "closed_enum");
}

#[test]
fn an_open_enum_is_a_sealed_class_covering_every_wire_value() {
    // §5, §12: the unknown case is a distinct nested type, so it can never be
    // confused with a declared one — and unlike Python's `IntEnum` + `Union`
    // alias, the sealed class already *is* the "declared, or not" type: no
    // separate value alias is needed.
    let file = example_file();
    assert_contains(&file, "sealed class HearingMode {");
    assert_contains(&file, "object Default : HearingMode() {");
    assert_contains(&file, "object Stereo : HearingMode() {");
    assert_contains(&file, "data class Unknown(override val raw: UByte) : HearingMode()");
    let body = class_body(&file, "HearingMode");
    assert_contains(&body, "else -> Unknown(raw)");
    assert!(!body.contains("throw DefgenUnknownValueError"), "an open enum must not reject any wire value");
    // A value it keeps has to be re-encodable, or the round trip is lossy.
    assert_contains(&body, "internal fun encode(value: HearingMode): UByte = value.raw");
}

#[test]
fn a_tagged_union_becomes_a_sealed_class_with_nested_variants() {
    // §7, §12: one nested type per variant under a shared sealed base, so a
    // decoded value is matched with `is`, never by reading a tag by hand.
    let file = example_file();
    assert_contains(&file, "sealed class Command {");
    assert_contains(&file, "data class SetVolume(var volume: Volume) : Command() {");
    assert_contains(&file, "object TriggerFactoryReset : Command() {");
    assert_contains(&file, "data class Unknown(val id: UShort, val raw: ULong) : Command() {");
    // Not `const`: kotlinc rejects a `UShort`/`UByte` `const val` initializer
    // (no literal suffix exists for either, so it can only be reached through
    // a conversion call, which kotlinc does not treat as constant-foldable).
    assert_contains(&file, "val ID: UShort = 1.toUShort()");
    assert_contains(&file, "val ID: UShort = 65535.toUShort()");
    // A payload-less variant is still a type; it just carries no properties.
    let reset = class_body(&file, "TriggerFactoryReset");
    assert!(!reset.contains(": UByte"), "TriggerFactoryReset declares no payload");
    assert_contains(&reset, "bits.put(off, 16, BigInteger(\"65535\"))");
}

#[test]
fn a_union_variant_carries_the_unions_id_on_its_type_not_an_instance() {
    // §7: the id is a property of the variant's type, never of an instance,
    // so it cannot be set to something that disagrees with the type it is on.
    let file = example_file();
    let body = class_body(&file, "SetVolume");
    assert_contains(&body, "val ID: UShort = 1.toUShort()");
    assert!(!body.contains("var id"), "a known variant has no per-instance id to get wrong");
    // The fallback variant is the one exception: its id is data, by definition.
    assert_contains(&file, "data class Unknown(val id: UShort, val raw: ULong) : Command()");
}

#[test]
fn a_closed_union_rejects_an_unrecognized_id() {
    // §7: without an `else` arm, an unknown id is a hard error.
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
    assert!(!file.contains("class Unknown"), "a closed union has no fallback variant");
    assert_contains(&file, "throw DefgenUnknownValueError(");
    assert_compiles(&file, "closed_union");
}

#[test]
fn a_struct_of_only_padding_still_produces_a_legal_class() {
    // §6.2 allows a container of pure padding, and Kotlin has no trouble with
    // a class that declares no properties at all.
    let src = schema("struct Blank: u8 { padding: u8, }");
    let file = file_of(&src, "blank");
    let body = class_body(&file, "Blank");
    assert_contains(&body, "const val SIZE: Int = 1");
    assert_contains(&body, "internal fun packFixed(bits: DefgenBits, off: Int) {");
    assert_contains(&body, "return Blank()");
    assert_compiles(&file, "padding_only");
}

#[test]
fn field_names_that_collide_with_kotlin_keywords_are_escaped() {
    // §1 does not reserve Kotlin's vocabulary, so the backend has to cope. The
    // schema's own spelling still has to appear in error messages, since that
    // is the name the author would go looking for.
    let src = schema("struct S: u32 { class: u8, object: u8, when: u8, is: u8, }");
    let file = file_of(&src, "keywords");
    for name in ["class_", "object_", "when_", "is_"] {
        assert_contains(&file, &format!("var {name}: UByte"));
    }
    assert_contains(&file, "\"S.class\"");
    assert_compiles(&file, "keyword_fields");
}

#[test]
fn field_names_that_would_shadow_the_generated_api_are_escaped() {
    // A property called `encode` would collide with the method that encodes
    // it, one called `pack_fixed` would collide with the method that packs it
    // (`packFixed`, once camel-cased), and one called `copy` would collide
    // with the data class's own synthesized `copy()`.
    let src = schema(
        "struct S: u40 { encode: u8, copy: u8, pack_fixed: u8, raw: u8, toString: u8, }\n\
         service Svc(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): S;\n\
         }",
    );
    let file = file_of(&src, "shadow");
    for name in ["encode_", "copy_", "packFixed_", "raw_", "toString_"] {
        assert_contains(&file, &format!("var {name}: UByte"));
    }
    assert_contains(&class_body(&file, "S"), "fun encode(): ByteArray");
    assert_compiles(&file, "shadowing_fields");
}

#[test]
fn names_are_recased_without_mangling_acronyms() {
    let src = schema("struct HTTPProxyID: u8 { x: u8, }");
    let file = file_of(&src, "acronym");
    // A type keeps the author's spelling; only casing derived from a field
    // name (camelCase) goes through the conversion.
    assert_contains(&file, "class HTTPProxyID(");
    assert_compiles(&file, "acronym");

    let src = schema(
        "alias HTTPProxyID = u8;\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): HTTPProxyID;\n\
         }",
    );
    let file = file_of(&src, "acronym2");
    assert_contains(&file, "fun encodeHTTPProxyID(");
    assert_contains(&file, "const val HTTP_PROXY_ID_SIZE: Int = 1");
}

#[test]
fn active_profile_becomes_camel_case() {
    // §12: `snake_case` fields become camelCase properties in Kotlin.
    assert_contains(&example_file(), "var activeProfile: UByte");
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
                let body = class_body(&file, &root.name);
                assert_contains(&body, "fun encode(): ByteArray");
                assert_contains(&file, &format!("fun decode(data: ByteArray): {}", root.name));
            }
            _ => {
                assert_contains(&file, &format!("fun encode{}(", root.name));
                assert_contains(&file, &format!("fun decode{}(", root.name));
            }
        }
    }
}

#[test]
fn a_type_that_is_only_ever_nested_gets_no_entry_points() {
    // §8, §10: byte order is a property of the root container, so a type that
    // is only ever nested has no byte order of its own to encode in.
    let file = example_file();
    let body = class_body(&file, "Orientation");
    assert_contains(&body, "internal fun packFixed(bits: DefgenBits, off: Int) {");
    assert!(!body.contains("fun encode()"), "a nested-only type needs no entry point");
}

#[test]
fn sizes_are_exposed_as_companion_constants() {
    let file = example_file();
    assert_contains(&class_body(&file, "Status"), "const val SIZE: Int = 8");
    assert_contains(&class_body(&file, "Orientation"), "const val SIZE: Int = 3");
    assert_contains(&class_body(&file, "LegacySerial"), "const val SIZE: Int = 4");
    // §6.3: a variable-length type has a prefix and a maximum instead.
    let label = class_body(&file, "DiagnosticLabel");
    assert_contains(&label, "const val FIXED_SIZE: Int = 1");
    assert_contains(&label, "const val MAX_SIZE: Int = 25");
    assert!(!label.contains("const val SIZE:"), "a varying size is not one number");
    assert_contains(&label, "fun encodedSize(): Int");
    // An alias has no class, so its sizes are top-level constants.
    assert_contains(&file, "const val OWNER_NAME_FIXED_SIZE: Int = 0");
    assert_contains(&file, "const val OWNER_NAME_MAX_SIZE: Int = 32");
}

#[test]
fn byte_order_is_resolved_per_root_container() {
    // §8: the file default, and the one struct that overrides it. Byte order
    // reaches the wire in exactly one place — where the bits meet a `ByteArray`.
    let file = example_file();
    assert_contains(&class_body(&file, "Status"), "return bits.toBytes(8, big = false)");
    assert_contains(&class_body(&file, "LegacySerial"), "return bits.toBytes(4, big = true)");
    assert_contains(&class_body(&file, "LegacySerial"), "DefgenBits.fromBytes(data, big = true)");
}

#[test]
fn a_variable_length_field_is_a_string_or_a_list() {
    // §12: Kotlin gets the idiomatic container, not C's buffer-plus-length.
    let file = example_file();
    assert_contains(&file, "var label: String = \"\"");
    assert_contains(&file, "var samples: List<Temperature> = ");
    assert!(!file.contains("labelLen"), "a Kotlin container carries its own length");
    assert_contains(&class_body(&file, "DiagnosticLabel"), "fun encodedSize(): Int = FIXED_SIZE + tailLen()");
}

#[test]
fn a_default_argument_is_a_fresh_expression_per_call() {
    // Unlike a Python dataclass, a Kotlin default is not a value shared
    // between instances — so a nested struct or array field can default
    // straight to a constructor call with no factory-function indirection.
    let file = example_file();
    assert_contains(&file, "var samples: List<Temperature> = List(4) { 0.0f }");
    assert_contains(&file, "var orientation: Orientation = Orientation()");
    assert_contains(&file, "var points: List<Orientation> = List(2) { Orientation() }");
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
    assert_contains(&file, "throw DefgenPaddingError(\"MotionPath: padding at bits 48..64 is not zero\")");
    assert_eq!(
        file.matches("throw DefgenPaddingError").count(),
        1,
        "the example declares exactly one `padding = 0` run"
    );
    // Reserved bits are neither: they are carried through untouched (§6.2).
    assert_contains(&file, "val flags: UByte");
    assert_contains(&file, "flags = bits.get((off + 60), 4).toInt().toUByte()");
}

#[test]
fn an_alias_of_a_variable_length_type_is_bindable_on_its_own() {
    // §6.3: "one characteristic is just a name" needs no wrapping class.
    let file = example_file();
    assert_contains(&file, "typealias OwnerName = String");
    assert_contains(&file, "fun encodeOwnerName(value: OwnerName): ByteArray");
    assert_contains(&file, "fun decodeOwnerName(data: ByteArray): OwnerName");
    assert!(!file.contains("class OwnerName"), "an alias generates no runtime type");
    assert_contains(&file, "defgenDecodeUtf8(data, \"OwnerName\")");
}

#[test]
fn utf8_is_validated_rather_than_patched_up() {
    // §6.3: malformed input fails; it is never replaced with U+FFFD, which
    // would turn a transport bug into silently wrong data.
    let file = example_file();
    assert_contains(&file, "CodingErrorAction.REPORT");
    assert_contains(&file, "throw DefgenUtf8Error");
}

#[test]
fn every_failure_is_a_defgen_error() {
    // §12: one sealed base class, so a caller can catch the lot with one `catch`.
    let file = example_file();
    assert_contains(&file, "sealed class DefgenError(message: String) : Exception(message)");
    for sub in ["Length", "Range", "UnknownValue", "Padding", "Utf8"] {
        assert_contains(&file, &format!("class Defgen{sub}Error(message: String) : DefgenError(message)"));
    }
}

#[test]
fn gatt_metadata_becomes_top_level_constants() {
    // §10: UUIDs and properties, with what to do with them left to the caller.
    let file = example_file();
    assert_contains(
        &file,
        "const val HEARING_AID_CONTROL_UUID: String = \"7d8f0000-3c1a-4e8a-9b5a-000000000000\"",
    );
    assert_contains(
        &file,
        "const val HEARING_AID_CONTROL_STATUS_CHAR_UUID: String = \"7d8f0001-3c1a-4e8a-9b5a-000000000000\"",
    );
    assert_contains(&file, "enum class GattProperty {");
    assert_contains(&file, "properties = setOf(GattProperty.READ, GattProperty.NOTIFY),");
    assert_contains(&file, "val SERVICES: List<GattService> = listOf(HEARING_AID_CONTROL)");
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
    // §6.3: the element count comes from the buffer length, so the emitter has
    // to divide rather than read a prefix.
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
    assert_contains(&file, "var readings: List<Reading> = emptyList()");
    assert_contains(&file, "defgenCheckMax(readings, 8, \"Batch.readings\")");
    assert_contains(&file, "if (data.size % 2 != 0) {");
    assert_contains(&file, "val count = data.size / 2");
    assert_contains(&file, "if (count > 8) {");
    assert_contains(&file, "return readings.size * 2");
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
    let outer = class_body(&file, "Outer");
    assert_contains(&outer, "const val FIXED_SIZE: Int = 2");
    assert_contains(&outer, "const val MAX_SIZE: Int = 6");
    assert_contains(&outer, "return inner.tailLen()");
    assert_contains(&outer, "return inner.packTail(big)");
    assert_contains(&outer, "inner.unpackTail(data, big)");
    assert!(!class_body(&file, "Inner").contains("fun encode()"));
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
    let body = class_body(&file, "Note");
    assert_contains(&body, "val prefix = bits.toBytes(2, big = true)");
    assert_contains(&body, "DefgenBits.fromBytes(data.copyOfRange(0, FIXED_SIZE), big = true)");
    assert_contains(&body, "value.unpackTail(data.copyOfRange(FIXED_SIZE, data.size), true)");
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
    assert_contains(&file, "fun encodeE(");
    assert_contains(&file, "fun decodeE(");
    assert_contains(&file, "const val E_SIZE: Int = 1");
    assert_compiles(&file, "alias_bound_enum");
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// Code generation is total: any model the checker accepts must produce a
/// file rather than a panic.
///
/// Deleting one line at a time from the worked example is a cheap way to reach
/// model shapes nobody wrote on purpose — a service with no characteristics, a
/// union whose `else` arm is gone, a struct that lost its only field — which is
/// exactly where an emitter that assumes "there is always a last field" breaks.
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
        let file = backends::kotlin::KotlinBackend.generate(&model, &Options::default());
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
    let Some(kotlinc) = kotlinc() else {
        eprintln!("skipping the Kotlin conformance run: no kotlinc found");
        return;
    };
    let dir = scratch("conformance");
    let schema_path = dir.join("commands.kt");
    std::fs::write(&schema_path, example_file()).unwrap();

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples/kotlin_conformance.kt");
    let fixture_path = dir.join("conformance.kt");
    std::fs::copy(&fixture, &fixture_path).expect("failed to stage the fixture");

    let jar = dir.join("conformance.jar");
    let build = Command::new(&kotlinc)
        .args(["-include-runtime", "-d"])
        .arg(&jar)
        .arg(&schema_path)
        .arg(&fixture_path)
        .output()
        .expect("failed to run kotlinc");
    assert!(
        build.status.success(),
        "the conformance fixture did not build:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = Command::new("java")
        .args(["-jar"])
        .arg(&jar)
        .output()
        .expect("failed to run the conformance fixture");
    assert!(
        run.status.success(),
        "the generated codecs disagree with the spec:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
