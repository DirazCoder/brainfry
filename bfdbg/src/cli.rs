//! `bfdbg` — the interactive, time-traveling Brainfuck debugger.
//!
//! Reads a `.bf` file with the shared parser and drops the *raw* op list
//! into a single-stepping engine (see `debugger.rs` for why the raw list
//! and not the optimizer's output): one op per source character, so every
//! step points at an exact line and column. The REPL (this file) reads
//! commands from stdin, executes them through `session.rs`, and routes
//! the streams the way a debugger should: the program's `.` bytes to
//! stdout, everything the debugger says to stderr.
//!
//! Commands (also `help` inside the session): `run`, `step`, `back`,
//! `tape`, `list`, `break`/`clear`, `watch`/`unwatch`, `set`, `feed`,
//! `info`, `quit`. The `,` op drains an input queue fed by `feed` or
//! preloaded via `--stdin <file>`; an empty queue is EOF and leaves the
//! cell untouched. The undo log powers `back` — see `debugger.rs` for
//! the record format and the cap.

use crate::debugger::{DEFAULT_UNDO_LIMIT, Debugger};
use crate::{debugger, session};
use bfc::parser;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;

const USAGE: &str = "usage: bfdbg [options] <input.bf>

options:
  --stdin <file>        preload the program's `,` input queue from a file
  --break <spec>        initial breakpoint — op index (42) or @line[:col]
  --watch <spec>        initial watchpoint — \"ptr <n>\" or \"cell <n>\"
  --undo-limit <n>      undo-history steps (default 1048576, 0 = disabled)
  -h, --help            this help

Interactive debugger over the raw parsed op list — one op per source
instruction, so every step maps to an exact line/column in the .bf file.
Commands come from stdin, program output goes to stdout, debugger
messages to stderr. `help` inside the session lists the commands.";

fn has_help_flag(args: &[String]) -> bool {
    args.iter()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
}

pub fn run(args: Vec<String>) -> ExitCode {
    if has_help_flag(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let (input, stdin_path, break_specs, watch_specs, undo_limit) = match parse_args(args) {
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

    // Raw parse, deliberately not the optimizer's output — the whole
    // point of a debugger is source correspondence; see debugger.rs.
    let ops = match parser::parse(&source) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("{input}:{}: {}", err.line, err.message);
            return ExitCode::FAILURE;
        }
    };

    let op_count = ops.len();
    let mut dbg = Debugger::new(ops, source, undo_limit);

    // Preload `,` input, then apply initial breakpoints/watchpoints by
    // replaying them as ordinary commands — one code path, one wording.
    if let Some(path) = stdin_path {
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("couldn't read {path}: {err}");
                return ExitCode::FAILURE;
            }
        };
        dbg.feed_stdin(&bytes);
    }

    let stdout = io::stdout();
    let mut prog_out = stdout.lock();
    let stderr = io::stderr();
    let mut dbg_out = stderr.lock();

    for spec in break_specs {
        let line = format!("break {spec}");
        dispatch(&mut dbg, &line, &mut prog_out, &mut dbg_out);
    }
    for spec in watch_specs {
        let line = format!("watch {spec}");
        dispatch(&mut dbg, &line, &mut prog_out, &mut dbg_out);
    }

    say(
        &mut dbg_out,
        &format!(
            "bfdbg: {input} — {op_count} ops (raw parse, one op per source instruction)\n\
             program output: stdout — debugger messages: stderr — `help` for commands\n"
        ),
    );

    // The prompt only appears on a real terminal, so piped sessions
    // (scripts, tests, `bfdbg prog.bf < cmds.txt`) print a clean
    // transcript instead of prompt spam.
    let interactive = io::stdin().is_terminal();
    let stdin = io::stdin();
    let mut reader = io::BufReader::new(stdin.lock());
    repl(
        &mut dbg,
        &mut reader,
        interactive,
        &mut prog_out,
        &mut dbg_out,
    );

    let _ = prog_out.flush();
    ExitCode::SUCCESS
}

/// The read/parse/dispatch loop, factored over trait objects so the
/// read-error, blank-line, and prompt arms are unit-testable in-process —
/// the same shape `bfrun`/`bfinterp` use for their stdio wrappers.
fn repl(
    dbg: &mut Debugger,
    reader: &mut dyn BufRead,
    interactive: bool,
    prog_out: &mut dyn Write,
    dbg_out: &mut dyn Write,
) {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF — the script is done
            Ok(_) => {}
            Err(_) => break,
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if line.trim().is_empty() {
            continue;
        }
        if interactive {
            let _ = write!(dbg_out, "(bfdbg) ");
            let _ = dbg_out.flush();
        }
        if dispatch(dbg, line, prog_out, dbg_out) {
            break;
        }
    }
}

/// Parses one input line into a command and executes it; true = quit.
fn dispatch(
    dbg: &mut Debugger,
    line: &str,
    prog_out: &mut dyn Write,
    dbg_out: &mut dyn Write,
) -> bool {
    match debugger::parse_command(line.trim()) {
        Ok(cmd) => session::execute_command(dbg, &cmd, prog_out, dbg_out),
        Err(msg) => {
            say(dbg_out, &format!("error: {msg}\n"));
            false
        }
    }
}

/// Say — write to the debugger's stderr and flush.
fn say(out: &mut dyn Write, s: &str) {
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

type Parsed = (String, Option<String>, Vec<String>, Vec<String>, usize);

fn parse_args(args: Vec<String>) -> Result<Parsed, String> {
    let mut input: Option<String> = None;
    let mut stdin_path: Option<String> = None;
    let mut break_specs: Vec<String> = Vec::new();
    let mut watch_specs: Vec<String> = Vec::new();
    let mut undo_limit: usize = DEFAULT_UNDO_LIMIT;

    let mut args = args.into_iter().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--stdin" => {
                stdin_path = Some(args.next().ok_or("--stdin needs a file path")?);
            }
            "--break" => {
                break_specs.push(args.next().ok_or("--break needs a spec: 42 | @3 | @3:5")?);
            }
            "--watch" => {
                watch_specs.push(
                    args.next()
                        .ok_or("--watch needs a spec: \"ptr <n>\" | \"cell <n>\"")?,
                );
            }
            "--undo-limit" => {
                let value = args.next().ok_or("--undo-limit needs a step count")?;
                undo_limit = value.parse().map_err(|_| {
                    format!("--undo-limit must be a non-negative integer, got {value:?}")
                })?;
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

    Ok((
        input.ok_or("no input file given")?,
        stdin_path,
        break_specs,
        watch_specs,
        undo_limit,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bfc::parser;
    use std::io::Cursor;

    #[test]
    fn help_flag_is_recognized() {
        assert!(has_help_flag(&["bfdbg".into(), "-h".into()]));
        assert!(has_help_flag(&["bfdbg".into(), "--help".into()]));
        assert!(!has_help_flag(&["bfdbg".into(), "program.bf".into()]));
    }

    /// A reader whose every read fails — the stdin-is-broken arm, which a
    /// real pipe can't be made to produce.
    struct FailingRead;
    impl io::Read for FailingRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("stdin went away"))
        }
    }
    impl BufRead for FailingRead {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::other("stdin went away"))
        }
        fn consume(&mut self, _: usize) {}
    }

    /// Drives one scripted session in-process: program stdout, debugger
    /// stderr, executed step count.
    fn session(source: &str, input: &str, interactive: bool) -> (Vec<u8>, String, u64) {
        let ops = parser::parse(source).unwrap();
        let mut dbg = Debugger::new(ops, source.to_string(), DEFAULT_UNDO_LIMIT);
        let mut reader = Cursor::new(input.to_string());
        let mut prog = Vec::new();
        let mut dbg_out = Vec::new();
        repl(&mut dbg, &mut reader, interactive, &mut prog, &mut dbg_out);
        (
            prog,
            String::from_utf8_lossy(&dbg_out).into_owned(),
            dbg.steps(),
        )
    }

    #[test]
    fn a_read_error_ends_the_session_without_a_panic() {
        let ops = parser::parse("+.").unwrap();
        let mut dbg = Debugger::new(ops, ".".into(), DEFAULT_UNDO_LIMIT);
        let mut prog = Vec::new();
        let mut dbg_out = Vec::new();
        repl(&mut dbg, &mut FailingRead, false, &mut prog, &mut dbg_out);
        assert_eq!(dbg.steps(), 0);
        assert!(prog.is_empty());
        // read_line's error arm only needs fill_buf to fail; read/consume
        // are trait plumbing it never reaches on that path, so drive both
        // directly to pin their behavior.
        let mut reader = FailingRead;
        assert!(io::Read::read(&mut reader, &mut [0u8; 4]).is_err());
        BufRead::consume(&mut reader, 0); // no-op, but must not panic
    }

    #[test]
    fn blank_lines_are_skipped_and_commands_still_run() {
        let (prog, dbg_out, steps) = session("+.", "\n\n   \nrun\nquit\n\n", false);
        assert_eq!(prog, vec![1u8]);
        assert_eq!(steps, 2);
        assert!(!dbg_out.contains("(bfdbg)"), "{dbg_out}");
    }

    #[test]
    fn eof_terminates_the_session_cleanly_without_quit() {
        let (prog, _, steps) = session("+.", "run", false);
        assert_eq!(prog, vec![1u8]);
        assert_eq!(steps, 2);
    }

    #[test]
    fn the_prompt_appears_only_in_interactive_sessions() {
        let (_, prompted, _) = session("+.", "run\nquit\n", true);
        assert!(prompted.contains("(bfdbg) "), "{prompted}");
        let (_, piped, _) = session("+.", "run\nquit\n", false);
        assert!(!piped.contains("(bfdbg)"), "{piped}");
    }
}
