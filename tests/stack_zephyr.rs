//! The Zephyr stack (SPEC.md §10): a GATT service table for firmware.
//!
//! A service table is harder to test than a codec. Its ATT callbacks are
//! `static`, its attribute indices are arithmetic the generator did in its
//! head, and none of it means anything without a Bluetooth stack underneath.
//! So there are two layers here: assertions on the generated text for the
//! decisions that are visible in it, and — in `zephyr_conformance` — a build
//! against stub Zephyr headers that runs the generated callbacks for real.

use std::path::{Path, PathBuf};
use std::process::Command;

use defgen::backends::{Backend, Options, c::CBackend};
use defgen::stacks::{self, Stack, zephyr::ZephyrStack};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn model(src: &str) -> defgen::Model {
    let compiled = defgen::compile(src);
    assert!(!compiled.has_errors(), "schema should compile: {:?}", compiled.diagnostics);
    compiled.model.expect("a schema with no errors has a model")
}

fn opts() -> Options {
    Options { stem: "schema".to_string(), source: None }
}

/// The two files the stack generates, as (header, source).
fn generate(src: &str) -> (String, String) {
    let generated = ZephyrStack.generate(&model(src), &opts());
    assert_eq!(generated.files.len(), 2, "the stack emits a header and a translation unit");
    assert_eq!(generated.files[0].name, "schema_gatt.h");
    assert_eq!(generated.files[1].name, "schema_gatt.c");
    (generated.files[0].contents.clone(), generated.files[1].contents.clone())
}

fn example_src() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples/commands.defs");
    std::fs::read_to_string(path).expect("the worked example")
}

fn assert_contains(text: &str, needle: &str) {
    assert!(text.contains(needle), "generated code is missing `{needle}`");
}

const LIGHT: &str = r#"
endian: little;
---
struct State: u16 {
    brightness: u8,
    level: u8,
}
service Light(uuid: "0000ffe0-0000-1000-8000-00805f9b34fb") {
    characteristic StateChar(
        uuid: "ffe1",
        properties: [read, notify],
    ): State;
}
"#;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[test]
fn the_stack_is_registered_under_the_name_the_cli_accepts() {
    assert!(stacks::names().contains(&"zephyr"));
    let found = stacks::find("zephyr").expect("`zephyr` resolves");
    assert_eq!(found.name(), "zephyr");
    assert!(!found.description().is_empty());
}

#[test]
fn the_stack_generates_against_the_c_backend() {
    // The table calls `state_encode` and sizes buffers with `STATE_SIZE`, so
    // the pairing is not incidental — a stack that named a backend the
    // registry does not have would generate a file that cannot compile.
    let name = ZephyrStack.codec_backend();
    assert!(defgen::backends::find(name).is_some(), "`{name}` is a registered backend");
    assert_eq!(name, "c");
}

// ---------------------------------------------------------------------------
// File split
// ---------------------------------------------------------------------------

#[test]
fn the_table_lives_in_a_translation_unit_and_the_hooks_in_a_header() {
    let (header, source) = generate(LIGHT);

    // BT_GATT_SERVICE_DEFINE defines objects, so it cannot be in a header that
    // more than one file includes.
    assert!(!header.contains("BT_GATT_SERVICE_DEFINE"), "the table must not be in the header");
    assert_contains(&source, "BT_GATT_SERVICE_DEFINE(light_svc,");

    // The header is the application's whole view of the server.
    assert_contains(&header, "#ifndef SCHEMA_GATT_H");
    assert_contains(&header, "int light_state_char_read(struct bt_conn *conn, State *out);");
    assert_contains(&source, "#include \"schema_gatt.h\"");
}

#[test]
fn the_header_includes_the_codec_it_exchanges_values_in() {
    let (header, _) = generate(LIGHT);
    assert_contains(&header, "#include \"schema.h\"");
}

// ---------------------------------------------------------------------------
// UUIDs (§10 — all three GATT forms)
// ---------------------------------------------------------------------------

#[test]
fn each_uuid_form_gets_the_zephyr_type_that_holds_it() {
    let (_, source) = generate(LIGHT);
    // A 128-bit service UUID, and a 16-bit characteristic UUID in the same
    // schema: the form is per-UUID, not per-file.
    assert_contains(
        &source,
        "static struct bt_uuid_128 light_uuid = BT_UUID_INIT_128(BT_UUID_128_ENCODE(0x0000ffe0, 0x0000, \
         0x1000, 0x8000, 0x00805f9b34fb));",
    );
    assert_contains(&source, "static struct bt_uuid_16 light_state_char_uuid = BT_UUID_INIT_16(0xffe1);");
}

#[test]
fn a_32_bit_uuid_keeps_its_own_form() {
    let src = LIGHT.replace("uuid: \"ffe1\"", "uuid: \"0000ffe1\"");
    let (_, source) = generate(&src);
    assert_contains(&source, "static struct bt_uuid_32 light_state_char_uuid = BT_UUID_INIT_32(0x0000ffe1);");
}

// ---------------------------------------------------------------------------
// Properties and permissions
// ---------------------------------------------------------------------------

#[test]
fn declared_properties_become_chrc_flags() {
    let src = LIGHT.replace("[read, notify]", "[read, write, write_without_response, notify, indicate]");
    let (_, source) = generate(&src);
    assert_contains(
        &source,
        "BT_GATT_CHRC_READ | BT_GATT_CHRC_WRITE | BT_GATT_CHRC_WRITE_WITHOUT_RESP | BT_GATT_CHRC_NOTIFY \
         | BT_GATT_CHRC_INDICATE,",
    );
}

#[test]
fn permissions_follow_the_direction_a_characteristic_declares() {
    let (_, read_notify) = generate(LIGHT);
    assert_contains(&read_notify, "BT_GATT_PERM_READ,");

    let (_, write_only) = generate(&LIGHT.replace("[read, notify]", "[write]"));
    assert_contains(&write_only, "BT_GATT_PERM_WRITE,");

    // Notify-only: a subscriber goes through the CCC, so the value attribute
    // itself needs no permission at all.
    let (_, notify_only) = generate(&LIGHT.replace("[read, notify]", "[notify]"));
    assert_contains(&notify_only, "BT_GATT_PERM_NONE,");
}

#[test]
fn only_the_declared_directions_get_a_callback() {
    let (_, read_only) = generate(&LIGHT.replace("[read, notify]", "[read]"));
    assert_contains(&read_only, "light_state_char_read_cb, NULL, NULL)");

    let (_, write_only) = generate(&LIGHT.replace("[read, notify]", "[write]"));
    assert_contains(&write_only, "NULL, light_state_char_write_cb, NULL)");
}

#[test]
fn a_notifiable_characteristic_gets_a_ccc_and_a_notify_helper() {
    let (header, source) = generate(LIGHT);
    assert_contains(&source, "BT_GATT_CCC(light_state_char_ccc_changed,");
    assert_contains(&header, "int light_state_char_notify(struct bt_conn *conn, const State *v);");
    assert_contains(&header, "bool light_state_char_is_subscribed(void);");

    // Without notify or indicate there is nothing to subscribe to.
    let (plain_header, plain_source) = generate(&LIGHT.replace("[read, notify]", "[read]"));
    assert!(!plain_source.contains("BT_GATT_CCC"));
    assert!(!plain_header.contains("_notify("));
    assert!(!plain_header.contains("_is_subscribed"));
}

// ---------------------------------------------------------------------------
// Attribute indices
// ---------------------------------------------------------------------------

/// The index arithmetic, on a shape where every wrong answer is a different
/// number: two characteristics, only the first of which has a CCC.
///
/// `zephyr_conformance` checks the same thing against the real macro
/// expansion; this checks the number itself, so a failure says which one.
#[test]
fn a_value_attribute_index_counts_declarations_and_cccs() {
    let src = r#"
endian: little;
---
struct State: u16 { brightness: u8, level: u8, }
service Light(uuid: "ffe0") {
    characteristic StateChar(uuid: "ffe1", properties: [read, notify]): State;
    characteristic OtherChar(uuid: "ffe2", properties: [read]): State;
}
"#;
    let (_, source) = generate(src);
    // Primary service 0, declaration 1, value 2, CCC 3, declaration 4, value 5.
    assert_contains(&source, "return &light_svc.attrs[2];");
    assert_contains(&source, "return &light_svc.attrs[5];");
}

#[test]
fn each_service_indexes_from_its_own_table() {
    let src = r#"
endian: little;
---
struct State: u16 { brightness: u8, level: u8, }
service First(uuid: "ffe0") {
    characteristic AChar(uuid: "ffe1", properties: [read]): State;
}
service Second(uuid: "fff0") {
    characteristic BChar(uuid: "fff1", properties: [read]): State;
}
"#;
    let (_, source) = generate(src);
    assert_contains(&source, "return &first_svc.attrs[2];");
    assert_contains(&source, "return &second_svc.attrs[2];");
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

#[test]
fn a_hook_is_named_for_its_service_and_its_characteristic() {
    // Everything generated for a characteristic — its hooks, its statics, its
    // ATT callbacks — carries both names, so which service a hook belongs to
    // is legible at the call site and two services' tables cannot collide in
    // the one C namespace they share.
    let src = r#"
endian: little;
---
struct State: u16 { brightness: u8, level: u8, }
service First(uuid: "ffe0") {
    characteristic AChar(uuid: "ffe1", properties: [read]): State;
}
service Second(uuid: "fff0") {
    characteristic BChar(uuid: "fff1", properties: [read]): State;
}
"#;
    let (header, source) = generate(src);
    assert_contains(&header, "int first_a_char_read(struct bt_conn *conn, State *out);");
    assert_contains(&header, "int second_b_char_read(struct bt_conn *conn, State *out);");
    assert_contains(&source, "static struct bt_uuid_16 first_a_char_uuid");
    assert_contains(&source, "static struct bt_uuid_16 second_b_char_uuid");
}

#[test]
fn a_variable_length_value_is_sized_by_its_maximum() {
    let src = r#"
endian: little;
---
alias Label = string(max: 24);
service Tag(uuid: "ffe0") {
    characteristic LabelChar(uuid: "ffe1", properties: [read]): Label;
}
"#;
    let (_, source) = generate(src);
    // §6.3: the buffer has to hold the largest legal encoding, while the
    // encoder reports what was actually written.
    assert_contains(&source, "uint8_t encoded[LABEL_MAX_SIZE];");
    assert_contains(&source, "label_encode(&value, encoded, sizeof encoded, &encoded_len);");
}

#[test]
fn doc_comments_reach_the_generated_code() {
    let src = r#"
endian: little;
---
struct State: u16 { brightness: u8, level: u8, }
/// The bulb's own service.
service Light(uuid: "ffe0") {
    /// Pushed on every change.
    characteristic StateChar(uuid: "ffe1", properties: [read, notify]): State;
}
"#;
    let (header, source) = generate(src);
    assert_contains(&header, " * Pushed on every change.");
    assert_contains(&source, " * The bulb's own service.");
}

// ---------------------------------------------------------------------------
// Conformance: build and run the generated server
// ---------------------------------------------------------------------------

fn cc() -> Option<String> {
    let candidates = std::env::var("CC").into_iter().chain(["cc".into(), "gcc".into(), "clang".into()]);
    candidates
        .into_iter()
        .find(|c| Command::new(c).arg("--version").output().is_ok_and(|o| o.status.success()))
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// Generates the server for the worked example, builds it against the stub
/// Zephyr headers, and runs the fixture's assertions.
///
/// `-Wall -Wextra -Werror`, as the C backend's own tests do: firmware projects
/// build with warnings as errors, and generated code that trips them is
/// generated code nobody can use. `-pedantic` is left off — a Zephyr build is
/// a GNU-dialect build, and the stub headers reproduce Zephyr's macros rather
/// than ISO-clean equivalents of them.
#[test]
fn zephyr_conformance() {
    let Some(cc) = cc() else {
        eprintln!("skipping `zephyr_conformance`: no C compiler found");
        return;
    };

    let src = example_src();
    let model = model(&src);
    let opts = Options { stem: "commands".to_string(), source: Some("commands.defs".to_string()) };

    let dir = scratch("zephyr_conformance");
    for file in
        CBackend.generate(&model, &opts).files.iter().chain(ZephyrStack.generate(&model, &opts).files.iter())
    {
        std::fs::write(dir.join(&file.name), &file.contents).expect("write generated file");
    }

    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples");
    let binary = dir.join("conformance");
    let build = Command::new(&cc)
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
        .arg(format!("-I{}", dir.display()))
        .arg(format!("-I{}", examples.join("zephyr_stub").display()))
        .arg(examples.join("zephyr_conformance.c"))
        .arg(dir.join("commands_gatt.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("failed to run the C compiler");
    assert!(
        build.status.success(),
        "the generated Zephyr server does not compile cleanly:\n{}",
        String::from_utf8_lossy(&build.stderr)
    );

    let run = Command::new(&binary).output().expect("failed to run the Zephyr conformance fixture");
    assert!(
        run.status.success(),
        "the Zephyr conformance fixture failed:\n{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}
