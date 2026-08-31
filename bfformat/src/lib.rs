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

#[derive(Debug)]
pub enum FormatError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u8),
    Truncated,
    UnknownOpTag(u8),
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
                write!(f, "bytecode format version {v} is not supported by this runtime")
            }
            FormatError::Truncated => write!(f, "bytecode file is truncated or corrupt"),
            FormatError::UnknownOpTag(t) => write!(f, "unknown instruction tag {t} in bytecode"),
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
            }
        }

        Ok(())
    }

    pub fn read_from<R: Read>(mut input: R) -> Result<Self, FormatError> {
        let mut magic = [0u8; 4];
        input.read_exact(&mut magic).map_err(|_| FormatError::Truncated)?;
        if magic != MAGIC {
            return Err(FormatError::BadMagic);
        }

        let mut version = [0u8; 1];
        input.read_exact(&mut version).map_err(|_| FormatError::Truncated)?;
        if version[0] != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion(version[0]));
        }

        let mut count_bytes = [0u8; 4];
        input.read_exact(&mut count_bytes).map_err(|_| FormatError::Truncated)?;
        let count = u32::from_le_bytes(count_bytes) as usize;

        let mut ops = Vec::with_capacity(count);
        for _ in 0..count {
            ops.push(read_op(&mut input)?);
        }

        Ok(Program { ops })
    }
}

fn read_op<R: Read>(input: &mut R) -> Result<Op, FormatError> {
    let mut tag = [0u8; 1];
    input.read_exact(&mut tag).map_err(|_| FormatError::Truncated)?;

    let op = match tag[0] {
        0 => Op::Add(read_u8(input)?),
        1 => Op::Sub(read_u8(input)?),
        2 => Op::MoveRight(read_u32(input)?),
        3 => Op::MoveLeft(read_u32(input)?),
        4 => Op::Output,
        5 => Op::Input,
        6 => Op::JumpIfZero { target: read_u32(input)? },
        7 => Op::JumpIfNonZero { target: read_u32(input)? },
        8 => Op::Zero,
        other => return Err(FormatError::UnknownOpTag(other)),
    };

    Ok(op)
}

fn read_u8<R: Read>(input: &mut R) -> Result<u8, FormatError> {
    let mut buf = [0u8; 1];
    input.read_exact(&mut buf).map_err(|_| FormatError::Truncated)?;
    Ok(buf[0])
}

fn read_u32<R: Read>(input: &mut R) -> Result<u32, FormatError> {
    let mut buf = [0u8; 4];
    input.read_exact(&mut buf).map_err(|_| FormatError::Truncated)?;
    Ok(u32::from_le_bytes(buf))
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
                Op::JumpIfZero { target: 4 },
                Op::Output,
                Op::JumpIfNonZero { target: 1 },
                Op::Zero,
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
}
