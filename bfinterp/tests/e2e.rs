//! End-to-end tests: run real programs through the `bfinterp` binary and
//! check stdout / stderr / exit status.
//!
//! Expected outputs are hardcoded (mirroring `bfnative`'s e2e suite, which
//! also pins expected bytes rather than consulting another tool at test
//! time). Every expectation here was cross-checked against `bfrun` first:
//! the four-way differential harness in `scripts/differential_test_all.py`
//! is the authority for cross-tool agreement; these tests pin the
//! interpreter's own behavior so regressions are caught without needing the
//! other binaries present.
//!
//! The mandelbrot test reads `../tests/mandelbrot.b` when present and
//! compares against the stored expected output byte-for-byte — the one
//! real-workload case.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const HELLO_WORLD: &str = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.\
                           +++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";

#[test]
fn hello_world_matches_expected_output() {
    run("hello-world", HELLO_WORLD, "", "Hello World!\n");
}

#[test]
fn cells_wrap_at_256() {
    // 256 increments on one cell wrap to 0. The naive path executes 256
    // separate Add(1) ops; the optimizer would fold them into Add(255) +
    // Add(1) — same result.
    run("wrap", &format!("{}.", "+".repeat(256)), "", "\x00");
}

#[test]
fn eof_leaves_the_cell_unchanged() {
    // Empty stdin: cell stays 0, +1 makes 1. A real byte round-trips.
    run("eof-unchanged", ",+.", "", "\x01");
    run("eof-byte", ",.", "AB", "A");
}

#[test]
fn echo_stdin_until_eof() {
    run("echo", ",[.[-],]", "echo, echo, echo\n", "echo, echo, echo\n");
}

#[test]
fn pointer_walks_past_the_initial_tape() {
    // 40,000 cells out is past the initial 30,000 — the tape must grow.
    run("big-move", &format!("{}+.", ">".repeat(40_000)), "", "\x01");
}

#[test]
fn tape_growth_beyond_the_first_commit_granule() {
    // 200,000 cells — far past 30,000 (and past the native backend's first
    // 64 KiB commit; here it's plain Vec growth).
    run("deep-tape", &format!("{}+.", ">".repeat(200_000)), "", "\x01");
}

#[test]
fn deeply_nested_loops_six_levels() {
    // 6*5*4*3*2*2 = 1440; 1440 mod 256 = 160. Verified against bfrun.
    let source = "++++++[>+++++[>++++[>+++[>++[>++[>+<-]<-]<-]<-]<-]<-]>>>>>>.";
    run_bytes("nested-six", source, "", &[160]);
}

#[test]
fn nested_loops_and_the_zero_idiom() {
    // 6 * 6 = 36, +1 = 37 = '%'. Verified against bfrun.
    let source = "++++++[>++++++<-]>[<+>-]<+.";
    run("nested-loops", source, "", "\u{25}");
}

#[test]
fn zero_loop_idiom_as_a_real_loop() {
    // The naive path runs [-] as an actual decrement loop (up to 255
    // iterations) instead of the optimizer's Zero op — same end state.
    run("zero-loop", "++++++++++[-]+.", "", "\x01");
    run("zero-loop-inc", "++[+++[-]+[-]]++.", "", "\x02");
}

#[test]
fn io_inside_loops() {
    run("io-in-loop", ",[.[-],]", "hi!", "hi!");
}

#[test]
fn pointer_underflow_is_a_clean_error() {
    // bfrun's exact message and exit code, not a panic.
    let (code, stdout, stderr) = run_raw("underflow", "<", "", &[]);
    assert_eq!(stdout, b"");
    assert_eq!(code.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&stderr),
        "runtime error: pointer moved left of cell 0\n"
    );
}

#[test]
fn huge_left_move_is_caught_not_wrapped() {
    let source = format!("{}{}", ">".repeat(10), "<".repeat(1_000_000));
    let (code, _, stderr) = run_raw("huge-left", &source, "", &[]);
    assert_eq!(code.code(), Some(1));
    assert!(String::from_utf8_lossy(&stderr).contains("left of cell 0"));
}

#[test]
fn optimize_flag_agrees_with_the_naive_path() {
    // Same program, both paths, identical bytes — the flag is a comparison
    // aid, not a behavior change.
    for (tag, source, stdin) in [
        ("hello", HELLO_WORLD, ""),
        ("loops", "++++++[>+++++[>++++[>+++[>++[>++[>+<-]<-]<-]<-]<-]>>>>>>.", ""),
        ("wrap", &"+".repeat(256), ""),
        ("echo", ",[.[-],]", "roundtrip"),
    ] {
        let naive = run_raw(tag, source, stdin, &[]);
        let optimized = run_raw_optimized(tag, source, stdin, &[]);
        assert_eq!(naive.0.code(), optimized.0.code(), "{tag}: exit codes differ");
        assert_eq!(naive.1, optimized.1, "{tag}: stdout differs");
        assert_eq!(naive.2, optimized.2, "{tag}: stderr differs");
    }
}

#[test]
fn bad_brackets_fail_cleanly() {
    let (code, _, stderr) = run_raw("unmatched", "[+", "", &[]);
    assert_eq!(code.code(), Some(1));
    assert!(String::from_utf8_lossy(&stderr).contains("unmatched '['"));
}

#[test]
fn mandelbrot_matches_the_stored_reference_output() {
    // The real-workload case: Erik Bosman's mandelbrot.b. Skipped when the
    // asset isn't checked out alongside the repo.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let source_path = root.join("tests").join("mandelbrot.b");
    let expected_path = root.join("tests").join("mandelbrot.expected");
    let Ok(source) = fs::read_to_string(&source_path) else {
        eprintln!("skipping: {} not present", source_path.display());
        return;
    };
    let Ok(expected) = fs::read(&expected_path) else {
        eprintln!("skipping: {} not present", expected_path.display());
        return;
    };
    run_bytes("mandelbrot", &source, "", &expected);
}

/// Runs `source` through the bfinterp binary and asserts stdout + success.
fn run(tag: &str, source: &str, stdin: &str, expected: &str) {
    run_bytes(tag, source, stdin, expected.as_bytes());
}

fn run_bytes(tag: &str, source: &str, stdin: &str, expected: &[u8]) {
    let (code, stdout, stderr) = run_raw(tag, source, stdin, &[]);
    assert_eq!(
        stdout,
        expected,
        "stdout mismatch for {tag}; stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(
        code.success(),
        "program {tag} exited with {code}; stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
}

fn run_raw(
    tag: &str,
    source: &str,
    stdin: &str,
    _unused: &[u8],
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    run_with_args(tag, source, stdin, &[])
}

fn run_raw_optimized(
    tag: &str,
    source: &str,
    stdin: &str,
    _unused: &[u8],
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    run_with_args(tag, source, stdin, &["--optimize"])
}

fn run_with_args(
    tag: &str,
    source: &str,
    stdin: &str,
    extra_args: &[&str],
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("bfinterp-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = dir.join("program.bf");
    fs::write(&source_path, source).expect("writing test program");

    let mut command = Command::new(env!("CARGO_BIN_EXE_bfinterp"));
    command.arg(&source_path).args(extra_args);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running bfinterp");
    child
        .stdin
        .take()
        .expect("piping stdin")
        .write_all(stdin.as_bytes())
        .expect("writing stdin");
    let output = child.wait_with_output().expect("waiting for program");
    (output.status, output.stdout, output.stderr)
}
