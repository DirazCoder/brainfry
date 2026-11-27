//! The execution engine of `bfinterp`: a deliberately naive, one-op-at-a-time
//! walk over the parsed instruction list.
//!
//! This is *not* `bfrun`'s VM and shares no code with it — `bfrun` lives in
//! its own crate and its VM is private to it. The two are independent
//! implementations of the same semantics, fed by the same shared parser
//! front end, which is exactly what makes a differential test between them
//! meaningful: if the outputs ever disagree, one of the two has a bug, and
//! this one is the simpler reference to audit.
//!
//! Semantics (matched op-for-op against `bfrun`'s VM, not invented here):
//!
//! - 30,000 zeroed cells at start, growing to the right on demand
//!   (`Vec::resize` to exactly the needed index, same as `bfrun`).
//! - 8-bit cells, wrapping add/sub.
//! - Moving left of cell 0 is an error: `pointer moved left of cell 0`.
//! - `,` on EOF (or any read failure) leaves the cell untouched.
//! - `[` / `]` are `JumpIfZero` / `JumpIfNonZero` over pre-resolved targets;
//!   like `bfrun`, a taken jump sets `pc = target` and the loop's trailing
//!   `pc += 1` lands execution on `target + 1`.
//! - stdout is flushed once, after the last op.
//!
//! The default path is maximally simple on purpose: the parser emits one op
//! per `+ - > < . , [ ]` character, and this loop dispatches on each one
//! individually. `bfinterp --optimize` runs the shared optimizer first, but
//! that flag exists for comparison runs, not for speed: anything faster
//! belongs in `bfrun`, `bfjit`, or `bfnative`.

use bfformat::Op;
use std::io::{self, Read, Write};

/// Classic Brainfuck starting tape size, matching `bfrun`'s
/// `INITIAL_TAPE_SIZE`. The tape grows past this automatically if a program
/// moves further right than it can hold.
pub const INITIAL_TAPE_SIZE: usize = 30_000;
const TAPE_LIMIT: usize = 1 << 30;

#[derive(Debug)]
pub enum RunError {
    /// The pointer moved left of cell 0. No way to grow in that direction,
    /// so it's a real error — same as `bfrun`.
    PointerUnderflow,
    /// The tape couldn't be grown (allocation failure) — same as `bfrun`.
    GrowFailed,
    TapeLimitExceeded,
    Io(io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Wording mirrors bfrun's RunError::Display exactly: the binary
            // prints `runtime error: {err}` and exits 1, and error-path
            // differential tests compare that string byte-for-byte.
            RunError::PointerUnderflow => {
                write!(f, "pointer moved left of cell 0")
            }
            RunError::GrowFailed => write!(f, "failed to grow tape"),
            RunError::TapeLimitExceeded => {
                write!(f, "tape limit exceeded (1073741824 bytes)")
            }
            RunError::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl From<io::Error> for RunError {
    fn from(err: io::Error) -> Self {
        RunError::Io(err)
    }
}

/// Runs `ops` against real stdin/stdout, locking each once for the whole
/// run (as `bfrun` does) so per-op I/O doesn't pay for repeated lock
/// acquisition.
pub fn run_stdio(ops: &[Op]) -> Result<(), RunError> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run(ops, &mut stdin.lock(), &mut stdout.lock())
}

/// The interpreter proper. Takes I/O as trait objects so every caller —
/// `run_stdio` with the real stdio, the tests with `Cursor`s, failing
/// writers, and unflushable writers — exercises this one function; the
/// semantics are those described in the module docs.
pub fn run(ops: &[Op], stdin: &mut dyn Read, stdout: &mut dyn Write) -> Result<(), RunError> {
    let mut tape = vec![0u8; INITIAL_TAPE_SIZE];
    let mut cell = 0usize;
    let mut pc = 0usize;

    while pc < ops.len() {
        match ops[pc] {
            Op::Add(n) => tape[cell] = tape[cell].wrapping_add(n),
            Op::Sub(n) => tape[cell] = tape[cell].wrapping_sub(n),
            Op::MoveRight(n) => {
                if n as usize >= TAPE_LIMIT - cell {
                    return Err(RunError::TapeLimitExceeded);
                }
                cell += n as usize;
                // Grow to exactly the cell we just landed on — bfrun's
                // grow-if-needed policy, not a doubling scheme.
                if cell >= tape.len() {
                    grow_to_at_least(&mut tape, cell)?;
                }
            }
            Op::MoveLeft(n) => {
                cell = cell
                    .checked_sub(n as usize)
                    .ok_or(RunError::PointerUnderflow)?;
            }
            Op::Output => stdout.write_all(&[tape[cell]])?,
            Op::Input => {
                let mut byte = [0u8];
                // EOF (or any read error) leaves the cell unchanged — the
                // most common real-world convention, and bfrun's exact
                // behavior: a failed `read_exact` simply doesn't touch the
                // cell.
                if stdin.read_exact(&mut byte).is_ok() {
                    tape[cell] = byte[0];
                }
            }
            // Taken jumps land on `target`, and the shared `pc += 1` below
            // advances to `target + 1` — the same off-by-one convention
            // bfrun uses, which the native/jit backends' branches encode as
            // "aim one past the paired bracket op".
            Op::JumpIfZero { target } => {
                if tape[cell] == 0 {
                    pc = target as usize;
                }
            }
            Op::JumpIfNonZero { target } => {
                if tape[cell] != 0 {
                    pc = target as usize;
                }
            }
            Op::Zero => tape[cell] = 0,
            Op::AddAt { offset, delta } => {
                let target = offset_cell(cell, offset)?;
                if target >= tape.len() {
                    grow_to_at_least(&mut tape, target)?;
                }
                tape[target] = tape[target].wrapping_add(delta);
            }
            Op::ZeroAt { offset } => {
                let target = offset_cell(cell, offset)?;
                if target >= tape.len() {
                    grow_to_at_least(&mut tape, target)?;
                }
                tape[target] = 0;
            }
            Op::SetAt { offset, value } => {
                let target = offset_cell(cell, offset)?;
                if target >= tape.len() {
                    grow_to_at_least(&mut tape, target)?;
                }
                tape[target] = value;
            }
            Op::BoundsCheck {
                min_offset,
                max_offset,
            } => {
                let min = offset_cell(cell, min_offset)?;
                let max = offset_cell(cell, max_offset)?;
                let target = max.max(min);
                if target >= tape.len() {
                    grow_to_at_least(&mut tape, target)?;
                }
            }
            Op::MulAdd { offset, factor } => {
                // Does NOT zero the source cell: the optimizer emits one
                // MulAdd per spread target followed by a single trailing
                // Zero, so a whole group of MulAds sharing one source cell
                // has to keep reading the original value until that final
                // Zero runs. Zeroing here too would make every MulAdd after
                // the first in a group read 0 instead of the real value.
                let value = tape[cell];
                if value != 0 {
                    let target = offset_cell(cell, offset)?;
                    if target >= tape.len() {
                        grow_to_at_least(&mut tape, target)?;
                    }
                    tape[target] = tape[target].wrapping_add(value.wrapping_mul(factor));
                }
            }
            Op::Scan { stride } => {
                while tape[cell] != 0 {
                    cell = offset_cell(cell, stride as isize)?;
                    if cell >= tape.len() {
                        grow_to_at_least(&mut tape, cell)?;
                    }
                }
            }
        }
        pc += 1;
    }

    stdout.flush()?;
    Ok(())
}

/// Applies a signed offset to a cell index, erroring on underflow the same
/// way `MoveLeft` does rather than silently wrapping.
fn offset_cell(cell: usize, offset: isize) -> Result<usize, RunError> {
    let new_cell = cell as i128 + offset as i128;
    if new_cell < 0 {
        Err(RunError::PointerUnderflow)
    } else if new_cell >= TAPE_LIMIT as i128 {
        Err(RunError::TapeLimitExceeded)
    } else {
        Ok(new_cell as usize)
    }
}

/// Grows the tape to cover `index`, failing cleanly on allocation failure
/// instead of aborting the process — the same hardening `bfrun`'s VM got:
/// an unguarded `resize` on a huge move count was a SIGABRT, not a runtime
/// error. `try_reserve` first means the resize itself can no longer fail.
fn grow_to_at_least(tape: &mut Vec<u8>, index: usize) -> Result<(), RunError> {
    debug_assert!(index >= tape.len());
    if index >= TAPE_LIMIT {
        return Err(RunError::TapeLimitExceeded);
    }
    let additional = index + 1 - tape.len();
    tape.try_reserve(additional)
        .map_err(|_| RunError::GrowFailed)?;
    tape.resize(index + 1, 0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::io::Cursor;

    fn interp(ops: &[Op], input: &str) -> (Vec<u8>, Result<(), RunError>) {
        let mut out = Vec::new();
        let result = run(ops, &mut Cursor::new(input.as_bytes()), &mut out);
        (out, result)
    }

    #[test]
    fn add_wraps_at_256() {
        // 256 increments on one cell — wraparound, not overflow panic.
        let (out, result) = interp(&[Op::Add(255), Op::Add(1), Op::Output], "");
        assert!(result.is_ok());
        assert_eq!(out, vec![0u8]);
    }

    #[test]
    fn sub_wraps_below_zero() {
        let (out, result) = interp(&[Op::Sub(1), Op::Output], "");
        assert!(result.is_ok());
        assert_eq!(out, vec![255u8]);
    }

    #[test]
    fn move_right_grows_past_the_initial_tape() {
        // 40,000 cells out is past the initial 30,000.
        let (out, result) = interp(&[Op::MoveRight(40_000), Op::Add(7), Op::Output], "");
        assert!(result.is_ok());
        assert_eq!(out, vec![7u8]);
    }

    #[test]
    fn impossible_tape_growth_returns_grow_failed() {
        let mut tape = vec![0u8; INITIAL_TAPE_SIZE];
        assert!(matches!(
            grow_to_at_least(&mut tape, usize::MAX - 1),
            Err(RunError::TapeLimitExceeded)
        ));
    }

    #[test]
    fn move_left_past_zero_is_an_error() {
        let (_, result) = interp(&[Op::MoveLeft(1)], "");
        assert!(matches!(result, Err(RunError::PointerUnderflow)));
    }

    #[test]
    fn huge_left_move_is_an_error_not_a_wrap() {
        // Offset 10, move left 1,000,000: must fail cleanly, not wrap the
        // index around into a huge value.
        let (_, result) = interp(&[Op::MoveRight(10), Op::MoveLeft(1_000_000)], "");
        assert!(matches!(result, Err(RunError::PointerUnderflow)));
    }

    #[test]
    fn eof_leaves_the_cell_unchanged() {
        // Empty stdin: the cell stays 0, +1 makes 1.
        let (out, _) = interp(&[Op::Input, Op::Add(1), Op::Output], "");
        assert_eq!(out, vec![1u8]);
    }

    #[test]
    fn input_byte_round_trips() {
        let (out, _) = interp(&[Op::Input, Op::Output], "AB");
        assert_eq!(out, b"A");
    }

    #[test]
    fn zero_loop_terminates_and_clears() {
        // The raw [-] idiom: 10 increments, then a decrement loop to zero.
        // Naive execution walks it as an actual loop — the optimizer is what
        // would fold this to Op::Zero, and it must produce the same result.
        let source = "++++++++++[-]";
        let ops = bfc::parser::parse(source).unwrap();
        let (out, result) = interp(&ops, "");
        assert!(result.is_ok());
        assert_eq!(out, Vec::<u8>::new());
    }

    #[test]
    fn offset_ops_update_cells_and_enforce_bounds() {
        let ops = [
            Op::Add(5),
            Op::AddAt {
                offset: 1,
                delta: 2,
            },
            Op::ZeroAt { offset: 1 },
            Op::SetAt {
                offset: 2,
                value: 7,
            },
            Op::BoundsCheck {
                min_offset: 0,
                max_offset: INITIAL_TAPE_SIZE as isize,
            },
            Op::MoveRight(2),
            Op::Output,
        ];
        let (out, result) = interp(&ops, "");
        assert!(result.is_ok());
        assert_eq!(out, vec![7]);

        assert!(matches!(
            interp(
                &[Op::AddAt {
                    offset: TAPE_LIMIT as isize,
                    delta: 1
                }],
                ""
            )
            .1,
            Err(RunError::TapeLimitExceeded)
        ));
        assert!(matches!(
            interp(
                &[Op::BoundsCheck {
                    min_offset: 0,
                    max_offset: TAPE_LIMIT as isize
                }],
                ""
            )
            .1,
            Err(RunError::TapeLimitExceeded)
        ));
        let (out, result) = interp(&[Op::Scan { stride: i32::MAX }], "");
        assert!(result.is_ok());
        assert!(out.is_empty());
    }

    #[test]
    fn offset_writes_and_scans_grow_the_tape() {
        for op in [
            Op::AddAt {
                offset: 40_000,
                delta: 3,
            },
            Op::ZeroAt { offset: 40_000 },
            Op::SetAt {
                offset: 40_000,
                value: 7,
            },
            Op::BoundsCheck {
                min_offset: 0,
                max_offset: 40_000,
            },
        ] {
            let (_, result) = interp(&[op], "");
            assert!(result.is_ok(), "{op:?}: {result:?}");
        }

        let (_, result) = interp(
            &[
                Op::Add(1),
                Op::MulAdd {
                    offset: 40_000,
                    factor: 2,
                },
            ],
            "",
        );
        assert!(result.is_ok());
        let (_, result) = interp(&[Op::Add(1), Op::Scan { stride: 40_000 }], "");
        assert!(result.is_ok());
        assert!(matches!(
            interp(&[Op::MoveRight(TAPE_LIMIT as u32)], "").1,
            Err(RunError::TapeLimitExceeded)
        ));
    }

    #[test]
    fn offset_cell_covers_valid_underflow_and_tape_limit() {
        assert!(matches!(offset_cell(1, -1), Ok(0)));
        assert!(matches!(
            offset_cell(0, -1),
            Err(RunError::PointerUnderflow)
        ));
        assert!(matches!(
            offset_cell(0, TAPE_LIMIT as isize),
            Err(RunError::TapeLimitExceeded)
        ));
    }

    #[test]
    fn jump_lands_one_past_the_target() {
        // +[-]+. — enter the loop, clear, exit, set 1, print.
        let source = "+[-]+.";
        let ops = bfc::parser::parse(source).unwrap();
        let (out, result) = interp(&ops, "");
        assert!(result.is_ok());
        assert_eq!(out, vec![1u8]);
    }

    #[test]
    fn a_zero_cell_skips_the_loop_body_entirely() {
        // Entering [-] with the cell already zero: JumpIfZero must fire
        // and land one past the loop without running a single Sub. Every
        // other loop test enters with a nonzero cell, so only the JNZ
        // side of the pair was ever exercised.
        let (out, result) = interp(&bfc::parser::parse("[-]+.").unwrap(), "");
        assert!(result.is_ok());
        assert_eq!(out, vec![1u8]);
    }

    #[test]
    fn optimized_and_naive_paths_agree() {
        // Whatever the optimizer does to this loop nest, the interpreter
        // must produce the same byte.
        let source = "++++++[>+++++[>++++[>+++[>++[>++[>+<-]<-]<-]<-]<-]<-]>>>>>>.";
        let raw = bfc::parser::parse(source).unwrap();
        let optimized = bfc::optimize::optimize(raw.clone());
        let (naive_out, naive_res) = interp(&raw, "");
        let (opt_out, opt_res) = interp(&optimized, "");
        naive_res.expect("naive interpreter should succeed");
        opt_res.expect("optimized interpreter should succeed");
        assert_eq!(naive_out, opt_out);
    }

    #[test]
    fn optimized_offset_add_with_untouched_excursion_still_underflows() {
        // +[<>-] and +[-<>]: the loop body visits cell -1 without ever
        // writing it. The naive path dies on the MoveLeft; the optimizer
        // used to fold both shapes to a plain Zero (only written cells
        // were tracked, so every structural check passed), silently
        // swallowing the underflow. The fold must be rejected so the
        // optimized path raises the same error.
        for source in ["+[<>-]", "+[-<>]"] {
            let raw = bfc::parser::parse(source).unwrap();
            let optimized = bfc::optimize::optimize(raw.clone());
            let (_, naive_res) = interp(&raw, "");
            let (_, opt_res) = interp(&optimized, "");
            assert!(
                matches!(naive_res, Err(RunError::PointerUnderflow)),
                "{source}: naive path should underflow"
            );
            assert!(
                matches!(opt_res, Err(RunError::PointerUnderflow)),
                "{source}: optimized path should underflow too"
            );
        }
    }

    #[test]
    fn stdout_receives_every_byte_before_flush() {
        // Echo loop with the zero idiom, driven through a Cursor.
        let source = ",[.[-],]";
        let ops = bfc::parser::parse(source).unwrap();
        let (out, result) = interp(&ops, "hi!");
        assert!(result.is_ok());
        assert_eq!(out, b"hi!");
    }

    #[test]
    fn display_and_conversion_cover_every_variant() {
        // Error wording is pinned byte-for-byte against bfrun (see the
        // module docs: the differential harness compares stderr exactly).
        assert_eq!(
            RunError::PointerUnderflow.to_string(),
            "pointer moved left of cell 0"
        );
        assert_eq!(RunError::GrowFailed.to_string(), "failed to grow tape");
        assert_eq!(
            RunError::TapeLimitExceeded.to_string(),
            "tape limit exceeded (1073741824 bytes)"
        );
        assert_eq!(
            RunError::Io(io::Error::other("bad fd")).to_string(),
            "I/O error: bad fd"
        );
        let converted: RunError = io::Error::new(io::ErrorKind::BrokenPipe, "x").into();
        assert!(matches!(converted, RunError::Io(_)));
    }

    #[test]
    fn mul_add_skips_a_zero_source_cell() {
        // The loop this op replaces never ran when the source was zero, so
        // neither the bounds check nor the write may happen.
        let (out, result) = interp(
            &[
                Op::MulAdd {
                    offset: 1,
                    factor: 9,
                },
                Op::Output,
            ],
            "",
        );
        assert!(result.is_ok());
        assert_eq!(out, vec![0u8]);
    }

    #[test]
    fn mul_add_and_scan_cover_growth_and_underflow() {
        let (_, mul_add_growth) = interp(
            &[
                Op::Add(1),
                Op::MulAdd {
                    offset: INITIAL_TAPE_SIZE as isize,
                    factor: 2,
                },
                Op::MoveRight(INITIAL_TAPE_SIZE as u32),
                Op::Output,
            ],
            "",
        );
        assert!(mul_add_growth.is_ok());

        let (_, scan_growth) = interp(
            &[
                Op::Add(1),
                Op::Scan {
                    stride: INITIAL_TAPE_SIZE as i32,
                },
            ],
            "",
        );
        assert!(scan_growth.is_ok());

        for op in [
            Op::MulAdd {
                offset: -1,
                factor: 1,
            },
            Op::Scan { stride: -1 },
        ] {
            let (_, result) = interp(&[Op::Add(1), op], "");
            assert!(matches!(result, Err(RunError::PointerUnderflow)));
        }
    }

    #[test]
    fn mul_add_grows_the_tape_for_targets_past_the_end() {
        let ops = [
            Op::Add(2),
            Op::MulAdd {
                offset: 40_000,
                factor: 3,
            },
            Op::Zero,
            Op::MoveRight(40_000),
            Op::Output,
        ];
        let (out, result) = interp(&ops, "");
        assert!(result.is_ok());
        assert_eq!(out, vec![6u8]);
    }

    #[test]
    fn mul_add_underflow_errors_like_move_left() {
        let (_, result) = interp(
            &[
                Op::Add(1),
                Op::MulAdd {
                    offset: -1,
                    factor: 1,
                },
            ],
            "",
        );
        assert!(matches!(result, Err(RunError::PointerUnderflow)));
    }

    #[test]
    fn scan_grows_the_tape_when_it_walks_past_the_end() {
        let (out, result) = interp(&[Op::Add(1), Op::Scan { stride: 40_000 }, Op::Output], "");
        assert!(result.is_ok());
        assert_eq!(out, vec![0u8]); // lands on a fresh zero cell
    }

    #[test]
    fn scan_underflow_errors_at_cell_zero() {
        let (_, result) = interp(&[Op::Add(1), Op::Scan { stride: -1 }], "");
        assert!(matches!(result, Err(RunError::PointerUnderflow)));
    }

    #[test]
    fn scan_left_stops_on_a_zero_behind_the_pointer() {
        // Cell 4 nonzero, cell 3 zero: scanning left from 4 must take one
        // step and stop exactly on cell 3.
        let ops = [
            Op::MoveRight(4),
            Op::Add(1),
            Op::Scan { stride: -1 },
            Op::Output,
        ];
        let (out, result) = interp(&ops, "");
        assert!(result.is_ok());
        assert_eq!(out, vec![0u8]);
    }

    #[test]
    fn impossible_growth_is_a_clean_error_not_an_abort() {
        // try_reserve fails instantly with a capacity overflow (no
        // allocation is attempted) once the reservation exceeds isize::MAX
        // bytes — the exact condition the tape cap rejects before resize.
        let mut tape = vec![0u8; INITIAL_TAPE_SIZE];
        let result = grow_to_at_least(&mut tape, usize::MAX - 1);
        assert!(matches!(result, Err(RunError::TapeLimitExceeded)));
        assert_eq!(tape.len(), INITIAL_TAPE_SIZE);
    }

    /// A writer whose `write` always fails — drives the Output op's error
    /// propagation (the `?` on write_all) without needing a real broken
    /// pipe.
    struct FailingWrite;

    impl Write for FailingWrite {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader gone"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader gone"))
        }
    }

    #[test]
    fn a_failing_writer_surfaces_as_an_io_error() {
        let (_, result) = interp(&[Op::Add(1), Op::Output], "");
        // Sanity on the happy path first...
        assert!(result.is_ok());
        let mut out = FailingWrite;
        let result = run(&[Op::Add(1), Op::Output], &mut Cursor::new(b""), &mut out);
        assert!(matches!(result, Err(RunError::Io(_))));
        // The writer's own flush fails the same way.
        assert!(out.flush().is_err());
    }

    /// A writer that accepts bytes into its own buffer but cannot flush —
    /// writes succeed, so execution reaches the final flush, whose failure
    /// must still surface (the `?` on stdout.flush).
    struct Unflushable;

    impl Write for Unflushable {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    #[test]
    fn a_failing_flush_surfaces_as_an_io_error() {
        let mut out = Unflushable;
        let result = run(&[Op::Add(1), Op::Output], &mut Cursor::new(b""), &mut out);
        assert!(matches!(result, Err(RunError::Io(_))));
    }

    /// Test-only allocator that refuses large reallocations on a thread that
    /// asks for it, so tape-growth failures run the same way on every target.
    /// `alloc` stays unchecked: growing the tape never goes through it.
    struct CappedAlloc;

    thread_local! {
        static ALLOC_LIMIT: Cell<usize> = const { Cell::new(usize::MAX) };
    }

    unsafe impl GlobalAlloc for CappedAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            unsafe { System.alloc(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if new_size > ALLOC_LIMIT.get() {
                return std::ptr::null_mut();
            }
            unsafe { System.realloc(ptr, layout, new_size) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CappedAlloc = CappedAlloc;

    fn with_alloc_cap(body: fn()) {
        ALLOC_LIMIT.set(1 << 20);
        body();
        ALLOC_LIMIT.set(usize::MAX);
    }

    #[test]
    fn allocation_failure_through_every_growing_op_is_a_clean_error() {
        // MoveRight, MulAdd's far target, and Scan's long stride all grow
        // the tape; with large allocations refused each must surface
        // GrowFailed cleanly rather than aborting.
        with_alloc_cap(|| {
            let result = run(
                &[Op::MoveRight(u32::MAX)],
                &mut Cursor::new(b""),
                &mut Vec::new(),
            );
            assert!(matches!(result, Err(RunError::TapeLimitExceeded)));

            let result = run(
                &[Op::MoveRight(2_000_000)],
                &mut Cursor::new(b""),
                &mut Vec::new(),
            );
            assert!(matches!(result, Err(RunError::GrowFailed)));

            for op in [
                Op::AddAt {
                    offset: 2_000_000,
                    delta: 1,
                },
                Op::ZeroAt { offset: 2_000_000 },
                Op::SetAt {
                    offset: 2_000_000,
                    value: 1,
                },
                Op::BoundsCheck {
                    min_offset: 0,
                    max_offset: 2_000_000,
                },
            ] {
                let result = run(&[op], &mut Cursor::new(b""), &mut Vec::new());
                assert!(matches!(result, Err(RunError::GrowFailed)), "{op:?}");
            }

            let result = run(
                &[
                    Op::Add(1),
                    Op::MulAdd {
                        offset: 2_000_000,
                        factor: 1,
                    },
                ],
                &mut Cursor::new(b""),
                &mut Vec::new(),
            );
            assert!(matches!(result, Err(RunError::GrowFailed)));

            let result = run(
                &[Op::Add(1), Op::Scan { stride: 2_000_000 }],
                &mut Cursor::new(b""),
                &mut Vec::new(),
            );
            assert!(matches!(result, Err(RunError::GrowFailed)));
        });
    }

    #[test]
    fn bounds_check_covers_the_farther_end_even_when_the_bounds_are_reversed() {
        // The optimizer never emits min_offset > max_offset, but the check
        // has to cover whichever end is farther.
        let reversed = [Op::BoundsCheck {
            min_offset: 40_000,
            max_offset: 0,
        }];
        assert!(interp(&reversed, "").1.is_ok());

        with_alloc_cap(|| {
            let result = run(
                &[Op::BoundsCheck {
                    min_offset: 2_000_000,
                    max_offset: 0,
                }],
                &mut Cursor::new(b""),
                &mut Vec::new(),
            );
            assert!(matches!(result, Err(RunError::GrowFailed)));
        });
    }

    #[test]
    fn zero_at_and_set_at_reject_bad_targets_like_add_at() {
        // AddAt's error paths were tested, but ZeroAt and SetAt each have
        // their own `?` on offset_cell, so they need their own cases.
        for op in [
            Op::ZeroAt { offset: -1 },
            Op::SetAt {
                offset: -1,
                value: 1,
            },
        ] {
            assert!(matches!(
                interp(&[op], "").1,
                Err(RunError::PointerUnderflow)
            ));
        }
        for op in [
            Op::ZeroAt {
                offset: TAPE_LIMIT as isize,
            },
            Op::SetAt {
                offset: TAPE_LIMIT as isize,
                value: 1,
            },
        ] {
            assert!(matches!(
                interp(&[op], "").1,
                Err(RunError::TapeLimitExceeded)
            ));
        }
    }
}
