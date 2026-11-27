//! End-to-end tests: drive the `bfc` binary itself and check stdout, stderr,
//! exit status, and the artifact it leaves behind. The parser and optimizer
//! passes have their own unit tests; these pin the CLI surface -- usage
//! errors, read/parse/write failures, the default output path, and the
//! success message -- so the binary can't regress independently of the
//! library.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

const HELLO_WORLD: &str = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.\
                           +++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bfc-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    dir
}

fn write_source(dir: &Path, source: &str) -> std::path::PathBuf {
    let source_path = dir.join("program.bf");
    fs::write(&source_path, source).expect("writing test program");
    source_path
}

/// Runs `bfc` with the given raw args (no implicit input path).
fn run_args(_tag: &str, args: &[&Path]) -> (std::process::ExitStatus, String, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bfc"));
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command.output().expect("running bfc");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn compile(
    tag: &str,
    source: &str,
) -> (std::process::ExitStatus, String, String, std::path::PathBuf) {
    let dir = temp_dir(tag);
    let source_path = write_source(&dir, source);
    let output_path = dir.join("program.bfry");
    let (status, stdout, stderr) = run_args(tag, &[&source_path, &output_path]);
    (status, stdout, stderr, output_path)
}

#[test]
fn no_arguments_prints_usage() {
    let (status, _, stderr) = run_args("no-args", &[]);
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr.contains("usage: bfc <input.bf> [output.bfry]"),
        "{stderr}"
    );
}

#[test]
fn unreadable_input_fails_cleanly() {
    let (status, _, stderr) = run_args(
        "missing-input",
        &[Path::new("/nonexistent/bfc-e2e/nope.bf")],
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
    let source_path = write_source(&dir, "+");
    let bad_output = dir.join("no-such-dir").join("program.bfry");
    let (status, _, stderr) = run_args("bad-output", &[&source_path, &bad_output]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't create"), "{stderr}");
}

/// Writing to /dev/full: the file opens fine, then every write reports
/// ENOSPC -- the one portable-to-induce write failure on Linux.
#[cfg(target_os = "linux")]
#[test]
fn write_failure_on_a_full_device_is_clean() {
    let dir = temp_dir("write-error");
    let source_path = write_source(&dir, "+");
    let (status, _, stderr) = run_args("write-error", &[&source_path, Path::new("/dev/full")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write /dev/full"), "{stderr}");
}

#[test]
fn compiles_and_writes_a_loadable_bfry_file() {
    let (status, stdout, stderr, output_path) = compile("success", HELLO_WORLD);
    assert!(
        status.success(),
        "bfc failed with {status}; stderr: {stderr}"
    );
    assert!(stdout.contains("compiled"), "{stdout}");
    assert!(stdout.contains("program.bf ->"), "{stdout}");
    assert!(stdout.contains("instructions"), "{stdout}");

    // The artifact must be a well-formed .bfry: right magic, right version,
    // and a body the loader accepts.
    let bytes = fs::read(&output_path).expect("reading compiled program");
    assert_eq!(&bytes[..4], b"BFRY");
    assert_eq!(bytes[4], bfformat::FORMAT_VERSION);
    let program = bfformat::Program::read_from(&bytes[..]).expect("loading compiled program");
    assert!(!program.ops.is_empty());
}

#[test]
fn default_output_path_replaces_the_bf_extension() {
    // No second argument: output lands next to the source as .bfry.
    let dir = temp_dir("default-output");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_args("default-output", &[&source_path]);
    assert!(
        status.success(),
        "bfc failed with {status}; stderr: {stderr}"
    );
    let default_output = dir.join("program.bfry");
    assert!(default_output.exists(), "default output missing: {stdout}");

    // And with an extension-less input, the default is still .bfry.
    let odd = dir.join("program");
    fs::write(&odd, "+.").expect("writing extension-less source");
    let (status, _, _) = run_args("default-output-2", &[&odd]);
    assert!(status.success());
    assert!(dir.join("program.bfry").exists());
}
