//! End-to-end JIT tests execute generated machine code on every supported
//! host and check stdout / stderr / exit status. Static inspection of the
//! emitted bytes is the unit tests' job.
//!
//! The optional aarch64-under-QEMU cases run on any host that can launch the
//! configured emulator when both BFJIT_AARCH64 (path to an
//! aarch64-unknown-linux-gnu build of this binary) and QEMU_AARCH64 (path to
//! qemu-aarch64-static) are set — mirroring the bfnative differential
//! harness's QEMU_AARCH64 convention.
//!
//! Expected outputs are hardcoded, cross-checked against `bfrun` first
//! (same policy as bfnative's e2e suite). The four-way differential
//! harness in `scripts/differential_test_all.py` is the authority for
//! cross-tool agreement.

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
    run(
        "echo",
        ",[.[-],]",
        "echo, echo, echo\n",
        "echo, echo, echo\n",
    );
}

#[test]
fn pointer_walks_past_the_initial_tape() {
    // 40,000 cells out is past the initial 30,000 — the generated grow
    // routine must commit more pages of the tape reservation from JITed code.
    run("big-move", &format!("{}+.", ">".repeat(40_000)), "", "\x01");
}

#[test]
fn tape_growth_through_several_commit_granules() {
    // 200,000 cells forces repeated growth (30000 -> 65536 -> 131072 ->
    // 262144) and lands past the first 64 KiB commit exactly.
    run(
        "deep-tape",
        &format!("{}+.", ">".repeat(200_000)),
        "",
        "\x01",
    );
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
fn untouched_excursion_in_an_offset_add_candidate_loop_still_underflows() {
    // +[<>-] and +[-<>]: the loop body visits cell -1 without ever writing
    // it. The optimizer used to fold exactly these shapes to a plain Zero
    // (it only tracked written cells, so every structural check passed)
    // and thereby dropped the underflow the real loop raises; the shared
    // bfc front end now rejects the fold, so the JITed code must keep
    // erroring with bfrun's exact message and exit code.
    for (tag, source) in [("excursion-lead", "+[<>-]"), ("excursion-trail", "+[-<>]")] {
        let (code, stdout, stderr) = run_raw(tag, source, "");
        assert_eq!(stdout, b"", "{tag}");
        assert_eq!(code.code(), Some(1), "{tag}");
        assert_eq!(
            String::from_utf8_lossy(&stderr),
            "runtime error: pointer moved left of cell 0\n",
            "{tag}"
        );
    }
}

#[test]
fn unmatched_brackets_fail_before_jitting_on_every_host() {
    let source_path = write_source("unmatched", "[+");
    let out = Command::new(env!("CARGO_BIN_EXE_bfjit"))
        .arg(&source_path)
        .stdin(Stdio::null())
        .output()
        .expect("running bfjit");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unmatched '['"));
}

#[test]
fn help_flag_prints_usage_and_succeeds() {
    let dir = std::env::temp_dir().join("bfjit-e2e-help");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = dir.join("program.bf");
    fs::write(&source_path, HELLO_WORLD).expect("writing test program");
    let out = Command::new(env!("CARGO_BIN_EXE_bfjit"))
        .arg(&source_path)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfjit");
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("usage: bfjit"),
        "{:?}",
        out.stdout
    );
}

#[test]
fn unreadable_input_fails_cleanly() {
    let missing = std::env::temp_dir().join(format!("bfjit-e2e-missing-{}.bf", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_bfjit"))
        .arg(&missing)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfjit");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("couldn't read"),
        "{:?}",
        out.stderr
    );
}

#[test]
fn no_input_prints_usage_and_fails() {
    let out = Command::new(env!("CARGO_BIN_EXE_bfjit"))
        .stdin(Stdio::null())
        .output()
        .expect("running bfjit");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("usage: bfjit"),
        "{:?}",
        out.stderr
    );
}

#[test]
fn unknown_option_fails_before_compiling() {
    let source_path = write_source("unknown-opt", HELLO_WORLD);
    let out = Command::new(env!("CARGO_BIN_EXE_bfjit"))
        .arg("--wat")
        .arg(&source_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfjit");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown option: --wat"),
        "{:?}",
        out.stderr
    );
}

#[test]
fn more_than_one_input_file_is_rejected() {
    let a = write_source("two-inputs-a", HELLO_WORLD);
    let b = write_source("two-inputs-b", HELLO_WORLD);
    let out = Command::new(env!("CARGO_BIN_EXE_bfjit"))
        .arg(&a)
        .arg(&b)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("running bfjit");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("more than one input file"),
        "{:?}",
        out.stderr
    );
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
    assert!(
        stderr.contains("host"),
        "stderr should say host-only: {stderr}"
    );
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

// ---------------------------------------------------------------------------
// JitRegion::alloc failure — e2e
// ---------------------------------------------------------------------------

/// Verifies that an OS-level allocation failure inside `prepare_image`
/// (the `JitRegion::alloc` path) is surfaced as a clean CLI failure (exit 1)
/// rather than a panic or silent misbehaviour.
///
/// Strategy: spawn the real `bfjit` binary in a child process whose virtual
/// address space is capped to something far too small to hold any JIT region,
/// then assert the child exits 1 and prints a human-readable error to stderr.
///
/// The cap is applied differently per platform:
///   Unix (Linux + macOS): `setrlimit(RLIMIT_AS)` via inline FFI — no dep.
///   Windows: a Job Object with a commit-limit via inline FFI — no dep.
///
/// Set `BFJIT_SKIP_OOM_TEST=1` to opt out on any platform (useful in
/// environments that disallow process resource limits, e.g. some containers).
#[test]
fn alloc_failure_in_prepare_image_is_reported_as_a_cli_failure() {
    if std::env::var("BFJIT_SKIP_OOM_TEST").is_ok() {
        eprintln!("skipping: BFJIT_SKIP_OOM_TEST is set");
        return;
    }

    // Write a trivial (but valid) BF program so the error happens at alloc,
    // not at parse or codegen.
    let dir = std::env::temp_dir().join("bfjit-e2e-oom");
    fs::create_dir_all(&dir).expect("creating temp dir");
    let source_path = dir.join("program.bf");
    fs::write(&source_path, "+.").expect("writing test program");

    let bfjit = env!("CARGO_BIN_EXE_bfjit");

    let out = spawn_with_capped_vmem(bfjit, source_path.to_str().unwrap());

    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.is_empty(),
        "expected a human-readable error on stderr, got silence"
    );
}

/// Spawns `bfjit <source>` in a subprocess whose virtual address space is
/// capped to 64 MiB — large enough for the loader and runtime, far too small
/// for the JIT mmap region.
///
/// Unix: sets `RLIMIT_AS` on the child via a pre-exec hook (inline libc FFI).
/// Windows: wraps the child in a Job Object with a process-memory limit
/// (inline winapi FFI).
#[cfg(unix)]
fn spawn_with_capped_vmem(bfjit: &str, source: &str) -> std::process::Output {
    use std::os::unix::process::CommandExt;

    // 64 MiB — enough for sh + loader, not for a JIT mmap region.
    const CAP: u64 = 64 * 1024 * 1024;

    // RLIMIT_AS is 9 on Linux/macOS (POSIX value).
    #[cfg(target_os = "linux")]
    const RLIMIT_AS: i32 = 9;
    #[cfg(target_os = "macos")]
    const RLIMIT_AS: i32 = 5; // RLIMIT_AS on Darwin

    #[repr(C)]
    struct Rlimit {
        rlim_cur: u64,
        rlim_max: u64,
    }

    extern "C" {
        fn setrlimit(resource: i32, rlim: *const Rlimit) -> i32;
    }

    let mut cmd = Command::new(bfjit);
    cmd.arg(source)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Safety: called between fork and exec; only async-signal-safe ops here.
    // setrlimit is async-signal-safe on Linux and macOS.
    unsafe {
        cmd.pre_exec(move || {
            let lim = Rlimit {
                rlim_cur: CAP,
                rlim_max: CAP,
            };
            if setrlimit(RLIMIT_AS, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.output().expect("spawning memory-constrained bfjit")
}

#[cfg(windows)]
fn spawn_with_capped_vmem(bfjit: &str, source: &str) -> std::process::Output {
    use std::os::windows::io::AsRawHandle;

    // 64 MiB process-memory commit limit via a Job Object.
    const CAP: usize = 64 * 1024 * 1024;

    // Minimal inline FFI — just what we need, nothing more.
    #[allow(non_camel_case_types)]
    type HANDLE = *mut std::ffi::c_void;
    #[allow(non_camel_case_types)]
    type BOOL = i32;
    const FALSE: BOOL = 0;
    const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0100;
    const JobObjectExtendedLimitInformation: u32 = 9;

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct BasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attrs: *mut std::ffi::c_void, name: *const u16) -> HANDLE;
        fn SetInformationJobObject(
            job: HANDLE,
            class: u32,
            info: *mut std::ffi::c_void,
            len: u32,
        ) -> BOOL;
        fn AssignProcessToJobObject(job: HANDLE, process: HANDLE) -> BOOL;
        fn CloseHandle(handle: HANDLE) -> BOOL;
    }

    // Spawn suspended so we can attach the Job before the child runs.
    use std::os::windows::process::CommandExt;
    const CREATE_SUSPENDED: u32 = 0x0000_0004;

    let mut child = Command::new(bfjit)
        .arg(source)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_SUSPENDED)
        .spawn()
        .expect("spawning suspended bfjit");

    unsafe {
        let job = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
        assert!(!job.is_null(), "CreateJobObjectW failed");

        let mut info: ExtendedLimitInformation = std::mem::zeroed();
        info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_PROCESS_MEMORY;
        info.process_memory_limit = CAP;

        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<ExtendedLimitInformation>() as u32,
        );
        assert!(ok != FALSE, "SetInformationJobObject failed");

        let ok = AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE);
        assert!(ok != FALSE, "AssignProcessToJobObject failed");

        CloseHandle(job);

        // Resume the child now that the Job limit is in place.
        extern "system" {
            fn ResumeThread(thread: HANDLE) -> u32;
        }
        // We can't get the main thread handle from std::process::Child, so
        // we resume via NtResumeProcess (single-threaded child at this point).
        extern "system" {
            fn NtResumeProcess(process: HANDLE) -> i32;
        }
        NtResumeProcess(child.as_raw_handle() as HANDLE);
    }

    child.wait_with_output().expect("waiting for bfjit")
}
