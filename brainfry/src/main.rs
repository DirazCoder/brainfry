use std::process::ExitCode;

const USAGE: &str = "usage: brainfry <tool> [arguments]

tools:
  bfc       compile Brainfuck source to .bfry bytecode
  bfrun     execute .bfry bytecode
  bfnative  compile source to a native executable (requires --target in WASI)
  bfinterp  interpret Brainfuck source
  bfwasm    compile source to WASI WebAssembly or browser JavaScript
  bfjs      compile source to JavaScript
  bf2c      compile source to C
  bfdbg     debug Brainfuck source

bfjit is not included in the WASI build.";

fn main() -> ExitCode {
    run(std::env::args().collect())
}

fn run(args: Vec<String>) -> ExitCode {
    let Some(command) = args.get(1) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };

    if matches!(command.as_str(), "-h" | "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    dispatch(command, args[1..].to_vec())
}

fn dispatch(command: &str, args: Vec<String>) -> ExitCode {
    match command {
        "bfc" => bfc::cli::run(args),
        "bfrun" => bfrun::cli::run(args),
        "bfnative" => bfnative::cli::run(args),
        "bfinterp" => bfinterp::cli::run(args),
        "bfwasm" => bfwasm::cli::run(args),
        "bfjs" => bfjs::cli::run(args),
        "bf2c" => bf2c::cli::run(args),
        "bfdbg" => bfdbg::cli::run(args),
        _ => {
            eprintln!("unknown tool: {command}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_help_and_missing_command_have_distinct_statuses() {
        assert_eq!(
            run(vec!["brainfry".into(), "--help".into()]),
            ExitCode::SUCCESS
        );
        assert_eq!(run(vec!["brainfry".into()]), ExitCode::FAILURE);
    }

    #[test]
    fn each_tool_is_dispatched_with_its_original_argument_shape() {
        let commands = [
            ("bfc", vec![]),
            ("bfrun", vec![]),
            ("bfnative", vec!["--help".into()]),
            ("bfinterp", vec!["--help".into()]),
            ("bfwasm", vec!["--help".into()]),
            ("bfjs", vec!["--help".into()]),
            ("bf2c", vec!["--help".into()]),
            ("bfdbg", vec!["--help".into()]),
        ];

        for (command, trailing_args) in commands {
            let mut args = vec!["brainfry".to_string(), command.to_string()];
            args.extend(trailing_args);
            let result = run(args);
            if matches!(command, "bfc" | "bfrun") {
                assert_eq!(result, ExitCode::FAILURE, "{command}");
            } else {
                assert_eq!(result, ExitCode::SUCCESS, "{command}");
            }
        }
    }

    #[test]
    fn unknown_tool_is_rejected() {
        assert_eq!(
            run(vec!["brainfry".into(), "bfjit".into()]),
            ExitCode::FAILURE
        );
        assert_eq!(
            run(vec!["brainfry".into(), "not-a-tool".into()]),
            ExitCode::FAILURE
        );
    }
}
