//! `bfwasm` — compiles a `.bf` file straight to a standalone `.wasm`
//! module. Same front end as every other backend in this workspace
//! (`bfc::parser` + `bfc::optimize`), same "no external toolchain" rule
//! `bfnative` holds itself to: no `wat2wasm`, no `wasm-bindgen`, no
//! Emscripten. The module this writes runs as-is under any
//! WASI-preview1-capable runtime (`wasmtime run`, `wasmer run`, `node
//! --experimental-wasi-unstable-preview1`, ...).

mod codegen;
mod encode;

use bfc::{optimize, parser};
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: bfwasm [options] <input.bf>

options:
  -o, --output <file>    output path (default: input basename with .wasm)

Compiles straight to a standalone .wasm module — WASI fd_write/fd_read for
I/O, one page-granular linear memory for the tape, no external toolchain.
Run the result with any WASI-preview1 runtime, e.g.:

  wasmtime run program.wasm
  wasmer run program.wasm";

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
    // bfnative's main.rs for why this is reused rather than reimplemented.
    let raw_ops = match parser::parse(&source) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("{input}:{}: {}", err.line, err.message);
            return ExitCode::FAILURE;
        }
    };
    let ops = optimize::optimize(raw_ops);

    let module = codegen::compile(&ops);
    let output_path = output.unwrap_or_else(|| default_output_path(&input));

    if let Err(err) = fs::write(&output_path, &module) {
        eprintln!("couldn't write {output_path}: {err}");
        return ExitCode::FAILURE;
    }

    println!(
        "compiled {input} -> {output_path} ({} instructions, {} bytes)",
        ops.len(),
        module.len()
    );
    ExitCode::SUCCESS
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
        .with_extension("wasm")
        .to_string_lossy()
        .into_owned()
}
