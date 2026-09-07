//! `bfinterp` — raw-source Brainfuck interpreter.
//!
//! Reads a `.bf` file, parses it with the shared parser, and executes the
//! instruction list directly — no bytecode file, no optimizer (by default),
//! no machine code. It exists as the maximally simple ground-truth
//! reference implementation: the slowest correct way to run a program, and
//! therefore the one whose output the other three execution strategies
//! (`bfrun`, `bfjit`, `bfnative`) are differential-tested against.
//!
//! The interpreter engine lives in `interp.rs` and shares no runtime code
//! with `bfrun` (or any other tool) — only the parser front end is shared,
//! the same one every other backend uses.

mod interp;

use bfc::parser;
use std::env;
use std::fs;
use std::process::ExitCode;

const USAGE: &str = "usage: bfinterp [--optimize] <input.bf>

Runs a .bf source file directly on a naive interpreter (stdin in, stdout
out; no output file). --optimize runs the shared optimizer first — the
default is the raw, unoptimized execution path.";

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

    let mut optimize = false;
    let mut input: Option<String> = None;

    for arg in args.into_iter().skip(1) {
        match arg.as_str() {
            "--optimize" => optimize = true,
            other if other.starts_with('-') => {
                eprintln!("unknown option: {other}\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
            other => {
                if input.is_some() {
                    eprintln!("more than one input file given\n\n{USAGE}");
                    return ExitCode::FAILURE;
                }
                input = Some(other.to_string());
            }
        }
    }

    let input = match input {
        Some(path) => path,
        None => {
            eprintln!("{USAGE}");
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

    let ops = match parser::parse(&source) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("{input}:{}: {}", err.line, err.message);
            return ExitCode::FAILURE;
        }
    };

    // Default: execute exactly what the parser produced, one op per source
    // character. --optimize is a clearly separate opt-in so a run can be
    // compared against the naive path, never mistaken for it.
    let ops = if optimize {
        bfc::optimize::optimize(ops)
    } else {
        ops
    };

    match interp::run_stdio(&ops) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("runtime error: {err}");
            ExitCode::FAILURE
        }
    }
}
