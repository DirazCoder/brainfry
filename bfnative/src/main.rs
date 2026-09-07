mod codegen;
mod listing;
mod obj;
mod sha256;
mod target;

use bfc::{optimize, parser};
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use target::{Os, Target};

const USAGE: &str = "usage: bfnative [options] <input.bf>

options:
  -o, --output <file>    output path (default: input basename; .exe for
                         windows targets)
      --target <name>    linux-x86_64 | linux-aarch64 | macos-x86_64 |
                         macos-aarch64 | windows-x86_64 | windows-aarch64
                         (default: host)
      --emit-asm         write an annotated listing of the generated machine
                         code (.lst) instead of the executable — the old
                         assembly-text output no longer exists since nothing
                         is assembled by an external toolchain
      --cc <command>     deprecated and ignored: bfnative builds executables
                         entirely in-process now, no external toolchain
      --keep-asm         deprecated and ignored: there is no intermediate
                         assembly file anymore";

struct Options {
    input: String,
    output: Option<String>,
    target: Option<String>,
    emit_asm: bool,
    // --cc / --keep-asm parse for compatibility and warn.
    deprecated: Vec<&'static str>,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args
        .iter()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
    {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let options = match parse_options(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let Options {
        input,
        output,
        target,
        emit_asm,
        deprecated,
    } = options;

    for flag in deprecated {
        eprintln!(
            "warning: {flag} is deprecated and ignored (bfnative no longer uses an external toolchain)"
        );
    }

    let target = match target.map(|name| Target::from_name(&name)) {
        Some(Ok(target)) => target,
        Some(Err(message)) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
        None => Target::host(),
    };

    let source = match fs::read_to_string(&input) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("couldn't read {input}: {err}");
            return ExitCode::FAILURE;
        }
    };

    // The same parse + optimize pipeline bfc runs before writing .bfry,
    // reused rather than reimplemented, so the bytecode and native backends
    // can't drift apart.
    let raw_ops = match parser::parse(&source) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("{input}:{}: {}", err.line, err.message);
            return ExitCode::FAILURE;
        }
    };
    let ops = optimize::optimize(raw_ops);

    let mut module = codegen::emit(&ops, target);

    let output_path = match output {
        Some(path) => path,
        // Listing-only mode defaults to a .lst next to the source, since the
        // output isn't an executable.
        None if emit_asm => Path::new(&input)
            .with_extension("lst")
            .to_string_lossy()
            .into_owned(),
        None => default_output_path(&input, target),
    };

    let module_ref = &mut module;
    let (bytes, layout) = obj::build(module_ref, target, &output_path);

    if emit_asm {
        let text = listing::render(&module, target, &layout);
        if let Err(err) = fs::write(&output_path, text) {
            eprintln!("couldn't write {output_path}: {err}");
            return ExitCode::FAILURE;
        }
        println!(
            "emitted {output_path} ({} instructions, target {target})",
            ops.len()
        );
        return ExitCode::SUCCESS;
    }

    if let Err(err) = fs::write(&output_path, &bytes) {
        eprintln!("couldn't write {output_path}: {err}");
        return ExitCode::FAILURE;
    }
    mark_executable(&output_path);

    println!(
        "compiled {input} -> {output_path} ({} instructions, target {target})",
        ops.len()
    );
    ExitCode::SUCCESS
}

fn parse_options(args: Vec<String>) -> Result<Options, String> {
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;
    let mut target: Option<String> = None;
    let mut emit_asm = false;
    let mut deprecated: Vec<&'static str> = Vec::new();

    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => output = Some(take_value(&mut args, "--output")?),
            "--target" => target = Some(take_value(&mut args, "--target")?),
            "--emit-asm" => emit_asm = true,
            "--cc" => {
                let _ = take_value(&mut args, "--cc")?;
                deprecated.push("--cc");
            }
            "--keep-asm" => deprecated.push("--keep-asm"),
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => {
                if input.is_some() {
                    return Err("more than one input file given".to_string());
                }
                input = Some(other.to_string());
            }
        }
    }

    Ok(Options {
        input: input.ok_or("no input file given")?,
        output,
        target,
        emit_asm,
        deprecated,
    })
}

fn take_value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

fn default_output_path(input_path: &str, target: Target) -> String {
    let mut output = Path::new(input_path)
        .with_extension("")
        .to_string_lossy()
        .into_owned();
    if target.os == Os::Windows {
        output.push_str(".exe");
    }
    output
}

/// Executables need the execute bit; the old toolchain set it implicitly.
fn mark_executable(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}
