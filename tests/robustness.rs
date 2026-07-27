//! The front end must terminate and never panic, whatever it is fed. Every
//! case runs the checker too: a schema that parses is fed straight to it, so
//! mutations of the example exercise name resolution and layout as well.

use defgen::compile;

const EXAMPLE: &str = include_str!("examples/commands.defs");

#[test]
fn every_prefix_of_the_example_compiles_without_panicking() {
    for end in 0..=EXAMPLE.len() {
        if !EXAMPLE.is_char_boundary(end) {
            continue;
        }
        let _ = compile(&EXAMPLE[..end]);
    }
}

#[test]
fn every_single_line_deletion_compiles_without_panicking() {
    let lines: Vec<&str> = EXAMPLE.lines().collect();
    for skip in 0..lines.len() {
        let mutated: String =
            lines.iter().enumerate().filter(|(i, _)| *i != skip).map(|(_, l)| format!("{l}\n")).collect();
        let _ = compile(&mutated);
    }
}

#[test]
fn pathological_inputs_terminate() {
    for src in [
        "",
        "---",
        "\0",
        "{{{{{{{{",
        "}}}}}}}}",
        "endian: little; --- struct",
        "endian: little;\n---\nstruct S: u8 {",
        "endian: little;\n---\nenum E(id: u8): u16 {",
        "endian: little;\n---\nservice S(uuid: \"x\") { characteristic",
        &"struct S: u8 { x: u8, ".repeat(200),
        &"#[endian(big)]".repeat(200),
        &",".repeat(500),
    ] {
        let _ = compile(src);
    }
}
