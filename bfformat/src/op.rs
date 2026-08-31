/// A single bytecode instruction. Repeated runs of `+`, `-`, `>`, `<` in the
/// source get folded into one op with a count, instead of one op per
/// character, so the runtime doesn't spend cycles re-dispatching on the same
/// instruction thousands of times in a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Add(u8),
    Sub(u8),
    MoveRight(u32),
    MoveLeft(u32),
    Output,
    Input,
    /// Jump to `target` (index into the op list) if the current cell is 0.
    JumpIfZero { target: u32 },
    /// Jump to `target` if the current cell is nonzero.
    JumpIfNonZero { target: u32 },
    /// Set the current cell to 0. Replaces the extremely common `[-]` and
    /// `[+]` idiom, which would otherwise burn a full loop iteration per
    /// decrement just to clear one cell.
    Zero,
}

impl Op {
    /// Numeric tag used in the serialized bytecode. Kept separate from the
    /// enum's own discriminant so the on-disk format doesn't silently change
    /// if variants get reordered later.
    pub fn tag(&self) -> u8 {
        match self {
            Op::Add(_) => 0,
            Op::Sub(_) => 1,
            Op::MoveRight(_) => 2,
            Op::MoveLeft(_) => 3,
            Op::Output => 4,
            Op::Input => 5,
            Op::JumpIfZero { .. } => 6,
            Op::JumpIfNonZero { .. } => 7,
            Op::Zero => 8,
        }
    }
}
