mod vm;

use bfformat::Program;
use std::env;
use std::fs::File;
use std::process::ExitCode;
use vm::Vm;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    let input_path = match args.get(1) {
        Some(path) => path,
        None => {
            eprintln!("usage: bfrun <program.bfry>");
            return ExitCode::FAILURE;
        }
    };

    let file = match File::open(input_path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("couldn't open {input_path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    let program = match Program::read_from(file) {
        Ok(program) => program,
        Err(err) => {
            eprintln!("couldn't load {input_path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut vm = Vm::new();
    if let Err(err) = vm.run(&program.ops) {
        eprintln!("runtime error: {err}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
