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

    write_program(&program, input_path, &output_path, file)
}

fn default_output_path(input_path: &str) -> String {
    let path = Path::new(input_path);
    path.with_extension("bfry").to_string_lossy().into_owned()
}

fn write_program<W: std::io::Write>(
    program: &Program,
    input_path: &str,
    output_path: &str,
    writer: W,
) -> ExitCode {
    if let Err(err) = program.write_to(writer) {
        eprintln!("couldn't write {output_path}: {err}");
        return ExitCode::FAILURE;
    }

    println!(
        "compiled {input_path} -> {output_path} ({} instructions)",
        program.ops.len()
    );
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("forced write failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // A read-only handle fails on every OS, unlike /dev/full. Both outcomes
    // run through the `File` instantiation, which coverage counts on its own.
    #[test]
    fn file_writes_report_success_and_failure() {
        let path = std::env::temp_dir().join(format!("bfc-write-{}.bfry", std::process::id()));
        let program = Program {
            ops: vec![bfformat::Op::Zero],
        };
        let writable = std::fs::File::create(&path).unwrap();
        assert_eq!(
            write_program(&program, "input.bf", "output.bfry", writable),
            ExitCode::SUCCESS
        );
        let read_only = std::fs::File::open(&path).unwrap();
        assert_eq!(
            write_program(&program, "input.bf", "output.bfry", read_only),
            ExitCode::FAILURE
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn write_failure_returns_failure() {
        assert!(io::Write::flush(&mut FailingWriter).is_ok());
        let program = Program {
            ops: vec![bfformat::Op::Zero],
        };
        assert_eq!(
            write_program(&program, "input.bf", "output.bfry", FailingWriter),
            ExitCode::FAILURE
        );
    }

    #[test]
    fn write_success_returns_success() {
        let program = Program {
            ops: vec![bfformat::Op::Zero],
        };
        assert_eq!(
            write_program(&program, "input.bf", "output.bfry", Vec::new()),
            ExitCode::SUCCESS
        );
    }
}
