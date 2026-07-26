use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use clap::builder::PossibleValuesParser;
use defgen::backends::{self, Generated, Options};
use defgen::diag::{Severity, emit_all};
use defgen::model::{Layout, Model};

/// Compiles a defgen schema into source for a target language.
#[derive(Parser)]
#[command(name = "defgen")]
struct Cli {
    /// Schema file to compile
    file: PathBuf,

    /// Target language to generate code for
    #[arg(long, value_name = "NAME", value_parser = PossibleValuesParser::new(backends::names()))]
    backend: String,

    /// Where to write generated code: a file for a single-file backend, a
    /// directory otherwise. Defaults to standard output.
    #[arg(short, long, value_name = "PATH")]
    out: Option<PathBuf>,

    /// Dump the parsed syntax tree instead of generating code
    #[arg(long)]
    ast: bool,

    /// Dump the checked model instead of generating code
    #[arg(long)]
    model: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let path = cli.file;

    let src = match std::fs::read_to_string(&path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("defgen: cannot read {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let filename = path.display().to_string();

    // The backend is resolved before any work, so an unknown name fails
    // immediately rather than after a schema has been parsed and checked.
    let Some(backend) = backends::find(&cli.backend) else {
        eprintln!("defgen: unknown backend `{}`; known backends are:", cli.backend);
        for b in backends::all() {
            eprintln!("  {:<8} {}", b.name(), b.description());
        }
        return ExitCode::FAILURE;
    };

    let parsed = defgen::parse(&src);
    let Some(schema) = parsed.schema else {
        emit_all(&parsed.diagnostics, &filename, &src);
        return give_up(&filename, &parsed.diagnostics);
    };
    if cli.ast {
        println!("{schema:#?}");
    }

    let checked = defgen::check(&schema);
    let mut diagnostics = parsed.diagnostics;
    diagnostics.extend(checked.diagnostics);
    emit_all(&diagnostics, &filename, &src);

    let Some(model) = checked.model else {
        return give_up(&filename, &diagnostics);
    };

    if cli.model {
        println!("{model:#?}");
    }
    // The dump flags are debugging aids, and printing a header into the same
    // stream would bury them.
    if cli.ast || cli.model {
        return ExitCode::SUCCESS;
    }

    let generated = backend.generate(&model, &Options::for_path(&path));
    match write_out(&generated, cli.out.as_deref()) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("defgen: {e}");
            return ExitCode::FAILURE;
        }
    }
    if cli.out.is_some() {
        summarize(&filename, &model, &generated);
    }
    ExitCode::SUCCESS
}

/// Writes generated files to `out`, or to standard output when it is `None`.
///
/// A single-file backend writes straight to the path given, which is what
/// `-o codec.h` should obviously do. With more than one file — or a path that
/// is already a directory — the path is treated as a directory instead.
fn write_out(generated: &Generated, out: Option<&Path>) -> std::io::Result<()> {
    let Some(out) = out else {
        for (i, file) in generated.files.iter().enumerate() {
            if generated.files.len() > 1 {
                if i > 0 {
                    println!();
                }
                println!("/* ===== {} ===== */", file.name);
            }
            print!("{}", file.contents);
        }
        return Ok(());
    };

    if generated.files.len() == 1 && !out.is_dir() {
        if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        return std::fs::write(out, &generated.files[0].contents);
    }

    std::fs::create_dir_all(out)?;
    for file in &generated.files {
        std::fs::write(out.join(&file.name), &file.contents)?;
    }
    Ok(())
}

fn give_up(filename: &str, diagnostics: &[defgen::Diagnostic]) -> ExitCode {
    let errors = diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
    eprintln!("defgen: {filename} has {errors} error{}", if errors == 1 { "" } else { "s" });
    ExitCode::FAILURE
}

fn summarize(filename: &str, model: &Model, generated: &Generated) {
    println!("{filename}: OK");
    println!("  version {}, {}-endian", model.version, model.endian.as_str());

    for ty in &model.types {
        let mut line = format!("  {} {}: {}", ty.kind_str(), ty.name, describe(ty.layout));
        if ty.root {
            line.push_str(", root");
            if ty.endian_explicit {
                line.push_str(&format!(" ({}-endian)", ty.endian.as_str()));
            }
        }
        println!("{line}");
    }

    for service in &model.services {
        println!("  service {} ({})", service.name, service.uuid);
        for c in &service.characteristics {
            let props: Vec<&str> = c.properties.iter().map(|p| p.as_str()).collect();
            println!(
                "    {}: {} — {} [{}]",
                c.name,
                model.get(c.ty).name,
                describe(c.layout),
                props.join(", ")
            );
        }
    }

    for file in &generated.files {
        println!("  wrote {} ({} bytes)", file.name, file.contents.len());
    }
}

/// A layout as a size, spelling out the variable part when there is one (§6.3).
fn describe(layout: Layout) -> String {
    let fixed = if layout.is_byte_aligned() {
        bytes(u128::from(layout.fixed_bytes()))
    } else {
        format!("{} bits", layout.fixed_bits)
    };
    match layout.tail {
        None => fixed,
        Some(tail) if layout.fixed_bits == 0 => format!("up to {}", bytes(tail.max_bits() / 8)),
        Some(tail) => {
            format!("{fixed} + up to {} ({} max)", bytes(tail.max_bits() / 8), bytes(layout.max_bytes()))
        }
    }
}

fn bytes(n: u128) -> String {
    format!("{n} byte{}", if n == 1 { "" } else { "s" })
}
