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
fn target_flag_without_a_value_fails() {
    let dir = temp_dir("target-no-value");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args("target-no-value", &[&source_path, Path::new("--target")]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("--target needs a value"), "{stderr}");
    assert!(stderr.contains("usage: bfwasm"), "{stderr}");
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
    let (status, _, stderr) = run_args("bad-output", &[&source_path, Path::new("-o"), &bad_output]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
}

#[test]
fn browser_target_writes_a_self_contained_javascript_file_by_default() {
    let dir = temp_dir("browser-target");
    let source_path = write_source(&dir, HELLO_WORLD);
    let (status, stdout, stderr) = run_args(
        "browser-target",
        &[&source_path, Path::new("--target"), Path::new("browser")],
    );
    assert!(status.success(), "stderr: {stderr}");
    assert!(stdout.contains("program.js"), "{stdout}");
    let generated = fs::read_to_string(dir.join("program.js")).expect("reading browser output");
    assert!(generated.contains("wasi_snapshot_preview1"));
    assert!(generated.contains("WebAssembly.instantiate"));
    assert!(generated.contains("atob(\""));
    assert!(!generated.contains("fetch("));
    assert!(!generated.contains("program.wasm"));
}

#[test]
fn javascript_output_extension_infers_browser_target() {
    let dir = temp_dir("browser-inferred");
    let source_path = write_source(&dir, "+.");
    let output_path = dir.join("inferred.js");
    let (status, stdout, stderr) = run_args(
        "browser-inferred",
        &[&source_path, Path::new("-o"), &output_path],
    );
    assert!(status.success(), "stderr: {stderr}");
    assert!(stdout.contains("inferred.js"), "{stdout}");
    let generated = fs::read_to_string(output_path).expect("reading inferred browser output");
    assert!(generated.contains("wasi_snapshot_preview1"));
}

#[test]
fn explicit_wasi_target_overrides_javascript_extension_inference() {
    let dir = temp_dir("wasi-explicit");
    let source_path = write_source(&dir, "+.");
    let output_path = dir.join("explicit.js");
    let (status, _, stderr) = run_args(
        "wasi-explicit",
        &[
            &source_path,
            Path::new("-o"),
            &output_path,
            Path::new("--target"),
            Path::new("wasi"),
        ],
    );
    assert!(status.success(), "stderr: {stderr}");
    let bytes = fs::read(output_path).expect("reading explicit WASI output");
    assert_eq!(&bytes[..4], b"\0asm");
}

#[test]
fn unknown_target_fails_with_a_usage_error() {
    let dir = temp_dir("unknown-target");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(
        "unknown-target",
        &[&source_path, Path::new("--target"), Path::new("native")],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("unknown target: native"), "{stderr}");
    assert!(stderr.contains("usage: bfwasm"), "{stderr}");
}

#[test]
fn browser_output_write_failure_is_reported() {
    let dir = temp_dir("browser-write-failure");
    let source_path = write_source(&dir, "+.");
    let output_path = dir.join("missing-directory").join("program.js");
    let (status, _, stderr) = run_args(
        "browser-write-failure",
        &[
            &source_path,
            Path::new("--target"),
            Path::new("browser"),
            Path::new("-o"),
            &output_path,
        ],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
}

#[test]
fn split_browser_target_writes_default_loader_and_wasm_sidecar() {
    let dir = temp_dir("split-default");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_args(
        "split-default",
        &[
            &source_path,
            Path::new("--target"),
            Path::new("browser"),
            Path::new("--split"),
        ],
    );
    assert!(status.success(), "stderr: {stderr}");
    assert!(stdout.contains("program.js"), "{stdout}");
    assert!(dir.join("program.wasm").exists());
    let bytes = fs::read(dir.join("program.wasm")).expect("reading split wasm output");
    assert_eq!(&bytes[..4], b"\0asm");
    let loader = fs::read_to_string(dir.join("program.js")).expect("reading split loader");
    assert!(loader.contains("new URL(\"./program.wasm\", import.meta.url)"));
    assert!(loader.contains("WebAssembly.instantiateStreaming"));
    assert!(loader.contains("response.arrayBuffer()"));
}

#[test]
fn split_browser_output_uses_custom_loader_stem_and_infers_target() {
    let dir = temp_dir("split-custom");
    let source_path = write_source(&dir, "+.");
    let output_dir = dir.join("dist");
    fs::create_dir_all(&output_dir).expect("creating output directory");
    let js_path = output_dir.join("app.js");
    let (status, stdout, stderr) = run_args(
        "split-custom",
        &[
            &source_path,
            Path::new("-o"),
            &js_path,
            Path::new("--split"),
        ],
    );
    assert!(status.success(), "stderr: {stderr}");
    assert!(stdout.contains("dist"), "{stdout}");
    assert!(js_path.exists());
    assert!(output_dir.join("app.wasm").exists());
    let loader = fs::read_to_string(js_path).expect("reading custom split loader");
    assert!(loader.contains("new URL(\"./app.wasm\", import.meta.url)"));
}

#[test]
fn split_is_rejected_for_wasi_target() {
    let dir = temp_dir("split-wasi");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_args(
        "split-wasi",
        &[
            &source_path,
            Path::new("--target"),
            Path::new("wasi"),
            Path::new("--split"),
        ],
    );
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr.contains("--split requires the browser target"),
        "{stderr}"
    );
    assert!(stderr.contains("usage: bfwasm"), "{stderr}");
}

#[test]
fn split_rejects_a_wasm_loader_path() {
    let dir = temp_dir("split-wasm-loader");
    let source_path = write_source(&dir, "+.");
    let output_path = dir.join("program.wasm");
    let (status, _, stderr) = run_args(
        "split-wasm-loader",
        &[
            &source_path,
            Path::new("--target"),
            Path::new("browser"),
            Path::new("--split"),
            Path::new("-o"),
            &output_path,
        ],
    );
    assert_eq!(status.code(), Some(1));
    assert!(
        stderr.contains("--split cannot use a .wasm loader path"),
        "{stderr}"
    );
}

#[test]
fn split_wasm_sidecar_write_failure_is_reported() {
    let dir = temp_dir("split-wasm-write-failure");
    let source_path = write_source(&dir, "+.");
    let js_path = dir.join("missing-directory").join("program.js");
    let (status, _, stderr) = run_args(
        "split-wasm-write-failure",
        &[
            &source_path,
            Path::new("--target"),
            Path::new("browser"),
            Path::new("--split"),
            Path::new("-o"),
            &js_path,
        ],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
    assert!(stderr.contains("program.wasm"), "{stderr}");
}

#[test]
fn split_loader_write_failure_is_reported_after_sidecar_write() {
    let dir = temp_dir("split-loader-write-failure");
    let source_path = write_source(&dir, "+.");
    let js_path = dir.join("program.js");
    fs::create_dir_all(&js_path).expect("creating loader path as a directory");
    let (status, _, stderr) = run_args(
        "split-loader-write-failure",
        &[
            &source_path,
            Path::new("--target"),
            Path::new("browser"),
            Path::new("--split"),
            Path::new("-o"),
            &js_path,
        ],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
    assert!(dir.join("program.wasm").exists());
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
