//! End-to-end tests: drive the `bfjs` binary, check the CLI surface
//! always (usage, failures, default output path), and — when a `node`
//! exists on PATH — actually execute the generated `.js` files and pin
//! their stdout, stderr, and exit status byte-for-byte. Node is a soft
//! dependency exactly like the differential harness's `wasmtime` lane:
//! the codegen has its own unit tests, so these execution tests skip
//! (with a note) rather than fail on hosts with no JS engine to run.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const HELLO_WORLD: &str = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.\
                           +++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bfjs-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    dir
}

fn write_source(dir: &Path, source: &str) -> PathBuf {
    let source_path = dir.join("program.bf");
    fs::write(&source_path, source).expect("writing test program");
    source_path
}

fn run_args(args: &[&Path]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bfjs"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfjs");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn compile_program(dir: &Path, source: &str) -> PathBuf {
    let source_path = write_source(dir, source);
    let output_path = dir.join("program.js");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("-o"), &output_path]);
    assert_eq!(status.code(), Some(0), "bfjs failed: {stderr}");
    output_path
}

/// Runs `node <js>` with the given stdin bytes; `None` when the spawn
/// fails (node not on PATH). The generated `readIn` reads fd 0 via
/// `fs.readSync`, so a piped stdin is exactly what it consumes — empty
/// included, where the closed pipe is immediate EOF.
fn run_node(js: &Path, stdin: &[u8]) -> Option<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let mut child = Command::new("node")
        .arg(js)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    if let Some(pipe) = child.stdin.as_mut() {
        pipe.write_all(stdin).expect("feeding program stdin to node");
    }
    let out = child.wait_with_output().expect("node run");
    Some((out.status, out.stdout, out.stderr))
}

fn have_node() -> bool {
    Command::new("node")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---- CLI surface (always runs) -------------------------------------------

#[test]
fn help_flag_prints_usage_and_succeeds() {
    let dir = temp_dir("help");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_args(&[&source_path, Path::new("--help")]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("usage: bfjs"), "{stdout}");
}

#[test]
fn no_arguments_prints_usage() {
    let (status, _, stderr) = run_args(&[]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("usage: bfjs"), "{stderr}");
}

#[test]
fn unknown_option_fails() {
    let dir = temp_dir("unknown-opt");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("--vm")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("unknown option: --vm"), "{stderr}");
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
    let (status, _, stderr) = run_args(&[Path::new("/nonexistent/bfjs-e2e/nope.bf")]);
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
    let bad_output = dir.join("no-such-dir").join("program.js");
    let (status, _, stderr) = run_args(&[&source_path, Path::new("-o"), &bad_output]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
}

#[test]
fn compiles_a_well_formed_js_file() {
    let dir = temp_dir("success");
    let source_path = write_source(&dir, HELLO_WORLD);
    let output_path = dir.join("program.js");
    let (status, stdout, stderr) = run_args(&[&source_path, Path::new("-o"), &output_path]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("compiled"), "{stdout}");
    let js = fs::read_to_string(&output_path).expect("reading generated js");
    assert!(js.starts_with("#!/usr/bin/env node\n"));
    assert!(js.contains("program.bf"));
    assert!(js.contains("while (tape[ptr] !== 0) {"));
}

#[test]
fn default_output_path_is_input_with_js_extension() {
    let dir = temp_dir("default-out");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_args(&[&source_path]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("program.js"), "{stdout}");
    assert!(dir.join("program.js").exists());
}

// ---- execution (node-gated) ----------------------------------------------

#[test]
fn hello_world_runs_byte_exact_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-hello");
    let output_path = compile_program(&dir, HELLO_WORLD);

    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, b"Hello World!\n");
    assert_eq!(stderr, b"");
}

#[test]
fn echo_program_reads_stdin_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-echo");
    let output_path = compile_program(&dir, ",[.[-],]");

    let (status, stdout, stderr) = run_node(&output_path, b"echo, echo, echo\n").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, b"echo, echo, echo\n");
    assert_eq!(stderr, b"");
}

#[test]
fn eof_leaves_the_cell_untouched_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
        return;
    }
    // Empty stdin: `,` leaves the cell at 0, `+1` makes 1.
    let dir = temp_dir("run-eof");
    let output_path = compile_program(&dir, ",+.");

    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![1u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn wraparound_is_byte_exact_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-wrap");
    let output_path = compile_program(&dir, &format!("{}{}", "+".repeat(256), ".-."));

    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    // 256 increments wrap to 0; 0-1 wraps to 255.
    assert_eq!(stdout, vec![0u8, 255u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn tape_grows_past_30k_cells_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
        return;
    }
    let source = ">".repeat(40_000) + "+.";
    let dir = temp_dir("run-grow");
    let output_path = compile_program(&dir, &source);

    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![1u8]);
    assert_eq!(stderr, b"");
}

#[test]
fn underflow_is_a_clean_error_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-underflow");
    let output_path = compile_program(&dir, "+[<]");

    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(1));
    assert_eq!(stdout, b"");
    assert_eq!(
        stderr,
        b"runtime error: pointer moved left of cell 0\n",
        "error wording must match every other backend byte-for-byte"
    );
}

#[test]
fn folded_ops_execute_correctly_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
        return;
    }
    let dir = temp_dir("run-folded");

    // Multiplication loop folds to MulAdd: 3*3 = 9 in cell 2.
    let output_path = compile_program(&dir, "+++[>+++[>+<-]<-]>>.");
    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![9u8]);
    assert_eq!(stderr, b"");

    // Scan: walk right past a run of non-zero cells, land on a zero.
    let output_path = compile_program(&dir, "+>+>+<<[>]+.");
    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![1u8]);
    assert_eq!(stderr, b"");

    // Zero folding inside a surviving loop.
    let output_path = compile_program(&dir, "+++++[->+[-]<]>");
    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, Vec::<u8>::new());
    assert_eq!(stderr, b"");
}

#[test]
fn mandelbrot_runs_byte_exact_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
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
    let output_path = dir.join("mandelbrot.js");
    let (status, _, stderr) = run_args(&[&mandelbrot, Path::new("-o"), &output_path]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");

    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, expected, "mandelbrot output drifted from expected");
    assert_eq!(stderr, b"");
}

#[test]
fn big_programs_run_chunked_and_stay_byte_exact_under_node() {
    if !have_node() {
        eprintln!("note: node not found — skipping execution test");
        return;
    }
    // Alternating `>+` never folds, so this is a real 2202-op program:
    // past the chunking threshold, so the file is emitted as small
    // functions instead of one giant body — exactly the shape mandelbrot
    // gets. It must still execute byte-exact.
    let dir = temp_dir("run-chunked");
    let source = format!("{}{}{}", ">+".repeat(1100), "<".repeat(1100), "+.");
    let output_path = compile_program(&dir, &source);
    let js = fs::read_to_string(&output_path).expect("reading generated js");
    assert!(js.contains("function S"), "program should be chunked: {js}");

    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![1u8]);
    assert_eq!(stderr, b"");

    // Chunked straight runs, a folded big move, and a zero loop.
    let source = format!("{}{}{}", ">+".repeat(1500), "<".repeat(1500), "[-].");
    let output_path = compile_program(&dir, &source);
    let (status, stdout, stderr) = run_node(&output_path, b"").expect("node run");
    assert_eq!(status.code(), Some(0));
    assert_eq!(stdout, vec![0u8]); // the cleared cell
    assert_eq!(stderr, b"");
}
