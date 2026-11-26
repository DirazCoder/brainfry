//! End-to-end tests: drive the `bf2c` binary, check the CLI surface
//! always (usage, failures, default output path), and — when a C
//! compiler exists on PATH — compile the generated `.c` and run it,
//! pinning stdout, stderr, and exit status byte-for-byte. The compiler
//! is a soft dependency exactly like the differential harness's
//! `wasmtime` lane: hosts without a C toolchain skip the execution
//! tests (with a note) instead of failing.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const HELLO_WORLD: &str = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.\
                           +++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bf2c-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    dir
}

fn write_source(dir: &Path, source: &str) -> PathBuf {
    let source_path = dir.join("program.bf");
    fs::write(&source_path, source).expect("writing test program");
    source_path
}

fn run_args(args: &[&Path]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bf2c"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bf2c");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn compile_program(dir: &Path, source: &str) -> PathBuf {
    let source_path = write_source(dir, source);
    let output_path = dir.join("program.c");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("-o"), &output_path]);
    assert_eq!(status.code(), Some(0), "bf2c failed: {stderr}");
    output_path
}

/// The first C compiler found on PATH, or `None`. Checked once per test
/// via `have_cc`, which compiles a trivial program as the probe —
/// `which cc` alone can't tell a shim from a working toolchain.
fn find_cc() -> Option<&'static str> {
    for compiler in ["cc", "gcc", "clang"] {
        let probe_dir = temp_dir(&format!("probe-{compiler}"));
        let probe = probe_dir.join("probe.c");
        fs::write(&probe, "int main(void) { return 0; }\n").expect("writing probe");
        let ok = Command::new(compiler)
            .arg("-o")
            .arg(probe_dir.join("probe"))
            .arg(&probe)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(compiler);
        }
    }
    None
}

fn have_cc() -> bool {
    find_cc().is_some()
}

/// Transpiles `source` to C, compiles it, runs it with the given stdin,
/// and returns (status, stdout, stderr). Panics if the compile step
/// fails, so a broken generated file is loud, not a skipped assertion.
fn compile_and_run(
    dir: &Path,
    source: &str,
    stdin: &[u8],
) -> (std::process::ExitStatus, Vec<u8>, Vec<u8>) {
    let cc = find_cc().expect("C compiler required by this test");
    let c_path = compile_program(dir, source);
    let exe = dir.join("program");

    let compile = Command::new(cc)
        .arg("-O2")
        .arg("-o")
        .arg(&exe)
        .arg(&c_path)
        .output()
        .expect("running C compiler");
    assert!(
        compile.status.success(),
        "C compile failed: {}\n{}",
        String::from_utf8_lossy(&compile.stderr),
        fs::read_to_string(&c_path).unwrap_or_default()
    );

    let mut child = Command::new(&exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running compiled program");
    if let Some(pipe) = child.stdin.as_mut() {
        pipe.write_all(stdin).expect("feeding program stdin");
    }
    let out = child.wait_with_output().expect("compiled program run");
    (out.status, out.stdout, out.stderr)
}

// ---- CLI surface (always runs) -------------------------------------------

#[test]
fn help_flag_prints_usage_and_succeeds() {
    let dir = temp_dir("help");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_args(&[&source_path, Path::new("--help")]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("usage: bf2c"), "{stdout}");
}

#[test]
fn no_arguments_prints_usage() {
    let (status, _, stderr) = run_args(&[]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("usage: bf2c"), "{stderr}");
}

#[test]
fn unknown_option_fails() {
    let dir = temp_dir("unknown-opt");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("--llvm")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("unknown option: --llvm"), "{stderr}");
}

#[test]
fn output_flag_without_a_value_fails() {
    let dir = temp_dir("no-value");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("-o")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("--output needs a value"), "{stderr}");
}

#[test]
fn more_than_one_input_file_is_rejected() {
    let dir = temp_dir("two-inputs");
    let a = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(&[&a, &a]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("more than one input file"), "{stderr}");
}

#[test]
fn unreadable_input_fails_cleanly() {
    let (status, _, stderr) = run_args(&[Path::new("/nonexistent/bf2c-e2e/nope.bf")]);
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
fn output_in_a_missing_directory_fails_cleanly() {
    let dir = temp_dir("bad-output");
    let source_path = write_source(&dir, "+.");
    let bad_output = dir.join("no-such-dir").join("program.c");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("-o"), &bad_output]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
}

#[test]
fn compiles_a_well_formed_c_file() {
    let dir = temp_dir("success");
    let source_path = write_source(&dir, HELLO_WORLD);
    let output_path = dir.join("program.c");
    let (status, stdout, stderr) = run_args(&[&source_path, Path::new("-o"), &output_path]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("compiled"), "{stdout}");
    let c = fs::read_to_string(&output_path).expect("reading generated c");
    assert!(c.starts_with("/* Generated by bf2c"));
    assert!(c.contains("program.bf"));
    assert!(c.contains("while (*p != 0) {"));
}

#[test]
fn default_output_path_is_input_with_c_extension() {
    let dir = temp_dir("default-out");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_args(&[&source_path]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("program.c"), "{stdout}");
    assert!(dir.join("program.c").exists());
}

// ---- execution (compiler-gated) -------------------------------------------

#[test]
fn hello_world_runs_byte_exact() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-hello");
    let (status, stdout, stderr) = compile_and_run(&dir, HELLO_WORLD, b"");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, b"Hello World!\n");
    assert_eq!(stderr, b"");
}

#[test]
fn echo_program_reads_stdin() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-echo");
    let (status, stdout, stderr) = compile_and_run(&dir, ",[.[-],]", b"echo, echo, echo\n");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, b"echo, echo, echo\n");
    assert_eq!(stderr, b"");
}

#[test]
fn eof_leaves_the_cell_untouched() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-eof");
    let (status, stdout, stderr) = compile_and_run(&dir, ",+.", b"");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![1u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn wraparound_is_byte_exact() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-wrap");
    let source = format!("{}{}", "+".repeat(256), ".-.");
    let (status, stdout, stderr) = compile_and_run(&dir, &source, b"");
    assert_eq!(status.code(), Some(0));
    // 256 increments wrap to 0; 0-1 wraps to 255.
    assert_eq!(stdout, vec![0u8, 255u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn tape_grows_past_30k_cells() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let source = ">".repeat(40_000) + "+.";
    let dir = temp_dir("run-grow");
    let (status, stdout, stderr) = compile_and_run(&dir, &source, b"");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![1u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn tape_grows_to_200k_cells() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    // Crosses several doubling boundaries of the realloc growth path.
    let source = ">".repeat(200_000) + "+.";
    let dir = temp_dir("run-grow-200k");
    let (status, stdout, stderr) = compile_and_run(&dir, &source, b"");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![1u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn underflow_is_a_clean_error() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-underflow");
    let (status, stdout, stderr) = compile_and_run(&dir, "+[<]", b"");
    assert_eq!(status.code(), Some(1));
    assert_eq!(stdout, b"");
    assert_eq!(
        stderr, b"runtime error: pointer moved left of cell 0\n",
        "error wording must match every other backend byte-for-byte"
    );
}

#[test]
fn huge_left_move_is_a_clean_error() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    // Left move far past cell 0: must be the underflow error, not a wrap.
    let source = ">".repeat(10) + &"<".repeat(100_000);
    let dir = temp_dir("run-huge-left");
    let (status, _stdout, stderr) = compile_and_run(&dir, &source, b"");
    assert_eq!(status.code(), Some(1));
    assert_eq!(stderr, b"runtime error: pointer moved left of cell 0\n");
}

#[test]
fn folded_ops_execute_correctly() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-folded");

    // Multiplication loop folds to MulAdd: 3*3 = 9 in cell 2.
    let (status, stdout, stderr) = compile_and_run(&dir, "+++[>+++[>+<-]<-]>>.", b"");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![9u8]);
    assert_eq!(stderr, b"");

    // Scan: walk right past non-zero cells, land on a zero.
    let (status, stdout, stderr) = compile_and_run(&dir, "+>+>+<<[>]+.", b"");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![1u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn deep_nesting_computes_correctly() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-nesting");
    let (status, stdout, stderr) = compile_and_run(
        &dir,
        "++++++[>+++++[>++++[>+++[>++[>++[>+<-]<-]<-]<-]<-]<-]>>>>>>.",
        b"",
    );
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![(6u32 * 5 * 4 * 3 * 2 * 2) as u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn mandelbrot_runs_byte_exact() {
    if !have_cc() {
        eprintln!("note: no C compiler found — skipping execution test");
        return;
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mandelbrot = root.join("tests/mandelbrot.b");
    if !mandelbrot.exists() {
        eprintln!("note: tests/mandelbrot.b not found — skipping");
        return;
    }
    let expected = fs::read(root.join("tests/mandelbrot.expected")).expect("expected output");

    let dir = temp_dir("run-mandelbrot");
    let output_path = dir.join("mandelbrot.c");
    let (status, _, stderr) = run_args(&[&mandelbrot, Path::new("-o"), &output_path]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");

    let cc = find_cc().expect("C compiler required by this test");
    let exe = dir.join("mandelbrot");
    let compile = Command::new(cc)
        .arg("-O2")
        .arg("-o")
        .arg(&exe)
        .arg(&output_path)
        .output()
        .expect("running C compiler");
    assert!(
        compile.status.success(),
        "C compile failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let out = Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("mandelbrot run");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        out.stdout, expected,
        "mandelbrot output drifted from expected"
    );
    assert_eq!(out.stderr, b"");
}
