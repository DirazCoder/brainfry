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

#[derive(Debug)]
pub enum RunError {
    /// The pointer moved left of cell 0. No way to grow in that direction,
    /// so it's a real error — same as `bfrun`.
    PointerUnderflow,
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
    run(ops, stdin.lock(), stdout.lock())
}

/// The interpreter proper. Generic over I/O so the unit tests can drive it
/// with `Cursor`s; the semantics are those described in the module docs.
pub fn run<R: Read, W: Write>(ops: &[Op], mut stdin: R, mut stdout: W) -> Result<(), RunError> {
    let mut tape = vec![0u8; INITIAL_TAPE_SIZE];
    let mut cell = 0usize;
    let mut pc = 0usize;

    while pc < ops.len() {
        match ops[pc] {
            Op::Add(n) => tape[cell] = tape[cell].wrapping_add(n),
            Op::Sub(n) => tape[cell] = tape[cell].wrapping_sub(n),
            Op::MoveRight(n) => {
                cell += n as usize;
                // Grow to exactly the cell we just landed on — bfrun's
                // grow-if-needed policy, not a doubling scheme.
                if cell >= tape.len() {
                    tape.resize(cell + 1, 0);
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
            Op::MulAdd { offset, factor } => {
                let value = tape[cell];
                if value != 0 {
                    let target = offset_cell(cell, offset)?;
                    if target >= tape.len() {
                        tape.resize(target + 1, 0);
                    }
                    tape[target] = tape[target].wrapping_add(value.wrapping_mul(factor));
                }
                tape[cell] = 0;
            }
            Op::Scan { stride } => {
                while tape[cell] != 0 {
                    cell = offset_cell(cell, stride)?;
                    if cell >= tape.len() {
                        tape.resize(cell + 1, 0);
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
fn offset_cell(cell: usize, offset: i32) -> Result<usize, RunError> {
    let new_cell = cell as i64 + offset as i64;
    if new_cell < 0 {
        Err(RunError::PointerUnderflow)
    } else {
        Ok(new_cell as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn interp(ops: &[Op], input: &str) -> (Vec<u8>, Result<(), RunError>) {
        let mut out = Vec::new();
        let result = run(ops, Cursor::new(input.as_bytes()), &mut out);
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
    fn jump_lands_one_past_the_target() {
        // +[-]+. — enter the loop, clear, exit, set 1, print.
        let source = "+[-]+.";
        let ops = bfc::parser::parse(source).unwrap();
        let (out, result) = interp(&ops, "");
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
        assert!(naive_res.is_ok() && opt_res.is_ok());
        assert_eq!(naive_out, opt_out);
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
}
