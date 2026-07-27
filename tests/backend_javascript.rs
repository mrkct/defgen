//! The JavaScript backend.
//!
//! Three layers of test live here. The cheap ones read the generated text and
//! assert on what it declares. The middle one hands the module to a real
//! JavaScript engine and imports it, which evaluates every class body, static
//! field and frozen object the emitter wrote — so a name used before it exists,
//! or a default that cannot be built, shows up here rather than in a user's
//! program. The expensive one runs `tests/examples/javascript_conformance.mjs`
//! against the generated module, which is what actually pins the wire format
//! down: a codegen bug that produces valid JavaScript saying the wrong thing is
//! invisible to string matching.
//!
//! The conformance fixture asserts the very same byte strings as its C, Python,
//! Java, Kotlin and Swift counterparts. That is the point of §13 — several
//! backends, one wire format — so if these ever disagree, one of the backends
//! is wrong.
//!
//! Anything needing an engine is skipped, loudly, when there isn't one, so this
//! file still does useful work on a machine without Node. The textual checks,
//! including the JSDoc sweep, run either way.

use std::path::{Path, PathBuf};
use std::process::Command;

use defgen::backends::{self, Backend, Options};
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
    let generated = backends::javascript::JavaScriptBackend.generate(&model, &opts);
    assert_eq!(generated.files.len(), 1, "the JavaScript backend emits exactly one file");
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
    let exact = format!("export class {name} {{");
    let derived = format!("export class {name} extends ");
    let start = module
        .lines()
        .position(|l| l == exact || l.starts_with(&derived))
        .unwrap_or_else(|| panic!("generated module declares no `class {name}`"));
    module
        .lines()
        .skip(start + 1)
        .take_while(|l| l.is_empty() || l.starts_with("  "))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// JSDoc (§12 — the generated API is fully typed)
// ---------------------------------------------------------------------------

/// One function, method or constructor the emitter wrote, as
/// `(line number, name, parameter names)`.
fn definitions(module: &str) -> Vec<(usize, String, Vec<String>)> {
    const NOT_A_DEFINITION: &[&str] =
        &["if", "for", "while", "switch", "catch", "do", "else", "try", "return"];
    let mut out = Vec::new();
    for (i, line) in module.lines().enumerate() {
        let text = line.trim_start();
        if !text.ends_with(") {") {
            continue;
        }
        let text = text.strip_prefix("export ").unwrap_or(text);
        let text = text.strip_prefix("static ").unwrap_or(text);
        let text = text.strip_prefix("function ").unwrap_or(text);
        let Some(open) = text.find('(') else { continue };
        let name = &text[..open];
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$') {
            continue;
        }
        if NOT_A_DEFINITION.contains(&name) {
            continue;
        }
        let close = text.rfind(')').expect("ends with `) {`");
        let params: Vec<String> = text[open + 1..close]
            .split(',')
            .map(|p| p.split('=').next().unwrap_or("").trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        out.push((i + 1, name.to_string(), params));
    }
    out
}

/// The JSDoc block immediately above `line`, or `None` if there is not one.
fn jsdoc_above(module: &str, line: usize) -> Option<String> {
    let lines: Vec<&str> = module.lines().collect();
    let mut end = line.checked_sub(2)?; // 1-based line number to 0-based index above
    if !lines[end].trim_end().ends_with("*/") {
        return None;
    }
    let mut block = Vec::new();
    loop {
        block.push(lines[end]);
        if lines[end].trim_start().starts_with("/**") {
            break;
        }
        end = end.checked_sub(1)?;
    }
    block.reverse();
    Some(block.join("\n"))
}

/// Every function the emitter writes is documented, and documents each of its
/// parameters — the JSDoc block *is* the type declaration in a `.mjs` with no
/// `.d.ts` beside it, so a missing one is a missing type.
fn assert_documented(module: &str, name: &str) {
    let definitions = definitions(module);
    for (line, fn_name, params) in &definitions {
        let at = format!("{name}:{line}");
        let block = jsdoc_above(module, *line)
            .unwrap_or_else(|| panic!("{at}: `{fn_name}` has no JSDoc block above it"));
        for param in params {
            let documented = block.contains("@param {") // a type is always given
                && (block.contains(&format!("}} {param}"))
                    || block.contains(&format!("}} [{param}]"))
                    || block.contains(&format!("}} [{param}=")));
            assert!(documented, "{at}: `{fn_name}`'s parameter `{param}` is not in its JSDoc:\n{block}");
        }
    }
    assert!(
        definitions.len() > 5,
        "the JSDoc sweep found only {} definitions in `{name}`",
        definitions.len()
    );
}

// ---------------------------------------------------------------------------
// JavaScript engine
// ---------------------------------------------------------------------------

/// The engine to test against, or `None` if this machine has none.
///
/// Node 16 is the floor the emitter targets — class static fields and `??` are
/// what set it — so an older one is treated as no engine rather than as a
/// failure.
fn node() -> Option<String> {
    const CHECK: &str = "process.exit(Number(process.versions.node.split('.')[0]) >= 16 ? 0 : 1)";
    let candidates = std::env::var("NODE").into_iter().chain(["node".into()]);
    candidates
        .into_iter()
        .find(|n| Command::new(n).args(["-e", CHECK]).output().is_ok_and(|o| o.status.success()))
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("javascript_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Checks a module the cheap way, then — if this machine has an engine —
/// imports it, which evaluates every class body and static field the emitter
/// wrote.
fn assert_valid(module: &str, name: &str) {
    assert_documented(module, name);

    let Some(node) = node() else {
        eprintln!("skipping the import of `{name}`: no Node found");
        return;
    };
    let dir = scratch(name);
    std::fs::write(dir.join("schema.mjs"), module).unwrap();
    // A `.mjs` is a module wherever it lands, with no `package.json` to say so —
    // which is exactly why the backend emits that extension. Importing it here
    // is what proves it.
    std::fs::write(
        dir.join("probe.mjs"),
        "import * as schema from \"./schema.mjs\";\nif (!schema) process.exit(1);\n",
    )
    .unwrap();

    let out = Command::new(&node).current_dir(&dir).arg("probe.mjs").output().expect("failed to run Node");

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
fn the_javascript_backend_is_registered_under_javascript() {
    assert!(backends::names().contains(&"javascript"));
    let backend = backends::find("javascript").expect("`javascript` resolves to a backend");
    assert_eq!(backend.name(), "javascript");
    assert!(!backend.description().is_empty());
    assert!(backends::find("JavaScript").is_none(), "backend names are matched exactly");
    assert!(backends::find("js").is_none(), "and only under the name they registered");
}

#[test]
fn the_file_stem_drives_the_module_name() {
    let (name, _) = generate(EXAMPLE, "commands");
    assert_eq!(name, "commands.mjs");

    // `.mjs` rather than `.js`: a `.js` file is ESM or CommonJS depending on
    // the nearest `package.json`, and a file that ships on its own has none.
    assert!(name.ends_with(".mjs"), "the extension has to say what the file is");

    // A stem that is not a legal file-name-shaped identifier still has to
    // produce one.
    assert_eq!(generate(EXAMPLE, "my-schema.v2").0, "my_schema_v2.mjs");
    assert_eq!(generate(EXAMPLE, "2fast").0, "_2fast.mjs");
    // JavaScript imports by path, not by identifier, so the author's casing
    // survives.
    assert_eq!(generate(EXAMPLE, "HearingAid").0, "HearingAid.mjs");
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
    assert!(!module.contains("\nimport "), "a generated module imports nothing");
    assert!(!module.contains("require("), "and is not CommonJS either");
    assert!(!module.contains("node:"), "nor does it reach for a Node built-in");
    // The two globals it does use are on every runtime that has ever run
    // JavaScript in a browser or a Node process.
    assert_contains(&module, "new TextEncoder()");
    assert_contains(&module, "new TextDecoder(\"utf-8\", { fatal: true })");
    assert_valid(&module, "self_contained");
}

#[test]
fn helpers_are_only_what_the_schema_needs() {
    // A module-level function nobody calls is what a linter objects to.
    let module = module_of(&schema("struct S: u8 { x: u8, }"), "s");
    assert!(!module.contains("defgenRound"), "no `scaled` type means no rounding helper");
    assert!(!module.contains("TextEncoder"), "no `string` means no UTF-8 helpers");
    assert!(!module.contains("defgenCheckCount"), "no array means no length check");
    assert!(!module.contains("defgenConcat"), "no variable-length root means no concatenation");
    assert!(!module.contains("defgenSext"), "an all-unsigned schema needs no sign extension");
    assert_contains(&module, "function defgenCheckUint(");
    assert_valid(&module, "minimal_helpers");
}

#[test]
fn the_exported_surface_is_the_public_one() {
    let module = example_module();
    for wanted in [
        "export class DefgenError",
        "export class Status",
        "export class Command",
        "export class CommandUnknown",
        "export const SERVICES",
        "export function encodeOwnerName",
    ] {
        assert_contains(&module, wanted);
    }
    // The plumbing is not exported: a module-level name is either part of the
    // API or invisible, and `DefgenBits` is the latter.
    assert_contains(&module, "\nclass DefgenBits {");
    assert!(!module.contains("export class DefgenBits"), "the bit container is internal");
    for internal in ["defgenCheckUint", "defgenSext", "defgenRound", "defgenDecodeHearingMode"] {
        assert!(
            !module.contains(&format!("export function {internal}")),
            "`{internal}` is internal and must not be exported"
        );
        assert_contains(&module, &format!("function {internal}("));
    }
}

#[test]
fn doc_comments_become_jsdoc() {
    let module = example_module();
    assert_contains(&module, " * Playback volume. The device only has 4 bits of resolution.");
    assert_contains(&module, " * Reusable 3-axis orientation reading");
}

#[test]
fn a_doc_comment_cannot_close_its_own_comment_block() {
    // A block comment ends at the first `*/`, so a doc comment containing one
    // would end the JSDoc early and spill the rest into the code.
    let src = schema("/// ends a comment */ right here\nstruct S: u8 { x: u8, }");
    let module = module_of(&src, "s");
    assert!(!module.contains("*/ right here"), "`*/` inside a doc comment must be escaped");
    assert_contains(&module, "*\\/ right here");
    assert_valid(&module, "comment_escape");
}

// ---------------------------------------------------------------------------
// Type mapping (§2, §3, §4, §5, §6, §7)
// ---------------------------------------------------------------------------

#[test]
fn a_value_is_a_number_up_to_a_32_bit_carrier_and_a_bigint_past_it() {
    // §2: `number` is a double, so it holds every value a 32-bit carrier can
    // and not every value a 64-bit one can. That is the same line `DataView`
    // draws between `getUint32` and `getBigUint64`.
    let src = schema(
        "struct Widths: u256 {\n\
             a: u1,\n\
             b: u4,\n\
             c: u9,\n\
             e: u32,\n\
             f: i8,\n\
             g: u33,\n\
             h: i48,\n\
             padding: u121,\n\
         }",
    );
    let module = module_of(&src, "widths");
    for f in ["a", "b", "c", "e", "f"] {
        assert_contains(&module, &format!("this.{f} = init.{f} ?? 0;"));
    }
    for f in ["g", "h"] {
        assert_contains(&module, &format!("this.{f} = init.{f} ?? 0n;"));
    }
    assert_contains(&module, "@param {number} [init.e]");
    assert_contains(&module, "@param {bigint} [init.g]");

    // The check is at the declared width, not the carrier's.
    assert_contains(&module, "defgenCheckUint(this.a, 1, \"Widths.a\")");
    assert_contains(&module, "defgenCheckUint(this.g, 33, \"Widths.g\")");
    assert_contains(&module, "defgenCheckInt(this.f, 8, \"Widths.f\")");
    assert_contains(&module, "defgenCheckInt(this.h, 48, \"Widths.h\")");
    // A signed value is sign-extended back out of its declared width, never
    // read as the unsigned bit pattern — and only a `number` carrier converts.
    assert_contains(&module, "f: Number(defgenSext(bits.get(off + 46, 8), 8))");
    assert_contains(&module, "h: defgenSext(bits.get(off + 87, 48), 48)");
    assert_valid(&module, "widths");
}

#[test]
fn values_wider_than_64_bits_need_no_special_case() {
    // §2: 128 bits is the ceiling. A `bigint` reaches it with nothing extra.
    let src = schema(
        "struct Wide: u128 {\n\
             id: u128,\n\
         }\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): Wide;\n\
         }",
    );
    let module = module_of(&src, "wide");
    assert_contains(&module, "this.id = init.id ?? 0n;");
    assert_contains(&module, "defgenCheckUint(this.id, 128, \"Wide.id\")");
    assert_contains(&module, "static SIZE = 16;");
    assert_valid(&module, "int128");
}

#[test]
fn an_alias_keeps_the_name_the_author_declared() {
    // §3: an alias generates no runtime type, but the domain name survives —
    // and a `@typedef` is exactly that: a name, with nothing behind it.
    let module = example_module();
    assert_contains(&module, "@typedef {number} Volume");
    assert_contains(&module, "@param {Volume} [init.volume]");
    assert_contains(&module, "@type {Volume}");
    assert!(!module.contains("class Volume"), "an alias generates no runtime type");
}

#[test]
fn a_scaled_type_exposes_both_representations() {
    // §4: the physical value to callers, the raw integer for exact round trips.
    let module = example_module();
    assert_contains(&module, "@typedef {number} Temperature");
    assert_contains(&module, "export const TEMPERATURE_SCALE = 0.01;");
    assert_contains(&module, "export const TEMPERATURE_OFFSET = 0.0;");
    assert_contains(&module, "export function temperatureFromRaw(raw) {");
    assert_contains(&module, "export function temperatureToRaw(value) {");
    assert_contains(&module, "throw new DefgenRangeError");
}

#[test]
fn scaled_rounding_uses_neither_math_round_nor_a_library() {
    // §4, §13: the backends have to agree on a raw integer down to the last
    // unit, and `Math.round` rounds half *up* — -0.5 to -0, where C's `round`
    // gives -1 — so it would disagree with every other backend below zero.
    let module = example_module();
    assert_contains(&module, "function defgenRound(value, where) {");
    assert_contains(
        &module,
        "const raw = defgenRound((value - TEMPERATURE_OFFSET) / TEMPERATURE_SCALE, \"Temperature\");",
    );
    assert!(!module.contains("Math.round("), "`Math.round` would disagree below zero");
    assert_contains(&module, "  const whole = Math.trunc(value);");
    assert_contains(&module, "  const remainder = value - whole;");
    // The bias-then-truncate shortcut is wrong at the double just below 0.5.
    assert!(!module.contains("value + 0.5"), "no bias is added before truncating");
}

#[test]
fn a_scaled_value_that_cannot_be_rounded_is_a_range_error() {
    // A NaN or an infinity has no integer to round to, and it has to fail as a
    // DefgenError rather than as a RangeError out of `BigInt()`.
    let module = example_module();
    assert_contains(&module, "if (!Number.isFinite(value)) {");
    assert_contains(&module, "cannot be rounded to an integer");
}

#[test]
fn an_integer_field_rejects_a_number_that_is_not_one() {
    // §2 with JavaScript's own numbers: every integer field is a double, so
    // `1.5` and `NaN` arrive as ordinary values rather than as type errors.
    let module = example_module();
    assert_contains(&module, "function defgenInt(value, where) {");
    assert_contains(&module, "Number.isInteger(value)");
    assert_contains(&module, "is not an integer");
}

#[test]
fn enum_variants_become_members_of_a_frozen_object() {
    // §12: casing is converted to the target language's convention, which for
    // an object standing in for an `enum` is the one TypeScript's own uses.
    let module = example_module();
    assert_contains(&module, "export const HearingMode = Object.freeze({");
    assert_contains(&module, "  Default: 0,");
    assert_contains(&module, "  Cinema: 3,");
    assert_contains(&module, "@typedef {number} HearingMode");
}

#[test]
fn an_implicitly_numbered_enum_gets_the_values_the_checker_resolved() {
    // §5: the counter advances from the last explicit value it saw.
    let src = schema(
        "enum E: u8 { A, B = 7, C, D, }\nalias Bound = E;\nservice S(uuid: \"180a\") {\n    characteristic C(uuid: \"2a00\", properties: [read]): Bound;\n}",
    );
    let module = module_of(&src, "e");
    for member in ["  A: 0,", "  B: 7,", "  C: 8,", "  D: 9,"] {
        assert_contains(&module, member);
    }
    assert_valid(&module, "implicit_numbering");
}

#[test]
fn a_closed_enum_rejects_an_unmatched_value_on_both_sides() {
    // §5, §12: decoding an undeclared value must be fallible.
    let src = schema("enum Mode: u8 { A = 0, B = 1, }\nstruct S: u8 { m: Mode, }");
    let module = module_of(&src, "closed");
    assert_eq!(
        module.matches("throw new DefgenUnknownValueError").count(),
        2,
        "a closed enum is validated on encode and on decode"
    );
    assert_contains(&module, "function defgenDecodeMode(raw) {");
    assert!(!module.contains("ModeValue"), "a closed enum has no second inhabitant to name");
    assert!(!module.contains("class ModeUnknown"), "and no fallback class");
    assert_valid(&module, "closed_enum");
}

#[test]
fn an_open_enum_decodes_an_undeclared_value_into_its_own_type() {
    // §5, §12: the unknown case is a distinct type carrying the raw value, so
    // it can never be confused with a declared one — which a bare number could
    // be, since every declared variant is one.
    let module = example_module();
    assert_contains(&module, "export class HearingModeUnknown {");
    assert_contains(&module, "@typedef {HearingMode | HearingModeUnknown} HearingModeValue");
    assert_contains(&module, "this.mode = init.mode ?? HearingMode.Default;");
    assert_contains(&module, "      return new HearingModeUnknown(raw);");
    assert!(
        !module.contains("function defgenDecodeHearingMode(raw) {\n  switch (raw) {\n    case HearingMode.Default:\n    case HearingMode.Stereo:\n    case HearingMode.Mono:\n    case HearingMode.Cinema:\n      return raw;\n    default:\n      throw"),
        "an open enum must not reject any wire value"
    );
    // A value it keeps has to be re-encodable, or the round trip is lossy.
    assert_contains(&module, "return value instanceof HearingModeUnknown ? value.raw : value;");
    // and it is frozen, so a decoded value cannot be edited into a different
    // one behind the caller's back.
    assert_contains(&class_body(&module, "HearingModeUnknown"), "Object.freeze(this);");
}

#[test]
fn a_tagged_union_becomes_a_class_hierarchy() {
    // §7, §12: one class per variant under a shared base, so a decoded value is
    // matched with `instanceof` rather than by reading a tag by hand.
    let module = example_module();
    assert_contains(&module, "export class Command {");
    assert_contains(&module, "export class CommandSetVolume extends Command {");
    assert_contains(&module, "export class CommandTriggerFactoryReset extends Command {");
    assert_contains(&module, "export class CommandUnknown extends Command {");
    assert_contains(&module, "static ID = 0x1;");
    assert_contains(&module, "static ID = 0xffff;");
    assert_contains(&module, "static TAG_BITS = 16;");
    // Dispatch is a switch on the id, so a decode is one read and one jump.
    assert_contains(&module, "    const tag = Number(bits.get(off, 16));");
    assert_contains(&module, "      case 0x1:\n        return CommandSetVolume._unpackPayload(bits, off);");
    // The base is uninhabited: every `Command` is one of its variants (§7).
    assert_contains(&module, "    if (new.target === Command) {");
    // A payload-less variant is still a class; it just has no fields.
    let reset = class_body(&module, "CommandTriggerFactoryReset");
    assert!(!reset.contains("init."), "TriggerFactoryReset declares no payload");
    assert_contains(&reset, "bits.put(off, 16, 0xffffn);");
}

#[test]
fn a_union_variant_carries_the_unions_id_but_not_a_mutable_one() {
    // §7: the id is a property of the variant's class, never of an instance, so
    // it cannot be set to something that disagrees with the class it is on.
    let module = example_module();
    let body = class_body(&module, "CommandSetVolume");
    assert_contains(&body, "static ID = 0x1;");
    assert!(!body.contains("this.id"), "a known variant has no per-instance id to get wrong");
    // The fallback variant is the one exception: its id is data, by definition.
    let unknown = class_body(&module, "CommandUnknown");
    assert_contains(&unknown, "this.id = init.id ?? 0;");
    // Its 48-bit payload is past what a `number` holds exactly (§2).
    assert_contains(&unknown, "this.raw = init.raw ?? 0n;");
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
    assert_contains(&class_body(&module, "Cmd"), "throw new DefgenUnknownValueError");
    assert_valid(&module, "closed_union");
}

#[test]
fn a_struct_of_only_padding_still_produces_a_legal_class() {
    // §6.2 allows a container of pure padding. It still has to declare its size
    // and pack nothing — and needs no constructor at all, since an `init`
    // nothing reads is exactly the unused parameter a linter objects to.
    let src = schema("struct Blank: u8 { padding: u8, }");
    let module = module_of(&src, "blank");
    let body = class_body(&module, "Blank");
    assert_contains(&body, "static SIZE = 1;");
    assert_contains(&body, "_packFixed(bits, off) {");
    assert_contains(&body, "return new Blank();");
    assert!(!body.contains("constructor"), "a fieldless class needs no constructor");
    assert_valid(&module, "padding_only");
}

#[test]
fn field_names_that_collide_with_javascript_keywords_are_escaped() {
    // §1 does not reserve JavaScript's vocabulary, so the backend has to cope.
    // The schema's own spelling still has to appear in the error messages,
    // since that is the name the author would go looking for.
    let src = schema("struct S: u32 { class: u8, new: u8, typeof: u8, null: u8, }");
    let module = module_of(&src, "keywords");
    for name in ["class_", "new_", "typeof_", "null_"] {
        assert_contains(&module, &format!("this.{name} = init.{name} ?? 0;"));
    }
    assert_contains(&module, "defgenCheckUint(this.class_, 8, \"S.class\")");
    assert_valid(&module, "keyword_fields");
}

#[test]
fn field_names_that_would_shadow_the_generated_api_are_escaped() {
    // A property called `encode` would shadow the method that encodes it, and
    // one called `constructor` would replace the class's own — both silently.
    let src = schema(
        "struct S: u40 { encode: u8, decode: u8, constructor: u8, prototype: u8, encodedSize: u8, }\n\
         service Svc(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): S;\n\
         }",
    );
    let module = module_of(&src, "shadow");
    for name in ["encode_", "decode_", "constructor_", "prototype_", "encodedSize_"] {
        assert_contains(&module, &format!("this.{name} = init.{name} ?? 0;"));
    }
    assert_contains(&class_body(&module, "S"), "  encode() {");

    // A field spelled like the emitter's own internals is not one: `snake`
    // drops the leading underscore, so it lands on an ordinary property name
    // rather than on the method it looks like.
    let internals = module_of(&schema("struct T: u8 { _packFixed: u8, }"), "internals");
    assert_contains(&internals, "this.packFixed = init.packFixed ?? 0;");
    assert!(!internals.contains("this._packFixed ="), "and never on the method itself");
    assert_valid(&internals, "internal_looking_field");
    assert_valid(&module, "shadowing_fields");
}

#[test]
fn type_names_that_would_shadow_a_global_are_escaped() {
    // A schema type becomes a top-level `class`, so a type named `TextDecoder`
    // would shadow the global the UTF-8 helpers are built from — and the module
    // would evaluate happily and then fail on the first decode.
    let src = schema(
        "struct TextDecoder: u8 { x: u8, }\n\
         struct Uint8Array: u8 { y: u8, }\n\
         struct Object: u8 { z: u8, }\n\
         alias Note = string(max: 4);\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): TextDecoder;\n\
             characteristic D(uuid: \"2a01\", properties: [read]): Note;\n\
         }",
    );
    let module = module_of(&src, "globals");
    assert_contains(&module, "export class TextDecoder_ {");
    assert_contains(&module, "export class Uint8Array_ {");
    assert_contains(&module, "export class Object_ {");
    assert_contains(&module, "new TextDecoder(\"utf-8\", { fatal: true })");
    // The schema's own spelling still names the type in its error messages.
    assert_contains(&module, "`TextDecoder: expected 1 bytes, got ${data.length}`");
    assert_valid(&module, "global_type_names");
}

#[test]
fn names_are_recased_without_mangling_acronyms() {
    let src = schema("struct HTTPProxyID: u8 { x: u8, }");
    let module = module_of(&src, "acronym");
    // A type keeps the author's spelling, since JavaScript classes are
    // PascalCase already; only the derived names are re-cased.
    assert_contains(&module, "export class HTTPProxyID {");
    assert_valid(&module, "acronym");

    let src = schema(
        "alias HTTPProxyID = u8;\n\
         service S(uuid: \"180a\") {\n\
             characteristic C(uuid: \"2a00\", properties: [read]): HTTPProxyID;\n\
         }",
    );
    let module = module_of(&src, "acronym2");
    assert_contains(&module, "export function encodeHttpProxyId(");
    assert_contains(&module, "export const HTTP_PROXY_ID_SIZE = 1;");
    assert_valid(&module, "acronym2");
}

#[test]
fn field_names_become_camel_case() {
    // §12: `active_profile` is `activeProfile`, the convention JavaScript
    // shares with Kotlin and Swift.
    let module = example_module();
    assert_contains(&module, "this.activeProfile = init.activeProfile ?? 0;");
    assert!(!module.contains("this.active_profile"), "a property keeps JavaScript's casing");
    // and the error message keeps the schema's.
    assert_contains(&module, "\"Status.active_profile\"");
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
                assert_contains(&body, "  encode() {");
                assert_contains(&body, "  static decode(data) {");
            }
            // An alias, an enum or a scaled type has no class of its own (§3),
            // so its codec is a pair of module-level functions instead.
            _ => {
                assert_contains(&module, &format!("export function encode{}(", root.name));
                assert_contains(&module, &format!("export function decode{}(", root.name));
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
    assert_contains(&body, "_packFixed(bits, off) {");
    assert!(!body.contains("  encode() {"), "a nested-only type needs no entry point");
    assert!(!body.contains("static decode("), "on either side");
}

#[test]
fn sizes_are_exposed_as_static_class_fields() {
    let module = example_module();
    assert_contains(&class_body(&module, "Status"), "static SIZE = 8;");
    assert_contains(&class_body(&module, "Orientation"), "static SIZE = 3;");
    assert_contains(&class_body(&module, "LegacySerial"), "static SIZE = 4;");
    // §6.3: a variable-length type has a prefix and a maximum instead.
    let label = class_body(&module, "DiagnosticLabel");
    assert_contains(&label, "static FIXED_SIZE = 1;");
    assert_contains(&label, "static MAX_SIZE = 25;");
    assert!(!label.contains("static SIZE ="), "a varying size is not one number");
    assert_contains(&label, "  encodedSize() {");
    // An alias has no class, so its sizes are module constants.
    assert_contains(&module, "export const OWNER_NAME_FIXED_SIZE = 0;");
    assert_contains(&module, "export const OWNER_NAME_MAX_SIZE = 32;");
}

#[test]
fn byte_order_is_resolved_per_root_container() {
    // §8: the file default, and the one struct that overrides it. Byte order
    // reaches the wire in exactly one place — where the bits meet bytes.
    let module = example_module();
    assert_contains(&class_body(&module, "Status"), "return bits.toBytes(8, false);");
    assert_contains(&class_body(&module, "LegacySerial"), "return bits.toBytes(4, true);");
    assert_contains(&class_body(&module, "LegacySerial"), "DefgenBits.fromBytes(data, true)");
    assert_contains(&module, "      const index = big ? data.length - 1 - i : i;");
}

#[test]
fn a_variable_length_field_is_a_string_or_an_array() {
    // §12: JavaScript gets the idiomatic container, not C's buffer-plus-length.
    let module = example_module();
    assert_contains(&module, "this.label = init.label ?? \"\";");
    assert_contains(&module, "@param {Temperature[]} [init.samples]");
    assert!(!module.contains("labelLen"), "a JavaScript container carries its own length");
    // §6.3: the length comes from the transport, so decode divides rather than
    // reading a prefix — and encode never pads out to the maximum.
    assert_contains(
        &class_body(&module, "DiagnosticLabel"),
        "return DiagnosticLabel.FIXED_SIZE + this._tailLen();",
    );
}

#[test]
fn a_container_default_is_built_per_instance() {
    // A mutable default shared between every instance is the classic bug, and
    // here it would mean two decoded values aliasing one array. A constructor
    // body runs per instance, so building it there is what avoids that.
    let module = example_module();
    assert_contains(&module, "this.samples = init.samples ?? Array.from({ length: 4 }, () => 0);");
    assert_contains(&module, "this.orientation = init.orientation ?? new Orientation();");
    assert_contains(
        &module,
        "this.points = init.points ?? Array.from({ length: 2 }, () => new Orientation());",
    );
}

#[test]
fn a_fixed_array_is_checked_for_its_exact_count() {
    // §6.1: a fixed array carries exactly its declared count, always — an
    // `Array` would happily hold any number.
    let module = example_module();
    assert_contains(&module, "defgenCheckCount(this.samples, 4, \"TemperatureLog.samples\")");
    assert_contains(&module, "defgenCheckCount(this.points, 2, \"MotionPath.points\")");
    // and it is an array at all: `defgenCheckCount` is handed whatever the
    // caller put on the property.
    assert_contains(&module, "  if (!Array.isArray(seq) || seq.length !== count) {");
}

#[test]
fn declared_padding_is_validated_and_bare_padding_is_not() {
    // §6.2: `padding: uN = 0` is a claim about the wire; bare padding is not.
    let module = example_module();
    assert_contains(
        &module,
        "throw new DefgenPaddingError(\"MotionPath: padding at bits 48..64 is not zero\");",
    );
    assert_eq!(
        module.matches("DefgenPaddingError(\"").count(),
        1,
        "the example declares exactly one `padding = 0` run"
    );
    // Reserved bits are neither: they are carried through untouched (§6.2).
    assert_contains(&module, "this.flags = init.flags ?? 0;");
    assert_contains(&module, "flags: Number(bits.get(off + 60, 4))");
}

#[test]
fn an_alias_of_a_variable_length_type_is_bindable_on_its_own() {
    // §6.3: "one characteristic is just a name" needs no wrapping class.
    let module = example_module();
    assert_contains(&module, "@typedef {string} OwnerName");
    assert_contains(&module, "export function encodeOwnerName(value) {");
    assert_contains(&module, "export function decodeOwnerName(data) {");
    assert!(!module.contains("class OwnerName"), "an alias generates no runtime type");
    assert_contains(&module, "return defgenDecodeUtf8(data, \"OwnerName\");");
}

#[test]
fn utf8_is_validated_rather_than_patched_up() {
    // §6.3: malformed input fails; it is never replaced with U+FFFD, which
    // would turn a transport bug into silently wrong data. That cuts both
    // ways: `TextEncoder` substitutes for a lone surrogate, so encode has to
    // reject one rather than let it through.
    let module = example_module();
    assert_contains(&module, "new TextDecoder(\"utf-8\", { fatal: true })");
    assert!(!module.contains("fatal: false"), "decoding must be strict");
    assert_contains(&module, "  if (/\\p{Surrogate}/u.test(text)) {");
    assert_contains(&module, "throw new DefgenUtf8Error");
}

#[test]
fn every_failure_is_a_defgen_error() {
    // §12: one base class, so a caller can catch the lot with one `catch`.
    let module = example_module();
    assert_contains(&module, "export class DefgenError extends Error {");
    for sub in ["Length", "Range", "UnknownValue", "Padding", "Utf8"] {
        assert_contains(&module, &format!("export class Defgen{sub}Error extends DefgenError {{"));
    }
    // A thrown error names itself, which is what a log line and a `console`
    // trace both show.
    assert_contains(&module, "    this.name = \"DefgenLengthError\";");
    assert!(!module.contains("throw new TypeError"), "a caller should not have to know the internals");
    assert!(!module.contains("console."), "generated code does not log");
}

#[test]
fn gatt_metadata_becomes_module_constants() {
    // §10: UUIDs and properties, with what to do with them left to the caller.
    let module = example_module();
    assert_contains(
        &module,
        "export const HEARING_AID_CONTROL_UUID = \"7d8f0000-3c1a-4e8a-9b5a-000000000000\";",
    );
    assert_contains(
        &module,
        "export const HEARING_AID_CONTROL_STATUS_CHAR_UUID = \"7d8f0001-3c1a-4e8a-9b5a-000000000000\";",
    );
    assert_contains(&module, "export const GattProperty = Object.freeze({");
    assert_contains(&module, "      properties: GattProperty.READ | GattProperty.NOTIFY,");
    assert_contains(&module, "export const SERVICES = Object.freeze([HEARING_AID_CONTROL]);");
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
    assert_contains(&module, "this.readings = init.readings ?? [];");
    assert_contains(&module, "defgenCheckMax(this.readings, 8, \"Batch.readings\")");
    assert_contains(&module, "if (data.length % 2 !== 0) {");
    assert_contains(&module, "const count = data.length / 2;");
    assert_contains(&module, "if (count > 8) {");
    assert_contains(&module, "return this.readings.length * 2;");
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
    assert_contains(&outer, "static FIXED_SIZE = 2;");
    assert_contains(&outer, "static MAX_SIZE = 6;");
    assert_contains(&outer, "return this.inner._tailLen();");
    assert_contains(&outer, "return this.inner._packTail(big);");
    assert_contains(&outer, "this.inner._unpackTail(data, big);");
    // Only the outer type is bound, so only it gets an entry point.
    assert!(!class_body(&module, "Inner").contains("  encode() {"));
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
    assert_contains(&body, "return defgenConcat(bits.toBytes(2, true), this._packTail(true));");
    assert_contains(&body, "DefgenBits.fromBytes(data.subarray(0, Note.FIXED_SIZE), true)");
    assert_contains(&body, "value._unpackTail(data.subarray(Note.FIXED_SIZE), true);");
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
    assert_contains(&module, "export function encodeE(");
    assert_contains(&module, "export function decodeE(");
    assert_contains(&module, "export const E_SIZE = 1;");
    // The enum's own codec is internal, and cannot collide with the entry
    // points named after it.
    assert_contains(&module, "function defgenEncodeE(value) {");
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
        let module = backends::javascript::JavaScriptBackend.generate(&model, &Options::default());
        let contents = &module.single().contents;
        assert!(!contents.is_empty(), "line {skip} produced an empty module");
        assert_documented(contents, &format!("mutation {skip}"));
        generated += 1;
    }
    assert!(generated > 10, "only {generated} mutations checked clean; the test is not exercising much");
}

// ---------------------------------------------------------------------------
// Conformance: the generated module, imported and run
// ---------------------------------------------------------------------------

/// Runs the hand-written conformance fixture against the worked example's
/// generated module. The fixture's byte strings are derived from SPEC.md by
/// hand and shared with the other backends' fixtures, so this is the test that
/// would catch the emitter and the spec — or two backends — disagreeing.
#[test]
fn the_generated_module_round_trips_the_worked_example() {
    let Some(node) = node() else {
        eprintln!("skipping the JavaScript conformance run: no Node found");
        return;
    };
    let dir = scratch("conformance");
    std::fs::write(dir.join("commands.mjs"), example_module()).unwrap();

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples/javascript_conformance.mjs");
    std::fs::copy(&fixture, dir.join("conformance.mjs")).expect("failed to stage the fixture");

    let run = Command::new(&node)
        .current_dir(&dir)
        .arg("conformance.mjs")
        .output()
        .expect("failed to run the conformance fixture");

    assert!(
        run.status.success(),
        "the generated codecs disagree with the spec:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
