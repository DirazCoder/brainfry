//! ARM64 machine code backend.
//!
//! One instruction selector, three OS runtimes (Linux syscalls, macOS
//! libSystem through the GOT, Windows kernel32 through the IAT). Register
//! roles and the tape-growth protocol mirror `x86_64.rs`; see
//! `codegen/mod.rs` for the shared budget:
//!
//! - `x19` = cell pointer, `x20` = tape base, `x21` = tape end. Windows
//!   additionally parks the std handles in `x22`/`x23`. All of these are
//!   callee-saved in every ABI used here, so they survive libSystem /
//!   kernel32 calls; the Linux kernel preserves them across `svc`.
//! - `x18` is never touched (Windows reserves it for the TEB; other OSes
//!   reserve it for shadow-call-stacks, so avoiding it is free insurance).
//! - `x24` is a temporary inside the grow routine (callee-saved, so it
//!   survives the mprotect / VirtualAlloc call; unused elsewhere).
//! - `x16` (the architectural intra-call scratch IP1) carries function
//!   addresses for GOT/IAT calls: `adrp x16; ldr x16, [x16, #off]; blr x16`.
//!
//! **Branch strategy.** ARM64 conditional branches and cbz/cbnz only reach
//! ±1 MiB, but a single Brainfuck loop body can legitimately compile to
//! more than that, so two rules keep every branch in range regardless of
//! program size:
//!
//! 1. Any conditional whose *failure* path is far is inverted: branch
//!    locally on the good condition, then fall into an unconditional `b`
//!    (±128 MiB) to the error/exit handler.
//! 2. Bracket jumps (`JumpIfZero`/`JumpIfNonZero`) need the *taken* path to
//!    reach far, so each one emits a tiny trampoline: `cbz → STUB` (+8
//!    bytes), `STUB: b target` (far unconditional). Loop back-edges cost
//!    two branches; correctness never depends on loop-body size.

use bfformat::Op;

use crate::codegen::{
    ArmAdrpPair, Emitter, GOT_EXIT, GOT_MMAP, GOT_MPROTECT, GOT_READ, GOT_WRITE, MSG_ALLOC,
    MSG_CAP, MSG_GROW, MSG_IO, MSG_UNDERFLOW, PatchTarget, TAPE_GRAN, TAPE_INITIAL, TAPE_RESERVE,
};
use crate::target::{Os, Target};

// Register numbers.
const X0: u32 = 0;
const X1: u32 = 1;
const X2: u32 = 2;
const X3: u32 = 3;
const X4: u32 = 4;
const X5: u32 = 5;
const X6: u32 = 6;
const X8: u32 = 8; // Linux syscall number register
const X9: u32 = 9; // scratch
const X10: u32 = 10; // constant scratch
const X11: u32 = 11; // MulAdd: source value scratch
const X12: u32 = 12; // MulAdd: factor scratch
const X16: u32 = 16; // call-address scratch (IP1)
const CELL: u32 = 19; // x19
const BASE: u32 = 20; // x20
const END: u32 = 21; // x21
const X22: u32 = 22; // Windows: stdout handle
const X23: u32 = 23; // Windows: stdin handle
const X24: u32 = 24; // grow: newcap

// IAT slot indices on Windows (same order as external_symbols()).
const IAT_GETSTDHANDLE: u32 = 0;
const IAT_WRITEFILE: u32 = 1;
const IAT_READFILE: u32 = 2;
const IAT_EXITPROCESS: u32 = 3;
const IAT_VIRTUALALLOC: u32 = 4;

/// Linux syscall numbers (aarch64 unified table).
const SYS_READ: u32 = 63;
const SYS_WRITE: u32 = 64;
const SYS_MMAP: u32 = 222;
const SYS_MPROTECT: u32 = 226;
const SYS_EXIT_GROUP: u32 = 94;

const MAP_PRIVATE_ANON_LINUX: u32 = 0x02 | 0x20;
const MAP_PRIVATE_ANON_MACOS: u32 = 0x02 | 0x1000;
const PROT_READ_WRITE: u32 = 3;

/// -errno value of EINTR for read/write retries.
const EINTR: u32 = 4;

const MEM_COMMIT: u32 = 0x1000;
const MEM_RESERVE: u32 = 0x2000;
const PAGE_NOACCESS: u32 = 1;
const PAGE_READWRITE: u32 = 4;

// Condition codes.
const EQ: u32 = 0;
const NE: u32 = 1;
const HS: u32 = 2; // unsigned >= (carry set)
const LO: u32 = 3; // unsigned <

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
            emit_grow(e, &rt, Os::Linux);
            emit_errors_linux(e, &rt);
        }
        Os::Macos => {
            emit_entry_macos(e, &rt);
            emit_body(e, ops, Os::Macos, &rt);
            emit_exit_ok_macos(e);
            emit_grow(e, &rt, Os::Macos);
            emit_errors_macos(e, &rt);
        }
        Os::Windows => {
            emit_entry_windows(e, &rt);
            emit_body(e, ops, Os::Windows, &rt);
            emit_exit_ok_windows(e);
            emit_grow(e, &rt, Os::Windows);
            emit_errors_windows(e, &rt);
        }
    }
}

// ---------------------------------------------------------------------------
// body
// ---------------------------------------------------------------------------

fn emit_body(e: &mut Emitter, ops: &[Op], os: Os, rt: &Rt) {
    for (i, op) in ops.iter().enumerate() {
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
    e.bind_here(ops.len() as u32);
}

fn emit_op(e: &mut Emitter, op: &Op, i: usize, rt: &Rt) {
    match *op {
        Op::Add(n) => {
            e.span(format!("op {i}: Add({n}) — ldrb/add/strb w9"));
            ldrb_w(e, X9, CELL);
            add_w_imm(e, X9, X9, n as u32);
            strb_w(e, X9, CELL);
        }
        Op::Sub(n) => {
            e.span(format!("op {i}: Sub({n}) — ldrb/sub/strb w9"));
            ldrb_w(e, X9, CELL);
            sub_w_imm(e, X9, X9, n as u32);
            strb_w(e, X9, CELL);
        }
        Op::Zero => {
            e.span(format!("op {i}: Zero — strb wzr, [x19]"));
            strb_wzr(e, CELL);
        }
        Op::MoveRight(n) => {
            e.span(format!("op {i}: MoveRight({n}) — bounds check + grow"));
            // x0 = cell + n; if x0 >= end (unsigned) call grow, which
            // preserves x0, so a single `mov x19, x0` updates the pointer
            // on both paths. Inverted condition: the local branch skips
            // the bl on the in-bounds path; the grow path falls into bl
            // directly (bl reaches ±128 MiB, so no trampoline is needed).
            if n <= 4095 {
                add_imm(e, X0, CELL, n);
            } else {
                mov_imm32(e, X10, n);
                add_reg(e, X0, CELL, X10);
            }
            cmp_reg(e, X0, END);
            let update = e.internal_label();
            bcond(e, LO, update); // x0 < end → skip grow
            bl(e, rt.grow);
            e.bind_here(update);
            e.span("  mov x19, x0");
            mov_reg(e, CELL, X0);
        }
        Op::MoveLeft(n) => {
            e.span(format!("op {i}: MoveLeft({n}) — underflow check"));
            // x9 = cell - base (never wraps: x19 >= x20 invariant);
            // underflow iff x9 < n. Inverted branch keeps the error path's
            // far jump unconditional.
            sub_reg(e, X9, CELL, BASE);
            if n <= 4095 {
                cmp_imm(e, X9, n);
                let ok = e.internal_label();
                bcond(e, HS, ok);
                b_label(e, rt.err_underflow);
                e.bind_here(ok);
                sub_imm(e, CELL, CELL, n);
            } else {
                mov_imm32(e, X10, n);
                cmp_reg(e, X9, X10);
                let ok = e.internal_label();
                bcond(e, HS, ok);
                b_label(e, rt.err_underflow);
                e.bind_here(ok);
                sub_reg(e, CELL, CELL, X10);
            }
        }
        // bfrun executes op `target + 1` after a taken jump, so the branch
        // aims one past the paired bracket op; `target + 1 == ops.len()`
        // lands on the exit label. The taken path may be arbitrarily far,
        // so it goes through the STUB trampoline; the fall-through path
        // uses one local `b` to hop over the stub.
        Op::JumpIfZero { target } => {
            e.span(format!(
                "op {i}: JumpIfZero -> op {} — ldrb; cbz w9, stub; b stub(far)",
                target as usize + 1
            ));
            emit_bracket_jump(e, false, target + 1);
        }
        Op::JumpIfNonZero { target } => {
            e.span(format!(
                "op {i}: JumpIfNonZero -> op {} — ldrb; cbnz w9, stub; b stub(far)",
                target as usize + 1
            ));
            emit_bracket_jump(e, true, target + 1);
        }
        Op::MulAdd { offset, factor } => {
            e.span(format!(
                "op {i}: MulAdd {{ offset: {offset}, factor: {factor} }} — \
                 [x19+offset] += [x19] * factor"
            ));
            emit_mul_add(e, offset, factor, rt);
        }
        Op::Scan { stride } => {
            e.span(format!(
                "op {i}: Scan {{ stride: {stride} }} — step by stride until [x19] == 0"
            ));
            emit_scan(e, stride, rt);
        }
        Op::Output | Op::Input => unreachable!("dispatched by emit_body"),
    }
}

/// The bracket-jump trampoline. Layout (label `i` is already bound by the
/// caller at the first instruction):
///
/// ```text
/// TEST:  ldrb w9, [x19]
///        cbz/cbnz w9, STUB    ; +8, local
///        b   NEXT             ; +8, local: skip the stub when not taken
/// STUB:  b   target           ; far unconditional
/// NEXT:  ...                  ; the next op's code starts here
/// ```
fn emit_bracket_jump(e: &mut Emitter, is_nonzero: bool, target: u32) {
    ldrb_w(e, X9, CELL);
    let stub = e.internal_label();
    let next = e.internal_label();
    cbz32(e, is_nonzero, X9, stub);
    b_label(e, next);
    e.bind_here(stub);
    e.span("  stub: b (far)");
    b_label(e, target);
    e.bind_here(next);
}

/// Computes `x19 + offset` into `x0`, applying the same bounds policy a
/// literal `MoveRight`/`MoveLeft` by that many cells would have applied —
/// growing the tape on the right, erroring on underflow on the left — since
/// `MulAdd`'s `offset` and `Scan`'s per-step `stride` both stand in for a
/// move the optimizer folded away, not a new kind of addressing. Leaves
/// `x19` (the real cell pointer) untouched; the caller decides whether to
/// commit the new address into `x19`.
///
/// `magnitude` must already be the absolute value of the offset/stride and
/// fit in a u32 (both fields are i32, so `unsigned_abs()` always fits).
fn emit_bounds_checked_offset(e: &mut Emitter, magnitude: u32, is_right: bool, rt: &Rt) {
    if is_right {
        // Same shape as MoveRight: x0 = cell + magnitude; grow if it lands
        // at or past the current end.
        if magnitude <= 4095 {
            add_imm(e, X0, CELL, magnitude);
        } else {
            mov_imm32(e, X10, magnitude);
            add_reg(e, X0, CELL, X10);
        }
        cmp_reg(e, X0, END);
        let update = e.internal_label();
        bcond(e, LO, update); // x0 < end → in bounds already
        bl(e, rt.grow);
        e.bind_here(update);
    } else {
        // Same shape as MoveLeft: x9 = cell - base (never wraps); underflow
        // iff x9 < magnitude. x0 ends up holding the checked target address.
        sub_reg(e, X9, CELL, BASE);
        if magnitude <= 4095 {
            cmp_imm(e, X9, magnitude);
            let ok = e.internal_label();
            bcond(e, HS, ok);
            b_label(e, rt.err_underflow);
            e.bind_here(ok);
            sub_imm(e, X0, CELL, magnitude);
        } else {
            mov_imm32(e, X10, magnitude);
            cmp_reg(e, X9, X10);
            let ok = e.internal_label();
            bcond(e, HS, ok);
            b_label(e, rt.err_underflow);
            e.bind_here(ok);
            sub_reg(e, X0, CELL, X10);
        }
    }
}

/// `MulAdd { offset, factor }`: add `[x19] * factor` into the cell at
/// `x19 + offset`, wrapping mod 256 like every other cell write.
///
/// Does NOT zero the source cell: the optimizer emits one MulAdd per spread
/// target sharing the same source cell, followed by a single trailing Zero
/// op. Zeroing here too would make every MulAdd after the first in such a
/// group read 0 instead of the real value.
fn emit_mul_add(e: &mut Emitter, offset: i32, factor: u8, rt: &Rt) {
    // x9 = value in the source cell. If it's already 0 the loop this
    // replaces would never have run, so skip the write to that cell
    // entirely (its address might not even be valid to touch, e.g. one
    // past the current tape end on a first-growth boundary).
    ldrb_w(e, X9, CELL);
    let done = e.internal_label();
    cbz32(e, false, X9, done);

    // The bounds check below may call bf_grow, which (through the commit
    // call it makes on macOS/Windows) clobbers the whole caller-saved set —
    // x11 included, despite what an older comment here claimed. So the
    // source value is *reloaded from the cell* after the check, exactly the
    // way the x86-64 backend reloads al after its bounds check: the cell
    // itself is the only storage guaranteed to survive the call.
    emit_bounds_checked_offset(e, offset.unsigned_abs(), offset >= 0, rt);
    // x0 now holds the checked target address.
    mov_reg(e, X9, X0); // x9 = target address, freed from x0 for the mul call
    ldrb_w(e, X11, CELL); // x11 = [x19] again — reloaded, not parked
    ldrb_w(e, X10, X9); // x10 = *target
    movz(e, X12, factor as u32); // x12 = factor
    mul_w(e, X11, X11, X12); // x11 = value * factor (mod 2^32; strb truncates mod 256)
    add_w_reg(e, X10, X10, X11); // x10 = *target + value*factor
    strb_w(e, X10, X9);

    e.bind_here(done);
}

/// `Scan { stride }`: step the cell pointer by `stride` cells at a time
/// until it lands on a zero cell, applying the same per-step bounds check a
/// literal `MoveRight`/`MoveLeft` loop body would have paid for on every
/// iteration.
fn emit_scan(e: &mut Emitter, stride: i32, rt: &Rt) {
    let magnitude = stride.unsigned_abs();
    let is_right = stride >= 0;
    let top = e.internal_label();
    let done = e.internal_label();
    e.bind_here(top);
    ldrb_w(e, X9, CELL);
    cbz32(e, false, X9, done);
    emit_bounds_checked_offset(e, magnitude, is_right, rt);
    mov_reg(e, CELL, X0); // commit the checked address as the new cell pointer
    b_label(e, top);
    e.bind_here(done);
}

// ---------------------------------------------------------------------------
// Linux runtime
// ---------------------------------------------------------------------------

fn emit_entry_linux(e: &mut Emitter, rt: &Rt) {
    e.span("entry _start (linux-aarch64): mmap tape reservation, mprotect first 64 KiB");
    // mmap(NULL, RESERVE, PROT_NONE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
    mov_zr(e, X0);
    movz_lsl16(e, X1, (TAPE_RESERVE >> 16) as u32);
    mov_zr(e, X2);
    movz(e, X3, MAP_PRIVATE_ANON_LINUX);
    movn(e, X4, 0); // fd = -1
    mov_zr(e, X5);
    movz(e, X8, SYS_MMAP);
    svc(e);
    // x0 in [-4095, -1] ⇔ cmn x0, #4095 sets carry.
    cmn_imm(e, X0, 4095);
    let ok = e.internal_label();
    bcond(e, LO, ok); // carry clear → fine
    b_label(e, rt.err_alloc);
    e.bind_here(ok);
    mov_reg(e, BASE, X0);

    // mprotect(base, TAPE_GRAN, PROT_READ|PROT_WRITE)
    mov_reg(e, X0, BASE);
    movz_lsl16(e, X1, (TAPE_GRAN >> 16) as u32);
    movz(e, X2, PROT_READ_WRITE);
    movz(e, X8, SYS_MPROTECT);
    svc(e);
    cmn_imm(e, X0, 4095);
    let ok = e.internal_label();
    bcond(e, LO, ok);
    b_label(e, rt.err_alloc);
    e.bind_here(ok);

    mov_reg(e, CELL, BASE);
    movz(e, X1, TAPE_INITIAL as u32);
    add_reg(e, END, BASE, X1);
}

fn emit_exit_ok_linux(e: &mut Emitter) {
    e.span("exit_ok: exit_group(0)");
    mov_zr(e, X0);
    movz(e, X8, SYS_EXIT_GROUP);
    svc(e);
}

fn emit_output_linux(e: &mut Emitter, rt: &Rt) {
    e.span("Output: write(1, cell, 1), retry on EINTR");
    let retry = e.internal_label();
    e.bind_here(retry);
    movz(e, X8, SYS_WRITE);
    movz(e, X0, 1);
    mov_reg(e, X1, CELL);
    movz(e, X2, 1);
    svc(e);
    cmp_imm(e, X0, 1);
    let done = e.internal_label();
    bcond(e, EQ, done);
    cmn_imm(e, X0, EINTR);
    bcond(e, EQ, retry);
    b_label(e, rt.err_io);
    e.bind_here(done);
}

fn emit_input_linux(e: &mut Emitter, rt: &Rt) {
    e.span("Input: read(0, cell, 1); EOF leaves cell unchanged");
    let retry = e.internal_label();
    e.bind_here(retry);
    movz(e, X8, SYS_READ);
    mov_zr(e, X0);
    mov_reg(e, X1, CELL);
    movz(e, X2, 1);
    svc(e);
    // The kernel stores the byte straight into the cell, so x0 == 1 is the
    // whole success story.
    cmp_imm(e, X0, 1);
    let done = e.internal_label();
    bcond(e, EQ, done);
    cmn_imm(e, X0, EINTR);
    bcond(e, EQ, retry);
    // x0 == 0 is EOF: buffer (the cell) untouched = bfrun's convention.
    cbz64(e, false, X0, done);
    b_label(e, rt.err_io);
    e.bind_here(done);
}

fn emit_errors_linux(e: &mut Emitter, rt: &Rt) {
    e.span("error tail (linux): write(2, msg), exit_group(1)");
    let tail = e.internal_label();
    e.bind_here(tail);
    // x1 = message, x2 = length (set by each jump site).
    movz(e, X0, 2);
    movz(e, X8, SYS_WRITE);
    svc(e);
    movz(e, X0, 1);
    movz(e, X8, SYS_EXIT_GROUP);
    svc(e);
    emit_error_jumps(e, rt, &|e, off, len| {
        adrp_str(e, X1, off);
        movz(e, X2, len);
        b_label(e, tail);
    });
}

// ---------------------------------------------------------------------------
// macOS runtime
// ---------------------------------------------------------------------------

fn emit_entry_macos(e: &mut Emitter, rt: &Rt) {
    e.span("entry main (macos-aarch64): mmap via libSystem");
    // dyld calls the LC_MAIN entry with sp 16-byte aligned; nothing in this
    // program ever pushes, so every blr call site stays aligned.
    mov_zr(e, X0);
    movz_lsl16(e, X1, (TAPE_RESERVE >> 16) as u32);
    mov_zr(e, X2);
    movz(e, X3, MAP_PRIVATE_ANON_MACOS);
    movn(e, X4, 0);
    mov_zr(e, X5);
    call_got(e, GOT_MMAP);
    // mmap returns MAP_FAILED ((void*)-1) on error.
    cmn_imm(e, X0, 1);
    let ok = e.internal_label();
    bcond(e, NE, ok); // Z clear ⇔ x0 != -1
    b_label(e, rt.err_alloc);
    e.bind_here(ok);
    mov_reg(e, BASE, X0);

    // mprotect(base, TAPE_GRAN, RW) — BSD return: 0 or -1. Zero = success.
    mov_reg(e, X0, BASE);
    movz_lsl16(e, X1, (TAPE_GRAN >> 16) as u32);
    movz(e, X2, PROT_READ_WRITE);
    call_got(e, GOT_MPROTECT);
    let ok = e.internal_label();
    cbz32(e, false, X0, ok); // w0 view: int return, 0 = success
    b_label(e, rt.err_alloc);
    e.bind_here(ok);

    mov_reg(e, CELL, BASE);
    movz(e, X1, TAPE_INITIAL as u32);
    add_reg(e, END, BASE, X1);
}

fn emit_exit_ok_macos(e: &mut Emitter) {
    e.span("exit_ok: _exit(0)");
    mov_zr(e, X0);
    call_got(e, GOT_EXIT);
}

fn emit_output_macos(e: &mut Emitter, rt: &Rt) {
    e.span("Output: write(1, cell, 1) via GOT, retry on EINTR");
    let retry = e.internal_label();
    e.bind_here(retry);
    movz(e, X0, 1);
    mov_reg(e, X1, CELL);
    movz(e, X2, 1);
    call_got(e, GOT_WRITE);
    cmp_imm(e, X0, 1);
    let done = e.internal_label();
    bcond(e, EQ, done);
    cmn_imm(e, X0, EINTR);
    bcond(e, EQ, retry);
    b_label(e, rt.err_io);
    e.bind_here(done);
}

fn emit_input_macos(e: &mut Emitter, rt: &Rt) {
    e.span("Input: read(0, cell, 1) via GOT; EOF leaves cell unchanged");
    let retry = e.internal_label();
    e.bind_here(retry);
    mov_zr(e, X0);
    mov_reg(e, X1, CELL);
    movz(e, X2, 1);
    call_got(e, GOT_READ);
    cmp_imm(e, X0, 1);
    let done = e.internal_label();
    bcond(e, EQ, done);
    cmn_imm(e, X0, EINTR);
    bcond(e, EQ, retry);
    cbz64(e, false, X0, done);
    b_label(e, rt.err_io);
    e.bind_here(done);
}

fn emit_errors_macos(e: &mut Emitter, rt: &Rt) {
    e.span("error tail (macos): write(2, msg) via GOT, _exit(1)");
    let tail = e.internal_label();
    e.bind_here(tail);
    movz(e, X0, 2);
    call_got(e, GOT_WRITE);
    movz(e, X0, 1);
    call_got(e, GOT_EXIT);
    emit_error_jumps(e, rt, &|e, off, len| {
        adrp_str(e, X1, off);
        movz(e, X2, len);
        b_label(e, tail);
    });
}

// ---------------------------------------------------------------------------
// Windows runtime
// ---------------------------------------------------------------------------

fn emit_entry_windows(e: &mut Emitter, rt: &Rt) {
    e.span("entry (windows-aarch64): align sp, VirtualAlloc tape, std handles");
    // The loader's entry alignment is a convention, not a contract, so
    // align defensively, then take a fixed 64-byte frame: [sp+0,16) is the
    // callee scratch area the Windows ARM64 ABI reserves, [sp+16,64) is
    // ours (the lpNumberOfBytesWritten qword lives at sp+16). Nothing ever
    // changes sp again, so every blr call site is 16-byte aligned.
    mov_sp_to(e, X9);
    movz(e, X10, 15);
    and_reg(e, X9, X9, X10);
    sub_sp_reg(e, X9);
    sub_imm(e, 31, 31, 64); // sub sp, sp, #64

    // VirtualAlloc(NULL, RESERVE, MEM_RESERVE, PAGE_NOACCESS)
    mov_zr(e, X0);
    movz_lsl16(e, X1, (TAPE_RESERVE >> 16) as u32);
    movz(e, X2, MEM_RESERVE);
    movz(e, X3, PAGE_NOACCESS);
    call_got(e, IAT_VIRTUALALLOC);
    let ok = e.internal_label();
    cbz64(e, true, X0, ok); // pointer nonzero → ok
    b_label(e, rt.err_alloc);
    e.bind_here(ok);
    mov_reg(e, BASE, X0);

    // VirtualAlloc(base, TAPE_GRAN, MEM_COMMIT, PAGE_READWRITE)
    mov_reg(e, X0, BASE);
    movz_lsl16(e, X1, (TAPE_GRAN >> 16) as u32);
    movz(e, X2, MEM_COMMIT);
    movz(e, X3, PAGE_READWRITE);
    call_got(e, IAT_VIRTUALALLOC);
    let ok = e.internal_label();
    cbz64(e, true, X0, ok);
    b_label(e, rt.err_alloc);
    e.bind_here(ok);

    // x22 = GetStdHandle(STD_OUTPUT_HANDLE), x23 = GetStdHandle(STD_INPUT_HANDLE)
    movn(e, X0, 10); // ~10 = -11
    call_got(e, IAT_GETSTDHANDLE);
    mov_reg(e, X22, X0);
    movn(e, X0, 9); // ~9 = -10
    call_got(e, IAT_GETSTDHANDLE);
    mov_reg(e, X23, X0);

    mov_reg(e, CELL, BASE);
    movz(e, X1, TAPE_INITIAL as u32);
    add_reg(e, END, BASE, X1);
}

fn emit_exit_ok_windows(e: &mut Emitter) {
    e.span("exit_ok: ExitProcess(0)");
    mov_zr(e, X0);
    call_got(e, IAT_EXITPROCESS);
}

fn emit_output_windows(e: &mut Emitter, rt: &Rt) {
    e.span("Output: WriteFile(stdout, cell, 1, &n, NULL)");
    // WriteFile(h, buf, n, &n, lpOverlapped): x0..x3 + x4, all in registers
    // on AAPCS64 — no shadow space to manage.
    mov_reg(e, X0, X22);
    mov_reg(e, X1, CELL);
    movz(e, X2, 1);
    add_imm(e, X3, 31, 16); // &written = sp + 16
    mov_zr(e, X4);
    call_got(e, IAT_WRITEFILE);
    let ok = e.internal_label();
    cbz32(e, true, X0, ok); // w0 view: BOOL, TRUE → check the count
    b_label(e, rt.err_io);
    e.bind_here(ok);
    ldr_imm(e, X9, 16); // ldr x9, [sp, #16]
    cmp_imm(e, X9, 1);
    let ok2 = e.internal_label();
    bcond(e, EQ, ok2);
    b_label(e, rt.err_io);
    e.bind_here(ok2);
}

fn emit_input_windows(e: &mut Emitter, rt: &Rt) {
    e.span("Input: ReadFile(stdin, cell, 1, &n, NULL); EOF leaves cell unchanged");
    mov_reg(e, X0, X23);
    mov_reg(e, X1, CELL);
    movz(e, X2, 1);
    add_imm(e, X3, 31, 16);
    mov_zr(e, X4);
    call_got(e, IAT_READFILE);
    let ok = e.internal_label();
    cbz32(e, true, X0, ok); // w0 view: BOOL, TRUE → ok
    b_label(e, rt.err_io);
    e.bind_here(ok);
    ldr_imm(e, X9, 16);
    // *n == 1 → byte written into the cell; *n == 0 → EOF, buffer untouched
    // (bfrun's convention); anything else → error.
    cmp_imm(e, X9, 1);
    let done = e.internal_label();
    bcond(e, EQ, done);
    cbz64(e, false, X9, done);
    b_label(e, rt.err_io);
    e.bind_here(done);
}

fn emit_errors_windows(e: &mut Emitter, rt: &Rt) {
    e.span("error tail (windows): GetStdHandle(stderr), WriteFile, ExitProcess(1)");
    let tail = e.internal_label();
    e.bind_here(tail);
    // x1 = message, x2 = length (set by each jump site).
    movn(e, X0, 11); // ~11 = -12 = STD_ERROR_HANDLE
    call_got(e, IAT_GETSTDHANDLE);
    // x0 = handle; x1/x2 already carry buffer/length.
    add_imm(e, X3, 31, 16); // &written
    mov_zr(e, X4);
    call_got(e, IAT_WRITEFILE);
    movz(e, X0, 1);
    call_got(e, IAT_EXITPROCESS);
    emit_error_jumps(e, rt, &|e, off, len| {
        adrp_str(e, X1, off);
        movz(e, X2, len);
        b_label(e, tail);
    });
}

// ---------------------------------------------------------------------------
// shared grow routine
// ---------------------------------------------------------------------------

/// Contract (identical on every OS): `x0` = desired pointer. Preserves x0,
/// x19, x20, x30; updates x21 to base + newcap; clobbers x1..x10, x16, x24.
/// Exits through err_cap / err_grow on failure. Same capacity policy as the
/// x86-64 backend:
///
/// ```text
/// newcap = min(RESERVE, max(round64k(needed + 1), round64k(2 * cap)))
/// commit [round64k(cap), newcap)
/// ```
fn emit_grow(e: &mut Emitter, rt: &Rt, os: Os) {
    e.span("bf_grow: extend the tape (x0 = desired, in/out)");
    e.bind_here(rt.grow);
    // 24 bytes (an odd multiple of 8): realigns sp to 16 for the commit
    // call (grow is entered via `bl`, which leaves sp 8 mod 16), and holds
    // x30 at [sp] plus the desired pointer at [sp, #8] plus one pad qword.
    // x30 must be saved because the macOS/Windows commit path makes a real
    // call (`blr`), and ARM64 `blr` overwrites x30 — the register this
    // routine's own `ret` jumps through. x86-64 gets this for free (`call`
    // pushes the return address on the stack; nested calls can't clobber
    // it); ARM64 requires every non-leaf function to save LR explicitly.
    sub_imm(e, 31, 31, 24); // sub sp, sp, #24
    str_x30_sp(e); // str x30, [sp] — save return address
    str_x0_sp_8(e); // str x0, [sp, #8] — save desired

    // x1 = needed = x0 - x20
    sub_reg(e, X1, X0, BASE);
    movz_lsl16(e, X2, (TAPE_RESERVE >> 16) as u32);
    cmp_reg(e, X1, X2);
    let ok_cap = e.internal_label();
    bcond(e, LO, ok_cap);
    grow_error_exit(e, rt.err_cap);
    e.bind_here(ok_cap);

    // x3 = candidate = round64k(needed + 1) = (needed + 64K) & ~0xFFFF.
    // The +1 matters — see the x86-64 backend for the analysis.
    movz_lsl16(e, X9, 1); // x9 = 0x10000
    movz(e, X10, 0xFFFF); // x10 = mask
    add_reg(e, X3, X1, X9);
    bic(e, X3, X3, X10);

    // x4 = doubled = round64k(2 * cap)
    sub_reg(e, X4, END, BASE); // cap
    add_reg(e, X4, X4, X4); // 2 * cap
    add_reg(e, X4, X4, X10); // + 0xFFFF
    bic(e, X4, X4, X10);

    // x3 = newcap = min(RESERVE, max(candidate, doubled))
    cmp_reg(e, X3, X4);
    csel(e, X3, X3, X4, 8); // hi: x3 = x3 > x4 ? x3 : x4
    cmp_reg(e, X3, X2);
    csel(e, X3, X3, X2, 9); // ls: x3 = x3 <= RESERVE ? x3 : RESERVE

    // x24 = newcap — callee-saved, survives the mprotect/VirtualAlloc call.
    mov_reg(e, X24, X3);

    // x5 = committed = round64k(cap); x6 = len = newcap - committed.
    sub_reg(e, X5, END, BASE);
    add_reg(e, X5, X5, X10);
    bic(e, X5, X5, X10);
    sub_reg(e, X6, X24, X5);

    // len == 0 is a real case (the first growth, 30000 -> 65536, needs no
    // new pages) and VirtualAlloc rejects zero-sized commits, so skip.
    let no_commit = e.internal_label();
    cbz64(e, false, X6, no_commit);

    // Commit args: x0 = addr = base + committed, x1 = len.
    add_reg(e, X0, BASE, X5);
    mov_reg(e, X1, X6);

    match os {
        Os::Linux => {
            movz(e, X2, PROT_READ_WRITE);
            movz(e, X8, SYS_MPROTECT);
            svc(e);
            // -errno range check, inverted so the far jump is unconditional.
            cmn_imm(e, X0, 4095);
            let ok = e.internal_label();
            bcond(e, LO, ok);
            grow_error_exit(e, rt.err_grow);
            e.bind_here(ok);
        }
        Os::Macos => {
            movz(e, X2, PROT_READ_WRITE);
            call_got(e, GOT_MPROTECT);
            // mprotect: 0 or -1. Zero = success (int return, w0 view).
            let ok = e.internal_label();
            cbz32(e, false, X0, ok);
            grow_error_exit(e, rt.err_grow);
            e.bind_here(ok);
        }
        Os::Windows => {
            movz(e, X2, MEM_COMMIT);
            movz(e, X3, PAGE_READWRITE);
            call_got(e, IAT_VIRTUALALLOC);
            // NULL = failure (pointer check, 64-bit).
            let ok = e.internal_label();
            cbz64(e, true, X0, ok);
            grow_error_exit(e, rt.err_grow);
            e.bind_here(ok);
        }
    }

    e.bind_here(no_commit);
    // x21 = x20 + newcap; restore x0 and x30; return.
    add_reg(e, END, BASE, X24);
    ldr_x0_sp_8(e); // ldr x0, [sp, #8] — restore desired
    ldr_x30_sp(e); // ldr x30, [sp] — restore return address
    add_imm(e, 31, 31, 24); // add sp, sp, #24
    ret(e);
}

/// Error exits reached from inside grow (capacity / commit failure): drop
/// grow's 24-byte frame before jumping, mirroring the x86-64 backend's
/// `pop rcx` on the same paths, so the error tail starts from the body's
/// stack level.
fn grow_error_exit(e: &mut Emitter, target: u32) {
    add_imm(e, 31, 31, 24); // add sp, sp, #24 — undo grow's frame
    b_label(e, target);
}

// ---------------------------------------------------------------------------
// error path helpers
// ---------------------------------------------------------------------------

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
// low-level encoding helpers (comments give the exact bit layout)
// ---------------------------------------------------------------------------

/// `ldrb Wt, [Xn]` — 39400000 | Rn<<5 | Rt.
fn ldrb_w(e: &mut Emitter, rt: u32, rn: u32) {
    e.u32(0x3940_0000 | (rn << 5) | rt);
}

/// `strb Wt, [Xn]` — 39000000 | Rn<<5 | Rt.
fn strb_w(e: &mut Emitter, rt: u32, rn: u32) {
    e.u32(0x3900_0000 | (rn << 5) | rt);
}

/// `strb wzr, [Xn]` — 39000000 | Rn<<5 | 31.
fn strb_wzr(e: &mut Emitter, rn: u32) {
    e.u32(0x3900_0000 | (rn << 5) | 31);
}

/// `add Wd, Wn, #imm12` — 11000000 | imm12<<10 | Rn<<5 | Rd. The 32-bit add
/// plus the byte-wide `strb` give mod-256 wraparound for free.
fn add_w_imm(e: &mut Emitter, rd: u32, rn: u32, imm: u32) {
    debug_assert!(imm <= 4095);
    e.u32(0x1100_0000 | (imm << 10) | (rn << 5) | rd);
}

/// `add Wd, Wn, Wm` — 0B000000 | Rm<<16 | Rn<<5 | Rd. 32-bit register add;
/// the byte-wide `strb` that follows truncates the result mod 256.
fn add_w_reg(e: &mut Emitter, rd: u32, rn: u32, rm: u32) {
    e.u32(0x0B00_0000 | (rm << 16) | (rn << 5) | rd);
}

/// `mul Wd, Wn, Wm` (MADD Wd, Wn, Wm, WZR) — 1B00_7C00 | Rm<<16 | Rn<<5 | Rd.
fn mul_w(e: &mut Emitter, rd: u32, rn: u32, rm: u32) {
    e.u32(0x1B00_7C00 | (rm << 16) | (rn << 5) | rd);
}

/// `sub Wd, Wn, #imm12` — 51000000.
fn sub_w_imm(e: &mut Emitter, rd: u32, rn: u32, imm: u32) {
    debug_assert!(imm <= 4095);
    e.u32(0x5100_0000 | (imm << 10) | (rn << 5) | rd);
}

/// `add Xd, Xn, #imm12` — 91000000.
fn add_imm(e: &mut Emitter, rd: u32, rn: u32, imm: u32) {
    debug_assert!(imm <= 4095);
    e.u32(0x9100_0000 | (imm << 10) | (rn << 5) | rd);
}

/// `sub Xd, Xn, #imm12` — D1000000.
fn sub_imm(e: &mut Emitter, rd: u32, rn: u32, imm: u32) {
    debug_assert!(imm <= 4095);
    e.u32(0xD100_0000 | (imm << 10) | (rn << 5) | rd);
}

/// `add Xd, Xn, Xm` — 8B000000 | Rm<<16 | Rn<<5 | Rd.
fn add_reg(e: &mut Emitter, rd: u32, rn: u32, rm: u32) {
    e.u32(0x8B00_0000 | (rm << 16) | (rn << 5) | rd);
}

/// `sub Xd, Xn, Xm` — CB000000.
fn sub_reg(e: &mut Emitter, rd: u32, rn: u32, rm: u32) {
    e.u32(0xCB00_0000 | (rm << 16) | (rn << 5) | rd);
}

/// `and Xd, Xn, Xm` — 8A000000.
fn and_reg(e: &mut Emitter, rd: u32, rn: u32, rm: u32) {
    e.u32(0x8A00_0000 | (rm << 16) | (rn << 5) | rd);
}

/// `bic Xd, Xn, Xm` (AND NOT) — 8A200000.
fn bic(e: &mut Emitter, rd: u32, rn: u32, rm: u32) {
    e.u32(0x8A20_0000 | (rm << 16) | (rn << 5) | rd);
}

/// `cmp Xn, #imm12` (SUBS xzr) — F1000000.
fn cmp_imm(e: &mut Emitter, rn: u32, imm: u32) {
    debug_assert!(imm <= 4095);
    e.u32(0xF100_0000 | (imm << 10) | (rn << 5) | 31);
}

/// `cmp Xn, Xm` (SUBS xzr, Xn, Xm) — EB000000.
fn cmp_reg(e: &mut Emitter, rn: u32, rm: u32) {
    e.u32(0xEB00_0000 | (rm << 16) | (rn << 5) | 31);
}

/// `cmn Xn, #imm12` (ADDS xzr) — B1000000. cmn x0, #4095 sets carry exactly
/// for the Linux -errno range [-4095, -1].
fn cmn_imm(e: &mut Emitter, rn: u32, imm: u32) {
    debug_assert!(imm <= 4095);
    e.u32(0xB100_0000 | (imm << 10) | (rn << 5) | 31);
}

/// `csel Xd, Xn, Xm, cond` — 9A800000 | Rm<<16 | cond<<12 | Rn<<5 | Rd.
fn csel(e: &mut Emitter, rd: u32, rn: u32, rm: u32, cond: u32) {
    e.u32(0x9A80_0000 | (rm << 16) | (cond << 12) | (rn << 5) | rd);
}

/// `mov Xd, Xm` (ORR Xd, xzr, Xm) — AA000000 | Rm<<16 | 31<<5 | Rd.
fn mov_reg(e: &mut Emitter, rd: u32, rm: u32) {
    e.u32(0xAA00_0000 | (rm << 16) | (31 << 5) | rd);
}

/// `mov Xd, xzr` — AA1F03E0 pattern.
fn mov_zr(e: &mut Emitter, rd: u32) {
    e.u32(0xAA1F_03E0 | rd);
}

/// `movz Xd, #imm16` (hw = 0) — D2800000.
fn movz(e: &mut Emitter, rd: u32, imm: u32) {
    debug_assert!(imm <= 0xFFFF);
    e.u32(0xD280_0000 | (imm << 5) | rd);
}

/// `movz Xd, #imm16, lsl #16` (hw = 1) — D2A00000.
fn movz_lsl16(e: &mut Emitter, rd: u32, imm: u32) {
    debug_assert!(imm <= 0xFFFF);
    e.u32(0xD2A0_0000 | (imm << 5) | rd);
}

/// `movn Xd, #imm16` (~imm) — 92800000.
fn movn(e: &mut Emitter, rd: u32, imm: u32) {
    debug_assert!(imm <= 0xFFFF);
    e.u32(0x9280_0000 | (imm << 5) | rd);
}

/// `movk Xd, #imm16, lsl #hw*16` — F2800000 | hw<<21 | imm<<5 | Rd.
fn movk(e: &mut Emitter, rd: u32, imm: u32, hw: u32) {
    debug_assert!(imm <= 0xFFFF && hw <= 3);
    e.u32(0xF280_0000 | (hw << 21) | (imm << 5) | rd);
}

/// Materializes any u32 constant: `movz` + optional `movk`.
fn mov_imm32(e: &mut Emitter, rd: u32, value: u32) {
    movz(e, rd, value & 0xFFFF);
    if value > 0xFFFF {
        movk(e, rd, value >> 16, 1);
    }
}

/// `mov Xd, sp` (ADD Xd, sp, #0) — 910003E0 | Rd.
fn mov_sp_to(e: &mut Emitter, rd: u32) {
    e.u32(0x9100_03E0 | rd);
}

/// `sub sp, sp, Xn` — SUB (extended register), full 64-bit: the encoding
/// assemblers emit for this exact mnemonic is
/// `0xCB2963FF` for Xn = x9, i.e.
/// `0xCB200000 | Rm<<16 | 0x63FF` (option = UXTX, imm3 = 0, Rn = sp,
/// Rd = sp).
///
/// Why not the plain shifted-register SUB? Because in ADD/SUB (shifted
/// register), register field 31 in Rd/Rn reads as XZR, never SP — a word
/// like `0xCB000000 | Rm<<16 | 31<<5 | 31` decodes as `sub xzr, xzr, Xm`,
/// a no-op that discards its result (this is exactly the bug the
/// Windows-entry stack alignment used to have: the emitted no-op left sp
/// 8 mod 16 at every kernel32 call site). SP is only addressable as a
/// destination through the extended-register form, with option = UXTX
/// (011) and imm3 = 0 so the full 64-bit register subtracts unchanged.
fn sub_sp_reg(e: &mut Emitter, rm: u32) {
    e.u32(0xCB20_0000 | (rm << 16) | 0x63FF);
}

/// `str x30, [sp]` — F90003FE. Grow's return-address save: `blr` overwrites
/// x30, so a routine that calls and then returns must spill LR first.
fn str_x30_sp(e: &mut Emitter) {
    e.u32(0xF900_03FE);
}

/// `ldr x30, [sp]` — F94003FE.
fn ldr_x30_sp(e: &mut Emitter) {
    e.u32(0xF940_03FE);
}

/// `str x0, [sp, #8]` — F9000BE0. Grow parks the desired pointer one slot
/// past the saved x30.
fn str_x0_sp_8(e: &mut Emitter) {
    e.u32(0xF900_07E0);
}

/// `ldr x0, [sp, #8]` — F9400BE0.
fn ldr_x0_sp_8(e: &mut Emitter) {
    e.u32(0xF940_07E0);
}

/// `ldr Xt, [sp, #imm]` (imm multiple of 8) — F9400000 | (imm/8)<<10 | 31<<5 | Rt.
fn ldr_imm(e: &mut Emitter, rt: u32, imm: u32) {
    debug_assert!(imm.is_multiple_of(8) && imm / 8 < 4096);
    e.u32(0xF940_0000 | ((imm / 8) << 10) | (31 << 5) | rt);
}

/// `ret` — D65F03C0.
fn ret(e: &mut Emitter) {
    e.u32(0xD65F_03C0);
}

/// `svc #0` — D4000001. Clobbers x0 (return), x1, x8.
fn svc(e: &mut Emitter) {
    e.u32(0xD400_0001);
}

/// `b label` — 14000000 | imm26 (patched).
fn b_label(e: &mut Emitter, label: u32) {
    e.arm_b(false, PatchTarget::Label(label));
}

/// `bl label` — 94000000 | imm26 (patched).
fn bl(e: &mut Emitter, label: u32) {
    e.arm_b(true, PatchTarget::Label(label));
}

/// `b.cond label` — 54000000 | imm19<<5 | cond (patched).
fn bcond(e: &mut Emitter, cond: u32, label: u32) {
    e.arm_cond(0x5400_0000 | cond, PatchTarget::Label(label));
}

/// `cbz`/`cbnz Wt, label` (32-bit register view) — 34000000/35000000 |
/// imm19<<5 | Rt (patched). Use for W-register tests (bytes, BOOLs,
/// 32-bit int returns).
fn cbz32(e: &mut Emitter, is_nonzero: bool, rt_reg: u32, label: u32) {
    let base = if is_nonzero { 0x3500_0000 } else { 0x3400_0000 };
    e.arm_cond(base | rt_reg, PatchTarget::Label(label));
}

/// `cbz`/`cbnz Xt, label` (64-bit register view) — B4000000/B5000000.
/// Required for pointer checks: the 32-bit form only tests the low half,
/// which misreads pointers like 0x1_00000000 as zero.
fn cbz64(e: &mut Emitter, is_nonzero: bool, rt_reg: u32, label: u32) {
    let base = if is_nonzero { 0xB500_0000 } else { 0xB400_0000 };
    e.arm_cond(base | rt_reg, PatchTarget::Label(label));
}

/// `adrp x16, <GOT/IAT slot>; ldr x16, [x16, #off]; blr x16` — the call
/// sequence for both libSystem (macOS) and kernel32 (Windows) targets.
/// x16 is the architectural scratch for indirect calls, so clobbering it is
/// explicitly allowed by every ABI here.
fn call_got(e: &mut Emitter, slot: u32) {
    e.arm_adrp_pair(X16, ArmAdrpPair::Ldr, PatchTarget::Got(slot));
    e.u32(0xD63F_0200); // blr x16
}

/// `adrp Xt, <string>; add Xt, Xt, #lo12` — string address materialization.
fn adrp_str(e: &mut Emitter, rd: u32, str_off: u32) {
    e.arm_adrp_pair(rd, ArmAdrpPair::Add, PatchTarget::Str(str_off));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w32(code: &[u8], pos: usize) -> u32 {
        u32::from_le_bytes(code[pos..pos + 4].try_into().unwrap())
    }

    #[test]
    fn known_golden_encodings() {
        let mut e = Emitter::new(0);
        ret(&mut e);
        assert_eq!(w32(&e.code, 0), 0xD65F_03C0);
        let mut e = Emitter::new(0);
        svc(&mut e);
        assert_eq!(w32(&e.code, 0), 0xD400_0001);
        let mut e = Emitter::new(0);
        e.u32(0xD63F_0200); // blr x16 (emitted inline by call_got)
        assert_eq!(w32(&e.code, 0), 0xD63F_0200);
    }

    #[test]
    fn mov_and_byte_ops_match_reference_encodings() {
        let mut e = Emitter::new(0);
        mov_zr(&mut e, X0); // mov x0, xzr
        assert_eq!(w32(&e.code, 0), 0xAA1F_03E0);

        let mut e = Emitter::new(0);
        add_w_imm(&mut e, X0, X0, 1); // add w0, w0, #1
        assert_eq!(w32(&e.code, 0), 0x1100_0400);

        let mut e = Emitter::new(0);
        ldrb_w(&mut e, X9, CELL);
        assert_eq!(w32(&e.code, 0), 0x3940_0269);

        let mut e = Emitter::new(0);
        strb_wzr(&mut e, CELL);
        assert_eq!(w32(&e.code, 0), 0x3900_027F);

        let mut e = Emitter::new(0);
        mov_reg(&mut e, X1, CELL); // mov x1, x19
        assert_eq!(w32(&e.code, 0), 0xAA13_03E1);
    }

    #[test]
    fn movz_movk_build_big_constants() {
        let mut e = Emitter::new(0);
        movz_lsl16(&mut e, X1, 0x4000); // 0x4000_0000
        assert_eq!(w32(&e.code, 0), 0xD2A8_0001);
        let mut e = Emitter::new(0);
        mov_imm32(&mut e, X10, 0x1234_5678);
        assert_eq!(w32(&e.code, 0), 0xD28A_CF0A); // movz x10, #0x5678
        assert_eq!(w32(&e.code, 4), 0xF2A2_468A); // movk x10, #0x1234, lsl #16
    }

    #[test]
    fn grow_call_is_bl_not_b() {
        // bf_grow returns, so MoveRight must reach it with bl (0x94......),
        // never a plain b.
        let ops = [Op::MoveRight(1)];
        let m = crate::codegen::emit(
            &ops,
            crate::target::Target::from_name("linux-aarch64").unwrap(),
        );
        assert!(
            m.code
                .chunks(4)
                .any(|w| u32::from_le_bytes(w.try_into().unwrap()) & 0xFC00_0000 == 0x9400_0000)
        );
    }

    #[test]
    fn bracket_jump_emits_trampoline_and_far_branch() {
        let ops = [Op::Add(1), Op::JumpIfZero { target: 3 }];
        let m = crate::codegen::emit(
            &ops,
            crate::target::Target::from_name("linux-aarch64").unwrap(),
        );
        let armbs: Vec<_> = m
            .fixups
            .iter()
            .filter(|f| matches!(f.kind, crate::codegen::PatchKind::ArmB))
            .collect();
        assert!(
            armbs.len() >= 2,
            "expected a local skip and a far jump, got {armbs:?}"
        );
        assert!(
            armbs
                .iter()
                .any(|f| matches!(f.target, PatchTarget::Label(4)))
        );
    }

    #[test]
    fn sub_sp_reg_encodes_the_extended_register_form() {
        // `sub sp, sp, x9` as assemblers encode it: SUB (extended
        // register), option UXTX, imm3 0, Rn/Rd = sp. The
        // shifted-register form cannot target sp at all (31 reads as xzr
        // there — the emitted word used to be a silent no-op).
        let mut e = Emitter::new(0);
        sub_sp_reg(&mut e, X9);
        assert_eq!(w32(&e.code, 0), 0xCB29_63FF);
        let mut e = Emitter::new(0);
        sub_sp_reg(&mut e, X24);
        assert_eq!(w32(&e.code, 0), 0xCB38_63FF);
    }

    #[test]
    fn grow_saves_x30_and_takes_an_aligned_frame() {
        // Any program that can grow must emit a grow routine that: (a)
        // saves x30 at [sp] — `blr` inside grow clobbers it, and grow
        // returns afterwards; (b) uses a 24-byte frame (an odd multiple
        // of 8) so the commit call sees 16-byte-aligned sp.
        let ops = [Op::MoveRight(40_000)];
        let m = crate::codegen::emit(
            &ops,
            crate::target::Target::from_name("macos-aarch64").unwrap(),
        );
        assert!(m.code.chunks(4).any(|w| {
            // sub sp, sp, #24
            u32::from_le_bytes(w.try_into().unwrap()) == 0xD100_63FF
        }));
        assert!(m.code.chunks(4).any(|w| {
            // str x30, [sp]
            u32::from_le_bytes(w.try_into().unwrap()) == 0xF900_03FE
        }));
        assert!(m.code.chunks(4).any(|w| {
            // ldr x30, [sp]
            u32::from_le_bytes(w.try_into().unwrap()) == 0xF940_03FE
        }));
    }

    #[test]
    fn mul_add_emits_a_multiply() {
        let ops = [Op::MulAdd {
            offset: 1,
            factor: 3,
        }];
        let m = crate::codegen::emit(
            &ops,
            crate::target::Target::from_name("linux-aarch64").unwrap(),
        );
        // MADD (mul_w) top byte pattern: 0001_1011_000..... with the low
        // 15 bits masked off (Ra field must be 11111 = xzr for a plain mul).
        assert!(m.code.chunks(4).any(|w| {
            let word = u32::from_le_bytes(w.try_into().unwrap());
            word & 0xFFE0_FC00 == 0x1B00_7C00
        }));
    }

    #[test]
    fn mul_add_negative_offset_checks_underflow() {
        let ops = [Op::MulAdd {
            offset: -5,
            factor: 1,
        }];
        let m = crate::codegen::emit(
            &ops,
            crate::target::Target::from_name("linux-aarch64").unwrap(),
        );
        // The message lives in the read-only blob, so both buffers are
        // searched.
        let underflow_msg = b"pointer moved left of cell 0";
        let in_code = m
            .code
            .windows(underflow_msg.len())
            .any(|w| w == underflow_msg);
        let in_rodata = m
            .rodata
            .windows(underflow_msg.len())
            .any(|w| w == underflow_msg);
        assert!(in_code || in_rodata);
    }

    #[test]
    fn scan_loops_back_to_its_own_start() {
        let ops = [Op::Scan { stride: 1 }];
        let m = crate::codegen::emit(
            &ops,
            crate::target::Target::from_name("linux-aarch64").unwrap(),
        );
        // emit_scan binds its own internal `top` label at the same code
        // offset as the op's label, so the check is "some fixup's target
        // label resolves to the offset of label 0".
        let op0_offset = m.labels[0];
        assert_ne!(op0_offset, u32::MAX, "label 0 must be bound");
        assert!(
            m.fixups.iter().any(|f| match f.target {
                PatchTarget::Label(i) => {
                    (i as usize) < m.labels.len() && m.labels[i as usize] == op0_offset
                }
                _ => false,
            }),
            "no branch targets the scan's own start"
        );
    }
}