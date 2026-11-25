mod op;

pub use op::Op;

use std::io::{self, Read, Write};

/// First 4 bytes of every compiled file. Lets the runtime reject non-bfry
/// files immediately instead of failing confusingly deep in the parser.
pub const MAGIC: [u8; 4] = *b"BFRY";

/// Bumped whenever the on-disk layout changes in a way old runtimes can't
/// read. The runtime checks this and refuses mismatched files rather than
/// guessing at a format it doesn't understand.
pub const FORMAT_VERSION: u8 = 1;

/// Upper bound on how many ops `read_from` pre-allocates before reading
/// the body. The file's declared op count is attacker-controlled input:
/// trusting it for `Vec::with_capacity` let a 9-byte file with
/// count = 0xFFFFFFFF abort the process on a 34 GB allocation attempt
/// before the body (which wasn't there) could even report `Truncated`.
/// Programs bigger than this still load fine — the Vec just grows
/// amortized as ops actually stream in, exactly as if the count were
/// unknown.
const MAX_PREALLOC_OPS: usize = 64 * 1024;

#[derive(Debug)]
pub enum FormatError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u8),
    Truncated,
    UnknownOpTag(u8),
    /// A jump op's target index falls outside the instruction list.
    InvalidJumpTarget { target: u32, len: u32 },
    /// A `JumpIfZero`/`JumpIfNonZero` pair doesn't point at each other.
    /// The compiler only ever emits mutually paired brackets, so anything
    /// else is a corrupt or hand-crafted file, not a program variant.
    UnbalancedJumps { op_index: u32 },
}

impl From<io::Error> for FormatError {
    fn from(err: io::Error) -> Self {
        FormatError::Io(err)
    }
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Io(err) => write!(f, "I/O error: {err}"),
            FormatError::BadMagic => write!(f, "not a bfry bytecode file"),
            FormatError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "bytecode format version {v} is not supported by this runtime"
                )
            }
            FormatError::Truncated => write!(f, "bytecode file is truncated or corrupt"),
            FormatError::UnknownOpTag(t) => write!(f, "unknown instruction tag {t} in bytecode"),
            FormatError::InvalidJumpTarget { target, len } => write!(
                f,
                "jump target {target} is out of range (program has {len} instructions)"
            ),
            FormatError::UnbalancedJumps { op_index } => write!(
                f,
                "jump at instruction {op_index} does not pair with a matching bracket"
            ),
        }
    }
}

impl std::error::Error for FormatError {}

/// A compiled program: just the resolved instruction list. Jump targets are
/// already resolved to indices by the compiler, so the runtime never has to
/// scan for matching brackets.
#[derive(Debug)]
pub struct Program {
    pub ops: Vec<Op>,
}

impl Program {
    pub fn write_to<W: Write>(&self, mut out: W) -> io::Result<()> {
        out.write_all(&MAGIC)?;
        out.write_all(&[FORMAT_VERSION])?;
        out.write_all(&(self.ops.len() as u32).to_le_bytes())?;

        for op in &self.ops {
            out.write_all(&[op.tag()])?;
            match op {
                Op::Add(n) | Op::Sub(n) => out.write_all(&[*n])?,
                Op::MoveRight(n) | Op::MoveLeft(n) => out.write_all(&n.to_le_bytes())?,
                Op::JumpIfZero { target } | Op::JumpIfNonZero { target } => {
                    out.write_all(&target.to_le_bytes())?
                }
                Op::Output | Op::Input | Op::Zero => {}
                Op::MulAdd { offset, factor } => {
                    out.write_all(&offset.to_le_bytes())?;
                    out.write_all(&[*factor])?;
                }
                Op::Scan { stride } => out.write_all(&stride.to_le_bytes())?,
            }
        }

        Ok(())
    }

    pub fn read_from<R: Read>(mut input: R) -> Result<Self, FormatError> {
        let mut magic = [0u8; 4];
        input
            .read_exact(&mut magic)
            .map_err(|_| FormatError::Truncated)?;
        if magic != MAGIC {
            return Err(FormatError::BadMagic);
        }

        let mut version = [0u8; 1];
        input
            .read_exact(&mut version)
            .map_err(|_| FormatError::Truncated)?;
        if version[0] != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion(version[0]));
        }

        let mut count_bytes = [0u8; 4];
        input
            .read_exact(&mut count_bytes)
            .map_err(|_| FormatError::Truncated)?;
        let count = u32::from_le_bytes(count_bytes) as usize;

        let mut ops = Vec::with_capacity(count.min(MAX_PREALLOC_OPS));
        for _ in 0..count {
            ops.push(read_op(&mut input)?);
        }

        validate_jumps(&ops)?;

        Ok(Program { ops })
    }
}

/// Rejects jump ops whose target is out of range or whose bracket pair
/// doesn't point back at them. `bfc` (the only writer of this format)
/// always emits mutually paired, in-range jumps, so a violation means a
/// corrupt or hand-crafted file — and previously such a file wasn't just
/// accepted, it *silently misexecuted*: an out-of-range target makes the
/// VM's `pc < ops.len()` loop condition false on the next tick, so the
/// program quietly "finishes" with exit 0 instead of erroring.
fn validate_jumps(ops: &[Op]) -> Result<(), FormatError> {
    let len = ops.len() as u32;
    for (i, op) in ops.iter().enumerate() {
        let is_open = matches!(op, Op::JumpIfZero { .. });
        let target = match op {
            Op::JumpIfZero { target } | Op::JumpIfNonZero { target } => *target,
            _ => continue,
        };

        if target >= len {
            return Err(FormatError::InvalidJumpTarget { target, len });
        }

        // An opening bracket (JumpIfZero) must be closed by the
        // JumpIfNonZero at its target, and that close must point back —
        // and vice versa. The two kinds never pair with their own kind.
        let back = match (is_open, &ops[target as usize]) {
            (true, Op::JumpIfNonZero { target: back })
            | (false, Op::JumpIfZero { target: back }) => *back,
            _ => {
                return Err(FormatError::UnbalancedJumps {
                    op_index: i as u32,
                })
            }
        };
        if back as usize != i {
            return Err(FormatError::UnbalancedJumps {
                op_index: i as u32,
            });
        }
    }
    Ok(())
}

fn read_op<R: Read>(input: &mut R) -> Result<Op, FormatError> {
    let mut tag = [0u8; 1];
    input
        .read_exact(&mut tag)
        .map_err(|_| FormatError::Truncated)?;

    let op = match tag[0] {
        0 => Op::Add(read_u8(input)?),
        1 => Op::Sub(read_u8(input)?),
        2 => Op::MoveRight(read_u32(input)?),
        3 => Op::MoveLeft(read_u32(input)?),
        4 => Op::Output,
        5 => Op::Input,
        6 => Op::JumpIfZero {
            target: read_u32(input)?,
        },
        7 => Op::JumpIfNonZero {
            target: read_u32(input)?,
        },
        8 => Op::Zero,
        9 => Op::MulAdd {
            offset: read_i32(input)?,
            factor: read_u8(input)?,
        },
        10 => Op::Scan {
            stride: read_i32(input)?,
        },
        other => return Err(FormatError::UnknownOpTag(other)),
    };

    Ok(op)
}

fn read_u8<R: Read>(input: &mut R) -> Result<u8, FormatError> {
    let mut buf = [0u8; 1];
    input
        .read_exact(&mut buf)
        .map_err(|_| FormatError::Truncated)?;
    Ok(buf[0])
}

fn read_u32<R: Read>(input: &mut R) -> Result<u32, FormatError> {
    let mut buf = [0u8; 4];
    input
        .read_exact(&mut buf)
        .map_err(|_| FormatError::Truncated)?;
    Ok(u32::from_le_bytes(buf))
}

fn read_i32<R: Read>(input: &mut R) -> Result<i32, FormatError> {
    let mut buf = [0u8; 4];
    input
        .read_exact(&mut buf)
        .map_err(|_| FormatError::Truncated)?;
    Ok(i32::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_bytes() {
        let program = Program {
            ops: vec![
                Op::Add(5),
                Op::MoveRight(3),
                // A mutually-paired bracket (JZ -> JNZ -> back), the only
                // shape the loader's jump validation accepts -- and the
                // only shape bfc ever writes.
                Op::JumpIfZero { target: 4 },
                Op::Output,
                Op::JumpIfNonZero { target: 2 },
                Op::Zero,
                Op::MulAdd {
                    offset: 2,
                    factor: 3,
                },
                Op::Scan { stride: 1 },
            ],
        };

        let mut bytes = Vec::new();
        program.write_to(&mut bytes).unwrap();

        let restored = Program::read_from(&bytes[..]).unwrap();
        assert_eq!(restored.ops, program.ops);
    }

    #[test]
    fn round_trips_negative_offset_and_stride() {
        // offset/stride are the first signed fields in the format -- the
        // original round-trip test only used positive values, which
        // wouldn't have caught a sign-extension bug in read_i32/to_le_bytes.
        let program = Program {
            ops: vec![
                Op::MulAdd {
                    offset: -7,
                    factor: 255,
                },
                Op::Scan { stride: -3 },
            ],
        };

        let mut bytes = Vec::new();
        program.write_to(&mut bytes).unwrap();

        let restored = Program::read_from(&bytes[..]).unwrap();
        assert_eq!(restored.ops, program.ops);
    }

    #[test]
    fn rejects_bad_magic() {
        let bytes = b"NOPE".to_vec();
        match Program::read_from(&bytes[..]) {
            Err(FormatError::BadMagic) => {}
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn rejects_future_version() {
        let mut bytes = MAGIC.to_vec();
        bytes.push(FORMAT_VERSION + 1);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        match Program::read_from(&bytes[..]) {
            Err(FormatError::UnsupportedVersion(v)) => assert_eq!(v, FORMAT_VERSION + 1),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn rejects_huge_declared_count_without_allocating_it() {
        // A 9-byte file whose header claims 4 billion ops used to abort the
        // process on a ~34 GB Vec::with_capacity before the missing body
        // could report Truncated. The loader must pre-reserve a bounded
        // amount and let the actual (missing) body drive the error.
        let bytes = {
            let mut bytes = MAGIC.to_vec();
            bytes.push(FORMAT_VERSION);
            bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
            bytes
        };
        match Program::read_from(&bytes[..]) {
            Err(FormatError::Truncated) => {}
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn loads_a_program_larger_than_the_pre_allocation_cap() {
        // More ops than MAX_PREALLOC_OPS, so read_from's bounded
        // pre-reservation has to grow dynamically. Both plain ops and a
        // well-formed bracket pair are included so validation also runs
        // over a >cap program.
        let mut ops: Vec<Op> = (0..70_000)
            .map(|i| if i % 2 == 0 { Op::Add(1) } else { Op::MoveRight(1) })
            .collect();
        ops.push(Op::JumpIfZero { target: 70_002 });
        ops.push(Op::Sub(1));
        ops.push(Op::JumpIfNonZero { target: 70_000 });

        let program = Program { ops };
        let mut bytes = Vec::new();
        program.write_to(&mut bytes).unwrap();

        let restored = Program::read_from(&bytes[..]).unwrap();
        assert_eq!(restored.ops.len(), program.ops.len());
        assert_eq!(restored.ops, program.ops);
    }

    #[test]
    fn rejects_jump_target_out_of_range() {
        let program = Program {
            ops: vec![Op::Add(1), Op::JumpIfZero { target: 99 }],
        };
        let mut bytes = Vec::new();
        program.write_to(&mut bytes).unwrap();

        match Program::read_from(&bytes[..]) {
            Err(FormatError::InvalidJumpTarget { target, len }) => {
                assert_eq!((target, len), (99, 2));
            }
            other => panic!("expected InvalidJumpTarget, got {other:?}"),
        }
    }

    #[test]
    fn rejects_jump_targeting_a_non_bracket_op() {
        // The JumpIfZero points at an Add instead of its closing
        // JumpIfNonZero -- corrupt pairing must not load.
        let program = Program {
            ops: vec![
                Op::JumpIfZero { target: 1 },
                Op::Add(1),
                Op::JumpIfNonZero { target: 0 },
            ],
        };
        let mut bytes = Vec::new();
        program.write_to(&mut bytes).unwrap();

        match Program::read_from(&bytes[..]) {
            Err(FormatError::UnbalancedJumps { op_index }) => assert_eq!(op_index, 0),
            other => panic!("expected UnbalancedJumps, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bracket_pair_that_does_not_point_back() {
        // Both brackets are in range and of the opposite kind, but the
        // close doesn't point back at the open.
        let program = Program {
            ops: vec![
                Op::Add(1),
                Op::JumpIfZero { target: 2 },
                Op::JumpIfNonZero { target: 2 }, // should be 1
            ],
        };
        let mut bytes = Vec::new();
        program.write_to(&mut bytes).unwrap();

        match Program::read_from(&bytes[..]) {
            Err(FormatError::UnbalancedJumps { .. }) => {}
            other => panic!("expected UnbalancedJumps, got {other:?}"),
        }
    }
}
