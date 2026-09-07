//! End-to-end execution tests for the JIT: compile real programs, execute
//! the generated machine code in-process, and check stdout / stderr / exit
//! status. A JIT is defined by "the process didn't crash and produced
//! correct output", so every test here actually runs generated code —
//! static inspection of the emitted bytes is the unit tests' job.
//!
//! Runs on any Linux host (x86_64 or aarch64) since the JIT always targets
//! the host it runs on. The aarch64 variant can be exercised from an
//! x86_64 host under QEMU by setting BFJIT_AARCH64 (path to an
//! aarch64-unknown-linux-gnu build of this binary) and QEMU_AARCH64 (path
//! to qemu-aarch64-static) — mirroring the bfnative differential harness's
//! QEMU_AARCH64 convention.
//!
//! Expected outputs are hardcoded, cross-checked against `bfrun` first
//! (same policy as bfnative's e2e suite). The four-way differential
//! harness in `scripts/differential_test_all.py` is the authority for
//! cross-tool agreement.

#![cfg(target_os = "linux")]

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
    // 256 increments fold to Add(255) + Add(1) and wrap to 0; the 8-bit ALU
    // op in the generated code wraps for free.
    run("wrap", &format!("{}.", "+".repeat(256)), "", "\x00");
}

#[test]
fn negative_cells_wrap_to_255() {
    // Raw byte 255, not the UTF-8 encoding of U+00FF — Brainfuck output is
    // bytes, so the expectation is pinned with run_bytes.
    run_bytes("wrap-negative", "-.", "", &[255]);
}

#[test]
fn eof_leaves_the_cell_unchanged() {
    run("eof-unchanged", ",+.", "", "\x01");
    run("eof-byte", ",.", "AB", "A");
}

#[test]
fn echo_stdin_until_eof() {
    run("echo", ",[.[-],]", "echo, echo, echo\n", "echo, echo, echo\n");
}

#[test]
fn pointer_walks_past_the_initial_tape() {
    // 40,000 cells out is past the initial 30,000 — the generated grow
    // routine must commit more pages of the tape reservation from JITed
    // code (mprotect through raw syscalls on Linux).
    run("big-move", &format!("{}+.", ">".repeat(40_000)), "", "\x01");
}

#[test]
fn tape_growth_through_several_commit_granules() {
    // 200,000 cells forces repeated growth (30000 -> 65536 -> 131072 ->
    // 262144) and lands past the first 64 KiB commit exactly.
    run("deep-tape", &format!("{}+.", ">".repeat(200_000)), "", "\x01");
}

#[test]
fn tape_write_then_read_back_across_a_commit_boundary() {
    // Write 3 at cell 65,600 (across the first 64 KiB commit), walk back to
    // cell 0, print it: 0. The value printed isn't the point — surviving
    // the commit boundary and keeping the tape intact is (bfrun agrees on
    // the output byte).
    run_bytes(
        "tape-rw",
        &format!("{}+++{}.", ">".repeat(65_600), "<".repeat(65_600)),
        "",
        &[0],
    );
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
fn zero_loop_folding_and_runs() {
    run("zero-loop", "++++++++++[-]+.", "", "\x01");
    run("zero-loop-inc", "++[+++[-]+[-]]++.", "", "\x02");
}

#[test]
fn io_inside_loops() {
    run("io-in-loop", ",[.[-],]", "hi!", "hi!");
}

#[test]
fn pointer_underflow_is_a_clean_error() {
    // bfrun's exact message and exit code, emitted by the generated error
    // path — not a crash.
    let (code, stdout, stderr) = run_raw("underflow", "<", "");
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
    let (code, _, stderr) = run_raw("huge-left", &source, "");
    assert_eq!(code.code(), Some(1));
    assert!(String::from_utf8_lossy(&stderr).contains("left of cell 0"));
}

#[test]
fn unmatched_brackets_fail_before_jitting() {
    let (code, _, stderr) = run_raw("unmatched", "[+", "");
    assert_eq!(code.code(), Some(1));
    assert!(String::from_utf8_lossy(&stderr).contains("unmatched '['"));
}

#[test]
fn cross_target_request_is_rejected() {
    // bfjit is host-only; --target must fail clearly, not JIT for a
    // foreign architecture.
    let source_path = write_source("target-reject", HELLO_WORLD);
    let out = Command::new(env!("CARGO_BIN_EXE_bfjit"))
        .arg("--target")
        .arg("linux-aarch64")
        .arg(&source_path)
        .output()
        .expect("running bfjit");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("host"), "stderr should say host-only: {stderr}");
}

#[test]
fn mandelbrot_matches_the_stored_reference_output() {
    // The real-workload case: Erik Bosman's mandelbrot.b — the same
    // differential workload the native-vs-bytecode benchmark used. Skipped
    // when the asset isn't checked out alongside the repo.
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

// ---------------------------------------------------------------------------
// aarch64 under QEMU (opt-in, mirrors the bfnative harness convention)
// ---------------------------------------------------------------------------

/// Runs the aarch64 bfjit build under qemu-aarch64-static when both env
/// vars point at real files. The generated aarch64 machine code executes
/// under emulation, which is still a real execution of the JIT pipeline
/// (map, patch, W^X flip, enter) — just not on real hardware.
#[test]
fn aarch64_under_qemu_runs_hello_world() {
    let (qemu, binary) = match emulator_setup() {
        Some(setup) => setup,
        None => {
            eprintln!(
                "skipping: set QEMU_AARCH64=<emulator> and BFJIT_AARCH64=<aarch64 bfjit> \
                 to enable"
            );
            return;
        }
    };

    let dir = std::env::temp_dir().join("bfjit-e2e-qemu");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = dir.join("program.bf");
    fs::write(&source_path, HELLO_WORLD).expect("writing test program");

    let out = Command::new(qemu)
        .arg(binary)
        .arg(&source_path)
        .stdin(Stdio::null())
        .output()
        .expect("running aarch64 bfjit under qemu");
    assert!(
        out.status.success(),
        "qemu run failed; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Hello World!\n");
}

/// Also under QEMU: the underflow error path (proves the generated error
/// tail and exit sequence work on aarch64 too).
#[test]
fn aarch64_under_qemu_reports_underflow_cleanly() {
    let (qemu, binary) = match emulator_setup() {
        Some(setup) => setup,
        None => {
            eprintln!("skipping: set QEMU_AARCH64 and BFJIT_AARCH64 to enable");
            return;
        }
    };

    let dir = std::env::temp_dir().join("bfjit-e2e-qemu-err");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = dir.join("program.bf");
    fs::write(&source_path, "<").expect("writing test program");

    let out = Command::new(qemu)
        .arg(binary)
        .arg(&source_path)
        .stdin(Stdio::null())
        .output()
        .expect("running aarch64 bfjit under qemu");
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "runtime error: pointer moved left of cell 0\n"
    );
}

fn emulator_setup() -> Option<(String, String)> {
    let qemu = std::env::var("QEMU_AARCH64").ok()?;
    let binary = std::env::var("BFJIT_AARCH64").ok()?;
    if qemu.is_empty() || binary.is_empty() {
        return None;
    }
    if !std::path::Path::new(&binary).exists() {
        eprintln!("skipping: {binary} does not exist");
        return None;
    }
    Some((qemu, binary))
}

// ---------------------------------------------------------------------------

fn run(tag: &str, source: &str, stdin: &str, expected: &str) {
    run_bytes(tag, source, stdin, expected.as_bytes());
}

fn run_bytes(tag: &str, source: &str, stdin: &str, expected: &[u8]) {
    let (code, stdout, stderr) = run_raw(tag, source, stdin);
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

fn run_raw(tag: &str, source: &str, stdin: &str) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("bfjit-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(tag, source);

    let mut child = Command::new(env!("CARGO_BIN_EXE_bfjit"))
        .arg(&source_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running bfjit");
    child
        .stdin
        .take()
        .expect("piping stdin")
        .write_all(stdin.as_bytes())
        .expect("writing stdin");
    let output = child.wait_with_output().expect("waiting for program");
    (output.status, output.stdout, output.stderr)
}

fn write_source(tag: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bfjit-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = dir.join("program.bf");
    fs::write(&source_path, source).expect("writing test program");
    source_path
}
