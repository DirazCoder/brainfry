//! macOS runtime: an ordinary `main` calling libSystem's `read`/`write`,
//! linked by cc the standard way.
//!
//! A no-libc executable isn't an option here — macOS has no static linking
//! and arm64 macOS won't even load a binary that doesn't go through dyld —
//! and it isn't a problem either: macOS targets are built on a mac, where cc
//! and libSystem always exist. That's the one cross-compilation story this
//! backend doesn't attempt to solve in tool code, per the design.

use crate::codegen::{line, TAPE_SIZE};
use crate::target::{Arch, Target};

pub(super) fn prologue(out: &mut String, target: Target) {
    line(out, ".globl _main");
    line(out, "_main:");
    match target.arch {
        // One push does two jobs: saves the caller's rbx (we take it over as
        // the cell pointer) and realigns rsp to 16 bytes — main is entered
        // with rsp misaligned by the pushed return address, and the SysV ABI
        // requires alignment at every call site.
        Arch::X86_64 => line(out, "    push rbx"),

        // Same job on arm64: save the caller's x19, keep sp 16-byte aligned.
        // (arm64 passes the return address in x30, not on the stack, so
        // alignment is already fine; the push-pair form is just the cheapest
        // way to save the register.)
        Arch::Aarch64 => line(out, "    str x19, [sp, #-16]!"),
    }
    super::load_tape_ptr(out, target);
}

pub(super) fn epilogue(out: &mut String, target: Target) {
    match target.arch {
        // Return 0 from main; the CRT turns that into exit(0).
        Arch::X86_64 => {
            line(out, "    xor eax, eax");
            line(out, "    pop rbx");
            line(out, "    ret");
        }
        Arch::Aarch64 => {
            line(out, "    mov w0, wzr");
            line(out, "    ldr x19, [sp], #16");
            line(out, "    ret");
        }
    }
}

pub(super) fn emit_output(out: &mut String, target: Target) {
    match target.arch {
        // write(1, cell, 1) in the SysV convention. libc preserves rbx/x19
        // (callee-saved), so the cell pointer just survives.
        Arch::X86_64 => {
            line(out, "    mov edi, 1");
            line(out, "    mov rsi, rbx");
            line(out, "    mov edx, 1");
            line(out, "    call _write");
        }
        Arch::Aarch64 => {
            line(out, "    mov x0, #1");
            line(out, "    mov x1, x19");
            line(out, "    mov x2, #1");
            line(out, "    bl _write");
        }
    }
}

pub(super) fn emit_input(out: &mut String, target: Target) {
    match target.arch {
        // read(0, cell, 1); EOF (return value 0) leaves the cell untouched,
        // matching bfrun.
        Arch::X86_64 => {
            line(out, "    xor edi, edi");
            line(out, "    mov rsi, rbx");
            line(out, "    mov edx, 1");
            line(out, "    call _read");
        }
        Arch::Aarch64 => {
            line(out, "    mov x0, #0");
            line(out, "    mov x1, x19");
            line(out, "    mov x2, #1");
            line(out, "    bl _read");
        }
    }
}

pub(super) fn emit_globals(out: &mut String, _target: Target) {
    // Mach-O's way of saying "zero-initialized bss with a symbol": zerofill
    // in a __DATA section. Local symbol, defined in-object.
    line(out, &format!(".zerofill __DATA,__bss,_tape,{TAPE_SIZE}"));
}
