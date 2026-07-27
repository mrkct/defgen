//! Semantic-error tests (SPEC.md §11). As with the parser's error tests, each
//! case asserts the *message* a schema author sees — the wording is the
//! feature. Only the checker's rules live here; syntax errors are in
//! `parse_errors.rs`.

use defgen::compile;
use defgen::diag::Severity;

const HEADER: &str = "endian: little;\n---\n";

/// Checks `HEADER + body` and returns every error message. Warnings are
/// excluded: they do not stop compilation.
#[track_caller]
fn errors(body: &str) -> Vec<String> {
    let src = format!("{HEADER}{body}");
    let compiled = compile(&src);
    assert!(compiled.model.is_none(), "expected this to be rejected:\n{body}");
    compiled.diagnostics.iter().filter(|d| d.severity == Severity::Error).map(|d| d.message.clone()).collect()
}

/// Asserts some error message contains `needle`.
#[track_caller]
fn rejects(body: &str, needle: &str) {
    let messages = errors(body);
    assert!(
        messages.iter().any(|m| m.contains(needle)),
        "expected an error mentioning {needle:?}, got {messages:?}"
    );
}

/// Asserts the schema checks clean (warnings allowed).
#[track_caller]
fn accepts(body: &str) {
    let src = format!("{HEADER}{body}");
    let compiled = compile(&src);
    let rendered: Vec<String> =
        compiled.diagnostics.iter().map(|d| d.render_plain("test.defs", &src)).collect();
    assert!(compiled.model.is_some(), "expected this to be accepted:\n{body}\n{}", rendered.join("\n"));
}

/// Every `help:` line of the first error, for asserting on suggestions.
#[track_caller]
fn first_helps(body: &str) -> Vec<String> {
    let src = format!("{HEADER}{body}");
    let compiled = compile(&src);
    compiled
        .diagnostics
        .iter()
        .find(|d| d.severity == Severity::Error)
        .map(|d| d.helps.clone())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Name resolution (§9, §11)
// ---------------------------------------------------------------------------

#[test]
fn unknown_type_is_reported_with_a_suggestion() {
    rejects("struct S: u8 { x: Volme, }", "unknown type `Volme`");
    let helps = first_helps("alias Volume = u4;\nstruct S: u8 { x: Volme, y: u4, }");
    assert!(helps.iter().any(|h| h.contains("did you mean `Volume`?")), "{helps:?}");
}

#[test]
fn forward_references_are_rejected() {
    rejects("struct Outer: u8 { inner: Inner, }\nstruct Inner: u8 { x: u8, }", "used before it is declared");
    // The same thing, declared the other way round, is fine.
    accepts("struct Inner: u8 { x: u8, }\nstruct Outer: u8 { inner: Inner, }");
}

#[test]
fn a_struct_cannot_contain_itself() {
    rejects("struct S: u8 { s: S, }", "`S` cannot contain itself");
    // Mutual recursion can only be written as a forward reference.
    rejects("struct A: u8 { b: B, }\nstruct B: u8 { a: A, }", "used before it is declared");
}

#[test]
fn duplicate_declaration_names() {
    rejects("alias A = u8;\nstruct A: u8 { x: u8, }", "`A` is declared more than once");
}

#[test]
fn a_declaration_may_not_be_named_after_a_primitive() {
    rejects("alias u8 = u16;", "`u8` is a built-in type name");
    rejects("struct bool: u8 { x: u8, }", "`bool` is a built-in type name");
}

#[test]
fn a_service_is_not_a_type() {
    rejects("service Svc(uuid: \"180a\") { }\nstruct S: u8 { x: Svc, }", "`Svc` is a service, not a type");
}

// ---------------------------------------------------------------------------
// Exact-fit widths (§6)
// ---------------------------------------------------------------------------

#[test]
fn struct_fields_must_fill_the_container_exactly() {
    rejects("struct S: u16 { a: u8, }", "does not fill its `u16` container");
    rejects("struct S: u8 { a: u8, b: u8, }", "overflows its `u8` container");
}

#[test]
fn an_underfilled_struct_is_told_how_much_padding_it_needs() {
    let helps = first_helps("struct S: u16 { a: u12, }");
    assert!(helps.iter().any(|h| h.contains("padding: u4")), "{helps:?}");
}

#[test]
fn widths_come_from_aliases_enums_scaled_types_structs_and_arrays() {
    accepts(concat!(
        "alias Volume = u4;\n",
        "scaled Temp: i16 as f32 (scale: 0.01);\n",
        "enum Mode: u4 { A, B, }\n",
        "struct Inner: u8 { x: u8, }\n",
        "struct S: u64 { v: Volume, m: Mode, t: Temp, i: Inner, arr: u8[3], b: bool, padding: u7, }\n",
    ));
}

#[test]
fn a_zero_length_array_is_rejected() {
    rejects("struct S: u8 { a: u8[0], }", "array length must be positive");
}

#[test]
fn a_container_may_be_wider_than_a_primitive() {
    // A container's width is a bit count, not a value, so it is not held to
    // the 128-bit primitive limit (§2, §6).
    accepts("struct Blob: u2048 { data: u8[255], trailer: u8, }");
    accepts("struct Gap: u1024 { header: u8, padding: u1016, }");
    accepts("enum C(id: u8): u256 { A(0x01) { payload: u8[8] } }");

    // ...but only up to 4096 bits, and only where the bits are not a value.
    rejects("struct S: u5000 { a: u8, }", "a struct's width cannot be `u5000`");
    rejects("struct S: u16 { reserved r: u200, }", "`u200` is not a valid integer type");
    rejects("struct S: u16 { a: u200, }", "`u200` is not a valid integer type");
    rejects("enum E: u200 { A, }", "`u200` is not a valid integer type");
}

#[test]
fn an_open_unions_fallback_must_fit_a_native_integer() {
    // `raw: u(N - T)` is an ordinary value, unlike the container it sits in.
    rejects("enum C(id: u8): u512 { A(0x01) else Unknown }", "capture 504 bits in one value");
    accepts("enum C(id: u8): u512 { A(0x01) }");
}

// ---------------------------------------------------------------------------
// Enums (§5)
// ---------------------------------------------------------------------------

#[test]
fn enum_values_must_fit_the_backing_type() {
    rejects("enum E: u2 { A = 0, B = 9, }", "does not fit in the enum's `u2` backing type");
}

#[test]
fn implicit_numbering_continues_from_the_last_explicit_value() {
    accepts("enum E: u4 { A = 0, B = 7, C, }\nstruct S: u8 { e: E, padding: u4, }");
    // ...and can run off the end of the backing type.
    rejects("enum E: u2 { A = 3, B, }", "does not fit in the enum's `u2` backing type");
}

#[test]
fn duplicate_enum_values_are_rejected() {
    rejects("enum E: u4 { A = 1, B = 1, }", "enum value 1 is used twice");
    // An implicitly numbered variant can collide with an explicit one too.
    rejects("enum E: u4 { A, B, C = 0, }", "enum value 0 is used twice");
}

#[test]
fn duplicate_variant_names_are_rejected() {
    rejects("enum E: u4 { A = 0, A = 1, }", "duplicate variant `A` in enum `E`");
    rejects("enum E: u4 { A = 0, else A, }", "duplicate variant `A` in enum `E`");
}

#[test]
fn an_enum_needs_at_least_one_variant() {
    rejects("enum E: u4 { }", "declares no variants");
}

// ---------------------------------------------------------------------------
// Tagged unions (§7)
// ---------------------------------------------------------------------------

#[test]
fn a_variant_payload_cannot_exceed_the_payload_region() {
    rejects("enum C(id: u8): u16 { Wide(0x01) { a: u16 } }", "needs 16 bits, more than the 8-bit payload");
    accepts("enum C(id: u8): u16 { Narrow(0x01) { a: u8 } Empty(0x02) }");
}

#[test]
fn variant_ids_are_unique_and_fit_the_discriminant() {
    rejects("enum C(id: u8): u16 { A(0x01) B(0x01) }", "variant id 0x1 is used twice");
    rejects("enum C(id: u4): u16 { A(0xff) }", "does not fit in the `u4` discriminant");
}

#[test]
fn a_discriminant_cannot_be_wider_than_its_container() {
    rejects("enum C(id: u32): u16 { A(0x01) }", "discriminant wider than its container");
}

#[test]
fn an_open_union_needs_room_for_its_raw_field() {
    rejects("enum C(id: u16): u16 { A(0x01) else Unknown }", "no payload bits to capture");
    accepts("enum C(id: u16): u16 { A(0x01) }");
}

#[test]
fn duplicate_field_names_within_a_variant() {
    rejects("enum C(id: u8): u32 { A(0x01) { x: u8, x: u8 } }", "duplicate field `x`");
}

// ---------------------------------------------------------------------------
// Variable-length fields (§6.3)
// ---------------------------------------------------------------------------

#[test]
fn a_variable_length_field_must_come_last() {
    rejects("struct S { name: string(max: 8), tail: u8, }", "is not the last field");
}

#[test]
fn a_struct_may_have_only_one_variable_length_field() {
    rejects("struct S { a: string(max: 8), b: string(max: 8), }", "more than one variable-length field");
}

#[test]
fn the_fixed_prefix_must_be_byte_aligned() {
    rejects("struct S { flags: u4, name: string(max: 8), }", "must fill whole bytes");
    accepts("struct S { a: u4, b: u4, name: string(max: 8), }");
}

#[test]
fn a_declared_width_and_a_variable_length_field_are_mutually_exclusive() {
    rejects(
        "struct S: u32 { name: string(max: 8), }",
        "declares a width but ends in a variable-length field",
    );
    rejects("struct S { a: u8, }", "does not end in a variable-length field");
}

#[test]
fn a_missing_width_is_told_what_it_should_have_been() {
    let helps = first_helps("struct S { a: u8, b: u16, }");
    assert!(helps.iter().any(|h| h.contains("struct S: u24")), "{helps:?}");
}

#[test]
fn variable_length_nesting_propagates() {
    // A variable-length struct is itself variable-length wherever it appears.
    accepts("struct Tail { a: u8, name: string(max: 8), }\nstruct Outer { b: u8, t: Tail, }");
    rejects(
        "struct Tail { a: u8, name: string(max: 8), }\nstruct Outer { t: Tail, b: u8, }",
        "is not the last field",
    );
    rejects(
        "struct Tail { a: u8, name: string(max: 8), }\nstruct Outer: u32 { t: Tail, }",
        "declares a width but ends in a variable-length field",
    );
}

#[test]
fn a_variable_length_type_cannot_be_an_array_element() {
    rejects("alias Name = string(max: 8);\nstruct S { a: u8, n: Name[2], }", "cannot be an array element");
    rejects(
        "alias Name = string(max: 8);\nstruct S { a: u8, n: Name[max: 2], }",
        "cannot be the element type",
    );
}

#[test]
fn a_var_array_element_must_be_a_whole_number_of_bytes() {
    rejects("struct S { a: u8, n: u4[max: 4], }", "must be a whole number of bytes");
    accepts("struct S { a: u8, n: u16[max: 4], }");
    accepts("struct Point: u16 { x: u8, y: u8, }\nstruct S { a: u8, p: Point[max: 4], }");
}

#[test]
fn a_union_variant_cannot_hold_a_variable_length_field() {
    rejects(
        "enum C(id: u8): u64 { WithName(0x01) { name: string(max: 4) } }",
        "cannot contain a variable-length field",
    );
}

// ---------------------------------------------------------------------------
// Constants (§3.1)
// ---------------------------------------------------------------------------

#[test]
fn a_constant_that_overflows_its_type_is_rejected() {
    rejects("const T: u8 = 256;", "does not fit in its declared type `u8`");
    rejects("const T: i8 = 128;", "does not fit in its declared type `i8`");
    rejects("const T: i8 = -129;", "does not fit in its declared type `i8`");
}

#[test]
fn a_negative_unsigned_constant_is_rejected_with_a_signedness_hint() {
    let src = format!("{HEADER}const T: u8 = -1;");
    let compiled = compile(&src);
    assert!(compiled.model.is_none());
    let rendered: Vec<String> =
        compiled.diagnostics.iter().map(|d| d.render_plain("test.defs", &src)).collect();
    assert!(
        rendered.iter().any(|m| m.contains("`uN` has no sign; use an `iN` type for a negative constant")),
        "{rendered:?}"
    );
}

#[test]
fn a_constant_at_its_types_exact_bounds_is_accepted() {
    accepts("const Max: u8 = 255;\nconst Min: i8 = -128;\nconst MaxSigned: i8 = 127;");
}

#[test]
fn duplicate_constant_names_are_rejected() {
    rejects("const T: u8 = 1;\nconst T: u8 = 2;", "declared more than once");
}

#[test]
fn a_constant_cannot_be_used_as_a_field_type() {
    rejects("const T: u8 = 1;\nstruct S: u8 { x: T }", "`T` is a constant, not a type");
}

#[test]
fn a_constant_cannot_be_bound_to_a_characteristic() {
    rejects(
        concat!(
            "const T: u8 = 1;\n",
            "service Svc(uuid: \"180a\") {\n",
            "    characteristic C(uuid: \"2a19\", properties: [read]): T;\n",
            "}\n",
        ),
        "`T` is a constant, not a type",
    );
}

// ---------------------------------------------------------------------------
// Endianness (§8)
// ---------------------------------------------------------------------------

#[test]
fn endian_on_a_nested_only_type_is_rejected() {
    rejects(
        concat!(
            "#[endian(big)]\nstruct Inner: u8 { x: u8, }\n",
            "struct Outer: u8 { i: Inner, }\n",
            "service Svc(uuid: \"180a\") {\n",
            "    characteristic C(uuid: \"2a19\", properties: [read]): Outer;\n",
            "}\n",
        ),
        "only ever used as a nested field",
    );
}

#[test]
fn endian_on_a_type_that_is_both_nested_and_bound_is_fine() {
    accepts(concat!(
        "#[endian(big)]\nstruct Inner: u8 { x: u8, }\n",
        "struct Outer: u16 { i: Inner, y: u8, }\n",
        "service Svc(uuid: \"180a\") {\n",
        "    characteristic InnerChar(uuid: \"2a19\", properties: [read]): Inner;\n",
        "    characteristic OuterChar(uuid: \"2a1a\", properties: [read]): Outer;\n",
        "}\n",
    ));
}

#[test]
fn endian_on_an_unbound_unnested_type_is_fine() {
    // Nothing says it is nested, so it may still be encoded on its own (§8).
    accepts("#[endian(big)]\nstruct Standalone: u8 { x: u8, }");
}

// ---------------------------------------------------------------------------
// GATT metadata (§10, §11)
// ---------------------------------------------------------------------------

fn with_service(chars: &str) -> String {
    format!("struct S: u8 {{ x: u8, }}\nservice Svc(uuid: \"180a\") {{\n{chars}\n}}\n")
}

#[test]
fn duplicate_characteristic_names_are_rejected() {
    rejects(
        &with_service(concat!(
            "    characteristic C(uuid: \"2a19\", properties: [read]): S;\n",
            "    characteristic C(uuid: \"2a1a\", properties: [read]): S;",
        )),
        "characteristic `C` is declared more than once",
    );
}

#[test]
fn duplicate_uuids_within_one_service_are_rejected() {
    rejects(
        &with_service(concat!(
            "    characteristic A(uuid: \"2a19\", properties: [read]): S;\n",
            "    characteristic B(uuid: \"2A19\", properties: [read]): S;",
        )),
        "share one UUID",
    );
}

#[test]
fn malformed_uuids_are_rejected() {
    rejects(&with_service("    characteristic C(uuid: \"nope\", properties: [read]): S;"), "malformed UUID");
    rejects("struct S: u8 { x: u8, }\nservice Svc(uuid: \"180\") { }", "service `Svc` has a malformed UUID");
    accepts(&with_service(
        "    characteristic C(uuid: \"0000180a-0000-1000-8000-00805f9b34fb\", properties: [read]): S;",
    ));
}

#[test]
fn duplicate_properties_are_rejected() {
    rejects(
        &with_service("    characteristic C(uuid: \"2a19\", properties: [read, read]): S;"),
        "duplicate property `read`",
    );
}

#[test]
fn only_structs_unions_and_aliases_are_bindable() {
    rejects(
        concat!(
            "enum Mode: u8 { A, B, }\n",
            "service Svc(uuid: \"180a\") {\n",
            "    characteristic C(uuid: \"2a19\", properties: [read]): Mode;\n",
            "}\n",
        ),
        "cannot bind the enum `Mode`",
    );
    // An alias of the same enum is bindable (§10).
    accepts(concat!(
        "enum Mode: u8 { A, B, }\n",
        "alias ModeValue = Mode;\n",
        "service Svc(uuid: \"180a\") {\n",
        "    characteristic C(uuid: \"2a19\", properties: [read]): ModeValue;\n",
        "}\n",
    ));
}

#[test]
fn a_bound_type_must_be_a_whole_number_of_bytes() {
    rejects(
        concat!(
            "alias Volume = u4;\n",
            "service Svc(uuid: \"180a\") {\n",
            "    characteristic C(uuid: \"2a19\", properties: [read]): Volume;\n",
            "}\n",
        ),
        "not a whole number of bytes",
    );
}

#[test]
fn an_oversized_characteristic_is_a_warning_not_an_error() {
    // A fixed struct cannot exceed 16 bytes (its width is a `uN`, §2), so the
    // MTU diagnostic is about variable-length values and their prefixes.
    let src = format!(
        "{HEADER}{}",
        concat!(
            "alias Name = string(max: 64);\n",
            "struct Log { samples: u16[16], note: string(max: 2), }\n",
            "service Svc(uuid: \"180a\") {\n",
            "    characteristic NameChar(uuid: \"2a19\", properties: [read]): Name;\n",
            "    characteristic LogChar(uuid: \"2a1a\", properties: [read]): Log;\n",
            "}\n",
        )
    );
    let compiled = compile(&src);
    assert!(compiled.model.is_some(), "the MTU diagnostic must not block compilation");
    let warnings: Vec<&String> =
        compiled.diagnostics.iter().filter(|d| d.severity == Severity::Warning).map(|d| &d.message).collect();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("up to 64 bytes"), "{warnings:?}");
    assert!(warnings[1].contains("up to 34 bytes"), "{warnings:?}");
}

#[test]
fn a_field_crossing_a_byte_boundary_is_a_warning_not_an_error() {
    // `b` starts at bit 5 and is 4 bits wide (bits 5-8), so it straddles
    // byte 0 and byte 1 — legal (§6), but flagged.
    let src = format!("{HEADER}struct S: u16 {{ a: u5, b: u4, c: u7, }}\n");
    let compiled = compile(&src);
    assert!(compiled.model.is_some(), "crossing a byte boundary must not block compilation");
    let warnings: Vec<&String> =
        compiled.diagnostics.iter().filter(|d| d.severity == Severity::Warning).map(|d| &d.message).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("field `b` crosses a byte boundary"), "{warnings:?}");
}

#[test]
fn a_byte_aligned_field_never_crosses_even_if_wide() {
    // `b` starts at bit 8 (byte-aligned) and spans two more bytes — starting
    // clean is enough, however wide the field is (§6).
    accepts("struct S: u32 { a: u8, b: u24, }");
}

#[test]
fn a_field_confined_to_one_byte_never_crosses_even_if_unaligned() {
    // `b` starts at bit 3 but ends at bit 6 — never leaves byte 0 (§6).
    accepts("struct S: u8 { a: u3, b: u4, c: u1, }");
}

#[test]
fn padding_crossing_a_byte_boundary_is_not_flagged() {
    // Padding carries no value, so an unaligned crossing run has no bug
    // signal — only named/reserved fields are flagged (§6).
    let src = format!("{HEADER}struct S: u16 {{ a: u5, padding: u4, c: u7, }}\n");
    let compiled = compile(&src);
    assert!(compiled.model.is_some());
    let warnings: Vec<&String> =
        compiled.diagnostics.iter().filter(|d| d.severity == Severity::Warning).map(|d| &d.message).collect();
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn a_tagged_union_variant_field_crossing_a_byte_boundary_is_flagged_relative_to_the_tag() {
    // The payload starts at bit 8 (the tag width); `flag` at payload bit 1
    // is absolute bit 9, so `wide` at absolute bit 10 spans bits 10-17,
    // crossing into byte 2.
    let src = format!(
        "{HEADER}{}",
        concat!("enum U(id: u8): u24 {\n", "    V(0x01) { flag: u1, wide: u8 }\n", "}\n",)
    );
    let compiled = compile(&src);
    assert!(compiled.model.is_some());
    let warnings: Vec<&String> =
        compiled.diagnostics.iter().filter(|d| d.severity == Severity::Warning).map(|d| &d.message).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("field `wide` crosses a byte boundary"), "{warnings:?}");
}

// ---------------------------------------------------------------------------
// Reporting behaviour
// ---------------------------------------------------------------------------

#[test]
fn independent_errors_are_all_reported_in_one_run() {
    let messages = errors(concat!(
        "struct A: u16 { x: u8, }\n",
        "struct B: u16 { y: u8, }\n",
        "enum E: u2 { P = 0, Q = 0, }\n",
    ));
    assert_eq!(messages.len(), 3, "{messages:?}");
}

#[test]
fn a_broken_type_does_not_cascade_into_its_users() {
    // `Inner` is unresolvable; `Outer`'s width is not second-guessed on top.
    let messages = errors("struct Outer: u16 { i: Nope, }");
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains("unknown type `Nope`"), "{messages:?}");
}
