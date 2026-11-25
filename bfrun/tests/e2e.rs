//! End-to-end tests: run real (hand-built) `.bfry` programs through the
//! `bfrun` binary and check stdout / stderr / exit status. The programs are
//! constructed directly as `bfformat::Program` values rather than compiled
//! from source, so these tests pin exactly what the loader-plus-VM pipeline
//! does with the on-disk format the `bfc` binary writes -- magic, version,
//! count, and op encoding -- while the VM's unit tests pin the semantics of
//! each op.

use bfformat::{Op, Program};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bfrun-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    dir
}

/// Serializes `ops` to a .bfry file and returns its path.
fn write_bfry(dir: &Path, ops: Vec<Op>) -> std::path::PathBuf {
    let path = dir.join("program.bfry");
    let mut bytes = Vec::new();
    Program { ops }
        .write_to(&mut bytes)
        .expect("serializing program");
    fs::write(&path, bytes).expect("writing program");
    path
}

/// Runs `bfrun` with raw args, empty stdin.
fn run_args(
    _tag: &str,
    args: &[&Path],
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let output = Command::new(env!("CARGO_BIN_EXE_bfrun"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfrun");
    (output.status, output.stdout, output.stderr)
}

/// Runs a program with piped stdin, returning (status, stdout, stderr).
fn run_program(
    tag: &str,
    ops: Vec<Op>,
    stdin: &[u8],
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let dir = temp_dir(tag);
    let path = write_bfry(&dir, ops);
    let mut child = Command::new(env!("CARGO_BIN_EXE_bfrun"))
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running bfrun");
    child
        .stdin
        .take()
        .expect("piping stdin")
        .write_all(stdin)
        .expect("writing stdin");
    let output = child.wait_with_output().expect("waiting for bfrun");
    (output.status, output.stdout, output.stderr)
}

#[test]
fn no_arguments_prints_usage() {
    let (status, stdout, stderr) = run_args("no-args", &[]);
    assert_eq!(status.code(), Some(1));
    assert_eq!(stdout, b"");
    assert!(String::from_utf8_lossy(&stderr).contains("usage: bfrun <program.bfry>"), "{stderr:?}");
}

#[test]
fn missing_file_fails_cleanly() {
    let (status, _, stderr) = run_args("missing-file", &[Path::new("/nonexistent/bfrun-e2e/x.bfry")]);
    assert_eq!(status.code(), Some(1));
    assert!(String::from_utf8_lossy(&stderr).contains("couldn't open"), "{stderr:?}");
}

#[test]
fn garbage_file_fails_at_the_loader() {
    let dir = temp_dir("garbage");
    let garbage = dir.join("program.bfry");
    fs::write(&garbage, b"not a bytecode file at all").expect("writing garbage");
    let (status, _, stderr) = run_args("garbage", &[&garbage]);
    assert_eq!(status.code(), Some(1));
    assert!(String::from_utf8_lossy(&stderr).contains("couldn't load"), "{stderr:?}");
    assert!(String::from_utf8_lossy(&stderr).contains("not a bfry bytecode file"), "{stderr:?}");
}

#[test]
fn pointer_underflow_is_a_clean_runtime_error() {
    let (status, stdout, stderr) = run_program("underflow", vec![Op::MoveLeft(1)], b"");
    assert_eq!(stdout, b"");
    assert_eq!(status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&stderr),
        "runtime error: pointer moved left of cell 0\n"
    );
}

#[test]
fn input_round_trips_through_stdout() {
    // The Input op's success path (a real byte arrives on stdin) can only be
    // exercised with actual pipes -- unit tests see EOF -- so it lives here:
    // read 'A', add 1, write 'B' back out.
    let (status, stdout, stderr) =
        run_program("io", vec![Op::Input, Op::Add(1), Op::Output], b"A");
    assert_eq!(stdout, b"B");
    assert_eq!(stderr, b"");
    assert!(status.success());
}

#[test]
fn input_at_eof_leaves_the_cell_unchanged() {
    // The other half of the Input contract: a closed stdin must leave the
    // cell at its prior value (0 + 1 = 1) and exit cleanly.
    let (status, stdout, stderr) =
        run_program("io-eof", vec![Op::Input, Op::Add(1), Op::Output], b"");
    assert_eq!(stdout, b"\x01");
    assert_eq!(stderr, b"");
    assert!(status.success());
}

#[test]
fn a_broken_stdout_pipe_is_a_clean_io_error() {
    // Rust ignores SIGPIPE, so a write to a pipe whose read end is already
    // gone surfaces as Err from write_all -- the VM must turn that into the
    // standard "runtime error: I/O error" exit, not a crash. This run
    // writes a single buffered byte, so the failure lands on the final
    // flush.
    let dir = temp_dir("broken-pipe-flush");
    let path = write_bfry(&dir, vec![Op::Add(65), Op::Output]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_bfrun"))
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running bfrun");
    drop(child.stdout.take()); // reader gone before the program runs
    let output = child.wait_with_output().expect("waiting for bfrun");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("I/O error"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn a_failing_write_on_a_broken_pipe_is_a_clean_io_error() {
    // Same setup, but enough output to overflow the line buffer so the
    // failure lands on write itself rather than the flush.
    let dir = temp_dir("broken-pipe-write");
    let ops: Vec<Op> = (0..2048).map(|_| Op::Output).collect();
    let path = write_bfry(&dir, ops);
    let mut child = Command::new(env!("CARGO_BIN_EXE_bfrun"))
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running bfrun");
    drop(child.stdout.take());
    let output = child.wait_with_output().expect("waiting for bfrun");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("I/O error"),
        "{:?}",
        output.stderr
    );
}

#[test]
fn optimized_style_ops_run_correctly() {
    // A program using the folded-op shapes bfc actually emits: counted adds,
    // MulAdd spread, Zero, Scan -- verified against the same semantics the
    // bfinterp/bfnative suites pin.
    let ops = vec![
        Op::Add(6),
        Op::MulAdd {
            offset: 1,
            factor: 6,
        },
        Op::Zero,
        Op::MoveRight(1),
        Op::MulAdd {
            offset: 1,
            factor: 1,
        },
        Op::Zero,
        Op::MoveRight(1),
        Op::Add(1),
        Op::Output,
    ];
    // 6 * 6 = 36 moved to cell 1, then moved to cell 2, +1 = 37 = '%'.
    let (status, stdout, stderr) = run_program("folded-ops", ops, b"");
    assert_eq!(stdout, b"%");
    assert_eq!(stderr, b"");
    assert!(status.success());
}
