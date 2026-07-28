//! The command line.
//!
//! Three subcommands, because what a schema can be turned into varies along
//! two independent axes and neither of them is optional: `codec` picks a
//! target language, `server` picks a BLE stack, and `check` picks nothing
//! because it generates nothing. No subcommand falls back on a target the
//! schema author never chose.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn defgen() -> Command {
    Command::new(env!("CARGO_BIN_EXE_defgen"))
}

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/examples/commands.defs")
}

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

#[test]
fn a_subcommand_is_required() {
    let out = defgen().arg(example()).output().expect("run defgen");
    assert!(!out.status.success(), "a schema path on its own is not a command");
    assert!(stderr(&out).contains("subcommand"), "{}", stderr(&out));

    let help = defgen().arg("--help").output().expect("run defgen");
    assert!(help.status.success());
    for name in ["codec", "server", "check"] {
        assert!(stdout(&help).contains(name), "--help should list `{name}`: {}", stdout(&help));
    }
}

#[test]
fn the_language_flag_is_required_by_codec() {
    let out = defgen().args(["codec"]).arg(example()).output().expect("run defgen");
    assert!(!out.status.success(), "a run with no --language must fail");
    assert!(stderr(&out).contains("--language"), "the error should name the missing flag: {}", stderr(&out));
}

#[test]
fn the_stack_flag_is_required_by_server() {
    let out = defgen().args(["server"]).arg(example()).output().expect("run defgen");
    assert!(!out.status.success(), "a run with no --stack must fail");
    assert!(stderr(&out).contains("--stack"), "the error should name the missing flag: {}", stderr(&out));
}

#[test]
fn an_unknown_language_is_rejected_with_the_known_ones_listed() {
    let out =
        defgen().args(["codec"]).arg(example()).args(["--language", "cobol"]).output().expect("run defgen");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("cobol"), "the error should quote what was asked for: {err}");
    assert!(err.contains("python"), "the error should list the languages that do exist: {err}");
}

#[test]
fn an_unknown_stack_is_rejected_with_the_known_ones_listed() {
    let out =
        defgen().args(["server"]).arg(example()).args(["--stack", "nimble"]).output().expect("run defgen");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("nimble"), "the error should quote what was asked for: {err}");
    assert!(err.contains("zephyr"), "the error should list the stacks that do exist: {err}");
}

#[test]
fn the_help_text_lists_each_registry_where_it_is_read_from() {
    // Read from the registries rather than spelling the names out: adding a
    // backend or a stack should not make this test wrong.
    let codec = defgen().args(["codec", "--help"]).output().expect("run defgen");
    assert!(codec.status.success());
    let expected = format!("[possible values: {}]", defgen::backends::names().join(", "));
    assert!(stdout(&codec).contains(&expected), "clap should list the backends ({expected})");

    let server = defgen().args(["server", "--help"]).output().expect("run defgen");
    assert!(server.status.success());
    let expected = format!("[possible values: {}]", defgen::stacks::names().join(", "));
    assert!(stdout(&server).contains(&expected), "clap should list the stacks ({expected})");
}

// ---------------------------------------------------------------------------
// codec
// ---------------------------------------------------------------------------

#[test]
fn generated_code_goes_to_stdout_by_default() {
    let out = defgen().args(["codec"]).arg(example()).args(["--language", "c"]).output().expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));
    let header = stdout(&out);
    assert!(header.starts_with("/*"), "stdout should be the header itself, not a summary");
    assert!(header.contains("status_encode"));
    // The MTU warnings (§10) are diagnostics, so they must not pollute the
    // stream a caller might be piping into a file.
    assert!(!header.contains("Warning"));
    assert!(stderr(&out).contains("20 bytes"), "warnings still go to stderr");
}

#[test]
fn the_out_flag_writes_a_file_and_summarizes() {
    let dir = scratch("out_file");
    let target = dir.join("commands.h");
    let out = defgen()
        .args(["codec"])
        .arg(example())
        .args(["--language", "c"])
        .arg("-o")
        .arg(&target)
        .output()
        .expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));

    let header = std::fs::read_to_string(&target).expect("the header was written");
    assert!(header.contains("#ifndef COMMANDS_H"));
    assert!(header.contains("status_encode"));

    // With output going to a file, stdout is free to carry the summary.
    let summary = stdout(&out);
    assert!(summary.contains("commands.defs: OK"), "{summary}");
    assert!(summary.contains("wrote commands.h"), "{summary}");
}

#[test]
fn a_missing_parent_directory_is_created() {
    let dir = scratch("nested_out");
    let target = dir.join("generated/ble/commands.h");
    let out = defgen()
        .args(["codec"])
        .arg(example())
        .args(["--language", "c"])
        .arg("-o")
        .arg(&target)
        .output()
        .expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(target.exists(), "defgen should create the directories it was pointed at");
}

#[test]
fn out_may_name_a_directory() {
    // Whichever backend comes next, `-o some/dir` has to keep working; a
    // multi-file backend has nowhere else to put its files.
    let dir = scratch("out_dir");
    let out = defgen()
        .args(["codec"])
        .arg(example())
        .args(["--language", "c"])
        .arg("-o")
        .arg(&dir)
        .output()
        .expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(dir.join("commands.h").exists(), "the file should land inside the directory");
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

#[test]
fn the_server_writes_the_table_and_the_codec_it_calls() {
    let dir = scratch("server");
    let out = defgen()
        .args(["server"])
        .arg(example())
        .args(["--stack", "zephyr"])
        .arg("-o")
        .arg(&dir)
        .output()
        .expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));

    // The table `#include`s the codec header, so generating one without the
    // other by default would hand someone a file that cannot compile.
    for name in ["commands.h", "commands_gatt.h", "commands_gatt.c"] {
        assert!(dir.join(name).exists(), "`server` should have written {name}");
    }
    let summary = stdout(&out);
    assert!(summary.contains("wrote commands_gatt.c"), "{summary}");
}

#[test]
fn no_codec_leaves_the_header_to_a_separate_run() {
    let dir = scratch("server_no_codec");
    let out = defgen()
        .args(["server"])
        .arg(example())
        .args(["--stack", "zephyr", "--no-codec"])
        .arg("-o")
        .arg(&dir)
        .output()
        .expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!dir.join("commands.h").exists(), "--no-codec should not have written the codec");
    assert!(dir.join("commands_gatt.c").exists());

    // The include is unconditional either way: the table needs the codec
    // whether or not this run generated it.
    let source = std::fs::read_to_string(dir.join("commands_gatt.h")).unwrap();
    assert!(source.contains("#include \"commands.h\""), "{source}");
}

#[test]
fn a_schema_with_no_service_has_no_server_to_generate() {
    let dir = scratch("no_service");
    let schema = dir.join("plain.defs");
    std::fs::write(&schema, "endian: little;\n---\nstruct S: u8 { a: u8, }\n").unwrap();

    let out = defgen()
        .args(["server"])
        .arg(&schema)
        .args(["--stack", "zephyr"])
        .arg("-o")
        .arg(&dir)
        .output()
        .expect("run defgen");
    assert!(!out.status.success(), "an empty service table is not a useful thing to write");
    assert!(stderr(&out).contains("no `service`"), "{}", stderr(&out));
    assert!(!dir.join("plain_gatt.c").exists());
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

#[test]
fn check_reports_a_clean_schema_without_generating_anything() {
    let out = defgen().args(["check"]).arg(example()).output().expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "a clean check has nothing to say on stdout: {}", stdout(&out));
}

#[test]
fn the_dump_flags_need_no_target_to_generate_for() {
    // The dumps are debugging aids for the schema itself, so asking for one
    // does not mean picking a language it has nothing to do with.
    for flag in ["--ast", "--model"] {
        let out = defgen().args(["check"]).arg(example()).arg(flag).output().expect("run defgen");
        assert!(out.status.success(), "{}", stderr(&out));
        let dump = stdout(&out);
        assert!(dump.contains("Status"), "{flag} should have dumped something");
        assert!(!dump.contains("#ifndef COMMANDS_H"), "{flag} generates no code to bury the dump");
    }
}

#[test]
fn check_fails_on_a_schema_that_does_not_check() {
    let dir = scratch("check_bad");
    let bad = dir.join("bad.defs");
    std::fs::write(&bad, "endian: little;\n---\nstruct S: u8 { a: u16, }\n").unwrap();

    let out = defgen().args(["check"]).arg(&bad).output().expect("run defgen");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("1 error"), "{}", stderr(&out));
}

// ---------------------------------------------------------------------------
// Failures common to every subcommand
// ---------------------------------------------------------------------------

#[test]
fn a_schema_that_does_not_check_generates_nothing() {
    let dir = scratch("bad_schema");
    let bad = dir.join("bad.defs");
    // The struct declares 8 bits and its fields supply 16 (§6, exact fit).
    std::fs::write(&bad, "endian: little;\n---\nstruct S: u8 { a: u16, }\n").unwrap();
    let target = dir.join("bad.h");

    let out = defgen()
        .args(["codec"])
        .arg(&bad)
        .args(["--language", "c"])
        .arg("-o")
        .arg(&target)
        .output()
        .expect("run defgen");
    assert!(!out.status.success(), "a schema with errors must not exit successfully");
    assert!(stderr(&out).contains("1 error"), "{}", stderr(&out));
    assert!(!target.exists(), "nothing should be written for a schema that did not check");
}

#[test]
fn an_unreadable_schema_fails_before_the_backend_runs() {
    let out =
        defgen().args(["codec", "does-not-exist.defs", "--language", "c"]).output().expect("run defgen");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("cannot read"), "{}", stderr(&out));
}
