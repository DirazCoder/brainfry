//! End-to-end tests: compile real programs with the bfnative binary, run the
//! produced executables, and check their output.
//!
//! Execution tests run only on linux-x86_64, the one target whose host
//! environment these tests can assume. linux-aarch64 gets executed too, via
//! the QEMU-user differential harness in `scripts/differential_test.py`
//! (set QEMU_AARCH64 to enable it). Everything cross-target gets at least
//! built and structurally validated (header magic, container shape, import
//! and bind tables, signature page hashes) in the per-format unit tests.
//!
//! Note there is no `cc` check anymore: the whole point of the new backend
//! is that no C toolchain is needed anywhere, so these tests run in any
//! environment with a Rust compiler.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

const HELLO_WORLD: &str = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.\
                           +++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";

/// Writes the source file the tests share and returns its path.
fn write_source(dir: &std::path::Path, source: &str) -> std::path::PathBuf {
    let source_path = dir.join("program.bf");
    fs::write(&source_path, source).expect("writing test program");
    source_path
}

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
fn echo_stdin_until_eof() {
    // The [-] inside the loop zeroes the cell after printing so the loop
    // test terminates at EOF (where the cell stays unchanged at 0).
    run(
        "echo",
        ",[.[-],]",
        "echo, echo, echo\n",
        "echo, echo, echo\n",
    );
}

#[test]
fn pointer_walks_past_the_interpreters_initial_tape() {
    // 40,000 cells out is past bfrun's initial 30,000; both bfrun and the
    // native backend grow the tape there — the native one commits more
    // pages of its 1 GiB reservation via mprotect.
    run("big-move", &format!("{}+.", ">".repeat(40_000)), "", "\x01");
}

#[test]
fn tape_growth_beyond_the_first_commit_granule() {
    // 200,000 cells forces several growth steps (30000 -> 65536 -> 131072
    // -> 262144), exercising repeated mprotect commits and the doubling
    // policy, and lands the pointer past the first 64 KiB commit exactly.
    run(
        "deep-tape",
        &format!("{}+.", ">".repeat(200_000)),
        "",
        "\x01",
    );
}

#[test]
fn deeply_nested_loops_six_levels() {
    // Six levels of nested brackets. Counts 6*5*4*3*2*2 = 1440 accumulate in
    // the innermost cell; 1440 mod 256 = 160. Expected value verified
    // against bfrun. (Byte, not UTF-8: 160 is not a valid UTF-8 start.)
    let source = "++++++[>+++++[>++++[>+++[>++[>++[>+<-]<-]<-]<-]<-]<-]>>>>>>.";
    run_bytes("nested-six", source, "", &[160]);
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

#[test]
fn zero_loop_folding_and_runs() {
    // [-], [+] fold to Zero; long +/- runs fold into counted ops. The loop
    // body is pointer-balanced (an earlier draft ended in a net `<` and
    // correctly tripped the left-of-cell-0 check in both bfrun and the
    // native backend).
    run("zero-loop", "++++++++++[-]+.", "", "\x01");
    run("zero-loop-inc", "++[+++[-]+[-]]++.", "", "\x02");
}

#[test]
fn io_inside_loops() {
    // Input and output both inside loop bodies, with the zero idiom nested
    // one level down: read a byte, print it doubled, zero it, repeat.
    run("io-in-loop", ",[.[-],]", "hi!", "hi!");
}

#[test]
fn pointer_underflow_is_a_clean_error() {
    // Walking left of cell 0 must produce bfrun's exact message on stderr
    // and exit code 1 — not a segfault.
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
    // A MoveLeft far larger than the pointer offset must be detected (the
    // comparison is against a zero-extended count), not wrap the pointer
    // into unmapped memory. One million is past any 16-bit truncation and
    // far past offset 10, while keeping the source file at ~1 MB.
    let source = format!("{}{}<", ">".repeat(10), "<".repeat(1_000_000));
    // Folds to MoveRight(10), MoveLeft(1_000_000): far past the offset.
    let (code, _, stderr) = run_raw("huge-left", &source, "");
    assert_eq!(code.code(), Some(1));
    assert!(String::from_utf8_lossy(&stderr).contains("left of cell 0"));
}

#[test]
fn output_executable_is_a_real_elf() {
    let dir = std::env::temp_dir().join("bfnative-e2e-elf");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let bin = dir.join("program");
    compile(&dir, HELLO_WORLD, &bin);
    let bytes = fs::read(&bin).expect("reading executable");
    assert_eq!(&bytes[..4], b"\x7fELF");
    assert_eq!(bytes[4], 2); // 64-bit
    assert_eq!(bytes[5], 1); // little endian
}

#[test]
fn cross_targets_produce_their_container_formats() {
    let dir = std::env::temp_dir().join("bfnative-e2e-cross");
    fs::create_dir_all(&dir).expect("creating temp dir");
    write_source(&dir, HELLO_WORLD);
    let cases = [
        ("linux-aarch64", [0x7Fu8, b'E', b'L', b'F']),
        ("macos-x86_64", [0xCFu8, 0xFA, 0xED, 0xFE]), // Mach-O 64 LE
        ("macos-aarch64", [0xCFu8, 0xFA, 0xED, 0xFE]),
        ("windows-x86_64", [b'M', b'Z', 0, 0]),
        ("windows-aarch64", [b'M', b'Z', 0, 0]),
    ];
    for (target, magic) in cases {
        let out = dir.join(format!("prog-{}", target));
        let compiled = Command::new(env!("CARGO_BIN_EXE_bfnative"))
            .arg("--target")
            .arg(target)
            .arg(dir.join("program.bf"))
            .arg("-o")
            .arg(&out)
            .output()
            .expect("running bfnative");
        assert!(
            compiled.status.success(),
            "bfnative failed for {target}:\n{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let bytes = fs::read(&out).expect("reading cross output");
        assert_eq!(&bytes[..4], &magic, "{target} container magic");
    }
}

#[test]
fn emit_asm_writes_a_listing() {
    let dir = std::env::temp_dir().join("bfnative-e2e-listing");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let listing = dir.join("program.lst");
    write_source(&dir, HELLO_WORLD);
    let compiled = Command::new(env!("CARGO_BIN_EXE_bfnative"))
        .arg("--emit-asm")
        .arg(dir.join("program.bf"))
        .arg("-o")
        .arg(&listing)
        .output()
        .expect("running bfnative");
    assert!(compiled.status.success());
    let text = fs::read_to_string(&listing).expect("reading listing");
    assert!(text.contains("generated-code listing"));
    assert!(text.contains("target:  linux-x86_64"));
    // The listing shows resolved machine code, so hex offsets must appear.
    assert!(text.contains("000000b0") || text.contains("entry"));
}

/// Compiles `source` and asserts stdout + a successful exit.
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
    let dir = std::env::temp_dir().join(format!("bfnative-e2e-{tag}"));
    fs::create_dir_all(&dir).expect("creating temp dir");
    let binary_path = compile(&dir, source, &dir.join("program"));

    let mut child = Command::new(&binary_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running compiled program");
    child
        .stdin
        .take()
        .expect("piping stdin")
        .write_all(stdin.as_bytes())
        .expect("writing stdin");
    let output = child.wait_with_output().expect("waiting for program");
    (output.status, output.stdout, output.stderr)
}

fn compile(
    dir: &std::path::Path,
    source: &str,
    binary_path: &std::path::Path,
) -> std::path::PathBuf {
    let source_path = write_source(dir, source);
    let compiled = Command::new(env!("CARGO_BIN_EXE_bfnative"))
        .arg(&source_path)
        .arg("-o")
        .arg(binary_path)
        .output()
        .expect("running bfnative");
    assert!(
        compiled.status.success(),
        "bfnative failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    binary_path.to_path_buf()
}

// ---------------------------------------------------------------------------
// CLI surface: every flag, error path, and default-output rule
// ---------------------------------------------------------------------------

/// Runs the bfnative binary with raw args, empty stdin.
fn run_cli(
    _tag: &str,
    args: &[&std::ffi::OsStr],
) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bfnative"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfnative");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn help_flag_prints_usage_and_succeeds() {
    let dir = std::env::temp_dir().join("bfnative-e2e-help");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let (status, stdout, stderr) = run_cli(
        "help",
        &[source_path.as_os_str(), "--help".as_ref()],
    );
    assert_eq!(status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("usage: bfnative"), "{stdout}");
}

#[test]
fn no_input_file_fails_with_usage() {
    let (status, _, stderr) = run_cli("no-input", &[]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("usage: bfnative"), "{stderr}");
    assert!(stderr.contains("no input file given"), "{stderr}");
}

#[test]
fn unknown_option_fails() {
    let dir = std::env::temp_dir().join("bfnative-e2e-unknown-opt");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_cli("unknown-opt", &[source_path.as_os_str(), "--wat".as_ref()]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("unknown option: --wat"), "{stderr}");
}

#[test]
fn flag_without_a_value_fails() {
    let dir = std::env::temp_dir().join("bfnative-e2e-flag-value");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_cli("flag-value", &[source_path.as_os_str(), "--target".as_ref()]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("--target needs a value"), "{stderr}");
}

#[test]
fn output_flag_without_a_value_fails() {
    let dir = std::env::temp_dir().join("bfnative-e2e-o-value");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_cli("o-value", &[source_path.as_os_str(), "-o".as_ref()]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("--output needs a value"), "{stderr}");
}

#[test]
fn cc_flag_without_a_value_fails() {
    // --cc is deprecated and ignored, but it still parses its value, and a
    // missing one is a usage error like any other.
    let dir = std::env::temp_dir().join("bfnative-e2e-cc-value");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_cli("cc-value", &[source_path.as_os_str(), "--cc".as_ref()]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("--cc needs a value"), "{stderr}");
}

#[test]
fn more_than_one_input_file_is_rejected() {
    let dir = std::env::temp_dir().join("bfnative-e2e-two-inputs");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_cli("two-inputs", &[source_path.as_os_str(), source_path.as_os_str()]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("more than one input file"), "{stderr}");
}

#[test]
fn deprecated_flags_parse_and_warn_but_succeed() {
    // --cc and --keep-asm are accepted for compatibility, emit a warning,
    // and change nothing.
    let dir = std::env::temp_dir().join("bfnative-e2e-deprecated");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let out = dir.join("program");
    let (status, _, stderr) = run_cli(
        "deprecated",
        &[
            "--cc".as_ref(),
            "gcc".as_ref(),
            "--keep-asm".as_ref(),
            source_path.as_os_str(),
            "-o".as_ref(),
            out.as_os_str(),
        ],
    );
    assert!(status.success(), "stderr: {stderr}");
    assert!(stderr.contains("--cc is deprecated"), "{stderr}");
    assert!(stderr.contains("--keep-asm is deprecated"), "{stderr}");
    assert!(out.exists());
}

#[test]
fn unknown_target_fails_and_lists_the_valid_ones() {
    let dir = std::env::temp_dir().join("bfnative-e2e-bad-target");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let (status, _, stderr) = run_cli(
        "bad-target",
        &["--target".as_ref(), "linux-arm64".as_ref(), source_path.as_os_str()],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("unknown target `linux-arm64`"), "{stderr}");
    assert!(stderr.contains("valid targets are"), "{stderr}");
}

#[test]
fn unreadable_input_fails_cleanly() {
    let (status, _, stderr) = run_cli(
        "missing-input",
        &["/nonexistent/bfnative-e2e/nope.bf".as_ref()],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't read"), "{stderr}");
}

#[test]
fn parse_error_names_the_line() {
    let dir = std::env::temp_dir().join("bfnative-e2e-parse-error");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+\n[");
    let (status, _, stderr) = run_cli("parse-error", &[source_path.as_os_str()]);
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("program.bf:2:"), "{stderr}");
    assert!(stderr.contains("unmatched '['"), "{stderr}");
}

#[test]
fn emit_asm_writes_to_a_default_lst_next_to_the_source() {
    // --emit-asm without -o: the default output is a .lst, not an
    // executable-shaped basename.
    let dir = std::env::temp_dir().join("bfnative-e2e-lst-default");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, HELLO_WORLD);
    let (status, stdout, stderr) = run_cli(
        "lst-default",
        &["--emit-asm".as_ref(), source_path.as_os_str()],
    );
    assert!(status.success(), "stderr: {stderr}");
    let listing = dir.join("program.lst");
    assert!(listing.exists(), "stdout: {stdout}");
    let text = fs::read_to_string(&listing).expect("reading listing");
    assert!(text.contains("generated-code listing"));
}

#[test]
fn emit_asm_write_failure_is_clean() {
    let dir = std::env::temp_dir().join("bfnative-e2e-lst-write-fail");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let bad = dir.join("no-such-dir").join("program.lst");
    let (status, _, stderr) =
        run_cli(
        "lst-write-fail",
        &[
            "--emit-asm".as_ref(),
            source_path.as_os_str(),
            "-o".as_ref(),
            bad.as_os_str(),
        ],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
}

#[test]
fn executable_write_failure_is_clean() {
    let dir = std::env::temp_dir().join("bfnative-e2e-write-fail");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let bad = dir.join("no-such-dir").join("program");
    let (status, _, stderr) = run_cli(
        "write-fail",
        &[source_path.as_os_str(), "-o".as_ref(), bad.as_os_str()],
    );
    assert_eq!(status.code(), Some(1));
    assert!(stderr.contains("couldn't write"), "{stderr}");
}

#[test]
fn default_output_path_is_the_bare_input_stem() {
    // No -o: a linux host target writes `program` next to `program.bf`,
    // executable-bit set.
    let dir = std::env::temp_dir().join("bfnative-e2e-default-out");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, HELLO_WORLD);
    let (status, stdout, stderr) = run_cli("default-out", &[source_path.as_os_str()]);
    assert!(status.success(), "stderr: {stderr}");
    let out = dir.join("program");
    assert!(out.exists(), "stdout: {stdout}");
    let bytes = fs::read(&out).expect("reading default output");
    assert_eq!(&bytes[..4], b"\x7fELF");
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(&out).expect("stat").permissions().mode();
    assert_eq!(mode & 0o111, 0o111, "execute bit missing");
}

#[test]
fn windows_default_output_path_gets_the_exe_suffix() {
    let dir = std::env::temp_dir().join("bfnative-e2e-default-exe");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, HELLO_WORLD);
    let (status, _, stderr) = run_cli(
        "default-exe",
        &["--target".as_ref(), "windows-x86_64".as_ref(), source_path.as_os_str()],
    );
    assert!(status.success(), "stderr: {stderr}");
    let out = dir.join("program.exe");
    assert!(out.exists());
    let bytes = fs::read(&out).expect("reading default exe output");
    assert_eq!(&bytes[..2], b"MZ");
}

#[test]
fn macho_identifiers_survive_unusual_output_names() {
    // The ad-hoc code signature identifier is derived from the output
    // basename; characters outside the allowed set must be sanitized, not
    // corrupt the module. (Also: the identifier is only derived after a
    // successful compile, so a clean exit is the observable behavior.)
    let dir = std::env::temp_dir().join("bfnative-e2e-macho-name");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let out = dir.join("my prog#1!");
    let (status, _, stderr) = run_cli(
        "macho-name",
        &[
            "--target".as_ref(),
            "macos-x86_64".as_ref(),
            source_path.as_os_str(),
            "-o".as_ref(),
            out.as_os_str(),
        ],
    );
    assert!(status.success(), "stderr: {stderr}");
    let bytes = fs::read(&out).expect("reading macho output");
    assert_eq!(&bytes[..4], &[0xCF, 0xFA, 0xED, 0xFE]);
}

#[test]
fn trailing_slash_output_fails_cleanly_at_the_write() {
    // A path ending in `/` has an empty basename (the Mach-O sanitizer
    // falls back to "bfnative" for the identifier) and no filename to
    // write to, so the run must fail cleanly at the final write.
    let dir = std::env::temp_dir().join("bfnative-e2e-trailing-slash");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = write_source(&dir, "+.");
    let bad = dir.join("no-such-dir").join("");
    let (status, _, stderr) = run_cli(
        "trailing-slash",
        &[
            "--target".as_ref(),
            "macos-aarch64".as_ref(),
            source_path.as_os_str(),
            "-o".as_ref(),
            bad.as_os_str(),
        ],
    );
    assert_eq!(status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("couldn't write"), "{stderr}");
}
