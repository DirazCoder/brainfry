# brainfry

Brainfuck has no standard bytecode, no packaging format, nothing beyond raw
`.bf` text that every existing implementation just interprets directly. This
project treats it like a real language instead: a compiler, a bytecode
format, a bytecode runtime, and — if you don't want an interpreter in the
loop at all — a backend that emits actual x86-64/ARM64 machine code and
links it straight into a native executable.

Three ways to run a `.bf` file, in increasing order of "how far do you want
to get from an interpreter":

1. **`bfc` + `bfrun`** — compile to `.bfry` bytecode, run it on a small VM.
   The javac/java split. Fast to build, portable, no toolchain needed.
2. **`bfnative`** — skip the bytecode entirely and compile straight to a
   native binary for Linux, macOS, or Windows, x86-64 or ARM64. No
   interpreter at runtime, but you'll need a C toolchain on your machine to
   assemble and link the output.

Pick based on what you're doing: iterating on a program, `bfc`/`bfrun` is
zero-friction. Shipping something you want to hand someone as a standalone
`.exe`, `bfnative` is the one you want.

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
  optimizer, then instead of writing bytecode, emits assembly for your
  target CPU and shells out to a real toolchain (gcc/clang) to turn that
  into a linked executable.

## Building

Requires a Rust toolchain (rustc + cargo).

```
cargo build --release
```

Lands `bfc` and `bfrun` in `target/release/`. `bfnative` isn't built by
default — grab it explicitly:

```
cargo build --release -p bfnative
```

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
bfnative --emit-asm program.bf               # just want the assembly, no linking
```

Full target list: `linux-x86_64`, `linux-aarch64`, `macos-x86_64`,
`macos-aarch64`, `windows-x86_64`, `windows-aarch64`. 32-bit x86 and 32-bit
ARM aren't supported — no plans to add them either, they're not where
anyone's actually running this.

### You need a real toolchain for this

`bfnative` writes assembly text, then hands it to `cc`/`gcc`/`clang` to
assemble and link. It doesn't touch object-file formats or linking itself —
building ELF/PE/Mach-O by hand would be several times the size of the whole
backend for zero improvement to the output, so it isn't worth doing.

What you need depends on the target:

- **Linux, compiling on Linux**: whatever `cc` you already have.
- **macOS, compiling on macOS**: same — `cc` via Xcode command line tools,
  `-arch` handles both Intel and Apple Silicon.
- **Windows**: needs a MinGW-family driver specifically —
  `x86_64-w64-mingw32-gcc` or the aarch64 equivalent. **MSVC's `cl.exe`
  will not work here, at all** — it expects MASM-syntax assembly, and
  `bfnative` only emits GNU syntax. Get MinGW-w64 through MSYS2 or grab a
  prebuilt llvm-mingw release; either gives you a working driver.
- **Cross-compiling** (e.g. building a Windows binary from Linux): pass
  `--cc` with whatever cross-driver you've got installed. The tool guesses a
  reasonable default per target, but cross-toolchain naming is inconsistent
  enough across distros that you'll often need to override it.

If the linker step fails, `bfnative` leaves the intermediate `.s` file
sitting next to your output on purpose — that assembly is what you actually
want to look at when a driver rejects something, and it's a lot easier to
diagnose with it in hand than without.

### Why the OS split is bigger than the CPU split

The six targets are really two instruction backends (x86-64, ARM64) with
three thin runtime layers bolted on top, and the runtime layers are where
almost all the platform-specific weirdness lives:

- **Linux** binaries are fully static, freestanding, and hit raw syscalls
  directly — zero runtime dependencies, nothing to link against.
- **macOS** doesn't support that. There's no such thing as a static, no-libc
  Mach-O executable — every macOS binary goes through an ordinary `cc` link
  against libSystem for I/O.
- **Windows** binaries are freestanding too, but instead of raw syscalls
  they call straight into `kernel32.dll` (`GetStdHandle`, `WriteFile`,
  `ReadFile`, `ExitProcess`) with a custom entry point wired up via
  `-Wl,-e,bf_start`. The Win32 API was picked over the C runtime
  specifically because `read`/`write` export names differ across CRT
  flavors and import libraries — kernel32's exports are the one thing every
  MinGW-family toolchain agrees on.

None of that touches instruction selection. A given CPU architecture emits
the same arithmetic and branch instructions no matter which of the three OSes
it's targeting; only the prologue, epilogue, and I/O calls change.

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

`bfnative` matches this behavior at the instruction level — an x86-64 `add
byte ptr [cell], n` wraps for free because it's an 8-bit store, and the
ARM64 backend gets the same wraparound from `strb` only keeping the low byte
of a 32-bit add.

## Format versioning (.bfry)

Every `.bfry` file starts with a 4-byte magic number and a version byte. The
runtime checks both before parsing anything else, so a corrupt file or a
version mismatch fails immediately with a clear message instead of
misparsing garbage further in. This only applies to `bfc`/`bfrun` — a
`bfnative`-compiled binary is a normal executable for its platform, with no
custom format of its own.

## What's not here

- **No copy/multiply loop folding.** The common `[->+<]` idiom and its
  relatives still run as actual loops instead of collapsing into one
  instruction, in either backend. Worth adding once there's a real reason to
  benchmark against — not before.
- **No hand-rolled object format or linker.** Covered above — `bfnative`
  deliberately delegates that to your system's toolchain rather than
  reimplementing ELF/PE/Mach-O and a linker from scratch.
- **No CI/build-matrix config for the six `bfnative` targets.** That's a
  packaging concern layered on top of this source, not something baked into
  the crates themselves.