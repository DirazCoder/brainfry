//! x86-64 machine code backend.
//!
//! One instruction selector, three OS runtimes (Linux syscalls, macOS
//! libSystem calls through the GOT, Windows kernel32 calls through the IAT).
//! Register roles, the tape-growth protocol, and the error paths are shared;
//! see `codegen/mod.rs` for the fixed register budget.
//!
//! Encoding notes sit next to each emitter. Every rip-relative reference and
//! every branch goes through a rel32 fixup, and all conditional branches use
//! the long 6-byte `0F 8x` form so branch sizes never depend on distance —
//! no near/far relaxation is needed, which is what makes single-pass
//! emission with post-hoc patching sound.
//!
//! Extra register roles on top of the shared ones (`r12` cell, `r13` base,
//! `r14` end):
//!
//! - `rbp` is a temporary inside the grow routine (it is callee-saved in
//!   every ABI here and unused everywhere else, so `mprotect` /
//!   `VirtualAlloc` calls preserve it across the commit).
//! - Windows only: `rbx` = stdout handle, `r15` = stdin handle, bound once
//!   in the entry (both callee-saved in Win64, so every kernel32 call
//!   preserves them).

use bfformat::Op;

use crate::codegen::{
    Emitter, GOT_EXIT, GOT_MMAP, GOT_MPROTECT, GOT_READ, GOT_WRITE, MSG_ALLOC, MSG_CAP, MSG_GROW,
    MSG_IO, MSG_UNDERFLOW, PatchTarget, TAPE_GRAN, TAPE_INITIAL, TAPE_RESERVE,
};
use crate::target::{Os, Target};

// Register numbers (x86-64 encoding order: rax=0 ... r15=15).
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RBP: u8 = 5;
const RSI: u8 = 6;
const RDI: u8 = 7;
const CELL: u8 = 12; // r12
const BASE: u8 = 13; // r13
const END: u8 = 14; // r14
const R15: u8 = 15;

// IAT slot indices on Windows (same order as external_symbols()).
const IAT_GETSTDHANDLE: u32 = 0;
const IAT_WRITEFILE: u32 = 1;
const IAT_READFILE: u32 = 2;
const IAT_EXITPROCESS: u32 = 3;
const IAT_VIRTUALALLOC: u32 = 4;

/// Linux syscall numbers (x86-64 historical table).
const SYS_READ: u32 = 0;
const SYS_WRITE: u32 = 1;
const SYS_MMAP: u32 = 9;
const SYS_MPROTECT: u32 = 10;
const SYS_EXIT_GROUP: u32 = 231;

const MAP_PRIVATE_ANON_LINUX: u32 = 0x02 | 0x20;
const MAP_PRIVATE_ANON_MACOS: u32 = 0x02 | 0x1000;
const PROT_READ_WRITE: u32 = 3;

/// -errno value for EINTR, to retry interrupted 1-byte I/O exactly like
/// bfrun's `read_exact` / `write_all` do.
const EINTR: i8 = -4;

const STD_INPUT_HANDLE: i32 = -10;
const STD_OUTPUT_HANDLE: i32 = -11;
const STD_ERROR_HANDLE: i32 = -12;
const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_NOACCESS: u32 = 1;
const PAGE_READWRITE: u32 = 4;

// jcc condition nibbles for `0F 8x`.
const CC_B: u8 = 2; // unsigned below
const CC_AE: u8 = 3; // unsigned above-or-equal
const CC_E: u8 = 4;
const CC_NE: u8 = 5;

/// Every runtime label, allocated once before the body so ops can reference
/// them regardless of emission order.
struct Rt {
    grow: u32,
    err_underflow: u32,
    err_io: u32,
    err_cap: u32,
    err_grow: u32,
    err_alloc: u32,
}

pub fn emit(e: &mut Emitter, ops: &[Op], target: Target) {
    let rt = Rt {
        grow: e.internal_label(),
        err_underflow: e.internal_label(),
        err_io: e.internal_label(),
        err_cap: e.internal_label(),
        err_grow: e.internal_label(),
        err_alloc: e.internal_label(),
    };

    match target.os {
        Os::Linux => {
            emit_entry_linux(e, &rt);
            emit_body(e, ops, Os::Linux, &rt);
            emit_exit_ok_linux(e);
            emit_grow_common(e, &rt, &commit_linux);
            emit_errors_linux(e, &rt);
        }
        Os::Macos => {
            emit_entry_macos(e, &rt);
            emit_body(e, ops, Os::Macos, &rt);
            emit_exit_ok_macos(e);
            emit_grow_common(e, &rt, &commit_macos);
            emit_errors_macos(e, &rt);
        }
        Os::Windows => {
            emit_entry_windows(e, &rt);
            emit_body(e, ops, Os::Windows, &rt);
            emit_exit_ok_windows(e);
            emit_grow_common(e, &rt, &commit_windows);
            emit_errors_windows(e, &rt);
        }
    }
}

// ---------------------------------------------------------------------------
// body: per-op lowering; only I/O is OS-specific
// ---------------------------------------------------------------------------

fn emit_body(e: &mut Emitter, ops: &[Op], os: Os, rt: &Rt) {
    for (i, op) in ops.iter().enumerate() {
        // Label `i` = start of op `i`'s code — jump targets aim here.
        e.bind_here(i as u32);
        match *op {
            Op::Output => match os {
                Os::Linux => emit_output_linux(e, rt),
                Os::Macos => emit_output_macos(e, rt),
                Os::Windows => emit_output_windows(e, rt),
            },
            Op::Input => match os {
                Os::Linux => emit_input_linux(e, rt),
                Os::Macos => emit_input_macos(e, rt),
                Os::Windows => emit_input_windows(e, rt),
            },
            op => emit_op(e, &op, i, rt),
        }
    }
    e.bind_here(ops.len() as u32); // the exit label
}

fn emit_op(e: &mut Emitter, op: &Op, i: usize, rt: &Rt) {
    match *op {
        Op::Add(n) => {
            e.span(format!("op {i}: Add({n}) — add byte [r12], {n}"));
            add_cell(e, n);
        }
        Op::Sub(n) => {
            e.span(format!("op {i}: Sub({n}) — sub byte [r12], {n}"));
            sub_cell(e, n);
        }
        Op::Zero => {
            e.span(format!("op {i}: Zero — mov byte [r12], 0"));
            zero_cell(e);
        }
        Op::MoveRight(n) => {
            e.span(format!("op {i}: MoveRight({n}) — bounds check + grow"));
            // rax = cell + n. If rax >= end (unsigned) the destination cell
            // is past current capacity: *call* grow with rax = desired
            // pointer (it returns via ret, preserving rax), so a single
            // `mov r12, rax` updates the pointer on both paths.
            if n <= i32::MAX as u32 {
                lea_rax_cell(e, n as i64);
            } else {
                // `add r64, imm32` sign-extends, so huge counts go through a
                // zero-extended register instead.
                mov_r32_imm32(e, RAX, n);
                add64(e, RAX, CELL);
            }
            let grow_path = e.internal_label();
            let update = e.internal_label();
            cmp64(e, RAX, END);
            jcc(e, CC_AE, grow_path);
            jmp(e, update);
            e.bind_here(grow_path);
            e.span("  grow: call bf_grow");
            call(e, rt.grow);
            e.bind_here(update);
            e.span("  update: mov r12, rax");
            mov64(e, CELL, RAX);
        }
        Op::MoveLeft(n) => {
            e.span(format!("op {i}: MoveLeft({n}) — underflow check"));
            // offset = r12 - r13 never wraps (r12 >= r13 is the invariant),
            // and underflow is exactly `offset < n`. Comparing the offset
            // against a zero-extended count avoids the classic bug where
            // (r12 - n) wraps around to a huge unsigned value and slips past
            // a naive bounds check.
            mov64(e, RAX, CELL);
            sub64(e, RAX, BASE);
            if n <= i32::MAX as u32 {
                cmp_rax_imm32(e, n);
                jcc(e, CC_B, rt.err_underflow);
                sub64_imm32(e, CELL, n);
            } else {
                mov_r32_imm32(e, RDX, n);
                cmp64(e, RAX, RDX);
                jcc(e, CC_B, rt.err_underflow);
                sub64(e, CELL, RDX);
            }
        }
        // bfrun executes op `target + 1` after a taken jump (pc = target,
        // then pc += 1), so branches aim one past the paired bracket op.
        // `target + 1 == ops.len()` (closing bracket last) lands on the exit
        // label, which is always bound.
        Op::JumpIfZero { target } => {
            e.span(format!(
                "op {i}: JumpIfZero -> op {} — cmp byte [r12], 0; je",
                target as usize + 1
            ));
            cmp_cell_zero(e);
            jcc(e, CC_E, target + 1);
        }
        Op::JumpIfNonZero { target } => {
            e.span(format!(
                "op {i}: JumpIfNonZero -> op {} — cmp byte [r12], 0; jne",
                target as usize + 1
            ));
            cmp_cell_zero(e);
            jcc(e, CC_NE, target + 1);
        }
        Op::Output | Op::Input => unreachable!("dispatched by emit_body"),
    }
}

// ---------------------------------------------------------------------------
// Linux runtime
// ---------------------------------------------------------------------------

fn emit_entry_linux(e: &mut Emitter, rt: &Rt) {
    e.span("entry _start (linux-x86_64): mmap tape reservation, mprotect first 64 KiB");
    // mmap(NULL, RESERVE, PROT_NONE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
    xor32(e, RDI);
    mov_r32_imm32(e, RSI, TAPE_RESERVE as u32);
    xor32(e, RDX);
    mov_r32_imm32(e, 10, MAP_PRIVATE_ANON_LINUX); // r10 = flags (4th arg)
    mov_r32_imm32(e, 8, 0xFFFF_FFFF); // r8 = fd (-1; ignored for MAP_ANON)
    xor32(e, 9); // r9 = offset
    mov_r32_imm32(e, RAX, SYS_MMAP);
    syscall(e);
    check_errno(e);
    jcc(e, CC_AE, rt.err_alloc);
    mov64(e, BASE, RAX);

    // mprotect(base, TAPE_GRAN, PROT_READ|WRITE): the first 64 KiB covers
    // the initial 30,000-cell capacity with room to spare, and keeps the
    // "committed == round64k(capacity)" invariant the grow routine relies on.
    mov64(e, RDI, BASE);
    mov_r32_imm32(e, RSI, TAPE_GRAN as u32);
    mov_r32_imm32(e, RDX, PROT_READ_WRITE);
    mov_r32_imm32(e, RAX, SYS_MPROTECT);
    syscall(e);
    check_errno(e);
    jcc(e, CC_AE, rt.err_alloc);

    mov64(e, CELL, BASE);
    mov64(e, END, BASE);
    add64_imm32(e, END, TAPE_INITIAL as u32);
}

fn emit_exit_ok_linux(e: &mut Emitter) {
    e.span("exit_ok: exit_group(0)");
    xor32(e, RDI);
    mov_r32_imm32(e, RAX, SYS_EXIT_GROUP);
    syscall(e);
}

fn emit_output_linux(e: &mut Emitter, rt: &Rt) {
    e.span("Output: write(1, cell, 1), retry on EINTR");
    let retry = e.internal_label();
    e.bind_here(retry);
    mov_r32_imm32(e, RAX, SYS_WRITE);
    mov_r32_imm32(e, RDI, 1);
    mov64(e, RSI, CELL);
    mov_r32_imm32(e, RDX, 1);
    syscall(e);
    cmp_rax_imm32(e, 1);
    let done = e.internal_label();
    jcc(e, CC_E, done);
    cmp64_imm8(e, RAX, EINTR);
    jcc(e, CC_E, retry);
    jmp(e, rt.err_io);
    e.bind_here(done);
}

fn emit_input_linux(e: &mut Emitter, rt: &Rt) {
    e.span("Input: read(0, cell, 1); EOF leaves cell unchanged");
    let retry = e.internal_label();
    e.bind_here(retry);
    mov_r32_imm32(e, RAX, SYS_READ);
    xor32(e, RDI);
    mov64(e, RSI, CELL);
    mov_r32_imm32(e, RDX, 1);
    syscall(e);
    // The kernel stores the byte straight into the cell, so rax == 1 is the
    // whole success path.
    cmp_rax_imm32(e, 1);
    let done = e.internal_label();
    jcc(e, CC_E, done);
    cmp64_imm8(e, RAX, EINTR);
    jcc(e, CC_E, retry);
    // rax == 0 is EOF; the buffer (the cell itself) is untouched, which is
    // exactly bfrun's leave-unchanged convention.
    cmp64_imm8(e, RAX, 0);
    jcc(e, CC_E, done);
    jmp(e, rt.err_io);
    e.bind_here(done);
}

/// Linux grow commit: `mprotect(rdi, rsi, 3)` where rdi/rsi were loaded by
/// `emit_grow_common`. Branches to `fail` (after its caller-side pop) when
/// the syscall returns -errno.
fn commit_linux(e: &mut Emitter, fail: u32) {
    mov_r32_imm32(e, RDX, PROT_READ_WRITE);
    mov_r32_imm32(e, RAX, SYS_MPROTECT);
    syscall(e);
    check_errno(e);
    jcc(e, CC_AE, fail);
}

fn emit_errors_linux(e: &mut Emitter, rt: &Rt) {
    e.span("error tail (linux): write(2, msg), exit_group(1)");
    let tail = e.internal_label();
    e.bind_here(tail);
    // rsi = message, rdx = length (set by each jump site).
    mov_r32_imm32(e, RDI, 2);
    mov_r32_imm32(e, RAX, SYS_WRITE);
    syscall(e);
    mov_r32_imm32(e, RDI, 1);
    mov_r32_imm32(e, RAX, SYS_EXIT_GROUP);
    syscall(e);
    emit_error_jumps(e, rt, &|e, off, len| {
        lea_rip(e, RSI, off);
        mov_r32_imm32(e, RDX, len);
        jmp(e, tail);
    });
}

// ---------------------------------------------------------------------------
// macOS runtime
// ---------------------------------------------------------------------------

fn emit_entry_macos(e: &mut Emitter, rt: &Rt) {
    e.span("entry main (macos-x86_64): align stack, mmap via libSystem");
    // dyld calls the LC_MAIN entry with a return address on the stack; this
    // code never returns, so realigning rsp outright is safe and simpler
    // than tracking the 8-byte skew through every call site. After this, rsp
    // stays untouched for the whole program, keeping every later
    // `call [rip+GOT]` 16-byte aligned as SysV requires.
    e.bytes(&[0x48, 0x83, 0xE4, 0xF0]); // and rsp, -16

    // mmap(NULL, RESERVE, PROT_NONE, MAP_PRIVATE|MAP_ANON, -1, 0)
    xor32(e, RDI);
    mov_r32_imm32(e, RSI, TAPE_RESERVE as u32);
    xor32(e, RDX);
    mov_r32_imm32(e, RCX, MAP_PRIVATE_ANON_MACOS);
    mov_r32_imm32(e, 8, 0xFFFF_FFFF);
    xor32(e, 9);
    call_got(e, GOT_MMAP);
    cmp64_imm8(e, RAX, -1); // MAP_FAILED
    jcc(e, CC_E, rt.err_alloc);
    mov64(e, BASE, RAX);

    // mprotect(base, TAPE_GRAN, RW) — BSD return: 0 or -1.
    mov64(e, RDI, BASE);
    mov_r32_imm32(e, RSI, TAPE_GRAN as u32);
    mov_r32_imm32(e, RDX, PROT_READ_WRITE);
    call_got(e, GOT_MPROTECT);
    test_eax(e);
    jcc(e, CC_NE, rt.err_alloc);

    mov64(e, CELL, BASE);
    mov64(e, END, BASE);
    add64_imm32(e, END, TAPE_INITIAL as u32);
}

fn emit_exit_ok_macos(e: &mut Emitter) {
    e.span("exit_ok: _exit(0)");
    xor32(e, RDI);
    call_got(e, GOT_EXIT);
}

fn emit_output_macos(e: &mut Emitter, rt: &Rt) {
    e.span("Output: write(1, cell, 1) via GOT, retry on EINTR");
    let retry = e.internal_label();
    e.bind_here(retry);
    mov_r32_imm32(e, RDI, 1);
    mov64(e, RSI, CELL);
    mov_r32_imm32(e, RDX, 1);
    call_got(e, GOT_WRITE);
    cmp_rax_imm32(e, 1);
    let done = e.internal_label();
    jcc(e, CC_E, done);
    cmp64_imm8(e, RAX, EINTR);
    jcc(e, CC_E, retry);
    jmp(e, rt.err_io);
    e.bind_here(done);
}

fn emit_input_macos(e: &mut Emitter, rt: &Rt) {
    e.span("Input: read(0, cell, 1) via GOT; EOF leaves cell unchanged");
    let retry = e.internal_label();
    e.bind_here(retry);
    xor32(e, RDI);
    mov64(e, RSI, CELL);
    mov_r32_imm32(e, RDX, 1);
    call_got(e, GOT_READ);
    cmp_rax_imm32(e, 1);
    let done = e.internal_label();
    jcc(e, CC_E, done);
    cmp64_imm8(e, RAX, EINTR);
    jcc(e, CC_E, retry);
    cmp64_imm8(e, RAX, 0);
    jcc(e, CC_E, done);
    jmp(e, rt.err_io);
    e.bind_here(done);
}

/// macOS grow commit: `mprotect(rdi, rsi, 3)` via the GOT.
fn commit_macos(e: &mut Emitter, fail: u32) {
    mov_r32_imm32(e, RDX, PROT_READ_WRITE);
    call_got(e, GOT_MPROTECT);
    test_eax(e);
    jcc(e, CC_NE, fail);
}

fn emit_errors_macos(e: &mut Emitter, rt: &Rt) {
    e.span("error tail (macos): write(2, msg) via GOT, _exit(1)");
    let tail = e.internal_label();
    e.bind_here(tail);
    mov_r32_imm32(e, RDI, 2);
    call_got(e, GOT_WRITE);
    mov_r32_imm32(e, RDI, 1);
    call_got(e, GOT_EXIT);
    emit_error_jumps(e, rt, &|e, off, len| {
        lea_rip(e, RSI, off);
        mov_r32_imm32(e, RDX, len);
        jmp(e, tail);
    });
}

// ---------------------------------------------------------------------------
// Windows runtime
// ---------------------------------------------------------------------------

fn emit_entry_windows(e: &mut Emitter, rt: &Rt) {
    e.span("entry (windows-x86_64): align stack, VirtualAlloc tape, std handles");
    // A raw PE entry point's stack alignment is not a contract; force it,
    // then take a fixed 64-byte frame that never changes for the whole
    // program: [rsp+0,32) = shadow space for callees, [rsp+32,40) = the
    // 5th-argument slot, [rsp+56,64) = scratch qword for
    // lpNumberOfBytesWritten. Nothing pushes or pops again, so every call
    // site is 16-byte aligned with 32 bytes of shadow below it, exactly as
    // the Win64 ABI requires.
    e.bytes(&[0x48, 0x83, 0xE4, 0xF0]); // and rsp, -16
    e.bytes(&[0x48, 0x83, 0xEC, 0x40]); // sub rsp, 64

    // VirtualAlloc(NULL, RESERVE, MEM_RESERVE, PAGE_NOACCESS)
    xor32(e, RCX);
    mov_r32_imm32(e, RDX, TAPE_RESERVE as u32);
    mov_r32_imm32(e, 8, MEM_RESERVE);
    mov_r32_imm32(e, 9, PAGE_NOACCESS);
    call_got(e, IAT_VIRTUALALLOC);
    test_rax(e);
    jcc(e, CC_E, rt.err_alloc);
    mov64(e, BASE, RAX);

    // VirtualAlloc(base, TAPE_GRAN, MEM_COMMIT, PAGE_READWRITE)
    mov64(e, RCX, BASE);
    mov_r32_imm32(e, RDX, TAPE_GRAN as u32);
    mov_r32_imm32(e, 8, MEM_COMMIT);
    mov_r32_imm32(e, 9, PAGE_READWRITE);
    call_got(e, IAT_VIRTUALALLOC);
    test_rax(e);
    jcc(e, CC_E, rt.err_alloc);

    // rbx = GetStdHandle(STD_OUTPUT_HANDLE); r15 = GetStdHandle(STD_INPUT_HANDLE).
    // Both survive every kernel32 call (Win64 callee-saved).
    mov_r32_imm32(e, RCX, STD_OUTPUT_HANDLE as u32);
    call_got(e, IAT_GETSTDHANDLE);
    mov64(e, RBX, RAX);
    mov_r32_imm32(e, RCX, STD_INPUT_HANDLE as u32);
    call_got(e, IAT_GETSTDHANDLE);
    mov64(e, R15, RAX);

    mov64(e, CELL, BASE);
    mov64(e, END, BASE);
    add64_imm32(e, END, TAPE_INITIAL as u32);
}

fn emit_exit_ok_windows(e: &mut Emitter) {
    e.span("exit_ok: ExitProcess(0)");
    xor32(e, RCX);
    call_got(e, IAT_EXITPROCESS);
}

fn emit_output_windows(e: &mut Emitter, rt: &Rt) {
    e.span("Output: WriteFile(stdout, cell, 1, &n, NULL)");
    // WriteFile(h, buf, n, &n, lpOverlapped): rcx, rdx, r8, r9, [rsp+32].
    mov64(e, RCX, RBX);
    mov64(e, RDX, CELL);
    mov_r32_imm32(e, 8, 1);
    lea_r9_rsp(e, 56); // &written
    mov_qword_rsp(e, 32, 0); // lpOverlapped = NULL
    call_got(e, IAT_WRITEFILE);
    test_eax(e);
    jcc(e, CC_E, rt.err_io); // FALSE -> error
    cmp_qword_rsp(e, 56, 1);
    jcc(e, CC_NE, rt.err_io); // short write -> error
}

fn emit_input_windows(e: &mut Emitter, rt: &Rt) {
    e.span("Input: ReadFile(stdin, cell, 1, &n, NULL); EOF leaves cell unchanged");
    mov64(e, RCX, R15);
    mov64(e, RDX, CELL);
    mov_r32_imm32(e, 8, 1);
    lea_r9_rsp(e, 56);
    mov_qword_rsp(e, 32, 0);
    call_got(e, IAT_READFILE);
    test_eax(e);
    jcc(e, CC_E, rt.err_io);
    // TRUE with *n == 0 is EOF, and ReadFile leaves the buffer untouched —
    // bfrun's convention exactly. TRUE with *n == 1 wrote the cell.
    cmp_qword_rsp(e, 56, 1);
    let done = e.internal_label();
    jcc(e, CC_E, done);
    cmp_qword_rsp(e, 56, 0);
    jcc(e, CC_NE, rt.err_io);
    e.bind_here(done);
}

/// Windows grow commit: `VirtualAlloc(rdi, rsi, MEM_COMMIT, PAGE_READWRITE)`.
fn commit_windows(e: &mut Emitter, fail: u32) {
    mov64(e, RCX, RDI); // addr
    mov64(e, RDX, RSI); // len
    mov_r32_imm32(e, 8, MEM_COMMIT);
    mov_r32_imm32(e, 9, PAGE_READWRITE);
    call_got(e, IAT_VIRTUALALLOC);
    test_rax(e);
    jcc(e, CC_E, fail);
}

fn emit_errors_windows(e: &mut Emitter, rt: &Rt) {
    e.span("error tail (windows): GetStdHandle(stderr), WriteFile, ExitProcess(1)");
    let tail = e.internal_label();
    e.bind_here(tail);
    // rsi = message, rdi = length (set by each jump site).
    mov_r32_imm32(e, RCX, STD_ERROR_HANDLE as u32);
    call_got(e, IAT_GETSTDHANDLE);
    mov64(e, RCX, RAX); // handle
    mov64(e, RDX, RSI); // buffer
    e.bytes(&[0x49, 0x89, 0xF8]); // mov r8, rdi (length; REX.B for r8 as rm)
    lea_r9_rsp(e, 56); // &written
    mov_qword_rsp(e, 32, 0); // lpOverlapped = NULL
    call_got(e, IAT_WRITEFILE);
    mov_r32_imm32(e, RCX, 1);
    call_got(e, IAT_EXITPROCESS);
    emit_error_jumps(e, rt, &|e, off, len| {
        lea_rip(e, RSI, off);
        mov_r32_imm32(e, RDI, len);
        jmp(e, tail);
    });
}

// ---------------------------------------------------------------------------
// shared grow routine
// ---------------------------------------------------------------------------

/// Emits the tape-growth routine with an OS-specific commit sequence.
///
/// Contract (identical on every OS): `rax` = desired pointer (absolute
/// address of the cell that must become valid). Preserves rax, r12, r13;
/// updates r14 to base + newcap; clobbers the caller-saved set and rbp.
/// Exits the process through err_cap / err_grow on failure.
///
/// Capacity policy (identical on every OS):
///
/// ```text
/// newcap  = min(RESERVE, max(round64k(needed + 1), round64k(2 * cap)))
/// commit  = [round64k(cap), newcap)   as read/write pages
/// ```
///
/// The invariant `committed == round64k(capacity)` holds from the entry
/// sequence onward (the initial commit is exactly TAPE_GRAN), so the commit
/// span is always exact and never re-protects already-committed memory —
/// which is also why growth can never lose tape contents. 64 KiB alignment
/// keeps every commit address valid on 4/16/64 KiB-page kernels without
/// having to query the runtime page size.
///
/// On entry to the commit closure: `rdi` = address, `rsi` = length,
/// `rbp` = newcap. The closure branches to `fail` on failure (the common
/// code has already adjusted the stack so the error path starts aligned).
fn emit_grow_common(e: &mut Emitter, rt: &Rt, commit: &dyn Fn(&mut Emitter, u32)) {
    e.span("bf_grow: extend the tape (rax = desired, in/out)");
    e.bind_here(rt.grow);
    push(e, RAX); // save desired pointer; also 16-aligns the stack for calls

    // rdx = needed = rax - r13
    mov64(e, RDX, RAX);
    sub64(e, RDX, BASE);
    cmp64_imm32(e, RDX, TAPE_RESERVE as u32);
    let cap_fail = e.internal_label();
    jcc(e, CC_AE, cap_fail);

    // rdx = candidate = round64k(needed + 1). The +1 matters: the cell at
    // offset `needed` is only valid when capacity > needed, and a plain
    // round64k(needed) would leave newcap == needed when needed is an exact
    // multiple of the granularity. (needed + 64K) & ~64Kmask is exactly
    // round64k(needed + 1), and cannot overflow (needed < RESERVE).
    add64_imm32(e, RDX, TAPE_GRAN as u32);
    and64_imm32(e, RDX, !(TAPE_GRAN - 1) as u32);

    // rcx = doubled = round64k(2 * cap)
    mov64(e, RCX, END);
    sub64(e, RCX, BASE);
    add64(e, RCX, RCX);
    add64_imm32(e, RCX, (TAPE_GRAN - 1) as u32);
    and64_imm32(e, RCX, !(TAPE_GRAN - 1) as u32);

    // rcx = max(candidate, doubled)
    cmp64(e, RDX, RCX);
    let keep = e.internal_label();
    jcc(e, CC_B, keep); // candidate <= doubled -> keep doubled
    mov64(e, RCX, RDX);
    e.bind_here(keep);
    // rcx = min(rcx, RESERVE)
    cmp64_imm32(e, RCX, TAPE_RESERVE as u32);
    let keep2 = e.internal_label();
    jcc(e, CC_B, keep2);
    mov_r32_imm32(e, RCX, TAPE_RESERVE as u32); // zero-extends
    e.bind_here(keep2);

    // rbp = newcap (callee-saved: survives the mprotect/VirtualAlloc call)
    mov64(e, RBP, RCX);

    // Commit [round64k(cap), newcap): rcx = committed start, rsi = len,
    // rdi = addr. The len == 0 case is real (the first growth from 30000
    // to 65536 needs no new pages), and skipping it matters on Windows,
    // where VirtualAlloc rejects zero-sized commits.
    mov64(e, RCX, END);
    sub64(e, RCX, BASE);
    add64_imm32(e, RCX, (TAPE_GRAN - 1) as u32);
    and64_imm32(e, RCX, !(TAPE_GRAN - 1) as u32);
    mov64(e, RSI, RBP);
    sub64(e, RSI, RCX); // len
    test_r64_r64(e, RSI, RSI);
    let commit_fail = e.internal_label();
    let no_commit = e.internal_label();
    jcc(e, CC_E, no_commit);
    lea(e, RDI, RCX, BASE); // addr = r13 + committed start
    commit(e, commit_fail);
    e.bind_here(no_commit);

    // r14 = r13 + newcap; restore rax; return.
    lea(e, END, RBP, BASE);
    pop(e, RAX);
    e.u8(0xC3); // ret

    e.bind_here(cap_fail);
    e.span("  cap fail");
    pop(e, RCX); // discard saved pointer, restore alignment
    jmp(e, rt.err_cap);
    e.bind_here(commit_fail);
    e.span("  commit fail");
    pop(e, RCX);
    jmp(e, rt.err_grow);
}

// ---------------------------------------------------------------------------
// error path helpers
// ---------------------------------------------------------------------------

/// Emits one labeled entry per runtime error. Each loads the message
/// address/length into the registers the shared tail expects and jumps to
/// it. `load(off, len)` is per-OS (rsi/rdx on Unix, rsi/rdi on Windows).
fn emit_error_jumps(e: &mut Emitter, rt: &Rt, load: &dyn Fn(&mut Emitter, u32, u32)) {
    let cases = [
        (
            rt.err_underflow,
            MSG_UNDERFLOW,
            "pointer moved left of cell 0",
        ),
        (rt.err_io, MSG_IO, "I/O error"),
        (rt.err_cap, MSG_CAP, "tape limit exceeded"),
        (rt.err_grow, MSG_GROW, "failed to grow tape"),
        (rt.err_alloc, MSG_ALLOC, "failed to reserve tape memory"),
    ];
    for (label, msg, note) in cases {
        e.bind_here(label);
        e.span(format!("error path: {note}"));
        let off = e.string(msg);
        load(e, off, msg.len() as u32);
    }
}

// ---------------------------------------------------------------------------
// low-level encoding helpers (comments give the exact byte layout)
// ---------------------------------------------------------------------------

/// `add byte ptr [r12], imm8` — 41 80 04 24 ib. The 8-bit ALU op wraps
/// mod 256 in hardware, matching bfrun's `wrapping_add`.
fn add_cell(e: &mut Emitter, n: u8) {
    e.bytes(&[0x41, 0x80, 0x04, 0x24, n]);
}

/// `sub byte ptr [r12], imm8` — 41 80 2C 24 ib.
fn sub_cell(e: &mut Emitter, n: u8) {
    e.bytes(&[0x41, 0x80, 0x2C, 0x24, n]);
}

/// `mov byte ptr [r12], 0` — 41 C6 04 24 00.
fn zero_cell(e: &mut Emitter) {
    e.bytes(&[0x41, 0xC6, 0x04, 0x24, 0x00]);
}

/// `cmp byte ptr [r12], 0` — 41 80 3C 24 00.
fn cmp_cell_zero(e: &mut Emitter) {
    e.bytes(&[0x41, 0x80, 0x3C, 0x24, 0x00]);
}

/// `mov r64, r64` — REX.W[+R][+B] 89 11reg_rm.
fn mov64(e: &mut Emitter, dst: u8, src: u8) {
    let (mut rex, s) = reg_field(src);
    let d = rm_field(dst, &mut rex);
    e.u8(rex);
    e.u8(0x89);
    e.u8(0xC0 | (s << 3) | d);
}

/// `add r64, r64` — REX 01 /r.
fn add64(e: &mut Emitter, dst: u8, src: u8) {
    let (mut rex, s) = reg_field(src);
    let d = rm_field(dst, &mut rex);
    e.u8(rex);
    e.u8(0x01);
    e.u8(0xC0 | (s << 3) | d);
}

/// `sub r64, r64` — REX 29 /r.
fn sub64(e: &mut Emitter, dst: u8, src: u8) {
    let (mut rex, s) = reg_field(src);
    let d = rm_field(dst, &mut rex);
    e.u8(rex);
    e.u8(0x29);
    e.u8(0xC0 | (s << 3) | d);
}

/// `cmp r64, r64` — REX 39 /r.
fn cmp64(e: &mut Emitter, lhs: u8, rhs: u8) {
    let (mut rex, s) = reg_field(rhs);
    let d = rm_field(lhs, &mut rex);
    e.u8(rex);
    e.u8(0x39);
    e.u8(0xC0 | (s << 3) | d);
}

/// `test r64, r64` — REX 85 /r.
fn test_r64_r64(e: &mut Emitter, lhs: u8, rhs: u8) {
    let (mut rex, s) = reg_field(rhs);
    let d = rm_field(lhs, &mut rex);
    e.u8(rex);
    e.u8(0x85);
    e.u8(0xC0 | (s << 3) | d);
}

/// `lea rax, [r12 + disp]` — 49 8D 44 24 ib / 49 8D 84 24 id.
fn lea_rax_cell(e: &mut Emitter, disp: i64) {
    if let Ok(d8) = i8::try_from(disp) {
        e.bytes(&[0x49, 0x8D, 0x44, 0x24, d8 as u8]);
    } else {
        e.bytes(&[0x49, 0x8D, 0x84, 0x24]);
        e.u32(disp as u32);
    }
}

/// `lea dst, [base + index]` — REX 8D 04|dst<<3 SIB. Always uses the
/// mod=01 + disp8=0 form: a mod=00 SIB with base field 101 means "no base
/// register, disp32 follows" *even when REX.B is set*, so [r13+x] can only
/// be encoded with a zero displacement byte present.
fn lea(e: &mut Emitter, dst: u8, index: u8, base: u8) {
    let (mut rex, d) = reg_field(dst);
    let i = index_field(index, &mut rex);
    let b = rm_field(base, &mut rex);
    e.u8(rex);
    e.u8(0x8D);
    e.u8(0x44 | (d << 3)); // mod=01, rm=100 -> SIB + disp8 follow
    e.u8(i << 3 | b); // scale=0, index, base
    e.u8(0); // disp8 = 0
}

/// `cmp r64, imm32` (sign-extended) — REX 81 /7 id.
fn cmp64_imm32(e: &mut Emitter, dst: u8, imm: u32) {
    let (rex, d) = if dst >= 8 {
        (0x49, dst - 8)
    } else {
        (0x48, dst)
    };
    e.u8(rex);
    e.u8(0x81);
    e.u8(0xF8 | d);
    e.u32(imm);
}

/// `cmp rax, imm32` — 48 3D id (compact accumulator form).
fn cmp_rax_imm32(e: &mut Emitter, imm: u32) {
    e.bytes(&[0x48, 0x3D]);
    e.u32(imm);
}

/// `cmp r64, imm8` — REX 83 /7 ib.
fn cmp64_imm8(e: &mut Emitter, dst: u8, imm: i8) {
    let (rex, d) = if dst >= 8 {
        (0x49, dst - 8)
    } else {
        (0x48, dst)
    };
    e.bytes(&[rex, 0x83, 0xF8 | d, imm as u8]);
}

/// `add r64, imm32` — REX 81 /0 id.
fn add64_imm32(e: &mut Emitter, dst: u8, imm: u32) {
    let (rex, d) = if dst >= 8 {
        (0x49, dst - 8)
    } else {
        (0x48, dst)
    };
    e.u8(rex);
    e.u8(0x81);
    e.u8(0xC0 | d);
    e.u32(imm);
}

/// `sub r64, imm32` — REX 81 /5 id.
fn sub64_imm32(e: &mut Emitter, dst: u8, imm: u32) {
    let (rex, d) = if dst >= 8 {
        (0x49, dst - 8)
    } else {
        (0x48, dst)
    };
    e.u8(rex);
    e.u8(0x81);
    e.u8(0xE8 | d);
    e.u32(imm);
}

/// `and r64, imm32` (sign-extended mask) — REX 81 /4 id.
fn and64_imm32(e: &mut Emitter, dst: u8, imm: u32) {
    let (rex, d) = if dst >= 8 {
        (0x49, dst - 8)
    } else {
        (0x48, dst)
    };
    e.u8(rex);
    e.u8(0x81);
    e.u8(0xE0 | d);
    e.u32(imm);
}

/// `mov r32, imm32` (zero-extends to 64) — B8+r / 41 B8+r.
fn mov_r32_imm32(e: &mut Emitter, dst: u8, imm: u32) {
    if dst >= 8 {
        e.bytes(&[0x41, 0xB8 | (dst - 8)]);
    } else {
        e.u8(0xB8 | dst);
    }
    e.u32(imm);
}

/// `xor r32, r32` — [REX] 31 /r (zeroes the full 64-bit register).
/// High registers need both the REX prefix and the real 0x31 opcode byte.
fn xor32(e: &mut Emitter, dst: u8) {
    let d = dst & 7;
    if dst >= 8 {
        e.bytes(&[0x45, 0x31, 0xC0 | (d << 3) | d]);
    } else {
        e.bytes(&[0x31, 0xC0 | (d << 3) | d]);
    }
}

/// `jmp rel32` — E9 cd.
fn jmp(e: &mut Emitter, label: u32) {
    e.u8(0xE9);
    e.x86_rel32(PatchTarget::Label(label));
}

/// `jcc rel32` — 0F 8x cd. Always the long form: fixed 6-byte size means
/// branch targets never need distance-dependent relaxation.
fn jcc(e: &mut Emitter, cond: u8, label: u32) {
    e.bytes(&[0x0F, 0x80 | cond]);
    e.x86_rel32(PatchTarget::Label(label));
}

/// `call rel32` — E8 cd.
fn call(e: &mut Emitter, label: u32) {
    e.u8(0xE8);
    e.x86_rel32(PatchTarget::Label(label));
}

/// `call qword ptr [rip + disp32]` — FF 15 cd (GOT/IAT slot).
fn call_got(e: &mut Emitter, slot: u32) {
    e.bytes(&[0xFF, 0x15]);
    e.x86_rel32(PatchTarget::Got(slot));
}

/// `lea reg, [rip + disp32]` — REX 8D 05|reg<<3 cd (mod=00, rm=101).
fn lea_rip(e: &mut Emitter, dst: u8, str_off: u32) {
    let (rex, d) = reg_field(dst);
    e.u8(rex);
    e.u8(0x8D);
    e.u8(0x05 | (d << 3));
    e.x86_rel32(PatchTarget::Str(str_off));
}

/// `test eax, eax` — 85 C0.
fn test_eax(e: &mut Emitter) {
    e.bytes(&[0x85, 0xC0]);
}

/// `test rax, rax` — 48 85 C0.
fn test_rax(e: &mut Emitter) {
    e.bytes(&[0x48, 0x85, 0xC0]);
}

/// `push r64` / `pop r64` — 50+r / 58+r (this backend only pushes rax/rcx).
fn push(e: &mut Emitter, reg: u8) {
    debug_assert!(reg < 8, "no high-register pushes in this backend");
    e.u8(0x50 | reg);
}

fn pop(e: &mut Emitter, reg: u8) {
    debug_assert!(reg < 8, "no high-register pops in this backend");
    e.u8(0x58 | reg);
}

/// `lea r9, [rsp + disp8]` — 4C 8D 4C 24 ib.
fn lea_r9_rsp(e: &mut Emitter, disp: u8) {
    e.bytes(&[0x4C, 0x8D, 0x4C, 0x24, disp]);
}

/// `mov qword ptr [rsp + disp8], imm32` — 48 C7 44 24 ib id.
fn mov_qword_rsp(e: &mut Emitter, disp: u8, val: u32) {
    e.bytes(&[0x48, 0xC7, 0x44, 0x24, disp]);
    e.u32(val);
}

/// `cmp qword ptr [rsp + disp8], imm8` — 48 83 7C 24 ib ib.
fn cmp_qword_rsp(e: &mut Emitter, disp: u8, imm: i8) {
    e.bytes(&[0x48, 0x83, 0x7C, 0x24, disp, imm as u8]);
}

/// `syscall` — 0F 05. Clobbers rcx, r11, rax.
fn syscall(e: &mut Emitter) {
    e.bytes(&[0x0F, 0x05]);
}

/// `cmp rax, -4095` — the Linux "rax holds -errno" check. Pair with `jae`
/// (unsigned >= 0xFFFFF001 selects exactly the -4095..-1 error range).
fn check_errno(e: &mut Emitter) {
    cmp_rax_imm32(e, 0xFFFF_F001);
}

/// REX prefix and 3-bit register field for the reg (source) position.
fn reg_field(reg: u8) -> (u8, u8) {
    if reg >= 8 {
        (0x4C, reg - 8)
    } else {
        (0x48, reg)
    }
}

/// 3-bit field for the r/m position, setting REX.B when needed.
fn rm_field(reg: u8, rex: &mut u8) -> u8 {
    if reg >= 8 {
        *rex |= 0x01;
        reg - 8
    } else {
        reg
    }
}

/// 3-bit field for the SIB index position, setting REX.X when needed.
fn index_field(reg: u8, rex: &mut u8) -> u8 {
    if reg >= 8 {
        *rex |= 0x02;
        reg - 8
    } else {
        reg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::Module;
    use crate::target::Target;

    fn module(ops: &[Op], name: &str) -> Module {
        crate::codegen::emit(ops, Target::from_name(name).unwrap())
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    #[test]
    fn add_cell_encodes_as_imm8_alu() {
        let mut e = Emitter::new(0);
        add_cell(&mut e, 5);
        assert_eq!(e.code, vec![0x41, 0x80, 0x04, 0x24, 0x05]);
        sub_cell(&mut e, 255);
        assert_eq!(&e.code[5..], &[0x41, 0x80, 0x2C, 0x24, 0xFF]);
    }

    #[test]
    fn zero_is_a_single_store() {
        let mut e = Emitter::new(0);
        zero_cell(&mut e);
        assert_eq!(e.code, vec![0x41, 0xC6, 0x04, 0x24, 0x00]);
    }

    #[test]
    fn mov64_r12_rax_has_rex_b() {
        let mut e = Emitter::new(0);
        mov64(&mut e, CELL, RAX); // mov r12, rax
        assert_eq!(e.code, vec![0x49, 0x89, 0xC4]);
        let mut e = Emitter::new(0);
        mov64(&mut e, RBX, RAX); // mov rbx, rax
        assert_eq!(e.code, vec![0x48, 0x89, 0xC3]);
    }

    #[test]
    fn cmp_rax_r14_uses_rex_r() {
        let mut e = Emitter::new(0);
        cmp64(&mut e, RAX, END); // cmp rax, r14
        assert_eq!(e.code, vec![0x4C, 0x39, 0xF0]);
    }

    #[test]
    fn lea_r13_plus_rcx_uses_the_disp8_form() {
        // [r13 + rcx] cannot use mod=00 (SIB base 101 means "no base" even
        // with REX.B), so the encoder must fall back to mod=01 + disp8 0.
        let mut e = Emitter::new(0);
        lea(&mut e, RDI, RCX, BASE); // lea rdi, [r13 + rcx]
        assert_eq!(e.code, vec![0x49, 0x8D, 0x7C, 0x0D, 0x00]);
        let mut e = Emitter::new(0);
        lea(&mut e, END, RBP, BASE); // lea r14, [r13 + rbp]
        assert_eq!(e.code, vec![0x4D, 0x8D, 0x74, 0x2D, 0x00]);
    }

    #[test]
    fn grow_routine_uses_push_rax_pop_rax() {
        let m = module(&[Op::MoveRight(1)], "linux-x86_64");
        // 50 = push rax, 58 = pop rax, C3 = ret must all appear.
        assert!(m.code.contains(&0x50));
        assert!(m.code.contains(&0x58));
        assert!(m.code.contains(&0xC3));
    }

    #[test]
    fn jump_if_zero_aims_one_past_target() {
        let ops = [Op::Add(1), Op::JumpIfZero { target: 3 }];
        let m = module(&ops, "linux-x86_64");
        let pattern: &[u8] = &[0x41, 0x80, 0x3C, 0x24, 0x00, 0x0F, 0x84];
        let pos = find(&m.code, pattern).expect("cmp/je sequence");
        let rel32_pos = pos as u32 + pattern.len() as u32;
        let fix = m
            .fixups
            .iter()
            .find(|f| f.pos == rel32_pos)
            .expect("je fixup");
        assert!(matches!(fix.target, PatchTarget::Label(4)));
    }

    #[test]
    fn entries_align_stack_on_macos_and_windows() {
        assert_eq!(
            &module(&[Op::Zero], "macos-x86_64").code[..4],
            &[0x48, 0x83, 0xE4, 0xF0]
        );
        let win = module(&[Op::Zero], "windows-x86_64").code;
        assert_eq!(&win[..4], &[0x48, 0x83, 0xE4, 0xF0]);
        assert_eq!(&win[4..8], &[0x48, 0x83, 0xEC, 0x40]);
    }

    #[test]
    fn linux_entry_starts_with_zeroed_rdi() {
        assert_eq!(
            &module(&[Op::Zero], "linux-x86_64").code[..2],
            &[0x31, 0xFF]
        );
    }
}
