//! Windows runtime: freestanding with a custom entry symbol, calling
//! kernel32 for everything.
//!
//! Why Win32 instead of the C runtime: ucrt/msvcrt export naming for
//! read/write is a maze (underscored or not, differing across CRT flavors
//! and import libraries), while `GetStdHandle`/`ReadFile`/`WriteFile`/
//! `ExitProcess` are the same stable symbols in every toolchain that can
//! build this target. The entry symbol `bf_start` is wired to the PE entry
//! point with `-Wl,-e,bf_start` (see driver_flags), since -nostdlib means
//! no CRT startup object to provide one.
//!
//! This expects a MinGW-family driver (cross binutils, MSYS2, or
//! llvm-mingw); MSVC's cl reads MASM, not GNU-syntax assembly, and isn't a
//! supported driver.

use crate::codegen::{line, TAPE_SIZE};
use crate::target::{Arch, Target};

/// GetStdHandle argument values, as 32-bit constants.
const STD_INPUT_HANDLE: i32 = -10;
const STD_OUTPUT_HANDLE: i32 = -11;

pub(super) fn prologue(out: &mut String, target: Target) {
    line(out, ".globl bf_start");
    line(out, "bf_start:");
    match target.arch {
        // Force 16-byte stack alignment (a raw PE entry point's alignment is
        // not something to bet on), then reserve the Win64 call frame: 32
        // bytes of shadow space plus the 5th-argument slot at [rsp+0x20].
        // 0x30 total keeps rsp 16-aligned at every call, as the ABI requires.
        // Nothing is ever returned from, so no teardown.
        Arch::X86_64 => {
            line(out, "    and rsp, -16");
            line(out, "    sub rsp, 0x30");
        }

        // 32 bytes of stack: keeps sp 16-byte aligned at the bl call sites
        // and provides the writable DWORD at [sp] that ReadFile/WriteFile's
        // lpNumberOfBytesWritten parameter points at.
        Arch::Aarch64 => {
            line(out, "    sub sp, sp, #32");
        }
    }
    super::load_tape_ptr(out, target);
}

pub(super) fn epilogue(out: &mut String, target: Target) {
    match target.arch {
        Arch::X86_64 => {
            line(out, "    xor ecx, ecx");
            line(out, "    call ExitProcess");
        }
        Arch::Aarch64 => {
            line(out, "    mov w0, wzr");
            line(out, "    bl ExitProcess");
        }
    }
}

pub(super) fn emit_output(out: &mut String, target: Target) {
    emit_file_op(out, target, STD_OUTPUT_HANDLE, "WriteFile");
}

pub(super) fn emit_input(out: &mut String, target: Target) {
    emit_file_op(out, target, STD_INPUT_HANDLE, "ReadFile");
}

/// WriteFile/ReadFile on the current cell, one byte at a time. The handle is
/// re-fetched per op — GetStdHandle is a cheap TLS lookup — which keeps
/// every op self-contained and stateless.
///
/// For ReadFile, both EOF (returns TRUE with *bytes read* == 0) and failure
/// (returns FALSE) leave the target buffer untouched, which is exactly
/// bfrun's "cell unchanged on EOF" convention.
fn emit_file_op(out: &mut String, target: Target, std_handle: i32, function: &str) {
    match target.arch {
        Arch::X86_64 => {
            line(out, &format!("    mov ecx, {std_handle}"));
            line(out, "    call GetStdHandle");
            line(out, "    mov rcx, rax");
            line(out, "    mov rdx, rbx");
            line(out, "    mov r8d, 1");
            line(out, "    lea r9, [rip + bf_n]");
            line(out, "    mov qword ptr [rsp+0x20], 0");
            line(out, &format!("    call {function}"));
        }
        Arch::Aarch64 => {
            // movn w0, #N encodes ~N: ~10 == 0xFFFFFFF5 == -11 (stdout),
            // ~9 == 0xFFFFFFF6 == -10 (stdin) — the GetStdHandle constants,
            // without depending on mov's alias machinery for 32-bit values.
            let movn_operand = !(std_handle as u32);
            line(out, &format!("    movn w0, #{movn_operand}"));
            line(out, "    bl GetStdHandle");
            // x0 already holds the handle from GetStdHandle; fill the rest
            // of the argument registers around it.
            line(out, "    mov x1, x19");
            line(out, "    mov x2, #1");
            line(out, "    mov x3, sp");
            line(out, "    mov x4, xzr");
            line(out, &format!("    bl {function}"));
        }
    }
}

pub(super) fn emit_globals(out: &mut String, target: Target) {
    line(out, &format!(".comm tape, {TAPE_SIZE}"));
    if target.arch == Arch::X86_64 {
        // Scratch DWORD for the lpNumberOfBytesWrite argument, which the
        // docs require to point somewhere writable for synchronous handles.
        // The arm64 path uses its stack slot instead, so only x86-64 needs
        // the symbol.
        line(out, ".comm bf_n, 8");
    }
}
