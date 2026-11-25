use bfformat::Op;
use std::io;

/// Classic Brainfuck starting tape size. The tape grows past this
/// automatically if a program moves further right than it can currently
/// hold -- unlike the original spec, where that's undefined behavior or a
/// crash depending on the implementation.
const INITIAL_TAPE_SIZE: usize = 30_000;

#[derive(Debug)]
pub enum RunError {
    /// The pointer moved left of cell 0. There's no meaningful way to grow
    /// in that direction, so this is a real error rather than something to
    /// paper over.
    PointerUnderflow,
    /// The tape couldn't be grown (allocation failure). Matches
    /// `bfnative`'s MSG_GROW wording so error-path differential runs keep
    /// comparing equal across backends.
    GrowFailed,
    Io(io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::PointerUnderflow => {
                write!(f, "pointer moved left of cell 0")
            }
            RunError::GrowFailed => write!(f, "failed to grow tape"),
            RunError::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl From<io::Error> for RunError {
    fn from(err: io::Error) -> Self {
        RunError::Io(err)
    }
}

pub struct Vm {
    tape: Vec<u8>,
    pointer: usize,
}

impl Vm {
    pub fn new() -> Self {
        Vm {
            tape: vec![0; INITIAL_TAPE_SIZE],
            pointer: 0,
        }
    }

    /// Runs `ops` against the process's real stdin/stdout. Locks each once
    /// for the whole run so per-op I/O doesn't pay for repeated lock
    /// acquisition.
    pub fn run(&mut self, ops: &[Op]) -> Result<(), RunError> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdin_lock = stdin.lock();
        let mut stdout_lock = stdout.lock();
        self.run_io(ops, &mut stdin_lock, &mut stdout_lock)
    }

    /// The interpreter loop proper, split out from `run` so tests can drive
    /// the I/O arms — input at EOF, a failed write, a failed final flush —
    /// with in-memory readers and writers instead of the process's real
    /// stdio (`bfinterp`'s `run` is generic over its I/O for the same
    /// reason; here plain trait objects keep one monomorphization).
    pub fn run_io(
        &mut self,
        ops: &[Op],
        input: &mut dyn io::Read,
        output: &mut dyn io::Write,
    ) -> Result<(), RunError> {
        let mut pc = 0usize;
        while pc < ops.len() {
            match ops[pc] {
                Op::Add(n) => {
                    let cell = self.current_cell();
                    *cell = cell.wrapping_add(n);
                }
                Op::Sub(n) => {
                    let cell = self.current_cell();
                    *cell = cell.wrapping_sub(n);
                }
                Op::MoveRight(n) => {
                    self.pointer += n as usize;
                    self.grow_if_needed()?;
                }
                Op::MoveLeft(n) => {
                    self.pointer = self
                        .pointer
                        .checked_sub(n as usize)
                        .ok_or(RunError::PointerUnderflow)?;
                }
                Op::Output => {
                    output.write_all(&[*self.current_cell()])?;
                }
                Op::Input => {
                    let mut byte = [0u8];
                    // EOF leaves the cell unchanged, matching the most common
                    // real-world Brainfuck convention (some implementations
                    // set 0 or -1 instead; unchanged is the safest default
                    // since it doesn't invent a value the program didn't ask
                    // for).
                    if input.read_exact(&mut byte).is_ok() {
                        *self.current_cell() = byte[0];
                    }
                }
                Op::JumpIfZero { target } => {
                    if *self.current_cell() == 0 {
                        pc = target as usize;
                    }
                }
                Op::JumpIfNonZero { target } => {
                    if *self.current_cell() != 0 {
                        pc = target as usize;
                    }
                }
                Op::Zero => {
                    *self.current_cell() = 0;
                }
                Op::MulAdd { offset, factor } => {
                    // Does NOT zero the source cell: a spread loop folds
                    // into multiple MulAdds sharing one source cell,
                    // followed by a single trailing Zero op. Zeroing here
                    // too would make every MulAdd after the first in a
                    // group read 0 instead of the real value.
                    let value = *self.current_cell();
                    if value != 0 {
                        let target = self.offset_pointer(offset)?;
                        self.grow_to_at_least(target)?;
                        self.tape[target] =
                            self.tape[target].wrapping_add(value.wrapping_mul(factor));
                    }
                }
                Op::Scan { stride } => {
                    while *self.current_cell() != 0 {
                        self.pointer = self.offset_pointer(stride)?;
                        self.grow_if_needed()?;
                    }
                }
            }
            pc += 1;
        }

        output.flush()?;
        Ok(())
    }

    fn current_cell(&mut self) -> &mut u8 {
        &mut self.tape[self.pointer]
    }

    fn grow_if_needed(&mut self) -> Result<(), RunError> {
        if self.pointer >= self.tape.len() {
            self.grow_to_at_least(self.pointer)?;
        }
        Ok(())
    }

    /// Grows the tape to cover `index`, failing cleanly on allocation
    /// failure instead of aborting the process. A crafted .bfry file can
    /// request a single MoveRight of up to 4 billion cells; an unguarded
    /// `resize` there was a SIGABRT ("memory allocation of N bytes failed")
    /// rather than a runtime error. `try_reserve` first means the resize
    /// itself can no longer fail.
    fn grow_to_at_least(&mut self, index: usize) -> Result<(), RunError> {
        if index >= self.tape.len() {
            let additional = index + 1 - self.tape.len();
            self.tape
                .try_reserve(additional)
                .map_err(|_| RunError::GrowFailed)?;
            self.tape.resize(index + 1, 0);
        }
        Ok(())
    }

    /// Applies a signed offset to the current pointer, erroring on underflow
    /// the same way `MoveLeft` does rather than silently wrapping.
    fn offset_pointer(&self, offset: i32) -> Result<usize, RunError> {
        let new_pointer = self.pointer as i64 + offset as i64;
        if new_pointer < 0 {
            Err(RunError::PointerUnderflow)
        } else {
            Ok(new_pointer as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use io::Write;

    fn run_ops(ops: &[Op]) -> Vm {
        let mut vm = Vm::new();
        vm.run(ops).unwrap();
        vm
    }

    #[test]
    fn add_and_sub_wrap_at_cell_boundary() {
        let mut vm = run_ops(&[Op::Add(255), Op::Add(1)]);
        assert_eq!(*vm.current_cell(), 0);
    }

    #[test]
    fn move_right_grows_tape_past_initial_size() {
        let mut vm = Vm::new();
        vm.run(&[Op::MoveRight(40_000), Op::Add(7)]).unwrap();
        assert_eq!(vm.tape[40_000], 7);
    }

    #[test]
    fn move_left_past_zero_errors() {
        let mut vm = Vm::new();
        let result = vm.run(&[Op::MoveLeft(1)]);
        assert!(matches!(result, Err(RunError::PointerUnderflow)));
    }

    #[test]
    fn move_left_lands_on_the_cell_to_its_left() {
        // The underflow test only exercises checked_sub's failure arm;
        // this one walks a successful left move through the assignment.
        let mut vm = Vm::new();
        vm.run_io(
            &[Op::MoveRight(2), Op::MoveLeft(1), Op::Add(9)],
            &mut io::empty(),
            &mut io::sink(),
        )
        .unwrap();
        assert_eq!(vm.pointer, 1);
        assert_eq!(vm.tape[1], 9);
    }

    #[test]
    fn input_reads_a_byte_and_eof_leaves_the_cell_untouched() {
        // Both halves of the Input arm: a reader with a byte in it (the
        // cell takes that byte) and one at EOF (the cell keeps its value).
        let mut vm = Vm::new();
        vm.run_io(&[Op::Input], &mut io::Cursor::new(&b"Q"[..]), &mut io::sink())
            .unwrap();
        assert_eq!(*vm.current_cell(), b'Q');

        let mut vm = Vm::new();
        vm.run_io(
            &[Op::Add(5), Op::Input],
            &mut io::Cursor::new(&b""[..]),
            &mut io::sink(),
        )
        .unwrap();
        assert_eq!(*vm.current_cell(), 5);
    }

    /// A writer whose `write` always fails — drives the Output op's error
    /// propagation (the `?` on write_all) without needing a real broken
    /// pipe. Its flush fails the same way.
    struct FailingWrite;

    impl io::Write for FailingWrite {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader gone"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "reader gone"))
        }
    }

    #[test]
    fn a_failing_write_surfaces_as_an_io_error() {
        let mut vm = Vm::new();
        let mut out = FailingWrite;
        let result = vm.run_io(&[Op::Add(1), Op::Output], &mut io::empty(), &mut out);
        assert!(matches!(result, Err(RunError::Io(_))));
        // The write error aborts the program before the final flush, so
        // exercise the helper's failing flush directly as well.
        assert!(out.flush().is_err());
    }

    /// A writer that accepts bytes but cannot flush — writes succeed, so
    /// execution reaches the final flush, whose failure must still surface
    /// (the `?` on output.flush).
    struct Unflushable;

    impl io::Write for Unflushable {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(io::ErrorKind::Other, "disk full"))
        }
    }

    #[test]
    fn a_failing_flush_surfaces_as_an_io_error() {
        let mut vm = Vm::new();
        let result = vm.run_io(&[Op::Add(1), Op::Output], &mut io::empty(), &mut Unflushable);
        assert!(matches!(result, Err(RunError::Io(_))));
    }

    #[test]
    fn zero_clears_current_cell() {
        let mut vm = run_ops(&[Op::Add(9), Op::Zero]);
        assert_eq!(*vm.current_cell(), 0);
    }

    #[test]
    fn grow_failed_message_matches_bfnative_wording() {
        // bfnative's MSG_GROW is "runtime error: failed to grow tape\n" and
        // bfrun's main prints "runtime error: {err}" -- the differential
        // harness compares stderr byte for byte, so the two must line up.
        assert_eq!(
            RunError::GrowFailed.to_string(),
            "failed to grow tape"
        );
    }

    #[test]
    fn display_and_conversion_cover_every_variant() {
        // Underflow wording is likewise pinned against bfnative's
        // MSG_UNDERFLOW; the Io arm wraps the underlying error's text.
        assert_eq!(
            RunError::PointerUnderflow.to_string(),
            "pointer moved left of cell 0"
        );
        assert_eq!(
            RunError::Io(io::Error::new(io::ErrorKind::Other, "pipe broke"))
                .to_string(),
            "I/O error: pipe broke"
        );
        let converted: RunError = io::Error::new(io::ErrorKind::UnexpectedEof, "eof").into();
        assert!(matches!(converted, RunError::Io(_)));
    }

    #[test]
    fn sub_wraps_below_zero() {
        let mut vm = run_ops(&[Op::Sub(1), Op::Sub(1)]);
        assert_eq!(*vm.current_cell(), 254);
    }

    #[test]
    fn output_runs_the_full_write_path() {
        // stdout is captured by the harness; what matters here is that the
        // Output arm executes its write and the run reports success. Byte
        // correctness is pinned end-to-end by tests/e2e.rs.
        let mut vm = Vm::new();
        let result = vm.run(&[Op::Add(65), Op::Output]);
        assert!(result.is_ok());
        assert_eq!(*vm.current_cell(), 65);
    }

    #[test]
    fn jump_pairs_form_a_working_loop() {
        // +[-] with real jumps: the JZ skips the loop when zero, the JNZ
        // walks back while nonzero -- the naive-loop shape the optimizer
        // would fold to Zero, driven here op by op.
        let ops = [
            Op::Add(3),
            Op::JumpIfZero { target: 3 }, // cell zero -> land on op 4 (Add)
            Op::Sub(1),
            Op::JumpIfNonZero { target: 1 }, // nonzero -> land on op 2 (Sub)
            Op::Add(1),
            Op::Output,
        ];
        let mut vm = Vm::new();
        let result = vm.run(&ops);
        assert!(result.is_ok());
        assert_eq!(*vm.current_cell(), 1);
    }

    #[test]
    fn mul_add_spreads_value_then_zero_clears() {
        // [->++<] followed by the trailing Zero: cell 1 gets 2 * 5.
        let mut vm = run_ops(&[
            Op::Add(5),
            Op::MulAdd {
                offset: 1,
                factor: 2,
            },
            Op::Zero,
            Op::MoveRight(1),
        ]);
        assert_eq!(*vm.current_cell(), 10);
        assert_eq!(vm.tape[0], 0);
    }

    #[test]
    fn mul_add_skips_when_the_source_cell_is_zero() {
        // The loop MulAdd replaces never ran on a zero source, so the write
        // (and the bounds check on a possibly-invalid target) must be
        // skipped entirely.
        let mut vm = run_ops(&[
            Op::MulAdd {
                offset: 1,
                factor: 7,
            },
            Op::MoveRight(1),
        ]);
        assert_eq!(*vm.current_cell(), 0);
    }

    #[test]
    fn mul_add_grows_the_tape_for_far_targets() {
        let mut vm = run_ops(&[
            Op::Add(2),
            Op::MulAdd {
                offset: 40_000,
                factor: 3,
            },
            Op::MoveRight(40_000),
        ]);
        assert_eq!(*vm.current_cell(), 6);
    }

    #[test]
    fn mul_add_underflow_errors_like_move_left() {
        let mut vm = Vm::new();
        let result = vm.run(&[Op::Add(1), Op::MulAdd { offset: -1, factor: 1 }]);
        assert!(matches!(result, Err(RunError::PointerUnderflow)));
    }

    #[test]
    fn scan_right_stops_on_the_next_zero_cell() {
        let vm = run_ops(&[Op::Add(1), Op::Scan { stride: 1 }]);
        assert_eq!(vm.pointer, 1);
    }

    #[test]
    fn scan_left_underflows_at_cell_zero() {
        let mut vm = Vm::new();
        let result = vm.run(&[Op::Add(1), Op::Scan { stride: -1 }]);
        assert!(matches!(result, Err(RunError::PointerUnderflow)));
    }

    #[test]
    fn scan_grows_the_tape_when_it_walks_past_the_end() {
        // One nonzero cell, then a stride that lands past 30,000: the scan
        // both grows the tape and finds the zero at the landing spot.
        let vm = run_ops(&[Op::Add(1), Op::Scan { stride: 40_000 }]);
        assert_eq!(vm.pointer, 40_000);
        assert!(vm.tape.len() > 40_000);
    }

    #[test]
    fn jump_taken_zero_skips_the_loop_body() {
        // Cell 0 is zero, so JumpIfZero fires: execution lands on op 3
        // (the pc = target; pc += 1 convention), skipping the Add entirely.
        let ops = [
            Op::JumpIfZero { target: 2 },
            Op::Add(1),
            Op::Output,
        ];
        let mut vm = Vm::new();
        let result = vm.run(&ops);
        assert!(result.is_ok());
        assert_eq!(*vm.current_cell(), 0);
    }

    /// Caps RLIMIT_AS for the duration of a closure, restoring it after —
    /// the one deterministic, dependency-free way to make real allocation
    /// fail in-process (a 4 GiB tape growth cannot fit under a 512 MiB
    /// address-space cap, so try_reserve reports failure without any
    /// multi-gigabyte attempt; the cap stays high enough that sibling
    /// tests running in parallel keep their normal stack and heap room). Linux x86-64 only: other hosts skip the
    /// allocation-failure tests entirely.
    // coverage(off) (nightly coverage runs only; the attribute is unstable,
    // so it is cfg-gated): the `get_ret == 0` guard below is the defensive
    // half of a syscall-error check that cannot be made to fail in any
    // real process, so its false branch is uninstrumentable by
    // construction. This is test-support code, not product code.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn with_address_space_cap<T>(body: impl FnOnce() -> T) -> T {
        // getrlimit/setrlimit syscall numbers (x86-64) and RLIMIT_AS.
        const GETRLIMIT: u64 = 97;
        const SETRLIMIT: u64 = 160;
        const RLIMIT_AS: u64 = 9;
        // (rlim_cur, rlim_max). RLIM_INFINITY on Linux x86-64 is ~0u64.
        let mut old: (u64, u64) = (u64::MAX, u64::MAX);
        unsafe {
            let get_ret: u64;
            core::arch::asm!(
                "syscall",
                inlateout("rax") GETRLIMIT => get_ret,
                in("rdi") RLIMIT_AS,
                in("rsi") &mut old as *mut (u64, u64) as u64,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
            // If the syscall itself failed, `old` is untouched (still
            // RLIM_INFINITY) and capping is a safe no-op skip below.
            if get_ret == 0 {
                // Cap at 64 MiB (never above the existing hard limit); the
                // process is already mapped, so only *new* large
                // reservations fail — exactly the failure mode under test.
                let cap = 512u64 << 20;
                let tight = (cap.min(old.1), old.1);
                core::arch::asm!(
                    "syscall",
                    inlateout("rax") SETRLIMIT => _,
                    in("rdi") RLIMIT_AS,
                    in("rsi") &tight as *const (u64, u64) as u64,
                    lateout("rcx") _,
                    lateout("r11") _,
                    options(nostack),
                );
            }
        }
        let result = body();
        unsafe {
            core::arch::asm!(
                "syscall",
                inlateout("rax") SETRLIMIT => _,
                in("rdi") RLIMIT_AS,
                in("rsi") &old as *const (u64, u64) as u64,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
        result
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn allocation_failure_through_every_growing_op_is_a_clean_error() {
        // MoveRight, MulAdd's far target, and Scan's long stride all grow
        // the tape; under a capped address space each must surface
        // GrowFailed (mapped to bfnative's exact wording) rather than
        // aborting — the hardening the try_reserve guard exists for.
        with_address_space_cap(|| {
            let mut vm = Vm::new();
            let result = vm.run(&[Op::MoveRight(u32::MAX)]);
            assert!(matches!(result, Err(RunError::GrowFailed)));

            let mut vm = Vm::new();
            let result = vm.run(&[
                Op::Add(1),
                Op::MulAdd {
                    offset: 2_000_000_000,
                    factor: 1,
                },
            ]);
            assert!(matches!(result, Err(RunError::GrowFailed)));

            let mut vm = Vm::new();
            let result = vm.run(&[Op::Add(1), Op::Scan { stride: 2_000_000_000 }]);
            assert!(matches!(result, Err(RunError::GrowFailed)));
        });
    }
}