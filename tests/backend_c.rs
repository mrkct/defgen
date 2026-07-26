//! The C backend.
//!
//! Two layers of test live here. The cheap ones read the generated text and
//! assert on what it declares. The expensive one hands the header to a real C
//! compiler and runs `tests/examples/c_conformance.c` against it, which is what
//! actually pins the wire format down — a codegen bug that produces valid C
//! saying the wrong thing is invisible to string matching.
//!
//! Anything needing a compiler is skipped, loudly, when there isn't one, so
//! this file still does useful work on a machine without a toolchain.

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

/// Generates a header from `src`, as the CLI would for a file named `stem`.
fn header_of(src: &str, stem: &str) -> String {
    generate(src, stem).1
}

/// The `(file name, contents)` the backend produced.
fn generate(src: &str, stem: &str) -> (String, String) {
    let model = model_of(src);
    let opts = Options { stem: stem.to_string(), source: Some(format!("{stem}.defs")) };
    let generated = backends::c::CBackend.generate(&model, &opts);
    assert_eq!(generated.files.len(), 1, "the C backend emits exactly one file");
    let file = generated.single();
    (file.name.clone(), file.contents.clone())
}

fn example_header() -> String {
    header_of(EXAMPLE, "commands")
}

/// A schema with just enough header to be legal, so a test can be one struct.
fn schema(body: &str) -> String {
    format!("version = 1;\nendian: little;\n---\n{body}")
}

fn assert_contains(header: &str, needle: &str) {
    assert!(header.contains(needle), "generated header is missing `{needle}`");
}

// ---------------------------------------------------------------------------
// C toolchain
// ---------------------------------------------------------------------------

/// The C compiler to test against, or `None` if this machine has none.
fn cc() -> Option<String> {
    let candidates = std::env::var("CC").into_iter().chain(["cc".into(), "gcc".into(), "clang".into()]);
    candidates
        .into_iter()
        .find(|c| Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success()))
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Compiles a header on its own, so a schema feature that never appears in the
/// worked example still has to produce C that a compiler accepts.
///
/// `-Wall -Wextra -pedantic` is deliberate: generated code that warns is
/// generated code nobody wants in their build.
fn assert_compiles(header: &str, name: &str) {
    let Some(cc) = cc() else {
        eprintln!("skipping `{name}`: no C compiler found");
        return;
    };
    let dir = scratch(name);
    let header_path = dir.join("schema.h");
    let main_path = dir.join("main.c");
    std::fs::write(&header_path, header).unwrap();
    std::fs::write(&main_path, "#include \"schema.h\"\nint main(void) { return 0; }\n").unwrap();

    let out = Command::new(&cc)
        .args(["-std=c99", "-Wall", "-Wextra", "-pedantic", "-Werror", "-fsyntax-only"])
        .arg(format!("-I{}", dir.display()))
        .arg(&main_path)
        .output()
        .expect("failed to run the C compiler");

    assert!(
        out.status.success(),
        "generated header does not compile cleanly:\n{}\n--- header ---\n{header}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Registry (SPEC.md §13 — one backend per target language)
// ---------------------------------------------------------------------------

#[test]
fn the_c_backend_is_registered_under_c() {
    assert!(backends::names().contains(&"c"));
    let backend = backends::find("c").expect("`c` resolves to a backend");
    assert_eq!(backend.name(), "c");
    assert!(!backend.description().is_empty());
}

#[test]
fn an_unknown_backend_name_resolves_to_nothing() {
    assert!(backends::find("cobol").is_none());
    assert!(backends::find("C").is_none(), "backend names are matched exactly");
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
fn the_file_stem_drives_the_file_name_and_include_guard() {
    let (name, header) = generate(EXAMPLE, "commands");
    assert_eq!(name, "commands.h");
    assert_contains(&header, "#ifndef COMMANDS_H");
    assert_contains(&header, "#define COMMANDS_H");
    assert_contains(&header, "#endif /* COMMANDS_H */");

    // A stem that is not a legal C identifier still has to produce one, for
    // both the guard and the file name.
    let (name, header) = generate(EXAMPLE, "my-schema.v2");
    assert_eq!(name, "my_schema_v2.h");
    assert_contains(&header, "#ifndef MY_SCHEMA_V2_H");

    let opts = Options::for_path(Path::new("/tmp/ble/device.defs"));
    assert_eq!(opts.stem, "device");
    assert_eq!(opts.source.as_deref(), Some("/tmp/ble/device.defs"));
}

#[test]
fn generation_is_deterministic() {
    assert_eq!(example_header(), example_header());
}

// ---------------------------------------------------------------------------
// Header shape
// ---------------------------------------------------------------------------

#[test]
fn the_header_is_self_contained() {
    let header = example_header();
    assert_contains(&header, "#include <stdint.h>");
    assert_contains(&header, "#include <stdbool.h>");
    assert_contains(&header, "#include <stddef.h>");
    assert_contains(&header, "#include <string.h>");
    assert_contains(&header, "extern \"C\" {");
    assert!(!header.contains("#include \""), "a generated header pulls in no siblings");
}

#[test]
fn nothing_generated_ever_needs_libm() {
    // §4 needs rounding, and the example exercises it — but a generated header
    // carries its own `round()` rather than making every consumer link -lm,
    // which on a bare-metal target may not exist at all.
    let header = example_header();
    assert!(!header.contains("#include <math.h>"), "generated C must not depend on libm");
    assert_contains(&header, "static inline double defgen__round(double v) {");
    assert_contains(&header, "double r = defgen__round(");

    // and a schema with no `scaled` type does not carry the helper either.
    let plain = header_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!plain.contains("defgen__round"), "the helper is only emitted where it is used");
}

#[test]
fn the_rounding_helper_takes_the_integer_part_before_comparing() {
    // `(int64_t)(v + 0.5)` is not `round()`: adding 0.5 to the double just
    // below 0.5 rounds up, so it turns 0.49999999999999994 into 1 — and it is
    // undefined behaviour once v exceeds the integer type's range. Taking the
    // integer part first is what makes the helper exact, and §14 needs it
    // exact, because the Python backend has to agree with it unit for unit.
    //
    // `c_conformance.c` is where the resulting values are actually checked;
    // this only pins the shape that makes them right.
    let header = example_header();
    assert_contains(&header, "if (!(a < 4503599627370496.0)) return v;");
    assert_contains(&header, "if (a - w >= 0.5) w += 1.0;");
}

#[test]
fn the_version_pragma_becomes_a_constant() {
    // SPEC.md §11: applications log or branch on the schema version.
    assert_contains(&example_header(), "#define DEFGEN_SCHEMA_VERSION 2");
}

#[test]
fn doc_comments_become_doxygen() {
    let header = example_header();
    assert_contains(&header, " * Playback volume. The device only has 4 bits of resolution.");
    assert_contains(&header, " * Reusable 3-axis orientation reading");
}

#[test]
fn a_doc_comment_cannot_close_its_own_block() {
    let src = schema("/// ends a block */ right here\nstruct S: u8 { x: u8, }");
    let header = header_of(&src, "s");
    assert!(!header.contains("*/ right here"), "`*/` inside a doc comment must be escaped");
    assert_compiles(&header, "doc_comment_escape");
}

// ---------------------------------------------------------------------------
// Type mapping (§2, §3, §4, §5, §6, §7)
// ---------------------------------------------------------------------------

#[test]
fn values_are_carried_in_the_smallest_native_integer() {
    // §2: 8, 16, 32, 64 or 128 bits, signed for `iN`.
    let src = schema(
        "struct Widths: u256 {\n\
             a: u1,\n\
             b: u4,\n\
             c: u9,\n\
             d: u12,\n\
             e: u33,\n\
             f: i8,\n\
             g: i12,\n\
             h: i48,\n\
             padding: u129,\n\
         }",
    );
    let header = header_of(&src, "widths");
    assert_contains(&header, "uint8_t a;");
    assert_contains(&header, "uint8_t b;");
    assert_contains(&header, "uint16_t c;");
    assert_contains(&header, "uint16_t d;");
    assert_contains(&header, "uint64_t e;");
    assert_contains(&header, "int8_t f;");
    assert_contains(&header, "int16_t g;");
    assert_contains(&header, "int64_t h;");
    assert_compiles(&header, "widths");
}

#[test]
fn an_alias_keeps_the_name_the_author_declared() {
    // §3: an alias generates no runtime type, but the domain name survives.
    let header = example_header();
    assert_contains(&header, "typedef uint8_t Volume;");
    assert_contains(&header, "    Volume volume;");
}

#[test]
fn a_scaled_type_exposes_both_representations() {
    // §4: the physical value to callers, the raw integer for exact round trips.
    let header = example_header();
    assert_contains(&header, "typedef float Temperature;");
    assert_contains(&header, "typedef int16_t TemperatureRaw;");
    assert_contains(&header, "#define TEMPERATURE_SCALE 0.01");
    assert_contains(&header, "#define TEMPERATURE_OFFSET 0.0");
    assert_contains(&header, "static inline Temperature temperature_from_raw(TemperatureRaw raw)");
    assert_contains(&header, "static inline defgen_err_t temperature_to_raw(Temperature v,");
    assert_contains(&header, "return DEFGEN_ERR_RANGE;");
}

#[test]
fn enum_variants_become_screaming_snake_constants() {
    // §13: casing is converted to the target language's convention.
    let header = example_header();
    assert_contains(&header, "typedef uint8_t HearingMode;");
    assert_contains(&header, "#define HEARING_MODE_DEFAULT ((HearingMode)UINT64_C(0))");
    assert_contains(&header, "#define HEARING_MODE_CINEMA ((HearingMode)UINT64_C(3))");
    assert_contains(&header, "static inline bool hearing_mode_is_known(HearingMode v)");
    assert_contains(&header, "static inline const char *hearing_mode_name(HearingMode v)");
}

#[test]
fn an_implicitly_numbered_enum_gets_the_values_the_checker_resolved() {
    // §5: the counter advances from the last explicit value it saw.
    let src = schema(
        "enum E: u8 { A, B = 7, C, D, }\nalias Bound = E;\nservice S(uuid: \"180a\") {\n    characteristic C(uuid: \"2a00\", properties: [read]): Bound;\n}",
    );
    let header = header_of(&src, "e");
    assert_contains(&header, "#define E_A ((E)UINT64_C(0))");
    assert_contains(&header, "#define E_B ((E)UINT64_C(7))");
    assert_contains(&header, "#define E_C ((E)UINT64_C(8))");
    assert_contains(&header, "#define E_D ((E)UINT64_C(9))");
}

#[test]
fn a_closed_enum_rejects_an_unmatched_value_on_both_sides() {
    // §5, §13: decoding an undeclared value must be fallible.
    let src = schema("enum Mode: u8 { A = 0, B = 1, }\nstruct S: u8 { m: Mode, }");
    let header = header_of(&src, "closed");
    let occurrences = header.matches("if (!mode_is_known(").count();
    assert_eq!(occurrences, 2, "a closed enum is validated on encode and on decode");
    assert_contains(&header, "return DEFGEN_ERR_UNKNOWN_VALUE;");
    assert_compiles(&header, "closed_enum");
}

#[test]
fn an_open_enum_never_fails_to_decode() {
    // §5: an `else` arm means decoding this enum cannot fail.
    let header = example_header();
    assert!(!header.contains("if (!hearing_mode_is_known("), "an open enum must not reject any wire value");
}

#[test]
fn a_tagged_union_becomes_a_discriminant_plus_a_c_union() {
    // §7, §13: the unknown case is a distinct variant carrying `raw`.
    let header = example_header();
    assert_contains(&header, "#define COMMAND_SET_VOLUME ((uint16_t)UINT64_C(0x1))");
    assert_contains(&header, "#define COMMAND_TRIGGER_FACTORY_RESET ((uint16_t)UINT64_C(0xffff))");
    assert_contains(&header, "    uint16_t id;");
    assert_contains(&header, "        } set_volume;");
    assert_contains(&header, "        } unknown;");
    assert_contains(&header, "uint64_t raw; /* 48 bits */");
    // `TriggerFactoryReset` has no payload, so it gets no union member.
    assert!(!header.contains("} trigger_factory_reset;"));
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
    let header = header_of(&src, "closed_union");
    assert!(!header.contains("payload.unknown"), "a closed union has no fallback member");
    assert_contains(&header, "default:");
    assert_contains(&header, "return DEFGEN_ERR_UNKNOWN_VALUE;");
    assert_compiles(&header, "closed_union");
}

#[test]
fn a_struct_of_only_padding_still_produces_a_legal_c_struct() {
    // C forbids an empty struct, and §6.2 allows a container of pure padding.
    let src = schema("struct Blank: u8 { padding: u8, }");
    let header = header_of(&src, "blank");
    assert_contains(&header, "char _unused;");
    assert_compiles(&header, "padding_only");
}

#[test]
fn field_names_that_collide_with_c_keywords_are_escaped() {
    // §1 does not reserve C's vocabulary, so the backend has to cope.
    let src = schema("struct S: u24 { int: u8, default: u8, register: u8, }");
    let header = header_of(&src, "keywords");
    assert_contains(&header, "uint8_t int_;");
    assert_contains(&header, "uint8_t default_;");
    assert_contains(&header, "uint8_t register_;");
    assert_compiles(&header, "keyword_fields");
}

#[test]
fn names_are_recased_without_mangling_acronyms() {
    let src = schema("struct HTTPProxyID: u8 { x: u8, }");
    let header = header_of(&src, "acronym");
    assert_contains(&header, "#define HTTP_PROXY_ID_SIZE 1u");
    assert_contains(&header, "http_proxy_id__pack_fixed");
}

// ---------------------------------------------------------------------------
// Codec surface (§13)
// ---------------------------------------------------------------------------

#[test]
fn every_root_type_gets_an_encode_and_a_decode() {
    let header = example_header();
    let model = model_of(EXAMPLE);
    let roots: Vec<&str> = model.roots().map(|t| t.name.as_str()).collect();
    assert!(roots.contains(&"Status") && roots.contains(&"OwnerName"));

    for root in roots {
        let fnp = root
            .chars()
            .enumerate()
            .flat_map(|(i, c)| {
                let sep = i > 0 && c.is_ascii_uppercase();
                sep.then_some('_').into_iter().chain(std::iter::once(c.to_ascii_lowercase()))
            })
            .collect::<String>();
        assert_contains(&header, &format!("{fnp}_encode("));
        assert_contains(&header, &format!("{fnp}_decode("));
    }
}

#[test]
fn a_type_that_is_only_ever_nested_gets_no_entry_points() {
    // §10: binding is additive; `Orientation` is never bound.
    let header = example_header();
    assert_contains(&header, "orientation__pack_fixed");
    assert!(!header.contains("orientation_encode("), "a nested-only type needs no entry point");
}

#[test]
fn sizes_are_exposed_as_constants() {
    let header = example_header();
    assert_contains(&header, "#define STATUS_SIZE 8u");
    assert_contains(&header, "#define ORIENTATION_SIZE 3u");
    assert_contains(&header, "#define LEGACY_SERIAL_SIZE 4u");
    // §6.3: a variable-length type has a prefix and a maximum instead.
    assert_contains(&header, "#define DIAGNOSTIC_LABEL_FIXED_SIZE 1u");
    assert_contains(&header, "#define DIAGNOSTIC_LABEL_MAX_SIZE 25u");
    assert!(!header.contains("#define DIAGNOSTIC_LABEL_SIZE"));
}

#[test]
fn byte_order_is_resolved_per_root_container() {
    // §8: the file default, and the one struct that overrides it.
    let header = example_header();
    assert_contains(&header, "status__pack_fixed(v, buf, STATUS_SIZE, 0, 0u)");
    assert_contains(&header, "legacy_serial__pack_fixed(v, buf, LEGACY_SERIAL_SIZE, 1, 0u)");
}

#[test]
fn a_variable_length_field_is_a_fixed_buffer_plus_a_length() {
    // §13: C gets no dynamically-allocated string or array.
    let header = example_header();
    assert_contains(&header, "char label[24];");
    assert_contains(&header, "size_t label_len;");
    assert!(!header.contains("malloc"), "generated C must not allocate");
    assert!(!header.contains("strcpy"), "lengths are explicit, so no NUL scanning");
}

#[test]
fn an_alias_of_a_variable_length_type_is_bindable_on_its_own() {
    // §6.3: "one characteristic is just a name" needs no wrapping struct.
    let header = example_header();
    assert_contains(&header, "char data[32];");
    assert_contains(&header, "} OwnerName;");
    assert_contains(&header, "owner_name_encode(");
    assert_contains(&header, "defgen__utf8_valid");
}

#[test]
fn gatt_metadata_becomes_constants() {
    // §10: UUIDs and properties, with what to do with them left to the caller.
    let header = example_header();
    assert_contains(
        &header,
        "#define HEARING_AID_CONTROL_SERVICE_UUID \"7d8f0000-3c1a-4e8a-9b5a-000000000000\"",
    );
    assert_contains(&header, "#define HEARING_AID_CONTROL_STATUS_CHAR_UUID");
    assert_contains(
        &header,
        "#define HEARING_AID_CONTROL_STATUS_CHAR_PROPERTIES (DEFGEN_PROP_READ | DEFGEN_PROP_NOTIFY)",
    );
}

#[test]
fn gatt_uuids_also_get_a_wire_order_byte_array_macro() {
    // A UUID's byte macro is its hex bytes reversed (little-endian wire
    // order), matching what BLE stacks actually put on the air — not the
    // order the UUID string above it is written in.
    let header = example_header();
    assert_contains(
        &header,
        "#define HEARING_AID_CONTROL_SERVICE_UUID_BYTES { 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, \
         0x5a, 0x9b, 0x8a, 0x4e, 0x1a, 0x3c, 0x00, 0x00, 0x8f, 0x7d }",
    );
    assert_contains(
        &header,
        "#define HEARING_AID_CONTROL_STATUS_CHAR_UUID_BYTES { 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, \
         0x5a, 0x9b, 0x8a, 0x4e, 0x1a, 0x3c, 0x01, 0x00, 0x8f, 0x7d }",
    );
}

#[test]
fn a_short_uuid_form_produces_a_two_byte_array() {
    // §10: the 16-bit GATT form ("2a19") stays two bytes, reversed.
    let header = header_of(
        &schema(
            "struct S: u8 { x: u8, }\n\
             service Svc(uuid: \"180a\") {\n\
                 characteristic C(uuid: \"2a19\", properties: [read]): S;\n\
             }",
        ),
        "s",
    );
    assert_contains(&header, "#define SVC_SERVICE_UUID_BYTES { 0x0a, 0x18 }");
    assert_contains(&header, "#define SVC_C_UUID_BYTES { 0x19, 0x2a }");
}

#[test]
fn a_schema_with_no_services_emits_no_gatt_section() {
    let header = header_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!header.contains("defgen_prop_t"));
    assert_contains(&header, "#endif /* S_H */");
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
    let header = header_of(&src, "batch");
    assert_contains(&header, "Reading readings[8];");
    assert_contains(&header, "size_t readings_len;");
    assert_contains(&header, "if (len % 2u != 0) return DEFGEN_ERR_LENGTH;");
    assert_contains(&header, "if (len / 2u > 8u) return DEFGEN_ERR_LENGTH;");
    assert_compiles(&header, "var_array_tail");
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
    let header = header_of(&src, "nested");
    assert_contains(&header, "#define OUTER_FIXED_SIZE 2u");
    assert_contains(&header, "#define OUTER_MAX_SIZE 6u");
    assert_contains(&header, "return inner__tail_len(&v->inner);");
    assert_contains(&header, "return inner__pack_tail(&v->inner, out, cap, big);");
    assert_compiles(&header, "nested_var_struct");
}

#[test]
fn values_wider_than_64_bits_use_int128_and_say_so() {
    // §2: 128 bits is the ceiling, and C only has it as an extension.
    let src = schema(
        "struct Wide: u128 {\n\
             id: u128,\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Wide;\n\
         }",
    );
    let header = header_of(&src, "wide");
    assert_contains(&header, "typedef unsigned __int128 defgen_u128;");
    assert_contains(&header, "#error");
    assert_contains(&header, "defgen__put_wide");
    assert_contains(&header, "defgen_u128 id;");
    assert_compiles(&header, "int128");
}

#[test]
fn int128_support_is_not_emitted_unless_a_schema_needs_it() {
    let header = header_of(&schema("struct S: u64 { x: u64, }"), "s");
    assert!(!header.contains("__int128"), "most schemas should stay strictly C99");
    assert!(!header.contains("defgen__put_wide"));
}

#[test]
fn a_narrow_signed_value_is_range_checked_at_its_declared_width() {
    // §2: an `i12` in an `int16_t` carrier can hold values the wire cannot.
    let src = schema("struct S: u12 { v: i12, }");
    let header = header_of(&src, "s");
    assert_contains(&header, "if ((v->v) < (-INT64_C(2048)) || (v->v) > INT64_C(2047))");
    assert_contains(&header, "defgen__sext(");
    assert_compiles(&header, "narrow_signed");
}

#[test]
fn an_exactly_sized_value_is_not_range_checked() {
    // A `u8` in a `uint8_t` cannot be out of range, and a dead comparison would
    // warn under -Wtype-limits.
    let header = header_of(&schema("struct S: u8 { v: u8, }"), "s");
    assert!(!header.contains("if ((v->v)"), "an exact fit needs no check");
    assert_compiles(&header, "exact_fit");
}

#[test]
fn a_big_endian_variable_length_root_compiles() {
    // §8 with §6.3: byte order covers the fixed prefix; the tail follows it.
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
    let header = header_of(&src, "note");
    assert_contains(&header, "note__pack_fixed(v, buf, NOTE_FIXED_SIZE, 1, 0u)");
    assert_compiles(&header, "big_endian_var");
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// Code generation is total: any model the checker accepts must produce a
/// header rather than a panic.
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
        let header = backends::c::CBackend.generate(&model, &Options::default());
        assert!(!header.single().contents.is_empty(), "line {skip} produced an empty header");
        generated += 1;
    }
    assert!(generated > 10, "only {generated} mutations checked clean; the test is not exercising much");
}

// ---------------------------------------------------------------------------
// Conformance: the generated header, compiled and run
// ---------------------------------------------------------------------------

/// Compiles the worked example's header together with the hand-written
/// conformance fixture and runs it. The fixture's byte strings are derived from
/// SPEC.md by hand, so this is the test that would catch the emitter and the
/// spec disagreeing.
#[test]
fn the_generated_header_round_trips_the_worked_example() {
    let Some(cc) = cc() else {
        eprintln!("skipping the C conformance run: no C compiler found");
        return;
    };
    let dir = scratch("conformance");
    let header_path = dir.join("commands.h");
    std::fs::write(&header_path, example_header()).unwrap();

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples/c_conformance.c");
    let binary = dir.join("conformance");

    let build = Command::new(&cc)
        .args(["-std=c99", "-Wall", "-Wextra", "-pedantic", "-Werror"])
        .arg(format!("-I{}", dir.display()))
        .arg("-o")
        .arg(&binary)
        .arg(&fixture)
        // Deliberately no `-lm`: if the generated header ever reaches for libm
        // again, this link fails rather than quietly acquiring a dependency.
        .output()
        .expect("failed to run the C compiler");
    assert!(
        build.status.success(),
        "the conformance fixture did not build:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = Command::new(&binary).output().expect("failed to run the conformance fixture");
    assert!(
        run.status.success(),
        "the generated codecs disagree with the spec:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
