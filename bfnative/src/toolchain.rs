use std::fmt;
use std::io;
use std::path::Path;
use std::process::Command;

use crate::target::{self, Target};

#[derive(Debug)]
pub enum ToolchainError {
    /// The driver binary itself couldn't be launched — most commonly, it
    /// simply isn't installed for this target on this machine.
    Spawn { program: String, source: io::Error },
    /// The driver ran, but the assembler or linker rejected what we emitted.
    Failed {
        program: String,
        status: String,
        stderr: String,
    },
}

impl fmt::Display for ToolchainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolchainError::Spawn { program, source } => write!(
                f,
                "couldn't run `{program}` ({source}); is a toolchain for this \
                 target installed? --cc can point at a different driver"
            ),
            ToolchainError::Failed {
                program,
                status,
                stderr,
            } => {
                write!(f, "`{program}` failed ({status}):\n{stderr}")
            }
        }
    }
}

impl std::error::Error for ToolchainError {}

/// Assembles and links `asm_path` into `output_path` by shelling out to the
/// assembler/linker driver (cc/clang/gcc/...).
///
/// `driver` is a full command prefix, split on whitespace — so
/// `--cc "clang --target=aarch64-linux-gnu -fuse-ld=lld"` works. The
/// per-target flags (see `target::driver_flags`) are appended after the
/// assembly file, followed by `-o output`. Building raw machine code and
/// PE/COFF/ELF/Mach-O structures by hand is deliberately out of scope: the
/// external toolchain already knows every target's encodings and linking
/// rules, and re-implementing them would be several times the size of this
/// whole backend for no benefit.
pub fn assemble_and_link(
    asm_path: &Path,
    output_path: &Path,
    target: Target,
    driver: &str,
) -> Result<(), ToolchainError> {
    let mut words = driver.split_whitespace();
    // main() rejects an empty --cc before we get here.
    let program = words
        .next()
        .expect("driver command is non-empty")
        .to_string();

    let mut command = Command::new(&program);
    command.args(words);
    command.arg(asm_path);
    for flag in target::driver_flags(target) {
        command.arg(flag);
    }
    command.arg("-o").arg(output_path);

    let output = command.output().map_err(|source| ToolchainError::Spawn {
        program: program.clone(),
        source,
    })?;

    if !output.status.success() {
        let mut diagnostics = String::from_utf8_lossy(&output.stderr).into_owned();
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.trim().is_empty() {
            diagnostics.push('\n');
            diagnostics.push_str(&stdout);
        }
        return Err(ToolchainError::Failed {
            program,
            status: output.status.to_string(),
            stderr: diagnostics,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_error_names_the_program_and_suggests_cc() {
        let err = assemble_and_link(
            Path::new("/does-not-exist.s"),
            Path::new("/does-not-exist"),
            Target::from_name("linux-x86_64").unwrap(),
            "definitely-not-a-real-compiler-xyz",
        )
        .unwrap_err();

        match err {
            ToolchainError::Spawn { ref program, .. } => {
                assert_eq!(program, "definitely-not-a-real-compiler-xyz");
                assert!(err.to_string().contains("--cc"));
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }
}
