use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::builder::PossibleValuesParser;
use clap::{Parser, Subcommand};
use defgen::backends::{self, Generated, Options};
use defgen::diag::{Severity, emit_all};
use defgen::model::{Layout, Model};
use defgen::stacks;

/// Compiles a defgen schema into codecs, or into a firmware GATT server.
///
/// The two are separate subcommands rather than one flag with a longer list of
/// values, because they vary independently: `codec` picks a *language*, and
/// `server` picks a *BLE stack* — which is always C, and says nothing about
/// which language the apps on the other end are written in.
#[derive(Parser)]
#[command(name = "defgen", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate codecs for one target language
    Codec {
        /// Schema file to compile
        file: PathBuf,

        /// Target language to generate codecs for
        #[arg(short, long, value_name = "NAME", value_parser = PossibleValuesParser::new(backends::names()))]
        language: String,

        /// Where to write generated code: a file for a single-file backend, a
        /// directory otherwise. Defaults to standard output.
        #[arg(short, long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// Generate a firmware GATT server for one BLE stack, plus the C codecs it calls
    Server {
        /// Schema file to compile
        file: PathBuf,

        /// BLE stack to generate the service table for
        #[arg(short, long, value_name = "NAME", value_parser = PossibleValuesParser::new(stacks::names()))]
        stack: String,

        /// Directory to write the generated files to. Defaults to standard output.
        #[arg(short, long, value_name = "DIR")]
        out: Option<PathBuf>,

        /// Emit only the service table, leaving the codec header to a separate
        /// `defgen codec` run. The table includes that header either way.
        #[arg(long)]
        no_codec: bool,
    },

    /// Parse and check a schema without generating anything
    Check {
        /// Schema file to check
        file: PathBuf,

        /// Dump the parsed syntax tree
        #[arg(long)]
        ast: bool,

        /// Dump the checked model: resolved layouts, offsets, enum values
        #[arg(long)]
        model: bool,
    },
}

impl Command {
    fn file(&self) -> &Path {
        match self {
            Command::Codec { file, .. } | Command::Server { file, .. } | Command::Check { file, .. } => file,
        }
    }

    /// Where generated files go, and `None` for a subcommand that generates
    /// none.
    fn out(&self) -> Option<&Path> {
        match self {
            Command::Codec { out, .. } | Command::Server { out, .. } => out.as_deref(),
            Command::Check { .. } => None,
        }
    }
}

/// What a generating subcommand resolved to, before the schema is even read.
enum Generator {
    Codec(Box<dyn backends::Backend>),
    /// A GATT server, and whether its codec header comes along.
    Server {
        stack: Box<dyn stacks::Stack>,
        codec: bool,
    },
}

impl Generator {
    fn generate(&self, model: &Model, opts: &Options) -> Generated {
        match self {
            Generator::Codec(backend) => backend.generate(model, opts),
            Generator::Server { stack, codec } => {
                let mut generated = Generated::default();
                // The codec header first: it is what the table `#include`s, and
                // a listing that puts a dependency before its user reads the
                // way the two files are meant to be read.
                if *codec {
                    let name = stack.codec_backend();
                    let backend =
                        backends::find(name).unwrap_or_else(|| panic!("stack names a real backend: {name}"));
                    generated.files.extend(backend.generate(model, opts).files);
                }
                generated.files.extend(stack.generate(model, opts).files);
                generated
            }
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let path = cli.command.file().to_path_buf();

    let src = match std::fs::read_to_string(&path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("defgen: cannot read {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let filename = path.display().to_string();

    // Whichever generator the subcommand names is resolved before any work, so
    // an unknown name fails immediately rather than after a schema has been
    // parsed and checked.
    let generator = match resolve(&cli.command) {
        Ok(generator) => generator,
        Err(code) => return code,
    };

    let parsed = defgen::parse(&src);
    let Some(schema) = parsed.schema else {
        emit_all(&parsed.diagnostics, &filename, &src);
        return give_up(&filename, &parsed.diagnostics);
    };
    if matches!(cli.command, Command::Check { ast: true, .. }) {
        println!("{schema:#?}");
    }

    let checked = defgen::check(&schema);
    let mut diagnostics = parsed.diagnostics;
    diagnostics.extend(checked.diagnostics);
    emit_all(&diagnostics, &filename, &src);

    let Some(model) = checked.model else {
        return give_up(&filename, &diagnostics);
    };

    let Some(generator) = generator else {
        if matches!(cli.command, Command::Check { model: true, .. }) {
            println!("{model:#?}");
        }
        return ExitCode::SUCCESS;
    };

    // A schema with no `service` has no GATT server to generate, and an empty
    // table is a quiet way to hand someone firmware that advertises nothing.
    if matches!(generator, Generator::Server { .. }) && model.services.is_empty() {
        eprintln!("defgen: {filename} declares no `service`, so there is no GATT server to generate");
        return ExitCode::FAILURE;
    }

    let out = cli.command.out();
    let generated = generator.generate(&model, &Options::for_path(&path));
    match write_out(&generated, out) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("defgen: {e}");
            return ExitCode::FAILURE;
        }
    }
    if out.is_some() {
        summarize(&filename, &model, &generated);
    }
    ExitCode::SUCCESS
}

/// Looks the subcommand's generator up in its registry, listing what does
/// exist when the name does not. `Ok(None)` is `check`, which generates
/// nothing.
fn resolve(command: &Command) -> Result<Option<Generator>, ExitCode> {
    match command {
        Command::Codec { language, .. } => match backends::find(language) {
            Some(backend) => Ok(Some(Generator::Codec(backend))),
            None => {
                eprintln!("defgen: unknown language `{language}`; known languages are:");
                for b in backends::all() {
                    eprintln!("  {:<12} {}", b.name(), b.description());
                }
                Err(ExitCode::FAILURE)
            }
        },
        Command::Server { stack, no_codec, .. } => match stacks::find(stack) {
            Some(stack) => Ok(Some(Generator::Server { stack, codec: !no_codec })),
            None => {
                eprintln!("defgen: unknown stack `{stack}`; known stacks are:");
                for s in stacks::all() {
                    eprintln!("  {:<12} {}", s.name(), s.description());
                }
                Err(ExitCode::FAILURE)
            }
        },
        Command::Check { .. } => Ok(None),
    }
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
    println!("  {}-endian", model.endian.as_str());

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

    for c in &model.consts {
        let ty = format!("{}{}", if c.signed { "i" } else { "u" }, c.bits);
        let value = if c.negative { format!("-{}", c.magnitude) } else { c.magnitude.to_string() };
        println!("  const {}: {ty} = {value}", c.name);
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
