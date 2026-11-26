//! Translates Brainfry's optimized IR into a standalone JavaScript source
//! file — the same front end as every other backend in this workspace
//! (`bfc::parser` + `bfc::optimize`), but the target is text: a `.js` file
//! that runs natively anywhere JavaScript runs, with no runtime, no shim,
//! and no build step.
//!
//! ## Why a JavaScript backend at all
//!
//! `bfwasm` already covers "runs in a browser" — but only through a WASI
//! runtime or shim, because a `.wasm` module is a binary blob with
//! imports to satisfy before it does anything. This backend is the other
//! trade: the output is *source code* a browser console, `node`, `deno`,
//! or `bun` can execute directly, and a human can read. Where `bfwasm`
//! keeps everything in its own binary format, `bfjs` delegates the
//! machine-code step to the host's JIT: V8 (Chrome, Node), JavaScriptCore
//! (Safari), SpiderMonkey (Firefox) all profile hot `while` loops and
//! compile them to native code at runtime, which is what gives this
//! backend near-native speed without Brainfry emitting CPU instructions.
//!
//! ## The tape is a `Uint8Array`, and that's the whole trick
//!
//! Brainfuck cells are 8-bit with wraparound arithmetic. A JavaScript
//! `Uint8Array` has exactly those semantics for free: writing `255 + 1`
//! into element index `i` stores `0`, and `0 - 1` stores `255` — the
//! store truncates modulo 256 automatically. So `Add`/`Sub`/`MulAdd`
//! lower to plain `tape[ptr] += n` statements with zero masking
//! operations, and the generated code reads the way the semantics are
//! usually explained.
//!
//! ## Host detection instead of a platform layer
//!
//! The one thing JavaScript doesn't standardize is byte-level stdio: Node
//! has `process.stdout`, Deno has `Deno.stdout`, a browser console has
//! neither. The prologue feature-detects the host once and binds `emit` /
//! `readIn` accordingly — synchronous `fs.writeSync(1, …)` on Node and Bun
//! (byte-exact, unbuffered, immune to the pending-write truncation
//! `process.exit(1)` would cause on a pipe), `Deno.stdout.writeSync` on
//! Deno, and a collected buffer printed at exit on browsers, where
//! there's no stdin at all so `,` behaves as EOF. Error paths keep the
//! byte-exact wording every other backend pins: `runtime error: pointer
//! moved left of cell 0` and friends, with `process.exit(1)` / `Deno.exit`
//! on the VM hosts.
//!
//! ## IR mapping
//!
//! The optimized `Op` list is walked directly (loop nesting recovered by
//! bracket matching, same assumption `bfwasm` makes: the optimizer only
//! removes loops whole, so surviving loops are well-nested) and one IR
//! op becomes one readable statement:
//!
//! - `Add(n)` / `Sub(n)` → `tape[ptr] += n;` / `tape[ptr] -= n;`
//! - `Zero` → `tape[ptr] = 0;`
//! - `MulAdd(off, k)` → `tape[ptr + off] += tape[ptr] * k;` (guarded)
//! - `Scan(step)` → `while (tape[ptr] !== 0) ptr += step;` (guarded)
//! - `[` / `]` → `while (tape[ptr] !== 0) {` / `}`
//!
//! "Guarded" means the moves and offsets carry the same bounds checks
//! the other backends emit: left of cell 0 is the standard underflow
//! error, right past the end grows the `Uint8Array` (doubling, capped at
//! the same 1 GiB as `bfnative`).

use bfformat::Op;

/// Cells at startup, matching `bfrun`'s `INITIAL_TAPE_SIZE` and every
/// other backend's starting tape.
const TAPE_INITIAL: u32 = 30_000;

/// 1 GiB cap on tape growth, matching `bfnative::codegen::TAPE_RESERVE`
/// and `bfwasm`'s `MAX_PAGES`. A program that walks past it gets the same
/// message the native backends print, not a `RangeError` from the host.
const TAPE_MAX: u32 = 1 << 30;

/// A `[start, end)` span of op indices covering one loop: `open` is the
/// `JumpIfZero` head, `close` the matching `JumpIfNonZero` tail. Built by
/// [`match_loops`] before emission, so the recursive walk below never has
/// to look ahead for a loop's extent.
struct LoopSpan {
    open: usize,
    close: usize,
}

/// Pairs every surviving `JumpIfZero` with its matching
/// `JumpIfNonZero`. Same contract as `bfwasm`'s `match_loops`: the parser
/// emits balanced pairs and the optimizer only removes loops whole, so
/// the stack-based pairing is exact for any program that reaches this
/// far.
fn match_loops(ops: &[Op]) -> Vec<LoopSpan> {
    let mut open_stack = Vec::new();
    let mut spans = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        match op {
            Op::JumpIfZero { .. } => open_stack.push(i),
            Op::JumpIfNonZero { .. } => {
                let open = open_stack.pop().expect(
                    "JumpIfNonZero with no matching JumpIfZero — optimizer \
                     produced unbalanced brackets",
                );
                spans.push(LoopSpan { open, close: i });
            }
            _ => {}
        }
    }

    assert!(
        open_stack.is_empty(),
        "JumpIfZero with no matching JumpIfNonZero — optimizer produced \
         unbalanced brackets"
    );

    spans
}

/// Programs at least this many ops long switch to the chunked emission
/// shape (see [`compile`]): below it, the flat one-op-one-statement body
/// is what every normal program gets, and it is well inside every JS
/// engine's optimizer budget.
const CHUNK_THRESHOLD: usize = 1000;

/// Inside the chunked shape, a straight-line run this long gets its own
/// function too, so every emitted function stays small enough to
/// optimize no matter how pathological the program's shape is.
const STRAIGHT_CHUNK_OPS: usize = 150;

/// Compiles an optimized op list into a complete, standalone JavaScript
/// file. The output has no dependencies, no `import`s, and no build step:
/// `node out.js`, `deno run out.js`, `bun out.js`, or paste-into-console
/// all work as-is.
///
/// Two emission shapes, picked by size:
///
/// - **Flat** (default, every normal program): one op → one statement,
///  loops as plain nested `while` blocks, nothing else. Readable top to
///  bottom.
/// - **Chunked** (programs over [`CHUNK_THRESHOLD`] ops): the same
///  statements, but each loop body and each long straight-line run moves
///  into its own small function (`L0`, `S1`, …), leaving `while` skeletons
///  that call them. This exists because V8 declines to optimize a single
///  huge function: a 213 KB one-IIFE mandelbrot triggers a handful of
///  OSRs and no full TurboFan compile (measured: 18.9 s — slower than
///  the naive Rust interpreter), while the chunked shape fires ~400
///  optimization events across its call tree and runs the same program
///  in 4.0 s. The hot innermost loop bodies in real programs are short,
///  stay inline in their parent function, and get TurboFan'd like the
///  flat shape intends; only the cold bulk gets a function-call boundary.
///  Small programs never see any of this.
pub fn compile(ops: &[Op], source_name: &str) -> String {
    let loops = match_loops(ops);
    let mut out = String::with_capacity(4096);

    out.push_str(&header(ops, source_name));
    if ops.len() >= CHUNK_THRESHOLD {
        let mut chunker = Chunker {
            ops,
            loops: &loops,
            funcs: Vec::new(),
            next_id: 0,
        };
        let mut body = String::new();
        chunker.emit_seq(&mut body, 0, ops.len(), 1);
        for func in &chunker.funcs {
            out.push_str(func);
            out.push('\n');
        }
        out.push_str("// ---- program (chunked: each loop body / long run is a function) ---\n");
        out.push_str(&body);
    } else {
        emit_block(&mut out, ops, &loops, 0, ops.len(), 1);
    }
    out.push_str(&footer());

    out
}

/// Walks a chunked program: loop bodies and long straight runs become
/// functions collected in `funcs`, while the enclosing sequence emits
/// only the statements and `while (…) { Lk(); }` skeletons.
struct Chunker<'a> {
    ops: &'a [Op],
    loops: &'a [LoopSpan],
    funcs: Vec<String>,
    next_id: usize,
}

impl Chunker<'_> {
    /// Emits `ops[start..end)` — straight runs plus loop skeletons — at
    /// the given indentation depth.
    fn emit_seq(&mut self, out: &mut String, start: usize, end: usize, depth: usize) {
        let pad = "  ".repeat(depth);
        let mut i = start;
        while i < end {
            // The straight run up to the next loop head (or `end`).
            let run_start = i;
            while i < end
                && !matches!(self.ops[i], Op::JumpIfZero { .. } | Op::JumpIfNonZero { .. })
            {
                i += 1;
            }
            if i - run_start >= STRAIGHT_CHUNK_OPS {
                let id = self.next_id;
                self.next_id += 1;
                let mut body = String::new();
                for op in &self.ops[run_start..i] {
                    emit_op(&mut body, op, "  ");
                }
                self.funcs.push(format!("function S{id}() {{\n{body}}}\n"));
                out.push_str(&format!("{pad}S{id}();\n"));
            } else {
                let inline_pad = pad.clone();
                for op in &self.ops[run_start..i] {
                    emit_op(out, op, &inline_pad);
                }
            }
            if i < end {
                // A loop head: its body becomes a function; the while
                // skeleton stays here and calls it.
                let span = self
                    .loops
                    .iter()
                    .find(|s| s.open == i)
                    .expect("JumpIfZero with no matching close in range");
                let id = self.next_id;
                self.next_id += 1;
                let mut body = String::new();
                self.emit_seq(&mut body, i + 1, span.close, 1);
                self.funcs.push(format!("function L{id}() {{\n{body}}}\n"));
                out.push_str(&format!("{pad}while (tape[ptr] !== 0) {{ L{id}(); }}\n"));
                i = span.close + 1;
            }
        }
    }
}

/// The fixed prologue: tape declaration, host detection, I/O helpers,
/// growth, and the error paths. Kept as one template string so the
/// runtime shape of the output is stable and reviewable in one place.
fn header(ops: &[Op], source_name: &str) -> String {
    format!(
        r#"#!/usr/bin/env node
// Generated by bfjs — brainfry's JavaScript transpiler.
// Source: {source_name} ({ops} ops after the shared optimizer).
// Runs anywhere JavaScript runs: node out.js, bun out.js, deno run out.js,
// or paste into a browser console. No dependencies, no build step.
(function () {{
"use strict";

// ---- tape ------------------------------------------------------------
// Cells are bytes: a Uint8Array wraps mod 256 on add and subtract for
// free (255 + 1 === 0, 0 - 1 === 255), which is Brainfuck's 8-bit cell
// semantics with zero masking operations.
const TAPE_INITIAL = {TAPE_INITIAL};
const TAPE_MAX = {TAPE_MAX};           // 1 GiB cap, same as bfnative
let tape = new Uint8Array(TAPE_INITIAL);
let ptr = 0;

// ---- host detection ----------------------------------------------------
// Node and Bun expose process.stdout.write; Deno exposes
// Deno.stdout.writeSync; a browser console has neither, so output bytes
// are collected and printed when the program finishes.
const isNode = typeof process === "object" && process !== null &&
               typeof process.stdout === "object" && process.stdout !== null &&
               typeof process.stdout.write === "function";
const isDeno = typeof Deno === "object" && Deno !== null &&
               typeof Deno.stdout === "object" && Deno.stdout !== null &&
               typeof Deno.stdout.writeSync === "function";
let fs = null;                          // lazy: only Node/Bun reach it
const outBuf = [];                      // browser-only sink
const scratch = new Uint8Array(1);      // shared one-byte I/O buffer

function die(msg) {{
  // Byte-identical to the other backends: "runtime error: ..." + exit 1.
  if (isNode) {{
    process.stderr.write(msg + "\n");
    process.exit(1);
  }} else if (isDeno) {{
    Deno.stderr.writeSync(new TextEncoder().encode(msg + "\n"));
    Deno.exit(1);
  }} else {{
    console.error(msg);                 // browser: no process to exit
    throw new Error("bfjs: " + msg);   // ...but still stop execution
  }}
}}

// Ensure cell index `need` exists. Doubling growth over the old tape,
// zero-filled by construction, capped at TAPE_MAX like bfnative's
// reserve-and-commit grow routine.
function grow(need) {{
  if (need >= TAPE_MAX) {{
    die("runtime error: tape limit exceeded (1073741824 bytes)");
  }}
  let want = tape.length;
  while (want <= need) {{ want *= 2; }}
  if (want > TAPE_MAX) {{ want = TAPE_MAX; }}
  let next;
  try {{
    next = new Uint8Array(want);
  }} catch (e) {{
    die("runtime error: failed to grow tape");
  }}
  next.set(tape);
  tape = next;
}}

// Write one byte. fs.writeSync on fd 1 is synchronous and byte-exact —
// process.stdout.write would be asynchronous on a pipe and could be
// truncated by the process.exit(1) inside die().
function emit(b) {{
  if (isNode) {{
    if (fs === null) {{ fs = require("fs"); }}
    scratch[0] = b;
    fs.writeSync(1, scratch);
    return;
  }}
  if (isDeno) {{
    scratch[0] = b;
    Deno.stdout.writeSync(scratch);
    return;
  }}
  outBuf.push(b);
}}

// Read one byte, or -1 on EOF / read failure. A failed read leaves the
// cell untouched at the call site, matching bfrun's EOF convention.
function readIn() {{
  if (isNode) {{
    if (fs === null) {{ fs = require("fs"); }}
    try {{
      const n = fs.readSync(0, scratch, 0, 1);
      return n === 1 ? scratch[0] : -1;
    }} catch (e) {{
      return -1;
    }}
  }}
  if (isDeno) {{
    const buf = new Uint8Array(1);
    const n = Deno.stdin.readSync(buf);
    return n === 1 ? buf[0] : -1;      // null or 0 both mean EOF
  }}
  return -1;                            // browser: no stdin, always EOF
}}

// ---- program -----------------------------------------------------------
"#,
        ops = ops.len(),
        TAPE_INITIAL = TAPE_INITIAL,
        TAPE_MAX = TAPE_MAX,
    )
}

fn footer() -> &'static str {
    r#"
// ---- end of program ----------------------------------------------------
if (outBuf.length > 0) {
  // Browser host: no stdout exists, so print the collected output when
  // the program finishes. Console output can't be byte-exact by nature;
  // on Node/Deno/Bun nothing is buffered and this never runs.
  let s = "";
  for (let i = 0; i < outBuf.length; i++) { s += String.fromCharCode(outBuf[i]); }
  console.log(s);
}
})();
"#
}

/// Emits `ops[start..end)` at `depth` levels of indentation, recursing
/// into each loop body between its head and tail. The structure is the
/// same walk `bfwasm`'s `emit_block` does — bracket-matched nesting
/// recovered once up front — but the target here is `while` loops over
/// the tape, not structured wasm branches.
fn emit_block(out: &mut String, ops: &[Op], loops: &[LoopSpan], start: usize, end: usize, depth: usize) {
    let pad = "  ".repeat(depth);
    let mut i = start;
    while i < end {
        match &ops[i] {
            Op::JumpIfZero { .. } => {
                let span = loops
                    .iter()
                    .find(|s| s.open == i)
                    .expect("JumpIfZero with no matching close in range");
                out.push_str(&format!("{pad}while (tape[ptr] !== 0) {{\n"));
                emit_block(out, ops, loops, i + 1, span.close, depth + 1);
                out.push_str(&format!("{pad}}}\n"));
                i = span.close + 1;
            }
            Op::JumpIfNonZero { .. } => panic!(
                "JumpIfNonZero outside its own loop's recursion — \
                 match_loops pairing is inconsistent"
            ),
            op => {
                emit_op(out, op, &pad);
                i += 1;
            }
        }
    }
}

/// One IR op → one readable statement. Every statement assumes `ptr` is
/// in bounds, which the move and offset guards below maintain as an
/// invariant.
fn emit_op(out: &mut String, op: &Op, pad: &str) {
    match op {
        Op::Add(n) => out.push_str(&format!("{pad}tape[ptr] += {n};\n")),
        Op::Sub(n) => out.push_str(&format!("{pad}tape[ptr] -= {n};\n")),
        Op::Zero => out.push_str(&format!("{pad}tape[ptr] = 0;\n")),
        Op::MoveRight(n) => out.push_str(&format!(
            "{pad}ptr += {n};\n\
             {pad}if (ptr >= tape.length) {{ grow(ptr); }}\n"
        )),
        Op::MoveLeft(n) => out.push_str(&format!(
            "{pad}ptr -= {n};\n\
             {pad}if (ptr < 0) {{ die(\"runtime error: pointer moved left of cell 0\"); }}\n"
        )),
        Op::Output => out.push_str(&format!("{pad}emit(tape[ptr]);\n")),
        Op::Input => out.push_str(&format!(
            "{pad}{{ const c = readIn(); if (c >= 0) {{ tape[ptr] = c; }} }}\n"
        )),
        // Source-cell zero is a full skip — no bounds check, no write —
        // exactly like the interpreter's `if value != 0` guard, since the
        // loop this op replaces never ran on a zero cell either. Built
        // with push_str fragments rather than one format! literal: the
        // template's brace density makes escaped format strings error-prone.
        Op::MulAdd { offset, factor } => {
            out.push_str(pad);
            out.push_str("{ const v = tape[ptr]; if (v !== 0) {\n");
            out.push_str(&format!("{pad}  const t = ptr + {offset};\n"));
            out.push_str(pad);
            out.push_str("  if (t < 0) { die(\"runtime error: pointer moved left of cell 0\"); }\n");
            out.push_str(pad);
            out.push_str("  if (t >= tape.length) { grow(t); }\n");
            out.push_str(&format!("{pad}  tape[t] += v * {factor};\n"));
            out.push_str(pad);
            out.push_str("} }\n");
        }
        Op::Scan { stride } => {
            out.push_str(pad);
            out.push_str("while (tape[ptr] !== 0) {\n");
            out.push_str(&format!("{pad}  ptr += {stride};\n"));
            out.push_str(pad);
            out.push_str("  if (ptr < 0) { die(\"runtime error: pointer moved left of cell 0\"); }\n");
            out.push_str(pad);
            out.push_str("  if (ptr >= tape.length) { grow(ptr); }\n");
            out.push_str(pad);
            out.push_str("}\n");
        }
        Op::JumpIfZero { .. } | Op::JumpIfNonZero { .. } => unreachable!(
            "jump ops are consumed by emit_block's loop arm"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bfc::{optimize, parser};

    fn compiled(source: &str) -> String {
        let ops = optimize::optimize(parser::parse(source).unwrap());
        compile(&ops, "test.bf")
    }

    #[test]
    fn ir_ops_lower_to_the_documented_statements() {
        let js = compiled("+-><.");
        assert!(js.contains("tape[ptr] += 1;"), "Add: {js}");
        assert!(js.contains("tape[ptr] -= 1;"), "Sub: {js}");
        assert!(js.contains("ptr += 1;"), "MoveRight: {js}");
        assert!(js.contains("ptr -= 1;"), "MoveLeft: {js}");
        assert!(js.contains("emit(tape[ptr]);"), "Output: {js}");
        // And nothing leaked from other shapes: no Zero, no MulAdd.
        assert!(!js.contains("tape[ptr] = 0;"));
        assert!(!js.contains("const t = ptr +"));
    }

    #[test]
    fn zero_lowering_uses_the_documented_form() {
        let js = compiled("+++[-]");
        assert!(js.contains("tape[ptr] = 0;"));
        // The raw decrement loop must be gone.
        assert!(!js.contains("while (tape[ptr] !== 0) {\n  tape[ptr] -= 1;"));
    }

    #[test]
    fn mul_add_lowering_uses_offset_and_factor() {
        let js = compiled("+++[->++<]");
        assert!(js.contains("const t = ptr + 1;"));
        assert!(js.contains("tape[t] += v * 2;"));
    }

    #[test]
    fn scan_lowering_is_a_guarded_while() {
        let js = compiled("[>]");
        assert!(js.contains("while (tape[ptr] !== 0) {\n    ptr += 1;"));
        assert!(js.contains("if (ptr >= tape.length) { grow(ptr); }"));
    }

    #[test]
    fn loops_translate_to_while_blocks() {
        // I/O inside a loop is never folded, so `,[.,]` keeps a real loop
        // all the way to this backend — its body starts with Output.
        let js = compiled(",[.,]");
        assert!(js.contains("while (tape[ptr] !== 0) {\n    emit(tape[ptr]);"));
        // Balanced braces across the whole file (template braces included).
        let opens = js.matches('{').count();
        let closes = js.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces in generated file");
    }

    #[test]
    fn nested_loops_keep_their_nesting() {
        let js = compiled(",[[.,].,]");
        assert!(js.contains("  while (tape[ptr] !== 0) {\n"));
        assert!(js.contains("    while (tape[ptr] !== 0) {\n"));
    }

    #[test]
    fn error_wording_matches_the_other_backends_byte_for_byte() {
        let js = compiled("+<");
        assert!(js.contains("\"runtime error: pointer moved left of cell 0\""));
        let grow = compiled(">");
        assert!(
            grow.contains("\"runtime error: tape limit exceeded (1073741824 bytes)\""),
            "grow helper must carry the 1 GiB cap message"
        );
        assert!(grow.contains("\"runtime error: failed to grow tape\""));
    }

    #[test]
    fn tape_constants_match_the_shared_conventions() {
        let js = compiled("+");
        assert!(js.contains("const TAPE_INITIAL = 30000;"));
        assert!(js.contains("const TAPE_MAX = 1073741824;"));
        assert!(js.contains("let tape = new Uint8Array(TAPE_INITIAL);"));
    }

    #[test]
    fn output_is_wrapped_and_strict() {
        let js = compiled("+");
        assert!(js.starts_with("#!/usr/bin/env node\n"));
        assert!(js.contains("(function () {\n\"use strict\";\n"));
        assert!(js.trim_end().ends_with("})();"));
    }

    #[test]
    fn move_counts_fold_into_single_statements() {
        // 5 `>` chars fold to one MoveRight(5) → one ptr += 5.
        let js = compiled(">>>>>.");
        assert!(js.contains("ptr += 5;"));
        assert!(!js.contains("ptr += 1;\n"));
    }

    #[test]
    fn huge_left_move_does_not_emit_a_negative_literal_twice() {
        // 100000 `<` in the differential corpus folds to one MoveLeft —
        // the generated statement must carry the count as one literal.
        let js = compiled(&"<".repeat(1000));
        assert!(js.contains("ptr -= 1000;"));
    }

    #[test]
    fn small_programs_stay_flat() {
        // Below the chunk threshold: no L/S functions anywhere, the
        // one-op-one-statement shape is the whole file.
        let js = compiled("++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]");
        assert!(!js.contains("function L"), "chunked a small program: {js}");
        assert!(!js.contains("function S"), "chunked a small program: {js}");
        assert!(js.contains("while (tape[ptr] !== 0) {\n"));
    }

    #[test]
    fn big_programs_chunk_loop_bodies_into_functions() {
        // 1198 ops (alternating `>+` never folds) plus a loop: past the
        // threshold, the loop body becomes a function and the while stays
        // as the calling skeleton.
        let source = format!("{}{}", ">+".repeat(599), "[.]");
        let js = compiled(&source);
        // Ids are shared between S and L in emission order: the straight
        // run first takes S0, so the loop body takes L1.
        assert!(js.contains("function L1() {"), "no L function: {js}");
        assert!(js.contains("while (tape[ptr] !== 0) { L1(); }"), "{js}");
        // The straight run before the loop is long enough to become an S
        // function too, and the program section says it's chunked.
        assert!(js.contains("function S0() {"), "no S function: {js}");
        assert!(js.contains("S0();"), "{js}");
        assert!(js.contains("(chunked"), "{js}");
        // Braces still balance across the whole file.
        let opens = js.matches('{').count();
        let closes = js.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces in generated file");
    }

    #[test]
    fn big_flat_programs_chunk_the_straight_runs() {
        // 1200 ops with no loops at all (alternating `>+` never folds): the
        // one long run still must not leave a giant single function behind.
        let js = compiled(&">+".repeat(600));
        assert!(js.contains("function S0() {"), "no S function: {js}");
        assert!(js.contains("S0();"), "{js}");
        assert!(!js.contains("function L"), "no loops, no L functions: {js}");
        let opens = js.matches('{').count();
        let closes = js.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces in generated file");
    }
}
