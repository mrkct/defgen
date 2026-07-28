//! The worked example from SPEC.md must parse, and parse into exactly the tree
//! the spec describes.

use defgen::ast::*;
use defgen::parse;

const EXAMPLE: &str = include_str!("examples/commands.defs");

fn example() -> Schema {
    let parsed = parse(EXAMPLE);
    let rendered: Vec<String> =
        parsed.diagnostics.iter().map(|d| d.render_plain("commands.defs", EXAMPLE)).collect();
    assert!(parsed.diagnostics.is_empty(), "unexpected diagnostics:\n{}", rendered.join("\n"));
    parsed.schema.expect("schema")
}

#[test]
fn header_pragmas() {
    let schema = example();
    assert_eq!(schema.endian.map(|e| e.value), Some(Endianness::Little));
    assert!(schema.separator.is_some());
}

#[test]
fn declaration_order_and_kinds() {
    let schema = example();
    let names: Vec<(&str, &str)> =
        schema.decls.iter().map(|d| (d.kind_str(), d.name().name.as_str())).collect();
    assert_eq!(
        names,
        vec![
            ("constant", "MaxWriteLength"),
            ("constant", "MinRatedTemperature"),
            ("alias", "Volume"),
            ("alias", "OwnerName"),
            ("scaled type", "Temperature"),
            ("scaled type", "BatteryVoltage"),
            ("enum", "HearingMode"),
            ("struct", "Orientation"),
            ("struct", "Status"),
            ("struct", "TemperatureLog"),
            ("struct", "MotionPath"),
            ("struct", "LegacySerial"),
            ("struct", "LegacyReading"),
            ("struct", "LegacyLog"),
            ("struct", "DiagnosticLabel"),
            ("tagged union", "Command"),
            ("service", "HearingAidControl"),
        ]
    );
}

#[test]
fn aliases_carry_primitive_and_variable_length_targets() {
    let schema = example();
    let Some(Decl::Alias(volume)) = schema.decl("Volume") else { panic!("Volume") };
    assert_eq!(
        volume.target.kind,
        FieldTypeKind::Scalar(ScalarType { kind: ScalarKind::UInt(4), span: volume.target.span })
    );

    let Some(Decl::Alias(owner)) = schema.decl("OwnerName") else { panic!("OwnerName") };
    match owner.target.kind {
        FieldTypeKind::Str { max } => assert_eq!(max.value, 32),
        ref other => panic!("expected string(max: 32), got {other:?}"),
    }
    assert!(owner.target.is_intrinsically_variable());
}

#[test]
fn scaled_declarations_record_raw_physical_scale_and_offset() {
    let schema = example();
    let Some(Decl::Scaled(temp)) = schema.decl("Temperature") else { panic!("Temperature") };
    assert_eq!(temp.raw.kind, ScalarKind::Int(16));
    assert_eq!(temp.physical.value, FloatType::F32);
    assert_eq!(temp.scale.value, 0.01);
    assert!(temp.offset.is_none(), "offset defaults to 0 and is not written here");

    let Some(Decl::Scaled(battery)) = schema.decl("BatteryVoltage") else { panic!("BatteryVoltage") };
    assert_eq!(battery.raw.kind, ScalarKind::UInt(8));
    assert_eq!(battery.scale.value, 0.02);
}

#[test]
fn open_enum_variants_and_else_arm() {
    let schema = example();
    let Some(Decl::Enum(mode)) = schema.decl("HearingMode") else { panic!("HearingMode") };
    assert_eq!(mode.backing_bits.value, 4);
    let variants: Vec<(&str, Option<u128>)> =
        mode.variants.iter().map(|v| (v.name.name.as_str(), v.value.map(|v| v.value))).collect();
    assert_eq!(
        variants,
        vec![("Default", Some(0)), ("Stereo", Some(1)), ("Mono", Some(2)), ("Cinema", Some(3))]
    );
    assert!(mode.is_open());
    assert_eq!(mode.else_arm.as_ref().unwrap().name.name, "Unknown");
}

#[test]
fn struct_fields_cover_every_field_kind() {
    let schema = example();
    let Some(Decl::Struct(status)) = schema.decl("Status") else { panic!("Status") };
    assert_eq!(status.width_bits.unwrap().value, 64);
    assert!(!status.declared_variable());
    assert_eq!(status.fields.len(), 8);

    // named value fields, including alias / enum / scaled / nested struct types
    let named: Vec<&str> = status.fields.iter().filter_map(|f| f.name()).map(|i| i.name.as_str()).collect();
    assert_eq!(named, vec!["active_profile", "volume", "mode", "muted", "battery", "orientation", "flags"]);

    match &status.fields[3].kind {
        FieldKind::Value { ty, .. } => {
            assert_eq!(ty.kind, FieldTypeKind::Scalar(ScalarType { kind: ScalarKind::Bool, span: ty.span }))
        }
        other => panic!("expected bool field, got {other:?}"),
    }

    // bare `padding: u15` — ignored on decode
    match &status.fields[6].kind {
        FieldKind::Padding { bits, check_zero, .. } => {
            assert_eq!(bits.value, 15);
            assert!(!check_zero);
        }
        other => panic!("expected padding, got {other:?}"),
    }
    assert!(status.fields[6].name().is_none(), "padding is anonymous");

    // `reserved flags: u4` — round-tripped, and exposed
    match &status.fields[7].kind {
        FieldKind::Reserved { name, bits } => {
            assert_eq!(name.name, "flags");
            assert_eq!(bits.value, 4);
        }
        other => panic!("expected reserved, got {other:?}"),
    }
}

#[test]
fn validated_padding_is_distinct_from_bare_padding() {
    let schema = example();
    let Some(Decl::Struct(path)) = schema.decl("MotionPath") else { panic!("MotionPath") };
    match &path.fields[1].kind {
        FieldKind::Padding { bits, check_zero, .. } => {
            assert_eq!(bits.value, 16);
            assert!(check_zero, "`padding: u16 = 0` must decode-check the bits");
        }
        other => panic!("expected padding, got {other:?}"),
    }
}

#[test]
fn fixed_arrays_of_scalars_and_structs() {
    let schema = example();
    let Some(Decl::Struct(log)) = schema.decl("TemperatureLog") else { panic!("TemperatureLog") };
    match &log.fields[0].kind {
        FieldKind::Value { ty, .. } => match &ty.kind {
            FieldTypeKind::FixedArray { elem, count } => {
                assert_eq!(elem.named().unwrap().name, "Temperature");
                assert_eq!(count.value, 4);
            }
            other => panic!("expected fixed array, got {other:?}"),
        },
        other => panic!("expected value field, got {other:?}"),
    }

    let Some(Decl::Struct(path)) = schema.decl("MotionPath") else { panic!("MotionPath") };
    match &path.fields[0].kind {
        FieldKind::Value { ty, .. } => match &ty.kind {
            FieldTypeKind::FixedArray { elem, count } => {
                assert_eq!(elem.named().unwrap().name, "Orientation");
                assert_eq!(count.value, 2);
            }
            other => panic!("expected fixed array, got {other:?}"),
        },
        other => panic!("expected value field, got {other:?}"),
    }
}

#[test]
fn endian_attribute_is_resolved() {
    let schema = example();
    let Some(Decl::Struct(serial)) = schema.decl("LegacySerial") else { panic!("LegacySerial") };
    assert_eq!(serial.endian().map(|e| e.value), Some(Endianness::Big));

    let Some(Decl::Struct(status)) = schema.decl("Status") else { panic!("Status") };
    assert_eq!(status.endian(), None, "no attribute means the file default applies");
}

#[test]
fn variable_length_struct_omits_its_width() {
    let schema = example();
    let Some(Decl::Struct(label)) = schema.decl("DiagnosticLabel") else { panic!("DiagnosticLabel") };
    assert!(label.declared_variable());
    assert!(label.width_bits.is_none());
    assert_eq!(label.fields[0].intrinsic_bits(), Some(8));
    match &label.fields[1].kind {
        FieldKind::Value { name, ty } => {
            assert_eq!(name.name, "label");
            assert!(ty.is_intrinsically_variable());
            match ty.kind {
                FieldTypeKind::Str { max } => assert_eq!(max.value, 24),
                ref other => panic!("expected string, got {other:?}"),
            }
        }
        other => panic!("expected value field, got {other:?}"),
    }
}

#[test]
fn tagged_union_tag_container_and_variants() {
    let schema = example();
    let Some(Decl::Union(command)) = schema.decl("Command") else { panic!("Command") };
    assert_eq!(command.tag_name.name, "id");
    assert_eq!(command.tag_bits.value, 16);
    assert_eq!(command.container_bits.value, 64);
    assert_eq!(command.payload_bits(), Some(48));
    assert!(command.is_open());
    assert_eq!(command.else_arm.as_ref().unwrap().name.name, "Unknown");

    let variants: Vec<(&str, u128, usize, bool)> = command
        .variants
        .iter()
        .map(|v| (v.name.name.as_str(), v.id.value, v.fields.len(), v.has_payload_block))
        .collect();
    assert_eq!(
        variants,
        vec![
            ("SetVolume", 0x0001, 1, true),
            ("SetMute", 0x0002, 1, true),
            ("SetMode", 0x0003, 1, true),
            ("SetOrientationOffset", 0x0004, 1, true),
            ("TriggerFactoryReset", 0xffff, 0, false),
        ]
    );
}

#[test]
fn service_and_characteristic_bindings() {
    let schema = example();
    let service = schema.services().next().expect("service");
    assert_eq!(service.name.name, "HearingAidControl");
    assert_eq!(service.uuid.value, "7d8f0000-3c1a-4e8a-9b5a-000000000000");
    assert_eq!(service.characteristics.len(), 8);

    let status = &service.characteristics[0];
    assert_eq!(status.name.name, "StatusChar");
    assert_eq!(status.uuid.value, "7d8f0001-3c1a-4e8a-9b5a-000000000000");
    assert_eq!(
        status.properties.iter().map(|p| p.value).collect::<Vec<_>>(),
        vec![Property::Read, Property::Notify]
    );
    assert_eq!(status.ty.name, "Status");

    let command = &service.characteristics[1];
    assert_eq!(
        command.properties.iter().map(|p| p.value).collect::<Vec<_>>(),
        vec![Property::Write, Property::WriteWithoutResponse]
    );

    // A variable-length alias binds directly, with no wrapping struct (§6.3).
    let owner = &service.characteristics[4];
    assert_eq!(owner.name.name, "OwnerNameChar");
    assert_eq!(owner.ty.name, "OwnerName");
}

#[test]
fn doc_comments_are_attached_line_by_line() {
    let schema = example();
    let volume = schema.decl("Volume").expect("Volume");
    assert_eq!(
        volume.docs().iter().map(|d| d.text.as_str()).collect::<Vec<_>>(),
        vec!["Playback volume. The device only has 4 bits of resolution."]
    );

    // `//` comments are not docs.
    let orientation = schema.decl("Orientation").expect("Orientation");
    assert_eq!(orientation.docs().len(), 2);
    assert!(orientation.docs()[0].text.starts_with("Reusable 3-axis"));

    let Some(Decl::Union(command)) = schema.decl("Command") else { panic!("Command") };
    assert!(!command.docs.is_empty(), "the tagged union keeps its docs even with an attribute-free body");
}

#[test]
fn spans_point_at_the_written_source() {
    let schema = example();
    let Some(Decl::Struct(status)) = schema.decl("Status") else { panic!("Status") };
    assert_eq!(status.name.span.text(EXAMPLE), "Status");
    assert_eq!(status.width_bits.unwrap().span.text(EXAMPLE), "u64");
    let (line, col) = defgen::span::line_col(EXAMPLE, status.name.span.start);
    assert_eq!((line, col), (61, 8));
}

#[test]
fn f32_and_f64_are_scalar_wire_types() {
    let src = "endian: little;\n---\nstruct S: u96 { a: f32, b: f64, }";
    let parsed = parse(src);
    let rendered: Vec<String> = parsed.diagnostics.iter().map(|d| d.render_plain("test.defs", src)).collect();
    assert!(parsed.diagnostics.is_empty(), "unexpected diagnostics:\n{}", rendered.join("\n"));
    let schema = parsed.schema.expect("schema");
    let Some(Decl::Struct(s)) = schema.decl("S") else { panic!("S") };
    let field_kind = |name: &str| {
        s.fields
            .iter()
            .find_map(|f| match &f.kind {
                FieldKind::Value { name: n, ty } if n.name == name => Some(ty.kind.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no field named `{name}`"))
    };
    match field_kind("a") {
        FieldTypeKind::Scalar(ScalarType { kind: ScalarKind::Float(FloatType::F32), .. }) => {}
        other => panic!("expected f32, got {other:?}"),
    }
    match field_kind("b") {
        FieldTypeKind::Scalar(ScalarType { kind: ScalarKind::Float(FloatType::F64), .. }) => {}
        other => panic!("expected f64, got {other:?}"),
    }
}
