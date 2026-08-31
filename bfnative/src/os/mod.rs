mod linux;
mod macos;
mod windows;

use crate::codegen::line;
use crate::target::{Arch, Os, Target};

/// Emits the entry sequence: the entry symbol, any stack setup the OS's
/// calling convention needs, and the cell pointer initialized to the tape's
/// first cell.
pub fn emit_prologue(out: &mut String, target: Target) {
    match target.os {
        Os::Linux => linux::prologue(out, target),
        Os::Macos => macos::prologue(out, target),
        Os::Windows => windows::prologue(out, target),
    }
}

/// Emits the exit sequence: return from main, exit_group, or ExitProcess,
/// depending on who owns process termination on this OS.
pub fn emit_epilogue(out: &mut String, target: Target) {
    match target.os {
        Os::Linux => linux::epilogue(out, target),
        Os::Macos => macos::epilogue(out, target),
        Os::Windows => windows::epilogue(out, target),
    }
}

pub fn emit_output(out: &mut String, target: Target) {
    match target.os {
        Os::Linux => linux::emit_output(out, target),
        Os::Macos => macos::emit_output(out, target),
        Os::Windows => windows::emit_output(out, target),
    }
}

pub fn emit_input(out: &mut String, target: Target) {
    match target.os {
        Os::Linux => linux::emit_input(out, target),
        Os::Macos => macos::emit_input(out, target),
        Os::Windows => windows::emit_input(out, target),
    }
}

/// Emits the tape (and any other data) the program needs, in whatever form
/// the target's object format wants for uninitialized zeroed data.
pub fn emit_globals(out: &mut String, target: Target) {
    match target.os {
        Os::Linux => linux::emit_globals(out, target),
        Os::Macos => macos::emit_globals(out, target),
        Os::Windows => windows::emit_globals(out, target),
    }
}

/// The tape symbol's assembler-level name. Mach-O prefixes C symbols with an
/// underscore; ELF and PE don't.
fn tape_symbol(target: Target) -> String {
    match target.os {
        Os::Macos => "_tape".to_string(),
        _ => "tape".to_string(),
    }
}

/// Materializes the tape's address into the cell-pointer register (rbx /
/// x19), PC-relative everywhere so the code is position-independent.
///
/// This is the one place an object-format quirk leaks into code generation:
/// on arm64 Mach-O, ADRP against a named symbol must be GOT-relative, so the
/// address goes through a GOT indirection there. On every other target a
/// direct PC-relative reference works (and was verified against each
/// toolchain).
fn load_tape_ptr(out: &mut String, target: Target) {
    let tape = tape_symbol(target);
    match (target.arch, target.os) {
        (Arch::X86_64, _) => line(out, &format!("    lea rbx, [rip + {tape}]")),

        (Arch::Aarch64, Os::Macos) => {
            line(out, &format!("    adrp x19, {tape}@GOTPAGE"));
            line(out, &format!("    ldr x19, [x19, {tape}@GOTPAGEOFF]"));
        }

        (Arch::Aarch64, _) => {
            line(out, &format!("    adrp x19, {tape}"));
            line(out, &format!("    add x19, x19, :lo12: {tape}"));
        }
    }
}
