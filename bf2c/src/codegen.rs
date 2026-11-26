//! Translates Brainfry's optimized IR into portable C99 — the same front
//! end as every other backend in this workspace (`bfc::parser` +
//! `bfc::optimize`), but the backend's job is deliberately different from
//! `bfnative`'s: instead of emitting machine code itself, it emits C and
//! hands the last mile to an industrial compiler.
//!
//! ## Why both a native backend and a C backend
//!
//! `bfnative` owns the whole pipeline so "no external toolchain" can be
//! true — every byte of the executable comes out of the compiler. The
//! trade is that it only knows two instruction sets (x86-64, ARM64) and
//! its code generator is a deliberately clean baseline: every op loads
//! and stores through the tape pointer, because that's the shape a small
//! hand-written emitter can verify by eye.
//!
//! `bf2c` is the opposite trade. GCC or Clang at `-O3` can spend decades
//! of compiler engineering on the generated code: keeping hot cells in
//! registers, unrolling loops, eliminating dead stores, auto-vectorizing
//! memory scans with SIMD (AVX2/NEON). And C runs everywhere those
//! compilers run — 32-bit microcontrollers, RISC-V, Emscripten to wasm,
//! exotic legacy platforms — none of which `bfnative` will ever target.
//! The output is standard C99 with no GNU extensions, so MSVC, `tcc`,
//! and cross toolchains all accept it too.
//!
//! ## Shape of the output
//!
//! The tape is a `calloc(30000, 1)` block of `uint8_t` with a moving
//! pointer (`uint8_t *p = tape;`) — the idiomatic C shape for Brainfuck.
//! One IR op becomes one statement:
//!
//! - `Add(5)` → `*p += 5;` — `uint8_t` arithmetic wraps mod 256 exactly
//!   like Brainfuck's 8-bit cells, no masking needed.
//! - `Move(3)` → `p += 3;` with the same left-of-zero and grow guards
//!   every other backend has.
//! - `MulAdd(2, 3)` → `p[2] += *p * 3;` (guarded).
//! - `Scan(step)` → `while (*p) { ... }` move loop (guarded).
//! - `[` / `]` → `while (*p) {` / `}`.
//!
//! Growth doubles the block with `realloc` and zeroes the newly
//! allocated tail (cells are born zero, and `realloc` doesn't guarantee
//! the extension is zeroed itself). The cap is the same 1 GiB as
//! `bfnative`, with the same message, and left-of-zero is the same
//! underflow error with the same exit status.
//!
//! ## Windows byte-exactness
//!
//! Windows C runtimes open the stdio streams in text mode: `\n` written
//! through `putchar`/`fprintf` reaches the pipe as CR LF, and reads fold
//! CR LF back to a single LF. Brainfuck I/O moves raw bytes, so `main`
//! starts with a `#ifdef _WIN32` shim that flips stdin, stdout, and
//! stderr to `_O_BINARY` — after which a Windows binary is byte-exact
//! with every other backend. Everywhere else the preprocessor deletes
//! the shim entirely, so non-Windows output is unchanged.
//!
//! ## Pointer discipline
//!
//! The generated C never computes a pointer outside the tape object:
//! right moves add first and compare against `tape + tape_len`, left
//! moves compare `p - tape` (always a valid difference) against the
//! count *before* subtracting, and `MulAdd`/`Scan` resolve their targets
//! as `ptrdiff_t` indices before touching memory. Strictly defined under
//! ISO C, not just "works on real compilers".

use bfformat::Op;

/// Cells at startup, matching `bfrun`'s `INITIAL_TAPE_SIZE` and every
/// other backend.
const TAPE_INITIAL: u32 = 30_000;

/// 1 GiB cap on tape growth, matching `bfnative::codegen::TAPE_RESERVE`
/// and `bfwasm`'s `MAX_PAGES`, message included.
const TAPE_MAX: u32 = 1 << 30;

/// A `[start, end)` span of op indices covering one loop: `open` is the
/// `JumpIfZero` head, `close` the matching `JumpIfNonZero` tail. Same
/// structure and same pairing argument as `bfjs`'s/`bfwasm`'s loop
/// matching: the optimizer only removes loops whole, so surviving
/// brackets are well-nested.
struct LoopSpan {
    open: usize,
    close: usize,
}

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

/// Compiles an optimized op list into a complete, standalone C99 source
/// file. `cc -O3 out.c` (or clang, MSVC, tcc, a cross-compiler) turns it
/// into a native binary with the full weight of a real compiler behind
/// it.
pub fn compile(ops: &[Op], source_name: &str) -> String {
    let loops = match_loops(ops);
    let mut out = String::with_capacity(4096);

    // `grow`/`die` are emitted only when an op can actually reach them,
    // so a plain `+.` program compiles warning-free even under
    // `-Wall -Wextra -Werror` — an unused static function is a warning
    // GCC really does raise.
    let uses_grow = ops.iter().any(needs_grow);
    // `grow` itself calls `die`, and MoveLeft/MulAdd/Scan carry their own
    // underflow guards that reference it.
    let uses_die = uses_grow || ops.iter().any(needs_die);

    out.push_str(&header(ops, source_name, uses_grow, uses_die));
    emit_block(&mut out, ops, &loops, 0, ops.len(), 1);
    out.push_str(FOOTER);

    out
}

/// Whether an op emits a reference to the `grow` helper.
fn needs_grow(op: &Op) -> bool {
    matches!(op, Op::MoveRight(_) | Op::MulAdd { .. } | Op::Scan { .. })
}

/// Whether an op emits a reference to the `die` helper directly.
fn needs_die(op: &Op) -> bool {
    matches!(op, Op::MoveLeft(_) | Op::MulAdd { .. } | Op::Scan { .. })
}

/// The fixed prologue: includes, tape globals, `die`, `grow`, and
/// `main`'s opening. Literal text pushed as raw fragments — braces in
/// C templates make escaped `format!` literals error-prone.
fn header(ops: &[Op], source_name: &str, uses_grow: bool, uses_die: bool) -> String {
    let mut h = String::with_capacity(2048);
    h.push_str(&format!(
        r#"/* Generated by bf2c — brainfry's C99 transpiler.
 * Source: {source_name} ({ops} ops after the shared optimizer).
 * Build:  cc -O3 -o program program.c   (clang / MSVC / tcc / cross
 * toolchains all work — the code is plain C99, no GNU extensions).
 */

"#,
        ops = ops.len(),
    ));
    h.push_str(
        r#"#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stddef.h>
#ifdef _WIN32
#include <fcntl.h>
#include <io.h>
#endif

"#,
    );
    h.push_str(&format!(
        "#define TAPE_INITIAL {TAPE_INITIAL}u\n#define TAPE_MAX {TAPE_MAX}u /* 1 GiB cap, same as bfnative */\n\n"
    ));
    h.push_str(
        r#"static uint8_t *tape;
static size_t tape_len;
static uint8_t *p;

"#,
    );
    if uses_die {
        h.push_str(
            r#"static void die(const char *msg)
{
    fprintf(stderr, "%s\n", msg);
    fflush(stdout); /* keep any buffered program output before exiting */
    exit(1);
}

"#,
        );
    }
    if uses_grow {
        h.push_str(
            r#"/* Ensure cell index `need` exists. Doubles the block with realloc and
 * zeroes the newly allocated tail (cells are born zero; realloc's
 * extension is not guaranteed to be). capped at TAPE_MAX. */
static void grow(size_t need)
{
    size_t want, off;
    uint8_t *nt;

    if (need >= TAPE_MAX) {
        die("runtime error: tape limit exceeded (1073741824 bytes)");
    }
    want = tape_len;
    while (want <= need) {
        want *= 2;
    }
    if (want > TAPE_MAX) {
        want = TAPE_MAX;
    }
    off = (size_t)(p - tape);
    nt = realloc(tape, want);
    if (nt == NULL) {
        die("runtime error: failed to grow tape");
    }
    memset(nt + tape_len, 0, want - tape_len);
    tape = nt;
    tape_len = want;
    p = nt + off;
}

"#,
        );
    }
    h.push_str(
        r#"int main(void)
{
    /* Windows opens stdio in text mode: '\n' written through putchar or
     * fprintf reaches the pipe as CR LF (0D 0A) and reads fold CR LF to
     * LF. Brainfry moves raw bytes, so flip all three streams to binary
     * mode and stay byte-exact with every other backend. Non-Windows
     * platforms have no text mode at all, so the preprocessor deletes
     * this whole block there. */
#ifdef _WIN32
    _setmode(_fileno(stdin), _O_BINARY);
    _setmode(_fileno(stdout), _O_BINARY);
    _setmode(_fileno(stderr), _O_BINARY);
#endif

    tape = calloc(TAPE_INITIAL, 1);
    if (tape == NULL) {
        fprintf(stderr, "runtime error: failed to grow tape\n");
        return 1;
    }
    tape_len = TAPE_INITIAL;
    p = tape;

"#,
    );
    h
}

const FOOTER: &str = r#"
    fflush(stdout);
    return 0;
}
"#;

/// Emits `ops[start..end)` at `depth` levels of indentation (4 spaces in
/// C style), recursing into loop bodies. Same walk as `bfjs`'s
/// `emit_block`.
fn emit_block(
    out: &mut String,
    ops: &[Op],
    loops: &[LoopSpan],
    start: usize,
    end: usize,
    depth: usize,
) {
    let pad = "    ".repeat(depth);
    let mut i = start;
    while i < end {
        match &ops[i] {
            Op::JumpIfZero { .. } => {
                let span = loops
                    .iter()
                    .find(|s| s.open == i)
                    .expect("JumpIfZero with no matching close in range");
                out.push_str(&format!("{pad}while (*p != 0) {{\n"));
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

/// One IR op → one statement. Every statement assumes `p` points inside
/// the tape, which the guards below maintain as an invariant.
fn emit_op(out: &mut String, op: &Op, pad: &str) {
    match op {
        Op::Add(n) => out.push_str(&format!("{pad}*p += {n};\n")),
        Op::Sub(n) => out.push_str(&format!("{pad}*p -= {n};\n")),
        Op::Zero => out.push_str(&format!("{pad}*p = 0;\n")),
        // Right moves add first, then grow if the index landed past the
        // end: `p += n` itself stays within one-past-the-end territory
        // for the folded counts real programs produce.
        Op::MoveRight(n) => out.push_str(&format!(
            "{pad}p += {n};\n\
             {pad}if ((size_t)(p - tape) >= tape_len) {{ grow((size_t)(p - tape)); }}\n"
        )),
        // Left moves compare the valid difference `p - tape` against the
        // count BEFORE subtracting — no pointer below the array is ever
        // computed, which keeps this strictly defined ISO C.
        Op::MoveLeft(n) => out.push_str(&format!(
            "{pad}if (p - tape < {n}) {{ die(\"runtime error: pointer moved left of cell 0\"); }}\n\
             {pad}p -= {n};\n"
        )),
        Op::Output => out.push_str(&format!("{pad}putchar(*p);\n")),
        Op::Input => out.push_str(&format!(
            "{pad}{{ int c = getchar(); if (c != EOF) {{ *p = (uint8_t)c; }} }}\n"
        )),
        // The source-cell-zero guard skips bounds check and write both,
        // matching the interpreter: the loop this replaced never ran on a
        // zero cell. The target resolves as a ptrdiff_t index first, so
        // negative offsets die at the underflow check instead of walking
        // a pointer below the array.
        Op::MulAdd { offset, factor } => {
            out.push_str(pad);
            out.push_str("if (*p != 0) {\n");
            out.push_str(&format!("{pad}    ptrdiff_t t = p - tape + {offset};\n"));
            out.push_str(pad);
            out.push_str(
                "    if (t < 0) { die(\"runtime error: pointer moved left of cell 0\"); }\n",
            );
            out.push_str(pad);
            out.push_str("    if ((size_t)t >= tape_len) { grow((size_t)t); }\n");
            out.push_str(&format!(
                "{pad}    p[{offset}] += (uint8_t)(*p * {factor});\n"
            ));
            out.push_str(&format!("{pad}}}\n"));
        }
        Op::Scan { stride } => {
            out.push_str(pad);
            out.push_str("while (*p != 0) {\n");
            out.push_str(&format!("{pad}    ptrdiff_t t = p - tape + {stride};\n"));
            out.push_str(pad);
            out.push_str(
                "    if (t < 0) { die(\"runtime error: pointer moved left of cell 0\"); }\n",
            );
            out.push_str(pad);
            out.push_str("    if ((size_t)t >= tape_len) { grow((size_t)t); }\n");
            out.push_str(&format!("{pad}    p += {stride};\n"));
            out.push_str(&format!("{pad}}}\n"));
        }
        Op::JumpIfZero { .. } | Op::JumpIfNonZero { .. } => {
            unreachable!("jump ops are consumed by emit_block's loop arm")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bfc::{optimize, parser};

    /// Deletes every `#ifdef _WIN32 ... #endif` block — a stand-in for
    /// what the preprocessor does on non-Windows targets.
    fn strip_win32_block(c: &str) -> String {
        let mut stripped = c.to_string();
        while let Some(start) = stripped.find("#ifdef _WIN32") {
            let end = stripped[start..]
                .find("#endif")
                .map(|i| start + i + "#endif".len())
                .expect("closing #endif");
            stripped.replace_range(start..end, "");
        }
        stripped
    }

    fn compiled(source: &str) -> String {
        let ops = optimize::optimize(parser::parse(source).unwrap());
        compile(&ops, "test.bf")
    }

    #[test]
    fn ir_ops_lower_to_the_documented_statements() {
        let c = compiled("+-><.");
        assert!(c.contains("*p += 1;"), "Add: {c}");
        assert!(c.contains("*p -= 1;"), "Sub: {c}");
        assert!(c.contains("p += 1;"), "MoveRight: {c}");
        assert!(c.contains("p -= 1;"), "MoveLeft: {c}");
        assert!(c.contains("putchar(*p);"), "Output: {c}");
    }

    #[test]
    fn zero_lowering_is_a_plain_store() {
        let c = compiled("+++[-]");
        assert!(c.contains("*p = 0;"));
        assert!(!c.contains("while (*p != 0) {\n    *p -= 1;"));
    }

    #[test]
    fn mul_add_uses_direct_pointer_arithmetic() {
        let c = compiled("+++[->++<]");
        assert!(c.contains("p[1] += (uint8_t)(*p * 2);"), "{c}");
        assert!(c.contains("ptrdiff_t t = p - tape + 1;"));
    }

    #[test]
    fn scan_is_a_guarded_move_loop() {
        let c = compiled("[>]");
        assert!(c.contains("while (*p != 0) {"), "{c}");
        assert!(c.contains("ptrdiff_t t = p - tape + 1;"));
        assert!(c.contains("p += 1;"));
    }

    #[test]
    fn loops_translate_to_while_blocks() {
        let c = compiled(",[.,]");
        assert!(c.contains("while (*p != 0) {\n        putchar(*p);"), "{c}");
        let opens = c.matches('{').count();
        let closes = c.matches('}').count();
        assert_eq!(opens, closes, "unbalanced braces in generated file");
    }

    #[test]
    fn nested_loops_keep_their_nesting() {
        let c = compiled(",[[.,].,]");
        assert!(c.contains("    while (*p != 0) {\n"));
        assert!(c.contains("        while (*p != 0) {\n"));
    }

    #[test]
    fn left_move_guards_before_the_subtract() {
        let c = compiled("<");
        assert!(
            c.contains(
                "if (p - tape < 1) { die(\"runtime error: pointer moved left of cell 0\"); }\n    p -= 1;"
            ),
            "the guard must precede the subtraction: {c}"
        );
    }

    #[test]
    fn error_wording_matches_the_other_backends_byte_for_byte() {
        // Needs both a left move (direct die reference) and a right move
        // (grow, which carries the cap and realloc-failure messages).
        let c = compiled("+<>");
        assert!(c.contains("\"runtime error: pointer moved left of cell 0\""));
        assert!(c.contains("\"runtime error: tape limit exceeded (1073741824 bytes)\""));
        assert!(c.contains("\"runtime error: failed to grow tape\""));
    }

    #[test]
    fn helpers_are_emitted_only_when_reachable() {
        // A pure arithmetic program references neither helper, so neither
        // is emitted — that keeps the file warning-free even under
        // `-Wall -Wextra -Werror`, where an unused static is a real
        // warning GCC raises.
        let bare = compiled("+.");
        assert!(!bare.contains("static void grow("), "grow leaked: {bare}");
        assert!(!bare.contains("static void die("), "die leaked: {bare}");

        // A right move reaches grow (and die through it).
        let right = compiled(">");
        assert!(right.contains("static void grow("));
        assert!(right.contains("static void die("));

        // A left move reaches die directly but never grow.
        let left = compiled("<");
        assert!(left.contains("static void die("));
        assert!(!left.contains("static void grow("));
    }

    #[test]
    fn memory_setup_matches_the_spec() {
        let c = compiled(">");
        assert!(c.contains("#define TAPE_INITIAL 30000u"));
        assert!(c.contains("#define TAPE_MAX 1073741824u"));
        assert!(c.contains("tape = calloc(TAPE_INITIAL, 1);"));
        assert!(c.contains("static uint8_t *p;"));
        // Growth is a doubling realloc with a zeroed tail.
        assert!(c.contains("nt = realloc(tape, want);"));
        assert!(c.contains("memset(nt + tape_len, 0, want - tape_len);"));
        assert!(c.contains("want *= 2;"));
    }

    #[test]
    fn the_file_is_comment_style_c89_safe() {
        let c = compiled("+");
        // No // line comments anywhere: MSVC's strictest modes and old
        // compilers choke on them, and /* */ costs nothing.
        assert!(!c.contains("//"), "line comments found: {c}");
        assert!(c.trim_end().ends_with("}"));
    }

    #[test]
    fn includes_are_the_minimal_set() {
        let c = compiled("+");
        for header_line in [
            "#include <stdio.h>\n",
            "#include <stdlib.h>\n",
            "#include <string.h>\n",
            "#include <stdint.h>\n",
            "#include <stddef.h>\n",
        ] {
            assert!(c.contains(header_line), "missing {header_line}");
        }
    }

    #[test]
    fn move_counts_fold_into_single_statements() {
        let c = compiled(">>>>>.");
        assert!(c.contains("p += 5;"));
        assert!(!c.contains("p += 1;\n"));
    }

    #[test]
    fn input_eof_leaves_the_cell_untouched() {
        let c = compiled(",");
        assert!(c.contains("{ int c = getchar(); if (c != EOF) { *p = (uint8_t)c; } }"));
    }

    #[test]
    #[should_panic(expected = "JumpIfNonZero outside its own loop's recursion")]
    fn a_lone_close_bracket_is_a_loud_invariant_failure() {
        // emit_block only ever sees a JumpIfNonZero nested inside the
        // recursion opened by its matching JumpIfZero; match_loops
        // guarantees that pairing. Feeding it a bare close must panic
        // loudly rather than silently emit unbalanced C.
        let mut out = String::new();
        let ops = [Op::JumpIfNonZero { target: 0 }];
        emit_block(&mut out, &ops, &[], 0, ops.len(), 0);
    }

    #[test]
    #[should_panic(expected = "jump ops are consumed by emit_block's loop arm")]
    fn emit_op_rejects_jump_ops_directly() {
        let mut out = String::new();
        emit_op(&mut out, &Op::JumpIfZero { target: 0 }, "");
    }

    #[test]
    fn windows_text_mode_is_disabled_for_byte_exact_io() {
        // Windows C runtimes translate '\n' to CR LF in text mode — that
        // is the whole class of Windows e2e failures (hello world ending
        // 13,10, error messages ending 13,10). The generated program must
        // flip all three stdio streams to binary mode under _WIN32.
        let c = compiled("+.");
        assert!(c.contains("#ifdef _WIN32"), "missing guard: {c}");
        assert!(c.contains("#include <fcntl.h>"), "missing fcntl.h: {c}");
        assert!(c.contains("#include <io.h>"), "missing io.h: {c}");
        assert!(
            c.contains("    _setmode(_fileno(stdin), _O_BINARY);"),
            "stdin not binary: {c}"
        );
        assert!(
            c.contains("    _setmode(_fileno(stdout), _O_BINARY);"),
            "stdout not binary: {c}"
        );
        assert!(
            c.contains("    _setmode(_fileno(stderr), _O_BINARY);"),
            "stderr not binary: {c}"
        );
        // The shim runs before any byte can reach a stream — including
        // the tape-allocation failure path — so it must open main, ahead
        // of the calloc that always follows.
        let main_body = c.split("int main(void)").nth(1).expect("main body");
        let shim_at = main_body
            .find("_setmode(_fileno(stdin), _O_BINARY);")
            .expect("shim in main");
        let calloc_at = main_body
            .find("tape = calloc")
            .expect("tape allocation in main");
        assert!(shim_at < calloc_at, "shim must precede all stdio: {c}");
    }

    #[test]
    fn the_windows_shim_is_c89_clean_and_balanced() {
        let c = compiled(".");
        // Block comments only (MSVC's strictest modes reject //), and the
        // preprocessor guards pair up so non-Windows compiles see nothing
        // of the shim at all.
        assert!(!c.contains("//"), "line comments found: {c}");
        let ifdefs = c.matches("#ifdef").count();
        let endifs = c.matches("#endif").count();
        assert_eq!(ifdefs, endifs, "unbalanced #ifdef/#endif");
        assert_eq!(
            ifdefs,
            c.matches("#ifdef _WIN32").count(),
            "only _WIN32 guards"
        );
        // On a non-Windows compile every shim line vanishes, so deleting
        // the guards must remove every Windows-only token.
        let stripped = strip_win32_block(&c);
        assert!(!stripped.contains("_setmode"), "shim leaked outside guard");
        assert!(
            !stripped.contains("fcntl.h"),
            "include leaked outside guard"
        );
        assert!(
            !stripped.contains("_O_BINARY"),
            "constant leaked outside guard"
        );
    }
}
