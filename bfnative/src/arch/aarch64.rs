use bfformat::Op;

use crate::codegen::{line, op_label};

/// Register that always holds the current cell's address. X19 is
/// callee-saved in every arm64 calling convention this backend targets —
/// Linux/macOS (the SysV-style AAPCS) and Windows — so it survives the bl
/// calls the os layer emits. W0 is the scratch register: caller-saved
/// everywhere and free between I/O calls. X18 is never touched (it's the
/// platform register Windows reserves).
const CELL: &str = "x19";

pub(super) fn emit_op(out: &mut String, op: &Op) {
    match op {
        // Load-modify-store. 8-bit wraparound comes from the final `strb`
        // storing only the low byte of the 32-bit result, so an underflowing
        // sub lands on 255 exactly like bfrun's wrapping_sub.
        Op::Add(n) => {
            line(out, &format!("    ldrb w0, [{CELL}]"));
            line(out, &format!("    add w0, w0, #{n}"));
            line(out, &format!("    strb w0, [{CELL}]"));
        }
        Op::Sub(n) => {
            line(out, &format!("    ldrb w0, [{CELL}]"));
            line(out, &format!("    sub w0, w0, #{n}"));
            line(out, &format!("    strb w0, [{CELL}]"));
        }

        Op::MoveRight(cells) => adjust_pointer(out, *cells, "add"),
        Op::MoveLeft(cells) => adjust_pointer(out, *cells, "sub"),

        Op::Zero => line(out, &format!("    strb wzr, [{CELL}]")),

        Op::JumpIfZero { target } => {
            line(out, &format!("    ldrb w0, [{CELL}]"));
            line(
                out,
                &format!("    cbz w0, {}", op_label(*target as usize + 1)),
            );
        }
        Op::JumpIfNonZero { target } => {
            line(out, &format!("    ldrb w0, [{CELL}]"));
            line(
                out,
                &format!("    cbnz w0, {}", op_label(*target as usize + 1)),
            );
        }

        Op::Output | Op::Input => unreachable!("I/O ops are lowered by the os module"),
    }
}

/// Moves the cell pointer by `cells`. The add/sub immediate field only holds
/// 0..=4095, so bigger counts are loaded into the scratch register first
/// with an explicit movz/movk pair (deterministic encodings, no dependence
/// on an assembler's `mov` alias rules).
fn adjust_pointer(out: &mut String, cells: u32, mnemonic: &str) {
    if cells <= 4095 {
        line(out, &format!("    {mnemonic} {CELL}, {CELL}, #{cells}"));
    } else {
        load_imm32(out, "w0", cells);
        line(out, &format!("    {mnemonic} {CELL}, {CELL}, x0"));
    }
}

fn load_imm32(out: &mut String, register: &str, value: u32) {
    line(
        out,
        &format!("    movz {register}, #0x{:x}", value & 0xFFFF),
    );
    if value > 0xFFFF {
        line(
            out,
            &format!("    movk {register}, #0x{:x}, lsl #16", value >> 16),
        );
    }
}
