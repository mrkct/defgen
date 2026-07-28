//! The Python backend.
//!
//! Three layers of test live here. The cheap ones read the generated text and
//! assert on what it declares. The middle one hands the module to a real
//! interpreter and imports it, which executes every `class` body — so a bad
//! `default_factory`, a name used before it exists, or a `@dataclass` that
//! cannot be built shows up here rather than in a user's program. The expensive
//! one runs `tests/examples/python_conformance.py` against the generated
//! module, which is what actually pins the wire format down: a codegen bug that
//! produces valid Python saying the wrong thing is invisible to string
//! matching.
//!
//! The conformance fixture asserts the very same byte strings as its C
//! counterpart. That is the point of §13 — two backends, one wire format — so
//! if these two files ever disagree, one of the backends is wrong.
//!
//! Anything needing an interpreter is skipped, loudly, when there isn't one, so
//! this file still does useful work on a machine without Python. The textual
//! checks, including the type-annotation sweep, run either way.

use std::path::{Path, PathBuf};
use std::process::Command;

use defgen::backends::{self, Backend, Options, snake};
use defgen::diag::Severity;
use defgen::model::{Model, TypeKind};

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

/// Generates a module from `src`, as the CLI would for a file named `stem`.
fn module_of(src: &str, stem: &str) -> String {
    generate(src, stem).1
}

/// The `(file name, contents)` the backend produced.
fn generate(src: &str, stem: &str) -> (String, String) {
    let model = model_of(src);
    let opts = Options { stem: stem.to_string(), source: Some(format!("{stem}.defs")) };
    let generated = backends::python::PythonBackend.generate(&model, &opts);
    assert_eq!(generated.files.len(), 1, "the Python backend emits exactly one file");
    let file = generated.single();
    (file.name.clone(), file.contents.clone())
}

fn example_module() -> String {
    module_of(EXAMPLE, "commands")
}

/// A schema with just enough header to be legal, so a test can be one struct.
fn schema(body: &str) -> String {
    format!("endian: little;\n---\n{body}")
}

fn assert_contains(module: &str, needle: &str) {
    assert!(module.contains(needle), "generated module is missing `{needle}`");
}

/// The indented body of `class <name>`, so a test can say "this class declares
/// this method" without a top-level match somewhere else counting.
fn class_body(module: &str, name: &str) -> String {
    let exact = format!("class {name}:");
    let derived = format!("class {name}(");
    let start = module
        .lines()
        .position(|l| l == exact || l.starts_with(&derived))
        .unwrap_or_else(|| panic!("generated module declares no `class {name}`"));
    module
        .lines()
        .skip(start + 1)
        .take_while(|l| l.is_empty() || l.starts_with("    "))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Type annotations (§12 — the generated API is fully typed)
// ---------------------------------------------------------------------------

/// Splits a parameter list on the commas that separate parameters, leaving the
/// ones inside `list[int, ...]` or a default expression alone.
fn split_params(params: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in params.chars() {
        match c {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Every `def` the emitter writes annotates its return and all its parameters.
///
/// This is a textual sweep rather than an `inspect` one so that it costs
/// nothing and runs for every schema in this file, not just the ones an
/// interpreter gets to see. `python_conformance.py` does the complementary job:
/// it evaluates the annotations, which is the only way to catch one that names
/// a type that does not exist.
fn assert_fully_annotated(module: &str, name: &str) {
    let mut seen = 0;
    for (i, line) in module.lines().enumerate() {
        let text = line.trim_start();
        let Some(rest) = text.strip_prefix("def ") else { continue };
        let at = format!("{name}:{}", i + 1);
        seen += 1;

        assert!(
            line.trim_end().ends_with(':'),
            "{at}: `def` runs onto another line, which this sweep cannot read: {text}"
        );
        assert!(rest.contains(") -> "), "{at}: `{text}` has no return annotation");

        let open = rest.find('(').unwrap_or_else(|| panic!("{at}: malformed def: {text}"));
        let close = rest.rfind(") -> ").unwrap_or_else(|| panic!("{at}: malformed def: {text}"));
        for param in split_params(&rest[open + 1..close]) {
            let param = param.trim();
            if param.is_empty() || param == "self" || param == "cls" {
                continue;
            }
            assert!(param.contains(": "), "{at}: parameter `{param}` of `{text}` is not annotated");
        }
    }
    assert!(seen > 5, "the annotation sweep found only {seen} definitions in `{name}`");
}

// ---------------------------------------------------------------------------
// Python interpreter
// ---------------------------------------------------------------------------

/// The interpreter to test against, or `None` if this machine has none.
///
/// 3.10 is the floor the emitter targets (`slots=True`, `X | Y` in a
/// `TypeAlias`), so an older interpreter is treated as no interpreter rather
/// than as a failure.
fn python() -> Option<String> {
    let candidates = std::env::var("PYTHON").into_iter().chain(["python3".into(), "python".into()]);
    candidates.into_iter().find(|p| {
        Command::new(p)
            .args(["-c", "import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("python_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Checks a module the cheap way, then — if this machine has an interpreter —
/// imports it, which runs every `class` body the emitter wrote.
///
/// `-W error` is deliberate: generated code that warns is generated code nobody
/// wants in their program.
fn assert_valid(module: &str, name: &str) {
    assert_fully_annotated(module, name);

    let Some(python) = python() else {
        eprintln!("skipping the import of `{name}`: no Python 3.10+ interpreter found");
        return;
    };
    let dir = scratch(name);
    std::fs::write(dir.join("schema.py"), module).unwrap();

    let out = Command::new(&python)
        .current_dir(&dir)
        .args(["-W", "error", "-c", "import schema"])
        .output()
        .expect("failed to run the Python interpreter");

    assert!(
        out.status.success(),
        "generated module does not import cleanly:\n{}\n--- module ---\n{module}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------------------------------------------------------------------------
// Registry (SPEC.md §12 — one backend per target language)
// ---------------------------------------------------------------------------

#[test]
fn the_python_backend_is_registered_under_python() {
    assert!(backends::names().contains(&"python"));
    let backend = backends::find("python").expect("`python` resolves to a backend");
    assert_eq!(backend.name(), "python");
    assert!(!backend.description().is_empty());
    assert!(backends::find("Python").is_none(), "backend names are matched exactly");
    assert!(backends::find("py").is_none(), "and only under the name they registered");
}

#[test]
fn the_file_stem_drives_the_module_name() {
    let (name, _) = generate(EXAMPLE, "commands");
    assert_eq!(name, "commands.py");

    // A stem that is not a legal Python module name still has to produce one:
    // `import` cannot reach a file whose name it cannot spell.
    assert_eq!(generate(EXAMPLE, "my-schema.v2").0, "my_schema_v2.py");
    assert_eq!(generate(EXAMPLE, "2fast").0, "_2fast.py");
    // PEP 8 module names are lowercase, whatever the file on disk was called.
    assert_eq!(generate(EXAMPLE, "HearingAid").0, "hearingaid.py");
}

#[test]
fn generation_is_deterministic() {
    assert_eq!(example_module(), example_module());
}

// ---------------------------------------------------------------------------
// Module shape
// ---------------------------------------------------------------------------

#[test]
fn the_module_is_self_contained() {
    let module = example_module();
    assert_contains(&module, "from __future__ import annotations");
    assert_contains(&module, "import enum");
    assert_contains(&module, "from dataclasses import dataclass, field");
    assert_contains(&module, "from typing import ClassVar, Final, TypeAlias, TypeVar");
    assert!(!module.contains("import numpy"), "a generated module has no dependencies");
    assert!(!module.contains("import construct"), "and does its own bit packing");
    assert!(!module.contains("from ."), "nor does it pull in a sibling module");
    assert_valid(&module, "self_contained");
}

#[test]
fn annotations_are_postponed() {
    // §12: a class annotates its own methods with its own name, which does not
    // exist yet while the class body runs. PEP 563 is what makes that legal,
    // and it has to be the first statement after the module docstring.
    let module = example_module();
    let first = module
        .lines()
        .find(|l| !l.is_empty() && !l.starts_with('"') && !l.starts_with("Codecs") && l.contains("import"))
        .expect("an import");
    assert_eq!(first, "from __future__ import annotations");
    assert_contains(&class_body(&module, "Status"), "def decode(cls, data: bytes) -> Status:");
}

#[test]
fn imports_are_only_what_the_schema_needs() {
    // A module that imports what it never uses is a module that lints dirty.
    let module = module_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!module.contains("import enum"), "no enum and no service means no need for enum");
    assert!(!module.contains("_round"), "no `scaled` type means no rounding helper");
    assert!(!module.contains(", field"), "no container default means no need for `field`");
    assert!(!module.contains("TypeAlias"), "no alias means no `TypeAlias`");
    assert_contains(&module, "from dataclasses import dataclass");
    assert_valid(&module, "minimal_imports");
}

#[test]
fn dunder_all_lists_the_public_surface_and_nothing_private() {
    let module = example_module();
    let start = module.find("__all__ = [").expect("__all__");
    let end = module[start..].find("\n]").expect("__all__ closes") + start;
    let names: Vec<&str> =
        module[start..end].lines().skip(1).map(|l| l.trim().trim_matches(['"', ','])).collect();

    for wanted in ["DefgenError", "Status", "Command", "CommandUnknown", "SERVICES"] {
        assert!(names.contains(&wanted), "`__all__` is missing `{wanted}`");
    }
    for name in &names {
        assert!(!name.starts_with('_'), "`__all__` exports the private name `{name}`");
        let defined = [format!("\n{name}"), format!("class {name}"), format!("def {name}(")]
            .iter()
            .any(|form| module.contains(form));
        assert!(defined, "`__all__` exports `{name}`, which the module does not define");
    }
}

#[test]
fn doc_comments_become_docstrings() {
    let module = example_module();
    assert_contains(&module, "\"\"\"Playback volume. The device only has 4 bits of resolution.");
    assert_contains(&module, "\"\"\"Reusable 3-axis orientation reading");
}

#[test]
fn a_doc_comment_cannot_close_its_own_docstring() {
    let src = schema("/// ends a docstring \"\"\" right here\nstruct S: u8 { x: u8, }");
    let module = module_of(&src, "s");
    assert!(!module.contains("\"\"\" right here"), "`\"\"\"` inside a doc comment must be escaped");
    assert_contains(&module, "\\\"\\\"\\\" right here");
    assert_valid(&module, "docstring_escape");
}

#[test]
fn a_doc_comment_cannot_smuggle_in_a_backslash_escape() {
    // A docstring is a normal string literal, so `\n` in a doc comment would
    // otherwise become a newline — or, worse, `\u0041` a different character.
    let src = schema("/// C:\\Users and \\u0041\nstruct S: u8 { x: u8, }");
    let module = module_of(&src, "s");
    assert_contains(&module, "C:\\\\Users and \\\\u0041");
    assert_valid(&module, "backslash_escape");
}

// ---------------------------------------------------------------------------
// Type mapping (§2, §3, §4, §5, §6, §7)
// ---------------------------------------------------------------------------

#[test]
fn every_integer_width_is_int_with_the_width_in_the_range_check() {
    // §2: Python's `int` is unbounded and has no signed/unsigned distinction,
    // so — unlike C — the declared width cannot live in the carrier type. It
    // lives in the check instead, at the exact declared width rather than a
    // rounded-up one.
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
    let module = module_of(&src, "widths");
    for f in ["a", "b", "c", "e", "f", "h"] {
        assert_contains(&module, &format!("    {f}: int = 0"));
    }
    assert_contains(&module, "_check_uint(self.a, 1, \"Widths.a\")");
    assert_contains(&module, "_check_uint(self.c, 9, \"Widths.c\")");
    assert_contains(&module, "_check_uint(self.e, 33, \"Widths.e\")");
    assert_contains(&module, "_check_int(self.f, 8, \"Widths.f\")");
    assert_contains(&module, "_check_int(self.h, 48, \"Widths.h\")");
    // A signed value is sign-extended back out of its declared width, never
    // read as the unsigned bit pattern.
    assert_contains(&module, "f=_sext(_bits.get(_off + 47, 8), 8)");
    assert_contains(&module, "h=_sext(_bits.get(_off + 55, 48), 48)");
    assert_valid(&module, "widths");
}

#[test]
fn values_wider_than_64_bits_need_no_special_case() {
    // §2: 128 bits is the ceiling, and it is the one place C needs a compiler
    // extension. Python's `int` reaches it with nothing extra.
    let src = schema(
        "struct Wide: u128 {\n\
             id: u128,\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Wide;\n\
         }",
    );
    let module = module_of(&src, "wide");
    assert_contains(&module, "    id: int = 0");
    assert_contains(&module, "_check_uint(self.id, 128, \"Wide.id\")");
    assert_contains(&module, "SIZE: ClassVar[int] = 16");
    assert_valid(&module, "int128");
}

#[test]
fn an_alias_keeps_the_name_the_author_declared() {
    // §3: an alias generates no runtime type, but the domain name survives —
    // and a `TypeAlias` is exactly that: a name, with no class behind it.
    let module = example_module();
    assert_contains(&module, "Volume: TypeAlias = int");
    assert_contains(&module, "    volume: Volume = 0");
}

/// A raw `f32`/`f64` field (§2) is carried as `float` and encodes to exactly
/// the IEEE-754 bit pattern, little-endian on the wire — this is what
/// actually pins the byte layout down, complementing the C backend's version
/// of the same test (§13).
#[test]
fn raw_floats_round_trip_ieee754_bit_patterns() {
    let Some(python) = python() else {
        eprintln!("skipping the raw float round trip: no Python 3.10+ interpreter found");
        return;
    };
    let src = schema(
        "struct Floats: u96 { a: f32, b: f64, }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Floats;\n\
         }",
    );
    let module = module_of(&src, "floats");
    assert_contains(&module, "a: float = 0.0");
    assert_contains(&module, "b: float = 0.0");

    let dir = scratch("raw_floats");
    std::fs::write(dir.join("floats.py"), module).unwrap();
    let script = r#"
import struct
import floats

v = floats.Floats(a=1.5, b=-2.25)
buf = v.encode()
assert buf[0:4] == struct.pack("<f", 1.5), buf[0:4]
assert buf[4:12] == struct.pack("<d", -2.25), buf[4:12]

back = floats.Floats.decode(buf)
assert back.a == 1.5, back.a
assert back.b == -2.25, back.b
"#;
    std::fs::write(dir.join("run.py"), script).unwrap();

    let out = Command::new(&python)
        .current_dir(&dir)
        .args(["-W", "error", "run.py"])
        .output()
        .expect("failed to run the Python interpreter");
    assert!(
        out.status.success(),
        "the raw float round trip failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_scaled_type_exposes_both_representations() {
    // §4: the physical value to callers, the raw integer for exact round trips.
    let module = example_module();
    assert_contains(&module, "Temperature: TypeAlias = float");
    assert_contains(&module, "TEMPERATURE_SCALE: Final = 0.01");
    assert_contains(&module, "TEMPERATURE_OFFSET: Final = 0.0");
    assert_contains(&module, "def temperature_from_raw(raw: int) -> Temperature:");
    assert_contains(&module, "def temperature_to_raw(value: Temperature) -> int:");
    assert_contains(&module, "raise DefgenRangeError");
}

#[test]
fn scaled_rounding_uses_neither_pythons_round_nor_the_standard_library() {
    // §4, §13: the backends have to agree on a raw integer down to the last
    // unit, and Python's `round` rounds half to even where C's rounds half away
    // from zero. 0.5 has to become 1 here, not 0.
    let module = example_module();
    assert_contains(&module, "def _round(value: float, where: str) -> int:");
    assert_contains(
        &module,
        "raw = _round((value - TEMPERATURE_OFFSET) / TEMPERATURE_SCALE, \"Temperature\")",
    );
    assert!(!module.contains("= round("), "the built-in `round` would disagree with the C backend");
    // The helper is a handful of comparisons, so the module needs no imports at
    // all beyond the three it already has — matching the C backend, which
    // carries the same helper rather than linking libm.
    assert!(!module.contains("import math"), "rounding does not justify a stdlib import");
    assert_contains(&module, "    whole = int(value)");
    assert_contains(&module, "    remainder = value - whole");
    // The bias-then-truncate shortcut is wrong at the double just below 0.5.
    assert!(!module.contains("value + 0.5"), "no bias is added before truncating");
}

#[test]
fn a_scaled_value_that_cannot_be_rounded_is_a_range_error() {
    // A NaN or an infinity has no integer to round to, and `int()` would raise
    // ValueError or OverflowError — neither of which a caller catching
    // `DefgenError` would see.
    let module = example_module();
    assert_contains(&module, "    if value - value != 0.0:");
    assert_contains(&module, "cannot be rounded to an integer");
}

#[test]
fn enum_variants_become_screaming_snake_members() {
    // §12: casing is converted to the target language's convention.
    let module = example_module();
    assert_contains(&module, "class HearingMode(enum.IntEnum):");
    assert_contains(&module, "    DEFAULT = 0");
    assert_contains(&module, "    CINEMA = 3");
}

#[test]
fn constants_become_module_level_finals() {
    // §3.1: no wire form, no codec — just a named value.
    let module = module_of(&schema("const MaxRetries: u8 = 5;\nconst MinTemperature: i16 = -40;"), "s");
    assert_contains(&module, "MAX_RETRIES: Final[int] = 5");
    assert_contains(&module, "MIN_TEMPERATURE: Final[int] = -40");
}

#[test]
fn an_implicitly_numbered_enum_gets_the_values_the_checker_resolved() {
    // §5: the counter advances from the last explicit value it saw.
    let src = schema(
        "enum E: u8 { A, B = 7, C, D, }\nalias Bound = E;\nservice S(uuid: \"180a\") {\n    characteristic C(uuid: \"2a00\", properties: [read]): Bound;\n}",
    );
    let body = class_body(&module_of(&src, "e"), "E");
    for member in ["    A = 0", "    B = 7", "    C = 8", "    D = 9"] {
        assert_contains(&body, member);
    }
}

#[test]
fn a_closed_enum_rejects_an_unmatched_value_on_both_sides() {
    // §5, §12: decoding an undeclared value must be fallible.
    let src = schema("enum Mode: u8 { A = 0, B = 1, }\nstruct S: u8 { m: Mode, }");
    let module = module_of(&src, "closed");
    let body = class_body(&module, "Mode");
    assert_eq!(
        body.matches("raise DefgenUnknownValueError").count(),
        2,
        "a closed enum is validated on encode and on decode"
    );
    assert_contains(&body, "def _decode(raw: int) -> Mode:");
    assert!(!module.contains("ModeValue"), "a closed enum has no second inhabitant to name");
    assert!(!module.contains("class ModeUnknown"), "and no fallback class");
    assert_valid(&module, "closed_enum");
}

#[test]
fn an_open_enum_decodes_an_undeclared_value_into_its_own_type() {
    // §5, §12: the unknown case is a distinct variant carrying the raw value,
    // so it can never be confused with a declared one — which an `IntEnum`
    // member alone could be, since it compares equal to its integer.
    let module = example_module();
    assert_contains(&module, "class HearingModeUnknown:");
    assert_contains(&module, "HearingModeValue: TypeAlias = HearingMode | HearingModeUnknown");
    assert_contains(&module, "    mode: HearingModeValue = HearingMode.DEFAULT");
    assert_contains(&class_body(&module, "HearingMode"), "return HearingModeUnknown(raw=raw)");
    assert!(
        !class_body(&module, "HearingMode").contains("raise DefgenUnknownValueError"),
        "an open enum must not reject any wire value"
    );
    // A value it keeps has to be re-encodable, or the round trip is lossy.
    assert_contains(&module, "        if isinstance(value, HearingModeUnknown):");
    assert_contains(&module, "            return value.raw");
    // and it is frozen, so it can be used as a dict key or compared safely.
    assert_contains(&module, "@dataclass(frozen=True, slots=True)\nclass HearingModeUnknown:");
}

#[test]
fn a_tagged_union_becomes_a_sealed_class_hierarchy() {
    // §7, §12: one class per variant under a shared base, so a decoded value is
    // matched with `isinstance` rather than by reading a tag by hand.
    let module = example_module();
    assert_contains(&module, "class Command:");
    assert_contains(&module, "class CommandSetVolume(Command):");
    assert_contains(&module, "class CommandTriggerFactoryReset(Command):");
    assert_contains(&module, "class CommandUnknown(Command):");
    assert_contains(&module, "    ID: ClassVar[int] = 0x1");
    assert_contains(&module, "    ID: ClassVar[int] = 0xffff");
    assert_contains(&module, "    TAG_BITS: ClassVar[int] = 16");
    // The dispatch table is what makes decode a lookup rather than a chain.
    assert_contains(&module, "_COMMAND_BY_ID: Final[dict[int, type[Command]]] = {");
    assert_contains(&module, "    0x1: CommandSetVolume,");
    // A payload-less variant is still a class; it just has no fields.
    let reset = class_body(&module, "CommandTriggerFactoryReset");
    assert!(!reset.contains(": int = 0"), "TriggerFactoryReset declares no payload");
    assert_contains(&reset, "_bits.put(_off, 16, 0xffff)");
}

#[test]
fn a_union_variant_carries_the_unions_id_but_not_a_mutable_one() {
    // §7: the id is a property of the variant's type, never of an instance, so
    // it cannot be set to something that disagrees with the class it is on.
    let module = example_module();
    let body = class_body(&module, "CommandSetVolume");
    assert_contains(&body, "ID: ClassVar[int] = 0x1");
    assert!(!body.contains("id:"), "a known variant has no per-instance id to get wrong");
    // The fallback variant is the one exception: its id is data, by definition.
    assert_contains(&class_body(&module, "CommandUnknown"), "id: int = 0");
    assert_contains(&class_body(&module, "CommandUnknown"), "raw: int = 0");
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
    let module = module_of(&src, "closed_union");
    assert!(!module.contains("class CmdUnknown"), "a closed union has no fallback variant");
    assert_contains(&class_body(&module, "Cmd"), "raise DefgenUnknownValueError");
    assert_valid(&module, "closed_union");
}

#[test]
fn a_struct_of_only_padding_still_produces_a_legal_class() {
    // §6.2 allows a container of pure padding, and a dataclass with no fields
    // is legal Python — but it still has to declare its size and pack nothing.
    let src = schema("struct Blank: u8 { padding: u8, }");
    let module = module_of(&src, "blank");
    let body = class_body(&module, "Blank");
    assert_contains(&body, "SIZE: ClassVar[int] = 1");
    assert_contains(&body, "def _pack_fixed(self, _bits: _Bits, _off: int) -> None:");
    assert_contains(&body, "return cls()");
    assert_valid(&module, "padding_only");
}

#[test]
fn field_names_that_collide_with_python_keywords_are_escaped() {
    // §1 does not reserve Python's vocabulary, so the backend has to cope. The
    // schema's own spelling still has to appear in the error messages, since
    // that is the name the author would go looking for.
    let src = schema("struct S: u32 { class: u8, def: u8, lambda: u8, None: u8, }");
    let module = module_of(&src, "keywords");
    for name in ["class_", "def_", "lambda_", "None_"] {
        assert_contains(&module, &format!("    {name}: int = 0"));
    }
    assert_contains(&module, "_check_uint(self.class_, 8, \"S.class\")");
    assert_valid(&module, "keyword_fields");
}

#[test]
fn field_names_that_would_shadow_the_generated_api_are_escaped() {
    // A field called `encode` would replace the method that encodes it, and one
    // called `field` would shadow `dataclasses.field` in its own default — both
    // at class-construction time, so the module would not even import.
    let src = schema(
        "struct S: u40 { encode: u8, field: u8, SIZE: u8, dataclass: u8, self: u8, }\n\
         service Svc(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): S;\n\
         }",
    );
    let module = module_of(&src, "shadow");
    for name in ["encode_", "field_", "SIZE_", "dataclass_", "self_"] {
        assert_contains(&module, &format!("    {name}: int = 0"));
    }
    assert_contains(&class_body(&module, "S"), "def encode(self) -> bytes:");
    assert_valid(&module, "shadowing_fields");
}

#[test]
fn type_names_that_would_shadow_a_builtin_are_escaped() {
    // A schema type becomes a module-level class, so a type named `int` would
    // shadow the builtin for the whole module — including `_Bits`, which is
    // built out of `int.from_bytes`. The module would import and then fail on
    // the first encode.
    let src = schema(
        "struct int: u8 { x: u8, }\n\
         struct bytes: u8 { y: u8, }\n\
         struct list: u8 { z: u8, }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): int;\n\
         }",
    );
    let module = module_of(&src, "builtins");
    assert_contains(&module, "class int_:");
    assert_contains(&module, "class bytes_:");
    assert_contains(&module, "class list_:");
    assert_contains(
        &module,
        "return cls(len(data), big, int.from_bytes(data, \"big\" if big else \"little\"))",
    );
    // The schema's own spelling still names the type in its error messages.
    assert_contains(&module, "f\"int: expected 1 bytes, got {len(data)}\"");
    assert_valid(&module, "builtin_type_names");
}

#[test]
fn names_are_recased_without_mangling_acronyms() {
    let src = schema("struct HTTPProxyID: u8 { x: u8, }");
    let module = module_of(&src, "acronym");
    // A type keeps the author's spelling, since Python classes are CamelCase
    // already; only the derived constant names are re-cased.
    assert_contains(&module, "class HTTPProxyID:");
    assert_valid(&module, "acronym");

    let src = schema(
        "alias HTTPProxyID = u8;\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): HTTPProxyID;\n\
         }",
    );
    let module = module_of(&src, "acronym2");
    assert_contains(&module, "def encode_http_proxy_id(");
    assert_contains(&module, "HTTP_PROXY_ID_SIZE: Final = 1");
}

// ---------------------------------------------------------------------------
// Codec surface (§12)
// ---------------------------------------------------------------------------

#[test]
fn every_root_type_gets_an_encode_and_a_decode() {
    let module = example_module();
    let model = model_of(EXAMPLE);
    let roots: Vec<&defgen::model::TypeDef> = model.roots().collect();
    assert!(roots.iter().any(|t| t.name == "Status") && roots.iter().any(|t| t.name == "OwnerName"));

    for root in roots {
        match root.kind {
            // A struct or a union is a class, so its codec is a pair of methods.
            TypeKind::Struct(_) | TypeKind::Union(_) => {
                let body = class_body(&module, &root.name);
                assert_contains(&body, "def encode(self) -> bytes:");
                assert_contains(&body, &format!("def decode(cls, data: bytes) -> {}:", root.name));
            }
            // An alias, an enum or a scaled type has no class of its own (§3),
            // so its codec is a pair of module-level functions instead.
            _ => {
                let fnp = snake(&root.name);
                assert_contains(&module, &format!("def encode_{fnp}("));
                assert_contains(&module, &format!("def decode_{fnp}("));
            }
        }
    }
}

#[test]
fn a_type_that_is_only_ever_nested_gets_no_entry_points() {
    // §8, §10: byte order is a property of the root container, so a type that
    // is only ever nested has no byte order of its own to encode in.
    let module = example_module();
    let body = class_body(&module, "Orientation");
    assert_contains(&body, "def _pack_fixed(self, _bits: _Bits, _off: int) -> None:");
    assert!(!body.contains("def encode(self)"), "a nested-only type needs no entry point");
    assert!(!body.contains("def decode("), "on either side");
}

#[test]
fn sizes_are_exposed_as_class_constants() {
    let module = example_module();
    assert_contains(&class_body(&module, "Status"), "SIZE: ClassVar[int] = 8");
    assert_contains(&class_body(&module, "Orientation"), "SIZE: ClassVar[int] = 3");
    assert_contains(&class_body(&module, "LegacySerial"), "SIZE: ClassVar[int] = 4");
    // §6.3: a variable-length type has a prefix and a maximum instead.
    let label = class_body(&module, "DiagnosticLabel");
    assert_contains(&label, "FIXED_SIZE: ClassVar[int] = 1");
    assert_contains(&label, "MAX_SIZE: ClassVar[int] = 25");
    assert!(!label.lines().any(|l| l.trim_start().starts_with("SIZE:")), "a varying size is not one number");
    assert_contains(&label, "def encoded_size(self) -> int:");
    // An alias has no class, so its sizes are module constants.
    assert_contains(&module, "OWNER_NAME_FIXED_SIZE: Final = 0");
    assert_contains(&module, "OWNER_NAME_MAX_SIZE: Final = 32");
}

#[test]
fn byte_order_is_resolved_per_root_container() {
    // §8: the file default, and the one struct that overrides it. Byte order
    // reaches the wire in exactly one place — the container the bits are packed
    // into, which carries both its size and its byte order.
    let module = example_module();
    assert_contains(&class_body(&module, "Status"), "_bits = _Bits(8, big=False)");
    assert_contains(&class_body(&module, "LegacySerial"), "_bits = _Bits(4, big=True)");
    assert_contains(&class_body(&module, "LegacySerial"), "_Bits.from_bytes(data, big=True)");
    assert_contains(
        &module,
        "return cls(len(data), big, int.from_bytes(data, \"big\" if big else \"little\"))",
    );

    // The mirror is what makes a big-endian container fill from its
    // most-significant end while its fields stay in declaration order (§6).
    assert_contains(&module, "return self.size * 8 - off - bits if self.big else off");
}

#[test]
fn a_variable_length_field_is_a_str_or_a_list() {
    // §12: Python gets the idiomatic container, not C's buffer-plus-length.
    let module = example_module();
    assert_contains(&module, "    label: str = \"\"");
    assert_contains(&module, "    samples: list[Temperature] = ");
    assert!(!module.contains("label_len"), "a Python container carries its own length");
    // §6.3: the length comes from the transport, so decode divides rather than
    // reading a prefix — and encode never pads out to the maximum.
    assert_contains(&class_body(&module, "DiagnosticLabel"), "return self.FIXED_SIZE + self._tail_len()");
}

#[test]
fn a_container_default_goes_through_default_factory() {
    // A mutable default shared between every instance is the classic dataclass
    // bug, and here it would mean two decoded values aliasing one list.
    let module = example_module();
    assert_contains(
        &module,
        "samples: list[Temperature] = field(default_factory=lambda: [0.0 for _ in range(4)])",
    );
    assert_contains(&module, "orientation: Orientation = field(default_factory=Orientation)");
    assert_contains(
        &module,
        "points: list[Orientation] = field(default_factory=lambda: [Orientation() for _ in range(2)])",
    );
    // Anchored to a field line: a `Temperature[max: N]` tail declares a local
    // `list[Temperature] = []` inside its unpack, which is not a shared default.
    assert!(
        !module.contains("\n    samples: list[Temperature] = ["),
        "a list literal default would be shared"
    );
}

#[test]
fn a_fixed_array_is_checked_for_its_exact_count() {
    // §6.1: a fixed array carries exactly its declared count, always — Python's
    // list would happily hold any number.
    let module = example_module();
    assert_contains(&module, "_check_count(self.samples, 4, \"TemperatureLog.samples\")");
    assert_contains(&module, "_check_count(self.points, 2, \"MotionPath.points\")");
}

#[test]
fn declared_padding_is_validated_and_bare_padding_is_not() {
    // §6.2: `padding: uN = 0` is a claim about the wire; bare padding is not.
    let module = example_module();
    assert_contains(&module, "raise DefgenPaddingError(\"MotionPath: padding at bits 48..64 is not zero\")");
    assert_eq!(
        module.matches("raise DefgenPaddingError").count(),
        1,
        "the example declares exactly one `padding = 0` run"
    );
    // Reserved bits are neither: they are carried through untouched (§6.2).
    assert_contains(&module, "    flags: int = 0");
    assert_contains(&module, "flags=_bits.get(_off + 60, 4)");
}

#[test]
fn an_alias_of_a_variable_length_type_is_bindable_on_its_own() {
    // §6.3: "one characteristic is just a name" needs no wrapping class.
    let module = example_module();
    assert_contains(&module, "OwnerName: TypeAlias = str");
    assert_contains(&module, "def encode_owner_name(value: OwnerName) -> bytes:");
    assert_contains(&module, "def decode_owner_name(data: bytes) -> OwnerName:");
    assert!(!module.contains("class OwnerName"), "an alias generates no runtime type");
    assert_contains(&module, "_decode_utf8(data, \"OwnerName\")");
}

#[test]
fn utf8_is_validated_rather_than_patched_up() {
    // §6.3: malformed input fails; it is never replaced with U+FFFD, which
    // would turn a transport bug into silently wrong data.
    let module = example_module();
    assert_contains(&module, "return data.decode(\"utf-8\")");
    assert!(!module.contains("errors=\"replace\""), "decoding must be strict");
    assert!(!module.contains("errors=\"ignore\""), "on both counts");
    assert_contains(&module, "raise DefgenUtf8Error");
}

#[test]
fn every_failure_is_a_defgen_error() {
    // §12: one base class, so a caller can catch the lot with one `except`.
    let module = example_module();
    assert_contains(&module, "class DefgenError(Exception):");
    for sub in ["Length", "Range", "UnknownValue", "Padding", "Utf8"] {
        assert_contains(&module, &format!("class Defgen{sub}Error(DefgenError):"));
    }
    assert!(!module.contains("raise ValueError"), "a caller should not have to know the internals");
    assert!(!module.contains("assert "), "generated code must not rely on `assert`, which -O strips");
}

#[test]
fn gatt_metadata_becomes_module_constants() {
    // §10: UUIDs and properties, with what to do with them left to the caller.
    let module = example_module();
    assert_contains(&module, "HEARING_AID_CONTROL_UUID: Final = \"7d8f0000-3c1a-4e8a-9b5a-000000000000\"");
    assert_contains(
        &module,
        "HEARING_AID_CONTROL_STATUS_CHAR_UUID: Final = \"7d8f0001-3c1a-4e8a-9b5a-000000000000\"",
    );
    assert_contains(&module, "class GattProperty(enum.Flag):");
    assert_contains(&module, "properties=GattProperty.READ | GattProperty.NOTIFY,");
    assert_contains(&module, "SERVICES: Final[tuple[GattService, ...]] = (HEARING_AID_CONTROL,)");
}

#[test]
fn a_schema_with_no_services_emits_no_gatt_section() {
    let module = module_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!module.contains("GattProperty"));
    assert!(!module.contains("SERVICES"));
    assert_valid(&module, "no_services");
}

// ---------------------------------------------------------------------------
// Features the worked example does not reach
// ---------------------------------------------------------------------------

#[test]
fn a_variable_length_array_tail_imports() {
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
    let module = module_of(&src, "batch");
    assert_contains(&module, "    readings: list[Reading] = field(default_factory=list)");
    assert_contains(&module, "_check_max(self.readings, 8, \"Batch.readings\")");
    assert_contains(&module, "if len(_data) % 2 != 0:");
    assert_contains(&module, "_count = len(_data) // 2");
    assert_contains(&module, "if _count > 8:");
    assert_contains(&module, "return len(self.readings) * 2");
    assert_valid(&module, "var_array_tail");
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
    let module = module_of(&src, "nested");
    let outer = class_body(&module, "Outer");
    assert_contains(&outer, "FIXED_SIZE: ClassVar[int] = 2");
    assert_contains(&outer, "MAX_SIZE: ClassVar[int] = 6");
    assert_contains(&outer, "return self.inner._tail_len()");
    assert_contains(&outer, "return self.inner._pack_tail(_big)");
    assert_contains(&outer, "self.inner._unpack_tail(_data, _big)");
    // Only the outer type is bound, so only it gets an entry point.
    assert!(!class_body(&module, "Inner").contains("def encode(self)"));
    assert_valid(&module, "nested_var_struct");
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
    let module = module_of(&src, "note");
    let body = class_body(&module, "Note");
    assert_contains(&body, "_bits = _Bits(2, big=True)");
    assert_contains(&body, "return _bits.to_bytes() + self._pack_tail(True)");
    assert_contains(&body, "_Bits.from_bytes(data[: cls.FIXED_SIZE], big=True)");
    assert_contains(&body, "value._unpack_tail(data[cls.FIXED_SIZE :], True)");
    assert_valid(&module, "big_endian_var");
}

#[test]
fn an_enum_bound_through_an_alias_gets_its_own_entry_points() {
    // §3 with §10: `mark_root` follows the alias chain, so binding `Bound`
    // makes `E` a root too — and a root needs a codec.
    let src = schema(
        "enum E: u8 { A = 1, B = 2, }\n\
         alias Bound = E;\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Bound;\n\
         }",
    );
    let module = module_of(&src, "bound");
    assert_contains(&module, "def encode_e(");
    assert_contains(&module, "def decode_e(");
    assert_contains(&module, "E_SIZE: Final = 1");
    assert_valid(&module, "alias_bound_enum");
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

/// Code generation is total: any model the checker accepts must produce a
/// module rather than a panic.
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
        let module = backends::python::PythonBackend.generate(&model, &Options::default());
        let contents = &module.single().contents;
        assert!(!contents.is_empty(), "line {skip} produced an empty module");
        assert_fully_annotated(contents, &format!("mutation {skip}"));
        generated += 1;
    }
    assert!(generated > 10, "only {generated} mutations checked clean; the test is not exercising much");
}

// ---------------------------------------------------------------------------
// Conformance: the generated module, imported and run
// ---------------------------------------------------------------------------

/// Runs the hand-written conformance fixture against the worked example's
/// generated module. The fixture's byte strings are derived from SPEC.md by
/// hand and shared with the C backend's fixture, so this is the test that would
/// catch the emitter and the spec — or the two backends — disagreeing.
#[test]
fn the_generated_module_round_trips_the_worked_example() {
    let Some(python) = python() else {
        eprintln!("skipping the Python conformance run: no Python 3.10+ interpreter found");
        return;
    };
    let dir = scratch("conformance");
    std::fs::write(dir.join("commands.py"), example_module()).unwrap();

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples/python_conformance.py");
    std::fs::copy(&fixture, dir.join("conformance.py")).expect("failed to stage the fixture");

    let run = Command::new(&python)
        .current_dir(&dir)
        // No `__pycache__`, and no site-packages: whatever the module imports
        // has to be the standard library, or this run fails.
        .args(["-B", "-s", "conformance.py"])
        .output()
        .expect("failed to run the conformance fixture");

    assert!(
        run.status.success(),
        "the generated codecs disagree with the spec:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
