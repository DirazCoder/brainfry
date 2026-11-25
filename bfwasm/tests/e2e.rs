//! End-to-end tests: drive the `bfwasm` binary and check stdout, stderr,
//! exit status, and the `.wasm` artifact it leaves behind. The codegen has
//! its own unit tests; these pin the CLI surface -- usage, read/parse/write
//! failures, the default output path, and a well-formed module header -- so
//! the binary can't regress independently of the library.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const HELLO_WORLD: &str = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.\
                           +++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bfwasm-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    dir
}

fn write_source(dir: &Path, source: &str) -> std::path::PathBuf {
    let source_path = dir.join("program.bf");
    fs::write(&source_path, source).expect("writing test program");
    source_path
}

fn run_args(_tag: &str, args: &[&Path]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bfwasm"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfwasm");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn help_flag_prints_usage_and_succeeds() {
    let dir = temp_dir("help");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_args("help", &[&source_path, Path::new("--help")]);
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("usage: bfwasm"), "{stdout}");
}

#[test]
fn no_arguments_prints_usage() {
    let (status, _, stderr) = run_args("no-args", &[]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("usage: bfwasm"), "{stderr}");
}

#[test]
fn unknown_option_fails() {
    let dir = temp_dir("unknown-opt");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args("unknown-opt", &[&source_path, Path::new("--wat")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("unknown option: --wat"), "{stderr}");
}

#[test]
fn output_flag_without_a_value_fails() {
    let dir = temp_dir("no-value");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args("no-value", &[&source_path, Path::new("-o")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("--output needs a value"), "{stderr}");
}

#[test]
fn more_than_one_input_file_is_rejected() {
    let dir = temp_dir("two-inputs");
    let a = write_source(&dir, "+.");
    let (status, _, stderr) = run_args("two-inputs", &[&a, &a]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("more than one input file"), "{stderr}");
}

#[test]
fn unreadable_input_fails_cleanly() {
    let (status, _, stderr) = run_args(
        "missing-input",
        &[Path::new("/nonexistent/bfwasm-e2e/nope.bf")],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't read"), "{stderr}");
}

#[test]
fn parse_error_names_the_line() {
    let dir = temp_dir("parse-error");
    let source_path = write_source(&dir, "+\n[");
    let (status, _, stderr) = run_args("parse-error", &[&source_path]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("program.bf:2:"), "{stderr}");
    assert!(stderr.contains("unmatched '['"), "{stderr}");
}

#[test]
fn output_in_a_missing_directory_fails_cleanly() {
    let dir = temp_dir("bad-output");
    let source_path = write_source(&dir, "+.");
    let bad_output = dir.join("no-such-dir").join("program.wasm");
    let (status, _, stderr) =
        run_args("bad-output", &[&source_path, Path::new("-o"), &bad_output]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
}

#[test]
fn compiles_a_well_formed_module() {
    let dir = temp_dir("success");
    let source_path = write_source(&dir, HELLO_WORLD);
    let output_path = dir.join("program.wasm");
    let (status, stdout, stderr) =
        run_args("success", &[&source_path, Path::new("-o"), &output_path]);
    assert!(status.success(), "stderr: {stderr}");
    assert!(stdout.contains("compiled"), "{stdout}");
    assert!(stdout.contains("program.wasm"), "{stdout}");

    // A standalone wasm module: magic + version header, always first.
    let bytes = fs::read(&output_path).expect("reading compiled module");
    assert_eq!(&bytes[..4], b"\0asm");
    assert_eq!(&bytes[4..8], &[1, 0, 0, 0]);
}

#[test]
fn default_output_path_replaces_the_bf_extension() {
    let dir = temp_dir("default-output");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args("default-output", &[&source_path]);
    assert!(status.success(), "stderr: {stderr}");
    let default_output = dir.join("program.wasm");
    assert!(default_output.exists());
    let bytes = fs::read(&default_output).expect("reading default output");
    assert_eq!(&bytes[..4], b"\0asm");
}
