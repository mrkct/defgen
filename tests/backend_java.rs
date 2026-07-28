//! The Java backend.
//!
//! Two layers of test live here. The cheap ones read the generated text and
//! assert on what it declares — these always run. The expensive one hands the
//! generated file to a real Java compiler together with
//! `tests/examples/java_conformance.java` and runs the result, which is what
//! actually pins the wire format down: a codegen bug that produces valid Java
//! saying the wrong thing is invisible to string matching.
//!
//! The conformance fixture asserts the very same byte strings as its C, Python
//! and Kotlin counterparts. That is the point of §13 — several backends, one
//! wire format — so if these ever disagree, one of the backends is wrong.
//!
//! Anything needing a JDK is skipped, loudly, when there isn't one, so this file
//! still does useful work on a machine without a Java toolchain.

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
    let generated = backends::java::JavaBackend.generate(&model, &opts);
    assert_eq!(generated.files.len(), 1, "the Java backend emits exactly one file");
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

/// The body of the nested `record`/`enum`/`interface` named `name`, so a test
/// can say "this type declares this member" without a match somewhere else in
/// the file counting.
fn type_body(file: &str, name: &str) -> String {
    // Brace-counting rather than indentation: a sealed interface's nested
    // records are indented deeper than the interface itself, and a record's
    // component list may run over several lines before its body opens.
    let markers = [
        format!("record {name}("),
        format!("enum {name} {{"),
        format!("interface {name} {{"),
        format!("class {name} {{"),
    ];
    let lines: Vec<&str> = file.lines().collect();
    let start = lines
        .iter()
        .position(|l| markers.iter().any(|m| l.contains(m.as_str())))
        .unwrap_or_else(|| panic!("generated file declares no `{name}`"));

    // Where the body actually opens: either the marker line ends with `{`
    // itself, or it opens a multi-line component list, in which case the body
    // starts at the `) {` that closes it.
    let body_start = if lines[start].trim_end().ends_with('{') {
        start
    } else {
        lines[start..]
            .iter()
            .position(|l| l.trim_end().ends_with(") {"))
            .map(|i| start + i)
            .unwrap_or_else(|| panic!("`{name}`'s component list never closes"))
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
// Java toolchain
// ---------------------------------------------------------------------------

/// The Java compiler to test against, or `None` if this machine has none.
fn javac() -> Option<String> {
    let candidates = std::env::var("JAVAC").into_iter().chain(["javac".into()]);
    candidates.into_iter().find(|j| Command::new(j).arg("-version").output().is_ok())
}

/// The Java launcher, for the conformance run.
fn java() -> Option<String> {
    let candidates = std::env::var("JAVA").into_iter().chain(["java".into()]);
    candidates.into_iter().find(|j| Command::new(j).arg("-version").output().is_ok())
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("java_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Compiles a generated file on its own, so a schema feature that never appears
/// in the worked example still has to produce Java a compiler accepts.
/// `-Xlint:all -Werror` is deliberate: generated code that warns is generated
/// code nobody wants in their build.
fn assert_compiles(file: &(String, String), name: &str) {
    let Some(javac) = javac() else {
        eprintln!("skipping `{name}`: no javac found");
        return;
    };
    let dir = scratch(name);
    let src = dir.join(&file.0);
    std::fs::write(&src, &file.1).unwrap();

    let out = Command::new(&javac)
        .args(["-encoding", "UTF-8", "-Xlint:all", "-Werror", "-d"])
        .arg(dir.join("out"))
        .arg(&src)
        .output()
        .expect("failed to run javac");
    assert!(
        out.status.success(),
        "generated file does not compile cleanly:\n{}\n--- file ---\n{}",
        String::from_utf8_lossy(&out.stderr),
        file.1
    );
}

// ---------------------------------------------------------------------------
// Registry (SPEC.md §12 — one backend per target language)
// ---------------------------------------------------------------------------

#[test]
fn the_java_backend_is_registered_under_java() {
    assert!(backends::names().contains(&"java"));
    let backend = backends::find("java").expect("`java` resolves to a backend");
    assert_eq!(backend.name(), "java");
    assert!(!backend.description().is_empty());
    assert!(backends::find("Java").is_none(), "backend names are matched exactly");
    assert!(backends::find("jvm").is_none(), "and only under the name they registered");
}

#[test]
fn the_file_stem_names_the_public_class() {
    // Java requires the file name to match the one public class it holds, so
    // the stem drives both, PascalCased.
    let (name, file) = generate(EXAMPLE, "commands");
    assert_eq!(name, "Commands.java");
    assert_contains(&file, "public final class Commands {");

    let (name, file) = generate(EXAMPLE, "my-schema.v2");
    assert_eq!(name, "MySchemaV2.java");
    assert_contains(&file, "public final class MySchemaV2 {");

    // A stem that is not a legal Java identifier still has to produce one.
    assert_eq!(generate(EXAMPLE, "2fast").0, "_2fast.java");
}

#[test]
fn the_wrapper_class_yields_to_a_type_of_the_same_name() {
    // A nested type may not repeat its enclosing class's simple name, and a
    // `status.defs` declaring a `struct Status` is an ordinary thing to write.
    // The wrapper is an artifact of Java's one-public-class rule, so it is the
    // one that gives way — the schema's own name is the author's.
    let src = schema("struct Status: u8 { x: u8, }");
    let (name, file) = generate(&src, "status");
    assert_eq!(name, "StatusSchema.java");
    assert_contains(&file, "public final class StatusSchema {");
    assert_contains(&file, "public record Status(");
    assert_compiles(&(name, file), "outer_collision");
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
    assert_contains(&file, "import java.math.BigInteger;");
    assert!(!file.contains("\npackage "), "a generated file has no package declaration");
    for third_party in ["import com.", "import org.", "import io."] {
        assert!(!file.contains(third_party), "and pulls in nothing beyond the JDK");
    }
}

#[test]
fn imports_are_only_what_the_schema_needs() {
    let file = example_file();
    assert_contains(&file, "import java.math.BigDecimal;");
    assert_contains(&file, "import java.util.List;");

    let plain = file_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!plain.contains("BigDecimal"), "no `scaled` type means no rounding helper");
    assert!(!plain.contains("defgenRound"));
    assert!(!plain.contains("import java.util.List;"), "nor a list import with nothing to hold");
    assert!(!plain.contains("import java.util.Arrays;"), "nor a tail helper with no tail");
}

#[test]
fn doc_comments_become_javadoc() {
    let file = example_file();
    assert_contains(&file, " * Reusable 3-axis orientation reading");
    assert_contains(&file, " * Device status, pushed via notify.");
    // A record's components are documented where Javadoc reads them from.
    assert_contains(&file, "@param flags Reserved (§6.2)");
    // An alias declares nothing for Javadoc to attach to, so its documentation
    // becomes a plain comment rather than being dropped (§3).
    assert_contains(&file, "// Playback volume. The device only has 4 bits of resolution.");
}

#[test]
fn a_doc_comment_cannot_close_its_own_javadoc_or_be_read_as_markup() {
    // Javadoc is HTML, so `<` would swallow the rest of the line and `*/` would
    // end the comment early.
    let src = schema("/// ends a comment */ right <here>\nstruct S: u8 { x: u8, }");
    let file = generate(&src, "s");
    assert!(!file.1.contains("*/ right"), "`*/` inside a doc comment must be escaped");
    assert_contains(&file.1, "*&#47; right &lt;here&gt;");
    assert_compiles(&file, "doc_comment_escape");
}

// ---------------------------------------------------------------------------
// Type mapping (§2, §3, §4, §5, §6, §7)
// ---------------------------------------------------------------------------

#[test]
fn an_unsigned_value_is_widened_rather_than_wrapped() {
    // §2 lets a language with no unsigned types either widen or wrap. Widening
    // is what this backend does: the carrier is the smallest *signed* Java
    // integer that holds every value of the type, so the number on the field is
    // always the number on the wire.
    let src = schema(
        "struct Widths: u256 {\n\
             a: u1,\n\
             b: u7,\n\
             c: u8,\n\
             d: u16,\n\
             e: u32,\n\
             f: u63,\n\
             g: u64,\n\
             padding: u65,\n\
         }",
    );
    let file = generate(&src, "widths");
    for (field, ty) in
        [("a", "byte"), ("b", "byte"), ("c", "short"), ("d", "int"), ("e", "long"), ("f", "long")]
    {
        assert_contains(&file.1, &format!("{ty} {field}"));
    }
    // No Java primitive holds 2^64 - 1, so a u64 lands in a BigInteger.
    assert_contains(&file.1, "BigInteger g");
    assert_compiles(&file, "unsigned_widths");
}

#[test]
fn a_signed_value_takes_the_carrier_its_own_width_needs() {
    let src = schema(
        "struct Widths: u256 {\n\
             a: i8,\n\
             b: i16,\n\
             c: i32,\n\
             d: i64,\n\
             e: i96,\n\
             padding: u40,\n\
         }",
    );
    let file = generate(&src, "signed");
    for (field, ty) in [("a", "byte"), ("b", "short"), ("c", "int"), ("d", "long"), ("e", "BigInteger")] {
        assert_contains(&file.1, &format!("{ty} {field}"));
    }
    // A signed value is sign-extended back out of its declared width, never
    // read as the unsigned bit pattern.
    assert_contains(&file.1, "defgenSext(bits.get(off, 8), 8).byteValue()");
    assert_contains(&file.1, "defgenSext(bits.get((off + 120), 96), 96)");
    assert_compiles(&file, "signed_widths");
}

#[test]
fn a_narrow_field_is_range_checked_against_its_declared_width() {
    // §2: a `u4` in a `byte` holds plenty of values the wire does not.
    let file = example_file();
    assert_contains(
        &file,
        "defgenCheckUInt(BigInteger.valueOf(activeProfile), 4, \"Status.active_profile\")",
    );
    assert_contains(&file, "defgenCheckInt(BigInteger.valueOf(x), 8, \"Orientation.x\")");
}

#[test]
fn an_alias_resolves_away_but_keeps_its_name_in_the_documentation() {
    // §3: Java has no type alias, so `Volume` is a `byte` — but the schema's
    // name still has to be findable in the generated file.
    let file = example_file();
    assert_contains(&file, "// `Volume` (§3): a name for `u4`.");
    assert_contains(&file, "byte volume");
    assert!(!file.contains("class Volume"), "an alias generates no runtime type");
}

#[test]
fn a_scaled_type_exposes_both_representations() {
    // §4: the physical value to callers, the raw integer for exact round trips.
    let file = example_file();
    assert_contains(&file, "public static final float TEMPERATURE_SCALE = 0.01f;");
    assert_contains(&file, "public static final float TEMPERATURE_OFFSET = 0.0f;");
    assert_contains(&file, "public static float temperatureFromRaw(short raw) {");
    assert_contains(&file, "public static short temperatureToRaw(float value) throws DefgenError {");
    assert_contains(&file, "defgenCheckInt(raw, 16, \"Temperature\")");
}

#[test]
fn constants_become_static_final_fields() {
    // §3.1: no wire form, no codec — just a named value.
    let file = file_of(
        &schema("const MaxRetries: u8 = 5;\nconst MinTemperature: i16 = -40;\nconst Big: u128 = 5;"),
        "s",
    );
    assert_contains(&file, "public static final short MAX_RETRIES = (short) 5;");
    assert_contains(&file, "public static final short MIN_TEMPERATURE = (short) -40;");
    // No Java literal reaches 128 bits (§2), so this goes through `BigInteger`.
    assert_contains(&file, "public static final BigInteger BIG = new BigInteger(\"5\");");
}

#[test]
fn scaled_rounding_rounds_half_away_from_zero() {
    // §4, §13: the backends have to agree on a raw integer down to the last
    // unit, and the JDK's own Math.round rounds half *up*, which disagrees with
    // a negative tie. The Java conformance fixture pins the behavior down
    // through the public `temperatureToRaw`.
    let file = example_file();
    assert_contains(
        &file,
        "static BigInteger defgenRound(double value, String where) throws DefgenRangeError {",
    );
    assert!(!file.contains("Math.round("), "Math.round rounds half up, which would disagree with C/Python");
    assert_contains(&file, "if (remainder >= 0.5) {");
    assert_contains(&file, "if (remainder <= -0.5) {");
}

#[test]
fn a_closed_enum_is_a_java_enum_that_rejects_an_unmatched_value() {
    // §5, §12: decoding an undeclared value must be fallible, and — unlike an
    // open enum — a closed one needs no sealed hierarchy at all.
    let src = schema("enum Mode: u8 { A = 0, Bravo = 1, }\nstruct S: u8 { m: Mode, }");
    let file = generate(&src, "closed");
    let body = type_body(&file.1, "Mode");
    // §12: casing is converted to the target language's convention — but only
    // for a closed enum's constants; an open enum's nested records keep the
    // schema's own PascalCase spelling (checked below).
    assert_contains(&body, "A((short) 0),");
    assert_contains(&body, "BRAVO((short) 1);");
    assert_contains(
        &body,
        "throw new DefgenUnknownValueError(\"Mode: \" + raw + \" matches no declared variant\");",
    );
    assert!(!file.1.contains("sealed interface Mode"), "a closed enum needs no sealed hierarchy");
    assert!(!file.1.contains("record Unknown"), "nor a fallback type");
    assert_compiles(&file, "closed_enum");
}

#[test]
fn an_implicitly_numbered_enum_gets_the_values_the_checker_resolved() {
    // §5: the counter advances from the last explicit value it saw.
    let src = schema(
        "enum E: u8 { A, B = 7, C, D, }\nalias Bound = E;\nservice S(uuid: \"180a\") {\n    \
         characteristic C(uuid: \"2a00\", properties: [read]): Bound;\n}",
    );
    let body = type_body(&file_of(&src, "e"), "E");
    for member in ["A((short) 0)", "B((short) 7)", "C((short) 8)", "D((short) 9)"] {
        assert_contains(&body, member);
    }
}

#[test]
fn an_open_enum_is_a_sealed_interface_covering_every_wire_value() {
    // §5, §12: the unknown case is a distinct nested type, so it can never be
    // confused with a declared one — and the sealed interface already *is* the
    // "declared, or not" type, so no separate value alias is needed.
    let file = example_file();
    assert_contains(&file, "public sealed interface HearingMode {");
    assert_contains(&file, "record Default() implements HearingMode {");
    assert_contains(&file, "record Stereo() implements HearingMode {");
    assert_contains(&file, "record Unknown(byte raw) implements HearingMode {");
    let body = type_body(&file, "HearingMode");
    assert_contains(&body, "return new Unknown(raw);");
    assert!(
        !body.contains("throw new DefgenUnknownValueError"),
        "an open enum must not reject any wire value"
    );
    // A value it keeps has to be re-encodable, or the round trip is lossy.
    assert_contains(&body, "public static byte encode(HearingMode value) {");
}

#[test]
fn a_tagged_union_becomes_a_sealed_interface_with_nested_records() {
    // §7, §12: one nested type per variant under a shared sealed base, so a
    // decoded value is matched with `instanceof`, never by reading a tag by hand.
    let file = example_file();
    assert_contains(&file, "public sealed interface Command {");
    assert_contains(&file, "record SetVolume(byte volume) implements Command {");
    assert_contains(&file, "record TriggerFactoryReset() implements Command {");
    assert_contains(&file, "record Unknown(int id, long raw) implements Command {");
    assert_contains(&file, "public static final int ID = 1;");
    assert_contains(&file, "public static final int ID = 65535;");
    // A payload-less variant is still a type; it just carries no components.
    let reset = type_body(&file, "TriggerFactoryReset");
    assert_contains(&reset, "bits.put(off, 16, new BigInteger(\"65535\"));");
}

#[test]
fn a_union_variant_carries_the_unions_id_on_its_type_not_an_instance() {
    // §7: the id is a property of the variant's type, never of an instance, so
    // it cannot be set to something that disagrees with the type it is on.
    let file = example_file();
    let body = type_body(&file, "SetVolume");
    assert_contains(&body, "public static final int ID = 1;");
    assert!(!body.contains("int id"), "a known variant has no per-instance id to get wrong");
    // The fallback variant is the one exception: its id is data, by definition.
    assert_contains(&file, "record Unknown(int id, long raw) implements Command {");
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
    let file = generate(&src, "closed_union");
    assert!(!file.1.contains("record Unknown"), "a closed union has no fallback variant");
    assert_contains(&file.1, "throw new DefgenUnknownValueError(");
    assert_compiles(&file, "closed_union");
}

#[test]
fn a_struct_becomes_an_immutable_record_with_a_usable_zero_value() {
    // A record supplies equals/hashCode/toString; the extra constructor is what
    // makes `new Status()` a zero value the way a Kotlin default argument does.
    let file = example_file();
    let body = type_body(&file, "Status");
    assert_contains(&body, "public Status() {");
    for arg in ["(byte) 0,", "new HearingMode.Default(),", "false,", "0.0f,", "new Orientation(),"] {
        assert_contains(&body, arg);
    }
    assert!(!body.contains("void set"), "a record has no setters");
}

#[test]
fn a_struct_of_only_padding_still_produces_a_legal_record() {
    // §6.2 allows a container of pure padding, and a record with no components
    // already has the no-argument constructor, so no second one is generated.
    // The stem is deliberately not `blank`: a type named after the outer class
    // is escaped, and that is a different test's business.
    let src = schema("struct Blank: u8 { padding: u8, }");
    let file = generate(&src, "pad");
    let body = type_body(&file.1, "Blank");
    assert_contains(&body, "public static final int SIZE = 1;");
    assert_contains(&body, "void packFixed(DefgenBits bits, int off) throws DefgenError {");
    assert_contains(&body, "return new Blank();");
    assert_eq!(body.matches("public Blank()").count(), 0, "the canonical constructor is already no-argument");
    assert_compiles(&file, "padding_only");
}

#[test]
fn field_names_that_collide_with_java_keywords_are_escaped() {
    // §1 does not reserve Java's vocabulary, so the backend has to cope. The
    // schema's own spelling still has to appear in error messages, since that is
    // the name the author would go looking for.
    let src = schema("struct S: u32 { class: u8, new: u8, static: u8, int: u8, }");
    let file = generate(&src, "keywords");
    for name in ["class_", "new_", "static_", "int_"] {
        assert_contains(&file.1, &format!("short {name}"));
    }
    assert_contains(&file.1, "\"S.class\"");
    assert_compiles(&file, "keyword_fields");
}

#[test]
fn field_names_that_would_shadow_the_generated_api_are_escaped() {
    // A component called `encode` would collide with the method that encodes it;
    // one called `pack_fixed` would collide with the method that packs it
    // (`packFixed`, once camel-cased); one called `bits` would shadow the
    // parameter its own packing statement reads; and a record component may not
    // be named `toString` at all — that is a compile error in Java.
    let src = schema(
        "struct S: u48 { encode: u8, bits: u8, pack_fixed: u8, raw: u8, toString: u8, off: u8, }\n\
         service Svc(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): S;\n\
         }",
    );
    let file = generate(&src, "shadow");
    for name in ["encode_", "bits_", "packFixed_", "raw_", "toString_", "off_"] {
        assert_contains(&file.1, &format!("short {name}"));
    }
    assert_contains(&type_body(&file.1, "S"), "public byte[] encode() throws DefgenError {");
    assert_compiles(&file, "shadowing_fields");
}

#[test]
fn names_are_recased_without_mangling_acronyms() {
    let src = schema("struct HTTPProxyID: u8 { x: u8, }");
    let file = generate(&src, "acronym");
    // A type keeps the author's spelling; only casing derived from a field name
    // (camelCase) goes through the conversion.
    assert_contains(&file.1, "public record HTTPProxyID(");
    assert_compiles(&file, "acronym");

    let src = schema(
        "alias HTTPProxyID = u8;\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): HTTPProxyID;\n\
         }",
    );
    let file = file_of(&src, "acronym2");
    assert_contains(&file, "public static byte[] encodeHTTPProxyID(");
    assert_contains(&file, "public static final int HTTP_PROXY_ID_SIZE = 1;");
}

#[test]
fn active_profile_becomes_camel_case() {
    // §12: `snake_case` fields become camelCase members in Java.
    assert_contains(&example_file(), "byte activeProfile");
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
                assert_contains(&body, "encode() throws DefgenError");
                assert_contains(&body, &format!("{} decode(byte[] data) throws DefgenError", root.name));
            }
            _ => {
                assert_contains(&file, &format!("encode{}(", root.name));
                assert_contains(&file, &format!("decode{}(", root.name));
            }
        }
    }
}

#[test]
fn a_type_that_is_only_ever_nested_gets_no_entry_points() {
    // §8, §10: byte order is a property of the root container, so a type that is
    // only ever nested has no byte order of its own to encode in.
    let file = example_file();
    let body = type_body(&file, "Orientation");
    assert_contains(&body, "void packFixed(DefgenBits bits, int off) throws DefgenError {");
    assert!(!body.contains("byte[] encode()"), "a nested-only type needs no entry point");
}

#[test]
fn the_plumbing_is_internal_even_where_java_will_not_say_so() {
    let file = example_file();
    // A record's own plumbing is package-private outright.
    assert_contains(&file, "    static Status unpackFixed(DefgenBits bits, int off) throws DefgenError {");
    // An interface's cannot be — every member of one is public — so what keeps
    // it internal is the argument type, which is package-private.
    assert_contains(&file, "static final class DefgenBits {");
    assert!(!file.contains("public static final class DefgenBits"));
}

#[test]
fn sizes_are_exposed_as_constants() {
    let file = example_file();
    assert_contains(&type_body(&file, "Status"), "public static final int SIZE = 8;");
    assert_contains(&type_body(&file, "Orientation"), "public static final int SIZE = 3;");
    assert_contains(&type_body(&file, "Command"), "int SIZE = 8;");
    // §6.3: a variable-length type has a prefix and a maximum instead.
    let label = type_body(&file, "DiagnosticLabel");
    assert_contains(&label, "public static final int FIXED_SIZE = 1;");
    assert_contains(&label, "public static final int MAX_SIZE = 25;");
    assert!(!label.contains("int SIZE ="), "a varying size is not one number");
    assert_contains(&label, "public int encodedSize() {");
    // An alias has no type, so its sizes are constants on the outer class.
    assert_contains(&file, "public static final int OWNER_NAME_FIXED_SIZE = 0;");
    assert_contains(&file, "public static final int OWNER_NAME_MAX_SIZE = 32;");
}

#[test]
fn byte_order_is_resolved_per_root_container() {
    // §8: the file default, and the one struct that overrides it. Byte order
    // reaches the wire in exactly one place — where the bits meet a byte array.
    let file = example_file();
    assert_contains(&type_body(&file, "Status"), "DefgenBits bits = new DefgenBits(8, false);");
    assert_contains(&type_body(&file, "LegacySerial"), "DefgenBits bits = new DefgenBits(4, true);");
    assert_contains(&type_body(&file, "LegacySerial"), "DefgenBits.fromBytes(data, true)");

    // The mirror is what makes a big-endian container fill from its
    // most-significant end while its fields stay in declaration order (§6).
    assert_contains(&file, "return big ? size * 8 - off - bits : off;");
}

#[test]
fn a_variable_length_field_is_a_string_or_a_list() {
    // §12: Java gets the idiomatic container, not C's buffer-plus-length.
    let file = example_file();
    assert_contains(&file, "String label");
    assert_contains(&file, "List<Float> samples");
    assert!(!file.contains("labelLen"), "a Java container carries its own length");
    assert_contains(&type_body(&file, "DiagnosticLabel"), "return FIXED_SIZE + tailLen();");
}

#[test]
fn a_tail_is_decoded_before_the_record_is_built() {
    // A record is immutable, so — unlike the Kotlin backend, which fills the
    // tail in after constructing — the tail has to be read first and passed in.
    let body = type_body(&example_file(), "DiagnosticLabel");
    assert_contains(
        &body,
        "String tail = unpackTail(Arrays.copyOfRange(data, FIXED_SIZE, data.length), false);",
    );
    assert_contains(&body, "static DiagnosticLabel unpackFixed(DefgenBits bits, int off, String tail)");
    assert_contains(&body, "return new DiagnosticLabel(bits.get(off, 8).shortValue(), tail);");
}

#[test]
fn a_fixed_array_is_checked_for_its_exact_count() {
    // §6.1: a fixed array carries exactly its declared count, always.
    let file = example_file();
    assert_contains(&file, "defgenCheckCount(samples, 4, \"TemperatureLog.samples\")");
    assert_contains(&file, "defgenCheckCount(points, 2, \"MotionPath.points\")");
    // And what comes back out is not writable through the record's accessor.
    assert_contains(&file, "return new TemperatureLog(List.copyOf(samples));");
}

#[test]
fn declared_padding_is_validated_and_bare_padding_is_not() {
    // §6.2: `padding: uN = 0` is a claim about the wire; bare padding is not.
    let file = example_file();
    assert_contains(
        &file,
        "throw new DefgenPaddingError(\"MotionPath: padding at bits 48..64 is not zero\");",
    );
    assert_eq!(
        file.matches("throw new DefgenPaddingError").count(),
        1,
        "the example declares exactly one `padding = 0` run"
    );
    // Reserved bits are neither: they are carried through untouched (§6.2).
    assert_contains(&file, "byte flags");
    assert_contains(&file, "bits.get((off + 60), 4).byteValue()");
}

#[test]
fn an_alias_of_a_variable_length_type_is_bindable_on_its_own() {
    // §6.3: "one characteristic is just a name" needs no wrapping type.
    let file = example_file();
    assert_contains(&file, "public static byte[] encodeOwnerName(String value) throws DefgenError {");
    assert_contains(&file, "public static String decodeOwnerName(byte[] data) throws DefgenError {");
    assert_contains(&file, "defgenDecodeUtf8(data, \"OwnerName\")");
    // Nothing precedes the tail, so the tail is the whole encoding.
    assert_contains(&file, "return defgenEncodeUtf8(value, 32, \"OwnerName\");");
}

#[test]
fn utf8_is_validated_rather_than_patched_up() {
    // §6.3: malformed input fails; it is never replaced with U+FFFD, which would
    // turn a transport bug into silently wrong data.
    let file = example_file();
    assert_contains(&file, "CodingErrorAction.REPORT");
    assert_contains(&file, "throw new DefgenUtf8Error");
    assert!(!file.contains("new String(data"), "the String constructor substitutes rather than failing");
}

#[test]
fn every_failure_is_a_checked_defgen_error() {
    // §12: one sealed base, so a caller can catch the lot with one `catch` —
    // and checked, so a caller cannot forget that decoding can fail.
    let file = example_file();
    assert_contains(&file, "public abstract static sealed class DefgenError extends Exception {");
    for sub in ["Length", "Range", "UnknownValue", "Padding", "Utf8"] {
        assert_contains(&file, &format!("public static final class Defgen{sub}Error extends DefgenError {{"));
    }
    // Serializable without a serialVersionUID is a warning, and generated code
    // that warns is generated code nobody wants in their build.
    assert_eq!(file.matches("private static final long serialVersionUID = 1L;").count(), 6);
}

#[test]
fn gatt_metadata_becomes_constants() {
    // §10: UUIDs and properties, with what to do with them left to the caller.
    let file = example_file();
    assert_contains(
        &file,
        "public static final String HEARING_AID_CONTROL_UUID = \"7d8f0000-3c1a-4e8a-9b5a-000000000000\";",
    );
    assert_contains(
        &file,
        "public static final String HEARING_AID_CONTROL_STATUS_CHAR_UUID = \
         \"7d8f0001-3c1a-4e8a-9b5a-000000000000\";",
    );
    assert_contains(&file, "public enum GattProperty {");
    assert_contains(&file, "Set.of(GattProperty.READ, GattProperty.NOTIFY)");
    assert_contains(&file, "public static final List<GattService> SERVICES = List.of(HEARING_AID_CONTROL);");
}

#[test]
fn a_schema_with_no_services_emits_no_gatt_section() {
    let file = generate(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!file.1.contains("GattProperty"));
    assert!(!file.1.contains("SERVICES"));
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
    let file = generate(&src, "batch");
    assert_contains(&file.1, "List<Reading> readings");
    assert_contains(&file.1, "defgenCheckMax(readings, 8, \"Batch.readings\")");
    assert_contains(&file.1, "if (data.length % 2 != 0) {");
    assert_contains(&file.1, "int count = data.length / 2;");
    assert_contains(&file.1, "if (count > 8) {");
    assert_contains(&file.1, "return readings.size() * 2;");
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
    let file = generate(&src, "nested");
    let outer = type_body(&file.1, "Outer");
    assert_contains(&outer, "public static final int FIXED_SIZE = 2;");
    assert_contains(&outer, "public static final int MAX_SIZE = 6;");
    assert_contains(&outer, "return inner.tailLen();");
    assert_contains(&outer, "return inner.packTail(big);");
    assert_contains(&outer, "return Inner.unpackTail(data, big);");
    // The nested type's tail type is what the outer one threads through.
    assert_contains(&outer, "static Outer unpackFixed(DefgenBits bits, int off, String tail)");
    assert_contains(&outer, "Inner.unpackFixed(bits, (off + 8), tail)");
    assert!(!type_body(&file.1, "Inner").contains("byte[] encode()"));
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
    let file = generate(&src, "note");
    let body = type_body(&file.1, "Note");
    assert_contains(&body, "DefgenBits bits = new DefgenBits(FIXED_SIZE, true);");
    assert_contains(&body, "return defgenConcat(bits.toBytes(), packTail(true));");
    assert_contains(
        &body,
        "String tail = unpackTail(Arrays.copyOfRange(data, FIXED_SIZE, data.length), true);",
    );
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
    let file = generate(&src, "bound");
    assert_contains(&file.1, "public static byte[] encodeE(E value)");
    assert_contains(&file.1, "public static E decodeE(byte[] data)");
    assert_contains(&file.1, "public static final int E_SIZE = 1;");
    assert_compiles(&file, "alias_bound_enum");
}

#[test]
fn an_f64_scaled_type_is_not_widened_a_second_time() {
    // §4: the division happens in `double` whichever physical type is declared,
    // so a `float` is cast to one first — but saying so where the value already
    // *is* a `double` is a redundant cast, which javac warns about and this
    // test's `-Werror` compile would reject. The worked example's `scaled` types
    // are both `f32`, so nothing else here reaches this.
    let src = schema(
        "scaled Volts: u32 as f64 (scale: 0.5, offset: -3.25);\n\
         struct S: u32 { v: Volts, }\n\
         service Svc(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): S;\n\
         }",
    );
    let file = generate(&src, "volts");
    assert_contains(&file.1, "public static final double VOLTS_SCALE = 0.5;");
    assert_contains(&file.1, "public static final double VOLTS_OFFSET = -3.25;");
    assert_contains(&file.1, "defgenRound((value - VOLTS_OFFSET) / VOLTS_SCALE, \"Volts\")");
    assert!(!file.1.contains("(double) value"), "a double needs no widening to a double");
    assert_compiles(&file, "f64_scaled");
}

#[test]
fn an_alias_of_a_variable_length_array_is_bindable_on_its_own() {
    // §6.3, the sibling of the `string` case the worked example covers: the
    // element count comes from the buffer, and there is no fixed prefix at all.
    let src = schema(
        "alias Samples = u16[max: 4];\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Samples;\n\
         }",
    );
    let file = generate(&src, "samples");
    assert_contains(&file.1, "public static byte[] encodeSamples(List<Integer> value) throws DefgenError {");
    assert_contains(&file.1, "public static List<Integer> decodeSamples(byte[] data) throws DefgenError {");
    assert_contains(&file.1, "defgenCheckMax(value, 4, \"Samples\")");
    assert_contains(&file.1, "int count = data.length / 2;");
    assert_contains(&file.1, "return List.copyOf(out);");
    assert_compiles(&file, "var_array_alias");
}

#[test]
fn a_128_bit_value_falls_back_to_biginteger() {
    // §2: 128 bits is the ceiling, and the JVM has nothing narrower than
    // BigInteger that reaches it.
    let src = schema(
        "struct Wide: u128 {\n\
             id: u128,\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Wide;\n\
         }",
    );
    let file = generate(&src, "wide");
    assert_contains(&file.1, "BigInteger id");
    assert_contains(&file.1, "this(BigInteger.ZERO);");
    assert_contains(&file.1, "public static final int SIZE = 16;");
    assert_compiles(&file, "int128");
}

#[test]
fn an_enum_backed_wider_than_a_long_compares_with_equals() {
    // A `BigInteger` carrier has no `==`, so the dispatch has to know which
    // comparison its carrier speaks.
    let src = schema(
        "enum Wide: u96 { A = 1, B = 2, else Other, }\n\
         enum Closed: u96 { C = 3, }\n\
         struct S: u192 { a: Wide, b: Closed, }",
    );
    let file = generate(&src, "wide_enum");
    assert_contains(&file.1, "if (raw.equals(new BigInteger(\"1\"))) {");
    assert_contains(&file.1, "if (variant.raw.equals(raw)) {");
    assert_compiles(&file, "wide_enum");
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// Code generation is total: any model the checker accepts must produce a file
/// rather than a panic.
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
        let file = backends::java::JavaBackend.generate(&model, &Options::default());
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

/// Compiles the worked example's generated file together with the hand-written
/// conformance fixture and runs the result. The fixture's byte strings are
/// derived from SPEC.md by hand, so this is the test that would catch the
/// emitter and the spec disagreeing.
#[test]
fn the_generated_file_round_trips_the_worked_example() {
    let (Some(javac), Some(java)) = (javac(), java()) else {
        eprintln!("skipping the Java conformance run: no JDK found");
        return;
    };
    let dir = scratch("conformance");
    let schema_path = dir.join("Commands.java");
    std::fs::write(&schema_path, example_file()).unwrap();

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples/java_conformance.java");
    // The fixture's public class is `Conformance`, and javac insists a public
    // class live in a file of its own name — which is why it is staged rather
    // than compiled where it sits.
    let fixture_path = dir.join("Conformance.java");
    std::fs::copy(&fixture, &fixture_path).expect("failed to stage the fixture");

    let classes = dir.join("out");
    let build = Command::new(&javac)
        .args(["-encoding", "UTF-8", "-Xlint:all", "-Werror", "-d"])
        .arg(&classes)
        .arg(&schema_path)
        .arg(&fixture_path)
        .output()
        .expect("failed to run javac");
    assert!(
        build.status.success(),
        "the conformance fixture did not build:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = Command::new(&java)
        .arg("-cp")
        .arg(&classes)
        .arg("Conformance")
        .output()
        .expect("failed to run the conformance fixture");
    assert!(
        run.status.success(),
        "the generated codecs disagree with the spec:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
