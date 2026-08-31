mod arch;
mod codegen;
mod os;
mod target;
mod toolchain;

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
      --cc <command>     assembler/linker driver, flags included, split on
                         spaces (default: per-target; cross-compiling setups
                         usually want to override this)
      --emit-asm         write the assembly file only, don't invoke a
                         toolchain
      --keep-asm         keep the intermediate .s file beside the output
                         (it's also kept automatically when the toolchain
                         fails)";

struct Options {
    input: String,
    output: Option<String>,
    target: Option<String>,
    cc: Option<String>,
    emit_asm: bool,
    keep_asm: bool,
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
        cc,
        emit_asm,
        keep_asm,
    } = options;

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

    let assembly = codegen::emit_assembly(&ops, target);
    let output_path = match output {
        Some(path) => path,
        // Assembly-only mode defaults to a .s next to the source, since the
        // output isn't an executable.
        None if emit_asm => Path::new(&input)
            .with_extension("s")
            .to_string_lossy()
            .into_owned(),
        None => default_output_path(&input, target),
    };

    if emit_asm {
        if let Err(err) = fs::write(&output_path, &assembly) {
            eprintln!("couldn't write {output_path}: {err}");
            return ExitCode::FAILURE;
        }
        println!(
            "emitted {output_path} ({} instructions, target {target})",
            ops.len()
        );
        return ExitCode::SUCCESS;
    }

    let driver = cc.unwrap_or_else(|| target::default_driver(target));
    if driver.trim().is_empty() {
        eprintln!("--cc can't be empty");
        return ExitCode::FAILURE;
    }

    let asm_path = format!("{output_path}.s");
    if let Err(err) = fs::write(&asm_path, &assembly) {
        eprintln!("couldn't write {asm_path}: {err}");
        return ExitCode::FAILURE;
    }

    if let Err(err) = toolchain::assemble_and_link(
        Path::new(&asm_path),
        Path::new(&output_path),
        target,
        &driver,
    ) {
        // The .s file is deliberately left behind: driver failures are much
        // easier to diagnose with the assembly in hand.
        eprintln!("{err}\nassembly kept at {asm_path}");
        return ExitCode::FAILURE;
    }

    if !keep_asm {
        // Best effort — failing to clean up isn't worth failing the build.
        let _ = fs::remove_file(&asm_path);
    }

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
    let mut cc: Option<String> = None;
    let mut emit_asm = false;
    let mut keep_asm = false;

    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => output = Some(take_value(&mut args, "--output")?),
            "--target" => target = Some(take_value(&mut args, "--target")?),
            "--cc" => cc = Some(take_value(&mut args, "--cc")?),
            "--emit-asm" => emit_asm = true,
            "--keep-asm" => keep_asm = true,
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
        cc,
        emit_asm,
        keep_asm,
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
