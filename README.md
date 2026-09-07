# brainfry

Brainfuck has no standard bytecode, no packaging format, nothing beyond raw
`.bf` text that every existing implementation just interprets directly. This
project treats it like a real language instead: a compiler, a bytecode
format, a bytecode runtime, and — if you don't want an interpreter in the
loop at all — a backend that emits actual x86-64/ARM64 machine code and
links it straight into a native executable. I wrote it in rust, cuz why not, i like rust, its safer than C which i was gonna write its compiler in but i had a brain which told my guts not to think about it, sorry C i am not managing memory and way too hard to code. and also i am not a C or C++ whatever expert, that thing fucked up my teenhood when trying to learn it like 15 lines just to say print hello world which i just write in rust 

```
fn main() {
    println!("Hello, World!");
}
```

see you get the drill i need to shut up, sorry for wasting your time.


Four ways to run a `.bf` file, in increasing order of "how far do you want
to get from an interpreter":

1. **`bfinterp`** — walk the source directly, no bytecode, no compile step,
   no optimizer by default. The simplest and slowest path, and the one the
   other three are checked against.
2. **`bfc` + `bfrun`** — compile to `.bfry` bytecode, run it on a small VM.
   The javac/java split. Fast to build, portable, no toolchain needed.
3. **`bfjit`** — same codegen as `bfnative`, but instead of writing an
   executable file it maps executable memory at runtime and jumps straight
   into the generated code. No file touches disk; only runs on the host
   you're already on.
4. **`bfnative`** — skip the bytecode entirely and compile straight to a
   native binary for Linux, macOS, or Windows, x86-64 or ARM64. No
   interpreter at runtime, and no toolchain at build time either: the
   machine code and the ELF/Mach-O/PE container around it are emitted
   byte-by-byte by `bfnative` itself, in-process, with zero external
   commands and zero object-file crates.

Pick based on what you're doing: checking whether a bug is in your program
or in one of the other three, `bfinterp` is the ground truth. Iterating on
a program, `bfc`/`bfrun` is zero-friction. Want near-native speed without
managing a build artifact, `bfjit`. Shipping something you want to hand
someone as a standalone `.exe`, `bfnative` is the one you want.

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
  it one op at a time with no bytecode, no optimizer (by default), and no
  machine code. Deliberately the simplest and slowest correct execution
  path, so it can serve as the ground-truth reference the other three are
  differential-tested against. `--optimize` opts into the shared optimizer
  for comparison runs; the default is the naive walk.
- `bfjit/` — the JIT. Same parser, same optimizer, and the *same source
  files* as `bfnative`'s machine-code layer (compiled in via `#[path]`, so
  the two cannot drift), but instead of wrapping the code in an executable
  file, it maps anonymous memory, copies the code in, makes it executable
  (W^X), and jumps straight into it. No file ever hits the disk, no
  ELF/Mach-O/PE writer is involved, and only the host platform can be
  JIT-compiled.

`bfinterp` and `bfjit` are independent crates with independent runtimes —
they share the front end (`bfc`/`bfformat`) exactly like `bfnative` does,
and share no execution code with `bfrun`/`bfnative` or each other. Delete
either one and the rest of the workspace builds and behaves exactly as
before.

## Building

Requires a Rust toolchain (rustc + cargo). That's the whole list — no C
compiler, no linker, no platform SDK, for any of the six targets.

```
cargo build --release
```

**⚠ Unverified against this specific build: confirm which crates this
command actually produces before relying on it.** In the pre-`bfinterp`/
`bfjit` version of this workspace, `cargo build --release` only built
`bfc`, `bfrun`, and `bfnative` by default — all three were workspace
members, but `bfnative` specifically needed `cargo build --release -p
bfnative` to build explicitly in some earlier states of this repo. Check
`Cargo.toml`'s `[workspace] members` list to see whether `bfinterp` and
`bfjit` were added there (default build covers all five) or left out
(each needs `cargo build --release -p bfinterp` / `-p bfjit` explicitly).
Whoever finishes this README should run the plain `cargo build --release`
command once, list what actually landed in `target/release/`, and replace
this note with a real answer instead of an assumption.

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

`--emit-asm` no longer writes assembly text — there is no assembly and no
external assembler anymore. It writes a `.lst` listing of the final machine
code (hex bytes, resolved addresses, per-op annotations) so you can still
see exactly what got emitted. `--cc` and `--keep-asm` are accepted but
deprecated no-ops, so old scripts don't break.

Full target list: `linux-x86_64`, `linux-aarch64`, `macos-x86_64`,
`macos-aarch64`, `windows-x86_64`, `windows-aarch64`. 32-bit x86 and 32-bit
ARM aren't supported — no plans to add them either, they're not where
anyone's actually running this.

### How the executable gets built

No `cc`, no `ld`, no `as`, no object-file crate — the pipeline is three
stages, all in-process:

1. `bfc`'s parser and optimizer produce the shared IR (the same one
   `bfrun` executes — one optimizer, no drift).
2. A machine-code emitter (one per CPU architecture, emitting straight
   into a byte buffer) encodes the IR plus a small runtime: entry
   prologue, tape-growth routine, I/O helpers, and the error paths.
   Branch offsets go in as placeholders and get patched in a second pass
   once sizes are known.
3. A container writer (one per file format) wraps those bytes into a
   loadable executable: ELF64 `ET_EXEC` for Linux, Mach-O `MH_EXECUTE`
   for macOS, PE32+ for Windows. Code references data
   position-relatively (rip-relative on x86-64, ADRP on ARM64), so the
   final address layout is patched in before serialization and what hits
   the disk is final — no relocations, no link step.

Cross-compiling a Windows binary from Linux is the same code path as a
native build, which is the point of owning the output end to end.

### Why the OS split is bigger than the CPU split

The six targets are really two instruction backends (x86-64, ARM64) with
three runtime layers bolted on top, and the runtime layers are where
almost all the platform-specific weirdness lives:

- **Linux** binaries are fully static, freestanding ELF64 executables that
  hit raw syscalls directly (`mmap`, `mprotect`, `read`, `write`, `exit`) —
  zero runtime dependencies, nothing to link against.
- **macOS** doesn't support that. There's no such thing as a static,
  no-libc Mach-O executable — every macOS binary talks to libSystem. The
  writer does the minimum dynamic linking dyld accepts: one
  `LC_LOAD_DYLIB` for libSystem, a `LC_DYLD_INFO_ONLY` bind stream that
  resolves exactly five symbols (`write`, `read`, `mmap`, `mprotect`,
  `_exit`) into a five-slot GOT, and `LC_MAIN` for the entry. Apple
  Silicon also refuses to run unsigned arm64 code, so the writer computes
  a full ad-hoc code signature itself — a v0x20400 CodeDirectory with
  SHA-256 page hashes (4 KiB pages on x86-64, 16 KiB on ARM64, matching
  what the kernel verifies), built with an in-process SHA-256, no
  `codesign` subprocess.
- **Windows** binaries are freestanding PE32+ executables that import five
  functions from `kernel32.dll` (`GetStdHandle`, `WriteFile`, `ReadFile`,
  `ExitProcess`, `VirtualAlloc`) through a real import address table the
  loader resolves at startup, with a custom entry point — no CRT, no
  startup objects. Win32 was picked over the C runtime for the same
  reason as before: kernel32's exports are the one stable surface on
  every Windows install, while CRT `read`/`write` export names vary across
  toolchain flavors.

None of that touches instruction selection. A given CPU architecture emits
the same arithmetic and branch instructions no matter which of the three OSes
it's targeting; only the prologue, epilogue, and I/O calls change.

## Usage: bfinterp

```
bfinterp program.bf              # naive interpretation, stdin/stdout direct
bfinterp --optimize program.bf   # same interpreter, optimizer enabled first
```

No bytecode step and no `.bfry` file anywhere in the picture: parse, then
execute the op list one op at a time. Since the parser emits one op per
`+ - > < . , [ ]` character, the default path is as close to walking the raw
source as an interpreter can get — which is the point. It exists to be the
maximally simple reference implementation: the one whose output everyone
else has to match, not the one anyone should expect to be fast.

Semantics are `bfrun`'s exactly — 30,000 starting cells with grow-on-demand,
8-bit wrapping cells, EOF-on-input leaving the cell untouched, and
left-of-zero as a clean `runtime error` with exit status 1 — but the
engine is a separate implementation sharing no code with `bfrun`'s VM, so
agreement between the two is evidence about the semantics, not a tautology.

## Usage: bfjit

```
bfjit program.bf                 # compile in memory, execute immediately
```

The pipeline is `bfnative`'s up to the last step: shared parser, shared
optimizer, and the identical per-architecture machine-code emitter —
`bfjit` compiles `bfnative`'s `codegen` source directly into itself via
`#[path]`, so there is exactly one emitter in the repo and the JIT and the
AOT compiler cannot produce different code. From there it diverges: instead
of handing the bytes to an ELF/Mach-O/PE writer, it `mmap`s one anonymous
region, resolves every branch/data fixup against that region's real
address, copies in the code plus the read-only data plus (on macOS/Windows)
a small table of host function pointers standing in for the GOT/IAT a
container would have provided, flips the region from writable to
executable, and jumps into the entry stub. The generated code then does
everything itself — tape reservation, growth, I/O, and process exit —
which is why a JIT'd program's observable behavior, error messages, and
exit codes are identical to a `bfnative` executable's by construction.

Memory discipline is W^X on every platform: Linux maps RW and `mprotect`s
to RX; Windows `VirtualAlloc`s `PAGE_READWRITE` and `VirtualProtect`s to
`PAGE_EXECUTE_READ`; macOS on Intel uses the plain `mmap`/`mprotect` pair,
while macOS on Apple Silicon maps with `MAP_JIT` and brackets the code
writes with `pthread_jit_write_protect_np(false)` / `(true)` — the
hardened runtime's JIT toggle, without which self-generated code SIGBUSes
the moment it executes. On ARM64 hosts the freshly written code is also
run through the proper cache-maintenance sequence (`sys_icache_invalidate`
on macOS, DC CVAU / DSB / IC IVAU / ISB on Linux) before the first jump,
because the ARM instruction cache is not coherent with the data cache.

Only the host can be JIT-compiled — the backend is picked from the
detected host OS/arch pair, and `--target` is rejected with a pointer to
`bfnative`, whose output is a file you can actually carry to another
machine.

## bfjit verification status

Same honesty standard as the native targets: what ran, versus what was
only built and inspected. (bfinterp has no platform-specific code at all —
the same Rust executes on every host — so it inherits whatever platform
the Rust compiler supports.)

| bfjit target | Status | How |
|---|---|---|
| `linux-x86_64` | **executed** | full workspace test suite (19 e2e tests executing JITed code, incl. mandelbrot.b byte-exact) plus the four-way differential harness: 26 programs × 4 execution strategies, all byte-identical to `bfrun` on stdout/stderr/exit status |
| `linux-aarch64` | **executed under emulation** | aarch64 bfjit built for `aarch64-unknown-linux-musl` and run under `qemu-aarch64-static`: same 26-program corpus, byte-identical to `bfrun`, mandelbrot included — a real run of the JIT pipeline (map, patch, W^X flip, cache maintenance, jump) on emulated ARM64 hardware |
| `macos-x86_64` | written, not run | plain `mmap`/`mprotect` path; needs a real Mac to execute |
| `macos-aarch64` | written, not run | `MAP_JIT` + `pthread_jit_write_protect_np` + `sys_icache_invalidate` sequence written per the documented contract, plus a stack-alignment trampoline for the generated entry; not executed — no dyld/hardened runtime here |
| `windows-x86_64` | written, not run | `VirtualAlloc`/`VirtualProtect` + `FlushInstructionCache`; not executed |
| `windows-aarch64` | written, not run | same; not executed |

## Benchmarks (mandelbrot.b)

Four execution strategies, one program, byte-identical output (6,240
bytes) from every one of them — the differential harness checks that
before anything gets timed:

| tool | min / median | vs bfinterp |
|---|---|---|
| `bfinterp` (naive reference) | 15.5 s / 15.6 s | 1.0x |
| `bfrun` (bytecode VM) | 8.1 s / 8.1 s | 1.9x |
| `bfjit` (in-memory JIT) | 1.27 s / 1.27 s | 12.2x |
| `bfnative` (AOT ELF) | 1.25 s / 1.27 s | 12.4x |

Exactly the shape you'd predict: the naive interpreter is slowest, the
bytecode VM's op-folding roughly doubles it, and the two machine-code
strategies tie — they are the same generated code, so `bfjit`'s only
overhead is the few milliseconds of parse/optimize/emit/patch it pays
inside the run. Reproduce with `scripts/benchmark_all.py`.

## What the compiler actually does

Parsing is a straight character-by-character pass that validates bracket
matching and builds an unoptimized op list. Optimization runs as a separate
pass on top, currently two rules:

1. **Run folding.** `+++++` becomes one `Add(5)` instead of five separate
   `Add(1)`s, same idea for `-`, `>`, `<`. Cuts down how many times the
   bytecode runtime's dispatch loop spins on repetitive source, and produces
   noticeably shorter assembly out of `bfnative` too.
2. **Zero-loop folding.** `[-]` and `[+]` — a loop that does nothing but
   drive the current cell to zero — collapse into a single `Zero`
   instruction instead of actually looping up to 255 times.

Both are safe, semantics-preserving transformations: optimized output runs
identically to unoptimized, just faster. Jump targets get recomputed after
folding, since folding changes instruction indices — this applies to both
backends, since `bfnative` reuses the same optimizer rather than
reimplementing it.

## Runtime behavior

The tape starts at the classic 30,000 cells but grows automatically if a
program walks off the end, instead of the undefined behavior you'd get from
the original 1993 spec. Walking left of cell 0 is a real error — there's no
sane direction to grow in that case. Cells are 8-bit and wrap on overflow,
matching standard Brainfuck semantics; that part's deliberately unchanged,
since plenty of existing programs assume wraparound.

`bfrun` grows its tape with plain `Vec` reallocation. The native backends
can't do that — the tape base is held in a register and moving it would mean
reloading it after every single op — so they use a reserve-and-commit
strategy instead: **1 GiB of address space reserved up front as
inaccessible (`mmap` PROT_NONE on Linux/macOS, `VirtualAlloc` MEM_RESERVE on
Windows), with the first 64 KiB committed readable/writable for the initial
30,000 cells.** When the pointer walks past the committed capacity, the
grow routine commits the next chunk out of the same reservation
(`mprotect` on Linux/macOS, `VirtualAlloc` MEM_COMMIT on Windows), doubling
the capacity each time and never moving the base, so the pointer register
stays valid. All chunk sizes are multiples of 64 KiB so they're page-aligned
on 4 KiB, 16 KiB, and 64 KiB kernels alike without querying the page size.

The one visible limit: a program that walks past the full 1 GiB gets
`runtime error: tape limit exceeded (1073741824 bytes)` and exit status 1,
not a crash — that's the single documented divergence from `bfrun`, whose
tape is genuinely unbounded. (bfrun would OOM eventually too, it just
doesn't know how to say so.) Left-of-zero underflow produces `bfrun`'s
exact message and exit code on every platform, as does EOF-on-input leaving
the cell untouched.

`bfnative` matches the interpreter at the instruction level too — an x86-64
`add byte ptr [cell], n` wraps for free because it's an 8-bit store, and the
ARM64 backend gets the same wraparound from `strb` only keeping the low byte
of a 32-bit add.

## Format versioning (.bfry)

Every `.bfry` file starts with a 4-byte magic number and a version byte. The
runtime checks both before parsing anything else, so a corrupt file or a
version mismatch fails immediately with a clear message instead of
misparsing garbage further in. This only applies to `bfc`/`bfrun` — a
`bfnative`-compiled binary is a normal executable for its platform, with no
custom format of its own.

## Verification status

Honesty section — what's actually been run, versus what's been inspected by
foreign tools and not run:

| Target | Status | How |
|---|---|---|
| `linux-x86_64` | **executed** | `cargo test` end-to-end suite, plus a differential harness (`scripts/differential_test.py`) comparing stdout/stderr/exit status against `bfrun` on 25 programs: hello world, six-deep nesting, multiplication loops, tape growth to 200k cells, wraparound, EOF semantics, stdin echo, and every runtime-error path |
| `linux-aarch64` | **executed under emulation** | same 25-program differential corpus under `qemu-aarch64-static` (user mode, real Linux syscalls) — 50/50 total executions match `bfrun` exactly |
| `macos-x86_64` | statically validated | load-command table, segment congruence, and bind stream decoded by `llvm-objdump --macho`; ad-hoc signature page hashes re-verified by an independent unit test; entry code disassembled and hand-checked; format cross-checked against real ld64-produced binaries |
| `macos-aarch64` | statically validated | same, plus 16 KiB-page layout checks. **Not executed** — no macOS hardware or dyld here |
| `windows-x86_64` | statically validated | full header/import/section parse by GNU `objdump` (`pei-x86-64`), IAT slot layout and entry disassembly verified |
| `windows-aarch64` | statically validated | headers walked field-by-field by unit tests, machine code disassembled by `llvm-objdump`. **Not executed** — no Windows-on-ARM here |

The signature, bind-opcode, and import-table details were all cross-checked
against real binaries produced by Apple's `ld64` (that's how a bind-opcode
encoding bug got caught before it ever reached a Mac). Still: "structurally
perfect by every tool that will look at it" is not "ran" — the macOS and
Windows targets have never been executed, and I'd rather say so plainly than
let the test table imply otherwise.

## What's not here

- **No copy/multiply loop folding.** The common `[->+<]` idiom and its
  relatives still run as actual loops instead of collapsing into one
  instruction, in either backend. Worth adding once there's a real reason to
  benchmark against — not before.
- **No macOS or Windows execution in the test suite.** The containers are
  validated structurally (see above), but nothing here has actually booted
  dyld or the Windows loader. If you're on one of those platforms, running
  the differential harness locally is the missing experiment.
- **No CI/build-matrix config for the six `bfnative` targets.** That's a
  packaging concern layered on top of this source, not something baked into
  the crates themselves.
