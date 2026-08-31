//! End-to-end tests: compile real programs with the bfnative binary, run the
//! produced executables, and check their output. These only run when the
//! host is linux-x86_64, the one target whose toolchain (`cc`) can be
//! assumed present in any environment this test runs in — everything cross
//! is exercised manually against whatever toolchain is installed, since
//! `cargo test` can't assume cross-compilers or emulators exist.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

const HELLO_WORLD: &str = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.\
                           +++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";

#[test]
fn hello_world_matches_expected_output() {
    run("hello-world", HELLO_WORLD, "", "Hello World!\n");
}

#[test]
fn cells_wrap_at_256() {
    // 256 increments on one cell fold to Add(255) + Add(1) and wrap to 0.
    run("wrap", &format!("{}.", "+".repeat(256)), "", "\x00");
}

#[test]
fn eof_leaves_the_cell_unchanged() {
    // bfrun's convention: a read that hits EOF doesn't touch the cell. The
    // untouched cell is 0, +1 makes 1 — so empty stdin prints \x01, and a
    // real byte round-trips.
    run("eof-unchanged", ",+.", "", "\x01");
    run("eof-byte", ",.", "AB", "A");
}

#[test]
fn pointer_walks_past_the_interpreters_initial_tape() {
    // 40,000 cells out is past bfrun's initial 30,000; bfrun grows its tape
    // there, the native backend's 16 MiB allocation just has it. Same
    // result either way.
    run("big-move", &format!("{}+.", ">".repeat(40_000)), "", "\x01");
}

#[test]
fn nested_loops_and_the_zero_idiom() {
    // Multiply 6 by 6 with a copy loop, then bump by one. The first loop
    // zeroes cell 0 while accumulating 36 in cell 1; the second moves those
    // 36 back, so the printed cell is 36 + 1 = 37. This exercises Zero-less
    // nested brackets, folded moves and adds — the whole optimizer surface.
    // (Expected value verified against bfrun.)
    let source = "++++++[>++++++<-]>[<+>-]<+.";
    run("nested-loops", source, "", "\u{25}");
}

/// Compiles `source` with the bfnative binary, runs the result with `stdin`
/// piped in, and asserts stdout matches `expected`.
fn run(tag: &str, source: &str, stdin: &str, expected: &str) {
    // Compile-only environments (no C toolchain) can't run this; skip rather
    // than fail, since the presence of cc isn't what's under test.
    if Command::new("cc").arg("--version").output().is_err() {
        eprintln!("skipping: no cc on PATH");
        return;
    }

    let dir = std::env::temp_dir().join(format!("bfnative-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = dir.join("program.bf");
    let binary_path = dir.join("program");
    fs::write(&source_path, source).expect("writing test program");

    let compiled = Command::new(env!("CARGO_BIN_EXE_bfnative"))
        .arg(&source_path)
        .arg("-o")
        .arg(&binary_path)
        .output()
        .expect("running bfnative");
    assert!(
        compiled.status.success(),
        "bfnative failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut child = Command::new(&binary_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("running compiled program");
    child
        .stdin
        .take()
        .expect("piping stdin")
        .write_all(stdin.as_bytes())
        .expect("writing stdin");
    let output = child.wait_with_output().expect("waiting for program");

    assert_eq!(
        output.stdout,
        expected.as_bytes(),
        "stdout mismatch for {tag}"
    );
    assert!(
        output.status.success(),
        "program {tag} exited with {}",
        output.status
    );

    fs::remove_dir_all(&dir).ok();
}
