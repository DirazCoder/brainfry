mod aarch64;
mod x86_64;

use bfformat::Op;

use crate::target::Arch;

/// Lowers one non-I/O op to instructions for the given architecture. The
/// current cell's address is assumed to live in the arch's cell register (rbx
/// on x86-64, x19 on arm64) — set up by the os layer's prologue.
///
/// `Op::Output` and `Op::Input` are lowered by the `os` module instead (they
/// never reach this function): their entire implementation is the OS's
/// calling convention for I/O.
pub fn emit_op(out: &mut String, arch: Arch, op: &Op) {
    match arch {
        Arch::X86_64 => x86_64::emit_op(out, op),
        Arch::Aarch64 => aarch64::emit_op(out, op),
    }
}

/// Line-comment prefix for the target's assembler dialect. x86 assembly
/// conventionally comments with `#`, arm64 with `//` (`@`, the arm32
/// comment, is the operand separator in arm64 unified syntax).
pub fn comment_prefix(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "#",
        Arch::Aarch64 => "//",
    }
}
