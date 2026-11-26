//! `bfjit` — just-in-time Brainfuck compiler.
//!
//! Pipeline: shared parser → shared optimizer → the same per-architecture
//! machine-code emitter `bfnative` uses (`x86_64` / `aarch64`, included
//! directly from `bfnative`'s source so the two can never drift) → fixups
//! resolved against an in-memory layout → bytes copied into an anonymous
//! W^X mapping → jump into the entry stub. No file is ever written, and
//! none of `bfnative`'s container writers (ELF/Mach-O/PE) are involved —
//! the executable *is* the mapping.
//!
//! The generated module is exactly what `bfnative` would put in a binary
//! for this host: entry stub (tape reservation, first commit), program
//! body, exit path, grow routine, error paths. Entering it at code byte 0
//! and letting it terminate the process is the same execution model a
//! loaded `bfnative` executable has — which is why `bfjit`'s output is
//! byte-for-byte identical to `bfnative`'s by construction.
//!
//! Only the *host* can be JIT-compiled: the codegen backend is selected by
//! the detected host OS/arch pair, and asking for anything else is an
//! error (cross-target compilation is `bfnative`'s job, since its output
//! is a file that can be carried to another machine — a JIT's output
//! cannot).

// coverage(off) is used by exec and host only for coverage runs.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

mod exec;
mod host;
mod patch;

use bfc::{optimize, parser};
use bfnative::{codegen, target};
use std::env;
use std::fs;
use std::process::ExitCode;

use target::Target;

// LLVM profiling runtime entry point, present only in coverage builds
// (`cargo-llvm-cov` sets `cfg(coverage)`; plain `cargo build`/`cargo test`
// compile this away to nothing).
//
// The JIT-generated entry stub terminates the process with a raw
// `exit_group` syscall (Linux) or `_exit`/`ExitProcess` (macOS/Windows) —
// none of which run the atexit hooks the profiling runtime relies on to
// flush its counters. Without an explicit dump, every *successful* JIT run
// would be recorded as if none of bfjit's own code had executed. Calling
// this right before the jump preserves coverage data through the one exit
// path that otherwise loses it, and is a no-op in normal builds.
#[cfg(coverage)]
unsafe extern "C" {
    fn __llvm_profile_write_file() -> i32;
}

const USAGE: &str = "usage: bfjit <input.bf>

Compiles the program to machine code in memory (the host platform's
architecture, detected at runtime — no cross-target JIT) and runs it
immediately: stdin in, stdout out, no output file. The generated code
handles its own tape growth, I/O, and exit, exactly like a bfnative-built
executable.";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    if has_help_flag(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }

    let input = match parse_args(args) {
        Ok(input) => input,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let source = match fs::read_to_string(&input) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("couldn't read {input}: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Same front end as bfc/bfnative: parse, then optimize. The JIT is
    // supposed to be fast — this is the one place where running the
    // optimizer is the whole point.
    let raw_ops = match parser::parse(&source) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("{input}:{}: {}", err.line, err.message);
            return ExitCode::FAILURE;
        }
    };
    let ops = optimize::optimize(raw_ops);

    // Host-only JIT: the codegen backend must match the machine this
    // process is running on, or the generated code would be for the wrong
    // CPU. Target::host() derives from the same cfg!s the binary was
    // compiled for, so this can't mismatch in practice — it's the explicit
    // statement of "the host, detected at runtime".
    let target = Target::host();

    // One module: entry + body + exits + runtime routines, with fixups
    // for everything whose address wasn't known during emission.
    let mut module = codegen::emit(&ops, target);

    // ---- layout the JIT image inside one region ----
    // code at offset 0 (entry stub first), rodata 16-aligned after it,
    // GOT/IAT (macOS/Windows only) 16-aligned after that. 16-alignment
    // satisfies every constraint in the patcher (ARM64 ADRP+LDR wants
    // 8-byte-aligned slots; 16 covers it).
    let got_slots = host::got_slots(target.os);
    let got_count = got_slots.len();
    let image_layout = image_layout(module.code.len(), module.rodata.len(), got_count);

    // Map first (writable, non-executable), then resolve fixups against
    // the region's real address, then copy.
    let region = match unsafe { exec::JitRegion::alloc(image_layout.total) } {
        Ok(region) => region,
        Err(message) => {
            eprintln!("bfjit: {message}");
            return ExitCode::FAILURE;
        }
    };

    let layout = patch::Layout {
        code_vaddr: region.base as u64,
        rodata_vaddr: region.base as u64 + image_layout.rodata_off as u64,
        got_vaddr: image_layout
            .got_off
            .map_or(0, |off| region.base as u64 + off as u64),
    };

    if let Err(message) = patch::patch(&mut module, &layout) {
        eprintln!("bfjit: {message}");
        return ExitCode::FAILURE;
    }

    unsafe {
        // Writable phase: code, read-only data, and the "dynamic link"
        // table all go in before the W^X flip.
        region.write_bytes(0, &module.code);
        region.write_bytes(image_layout.rodata_off, &module.rodata);
        if let Some(got_off) = image_layout.got_off {
            for (i, function) in got_slots.iter().enumerate() {
                region.write_u64(got_off + 8 * i, *function as u64);
            }
        }

        if let Err(message) = region.make_executable() {
            eprintln!("bfjit: {message}");
            return ExitCode::FAILURE;
        }

        // Flush coverage counters (see the extern block above) before the
        // jump: the generated entry never returns and exits the process via
        // a path that skips the profile writer.
        #[cfg(coverage)]
        {
            // Safety: this FFI function is only linked in coverage builds
            // (where the profiling runtime that defines it is linked too)
            // and takes no arguments and touches no Rust state.
            __llvm_profile_write_file();
        }

        // Jump. The generated entry never returns — it runs the program
        // and exits the process with the program's own exit status and
        // I/O, so nothing after this line executes.
        region.enter()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ImageLayout {
    rodata_off: usize,
    got_off: Option<usize>,
    total: usize,
}

fn image_layout(code_len: usize, rodata_len: usize, got_count: usize) -> ImageLayout {
    let rodata_off = align_up(code_len, 16);
    let rodata_end = rodata_off + rodata_len;
    let got_off = (got_count > 0).then(|| align_up(rodata_end, 16));
    let total = got_off.map_or(rodata_end, |off| off + 8 * got_count);

    ImageLayout {
        rodata_off,
        got_off,
        total,
    }
}

fn has_help_flag(args: &[String]) -> bool {
    args.iter()
        .skip(1)
        .any(|arg| arg == "-h" || arg == "--help")
}

/// Accepts exactly one input path. `--target` (and anything else that
/// looks like a flag) is rejected with a message that says plainly that
/// the JIT is host-only, so nobody reaches for cross-target JIT by
/// accident.
fn parse_args(args: Vec<String>) -> Result<String, String> {
    let mut input: Option<String> = None;

    for arg in args.into_iter().skip(1) {
        match arg.as_str() {
            "--target" | "-t" => {
                return Err(format!(
                    "bfjit JIT-compiles only for the host it runs on \
                     (detected at runtime: {}); cross-target compilation is \
                     bfnative's job",
                    Target::host()
                ));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => {
                if input.is_some() {
                    return Err("more than one input file given".to_string());
                }
                input = Some(other.to_string());
            }
        }
    }

    input.ok_or_else(|| "no input file given".to_string())
}

fn align_up(value: usize, align: usize) -> usize {
    debug_assert!(align.is_power_of_two());
    (value + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs parse_args over a synthetic argv (program name first).
    fn parsed(args: &[&str]) -> Result<String, String> {
        parse_args(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn parse_args_accepts_exactly_one_input() {
        assert_eq!(parsed(&["bfjit", "prog.bf"]).as_deref(), Ok("prog.bf"));
    }

    #[test]
    fn parse_args_rejects_missing_input() {
        let err = parsed(&["bfjit"]).unwrap_err();
        assert!(err.contains("no input file given"), "{err}");
    }

    #[test]
    fn parse_args_rejects_a_second_input() {
        let err = parsed(&["bfjit", "a.bf", "b.bf"]).unwrap_err();
        assert!(err.contains("more than one input file"), "{err}");
    }

    #[test]
    fn parse_args_rejects_unknown_options() {
        let err = parsed(&["bfjit", "--wat", "a.bf"]).unwrap_err();
        assert!(err.contains("unknown option: --wat"), "{err}");
    }

    #[test]
    fn parse_args_names_the_host_when_target_is_requested() {
        // The whole point of this message: steer cross-target requests at
        // bfnative instead of failing cryptically inside the JIT.
        let err = parsed(&["bfjit", "--target"]).unwrap_err();
        assert!(err.contains("host"), "{err}");
        assert!(err.contains("bfnative"), "{err}");
        // The short form is rejected the same way.
        let err = parsed(&["bfjit", "-t", "a.bf"]).unwrap_err();
        assert!(err.contains("host"), "{err}");
    }

    #[test]
    fn align_up_rounds_up_and_passes_exact_values_through() {
        assert_eq!(align_up(0, 16), 0);
        assert_eq!(align_up(1, 16), 16);
        assert_eq!(align_up(16, 16), 16);
        assert_eq!(align_up(17, 16), 32);
        assert_eq!(align_up(33, 16), 48);
    }

    #[test]
    fn help_flag_is_recognized_after_the_program_name() {
        assert!(has_help_flag(&["bfjit".to_string(), "--help".to_string()]));
        assert!(has_help_flag(&["bfjit".to_string(), "-h".to_string()]));
        assert!(!has_help_flag(&["--help".to_string()]));
        assert!(!has_help_flag(&[
            "bfjit".to_string(),
            "program.bf".to_string()
        ]));
    }

    #[test]
    fn image_layout_handles_images_with_and_without_got_slots() {
        assert_eq!(
            image_layout(101, 3, 0),
            ImageLayout {
                rodata_off: 112,
                got_off: None,
                total: 115,
            }
        );
        assert_eq!(
            image_layout(101, 3, 2),
            ImageLayout {
                rodata_off: 112,
                got_off: Some(128),
                total: 144,
            }
        );
    }
}
