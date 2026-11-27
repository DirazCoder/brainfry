# brainfry

Brainfuck has no standard bytecode, no packaging format — nothing beyond raw
`.bf` text that every existing implementation just interprets directly. This
project treats it like a real language instead: a compiler, a bytecode
format, a bytecode runtime, a backend that emits actual x86-64/ARM64
machine code and links it straight into a native executable, and a backend
that emits a standalone WebAssembly module, for anyone who doesn't want an
interpreter in the loop at all. Written in Rust — no manual memory
management to get wrong while hand-rolling machine code, executable
containers, and wasm modules.

Seven ways to run a `.bf` file, in increasing order of "how far do you want
to get from an interpreter":

1. **`bfinterp`** — walk the source directly, no bytecode, no compile step,
   no optimizer by default. The simplest and slowest path, and the one the
   other six are checked against.
2. **`bfc` + `bfrun`** — compile to `.bfry` bytecode, run it on a small VM.
   The javac/java split. Fast to build, portable, no toolchain needed.
3. **`bfjit`** — same codegen as `bfnative`, but instead of writing an
   executable file it maps executable memory at runtime and jumps straight
   into the generated code. No file touches disk; only runs on the host
   you're already on.
4. **`bfnative`** — skip the bytecode entirely and compile straight to a
   native binary for Linux, macOS, or Windows, x86-64 or ARM64. No
   interpreter at runtime, no toolchain at build time either. The machine
   code and the ELF/Mach-O/PE container around it are emitted byte-by-byte
   by `bfnative` itself, in-process, with zero external commands and zero
   object-file crates.
5. **`bfwasm`** — compile straight to a standalone `.wasm` module: WASI
   imports for I/O, linear memory for the tape, structured control flow
   for loops. Runs on any WASI-preview1 runtime (`wasmtime`, `wasmer`,
   Node's built-in `node:wasi` API, or a browser with a WASI shim) instead
   of a specific OS/CPU pair — the one backend whose output isn't tied
   to the machine it was compiled on.
6. **`bfjs`** — compile to a standalone `.js` file: a `Uint8Array` tape,
   guarded moves with doubling growth, and a host-detecting I/O layer. The
   output runs natively anywhere JavaScript does — Node, Bun, Deno, a
   browser console — with no WASI shim, no runtime, and no build step,
   because the host's own JIT (V8 and friends) is the machine-code stage.
7. **`bf2c`** — compile to portable C99, then hand the last mile to a real
   C compiler (`cc -O3`, clang, MSVC, a cross-toolchain). The opposite
   trade from `bfnative`: instead of Brainfry emitting machine code itself
   for two architectures, GCC/Clang get to spend decades of optimizer
   engineering on the program — and the output runs on any hardware with
   a C compiler, 32-bit micros and RISC-V included.

And one tool for when the question isn't "how fast" but "why did my cell
end up as 100": **`bfdbg`**, the interactive, time-traveling debugger
(breakpoints, watchpoints, live tape editing, `back`).

Checking whether a bug is in your program or in one of the other six?
`bfinterp` is the ground truth. Iterating on a program, `bfc`/`bfrun` is
zero-friction. Want near-native speed without managing a build artifact —
`bfjit`. Shipping something you want to hand someone as a standalone
`.exe`, that's `bfnative`. Shipping something that runs anywhere a wasm
runtime does, independent of OS or CPU — that's `bfwasm`. Want it to run
in a browser console or a serverless function with zero tooling — `bfjs`.
Targeting hardware `bfnative` will never support — `bf2c`. Trying to figure
out *why* a program misbehaves — `bfdbg`.

## Layout

- `bfformat/` — shared library defining the bytecode instruction set and the
  `.bfry` file format. Both `bfc` and `bfrun` depend on it so the two can't
  drift out of sync with each other.
- `bfc/` — the compiler. Reads `.bf` source, validates it, optimizes it,
  writes a `.bfry` file.
- `bfrun/` — the runtime. Loads a `.bfry` file and executes it. If you're
  just running someone else's compiled program, this is the only binary you
  need.
- `bfnative/` — a second, independent backend. Reuses `bfc`'s parser and
  optimizer, then instead of writing bytecode, emits x86-64 or ARM64
  machine code straight from the optimized IR and wraps it in a hand-built
  ELF64, Mach-O, or PE32+ executable. Nothing is assembled or linked by an
  external toolchain — every byte of the output comes out of the compiler.
- `bfinterp/` — the raw source interpreter. Reads a `.bf` file and executes
  it one op at a time, no bytecode, no optimizer by default, no machine
  code. Deliberately the simplest and slowest correct execution path, so it
  can serve as the ground-truth reference the other six are
  differential-tested against. `--optimize` opts into the shared optimizer
  for comparison runs; the default is the naive walk.
- `bfjit/` — the JIT. Same parser, same optimizer, and the *same source
  files* as `bfnative`'s machine-code layer (compiled in via `#[path]`, so
  the two can't drift), but instead of wrapping the code in an executable
  file, it maps anonymous memory, copies the code in, makes it executable
  (W^X), and jumps straight into it. No file ever hits the disk, no
  ELF/Mach-O/PE writer involved, and only the host platform can be
  JIT-compiled.
- `bfwasm/` — a third independent backend, targeting WebAssembly instead of
  a CPU. Reuses `bfc`'s parser and optimizer like every other backend, then
  emits a binary `.wasm` module directly: a hand-rolled LEB128/section
  encoder (`encode.rs`), WASI `fd_write`/`fd_read` imports for I/O, one
  page-granular linear memory for the tape, and Brainfuck's `[...]` loops
  translated to structured `block`/`loop`/`br_if` rather than the
  fixup-based jumps `bfnative`'s emitter uses — wasm has no such thing as a
  branch to an absolute address, so the whole fixup mechanism `bfnative`
  needs doesn't apply here. No `wasm-bindgen`, no `wat2wasm`, no
  Emscripten; every byte comes out of `bfwasm` itself, same discipline as
  `bfnative`.
- `bfjs/` — the JavaScript transpiler. Same front end as every other
  backend; the output is a standalone `.js` file whose tape is a
  `Uint8Array` — 8-bit wraparound semantics for free, zero masking — with
  a doubling grow, the same 1 GiB cap and left-of-zero error as the other
  backends, and an I/O layer that feature-detects the host once (Node/Bun
  via `fs.writeSync`, Deno via `Deno.stdout.writeSync`, browsers via a
  collected buffer). The host's JIT is the machine-code stage, which is
  what makes the output near-native without Brainfry emitting a single
  CPU instruction.
- `bf2c/` — the C99 transpiler. Emits portable C with a moving
  `uint8_t *p` over a `calloc`'d tape: `Add(5)` becomes `*p += 5;`,
  `Move(3)` becomes `p += 3;`, `MulAdd(2, 3)` becomes `p[2] += *p * 3;`,
  all guarded the same way every backend guards them. No GNU extensions,
  strictly-defined pointer arithmetic (left moves compare `p - tape`
  before subtracting), and helpers emitted only when reachable so the
  file compiles warning-clean even under `-Wall -Wextra -Werror`. The C
  compiler — not Brainfry — is where register allocation, unrolling, and
  auto-vectorization happen.
- `bfdbg/` — the interactive debugger. Its own single-stepping engine
  over the *raw parsed* op list — one op per source character, so every
  step maps to an exact line/column — with breakpoints by op index or
  source position, watchpoints on the pointer or on a cell's value, a
  live tape view (dec/hex/ASCII) with live editing, and time travel: an
  undo log of `(op_index, old_ptr, old_cell_value)` records that `back`
  replays in reverse, so a runtime error can be rewound, patched with
  `set`, and retried without restarting the program.

`bfinterp`, `bfjit`, `bfwasm`, `bfjs`, `bf2c`, and `bfdbg` are independent
crates with independent runtimes/emitters. They share the front end
(`bfc`/`bfformat`) exactly like `bfnative` does, and share no execution
code with `bfrun`/`bfnative` or each other. Delete any one of them and the
rest of the workspace builds and behaves exactly as before.

## Building

Requires a Rust toolchain (rustc + cargo). That's the whole list — no C
compiler, no linker, no platform SDK, for any target, including wasm.

```
cargo build --release
```

This builds all nine binaries — `bfc`, `bfrun`, `bfnative`, `bfinterp`,
`bfjit`, `bfwasm`, `bfjs`, `bf2c`, and `bfdbg` — in one shot. Every crate
is listed in `Cargo.toml`'s `[workspace] members`, so there's no
special-casing needed for any of them. The one external tool any of this
can involve is a C compiler or node — and only for *running* `bf2c`/`bfjs`
output, never for building the workspace.

## Usage: bfc / bfrun

```
bfc program.bf              # writes program.bfry
bfc program.bf out.bfry     # or pick the output name yourself
bfrun program.bfry          # runs it
```

## Usage: bfnative

```
bfnative program.bf                          # compiles for your current OS/CPU
bfnative --target windows-x86_64 program.bf  # or cross-compile for one of six targets
bfnative --emit-asm program.bf               # dump an annotated machine-code listing instead
```

`--emit-asm` no longer writes assembly text. There's no assembly and no
external assembler anymore — it writes a `.lst` listing of the final
machine code instead: hex bytes, resolved addresses, per-op annotations, so
you can still see exactly what got emitted. `--cc` and `--keep-asm` are
accepted but deprecated no-ops, so old scripts don't break.

Full target list: `linux-x86_64`, `linux-aarch64`, `macos-x86_64`,
`macos-aarch64`, `windows-x86_64`, `windows-aarch64`. 32-bit x86 and 32-bit
ARM aren't supported. No plans to add them either — that's not where
anyone's actually running this.

### How the executable gets built

No `cc`, no `ld`, no `as`, no object-file crate. The pipeline is three
stages, all in-process:

1. `bfc`'s parser and optimizer produce the shared IR — the same one
   `bfrun` executes, so there's one optimizer and no drift.
2. A machine-code emitter, one per CPU architecture, writes straight into a
   byte buffer: the IR plus a small runtime (entry prologue, tape-growth
   routine, I/O helpers, error paths). Branch offsets go in as placeholders
   and get patched in a second pass once sizes are known.
3. A container writer, one per file format, wraps those bytes into a
   loadable executable: ELF64 `ET_EXEC` for Linux, Mach-O `MH_EXECUTE` for
   macOS, PE32+ for Windows. Code references data position-relatively
   (rip-relative on x86-64, ADRP on ARM64), so the final address layout is
   patched in before serialization. What hits the disk is final — no
   relocations, no link step.

Cross-compiling a Windows binary from Linux runs the same code path as a
native build. That's the whole point of owning the output end to end.

### Why the OS split is bigger than the CPU split

The six targets are really two instruction backends — x86-64, ARM64 — with
three runtime layers bolted on top. Almost all the platform-specific
weirdness lives in those runtime layers, not the instruction encoding.

Linux binaries are fully static, freestanding ELF64 executables that hit
raw syscalls directly (`mmap`, `mprotect`, `read`, `write`, `exit`). Zero
runtime dependencies, nothing to link against.

macOS doesn't support that kind of thing. There's no such thing as a
static, no-libc Mach-O executable — every macOS binary talks to
libSystem. So the writer does the minimum dynamic linking dyld accepts:
one `LC_LOAD_DYLIB` for libSystem, a `LC_DYLD_INFO_ONLY` bind stream that
resolves exactly five symbols (`write`, `read`, `mmap`, `mprotect`,
`_exit`) into a five-slot GOT, and `LC_MAIN` for the entry. Apple Silicon
also refuses to run unsigned arm64 code, so the writer computes a full
ad-hoc code signature itself: a v0x20400 CodeDirectory with SHA-256 page
hashes (4 KiB pages on x86-64, 16 KiB on ARM64, matching what the kernel
verifies), built with an in-process SHA-256. No `codesign` subprocess.

Windows binaries are freestanding PE32+ executables that import six
functions from `kernel32.dll` (`GetStdHandle`, `WriteFile`, `ReadFile`,
`ExitProcess`, `VirtualAlloc`, `GetLastError`) through a real import address table the
loader resolves at startup, with a custom entry point. No CRT, no startup
objects. Win32 got picked over the C runtime for the usual reason:
kernel32's exports are the one stable surface on every Windows install,
while CRT `read`/`write` export names vary across toolchain flavors.

The ARM64 image is not the x86-64 one with a different machine field.
Windows on ARM doesn't let you turn ASLR off (the linker rejects
`/DYNAMICBASE:NO` for ARM64), so that image stays relocatable: no
`RELOCS_STRIPPED`, `DYNAMIC_BASE` on, one real base relocation, and a
`.pdata`/`.xdata` unwind record over `.text`. The unwind record is
probably not required (a minimal `lld-link` ARM64 exe has neither it nor
a `.reloc`), but it's valid and harmless, so it stayed. What did matter
was the header flags: both Windows arches set `LARGE_ADDRESS_AWARE`
(the image base is above 4 GiB), and ARM64 uses the same
DllCharacteristics `lld-link` writes (`0x8160`). With the flags missing,
Windows 11 ARM refused the exe at `CreateProcess` with
`ERROR_BAD_EXE_FORMAT` even though every offline parser accepted it. Both
flags were fixed in one commit, so which of them the loader objected to
isn't pinned down.

None of that touches instruction selection. A given CPU architecture emits
the same arithmetic and branch instructions no matter which of the three
OSes it's targeting — only the prologue, epilogue, and I/O calls change.

## Usage: bfinterp

```
bfinterp program.bf              # naive interpretation, stdin/stdout direct
bfinterp --optimize program.bf   # same interpreter, optimizer enabled first
```

No bytecode step, no `.bfry` file anywhere in the picture — parse, then
execute the op list one op at a time. The parser emits one op per
`+ - > < . , [ ]` character, so the default path is as close to walking
the raw source as an interpreter can get. It exists to be the maximally
simple reference implementation: the one whose output everyone else has
to match, not the one anyone should expect to be fast.

Semantics are `bfrun`'s exactly: 30,000 starting cells with grow-on-demand,
8-bit wrapping cells, EOF-on-input leaving the cell untouched, left-of-zero
as a clean `runtime error` with exit status 1. But the engine is a separate
implementation sharing no code with `bfrun`'s VM, so agreement between the
two is actual evidence about the semantics, not a tautology.

## Usage: bfjit

```
bfjit program.bf                 # compile in memory, execute immediately
```

The pipeline is `bfnative`'s up to the last step — shared parser, shared
optimizer, and the identical per-architecture machine-code emitter.
`bfjit` compiles `bfnative`'s `codegen` source directly into itself via
`#[path]`, so there's exactly one emitter in the repo and the JIT and the
AOT compiler can't produce different code. From there it diverges: instead
of handing the bytes to an ELF/Mach-O/PE writer, it `mmap`s one anonymous
region, resolves every branch/data fixup against that region's real
address, copies in the code plus the read-only data plus — on
macOS/Windows — a small table of host function pointers standing in for
the GOT/IAT a container would have provided, flips the region from
writable to executable, and jumps into the entry stub. The generated code
then handles everything itself: tape reservation, growth, I/O, process
exit. That's why a JIT'd program's observable behavior, error messages,
and exit codes match a `bfnative` executable's by construction, not by
coincidence.

Memory discipline is W^X on every platform. Linux maps RW and `mprotect`s
to RX. Windows `VirtualAlloc`s `PAGE_READWRITE` and `VirtualProtect`s to
`PAGE_EXECUTE_READ`. macOS uses the same plain `mmap`/`mprotect` pair on
both Intel and Apple Silicon. `MAP_JIT` is deliberately not used: it needs
the `allow-jit` entitlement honored by a Developer ID, Hardened
Runtime signature, and with the ad-hoc signing this project does the
`mmap` fails with `ENOMEM`. On ARM64 hosts, freshly written code also runs through the
proper cache-maintenance sequence (`sys_icache_invalidate` on macOS,
DC CVAU / DSB / IC IVAU / ISB on Linux) before the first jump, because the
ARM instruction cache isn't coherent with the data cache.

Only the host can be JIT-compiled. The backend is picked from the
detected host OS/arch pair, and `--target` gets rejected with a pointer to
`bfnative`, whose output is a file you can actually carry to another
machine.

## bfjit verification status

Same honesty standard as the native targets: what ran, versus what was
only built and inspected. (bfinterp has no platform-specific code at all —
the same Rust executes on every host — so it inherits whatever platform
the Rust compiler supports.)

| bfjit target | Status | How |
|---|---|---|
| `linux-x86_64` | **executed** | full workspace test suite (19 e2e tests executing JITed code, incl. mandelbrot.b byte-exact) plus the differential harness; CI on `ubuntu-latest` |
| `linux-aarch64` | **executed** | CI on a native ARM runner (`ubuntu-26.04-arm`). Also run during development under `qemu-aarch64-static` (aarch64 bfjit built for `aarch64-unknown-linux-musl`): same corpus, byte-identical to `bfrun`, mandelbrot included |
| `macos-x86_64` | **executed** | CI on `macos-26-intel`: plain `mmap`/`mprotect` path, ad-hoc signed |
| `macos-aarch64` | **executed** | CI on `macos-latest` (Apple Silicon): plain `mmap`/`mprotect` plus `sys_icache_invalidate` and a stack-alignment trampoline for the generated entry |
| `windows-x86_64` | **executed** | CI on `windows-latest`: `VirtualAlloc`/`VirtualProtect` + `FlushInstructionCache` |
| `windows-aarch64` | **executed** | CI on `windows-11-arm`: same Windows path on ARM64 |

## Usage: bfwasm

```
bfwasm program.bf                         # writes program.wasm (WASI by default)
bfwasm program.bf -o out.wasm             # standalone WASI module
wasmtime run program.wasm                 # any WASI preview1 runtime
bfwasm program.bf --target browser        # writes a self-contained program.js
bfwasm program.bf -o out.js               # .js extension infers browser target
bfwasm program.bf --target browser --split # writes program.js and program.wasm
bfwasm program.bf -o dist/app.js --split   # writes dist/app.js and dist/app.wasm
bfwasm program.bf --target wasi -o out.js # explicit target overrides inference
```

The default `wasi` target remains a standalone WASI Preview1 `.wasm` module.
The `browser` target writes one self-contained JavaScript file: it embeds the
same compiled module as base64 and supplies an inline `wasi_snapshot_preview1`
shim. It needs no separate `.wasm` file, network request, or external runtime,
so it can run from `file://`, a browser console, or a `<script type="module">`.
The optional `--split` mode instead writes a small ES module loader and an
adjacent `.wasm` asset. The loader fetches that asset relative to itself and
falls back to `arrayBuffer()` if streaming instantiation is unavailable or
fails; serve the pair over HTTP when browser origin rules prevent `file://`
fetches.

Both browser modes expose optional hooks through `globalThis.__bfwasm`, which
must be set before the loader runs:

```js
globalThis.__bfwasm = {
  stdin: "input bytes", // or a Uint8Array
  onStdout: chunk => console.log("stdout bytes:", chunk),
  onStderr: chunk => console.error("stderr bytes:", chunk),
};
```

Without hooks, input is EOF and buffered output is flushed to `console.log` /
`console.error` on exit. Output hooks receive live `Uint8Array` chunks during
WASI writes. Both modes expose the final status as
`globalThis.__bfwasmExitCode`.

Semantics are `bfrun`'s: 30,000 starting cells, 8-bit wrapping, EOF leaving
the cell untouched. Moving left of cell 0 prints `runtime error: pointer
moved left of cell 0` to stderr and exits with status 1 (through WASI's
`proc_exit`), same as every other backend.

Two failures can't do that. If `memory.grow` is refused (the tape is capped
at 1 GiB, same as `bfnative`) or a WASI read or write returns an error, the
module hits a wasm trap, which has no text to print. What a trap turns into
(exit status, any message) is up to the runtime, and the differential
harness doesn't exercise either path.

## Usage: bfjs

```
bfjs program.bf               # writes program.js
bfjs program.bf -o out.js     # or pick the output name yourself
node program.js               # or: bun program.js / deno run program.js
                               # or paste the whole file into a browser console
```

The output is a standalone JavaScript source file — no imports, no runtime
library, no build step. The tape is a `Uint8Array`, which has Brainfuck's
8-bit wraparound semantics built in (`255 + 1 === 0`), so the generated
statements are as plain as `tape[ptr] += 5;`. One I/O layer is
feature-detected at startup: Node and Bun write bytes synchronously through
`fs.writeSync(1, …)`, Deno through `Deno.stdout.writeSync`, and a browser
collects the output and prints it when the program ends (there's no stdin
in a console, so `,` behaves as EOF there). Program speed comes from the
host's JIT: V8/JavaScriptCore/SpiderMonkey profile the hot `while` loops
and compile them to native code at runtime, which is why this backend
needs no machine-code stage of its own. One honest wrinkle measured on
mandelbrot: V8 declines to optimize a single function past a size limit,
so programs over ~1000 ops are emitted in a *chunked* shape — each loop
body and long straight run becomes a small function called from its `while`
skeleton — which moved mandelbrot from 18.9 s (interpreted bytecode, slower
than `bfinterp`) to 4.0 s on the benchmark script's host. Small programs
always get the flat one-op-one-statement file.

Semantics are `bfrun`'s exactly — same starting tape, same wraparound,
same EOF convention, same left-of-zero error message and exit status on
every host that has a real stderr/stdout pair. Tape growth doubles the
`Uint8Array` up to the same 1 GiB cap as `bfnative`, with the same
messages for the cap and for a failed allocation.

## Usage: bf2c

```
bf2c program.bf               # writes program.c
bf2c program.bf -o out.c      # or pick the output name yourself
cc -O3 -o program program.c   # any C99 compiler: gcc, clang, MSVC, tcc...
./program
```

The output is portable C99 — no GNU extensions, no compiler assumptions,
declarations in places even strict C89 modes accept — so the same file
builds on 64-bit Linux, a 32-bit microcontroller, or cross-compiled for
RISC-V. The tape is `calloc(30000, 1)` with a moving `uint8_t *p`;
optimized IR maps one op to one statement (`*p += 5;`, `p += 3;`,
`p[2] += *p * 3;`, `while (*p) { … }`). Growth `realloc`s with a doubling
schedule and zeroes the new tail, the 1 GiB cap and left-of-zero checks
match every other backend's wording and exit status, and the helpers are
emitted only when an op can reach them, so a plain `+.` program compiles
warning-clean even under `-Wall -Wextra -Werror`.

What `bf2c` buys over `bfnative` is the C compiler's optimizer: register
allocation for hot cells, loop unrolling, dead-store elimination, and
SIMD auto-vectorization of memory scans — plus coverage of every
architecture with a C toolchain, not just x86-64 and ARM64.

## Usage: bfdbg

```
bfdbg program.bf                      # interactive session on your terminal
bfdbg --break @42 program.bf < cmds    # or drive it from a script
bfdbg --stdin input.txt program.bf     # preload the program's `,` input
```

A debugger needs to stop between ops, so it has its own single-stepping
engine (it can't reuse `bfinterp`'s run-to-completion loop) — with
`bfinterp`'s exact semantics. It debugs the **raw parsed** op list, one op
per source character, which is what makes every stop point at a real
line/column in the `.bf` file; the optimizer's rewritten ops (`Zero`,
`MulAdd`, `Scan`, and the fixed-offset `AddAt`, `ZeroAt`, `SetAt` and
`BoundsCheck`) correspond to no single character, so stepping them
against the source would be meaningless — which is why there's no
`--optimize` flag here.

The session commands: `run`/`continue` resume until a stop; `step [n]`
executes ops one at a time; `tape [center] [count]` shows cells around the
pointer in decimal, hex, and ASCII; `list` shows the source window with
the next instruction careted; `break 42` / `break @3:5` sets breakpoints
by op index or line/column; `watch ptr 42` stops when the pointer reaches
a cell, `watch cell 10` stops when a cell's value changes; `set cell[10]
= 65` and `set ptr = 3` edit the machine live (growing the tape as
needed); `feed 65 …` queues input bytes for the program's `,`. Program
output goes to stdout, debugger messages to stderr, commands come from
stdin — a piped session keeps the program's output stream clean.

Time travel: every executed op appends one `(op_index, old_ptr,
old_cell_value)` record before it runs — raw ops touch at most one cell
plus the pointer, so that triple inverts any of them exactly. `back [n]`
replays records in reverse, restoring tape, pointer, and program counter;
rewinding out of a whole loop is just several backs. A *failed* op records
too, so the signature workflow is: hit a runtime error, `back` it, `set`
whatever you needed, `run` again — without restarting the program. The
log is bounded (default 1 Mi steps ≈ 24 MB, `--undo-limit` to tune, `0`
disables), because a program like mandelbrot executes ~120M ops and an
unbounded log would exhaust memory long before the program finished.

## Benchmarks (mandelbrot.b)

Four execution strategies, one program, byte-identical output (6,240
bytes) from every one of them — the differential harness checks that
before anything gets timed.

| tool | min / median | vs bfinterp |
|---|---|---|
| `bfinterp` (naive reference) | 15.5 s / 15.6 s | 1.0x |
| `bfrun` (bytecode VM) | 8.1 s / 8.1 s | 1.9x |
| `bfjit` (in-memory JIT) | 1.27 s / 1.27 s | 12.2x |
| `bfnative` (AOT ELF) | 1.25 s / 1.27 s | 12.4x |

The two transpiler lanes join the same benchmark through the same harness
when their host tools are on PATH: `bfjs` under node/V8 lands around 3x
`bfinterp` — a JIT'd guarded-op loop is real speed, just not hand-emitted
machine code — and `bf2c` compiled with `cc -O2` around 5x, behind
`bfnative` because it pays for tape writes through a pointer like every
portable C program does. Both produce byte-identical output, and
`scripts/benchmark_all.py` reproduces the comparison.

Pretty much the shape you'd predict: the naive interpreter is slowest, the
bytecode VM's op-folding roughly doubles it, and the two machine-code
strategies tie, because they're the same generated code — `bfjit`'s only
overhead is the few milliseconds of parse/optimize/emit/patch it pays
inside the run. Reproduce with `scripts/benchmark_all.py`.

## What the compiler actually does

Parsing is a straight character-by-character pass that validates bracket
matching and builds an unoptimized op list. The optimizer runs on top of
that as seven passes in a fixed order, and the whole sequence repeats until
a full round changes nothing. One pass often sets up another: a loop folded
into a `Zero` can make a later store redundant, which can leave a following
loop guarded by a cell that's now known to be zero. A single sweep misses
that. Jump targets get recomputed twice per round, since most of the passes
change instruction indices.

Run folding turns `+++++` into one `Add(5)` instead of five separate
`Add(1)`s (same idea for `-`, `>`, `<`). That cuts how many times the
bytecode runtime's dispatch loop spins on repetitive source and produces
noticeably shorter assembly out of `bfnative` too.

Zero-loop folding takes `[-]`, `[+]` and any other single-step loop with an
odd step, and collapses it into one `Zero` instead of looping up to 255
times. It has to be odd: an odd step is a unit mod 256, so it reaches 0 from
any starting value. An even step from an odd cell never lands on 0 (`[++]`
with the cell holding 3 stays odd forever), and folding that would turn a
diverging loop into a terminating one. Even-step loops stay real loops.

Linear-loop folding handles `[->+<]` and its relatives, and it's more
general than the earlier version, which only accepted a counter that drops
by exactly 1. Now the loop can change its counter cell by any odd amount per
iteration, and each target cell can be added to or subtracted from. The
iteration count is still fixed by the starting value, because an odd step
has an inverse mod 256: for a counter step `s` and a target delta `d`, the
target ends up with `c * (-d / s)` added to it, with the division done in
mod-256 arithmetic. The loop becomes one `MulAdd` per target followed by a
`ZeroAt` on the counter. The loop has to return to its starting cell, and
the body can only contain adds, subtracts and moves. Any `Output`, `Input`
or nested loop would change how many times a side effect fires, so those
stay loops. Every cell the pointer lands on must also be written by the
body. A body like `[<>-]` visits cell -1 without writing it, which has no
effect on values but still raises the underflow error when the real loop
runs. A folded `MulAdd` can't perform that visit, so the error would
silently disappear.

Scan-loop folding handles `[>]` / `[<]` and the multi-cell-stride variants,
a loop whose whole body is one move, used to walk the tape to a zero cell.
That becomes a single `Scan` carrying the stride and direction, which drops
the per-cell jump-check-jump overhead without changing how many cells get
inspected.

Pointer windowing targets straight-line runs of `+`, `-`, `<`, `>` and
`Zero`. Instead of moving the pointer back and forth between edits, the
pass rewrites the run as `AddAt` / `ZeroAt` ops at fixed offsets from where
the run started, followed by one final move. A `BoundsCheck` op comes first
and carries the lowest and highest offsets the original moves reached. This
matters because those moves would have raised the left-of-cell-0 error or
grown the tape even if nothing was written at the extremes, and collapsing
them into one move would hide that. It's only emitted when the pointer
actually moved. Runs with no stores are left alone, and so are runs whose
displacement doesn't fit the op operands.

Store fusion merges adjacent stores to the same offset: `ZeroAt` then
`AddAt` becomes a `SetAt` (store a constant), two `AddAt`s sum their deltas,
and any store followed by `ZeroAt` or `SetAt` is overwritten by it. It only
looks at neighbors and never reorders across other ops.

Dead-code elimination is a forward known-value analysis. The tape starts
all zero, and the pass tracks what it can prove about cell values through
adds, zeroing, stores and `MulAdd`. With that it drops adds of 0, `Zero` on
a cell that's already zero, `Scan` on a zero cell, a `MulAdd` whose source
is zero, and any loop entered on a cell known to be zero, which deletes the
whole loop. That last case is why a comment block written as a loop at the
start of a program costs nothing. A `MulAdd` from a known value turns into
a plain `SetAt` or `AddAt`. `Input` makes the cell unknown, and after a
loop or a `Scan` the pass forgets everything except that the current cell is
zero. It doesn't analyze loop bodies; they're copied through untouched, and
the other six passes still work inside them. It also tracks how much tape
has been allocated, so a zero-delta write past the end, which exists only to
grow the tape, is kept, and it stops rewriting as soon as the pointer could
leave the known range so the runtime's bounds behavior stays intact.

All seven are meant to be semantics-preserving, so optimized output runs
identically to unoptimized, including when a program walks off the left
edge or grows the tape. They only make it faster. This applies to every
backend, since `bfnative` and the rest reuse the same optimizer rather than
reimplementing it.

## Runtime behavior

The tape starts at the classic 30,000 cells but grows automatically if a
program walks off the end, instead of the undefined behavior you'd get
from the original 1993 spec. Walking left of cell 0 is a real error —
there's no sane direction to grow in that case. Cells are 8-bit and wrap
on overflow, matching standard Brainfuck semantics. That part's
deliberately unchanged, since plenty of existing programs assume
wraparound.

`bfrun` grows its tape with plain `Vec` reallocation. The native backends
can't do that, because the tape base is held in a register and moving it
would mean reloading it after every single op. So they use a
reserve-and-commit strategy instead: 1 GiB of address space reserved up
front as inaccessible (`mmap` PROT_NONE on Linux/macOS, `VirtualAlloc`
MEM_RESERVE on Windows), with the first 64 KiB committed
readable/writable for the initial 30,000 cells. When the pointer walks
past the committed capacity, the grow routine commits the next chunk out
of the same reservation (`mprotect` on Linux/macOS, `VirtualAlloc`
MEM_COMMIT on Windows), doubling the capacity each time and never moving
the base, so the pointer register stays valid. All chunk sizes are
multiples of 64 KiB, so they're page-aligned on 4 KiB, 16 KiB, and 64 KiB
kernels alike without querying the page size.

The one visible limit: a program that walks past the full 1 GiB gets
`runtime error: tape limit exceeded (1073741824 bytes)` and exit status 1,
not a crash. `bfrun`'s tape has no fixed cap, but it reports a failed
growth the same way (`runtime error: failed to grow tape`, exit 1) when
the allocation fails. Left-of-zero underflow produces `bfrun`'s
exact message and exit code on every platform, same as EOF-on-input
leaving the cell untouched.

`bfnative` matches the interpreter at the instruction level too. An
x86-64 `add byte ptr [cell], n` wraps for free because it's an 8-bit
store, and the ARM64 backend gets the same wraparound from `strb` only
keeping the low byte of a 32-bit add.

## Format versioning (.bfry)

Every `.bfry` file starts with a 4-byte magic number and a version byte.
The runtime checks both before parsing anything else, so a corrupt file
or a version mismatch fails immediately with a clear message instead of
misparsing garbage further in. This only applies to `bfc`/`bfrun` — a
`bfnative`-compiled binary is a normal executable for its platform, with
no custom format of its own.

`FORMAT_VERSION` is 2. It went up from 1 when the optimizer's fixed-offset
ops (`AddAt`, `ZeroAt`, `SetAt`, `BoundsCheck`, tags 11-14) were added, so a
current `bfrun` rejects a version-1 file with a clear message, and an older
`bfrun` rejects a version-2 file the same way. The bump was done by hand,
though. Nothing forces it when the tag range grows, so the next new op
without a bump would be misparsed by older builds instead of rejected.

## Verification status

Every target now runs for real. `.github/workflows/test.yml` builds all
six on native GitHub-hosted runners and runs `scripts/differential_test_all.py`.
The 34-program corpus is compared with `bfrun` across the interpreter,
native/JIT, C, WASI, and JavaScript backends. `bfjs` and `bfwasm` are also
checked under Node.js 22, 24, and 26 on each target; Node 22 and 24 use
`--only-node` to avoid rerunning the other backends. On Node 26, Playwright
executes both outputs in Chromium, Firefox, and WebKit. Browser checks use
empty stdin (EOF), and compare stdout, stderr, and exit status to `bfrun`.
CI installs the required runtimes and uses `--require-all`, so a missing
runtime or compiler fails instead of silently omitting its lane.

The Rust workspace has verified 100% test coverage on all six targets:
functions, lines, regions and branches, measured by `cargo-llvm-cov` in
each CI job, which fails below that. Code marked `coverage(off)` isn't counted; the comments in the
source explain why those items are excluded.

`bfwasm` was the last backend added to the harness, and its first runs
found two real bugs. The optimizer's folded scan and multiply-add ops lose
their loop's zero check, and `bfwasm` hadn't put it back, so `[<]` on a
zero cell stepped left anyway and `mandelbrot.b` died at the left edge.
Separately, left-of-cell-0 was a bare trap with no message instead of
`bfrun`'s error and exit status 1. Both are fixed and both have small
regression programs in the corpus. The module is exercised by wasmtime,
Node's built-in `node:wasi` API, and the browser preview1 shim.

| Target | Status | Runner | Also checked statically |
|---|---|---|---|
| `linux-x86_64` | **executed** | `ubuntu-latest` | `cargo test` end-to-end suite |
| `linux-aarch64` | **executed** | `ubuntu-26.04-arm` | `cargo test` end-to-end suite; earlier run under `qemu-aarch64-static` (user mode, real Linux syscalls): 50/50 executions matched `bfrun` |
| `macos-x86_64` | **executed** | `macos-26-intel` | `cargo test` end-to-end suite; load-command table, segment congruence and bind stream decoded by `llvm-objdump --macho`; ad-hoc signature page hashes re-verified by an independent unit test; format cross-checked against real ld64-produced binaries |
| `macos-aarch64` | **executed** | `macos-latest` | `cargo test` end-to-end suite; same, plus 16 KiB-page layout checks |
| `windows-x86_64` | **executed** | `windows-latest` | `cargo test` end-to-end suite; full header/import/section parse by GNU `objdump` (`pei-x86-64`), IAT slot layout and entry disassembly |
| `windows-aarch64` | **executed** | `windows-11-arm` | `cargo test` end-to-end suite; headers walked field-by-field by unit tests, machine code disassembled by `llvm-objdump` |

Static validation was the only evidence for the macOS and Windows targets
for a long time, and it wasn't enough on its own. The signature,
bind-opcode and import-table details were cross-checked against real
`ld64` output, which caught a bind-opcode encoding bug before it reached
a Mac. But the Windows ARM64 image passed every offline parse and was
still refused by the loader until two header flags were corrected (see
the Windows paragraph above). "Structurally perfect by every tool that
will look at it" isn't the same thing as "ran", so the runner column is
the one that counts.

## What's not here

Loop folding stops at linear loops and plain scans. A loop with I/O, a
nested loop, a body that visits a cell it doesn't write, an even counter
step, or a net pointer drift per iteration still runs as an actual loop.
Dead-code elimination also doesn't look inside loop bodies. Folding more
would need real loop analysis (induction variables, or tracking values
across iterations), and the cost of a wrong fold is a program that silently
computes something different, so these fall back to a real loop rather than
guess at a pattern they can't fully verify.

CI now runs `cargo test` on all six targets, alongside the differential
harness, so the unit tests (PE and Mach-O header checks, instruction
encoders) are no longer only checked locally.

`bfwasm` is exercised under wasmtime, Node's built-in `node:wasi` API,
and Chromium, Firefox, and WebKit with a minimal WASI preview1 shim.

Each target is exercised on one runner image (listed above). Other
Windows and macOS releases, or older Linux distributions, aren't covered.
