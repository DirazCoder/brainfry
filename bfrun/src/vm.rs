use bfformat::Op;
use std::io::{self, Read, Write};

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
    Io(io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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

    pub fn run(&mut self, ops: &[Op]) -> Result<(), RunError> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdin_lock = stdin.lock();
        let mut stdout_lock = stdout.lock();

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
                    self.grow_if_needed();
                }
                Op::MoveLeft(n) => {
                    self.pointer = self
                        .pointer
                        .checked_sub(n as usize)
                        .ok_or(RunError::PointerUnderflow)?;
                }
                Op::Output => {
                    stdout_lock.write_all(&[*self.current_cell()])?;
                }
                Op::Input => {
                    let mut byte = [0u8];
                    // EOF leaves the cell unchanged, matching the most common
                    // real-world Brainfuck convention (some implementations
                    // set 0 or -1 instead; unchanged is the safest default
                    // since it doesn't invent a value the program didn't ask
                    // for).
                    if stdin_lock.read_exact(&mut byte).is_ok() {
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
            }
            pc += 1;
        }

        stdout_lock.flush()?;
        Ok(())
    }

    fn current_cell(&mut self) -> &mut u8 {
        &mut self.tape[self.pointer]
    }

    fn grow_if_needed(&mut self) {
        if self.pointer >= self.tape.len() {
            self.tape.resize(self.pointer + 1, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn zero_clears_current_cell() {
        let mut vm = run_ops(&[Op::Add(9), Op::Zero]);
        assert_eq!(*vm.current_cell(), 0);
    }
}
