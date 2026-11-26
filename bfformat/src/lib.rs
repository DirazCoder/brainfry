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
    InvalidJumpTarget {
        target: u32,
        len: u32,
    },
    /// A `JumpIfZero`/`JumpIfNonZero` pair doesn't point at each other.
    /// The compiler only ever emits mutually paired brackets, so anything
    /// else is a corrupt or hand-crafted file, not a program variant.
    UnbalancedJumps {
        op_index: u32,
    },
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
            _ => return Err(FormatError::UnbalancedJumps { op_index: i as u32 }),
        };
        if back as usize != i {
            return Err(FormatError::UnbalancedJumps { op_index: i as u32 });
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
                Op::Sub(3),
                Op::MoveRight(3),
                Op::MoveLeft(1),
                Op::Input,
                // A mutually-paired bracket (JZ -> JNZ -> back), the only
                // shape the loader's jump validation accepts -- and the
                // only shape bfc ever writes.
                Op::JumpIfZero { target: 7 },
                Op::Output,
                Op::JumpIfNonZero { target: 5 },
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
        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::BadMagic));
        assert_eq!(err.to_string(), "not a bfry bytecode file");
    }

    #[test]
    fn rejects_future_version() {
        let mut bytes = MAGIC.to_vec();
        bytes.push(FORMAT_VERSION + 1);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::UnsupportedVersion(_)));
        assert_eq!(
            err.to_string(),
            "bytecode format version 2 is not supported by this runtime"
        );
    }

    #[test]
    fn io_errors_convert_into_format_errors() {
        let err: FormatError = io::Error::other("disk fell off").into();
        assert!(matches!(err, FormatError::Io(_)));
        assert_eq!(err.to_string(), "I/O error: disk fell off");
    }

    #[test]
    fn display_names_every_variant_specifically() {
        // Error wording is load-bearing: bfrun prints `couldn't load {path}:
        // {err}`, and the differential harness compares stderr byte for
        // byte, so each variant's exact text is pinned here.
        let cases = [
            (
                FormatError::BadMagic,
                "not a bfry bytecode file".to_string(),
            ),
            (
                FormatError::UnsupportedVersion(7),
                "bytecode format version 7 is not supported by this runtime".to_string(),
            ),
            (
                FormatError::Truncated,
                "bytecode file is truncated or corrupt".to_string(),
            ),
            (
                FormatError::UnknownOpTag(11),
                "unknown instruction tag 11 in bytecode".to_string(),
            ),
            (
                FormatError::InvalidJumpTarget { target: 9, len: 3 },
                "jump target 9 is out of range (program has 3 instructions)".to_string(),
            ),
            (
                FormatError::UnbalancedJumps { op_index: 4 },
                "jump at instruction 4 does not pair with a matching bracket".to_string(),
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(err.to_string(), expected, "{err:?}");
        }
    }

    #[test]
    fn empty_input_is_truncated_not_bad_magic() {
        // Zero bytes can't even satisfy the magic read, which must report
        // Truncated rather than misreading garbage as a magic mismatch.
        let err = Program::read_from(&[][..]).unwrap_err();
        assert!(matches!(err, FormatError::Truncated));
    }

    #[test]
    fn magic_without_a_version_byte_is_truncated() {
        let err = Program::read_from(&MAGIC[..]).unwrap_err();
        assert!(matches!(err, FormatError::Truncated));
    }

    #[test]
    fn header_without_the_count_is_truncated() {
        let mut bytes = MAGIC.to_vec();
        bytes.push(FORMAT_VERSION);
        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::Truncated));
    }

    #[test]
    fn rejects_unknown_op_tag() {
        let mut bytes = MAGIC.to_vec();
        bytes.push(FORMAT_VERSION);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(11); // no tag 11 exists
        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::UnknownOpTag(11)));
    }

    #[test]
    fn rejects_truncated_op_operands() {
        fn with_body(tag: u8) -> Vec<u8> {
            let mut bytes = MAGIC.to_vec();
            bytes.push(FORMAT_VERSION);
            bytes.extend_from_slice(&1u32.to_le_bytes());
            bytes.push(tag);
            bytes
        }
        // Every operand-carrying tag, each with its operand missing: Add's
        // u8, Sub's u8, MoveRight's u32, MoveLeft's u32, both jump targets'
        // u32, MulAdd's i32, and Scan's i32.
        for tag in [0u8, 1, 2, 3, 6, 7, 9, 10] {
            let err = Program::read_from(&with_body(tag)[..]).unwrap_err();
            assert!(matches!(err, FormatError::Truncated), "tag {tag}: {err:?}");
        }
    }

    #[test]
    fn mul_add_with_offset_but_no_factor_is_truncated() {
        // The operand-sweep test below truncates MulAdd after the tag, so
        // only the offset read ever fails there; this file carries the
        // full i32 offset and stops, driving the *factor* read's error
        // path as well.
        let mut bytes = MAGIC.to_vec();
        bytes.push(FORMAT_VERSION);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.push(9); // MulAdd
        bytes.extend_from_slice(&0i32.to_le_bytes()); // offset present
        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::Truncated));
    }

    /// A writer that succeeds for the first `n` write calls and fails
    /// every one after — sweeping `n` across a full program drives the
    /// error propagation of every `?` in `write_to`, not just the first.
    struct FailAfter {
        remaining_ok: usize,
    }

    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining_ok == 0 {
                Err(io::Error::other("planned failure"))
            } else {
                self.remaining_ok -= 1;
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn write_to_propagates_a_failure_at_every_call_site() {
        // One op of every operand shape, so the write sequence touches all
        // nine write sites: magic, version, count, and each operand kind.
        let program = Program {
            ops: vec![
                Op::Add(1),
                Op::Sub(1),
                Op::MoveRight(1),
                Op::MoveLeft(1),
                Op::Output,
                Op::Input,
                Op::JumpIfZero { target: 7 },
                Op::JumpIfNonZero { target: 6 },
                Op::Zero,
                Op::MulAdd {
                    offset: -1,
                    factor: 2,
                },
                Op::Scan { stride: -1 },
            ],
        };

        // 4 header writes + 11 tag bytes + 8 operands = 23 write calls;
        // failing at each position in turn covers every `?` (the n =
        // total_writes case is the all-succeed sanity below).
        let total_writes = 4 + 11 + 8;
        for n in 0..total_writes {
            let mut sink = FailAfter { remaining_ok: n };
            assert!(
                program.write_to(&mut sink).is_err(),
                "write_to must fail when write #{} fails",
                n + 1
            );
            // The helper implements the whole Write contract; flushing the
            // failed sink keeps that half of the impl exercised too.
            assert!(sink.flush().is_ok());
        }
        // And the same helper with capacity for every write completes:
        // the sweep only ever saw FailAfter failing, so its success
        // return was never taken.
        let mut sink = FailAfter {
            remaining_ok: total_writes,
        };
        assert!(program.write_to(&mut sink).is_ok());
        // And a writer with enough capacity succeeds, proving the sweep
        // wasn't passing on a broken program (and flush runs too).
        let mut ok = Vec::new();
        assert!(program.write_to(&mut ok).is_ok());
        assert!(ok.flush().is_ok());
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
        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::Truncated));
    }

    #[test]
    fn loads_a_program_larger_than_the_pre_allocation_cap() {
        // More ops than MAX_PREALLOC_OPS, so read_from's bounded
        // pre-reservation has to grow dynamically. Both plain ops and a
        // well-formed bracket pair are included so validation also runs
        // over a >cap program.
        let mut ops: Vec<Op> = (0..70_000)
            .map(|i| {
                if i % 2 == 0 {
                    Op::Add(1)
                } else {
                    Op::MoveRight(1)
                }
            })
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

        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::InvalidJumpTarget { .. }));
        assert_eq!(
            err.to_string(),
            "jump target 99 is out of range (program has 2 instructions)"
        );
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

        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::UnbalancedJumps { .. }));
        assert_eq!(
            err.to_string(),
            "jump at instruction 0 does not pair with a matching bracket"
        );
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

        let err = Program::read_from(&bytes[..]).unwrap_err();
        assert!(matches!(err, FormatError::UnbalancedJumps { .. }));
    }

    #[test]
    fn rejects_unbalanced_closing_brackets() {
        let out_of_range = [Op::JumpIfNonZero { target: 1 }];
        assert!(matches!(
            validate_jumps(&out_of_range),
            Err(FormatError::InvalidJumpTarget { target: 1, len: 1 })
        ));

        let wrong_kind = [Op::JumpIfNonZero { target: 0 }];
        assert!(matches!(
            validate_jumps(&wrong_kind),
            Err(FormatError::UnbalancedJumps { op_index: 0 })
        ));

        let non_reciprocal = [
            Op::JumpIfZero { target: 1 },
            Op::JumpIfNonZero { target: 0 },
            Op::JumpIfNonZero { target: 0 },
        ];
        assert!(matches!(
            validate_jumps(&non_reciprocal),
            Err(FormatError::UnbalancedJumps { op_index: 2 })
        ));
    }
}
