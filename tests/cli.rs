//! The command line.
//!
//! `--backend` is required, so every successful run picks a target language
//! explicitly rather than falling back on one the schema author never chose.

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

#[test]
fn the_backend_flag_is_required() {
    let out = defgen().arg(example()).output().expect("run defgen");
    assert!(!out.status.success(), "a run with no --backend must fail");
    assert!(stderr(&out).contains("--backend"), "the error should name the missing flag: {}", stderr(&out));
}

#[test]
fn an_unknown_backend_is_rejected_with_the_known_ones_listed() {
    let out = defgen().arg(example()).args(["--backend", "cobol"]).output().expect("run defgen");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("cobol"), "the error should quote what was asked for: {err}");
    assert!(err.contains('c'), "the error should list the backends that do exist: {err}");
}

#[test]
fn the_help_text_lists_the_registered_backends() {
    let out = defgen().arg("--help").output().expect("run defgen");
    assert!(out.status.success());
    assert!(stdout(&out).contains("--backend"), "{}", stdout(&out));
    // Read from the registry rather than spelling the names out: adding a
    // backend should not make this test wrong.
    let expected = format!("[possible values: {}]", defgen::backends::names().join(", "));
    assert!(
        stdout(&out).contains(&expected),
        "clap should list the registry's names ({expected}): {}",
        stdout(&out)
    );
}

#[test]
fn generated_code_goes_to_stdout_by_default() {
    let out = defgen().arg(example()).args(["--backend", "c"]).output().expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));
    let header = stdout(&out);
    assert!(header.starts_with("/*"), "stdout should be the header itself, not a summary");
    assert!(header.contains("#define DEFGEN_SCHEMA_VERSION 2"));
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
    let out =
        defgen().arg(example()).args(["--backend", "c"]).arg("-o").arg(&target).output().expect("run defgen");
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
    let out =
        defgen().arg(example()).args(["--backend", "c"]).arg("-o").arg(&target).output().expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(target.exists(), "defgen should create the directories it was pointed at");
}

#[test]
fn out_may_name_a_directory() {
    // Whichever backend comes next, `-o some/dir` has to keep working; a
    // multi-file backend has nowhere else to put its files.
    let dir = scratch("out_dir");
    let out =
        defgen().arg(example()).args(["--backend", "c"]).arg("-o").arg(&dir).output().expect("run defgen");
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(dir.join("commands.h").exists(), "the file should land inside the directory");
}

#[test]
fn the_dump_flags_suppress_code_generation() {
    for flag in ["--ast", "--model"] {
        let out = defgen().arg(example()).args(["--backend", "c", flag]).output().expect("run defgen");
        assert!(out.status.success(), "{}", stderr(&out));
        let dump = stdout(&out);
        assert!(dump.contains("Status"), "{flag} should have dumped something");
        assert!(
            !dump.contains("#ifndef COMMANDS_H"),
            "{flag} is a debugging aid; a header printed alongside would bury it"
        );
    }
}

#[test]
fn a_schema_that_does_not_check_generates_nothing() {
    let dir = scratch("bad_schema");
    let bad = dir.join("bad.defs");
    // The struct declares 8 bits and its fields supply 16 (§6, exact fit).
    std::fs::write(&bad, "version = 1;\nendian: little;\n---\nstruct S: u8 { a: u16, }\n").unwrap();
    let target = dir.join("bad.h");

    let out =
        defgen().arg(&bad).args(["--backend", "c"]).arg("-o").arg(&target).output().expect("run defgen");
    assert!(!out.status.success(), "a schema with errors must not exit successfully");
    assert!(stderr(&out).contains("1 error"), "{}", stderr(&out));
    assert!(!target.exists(), "nothing should be written for a schema that did not check");
}

#[test]
fn an_unreadable_schema_fails_before_the_backend_runs() {
    let out = defgen().arg("does-not-exist.defs").args(["--backend", "c"]).output().expect("run defgen");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("cannot read"), "{}", stderr(&out));
}
