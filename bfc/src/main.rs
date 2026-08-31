use bfc::{optimize, parser};
use bfformat::Program;
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    let input_path = match args.get(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: bfc <input.bf> [output.bfry]");
            return ExitCode::FAILURE;
        }
    };

    let output_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| default_output_path(input_path));

    let source = match fs::read_to_string(input_path) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("couldn't read {input_path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    let raw_ops = match parser::parse(&source) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("{input_path}:{}: {}", err.line, err.message);
            return ExitCode::FAILURE;
        }
    };

    let ops = optimize::optimize(raw_ops);
    let program = Program { ops };

    let file = match fs::File::create(&output_path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("couldn't create {output_path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(err) = program.write_to(file) {
        eprintln!("couldn't write {output_path}: {err}");
        return ExitCode::FAILURE;
    }

    println!(
        "compiled {input_path} -> {output_path} ({} instructions)",
        program.ops.len()
    );
    ExitCode::SUCCESS
}

fn default_output_path(input_path: &str) -> String {
    let path = Path::new(input_path);
    path.with_extension("bfry").to_string_lossy().into_owned()
}
