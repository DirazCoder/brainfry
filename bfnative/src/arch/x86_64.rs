use bfformat::Op;

use crate::codegen::{line, op_label};

/// Register that always holds the current cell's address. RBX is
/// callee-saved in both the SysV and Win64 ABIs, so it survives the calls
/// the os layer emits (write/read/WriteFile/...), and nothing else in the
/// generated code needs a register to stay live across those calls. EAX and
/// friends are caller-saved scratch, used freely between I/O calls.
const CELL: &str = "rbx";

pub(super) fn emit_op(out: &mut String, op: &Op) {
    match op {
        // Byte-sized memory operands make 8-bit wraparound automatic: the ALU
        // computes the full result and the store only keeps its low byte.
        Op::Add(n) => line(out, &format!("    add byte ptr [{CELL}], {n}")),
        Op::Sub(n) => line(out, &format!("    sub byte ptr [{CELL}], {n}")),

        Op::MoveRight(cells) => adjust_pointer(out, *cells, "add"),
        Op::MoveLeft(cells) => adjust_pointer(out, *cells, "sub"),

        Op::Zero => line(out, &format!("    mov byte ptr [{CELL}], 0")),

        Op::JumpIfZero { target } => {
            line(out, &format!("    cmp byte ptr [{CELL}], 0"));
            line(out, &format!("    je {}", op_label(*target as usize + 1)));
        }
        Op::JumpIfNonZero { target } => {
            line(out, &format!("    cmp byte ptr [{CELL}], 0"));
            line(out, &format!("    jne {}", op_label(*target as usize + 1)));
        }

        // Reached only through the os layer's prologue/epilogue/I/O paths;
        // codegen routes these elsewhere before calling in.
        Op::Output | Op::Input => unreachable!("I/O ops are lowered by the os module"),
    }
}

/// Moves the cell pointer by `cells`. `add/sub rbx, imm32` sign-extends its
/// immediate, so counts of 2^31 or more (a `>` run that long is absurd but
/// representable) can't be encoded directly and go through a scratch
/// register instead.
fn adjust_pointer(out: &mut String, cells: u32, mnemonic: &str) {
    if cells <= i32::MAX as u32 {
        line(out, &format!("    {mnemonic} {CELL}, {cells}"));
    } else {
        line(out, &format!("    mov eax, 0x{cells:x}"));
        line(out, &format!("    {mnemonic} {CELL}, rax"));
    }
}
