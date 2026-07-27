//! Valid-but-unusual syntax the parser must accept without complaint. Anything
//! rejected here would be a false positive in front of a legal schema.

use defgen::ast::*;
use defgen::parse;

#[track_caller]
fn schema(src: &str) -> Schema {
    let parsed = parse(src);
    let messages: Vec<&str> = parsed.diagnostics.iter().map(|d| d.message.as_str()).collect();
    assert!(parsed.diagnostics.is_empty(), "unexpected diagnostics: {messages:?}");
    parsed.schema.expect("schema")
}

#[track_caller]
fn body(decls: &str) -> Schema {
    schema(&format!("endian: big;\n\n---\n\n{decls}"))
}

#[test]
fn trailing_commas_are_allowed_everywhere() {
    let s = body(
        "\
enum E: u4 { A, B, }
struct S: u16 { a: u8, b: u8, }
enum U(id: u8): u16 { A(1) { x: u8, }, B(2), }
service Svc(uuid: \"x\",) {
    characteristic C(uuid: \"y\", properties: [read, notify,],): S;
}
",
    );
    assert_eq!(s.decls.len(), 4);
}

#[test]
fn header_order_and_blank_lines_are_flexible() {
    let s = schema("\n\n// a comment\nendian: big;\n\n---\n\nalias A = u8;\n");
    assert_eq!(s.endian.map(|e| e.value), Some(Endianness::Big));
}

#[test]
fn header_is_entirely_optional() {
    let s = schema("alias A = u8;\n");
    assert!(s.endian.is_none());
    assert!(s.separator.is_none());
    assert_eq!(s.decls.len(), 1);
}

#[test]
fn bare_separator_with_no_endian_pragma_is_allowed() {
    let s = schema("---\n\nalias A = u8;\n");
    assert!(s.endian.is_none());
    assert!(s.separator.is_some());
}

#[test]
fn leading_doc_comment_with_no_header_attaches_to_first_declaration() {
    let s = schema("/// first\nalias A = u8;\n");
    assert!(s.endian.is_none());
    assert_eq!(s.decl("A").unwrap().docs()[0].text, "first");
}

#[test]
fn contextual_keywords_are_usable_as_names() {
    let s = body(
        "\
struct S: u32 {
    version: u8,
    endian: u8,
    max: u8,
    read: u8,
}
alias uuid = u8;
",
    );
    let Some(Decl::Struct(st)) = s.decl("S") else { panic!() };
    let names: Vec<&str> = st.fields.iter().filter_map(|f| f.name()).map(|i| i.name.as_str()).collect();
    assert_eq!(names, vec!["version", "endian", "max", "read"]);
    assert!(s.decl("uuid").is_some());
}

#[test]
fn enum_numbering_may_mix_implicit_and_explicit_values() {
    let s = body("enum E: u8 { A, B = 10, C, D = 0x20, }");
    let Some(Decl::Enum(e)) = s.decl("E") else { panic!() };
    let values: Vec<Option<u128>> = e.variants.iter().map(|v| v.value.map(|v| v.value)).collect();
    assert_eq!(values, vec![None, Some(10), None, Some(0x20)]);
}

#[test]
fn repeated_padding_fields_are_fine() {
    let s = body("struct S: u16 { padding: u4, a: u4, padding: u4 = 0, b: u4, }");
    let Some(Decl::Struct(st)) = s.decl("S") else { panic!() };
    let padding: Vec<bool> = st
        .fields
        .iter()
        .filter_map(|f| match f.kind {
            FieldKind::Padding { check_zero, .. } => Some(check_zero),
            _ => None,
        })
        .collect();
    assert_eq!(padding, vec![false, true]);
}

#[test]
fn variable_length_arrays_and_nested_variable_structs() {
    let s = body(
        "\
struct Samples {
    kind: u8,
    values: u16[max: 8],
}
struct Wrapper {
    header: u16,
    body: Samples,
}
",
    );
    let Some(Decl::Struct(samples)) = s.decl("Samples") else { panic!() };
    match &samples.fields[1].kind {
        FieldKind::Value { ty, .. } => match &ty.kind {
            FieldTypeKind::VarArray { elem, max } => {
                assert_eq!(elem.intrinsic_bits(), Some(16));
                assert_eq!(max.value, 8);
            }
            other => panic!("expected a variable-length array, got {other:?}"),
        },
        other => panic!("expected a value field, got {other:?}"),
    }
    // A nested variable-length struct is a plain named field here; whether the
    // nesting is legal is a semantic question, not a syntactic one.
    let Some(Decl::Struct(wrapper)) = s.decl("Wrapper") else { panic!() };
    assert_eq!(wrapper.fields[1].intrinsic_bits(), None);
}

#[test]
fn maximum_and_minimum_integer_widths() {
    let s = body("struct S: u128 { a: u1, b: i2, c: u128, d: bool, }");
    let Some(Decl::Struct(st)) = s.decl("S") else { panic!() };
    let widths: Vec<Option<u32>> = st.fields.iter().map(|f| f.intrinsic_bits()).collect();
    assert_eq!(widths, vec![Some(1), Some(2), Some(128), Some(1)]);
}

#[test]
fn docs_attach_to_fields_variants_and_characteristics() {
    let s = body(
        "\
struct S: u8 {
    /// how loud
    /// (0-15)
    volume: u4,
    padding: u4,
}
enum E: u4 {
    /// the default
    A = 0,
    /// anything new
    else Unknown,
}
service Svc(uuid: \"x\") {
    /// pushed on change
    characteristic C(uuid: \"y\", properties: [notify]): S;
}
",
    );
    let Some(Decl::Struct(st)) = s.decl("S") else { panic!() };
    assert_eq!(
        st.fields[0].docs.iter().map(|d| d.text.as_str()).collect::<Vec<_>>(),
        vec!["how loud", "(0-15)"]
    );
    let Some(Decl::Enum(e)) = s.decl("E") else { panic!() };
    assert_eq!(e.variants[0].docs[0].text, "the default");
    assert_eq!(e.else_arm.as_ref().unwrap().docs[0].text, "anything new");
    let c = &s.services().next().unwrap().characteristics[0];
    assert_eq!(c.docs[0].text, "pushed on change");
}

#[test]
fn payload_less_union_variants_and_hex_ids() {
    let s = body("enum U(kind: u16): u64 {\n    Reset(0xFFFF)\n    Ping(0x0001) { seq: u8 }\n}");
    let Some(Decl::Union(u)) = s.decl("U") else { panic!() };
    assert_eq!(u.tag_name.name, "kind");
    assert_eq!(u.variants[0].id.value, 0xffff);
    assert!(!u.variants[0].has_payload_block);
    assert!(u.variants[1].has_payload_block);
    assert!(!u.is_open());
}

#[test]
fn multiple_services_and_shared_types() {
    let s = body(
        "\
struct S: u8 { x: u8, }
service A(uuid: \"a\") { characteristic One(uuid: \"1\", properties: [read]): S; }
service B(uuid: \"b\") { characteristic Two(uuid: \"2\", properties: [indicate, write_without_response]): S; }
",
    );
    assert_eq!(s.services().count(), 2);
    assert_eq!(s.services().flat_map(|svc| svc.characteristics.iter()).count(), 2);
}

#[test]
fn empty_service_body_parses() {
    let s = body("service Empty(uuid: \"x\") { }");
    assert_eq!(s.services().next().unwrap().characteristics.len(), 0);
}

#[test]
fn constants_parse_signed_unsigned_and_hex() {
    let s = body(
        "\
const MaxRetries: u8 = 5;
const MinTemperature: i16 = -40;
const Big: u32 = 0xffff;
",
    );
    let Some(Decl::Const(max_retries)) = s.decl("MaxRetries") else { panic!("MaxRetries") };
    assert_eq!(max_retries.ty.kind, ScalarKind::UInt(8));
    assert_eq!(max_retries.value.value, ConstLit { magnitude: 5, negative: false });

    let Some(Decl::Const(min_temp)) = s.decl("MinTemperature") else { panic!("MinTemperature") };
    assert_eq!(min_temp.ty.kind, ScalarKind::Int(16));
    assert_eq!(min_temp.value.value, ConstLit { magnitude: 40, negative: true });

    let Some(Decl::Const(big)) = s.decl("Big") else { panic!("Big") };
    assert_eq!(big.value.value, ConstLit { magnitude: 0xffff, negative: false });
}
