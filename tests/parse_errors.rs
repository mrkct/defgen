//! Error-path tests. Each case asserts the *message* a schema author sees, not
//! just that parsing failed — the wording is the feature.

use defgen::parse;

const HEADER: &str = "endian: little;\n---\n";

/// Parses `HEADER + body` and returns every diagnostic message.
fn errors(body: &str) -> Vec<String> {
    let src = format!("{HEADER}{body}");
    let parsed = parse(&src);
    assert!(parsed.schema.is_none(), "expected a parse failure for:\n{body}");
    parsed.diagnostics.iter().map(|d| d.message.clone()).collect()
}

/// Asserts the first diagnostic's message contains `needle`.
#[track_caller]
fn assert_first(body: &str, needle: &str) {
    let messages = errors(body);
    assert!(
        messages.first().is_some_and(|m| m.contains(needle)),
        "expected the first error to mention {needle:?}, got {messages:?}"
    );
}

/// Asserts some diagnostic's message contains `needle`.
#[track_caller]
fn assert_any(body: &str, needle: &str) {
    let messages = errors(body);
    assert!(
        messages.iter().any(|m| m.contains(needle)),
        "expected an error mentioning {needle:?}, got {messages:?}"
    );
}

// ---------------------------------------------------------------------------
// Header (§1.1)
// ---------------------------------------------------------------------------

#[test]
fn missing_separator() {
    let parsed = parse("endian: big;\n");
    assert!(parsed.diagnostics.iter().any(|d| d.message == "missing `---` separator"));
}

#[test]
fn duplicate_pragma_points_at_the_first_one() {
    let parsed = parse("endian: little;\nendian: big;\n---\n");
    let d = &parsed.diagnostics[0];
    assert_eq!(d.message, "the `endian` pragma is declared more than once");
    assert!(d.labels.iter().any(|l| !l.primary && l.message.contains("first declared here")));
}

#[test]
fn pragma_punctuation_mixups_are_called_out() {
    let parsed = parse("endian = big;\n---\n");
    assert!(
        parsed.diagnostics.iter().any(|d| d.message.contains("`endian` pragma is written with `:`, not `=`"))
    );
}

#[test]
fn pragma_below_the_separator_says_where_it_belongs() {
    let parsed = parse("endian: big;\n---\nendian: little;\n");
    let d = &parsed.diagnostics[0];
    assert!(d.message.contains("`endian` pragma must appear in the file header"), "{}", d.message);
    assert!(d.helps.iter().any(|h| h.contains("above the `---`")));
}

#[test]
fn declaration_above_the_separator() {
    let parsed = parse("endian: big;\nalias A = u8;\n---\n");
    assert!(parsed.diagnostics.iter().any(|d| d.message == "declaration appears above the `---` separator"));
}

#[test]
fn unknown_byte_order_suggests_a_real_one() {
    let parsed = parse("endian: litle;\n---\n");
    let d = &parsed.diagnostics[0];
    assert_eq!(d.message, "unknown byte order `litle`");
    assert!(d.helps.iter().any(|h| h.contains("did you mean `little`?")));
}

#[test]
fn version_pragma_no_longer_exists() {
    // `version` is not a header pragma anymore, so a file that only ever wrote
    // one is parsed as a stray declaration, not specially recognized.
    let parsed = parse("version = 1;\nendian: little;\n---\n");
    assert!(parsed.schema.is_none());
    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|d| d.message.contains("expected a declaration, found identifier `version`"))
    );
}

// ---------------------------------------------------------------------------
// Types (§2, §6.1, §6.3)
// ---------------------------------------------------------------------------

#[test]
fn integer_widths_are_range_checked() {
    assert_first("alias A = u0;", "`u0` is not a valid integer type");
    assert_first("alias A = u129;", "`u129` is not a valid integer type");
    assert_first("alias A = i1;", "`i1` is not a valid integer type");
}

#[test]
fn signed_one_bit_explains_the_sign_bit() {
    let src = format!("{HEADER}alias A = i1;");
    let parsed = parse(&src);
    assert!(parsed.diagnostics[0].helps.iter().any(|h| h.contains("sign bit")));
}

#[test]
fn string_needs_a_max_bound() {
    assert_first("alias A = string;", "`string` requires a maximum byte length");
    assert_first("struct S { s: string(24), }", "expected `max` in `string(...)`");
}

#[test]
fn max_must_be_positive() {
    assert_first("alias A = string(max: 0);", "`max` must be a positive integer");
    assert_first("alias A = u8[max: 0];", "`max` must be a positive integer");
}

#[test]
fn array_length_must_be_a_literal() {
    assert_first("struct S: u32 { a: u8[n], }", "expected an array length, found `n`");
}

#[test]
fn string_arrays_are_rejected() {
    assert_first("struct S { a: string(max: 4)[2], }", "an array of `string` is not supported");
}

#[test]
fn container_widths_must_be_unsigned_types() {
    assert_first("struct S: i32 { x: i32, }", "a struct's width must be unsigned, found `i32`");
    assert_first("struct S: 32 { x: u32, }", "a struct's width is written as a type, not a number");
    assert_first(
        "enum E: bool { A, }",
        "an enum's backing type must be an unsigned integer type, found `bool`",
    );
}

// ---------------------------------------------------------------------------
// Fields (§6, §6.2)
// ---------------------------------------------------------------------------

#[test]
fn missing_comma_between_fields_offers_the_fix() {
    let src = format!("{HEADER}struct S: u8 {{\n    a: u4\n    b: u4,\n}}\n");
    let parsed = parse(&src);
    let d = &parsed.diagnostics[0];
    assert!(d.message.starts_with("expected `,` or `}`"), "{}", d.message);
    assert!(d.labels.iter().any(|l| l.message == "add `,` here"));
    // and the error says which struct it happened in
    assert!(d.labels.iter().any(|l| l.message.contains("in struct `S`")));
}

#[test]
fn padding_can_only_be_checked_against_zero() {
    assert_first("struct S: u8 { padding: u8 = 1, }", "padding can only be checked against `0`");
}

#[test]
fn reserved_fields_must_be_named() {
    let src = format!("{HEADER}struct S: u8 {{ reserved: u8, }}");
    let parsed = parse(&src);
    let d = &parsed.diagnostics[0];
    assert_eq!(d.message, "a `reserved` field must be named");
    assert!(d.helps.iter().any(|h| h.contains("padding: u4")), "should mention the padding alternative");
}

#[test]
fn keywords_cannot_be_field_names() {
    assert_first("struct S: u8 { string: u8, }", "expected a field name, found keyword `string`");
}

#[test]
fn struct_needs_a_width_or_a_body() {
    assert_first("struct S u8 { x: u8, }", "expected `:` or `{` after the struct name");
}

#[test]
fn field_attributes_are_rejected() {
    assert_any(
        "struct S: u8 {\n    #[endian(big)]\n    x: u8,\n}",
        "attributes cannot be applied to a field",
    );
}

// ---------------------------------------------------------------------------
// Enums and unions (§5, §7)
// ---------------------------------------------------------------------------

#[test]
fn else_arm_must_be_last() {
    assert_any(
        "enum E: u4 {\n    A = 0,\n    else Unknown,\n    B = 1,\n}",
        "the `else` arm must be the last arm",
    );
}

#[test]
fn only_one_else_arm() {
    assert_any("enum E: u4 { A, else X, else Y, }", "may have only one `else` arm");
}

#[test]
fn else_arm_needs_a_name() {
    assert_first("enum E: u4 { A, else, }", "the `else` arm needs a variant name");
}

#[test]
fn plain_enum_and_union_syntax_are_not_mixed_up() {
    assert_first("enum E: u4 { A(1), }", "a plain enum variant takes its value with `=`");
    assert_first("enum E: u4 { A { x: u4 }, }", "a plain enum variant cannot carry fields");
    assert_first("enum E(id: u8): u16 { A = 1 }", "a tagged-union variant takes its id in parentheses");
}

#[test]
fn union_variant_ids_are_mandatory() {
    let src = format!("{HEADER}enum E(id: u8): u16 {{ A {{ x: u8 }} }}");
    let parsed = parse(&src);
    let d = &parsed.diagnostics[0];
    assert_eq!(d.message, "variant `A` is missing its id");
    assert!(d.notes.iter().any(|n| n.contains("never auto-numbered")));
}

#[test]
fn union_discriminant_needs_a_name() {
    assert_first("enum E(u8): u16 { A(1) }", "the discriminant needs a name and a type");
}

#[test]
fn union_variants_do_not_need_commas() {
    let src =
        format!("{HEADER}enum E(id: u8): u16 {{\n    A(1) {{ x: u8 }}\n    B(2)\n    else Unknown\n}}\n");
    let parsed = parse(&src);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert!(parsed.schema.is_some());
}

// ---------------------------------------------------------------------------
// scaled (§4)
// ---------------------------------------------------------------------------

#[test]
fn scaled_requires_an_integer_raw_type() {
    assert_first("scaled T: bool as f32 (scale: 0.5);", "`scaled` must wrap an integer wire type");
}

#[test]
fn scaled_requires_scale() {
    assert_any("scaled T: i16 as f32 (offset: 1.0);", "missing `scale`");
    assert_first("scaled T: i16 as f32;", "expected `(scale: ...)` after the physical type");
}

#[test]
fn scaled_rejects_zero_scale_and_unknown_args() {
    assert_first("scaled T: i16 as f32 (scale: 0);", "`scale` must not be zero");
    assert_first("scaled T: i16 as f32 (scale: 1.0, unit: 2);", "unknown `scaled` argument `unit`");
}

#[test]
fn scaled_physical_type_must_be_a_float() {
    assert_first("scaled T: i16 as u32 (scale: 1.0);", "`u32` is not a physical type");
    assert_first("scaled T: i16 f32 (scale: 1.0);", "expected `as` after the raw type");
}

#[test]
fn scaled_accepts_negative_offsets() {
    let src = format!("{HEADER}scaled T: u8 as f32 (scale: 0.5, offset: -40.0);");
    let parsed = parse(&src);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let schema = parsed.schema.unwrap();
    let defgen::ast::Decl::Scaled(t) = schema.decl("T").unwrap() else { panic!() };
    assert_eq!(t.offset.unwrap().value, -40.0);
}

// ---------------------------------------------------------------------------
// const (§3.1)
// ---------------------------------------------------------------------------

#[test]
fn const_requires_an_integer_type() {
    assert_first("const T: bool = 1;", "a `const` must be an integer wire type, found `bool`");
    assert_first("const T: Volume = 1;\nalias Volume = u4;", "a `const` must be an integer wire type");
}

#[test]
fn const_value_must_be_a_whole_number() {
    assert_first("const T: u8 = 1.5;", "a constant's value must be a whole number");
}

#[test]
fn const_value_must_be_an_integer_literal() {
    assert_first("const T: u8 = true;", "expected a constant value (an integer literal)");
}

#[test]
fn const_cannot_carry_attributes() {
    assert_first("#[endian(big)]\nconst T: u8 = 1;", "attributes cannot be applied to a `const`");
}

// ---------------------------------------------------------------------------
// Attributes (§1.2)
// ---------------------------------------------------------------------------

#[test]
fn unknown_attribute_is_reported_but_the_declaration_still_parses() {
    let src = format!("{HEADER}#[packed]\nstruct S: u8 {{ x: u8, }}\nstruct Broken: u8 {{ x: y z, }}\n");
    let parsed = parse(&src);
    let messages: Vec<&str> = parsed.diagnostics.iter().map(|d| d.message.as_str()).collect();
    assert!(messages.iter().any(|m| m.contains("unknown attribute `packed`")), "{messages:?}");
    // Recovery: the second, unrelated error is found in the same run.
    assert!(messages.len() >= 2, "{messages:?}");
}

#[test]
fn attributes_are_only_allowed_on_containers() {
    assert_any("#[endian(big)]\nalias A = u8;", "attributes cannot be applied to an `alias`");
    assert_any("#[endian(big)]\nenum E: u4 { A, }", "attributes cannot be applied to a plain `enum`");
    assert_any(
        "#[endian(big)]\nscaled T: u8 as f32 (scale: 1.0);",
        "attributes cannot be applied to a `scaled` declaration",
    );
}

#[test]
fn endian_attribute_needs_a_valid_argument() {
    assert_first("#[endian]\nstruct S: u8 { x: u8, }", "`endian` attribute needs a byte order argument");
    assert_first("#[endian(middle)]\nstruct S: u8 { x: u8, }", "unknown byte order `middle`");
    assert_any("#[endian(big)]\n#[endian(little)]\nstruct S: u8 { x: u8, }", "duplicate `endian` attribute");
}

#[test]
fn attribute_typo_is_suggested() {
    let src = format!("{HEADER}#[endain(big)]\nstruct S: u8 {{ x: u8, }}");
    let parsed = parse(&src);
    assert!(parsed.diagnostics[0].helps.iter().any(|h| h.contains("did you mean `endian`?")));
}

// ---------------------------------------------------------------------------
// Services and characteristics (§10)
// ---------------------------------------------------------------------------

#[test]
fn service_needs_a_uuid() {
    assert_first("service S { }", "a `service` needs a UUID");
    assert_first("service S(id: \"x\") { }", "unknown service argument `id`");
}

#[test]
fn characteristic_arguments_are_checked() {
    assert_any(
        "service S(uuid: \"x\") { characteristic C(properties: [read]): T; }",
        "is missing its `uuid`",
    );
    assert_any("service S(uuid: \"x\") { characteristic C(uuid: \"y\"): T; }", "is missing its `properties`");
    assert_any(
        "service S(uuid: \"x\") { characteristic C(uuid: \"y\", properties: []): T; }",
        "declares no properties",
    );
    assert_any(
        "service S(uuid: \"x\") { characteristic C(uuid: \"y\", props: [read]): T; }",
        "unknown characteristic argument `props`",
    );
}

#[test]
fn unknown_property_is_suggested() {
    let src = format!(
        "{HEADER}service S(uuid: \"x\") {{ characteristic C(uuid: \"y\", properties: [notfy]): T; }}"
    );
    let parsed = parse(&src);
    let d = &parsed.diagnostics[0];
    assert_eq!(d.message, "unknown GATT property `notfy`");
    assert!(d.helps.iter().any(|h| h.contains("did you mean `notify`?")));
}

#[test]
fn characteristics_bind_declared_types_only() {
    assert_any(
        "service S(uuid: \"x\") { characteristic C(uuid: \"y\", properties: [read]): u16; }",
        "cannot bind the primitive type `u16`",
    );
    assert_any(
        "service S(uuid: \"x\") { characteristic C(uuid: \"y\", properties: [read]): string(max: 4); }",
        "cannot bind an inline compound type",
    );
}

#[test]
fn characteristic_outside_a_service() {
    assert_first(
        "characteristic C(uuid: \"y\", properties: [read]): T;",
        "`characteristic` may only appear inside a `service` block",
    );
}

#[test]
fn misspelled_characteristic_keyword_is_suggested() {
    let src =
        format!("{HEADER}service S(uuid: \"x\") {{ charactersitic C(uuid: \"y\", properties: [read]): T; }}");
    let parsed = parse(&src);
    assert!(parsed.diagnostics[0].helps.iter().any(|h| h.contains("did you mean `characteristic`?")));
}

// ---------------------------------------------------------------------------
// Structure and recovery
// ---------------------------------------------------------------------------

#[test]
fn unclosed_brace_labels_the_opening_delimiter() {
    let src = format!("{HEADER}struct S: u8 {{\n    a: u8,\n\nstruct T: u8 {{ b: u8, }}\n");
    let parsed = parse(&src);
    let d = &parsed.diagnostics[0];
    assert!(d.message.starts_with("unclosed `{`"), "{}", d.message);
    assert!(d.labels.iter().any(|l| l.message.contains("unclosed `{`") && !l.primary));
}

#[test]
fn recovery_reports_several_independent_errors_in_one_run() {
    let body = "\
struct A: u8 { x: u0, }
struct B: u8 { padding: u8 = 3, }
enum C: u4 { X(1), }
";
    let messages = errors(body);
    assert_eq!(messages.len(), 3, "one error per broken declaration, got {messages:?}");
}

#[test]
fn misspelled_declaration_keyword_is_suggested() {
    let src = format!("{HEADER}strcut S: u8 {{ x: u8, }}");
    let parsed = parse(&src);
    let d = &parsed.diagnostics[0];
    assert!(d.message.contains("expected a declaration, found identifier `strcut`"), "{}", d.message);
    assert!(d.helps.iter().any(|h| h.contains("did you mean `struct`?")));
}

#[test]
fn second_separator_is_rejected() {
    let parsed = parse("version = 1;\nendian: big;\n---\nalias A = u8;\n---\n");
    assert!(parsed.diagnostics.iter().any(|d| d.message == "unexpected `---`"));
}

#[test]
fn trailing_doc_comment_documents_nothing() {
    assert_any("alias A = u8;\n/// dangling\n", "doc comment documents nothing");
}

#[test]
fn doc_comment_must_precede_the_attribute() {
    assert_any(
        "#[endian(big)]\n/// docs\nstruct S: u8 { x: u8, }",
        "doc comment must come before the attribute",
    );
}

// ---------------------------------------------------------------------------
// Lexical errors
// ---------------------------------------------------------------------------

#[test]
fn lexical_errors_are_reported_before_parsing() {
    assert_first("alias A = u8@;", "unexpected character `@`");
    assert_first("service S(uuid: \"oops) { }", "unterminated string literal");
    assert_first("alias A = 4u8;", "invalid suffix `u8` on a number literal");
    assert_first("/* nope */\nalias A = u8;", "block comments are not part of the defgen language");
    assert_first("alias A = u8; ----", "expected `---`, found a run of 4 dashes");
}

#[test]
fn oversized_integer_literal() {
    assert_first(
        "enum E(id: u8): u16 { A(999999999999999999999999999999999999999999) }",
        "integer literal is too large",
    );
}

#[test]
fn unknown_escape_in_a_uuid() {
    assert_first("service S(uuid: \"a\\qb\") { }", "unknown escape sequence");
}

// ---------------------------------------------------------------------------
// Diagnostic rendering
// ---------------------------------------------------------------------------

#[test]
fn rendering_reports_the_right_line_and_column() {
    let src = format!("{HEADER}struct S: u8 {{\n    x: u0,\n}}\n");
    let parsed = parse(&src);
    let plain = parsed.diagnostics[0].render_plain("s.defs", &src);
    assert!(plain.starts_with("s.defs:4:8: error: `u0` is not a valid integer type"), "{plain}");

    // The fancy renderer produces a source snippet with a caret.
    let fancy = parsed.diagnostics[0].render("s.defs", &src, false);
    assert!(fancy.contains("s.defs:4:8"), "{fancy}");
    assert!(fancy.contains("x: u0"), "{fancy}");
    assert!(fancy.contains("width must be between 1 and 128"), "{fancy}");
}
