//! The worked example from SPEC.md must check clean, and produce exactly the
//! layout the spec describes: bit offsets, container sizes, resolved enum
//! values, root-ness and byte order.

use defgen::ast::{Endianness, FloatType, Property};
use defgen::compile;
use defgen::diag::Severity;
use defgen::model::*;

const EXAMPLE: &str = include_str!("examples/commands.defs");

fn example() -> Model {
    let compiled = compile(EXAMPLE);
    let errors: Vec<String> = compiled
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.render_plain("commands.defs", EXAMPLE))
        .collect();
    assert!(errors.is_empty(), "unexpected errors:\n{}", errors.join("\n"));
    compiled.model.expect("model")
}

fn ty<'m>(model: &'m Model, name: &str) -> &'m TypeDef {
    model.find(name).unwrap_or_else(|| panic!("no type named `{name}`"))
}

/// `(name, offset, bits)` for each field of a struct, in declaration order.
fn field_layout(model: &Model, name: &str) -> Vec<(String, u32, u32)> {
    ty(model, name)
        .as_struct()
        .unwrap_or_else(|| panic!("`{name}` is not a struct"))
        .fields
        .iter()
        .map(|f| {
            let label = match &f.role {
                FieldRole::Value { name } => name.clone(),
                FieldRole::Reserved { name } => format!("reserved {name}"),
                FieldRole::Padding { check_zero } => {
                    if *check_zero {
                        "padding = 0".to_string()
                    } else {
                        "padding".to_string()
                    }
                }
            };
            (label, f.offset_bits, f.layout.fixed_bits)
        })
        .collect()
}

#[test]
fn the_example_checks_clean_apart_from_non_fatal_warnings() {
    let compiled = compile(EXAMPLE);
    let messages: Vec<(Severity, &str)> =
        compiled.diagnostics.iter().map(|d| (d.severity, d.message.as_str())).collect();
    assert!(compiled.model.is_some());
    assert!(
        messages.iter().all(|(severity, _)| *severity == Severity::Warning),
        "the example must not produce errors: {messages:?}"
    );
    // Both variable-length characteristics can exceed the 20-byte default (§10),
    // and `Status` deliberately packs `battery`/`orientation` across a byte
    // boundary to keep the container dense (§6) — both are diagnostics, not errors.
    assert_eq!(messages.len(), 4, "{messages:?}");
}

#[test]
fn pragmas_reach_the_model() {
    let model = example();
    assert_eq!(model.endian, Endianness::Little);
}

// ---------------------------------------------------------------------------
// Layout (§6)
// ---------------------------------------------------------------------------

#[test]
fn fields_are_packed_lsb_first_with_no_gaps() {
    let model = example();
    assert_eq!(
        field_layout(&model, "Status"),
        vec![
            ("active_profile".to_string(), 0, 4),
            ("volume".to_string(), 4, 4),
            ("mode".to_string(), 8, 4),
            ("muted".to_string(), 12, 1),
            ("battery".to_string(), 13, 8),
            ("orientation".to_string(), 21, 24),
            ("padding".to_string(), 45, 15),
            ("reserved flags".to_string(), 60, 4),
        ]
    );
    assert_eq!(ty(&model, "Status").layout, Layout::fixed(64));
}

#[test]
fn a_containers_declared_width_is_its_layout() {
    let model = example();
    for (name, bits) in [
        ("Orientation", 24),
        ("Status", 64),
        ("TemperatureLog", 64),
        ("MotionPath", 64),
        ("LegacySerial", 32),
    ] {
        let def = ty(&model, name);
        assert_eq!(def.layout, Layout::fixed(bits), "{name}");
        assert_eq!(def.as_struct().unwrap().declared_bits, Some(bits), "{name}");
    }
    assert_eq!(ty(&model, "Command").layout, Layout::fixed(64));
}

#[test]
fn arrays_multiply_their_element_width() {
    let model = example();
    let samples = &ty(&model, "TemperatureLog").as_struct().unwrap().fields[0];
    let WireType::Array { elem, count } = &samples.ty else { panic!("{:?}", samples.ty) };
    assert_eq!(**elem, WireType::Named(ty(&model, "Temperature").id));
    assert_eq!(*count, 4);
    assert_eq!(samples.layout, Layout::fixed(64));

    // An array of structs works the same way (§6.1).
    let points = &ty(&model, "MotionPath").as_struct().unwrap().fields[0];
    assert_eq!(points.layout, Layout::fixed(48));
    assert_eq!(field_layout(&model, "MotionPath")[1], ("padding = 0".to_string(), 48, 16));
}

#[test]
fn padding_and_reserved_keep_their_decode_policy() {
    let model = example();
    let fields = &ty(&model, "Status").as_struct().unwrap().fields;
    assert!(matches!(fields[6].role, FieldRole::Padding { check_zero: false }));
    assert!(!fields[6].is_visible());
    assert!(matches!(&fields[7].role, FieldRole::Reserved { name } if name == "flags"));
    assert!(fields[7].is_visible());
    // `MotionPath`'s padding is the validated form.
    let padding = &ty(&model, "MotionPath").as_struct().unwrap().fields[1];
    assert!(matches!(padding.role, FieldRole::Padding { check_zero: true }));
}

// ---------------------------------------------------------------------------
// Variable-length types (§6.3)
// ---------------------------------------------------------------------------

#[test]
fn a_variable_length_struct_records_its_prefix_and_bound() {
    let model = example();
    let label = ty(&model, "DiagnosticLabel");
    assert_eq!(label.layout, Layout { fixed_bits: 8, tail: Some(Tail { elem_bits: 8, max_elems: 24 }) });
    assert!(label.layout.is_variable());
    assert_eq!(label.layout.fixed_bytes(), 1);
    assert_eq!(label.layout.max_bytes(), 25);
    assert_eq!(label.as_struct().unwrap().declared_bits, None);

    let name = ty(&model, "OwnerName");
    assert_eq!(name.layout, Layout { fixed_bits: 0, tail: Some(Tail { elem_bits: 8, max_elems: 32 }) });
    assert_eq!(name.layout.max_bytes(), 32);
}

// ---------------------------------------------------------------------------
// Enums and unions (§5, §7)
// ---------------------------------------------------------------------------

#[test]
fn enum_values_are_resolved() {
    let model = example();
    let mode = ty(&model, "HearingMode").as_enum().expect("enum");
    assert_eq!(mode.backing_bits, 4);
    let variants: Vec<(&str, u128)> = mode.variants.iter().map(|v| (v.name.as_str(), v.value)).collect();
    assert_eq!(variants, vec![("Default", 0), ("Stereo", 1), ("Mono", 2), ("Cinema", 3)]);
    let arm = mode.else_arm.as_ref().expect("open enum");
    assert_eq!(arm.name, "Unknown");
    assert_eq!(arm.raw_bits, 4, "the fallback carries `raw: u4` (§5)");
    assert_eq!(arm.id_bits, None, "only a union's fallback carries an id (§7)");
    assert!(mode.is_open());
}

#[test]
fn a_union_splits_its_container_into_tag_and_payload() {
    let model = example();
    let command = ty(&model, "Command").as_union().expect("union");
    assert_eq!(command.tag_name, "id");
    assert_eq!((command.tag_bits, command.container_bits, command.payload_bits), (16, 64, 48));

    let variants: Vec<(&str, u128, u32)> =
        command.variants.iter().map(|v| (v.name.as_str(), v.id, v.used_bits)).collect();
    assert_eq!(
        variants,
        vec![
            ("SetVolume", 0x0001, 4),
            ("SetMute", 0x0002, 1),
            ("SetMode", 0x0003, 4),
            ("SetOrientationOffset", 0x0004, 24),
            ("TriggerFactoryReset", 0xffff, 0),
        ]
    );

    // Variant payload offsets are relative to the payload region, so the first
    // field of every variant starts at 0.
    for variant in &command.variants {
        if let Some(first) = variant.fields.first() {
            assert_eq!(first.offset_bits, 0, "{}", variant.name);
        }
    }

    let arm = command.else_arm.as_ref().expect("open union");
    assert_eq!(
        (arm.id_bits, arm.raw_bits),
        (Some(16), 48),
        "fallback carries `{{ id: u16, raw: u48 }}` (§7)"
    );
}

// ---------------------------------------------------------------------------
// Aliases, scaled types, byte order (§3, §4, §8)
// ---------------------------------------------------------------------------

#[test]
fn aliases_keep_their_name_but_borrow_their_targets_layout() {
    let model = example();
    let volume = ty(&model, "Volume");
    assert_eq!(volume.layout, Layout::fixed(4));
    let TypeKind::Alias(alias) = &volume.kind else { panic!("alias") };
    assert_eq!(alias.target, WireType::UInt(4));

    // A field keeps referring to `Volume`, not to `u4` (§12 wants the name).
    let field = &ty(&model, "Status").as_struct().unwrap().fields[1];
    assert_eq!(field.ty, WireType::Named(volume.id));
}

#[test]
fn scaled_types_carry_their_conversion() {
    let model = example();
    let TypeKind::Scaled(temp) = &ty(&model, "Temperature").kind else { panic!("scaled") };
    assert_eq!((temp.raw_bits, temp.signed), (16, true));
    assert_eq!(temp.physical, FloatType::F32);
    assert_eq!((temp.scale, temp.offset), (0.01, 0.0), "offset defaults to 0 (§4)");
    assert_eq!(temp.raw_range(), (-32768, 32767));

    let TypeKind::Scaled(battery) = &ty(&model, "BatteryVoltage").kind else { panic!("scaled") };
    assert_eq!(battery.raw_range(), (0, 255));
}

#[test]
fn value_widths_map_onto_native_integers() {
    // Any width up to 128 is legal; §2 pins down what carries it, so five
    // backends cannot drift apart on `u12`.
    assert_eq!(carrier_bits(1), 8);
    assert_eq!(carrier_bits(12), 16);
    assert_eq!(carrier_bits(24), 32);
    assert_eq!(carrier_bits(48), 64);
    assert_eq!(carrier_bits(128), 128);
    assert_eq!(WireType::Int(12).carrier_bits(), Some(16));
    assert!(WireType::Int(12).is_signed());
    assert!(!WireType::UInt(12).is_signed());

    // The bounds encode has to check against, at both extremes of the range.
    assert_eq!(int_range(12, false), (0, 4095));
    assert_eq!(int_range(12, true), (-2048, 2047));
    assert_eq!(int_range(128, false), (0, u128::MAX));
    assert_eq!(int_range(128, true), (i128::MIN, i128::MAX as u128));

    let model = example();
    let volume = &ty(&model, "Status").as_struct().unwrap().fields[0];
    assert_eq!(volume.ty.carrier_bits(), Some(8), "a `u4` field rides in a byte");
}

#[test]
fn byte_order_is_resolved_per_type() {
    let model = example();
    for name in ["Status", "TemperatureLog", "Command"] {
        let def = ty(&model, name);
        assert_eq!(def.endian, Endianness::Little, "{name} inherits the file default (§8)");
        assert!(!def.endian_explicit, "{name}");
    }
    let legacy = ty(&model, "LegacySerial");
    assert_eq!(legacy.endian, Endianness::Big);
    assert!(legacy.endian_explicit);
}

#[test]
fn root_and_nested_use_are_tracked() {
    let model = example();
    let roots: Vec<&str> = model.roots().map(|t| t.name.as_str()).collect();
    assert_eq!(
        roots,
        vec!["OwnerName", "Status", "TemperatureLog", "LegacySerial", "DiagnosticLabel", "Command"]
    );

    // `Orientation` is only ever embedded — which is exactly why it may not
    // carry `#[endian(...)]` (§8).
    let orientation = ty(&model, "Orientation");
    assert!(orientation.nested && !orientation.root);
    // `Status` is bound to a characteristic and never embedded.
    assert!(ty(&model, "Status").root && !ty(&model, "Status").nested);
}

// ---------------------------------------------------------------------------
// GATT metadata (§10)
// ---------------------------------------------------------------------------

#[test]
fn characteristics_resolve_to_their_types_sizes_and_byte_order() {
    let model = example();
    let service = &model.services[0];
    assert_eq!(service.name, "HearingAidControl");
    assert_eq!(service.uuid, "7d8f0000-3c1a-4e8a-9b5a-000000000000");
    assert_eq!(service.characteristics.len(), 6);

    let status = &service.characteristics[0];
    assert_eq!(status.name, "StatusChar");
    assert_eq!(model.get(status.ty).name, "Status");
    assert_eq!(status.properties, vec![Property::Read, Property::Notify]);
    assert_eq!(status.layout.max_bytes(), 8);
    assert_eq!(status.endian, Endianness::Little);

    let serial = &service.characteristics[3];
    assert_eq!(serial.endian, Endianness::Big, "the binding picks up `#[endian(big)]` (§8)");

    let owner = &service.characteristics[4];
    assert!(owner.layout.is_variable());
    assert_eq!(owner.layout.max_bytes(), 32);
}

#[test]
fn every_characteristic_is_a_whole_number_of_bytes() {
    let model = example();
    for (service, c) in model.characteristics() {
        assert!(c.layout.is_byte_aligned(), "{}::{} is {} bits", service.name, c.name, c.layout.fixed_bits);
    }
}

#[test]
fn constants_are_resolved_with_no_type_of_their_own() {
    let model = example();
    assert!(
        model.types.iter().all(|t| t.name != "MaxWriteLength"),
        "a const is not a TypeDef, so it must not show up in model.types"
    );

    let names: Vec<&str> = model.consts.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["MaxWriteLength", "MinRatedTemperature"]);

    let max_write = model.consts.iter().find(|c| c.name == "MaxWriteLength").unwrap();
    assert_eq!((max_write.bits, max_write.signed), (8, false));
    assert_eq!((max_write.magnitude, max_write.negative), (32, false));

    let min_temp = model.consts.iter().find(|c| c.name == "MinRatedTemperature").unwrap();
    assert_eq!((min_temp.bits, min_temp.signed), (16, true));
    assert_eq!((min_temp.magnitude, min_temp.negative), (40, true));
    assert_eq!(min_temp.as_i128(), -40);
}

#[test]
fn a_signed_constant_at_i128_min_round_trips_exactly() {
    let src = "const T: i128 = -170141183460469231731687303715884105728;";
    let compiled = defgen::compile(src);
    assert!(compiled.diagnostics.is_empty(), "{:?}", compiled.diagnostics);
    let model = compiled.model.expect("model");
    assert_eq!(model.consts[0].as_i128(), i128::MIN);
}

#[test]
fn raw_f32_and_f64_fields_resolve_to_their_ieee754_widths() {
    let src = "endian: little;\n---\nstruct S: u96 { a: f32, b: f64, }";
    let compiled = defgen::compile(src);
    assert!(compiled.diagnostics.is_empty(), "{:?}", compiled.diagnostics);
    let model = compiled.model.expect("model");
    let s = ty(&model, "S").as_struct().unwrap();

    let a = &s.fields[0];
    assert_eq!(a.ty, WireType::Float(FloatType::F32));
    assert_eq!(a.layout, Layout::fixed(32));
    assert_eq!(a.offset_bits, 0);

    let b = &s.fields[1];
    assert_eq!(b.ty, WireType::Float(FloatType::F64));
    assert_eq!(b.layout, Layout::fixed(64));
    assert_eq!(b.offset_bits, 32);
}

#[test]
fn underlying_follows_alias_chains() {
    let model = example();
    let volume = ty(&model, "Volume").id;
    // `Volume` aliases a primitive, so it defines its own layout.
    assert_eq!(model.underlying(volume), volume);
    // A struct is its own underlying type.
    let status = ty(&model, "Status").id;
    assert_eq!(model.underlying(status), status);
}
