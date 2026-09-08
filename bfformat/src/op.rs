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
    JumpIfZero {
        target: u32,
    },
    /// Jump to `target` if the current cell is nonzero.
    JumpIfNonZero {
        target: u32,
    },
    /// Set the current cell to 0. Replaces the extremely common `[-]` and
    /// `[+]` idiom, which would otherwise burn a full loop iteration per
    /// decrement just to clear one cell.
    Zero,
    /// Add the current cell's value, scaled by `factor`, into the cell at
    /// `offset` from the current position, then zero the current cell.
    /// Replaces loops like `[->+<]` or `[->++<]`, which are really just
    /// "multiply this value and dump it over there" written as a decrement
    /// loop. `offset` is signed since the target cell can be on either side
    /// of the tape head; `factor` is a u8 because the loop body can only add
    /// a bounded amount per iteration before folding would itself change
    /// behavior (see optimize.rs for the exact conditions).
    MulAdd {
        offset: i32,
        factor: u8,
    },
    /// Step the tape head by `stride` cells at a time until landing on a
    /// zero cell. Replaces loops like `[>]` or `[<<]`, which walk the tape
    /// looking for a boundary and do nothing else per iteration -- there's
    /// no reason to pay for a jump-check-jump per cell when the loop body
    /// is just "move." `stride` is signed: positive scans right, negative
    /// scans left.
    Scan {
        stride: i32,
    },
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
            Op::MulAdd { .. } => 9,
            Op::Scan { .. } => 10,
        }
    }
}
