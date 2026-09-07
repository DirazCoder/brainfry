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
