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

Five ways to run a `.bf` file, in increasing order of "how far do you want
to get from an interpreter":

1. **`bfinterp`** — walk the source directly, no bytecode, no compile step,
   no optimizer by default. The simplest and slowest path, and the one the
   other four are checked against.
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
   Node with `--experimental-wasi-unstable-preview1`, a browser with a
   WASI shim) instead of a specific OS/CPU pair — the one backend in this
   project whose output isn't tied to the machine it was compiled on.

Checking whether a bug is in your program or in one of the other four?
`bfinterp` is the ground truth. Iterating on a program, `bfc`/`bfrun` is
zero-friction. Want near-native speed without managing a build artifact —
`bfjit`. Shipping something you want to hand someone as a standalone
`.exe`, that's `bfnative`. Shipping something that runs anywhere a wasm
runtime does, independent of OS or CPU — that's `bfwasm`.

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
  can serve as the ground-truth reference the other four are
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

`bfinterp`, `bfjit`, and `bfwasm` are independent crates with independent
runtimes/emitters. They share the front end (`bfc`/`bfformat`) exactly like
`bfnative` does, and share no execution code with `bfrun`/`bfnative` or each
other. Delete any one of them and the rest of the workspace builds and
behaves exactly as before.

## Building

Requires a Rust toolchain (rustc + cargo). That's the whole list — no C
compiler, no linker, no platform SDK, for any target, including wasm.

```
cargo build --release
```

This builds all six binaries — `bfc`, `bfrun`, `bfnative`, `bfinterp`,
`bfjit`, and `bfwasm` — in one shot. Every crate is listed in
`Cargo.toml`'s `[workspace] members`, so there's no special-casing needed
for any of them.

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
bfwasm program.bf              # writes program.wasm
bfwasm program.bf -o out.wasm  # or pick the output name yourself
wasmtime run program.wasm      # any WASI preview1 runtime
```

The output is a standalone WASI module, not a browser one. It does its I/O
through `fd_read` and `fd_write`, which browsers don't provide, so running
it on a web page means supplying a WASI shim yourself.

Semantics are `bfrun`'s: 30,000 starting cells, 8-bit wrapping, EOF leaving
the cell untouched. Moving left of cell 0 prints `runtime error: pointer
moved left of cell 0` to stderr and exits with status 1 (through WASI's
`proc_exit`), same as every other backend.

Two failures can't do that. If `memory.grow` is refused (the tape is capped
at 1 GiB, same as `bfnative`) or a WASI read or write returns an error, the
module hits a wasm trap, which has no text to print. What a trap turns into
(exit status, any message) is up to the runtime, and the differential
harness doesn't exercise either path.

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

Pretty much the shape you'd predict: the naive interpreter is slowest, the
bytecode VM's op-folding roughly doubles it, and the two machine-code
strategies tie, because they're the same generated code — `bfjit`'s only
overhead is the few milliseconds of parse/optimize/emit/patch it pays
inside the run. Reproduce with `scripts/benchmark_all.py`.

## What the compiler actually does

Parsing is a straight character-by-character pass that validates bracket
matching and builds an unoptimized op list. Optimization runs as a separate
pass on top, currently four rules, each running in sequence on the output
of the last. Run folding turns `+++++` into one `Add(5)` instead of five
separate `Add(1)`s (same idea for `-`, `>`, `<`), which cuts down how many
times the bytecode runtime's dispatch loop spins on repetitive source and
produces noticeably shorter assembly out of `bfnative` too. Zero-loop
folding takes `[-]` and `[+]` — a loop that does nothing but drive the
current cell to zero — and collapses it into a single `Zero` instruction
instead of actually looping up to 255 times. It only fires when the
per-iteration step is odd: an odd step reaches 0 from any starting value
(it's a unit mod 256), while an even step from an odd cell never lands on
0 at all — `[++]` with the cell holding 3 stays odd forever — so folding
that would silently turn a diverging loop into a terminating one.
Even-step loops stay real loops.

Offset-add folding targets the `[->+<]` idiom and its relatives: a loop
that decrements the current cell by 1 per iteration while adding to one or
more other cells and returning to its starting position each time. Since a
loop like that always runs exactly N times for a starting value of N, the
result is fully determined before the first iteration — so it collapses
into one `MulAdd` per target cell (or several, for patterns like
`[->+>+<<]` that spread a value into more than one place) followed by a
`Zero`, instead of actually iterating. This only fires when every
condition holds: the loop returns to its start, the counting cell's net
change is exactly `-1`, every other touched cell only gets added to,
every cell the pointer lands on is one the body actually writes to, and
the body has no `Output`, `Input`, or nested loop — any of those would
mean collapsing the loop changes something other than speed, like how many
times a side effect fires. The write-free condition covers the subtle one:
a body like `[<>-]` touches down on cell -1 without writing it, and while
that excursion has no effect on any cell's value, it still has to be
bounds-checked when the real loop runs — a folded `MulAdd`/`Zero` body
can't perform it, so the underflow the real loop raises would silently
disappear. A loop that doesn't meet all five stays a real
loop rather than risk folding something unsafe.

Scan-loop folding handles the other common idiom, `[>]` / `[<]` and their
multi-cell-stride variants — a loop whose entire body is a single move,
used to walk the tape until landing on a zero cell. That collapses into a
single `Scan` instruction carrying the stride and direction, which removes
the per-cell jump-check-jump overhead without changing how many cells
actually get inspected.

All four are safe, semantics-preserving transformations: optimized output
runs identically to unoptimized, just faster. Jump targets get recomputed
once at the end, after every fold, since folding changes instruction
indices. This applies to both backends, since `bfnative` reuses the same
optimizer rather than reimplementing it.

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

The version check only guards one direction: it stops a `bfrun` build
from accepting a `.bfry` file from a *newer* format than it understands,
but nothing currently forces `FORMAT_VERSION` to bump when the op tag
range grows. `MulAdd`/`Scan` (tags 9-10) are new as of the offset-add and
scan-loop optimizer passes; a `.bfry` file compiled with them and read by
a `bfrun` build old enough to only know tags 0-8 will misparse those
bytes rather than fail cleanly. Whether that's worth a version bump now
or only once it matters for a real compatibility case is an open call,
not something this doc is deciding.

## Verification status

Every target now runs for real. `.github/workflows/test.yml` builds all
six on a native GitHub-hosted runner (same OS and architecture, no
emulation) and runs `scripts/differential_test_all.py`: 33 programs (the
27-program corpus, five regression programs for folded scans and multiply
loops, plus `mandelbrot.b`) through `bfinterp`, `bfinterp --optimize`,
`bfrun`, `bfnative`, `bfjit` and `bfwasm`, with stdout, stderr and exit
status required to be byte-identical to `bfrun`. The corpus covers hello
world, six-deep nesting, multiplication loops, tape growth to 200k cells,
wraparound, EOF semantics, stdin echo, and every runtime-error path.

The Rust workspace has 100% verified test coverage, measured with
`cargo-llvm-cov`.

`bfwasm` was the last backend added to the harness, and its first runs
found two real bugs. The optimizer's folded scan and multiply-add ops lose
their loop's zero check, and `bfwasm` hadn't put it back, so `[<]` on a
zero cell stepped left anyway and `mandelbrot.b` died at the left edge.
Separately, left-of-cell-0 was a bare trap with no message instead of
`bfrun`'s error and exit status 1. Both are fixed and both have small
regression programs in the corpus. The module runs under `wasmtime` through
its Python package, which CI installs with pip. If that install fails on a
runner, the lane is skipped with a warning rather than failing the build,
so check the job's annotations before assuming wasm is covered there.

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

No general-purpose loop folding beyond the offset-add and scan patterns.
A loop that touches a cell with anything other than a net `+1`/`-1`
step, or that both adds to and subtracts from other cells, still runs as
an actual loop — the two folding passes are conservative by design and
fall back to a real loop rather than guess at a pattern they can't fully
verify.

CI now runs `cargo test` on all six targets, alongside the differential
harness, so the unit tests (PE and Mach-O header checks, instruction
encoders) are no longer only checked locally.

`bfwasm` is only ever run under `wasmtime`. The module should work on any
WASI preview1 runtime, but CI doesn't try `wasmer`, Node or a browser shim,
and runtimes differ on what a trap does (see Usage: bfwasm).

Each target is exercised on one runner image (listed above). Other
Windows and macOS releases, or older Linux distributions, aren't covered.
