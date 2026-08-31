//! Linux runtime: freestanding, raw syscalls, no libc, fully static.
//!
//! The kernel starts the process at `_start` directly — no return address,
//! no caller to be polite to, so there's no frame setup at all and no
//! registers to preserve. Syscall numbers differ per architecture (x86-64
//! keeps its historical table, arm64 uses the newer unified one), which is
//! the only per-arch split in this file.

use crate::codegen::{line, TAPE_SIZE};
use crate::target::{Arch, Target};

pub(super) fn prologue(out: &mut String, target: Target) {
    line(out, ".globl _start");
    line(out, "_start:");
    super::load_tape_ptr(out, target);
}

pub(super) fn epilogue(out: &mut String, target: Target) {
    match target.arch {
        // exit_group(0). Kernel threads don't exist here, but exit_group is
        // the stronger "terminate everything" choice and costs nothing.
        Arch::X86_64 => {
            line(out, "    mov eax, 231");
            line(out, "    xor edi, edi");
            line(out, "    syscall");
        }
        Arch::Aarch64 => {
            line(out, "    mov x8, #94");
            line(out, "    mov x0, #0");
            line(out, "    svc 0");
        }
    }
}

pub(super) fn emit_output(out: &mut String, target: Target) {
    match target.arch {
        // write(1, cell, 1) — the buffer points straight at the current cell,
        // no copy needed. rax/rcx/r11 are clobbered by the kernel; none of
        // them hold state.
        Arch::X86_64 => {
            line(out, "    mov eax, 1");
            line(out, "    mov edi, 1");
            line(out, "    mov rsi, rbx");
            line(out, "    mov edx, 1");
            line(out, "    syscall");
        }
        Arch::Aarch64 => {
            line(out, "    mov x8, #64");
            line(out, "    mov x0, #1");
            line(out, "    mov x1, x19");
            line(out, "    mov x2, #1");
            line(out, "    svc 0");
        }
    }
}

pub(super) fn emit_input(out: &mut String, target: Target) {
    match target.arch {
        // read(0, cell, 1). A read of zero bytes at EOF leaves the buffer
        // untouched, which is exactly bfrun's "cell unchanged on EOF"
        // convention — no fixup needed. (bfrun also retries read_exact on
        // EINTR; a raw read doesn't, but a 1-byte stdin read that lands on
        // EINTR is not a case any Brainfuck program can observe meaningfully.)
        Arch::X86_64 => {
            line(out, "    xor eax, eax");
            line(out, "    xor edi, edi");
            line(out, "    mov rsi, rbx");
            line(out, "    mov edx, 1");
            line(out, "    syscall");
        }
        Arch::Aarch64 => {
            line(out, "    mov x8, #63");
            line(out, "    mov x0, #0");
            line(out, "    mov x1, x19");
            line(out, "    mov x2, #1");
            line(out, "    svc 0");
        }
    }
}

pub(super) fn emit_globals(out: &mut String, _target: Target) {
    line(out, &format!(".comm tape, {TAPE_SIZE}"));
}
