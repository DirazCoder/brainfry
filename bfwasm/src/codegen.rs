//! Compiles the optimized `bfformat::Op` list straight to a binary
//! WebAssembly module — no external assembler, no `wasm-bindgen`, no
//! `wat2wasm`. Same relationship to the front end as `bfnative`'s
//! `codegen`: same `Op` slice in, but the target here is a stack machine
//! with structured control flow instead of x86-64/ARM64 registers and
//! branch fixups, so the shape of the emitter is different in a way that
//! actually matters, not just cosmetically.
//!
//! ## Why this can't reuse `bfnative`'s `Emitter`
//!
//! `bfnative::codegen::Emitter` exists to solve one problem: you don't know
//! a branch's target address until the whole module is laid out, so every
//! jump is emitted as a placeholder byte range plus a fixup entry resolved
//! in a second pass once the final layout is known. Wasm doesn't have that
//! problem — there's no
//! such thing as a wasm branch to an absolute address. `br`/`br_if` target
//! a *label depth*, counted outward through the enclosing `block`/`loop`
//! nesting, and that depth is known the moment you're inside the nesting
//! that defines it. So the fixup machinery isn't a smaller version of what
//! bfnative needs — it's solving a problem this backend doesn't have.
//!
//! ## Register budget → one local
//! bfnative dedicates whole registers to the cell pointer and tape bounds
//! (see that crate's `codegen/mod.rs`). Wasm functions don't have a
//! register file to divide up; a function's whole working set is its
//! operand stack plus its declared locals. This backend uses exactly one:
//! local 0, `$ptr`, the current cell's *byte* offset into linear memory
//! (cell index × 1, since cells are bytes — no scaling needed). Tape bounds
//! aren't tracked in a local at all, because `memory.size` is one
//! instruction and costs nothing to call fresh at each check.
//!
//! ## Control flow: recovered nesting, not fixups
//! `bfc::optimize::resolve_jumps` leaves `[`/`]` as an
//! `Op::JumpIfZero`/`Op::JumpIfNonZero` pair pointing at each other's
//! index, and folding never removes a loop without removing both ends
//! together — so surviving loops are always well-nested. [`compile`]
//! recovers that nesting once with a bracket-matching pass over the flat
//! op list, then walks it recursively, so every `[...]` becomes exactly
//! `block $exit / loop $body / ... / br_if $body (nonzero) / end / end` —
//! standard "translate a while-loop to structured branches" shape, no
//! fixup table involved.

use crate::encode::{byte_vec, section, sleb, uleb};
use bfformat::Op;

/// Cells at startup, matching `bfrun`'s `INITIAL_TAPE_SIZE` and
/// `bfnative::codegen::TAPE_INITIAL`. Rounded up to whole 64 KiB pages
/// below, since that's wasm memory's only granularity.
const TAPE_INITIAL_CELLS: u32 = 30_000;

/// One wasm page is fixed by the spec at 64 KiB — not a tunable, just
/// naming the constant.
const PAGE_SIZE: u32 = 65_536;

/// Pages reserved for the tape at startup: `TAPE_INITIAL_CELLS` bytes
/// rounded up to a whole page. One page comfortably covers 30,000 cells
/// (fits in under half a page), but the rounding is written generally
/// rather than hardcoded to 1 in case `TAPE_INITIAL_CELLS` ever changes.
const TAPE_INITIAL_PAGES: u32 = TAPE_INITIAL_CELLS.div_ceil(PAGE_SIZE);

/// Scratch region for the WASI iovec used by every `fd_write`/`fd_read`
/// call, placed one page after the tape's initial reservation so it never
/// overlaps cells 0..30000. A program that grows the tape past this offset
/// via `memory.grow` is fine — growth only adds pages at the *end* of
/// memory, it never relocates what's already there, so this fixed offset
/// stays valid for the module's whole lifetime.
const IO_SCRATCH_OFFSET: u32 = TAPE_INITIAL_PAGES * PAGE_SIZE;

/// The iovec plus its result slot needs 12 bytes (two i32 fields for the
/// iovec, one i32 for the byte count `fd_write`/`fd_read` writes back);
/// rounding to 16 keeps everything after it word-aligned.
const IO_SCRATCH_SIZE: u32 = 16;

/// Where the single data byte read or written per op actually lives —
/// right after the iovec/result fields.
const IO_BUF_OFFSET: u32 = IO_SCRATCH_OFFSET + IO_SCRATCH_SIZE;

/// Total pages reserved before any user `memory.grow`: tape plus the fixed
/// I/O scratch page.
const RESERVED_PAGES: u32 = TAPE_INITIAL_PAGES + 1;

/// 1 GiB in pages, matching `bfnative::codegen::TAPE_RESERVE`. Wasm32
/// linear memory is capped at 4 GiB (an i32 address space) regardless, but
/// this backend caps growth at the same 1 GiB bfnative documents, so a
/// program that behaves one way on one backend behaves the same way on
/// this one instead of running until the wasm runtime's own memory limit
/// kicks in with a less specific error.
const MAX_PAGES: u32 = (1 << 30) / PAGE_SIZE;

const WASM_MAGIC: [u8; 4] = *b"\0asm";
const WASM_VERSION: [u8; 4] = [1, 0, 0, 0];

const SECTION_TYPE: u8 = 1;
const SECTION_IMPORT: u8 = 2;
const SECTION_FUNCTION: u8 = 3;
const SECTION_MEMORY: u8 = 5;
const SECTION_EXPORT: u8 = 7;
const SECTION_CODE: u8 = 10;

const TYPE_FUNC: u8 = 0x60;
const VALTYPE_I32: u8 = 0x7f;
const BLOCKTYPE_EMPTY: u8 = 0x40;

const OP_UNREACHABLE: u8 = 0x00;
const OP_BLOCK: u8 = 0x02;
const OP_LOOP: u8 = 0x03;
const OP_BR_IF: u8 = 0x0d;
const OP_CALL: u8 = 0x10;
const OP_END: u8 = 0x0b;
const OP_LOCAL_GET: u8 = 0x20;
const OP_LOCAL_TEE: u8 = 0x22;
const OP_I32_LOAD8_U: u8 = 0x2d;
const OP_I32_STORE: u8 = 0x36;
const OP_I32_LOAD: u8 = 0x28;
const OP_I32_STORE8: u8 = 0x3a;
const OP_I32_CONST: u8 = 0x41;
const OP_I32_EQZ: u8 = 0x45;
const OP_I32_NE: u8 = 0x47;
const OP_I32_LT_S: u8 = 0x48;
const OP_I32_GT_S: u8 = 0x4a;
const OP_I32_ADD: u8 = 0x6a;
const OP_I32_SUB: u8 = 0x6b;
const OP_I32_MUL: u8 = 0x6c;
const OP_MEMORY_SIZE: u8 = 0x3f;
const OP_MEMORY_GROW: u8 = 0x40;
const OP_I32_DIV_U: u8 = 0x6e;
const OPCODE_IF: u8 = 0x04;

/// WASI import slots, in the order they're declared in the import section —
/// this order is what makes function index 0 mean `fd_write` and so on, so
/// [`Body::call_wasi_io`]'s callers have to agree with it.
const WASI_FD_WRITE: u32 = 0;
const WASI_FD_READ: u32 = 1;

/// Converts a `MoveRight`/`MoveLeft` magnitude to a signed cell delta.
/// `Op`'s move counts are `u32`, but every address in this backend is a
/// wasm32 `i32`, and a plain `as i32` cast silently wraps to negative for
/// anything past `i32::MAX` (~2.1 billion) — which would turn a rightward
/// move into a bogus leftward one instead of the out-of-range error it
/// should be. `MAX_PAGES` caps the tape at 1 GiB either way, so no
/// well-behaved program's single move ever approaches this; clamping to
/// `i32::MAX` here just guarantees a single `.bf` file with billions of
/// consecutive `>`/`<` fails the ordinary bounds-check trap in `move_ptr`
/// instead of silently miscompiling.
fn move_delta(magnitude: u32) -> i32 {
    magnitude.min(i32::MAX as u32) as i32
}

/// Compiles an optimized op list to a complete `.wasm` module: WASI
/// `fd_write`/`fd_read` imports for I/O, one page-granular linear memory
/// for the tape, a `_start` function containing the whole program, and a
/// `memory` export so a host embedding this outside a WASI runtime (a
/// browser, say) can still read the tape back out after the run.
pub fn compile(ops: &[Op]) -> Vec<u8> {
    let loops = match_loops(ops);
    let mut body = Body::new();
    body.emit_block(ops, &loops, 0, ops.len());
    body.finish();

    let mut module = Vec::new();
    module.extend_from_slice(&WASM_MAGIC);
    module.extend_from_slice(&WASM_VERSION);

    section(&mut module, SECTION_TYPE, &type_section());
    section(&mut module, SECTION_IMPORT, &import_section());
    section(&mut module, SECTION_FUNCTION, &function_section());
    section(&mut module, SECTION_MEMORY, &memory_section());
    section(&mut module, SECTION_EXPORT, &export_section());
    section(&mut module, SECTION_CODE, &code_section(&body.code));

    module
}

/// Two function types: `(i32, i32, i32, i32) -> i32` for the WASI imports
/// (every `fd_write`/`fd_read` signature, regardless of how many args a
/// given call site actually varies), and `() -> ()` for `_start`, the
/// signature the WASI convention requires for the entry point.
fn type_section() -> Vec<u8> {
    let mut out = Vec::new();
    uleb(&mut out, 2);

    out.push(TYPE_FUNC);
    uleb(&mut out, 4);
    for _ in 0..4 {
        out.push(VALTYPE_I32);
    }
    uleb(&mut out, 1);
    out.push(VALTYPE_I32);

    out.push(TYPE_FUNC);
    uleb(&mut out, 0);
    uleb(&mut out, 0);

    out
}

fn import_section() -> Vec<u8> {
    let mut out = Vec::new();
    uleb(&mut out, 2);
    import_func(&mut out, "wasi_snapshot_preview1", "fd_write", 0);
    import_func(&mut out, "wasi_snapshot_preview1", "fd_read", 0);
    out
}

fn import_func(out: &mut Vec<u8>, module: &str, name: &str, type_index: u32) {
    byte_vec(out, module.as_bytes());
    byte_vec(out, name.as_bytes());
    out.push(0x00); // import kind: function
    uleb(out, type_index as u64);
}

/// One locally-defined function (`_start`), using type index 1.
fn function_section() -> Vec<u8> {
    let mut out = Vec::new();
    uleb(&mut out, 1);
    uleb(&mut out, 1);
    out
}

/// One memory, `RESERVED_PAGES` initial, `MAX_PAGES` max. Declaring the max
/// isn't just documentation — it's what lets a wasm runtime reject a
/// `memory.grow` past 1 GiB with the instruction's normal -1-return
/// failure path (checked in [`Body::emit_grow_check_for`]) instead of the
/// runtime's own unrelated resource limit.
fn memory_section() -> Vec<u8> {
    let mut out = Vec::new();
    uleb(&mut out, 1);
    out.push(0x01); // limits flag: min and max both present
    uleb(&mut out, RESERVED_PAGES as u64);
    uleb(&mut out, MAX_PAGES as u64);
    out
}

/// Exports `_start` (the WASI entry point every `wasm_exec`-style host
/// looks for) and `memory` (so the tape's final contents are inspectable
/// from outside — useful for embedding this without a WASI shell around
/// it, and free to add).
fn export_section() -> Vec<u8> {
    let mut out = Vec::new();
    uleb(&mut out, 2);
    export_entry(&mut out, "_start", 0x00, 2); // function index 2: imports take 0 and 1
    export_entry(&mut out, "memory", 0x02, 0);
    out
}

fn export_entry(out: &mut Vec<u8>, name: &str, kind: u8, index: u32) {
    byte_vec(out, name.as_bytes());
    out.push(kind);
    uleb(out, index as u64);
}

fn code_section(function_body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    uleb(&mut out, 1);
    uleb(&mut out, function_body.len() as u64);
    out.extend_from_slice(function_body);
    out
}

/// One end of a matched `[`/`]` pair, keyed by the index of *either* end so
/// [`Body::emit_block`] can look a bracket up from whichever side it's
/// currently standing on without caring which one that is.
struct LoopSpan {
    open: usize,
    close: usize,
}

/// Bracket-matches the flat op list once up front. `bfc::optimize` leaves
/// every surviving loop's `JumpIfZero`/`JumpIfNonZero` pointing at each
/// other's index already, so this is just reading that back into a nested
/// shape rather than re-deriving it — a stack of open indices, same
/// algorithm as `bfc::parser::parse` uses on raw brackets, just running
/// over ops instead of source characters.
fn match_loops(ops: &[Op]) -> Vec<LoopSpan> {
    let mut open_stack = Vec::new();
    let mut spans = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::JumpIfZero { .. } => open_stack.push(i),
            Op::JumpIfNonZero { .. } => {
                let open = open_stack.pop().expect(
                    "JumpIfNonZero with no matching JumpIfZero — optimizer produced unbalanced brackets",
                );
                spans.push(LoopSpan { open, close: i });
            }
            _ => {}
        }
    }

    assert!(
        open_stack.is_empty(),
        "JumpIfZero with no matching JumpIfNonZero — optimizer produced unbalanced brackets"
    );

    spans
}

/// Accumulates the bytes of the `_start` function body: the local
/// declaration vector, then the instruction stream.
struct Body {
    code: Vec<u8>,
}

impl Body {
    fn new() -> Self {
        let mut code = Vec::new();
        // One locals-declaration group: 3 locals, all i32 — local 0 is
        // `$ptr` (the current cell's byte offset), local 1 is `$grow_by`,
        // scratch space for emit_grow_check's page-count computation, and
        // local 2 is `$target`, used by the MulAdd optimization path.
        uleb(&mut code, 1);
        uleb(&mut code, 3);
        code.push(VALTYPE_I32);
        Body { code }
    }

    /// Closes the function body with the implicit `end` every wasm
    /// function needs, whether or not the source program's own brackets
    /// already balanced back to zero nesting.
    fn finish(&mut self) {
        self.code.push(OP_END);
    }

    /// Emits ops `[start, end)` at the current nesting depth, recursing
    /// into `emit_loop` whenever a `JumpIfZero` opens a span found in
    /// `loops`. `start`/`end` bound a single straight-line run with no
    /// bracket that isn't fully contained in it, which is exactly what
    /// `match_loops` guarantees for a well-nested program.
    fn emit_block(&mut self, ops: &[Op], loops: &[LoopSpan], start: usize, end: usize) {
        let mut i = start;
        while i < end {
            match &ops[i] {
                Op::JumpIfZero { .. } => {
                    let span = loops
                        .iter()
                        .find(|s| s.open == i)
                        .expect("match_loops covers every JumpIfZero");
                    self.emit_loop(ops, loops, span.open + 1, span.close);
                    i = span.close + 1;
                }
                Op::JumpIfNonZero { .. } => {
                    unreachable!("close bracket reached outside its own loop's recursion")
                }
                op => {
                    self.emit_op(op);
                    i += 1;
                }
            }
        }
    }

    /// One Brainfuck `[...]` as the standard structured-branch translation
    /// of a leading-condition while-loop:
    ///
    /// ```text
    /// block $exit
    ///   loop $body
    ///     <check current cell, br_if $exit if zero>
    ///     <body>
    ///     <check current cell, br_if $body if nonzero>
    ///   end
    /// end
    /// ```
    ///
    /// The entry check and the back-edge check are two different branches
    /// (`$exit` at depth 1, `$body` at depth 0 from inside the loop) rather
    /// than one shared test, because they target different labels — trying
    /// to share them would mean the depths no longer line up.
    fn emit_loop(&mut self, ops: &[Op], loops: &[LoopSpan], start: usize, end: usize) {
        self.code.push(OP_BLOCK);
        self.code.push(BLOCKTYPE_EMPTY);
        self.code.push(OP_LOOP);
        self.code.push(BLOCKTYPE_EMPTY);

        self.load_cell();
        self.code.push(OP_I32_EQZ);
        self.code.push(OP_BR_IF);
        uleb(&mut self.code, 1); // depth 1: the enclosing block ($exit) — branching there skips past its `end`

        self.emit_block(ops, loops, start, end);

        self.load_cell();
        self.code.push(OP_BR_IF);
        uleb(&mut self.code, 0); // depth 0: the loop itself ($body) — branching there re-enters at the top

        self.code.push(OP_END); // loop
        self.code.push(OP_END); // block
    }

    fn emit_op(&mut self, op: &Op) {
        match op {
            Op::Add(n) => self.arith_current_cell(*n as i32, OP_I32_ADD),
            Op::Sub(n) => self.arith_current_cell(*n as i32, OP_I32_SUB),
            Op::MoveRight(n) => self.move_ptr(move_delta(*n)),
            Op::MoveLeft(n) => self.move_ptr(-move_delta(*n)),
            Op::Output => self.emit_output(),
            Op::Input => self.emit_input(),
            Op::Zero => self.store_cell_const(0),
            Op::MulAdd { offset, factor } => self.emit_mul_add(*offset, *factor),
            Op::Scan { stride } => self.emit_scan(*stride),
            // Resolved to structured branches by emit_block/emit_loop —
            // never reached as a standalone op.
            Op::JumpIfZero { .. } | Op::JumpIfNonZero { .. } => {
                unreachable!("jumps are consumed by emit_block, not emitted directly")
            }
        }
    }

    /// `local.get $ptr / i32.load8_u`. Every op that reads the current
    /// cell's value goes through this, so there's exactly one place that
    /// knows cells are unsigned bytes.
    fn load_cell(&mut self) {
        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 0);
        self.code.push(OP_I32_LOAD8_U);
        self.mem_arg(0, 0);
    }

    /// `local.get $ptr / <value on stack> / i32.store8`, i32.store8's
    /// operand order is address-then-value, so the pointer has to go on
    /// the stack before whatever computed the value the caller already
    /// pushed — every call site pushes the pointer first for this reason.
    fn store_cell(&mut self) {
        self.code.push(OP_I32_STORE8);
        self.mem_arg(0, 0);
    }

    fn store_cell_const(&mut self, value: i32) {
        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 0);
        self.const_i32(value);
        self.store_cell();
    }

    /// `align, offset` memarg pair every load/store carries. Every access
    /// in this backend is a single unaligned byte, so align is always 0 and
    /// offset is always 0 (the pointer local already holds the full
    /// address) — factored out only so that fact is stated once instead of
    /// six call sites each writing `0x00, 0x00` with no explanation.
    fn mem_arg(&mut self, align: u32, offset: u32) {
        uleb(&mut self.code, align as u64);
        uleb(&mut self.code, offset as u64);
    }

    fn const_i32(&mut self, value: i32) {
        self.code.push(OP_I32_CONST);
        sleb(&mut self.code, value as i64);
    }

    /// `cell[$ptr] (op)= n`, wrapping mod 256 the same way `bfrun`'s `u8`
    /// arithmetic and `bfnative`'s byte-sized store both do — a plain
    /// `i32.store8` truncates to the low byte on write regardless of what
    /// the addition overflowed to, so no explicit mask is needed here.
    fn arith_current_cell(&mut self, n: i32, wasm_op: u8) {
        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 0);
        self.load_cell();
        self.const_i32(n);
        self.code.push(wasm_op);
        self.store_cell();
    }

    /// Moves `$ptr` by `delta` cells (delta already carries its sign), then
    /// checks the new position: underflow traps immediately, since there's
    /// no sane fallback for walking left of cell 0; a position past the
    /// currently-committed memory grows first. Matches
    /// `bfnative::codegen`'s per-op bounds check rather than deferring to a
    /// single boundary check per program — see the module doc comment for
    /// why a wasm trap is this backend's version of that error path.
    fn move_ptr(&mut self, delta: i32) {
        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 0);
        self.const_i32(delta);
        self.code.push(OP_I32_ADD);
        self.code.push(OP_LOCAL_TEE);
        uleb(&mut self.code, 0);

        self.const_i32(0);
        self.code.push(OP_I32_LT_S);
        self.trap_if();

        if delta > 0 {
            self.emit_grow_check_for(0); // $ptr
        }
    }

    /// `if (cond) { unreachable }` — the trap idiom every bounds check in
    /// this file uses. Deliberately not a `br_if` to a shared trap label:
    /// a wasm function body has no implicit outer label to branch to, and
    /// giving every trap site its own tiny `block` just to have somewhere
    /// to `br_if` into would be more ceremony than an inline `unreachable`
    /// guarded by an `if`. Consumes the i32 condition on the stack.
    fn trap_if(&mut self) {
        self.code.push(OPCODE_IF);
        self.code.push(BLOCKTYPE_EMPTY);
        self.code.push(OP_UNREACHABLE);
        self.code.push(OP_END);
    }

    /// If the address in local `addr_local` has walked past the memory
    /// currently committed, grows by however many pages are needed to
    /// cover it — not a flat one page, since a single move can land more
    /// than one page past the old boundary (a folded run of many `>`, or a
    /// `Scan` step, can jump further than 64 KiB in one move).
    /// `memory.grow` returns -1 on failure, which becomes a trap rather
    /// than a silently ignored error — same "clean failure, not a crash
    /// further down" intent as `bfnative`'s `MSG_GROW`/`MSG_CAP`, just
    /// without distinct messages since a trap has no stdout/stderr text of
    /// its own. Only needs to run for an address that could be past the
    /// old high-water mark: `move_ptr` calls this for `$ptr` (local 0)
    /// after a rightward move, and `bounds_checked_target` calls this for
    /// `$target` (local 2) whenever `MulAdd`'s target offset is positive —
    /// a leftward move or negative offset can only be landing on memory
    /// already committed, so only the underflow check above applies there.
    fn emit_grow_check_for(&mut self, addr_local: u32) {
        // needed_pages = (addr / PAGE_SIZE) + 1 — the page index the
        // address falls in, made 1-based so it's a page *count* covering
        // up to and including that page.
        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, addr_local as u64);
        self.const_i32(PAGE_SIZE as i32);
        self.code.push(OP_I32_DIV_U);
        self.const_i32(1);
        self.code.push(OP_I32_ADD);

        // grow_by = needed_pages - memory.size(); only grow when positive,
        // since memory.size() already covering the address's page means
        // there's nothing to do (and memory.grow(0) is a valid but
        // wasteful no-op this skips).
        self.code.push(OP_MEMORY_SIZE);
        self.code.push(0x00); // memory index 0
        self.code.push(OP_I32_SUB);
        self.code.push(OP_LOCAL_TEE);
        uleb(&mut self.code, 1); // $grow_by (local 1, declared in Body::new)

        self.const_i32(0);
        self.code.push(OP_I32_GT_S);
        self.code.push(OPCODE_IF);
        self.code.push(BLOCKTYPE_EMPTY);
        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 1); // $grow_by
        self.code.push(OP_MEMORY_GROW);
        self.code.push(0x00); // memory index 0
        self.const_i32(0);
        self.code.push(OP_I32_LT_S);
        self.trap_if(); // grow failed (returned -1)
        self.code.push(OP_END); // if
    }

    /// Writes the iovec WASI expects at `IO_SCRATCH_OFFSET`: a `buf`
    /// pointer (always `IO_BUF_OFFSET`, the one-byte data buffer right
    /// after it) and a `len` of 1. Shared by `emit_output` and
    /// `emit_input` since both only ever transfer a single byte.
    fn write_iovec(&mut self) {
        self.const_i32(IO_SCRATCH_OFFSET as i32);
        self.const_i32(IO_BUF_OFFSET as i32);
        self.code.push(OP_I32_STORE); // iovec.buf
        self.mem_arg(2, 0);
        self.const_i32(IO_SCRATCH_OFFSET as i32);
        self.const_i32(1);
        self.code.push(OP_I32_STORE); // iovec.len
        self.mem_arg(2, 4);
    }

    /// Calls `fd_write`/`fd_read` with the standard WASI four-argument
    /// shape (fd, iovs, iovs_len, nbytes-out), leaving the returned errno
    /// on the stack for the caller to check.
    fn call_wasi_io(&mut self, fd: i32, function_index: u32) {
        self.const_i32(fd);
        self.const_i32(IO_SCRATCH_OFFSET as i32); // iovs
        self.const_i32(1); // iovs_len
        self.const_i32(IO_SCRATCH_OFFSET as i32 + 8); // nbytes out-param
        self.code.push(OP_CALL);
        uleb(&mut self.code, function_index as u64);
    }

    /// Traps if the WASI errno left on the stack by `call_wasi_io` is
    /// nonzero — the wasm equivalent of `bfnative`'s raw-syscall backends
    /// checking `write`/`read`'s return value and jumping to `MSG_IO` on
    /// failure.
    fn trap_unless_zero(&mut self) {
        self.const_i32(0);
        self.code.push(OP_I32_NE);
        self.trap_if();
    }

    /// `fd_write(1, iovec_ptr, 1, nwritten_ptr)`: copies the current
    /// cell's byte into the I/O buffer, points a single iovec at it, calls
    /// out to WASI, and traps on a nonzero errno.
    fn emit_output(&mut self) {
        self.const_i32(IO_BUF_OFFSET as i32);
        self.load_cell();
        self.code.push(OP_I32_STORE8);
        self.mem_arg(0, 0);

        self.write_iovec();
        self.call_wasi_io(1, WASI_FD_WRITE);
        self.trap_unless_zero();
    }

    /// `fd_read(0, iovec_ptr, 1, nread_ptr)`, then stores the byte read
    /// into the current cell — unless `nread` came back 0 (EOF), in which
    /// case the cell is left untouched, matching `bfrun`'s exact
    /// EOF-leaves-cell-unchanged semantics rather than zeroing it or
    /// erroring.
    fn emit_input(&mut self) {
        self.write_iovec();
        self.call_wasi_io(0, WASI_FD_READ);
        self.trap_unless_zero();

        // The store-on-success path only runs conditionally, so it needs
        // its own block to skip past on EOF — unlike move_ptr's trap
        // branches, this can't just br_if straight out of the function,
        // there's nothing to unconditionally trap into here.
        self.code.push(OP_BLOCK);
        self.code.push(BLOCKTYPE_EMPTY);

        self.const_i32(IO_SCRATCH_OFFSET as i32 + 8);
        self.code.push(OP_I32_LOAD); // nread out-param
        self.mem_arg(2, 0);
        self.code.push(OP_I32_EQZ);
        self.code.push(OP_BR_IF);
        uleb(&mut self.code, 0); // EOF: skip the store below

        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 0);
        self.const_i32(IO_BUF_OFFSET as i32);
        self.code.push(OP_I32_LOAD8_U);
        self.mem_arg(0, 0);
        self.store_cell();

        self.code.push(OP_END); // block
    }

    /// Computes `$ptr + offset`, bounds-checks it the same way `move_ptr`
    /// checks a real pointer move, and leaves the checked address on the
    /// stack. `MulAdd`'s target cell is `$ptr + offset`, not `$ptr` itself
    /// — a store there needs the exact same underflow trap and
    /// grow-if-needed treatment a literal move to that address would get,
    /// or a target past the currently committed pages traps with a raw
    /// memory-access fault instead of growing first. Mirrors
    /// `bfnative::codegen::{x86_64,aarch64}::emit_bounds_checked_offset`,
    /// which every native backend already runs before a `MulAdd`/`Scan`
    /// store for exactly this reason.
    fn bounds_checked_target(&mut self, offset: i32) {
        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 0);
        self.const_i32(offset);
        self.code.push(OP_I32_ADD);
        self.code.push(OP_LOCAL_TEE);
        uleb(&mut self.code, 2); // $target (local 2, declared in Body::new)

        self.const_i32(0);
        self.code.push(OP_I32_LT_S);
        self.trap_if();

        if offset > 0 {
            self.emit_grow_check_for(2); // $target, not $ptr
        }

        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 2); // leave the checked address on the stack
    }

    /// `[->+<]`-style fold: add `factor * cell[$ptr]` into `cell[$ptr +
    /// offset]`. `offset` is a cell count, not a byte count, but since
    /// cells are one byte each here the two are the same number.
    ///
    /// Does NOT zero the source cell: the optimizer emits one `MulAdd` per
    /// spread target sharing the same source cell (see the `Op::MulAdd`
    /// four-in-a-row sequence a `[->>>+<<<]`-style hello-world setup loop
    /// folds to), followed by a single trailing `Op::Zero` once every
    /// target has read it. Zeroing here too would make every `MulAdd`
    /// after the first in such a group read back 0 instead of the real
    /// source value — exactly the bug this comment used to not warn about;
    /// see `bfnative::codegen::{x86_64,aarch64}::emit_mul_add`'s matching
    /// comment, which this backend didn't follow the first time around.
    fn emit_mul_add(&mut self, offset: i32, factor: u8) {
        self.bounds_checked_target(offset); // stack: [target_addr]

        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 2); // $target, same address again for the load
        self.code.push(OP_I32_LOAD8_U);
        self.mem_arg(0, 0);

        self.load_cell();
        self.const_i32(factor as i32);
        self.code.push(OP_I32_MUL);
        self.code.push(OP_I32_ADD);
        self.code.push(OP_I32_STORE8);
        self.mem_arg(0, 0);
    }

    /// `[>]`/`[<]`-style fold: step `$ptr` by `stride` cells until landing
    /// on a zero cell, expressed as a small wasm `loop` rather than
    /// unrolled, since the number of steps isn't known until run time —
    /// exactly the loop this instruction exists to replace, just with a
    /// single cheap load-and-branch body instead of the full bracket
    /// machinery `emit_loop` would generate. Each step reuses
    /// `move_ptr`'s underflow/grow checks so a scan can't walk past the
    /// tape bounds any move sequence would've been stopped at.
    fn emit_scan(&mut self, stride: i32) {
        self.code.push(OP_LOOP);
        self.code.push(BLOCKTYPE_EMPTY);

        self.code.push(OP_LOCAL_GET);
        uleb(&mut self.code, 0);
        self.const_i32(stride);
        self.code.push(OP_I32_ADD);
        self.code.push(OP_LOCAL_TEE);
        uleb(&mut self.code, 0);

        self.const_i32(0);
        self.code.push(OP_I32_LT_S);
        self.trap_if();
        if stride > 0 {
            self.emit_grow_check_for(0); // $ptr
        }

        self.load_cell();
        self.code.push(OP_BR_IF);
        uleb(&mut self.code, 0); // still nonzero: keep scanning

        self.code.push(OP_END); // loop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::uleb as encode_uleb;

    #[test]
    fn module_starts_with_magic_and_version() {
        let module = compile(&[]);
        assert_eq!(&module[0..4], b"\0asm");
        assert_eq!(&module[4..8], &[1, 0, 0, 0]);
    }

    #[test]
    fn empty_program_exports_start_and_memory() {
        // An empty (or fully-comment) .bf file still has to compile to a
        // module a WASI host can run: the export section is where the
        // host looks up `_start`, so the export names have to actually be
        // present in the bytes, not just "some section exists."
        let module = compile(&[]);
        let text = String::from_utf8_lossy(&module);
        assert!(text.contains("_start"));
        assert!(text.contains("memory"));
        assert!(text.contains("wasi_snapshot_preview1"));
        assert!(text.contains("fd_write"));
        assert!(text.contains("fd_read"));
    }

    #[test]
    fn match_loops_pairs_nested_brackets() {
        // [ + [ - ] ] -> indices 0,1,2,3,4
        let ops = vec![
            Op::JumpIfZero { target: 4 },
            Op::Add(1),
            Op::JumpIfZero { target: 3 },
            Op::JumpIfNonZero { target: 2 },
            Op::JumpIfNonZero { target: 0 },
        ];
        let spans = match_loops(&ops);
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().any(|s| s.open == 0 && s.close == 4));
        assert!(spans.iter().any(|s| s.open == 2 && s.close == 3));
    }

    #[test]
    #[should_panic(expected = "unbalanced brackets")]
    fn match_loops_panics_on_unbalanced_close() {
        match_loops(&[Op::JumpIfNonZero { target: 0 }]);
    }

    /// Walks the instruction stream decoding each opcode's actual operand
    /// width, tracking `block`/`loop`/`if` opens against `end`s the way a
    /// wasm validator does. This has to be a real decoder, not a byte scan:
    /// LEB128 immediates (`IO_SCRATCH_OFFSET`, function indices, memargs,
    /// ...) routinely contain byte values that collide with the raw
    /// `block`/`loop`/`if`/`end` opcodes — `65536` alone encodes as `80 80
    /// 04`, and that trailing `0x04` is indistinguishable from `if` to
    /// anything that isn't tracking operand boundaries. An earlier version
    /// of this checker scanned raw bytes and reported wildly wrong depths
    /// (19, for a two-op `,.` program) for exactly this reason. Only
    /// decodes opcodes this backend's codegen actually emits — not a
    /// general wasm decoder.
    fn opens_close(code: &[u8]) -> i32 {
        // Skip the locals-declaration prefix `emit_block`'s caller always
        // starts past: count-of-groups, then (count, valtype) per group.
        let mut i = 0;
        let group_count = read_uleb(code, &mut i);
        for _ in 0..group_count {
            read_uleb(code, &mut i); // local count
            i += 1; // valtype byte
        }

        let mut depth = 0;
        while i < code.len() {
            let op = code[i];
            i += 1;
            match op {
                OP_BLOCK | OP_LOOP => {
                    depth += 1;
                    i += 1; // blocktype byte
                }
                OPCODE_IF => {
                    depth += 1;
                    i += 1; // blocktype byte
                }
                OP_END => depth -= 1,
                OP_BR_IF | OP_LOCAL_GET | OP_LOCAL_TEE | OP_CALL => {
                    read_uleb(code, &mut i);
                }
                OP_I32_CONST => {
                    read_sleb(code, &mut i);
                }
                OP_I32_LOAD8_U | OP_I32_STORE8 | OP_I32_LOAD | OP_I32_STORE => {
                    read_uleb(code, &mut i); // align
                    read_uleb(code, &mut i); // offset
                }
                OP_MEMORY_SIZE | OP_MEMORY_GROW => {
                    i += 1; // memory index byte
                }
                OP_UNREACHABLE | OP_I32_EQZ | OP_I32_NE | OP_I32_LT_S | OP_I32_GT_S
                | OP_I32_ADD | OP_I32_SUB | OP_I32_MUL | OP_I32_DIV_U => {
                    // no operands
                }
                other => panic!(
                    "opens_close: unrecognized opcode {other:#04x} at byte {}; this decoder \
                     only knows the opcodes codegen.rs actually emits and needs a new arm",
                    i - 1
                ),
            }
        }
        depth
    }

    /// Advances `*i` past one ULEB128 value, returning it. Only used by
    /// [`opens_close`] to skip operands it doesn't otherwise care about.
    fn read_uleb(code: &[u8], i: &mut usize) -> u64 {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let byte = code[*i];
            *i += 1;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        result
    }

    /// Advances `*i` past one SLEB128 value (used for `i32.const`
    /// immediates, which can be negative). The return value is unused by
    /// [`opens_close`] — only the advancement matters — but returning it
    /// keeps this symmetric with [`read_uleb`].
    fn read_sleb(code: &[u8], i: &mut usize) -> i64 {
        let mut result = 0i64;
        let mut shift = 0;
        loop {
            let byte = code[*i];
            *i += 1;
            result |= ((byte & 0x7f) as i64) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                break;
            }
        }
        result
    }

    #[test]
    fn every_control_construct_is_balanced_for_simple_programs() {
        // Covers every emit_* path at least once: a loop (triggers
        // emit_loop), input/output (WASI call sequences and their traps),
        // and moves in both directions (move_ptr's underflow/grow checks).
        //
        // finish() always appends one trailing `end` for the function body
        // itself, which isn't a `block`/`loop`/`if` and has nothing in
        // this counter tracking its "open" — so a fully-balanced program
        // lands at -1, not 0, once that final byte is included.
        for source in ["+[-]", ",.", ">>><<<", "+++[->+++<]", "[>]", "[<]"] {
            let ops = bfc::optimize::optimize(bfc::parser::parse(source).unwrap());
            let loops = match_loops(&ops);
            let mut body = Body::new();
            body.emit_block(&ops, &loops, 0, ops.len());
            body.finish();
            assert_eq!(
                opens_close(&body.code),
                -1,
                "unbalanced block/loop/if vs end for {source:?}"
            );
        }
    }

    #[test]
    fn move_delta_clamps_instead_of_wrapping_negative() {
        assert_eq!(move_delta(5), 5);
        assert_eq!(move_delta(u32::MAX), i32::MAX);
    }

    #[test]
    fn uleb_and_sleb_agree_with_encode_module_on_a_shared_call_path() {
        // codegen.rs re-exposes encode::uleb/sleb rather than wrapping
        // them, so this just confirms the import actually resolves to the
        // same function encode.rs's own tests already cover in depth.
        let mut a = Vec::new();
        let mut b = Vec::new();
        uleb(&mut a, 300);
        encode_uleb(&mut b, 300);
        assert_eq!(a, b);
    }
}