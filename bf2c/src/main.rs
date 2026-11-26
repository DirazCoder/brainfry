//! `bf2c` — compiles a `.bf` file to portable C99 source.
//!
//! Same front end as every other backend in this workspace (`bfc::parser`
//! + `bfc::optimize`); the output is a `.c` file you hand to a real C
//!   compiler, which is the whole point: `bfnative` emits machine code
//!   itself for exactly two architectures with a clean, verifiable
//!   baseline code generator, while `bf2c` lets GCC/Clang/MSVC spend
//!   decades of optimizer engineering (register allocation, loop
//!   unrolling, dead-store elimination, SIMD auto-vectorization) on the
//!   program — and runs anywhere a C compiler exists. See `codegen.rs`
//!   for the emitted code's shape and its pointer discipline.

mod codegen;

use bfc::{optimize, parser};
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: bf2c [options] <input.bf>

options:
  -o, --output <file>    output path (default: input basename with .c)

Transpiles a .bf file to portable C99 through the shared bfc optimizer.
Compile the result with any C compiler and run it:

  cc -O3 -o program program.c && ./program";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if has_help_flag(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let (input, output) = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let source = match fs::read_to_string(&input) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("couldn't read {input}: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Same parse + optimize pipeline every other backend runs — see
    // bfwasm's main.rs for why this is reused rather than reimplemented.
    let raw_ops = match parser::parse(&source) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("{input}:{}: {}", err.line, err.message);
            return ExitCode::FAILURE;
        }
    };
    let ops = optimize::optimize(raw_ops);

    let c = codegen::compile(&ops, &input);
    let output_path = output.unwrap_or_else(|| default_output_path(&input));

    if let Err(err) = fs::write(&output_path, c) {
        eprintln!("couldn't write {output_path}: {err}");
        return ExitCode::FAILURE;
    }

    println!(
        "compiled {input} -> {output_path} ({} instructions)",
        ops.len()
    );
    ExitCode::SUCCESS
}

fn has_help_flag(args: &[String]) -> bool {
    args.iter()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
}

fn parse_args(args: Vec<String>) -> Result<(String, Option<String>), String> {
    let mut input: Option<String> = None;
    let mut output: Option<String> = None;

    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" | "--output" => {
                output = Some(args.next().ok_or("--output needs a value")?);
            }
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

    Ok((input.ok_or("no input file given")?, output))
}

fn default_output_path(input_path: &str) -> String {
    Path::new(input_path)
        .with_extension("c")
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_flag_is_recognized() {
        assert!(has_help_flag(&["bf2c".into(), "-h".into()]));
        assert!(has_help_flag(&["bf2c".into(), "--help".into()]));
        assert!(!has_help_flag(&["bf2c".into(), "program.bf".into()]));
    }
}
