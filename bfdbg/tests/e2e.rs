//! End-to-end tests: drive the `bfdbg` binary with command scripts piped
//! to stdin the same way a non-interactive user would
//! (`bfdbg program.bf < cmds.txt`). The contract being pinned:
//!
//! - the program's `.` bytes land on stdout only,
//! - every debugger message lands on stderr only,
//! - parse/usage errors behave like the other tools,
//! - the time-travel workflow (error -> back -> set -> run) recovers.
//!
//! No external runtime is needed, so unlike the bfjs/bf2c execution
//! tests every one of these runs everywhere.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bfdbg-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    dir
}

fn write_source(dir: &Path, source: &str) -> PathBuf {
    let source_path = dir.join("program.bf");
    fs::write(&source_path, source).expect("writing test program");
    source_path
}

fn run_args(args: &[&Path]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bfdbg"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfdbg");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Runs a debugger session: `bfdbg [flags] program.bf` with `script`
/// piped to its stdin. Returns (status, stdout, stderr).
fn session(
    flags: &[&Path],
    program: &Path,
    script: &str,
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_bfdbg"))
        .args(flags)
        .arg(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning bfdbg");
    if let Some(pipe) = child.stdin.as_mut() {
        pipe.write_all(script.as_bytes()).expect("feeding commands");
    }
    let out = child.wait_with_output().expect("bfdbg session");
    (out.status, out.stdout, out.stderr)
}

// ---- CLI surface -----------------------------------------------------------

#[test]
fn help_flag_prints_usage_and_succeeds() {
    let dir = temp_dir("help");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_args(&[&source_path, Path::new("--help")]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("usage: bfdbg"), "{stdout}");
}

#[test]
fn no_arguments_prints_usage() {
    let (status, _, stderr) = run_args(&[]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("usage: bfdbg"), "{stderr}");
}

#[test]
fn unknown_option_fails() {
    let dir = temp_dir("unknown-opt");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("--turbo")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("unknown option: --turbo"), "{stderr}");
}

#[test]
fn option_without_a_value_fails() {
    let dir = temp_dir("no-value");
    let source_path = write_source(&dir, "+.");
    for opt in ["--stdin", "--break", "--watch", "--undo-limit"] {
        let (status, _, stderr) = run_args(&[&source_path, Path::new(opt)]);
        assert_eq!(status.code(), Some(1), "stderr: {stderr}");
        assert!(stderr.contains(&format!("{opt} needs")), "{stderr}");
    }
}

#[test]
fn bad_undo_limit_fails() {
    let dir = temp_dir("bad-limit");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("--undo-limit"), Path::new("x")]);
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr.contains("--undo-limit must be a non-negative integer"),
        "{stderr}"
    );
}

#[test]
fn unreadable_input_fails_cleanly() {
    let (status, _, stderr) = run_args(&[Path::new("/nonexistent/bfdbg-e2e/nope.bf")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't read"), "{stderr}");
}

#[test]
fn unreadable_stdin_file_fails_cleanly() {
    let dir = temp_dir("bad-stdin");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(&[
        &source_path,
        Path::new("--stdin"),
        Path::new("/nonexistent/bfdbg-e2e/nope.txt"),
    ]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't read"), "{stderr}");
}

#[test]
fn parse_error_names_the_line() {
    let dir = temp_dir("parse-error");
    let source_path = write_source(&dir, "+\n[");
    let (status, _, stderr) = run_args(&[&source_path]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("program.bf:2:"), "{stderr}");
    assert!(stderr.contains("unmatched '['"), "{stderr}");
}

#[test]
fn more_than_one_input_file_is_rejected() {
    let dir = temp_dir("two-inputs");
    let a = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(&[&a, &a]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("more than one input file"), "{stderr}");
}

// ---- sessions ---------------------------------------------------------------

#[test]
fn a_run_renders_on_stderr_and_program_output_on_stdout() {
    let dir = temp_dir("run");
    let source_path = write_source(&dir, "++.");
    let (status, prog, dbg) = session(&[], &source_path, "run\nquit\n");
    assert_eq!(status.code(), Some(0));
    assert_eq!(prog, vec![2u8]);
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(dbg.contains("program.bf — 3 ops"), "{dbg}");
    assert!(dbg.contains("program finished — 3 steps executed"), "{dbg}");
    // The prompt never appears when stdin is piped.
    assert!(!dbg.contains("(bfdbg)"), "{dbg}");
}

#[test]
fn eof_on_the_command_stream_quits_cleanly() {
    // No `quit` at all: the REPL reads EOF and exits 0.
    let dir = temp_dir("eof");
    let source_path = write_source(&dir, "+.");
    let (status, prog, _) = session(&[], &source_path, "");
    assert_eq!(status.code(), Some(0));
    assert!(prog.is_empty());
}

#[test]
fn step_and_back_round_trip_the_tape() {
    let dir = temp_dir("step-back");
    let source_path = write_source(&dir, "++");
    let (status, prog, dbg) = session(&[], &source_path, "step\nstep\ninfo\nback\ninfo\nquit\n");
    assert_eq!(status.code(), Some(0));
    assert!(prog.is_empty());
    let dbg = String::from_utf8_lossy(&dbg);
    // After two steps: 2 steps executed.
    assert!(dbg.contains("2 steps executed"), "{dbg}");
    // After one back: 1 step, and the state is paused again.
    assert!(dbg.contains("1 steps executed"), "{dbg}");
    assert!(dbg.contains("stepped back 1 op(s)"), "{dbg}");
}

#[test]
fn the_time_travel_workflow_recovers_from_underflow() {
    let dir = temp_dir("recover");
    let source_path = write_source(&dir, "+.<");
    // run: prints 1, then dies on `<`. back the failed op, move the
    // pointer, run again: the `<` now succeeds and the program ends.
    let (status, prog, dbg) = session(
        &[],
        &source_path,
        "run\nback\nset ptr = 1\nrun\ninfo\nquit\n",
    );
    assert_eq!(status.code(), Some(0));
    assert_eq!(prog, vec![1u8]);
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(
        dbg.contains("runtime error: pointer moved left of cell 0"),
        "{dbg}"
    );
    assert!(dbg.contains("failed at op 3/3"), "{dbg}");
    assert!(dbg.contains("hint: rewind it with `back`"), "{dbg}");
    assert!(dbg.contains("stepped back 1 op(s)"), "{dbg}");
    assert!(dbg.contains("ptr = 1"), "{dbg}");
    assert!(dbg.contains("program finished — 3 steps executed"), "{dbg}");
    assert!(dbg.contains("state:   finished"), "{dbg}");
}

#[test]
fn breakpoints_pause_and_list_highlights_the_source() {
    let dir = temp_dir("break");
    let source_path = write_source(&dir, "+++\n.\n");
    let (status, prog, dbg) = session(&[], &source_path, "break 2\nrun\nlist\nstep 2\nquit\n");
    assert_eq!(status.code(), Some(0));
    // `step 2` walks through the breakpoint: the third Add, then the
    // `.` — which prints the cell built up to that point.
    assert_eq!(prog, vec![3u8]);
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(
        dbg.contains("breakpoint at op 3/4 (line 1, col 3)"),
        "{dbg}"
    );
    assert!(
        dbg.contains("stopped — breakpoint at op 3/4 (line 1, col 3)"),
        "{dbg}"
    );
    assert!(
        dbg.contains("next: op 3/4 — line 1, col 3 — Add(1)"),
        "{dbg}"
    );
    assert!(dbg.contains("2 | ."), "{dbg}");
    assert!(dbg.contains("^"), "{dbg}");
}

#[test]
fn initial_break_and_watch_flags_install_stops() {
    let dir = temp_dir("flags");
    let source_path = write_source(&dir, ">>>");
    let (status, _, dbg) = session(
        &[
            Path::new("--break"),
            Path::new("1"),
            Path::new("--watch"),
            Path::new("ptr 2"), // one argv entry: the spec is "ptr 2"
        ],
        &source_path,
        "run\nquit\n",
    );
    assert_eq!(status.code(), Some(0));
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(dbg.contains("breakpoint at op 2/3"), "{dbg}");
    assert!(dbg.contains("watching ptr == 2"), "{dbg}");
    assert!(dbg.contains("stopped — breakpoint at op 2/3"), "{dbg}");
}

#[test]
fn watchpoints_trigger_on_pointer_and_cell() {
    let dir = temp_dir("watch");
    let source_path = write_source(&dir, ">>>");
    let (_, _, dbg) = session(&[], &source_path, "watch ptr 2\nrun\nquit\n");
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(dbg.contains("stopped — watchpoint: ptr == 2"), "{dbg}");

    let source_path = write_source(&dir, "+>+");
    let (_, _, dbg) = session(&[], &source_path, "watch cell 1\nrun\nquit\n");
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(
        dbg.contains("stopped — watchpoint: cell 1 changed 0 -> 1"),
        "{dbg}"
    );
}

#[test]
fn feed_and_stdin_file_fill_the_input_queue() {
    let dir = temp_dir("feed");
    // `, . [-] ,` — read a byte, print it, clear the cell, then read
    // again: on EOF the cleared cell stays 0, so the second print is 0.
    let source_path = write_source(&dir, ",.[-],.");
    let (status, prog, dbg) = session(&[], &source_path, "feed 65\nrun\nquit\n");
    assert_eq!(status.code(), Some(0));
    assert_eq!(prog, vec![65u8, 0u8]);
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(dbg.contains("queued 1 byte(s) — input queue: 1"), "{dbg}");

    // Same program with --stdin preloading two bytes.
    let stdin_path = dir.join("input.bin");
    fs::write(&stdin_path, [72u8, 105u8]).expect("writing stdin file");
    let source_path = write_source(&dir, ",.,.");
    let (_, prog, _) = session(
        &[Path::new("--stdin"), &stdin_path],
        &source_path,
        "run\nquit\n",
    );
    assert_eq!(prog, vec![72u8, 105u8]);
}

#[test]
fn tape_shows_dec_hex_ascii_and_the_pointer() {
    let dir = temp_dir("tape");
    let source_path = write_source(&dir, "+");
    let (_, _, dbg) = session(&[], &source_path, "step\ntape\nquit\n");
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(dbg.contains("tape: 30000 cells, ptr at 0"), "{dbg}");
    assert!(dbg.contains("idx"), "{dbg}");
    assert!(dbg.contains("dec"), "{dbg}");
    assert!(dbg.contains("hex"), "{dbg}");
    // Cell 0 = 1 after one step: the pointer row with dec 1.
    assert!(dbg.contains("> "), "{dbg}");
}

#[test]
fn tape_editing_and_info_round_trip() {
    let dir = temp_dir("set");
    let source_path = write_source(&dir, ".");
    let (status, prog, dbg) = session(&[], &source_path, "set cell[0] = 66\ninfo\nrun\nquit\n");
    assert_eq!(status.code(), Some(0));
    assert_eq!(prog, vec![66u8]);
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(dbg.contains("cell 0 = 66"), "{dbg}");
    assert!(dbg.contains("machine: ptr 0, tape 30000 cells"), "{dbg}");
    assert!(dbg.contains("state:   at the start"), "{dbg}");
}

#[test]
fn undo_limit_zero_disables_back() {
    let dir = temp_dir("no-undo");
    let source_path = write_source(&dir, "+");
    let (_, _, dbg) = session(
        &[Path::new("--undo-limit"), Path::new("0")],
        &source_path,
        "step\nback\ninfo\nquit\n",
    );
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(
        dbg.contains("undo log disabled (--undo-limit 0) — `back` is unavailable"),
        "{dbg}"
    );
    assert!(dbg.contains("undo:    disabled"), "{dbg}");
}

#[test]
fn help_lists_the_commands() {
    let dir = temp_dir("help-cmd");
    let source_path = write_source(&dir, "+.");
    let (_, prog, dbg) = session(&[], &source_path, "help\nquit\n");
    assert!(prog.is_empty());
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(dbg.contains("commands:"), "{dbg}");
    assert!(dbg.contains("back [n]"), "{dbg}");
    assert!(dbg.contains("set cell[<n>] = <v>"), "{dbg}");
    assert!(dbg.contains("feed <v> ..."), "{dbg}");
}

#[test]
fn unknown_command_is_an_error_not_a_crash() {
    let dir = temp_dir("bad-cmd");
    let source_path = write_source(&dir, "+.");
    let (status, _, dbg) = session(&[], &source_path, "frobnicate\nquit\n");
    assert_eq!(status.code(), Some(0));
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(
        dbg.contains("error: unknown command: \"frobnicate\""),
        "{dbg}"
    );
}

#[test]
fn echo_program_debug_session_stays_byte_exact() {
    let dir = temp_dir("echo");
    let source_path = write_source(&dir, ",[.[-],]");
    let script = "break 0\nrun\nstep\nrun\nquit\n";
    let (status, prog, dbg) = session(&[], &source_path, script);
    assert_eq!(status.code(), Some(0));
    // Breakpoint at op 0: nothing has run yet, so no stdin consumed yet —
    // the script's commands end in quit, so the program never reads.
    assert!(prog.is_empty());
    let dbg = String::from_utf8_lossy(&dbg);
    assert!(dbg.contains("stopped — breakpoint at op 1/8"), "{dbg}");
}
