# brainfry

Brainfuck is small enough that the runtime often decides what a program
means in practice. Does input at EOF leave the cell alone? Do cells wrap?
What happens when the pointer moves left of zero? Brainfry makes those
choices explicit, then checks several independent ways to run a program
against the same reference.

The workspace is a compiler, bytecode format, interpreter, VM, JIT, native
compiler, WebAssembly compiler, JavaScript and C transpilers, and an
interactive debugger. Pick the output that fits where you want to run the
program:

| Tool | Use it when |
|---|---|
| `bfinterp` | You want a direct, easy-to-audit reference run. |
| `bfc` + `bfrun` | You want a reusable `.bfry` bytecode file and VM. |
| `bfjit` | You want native execution on the current machine without saving an executable. |
| `bfnative` | You want a standalone executable for one of the six supported OS/CPU targets. |
| `bfwasm` | You want a WASI module or a browser loader/bundle. |
| `bfjs` | You want standalone JavaScript with no WASI shim. |
| `bf2c` | You want C source that you can compile with your own toolchain. |
| `bfdbg` | You want to inspect and change program state while it runs. |

## Build

Install Rust and build the workspace:

```sh
cargo build --release
```

The binaries are written to `target/release`. Cargo uses the Rust
toolchain and the host linker to build them. A separate C compiler,
Node.js, or WASI runtime is only needed when you choose to use generated
C, JavaScript, or WASI output.

## Run a program

### Bytecode

```sh
bfc program.bf                 # writes program.bfry
bfc program.bf output.bfry     # choose a different output path
bfrun program.bfry
```

`bfc` parses and optimizes the source before writing bytecode. `bfrun`
loads the `.bfry` file and executes it.

### Interpreter

```sh
bfinterp program.bf
bfinterp --optimize program.bf
```

By default, `bfinterp` runs the raw parsed instructions without the
optimizer. `--optimize` opts into the shared optimizer. The default is
useful as a reference when checking a backend that consumes optimized IR.

### Native executable

```sh
bfnative program.bf
bfnative --target windows-x86_64 program.bf -o program.exe
bfnative --emit-asm program.bf
```

With no target, `bfnative` builds for the current host. Its supported
targets are:

```text
linux-x86_64       linux-aarch64
macos-x86_64       macos-aarch64
windows-x86_64     windows-aarch64
```

It emits machine code and the executable container itself; it does not
call an assembler or linker. `--emit-asm` is a historical option name:
it writes an annotated `.lst` listing of the generated machine code, not
assembly language. `--cc` and `--keep-asm` remain accepted for older
commands, but are deprecated and ignored.

The output is for the selected target, so a cross-compiled executable
won't run on the build host unless that host matches the target.

### JIT

```sh
bfjit program.bf
```

`bfjit` parses, optimizes, and compiles the program in memory, then runs
it on the current machine. It has no cross-target option and writes no
executable file. Use `bfnative` when you need an artifact to copy to
another machine.

### WebAssembly

By default, `bfwasm` writes a standalone WASI Preview 1 module:

```sh
bfwasm program.bf
bfwasm program.bf -o output.wasm
wasmtime run output.wasm
```

Select browser output explicitly, or let a `.js` output path select it:

```sh
bfwasm program.bf --target browser
bfwasm program.bf -o program.js
```

The default browser output is one self-contained JavaScript file. It
embeds the compiled Wasm bytes and includes a small
`wasi_snapshot_preview1` shim. It makes no fetch request and needs no
sidecar file, so it can be used from a browser console or embedded in a
page, including a page opened with `file://`.

To keep the module separate, use `--split`:

```sh
bfwasm program.bf --target browser --split
bfwasm program.bf -o dist/app.js --split
```

This writes a JavaScript ES module and an adjacent `.wasm` file with the
same stem. The loader resolves the module relative to itself and tries
streaming instantiation before falling back to an `arrayBuffer`. Since
it fetches the Wasm asset, serve both files over HTTP rather than opening
the loader with `file://`. `--split` requires browser mode; an explicit
`--target wasi` takes precedence over `.js` extension inference.

Both browser outputs read optional I/O hooks from `globalThis.__bfwasm`.
Set them before running the generated JavaScript:

```js
globalThis.__bfwasm = {
  stdin: "input bytes", // string or Uint8Array
  onStdout: chunk => console.log(chunk),
  onStderr: chunk => console.error(chunk),
};
```

Without `stdin`, input is EOF. Without output hooks, stdout and stderr
are buffered and printed to the console when the module exits. Hooks
receive live `Uint8Array` chunks. The final status is stored in
`globalThis.__bfwasmExitCode`. A Wasm trap is not a normal Brainfuck
runtime error; the browser shim reports it to the console and records
status 134.

### JavaScript

```sh
bfjs program.bf
node program.js
```

`bfjs` writes a standalone `.js` file. The generated program uses a
`Uint8Array` tape and includes its own runtime code; it does not import a
Brainfry package. The output runs under Node, Bun, Deno, or in a browser
console. Node and Bun write bytes through their file APIs, Deno uses its
stdout API, and browsers collect output for the console. Browser input
is EOF.

### C

```sh
bf2c program.bf
cc -O2 -o program program.c
./program
```

`bf2c` writes C99 source; a C compiler is needed to turn that source into
an executable. The generated code handles tape growth, input, and
output. On Windows it switches standard streams to binary mode so
Brainfuck I/O remains byte-oriented.

## Debug a program

```sh
bfdbg program.bf
bfdbg --stdin input.txt --break @3:5 program.bf
```

`bfdbg` reads raw source, one instruction at a time. This keeps each
step associated with its source line and column. It does not load
`.bfry` files or optimized IR.

Inside the session, `help` lists the commands. Common ones:

| Command | What it does |
|---|---|
| `run` / `continue` | Run until a breakpoint, watchpoint, error, or program end. |
| `step [n]` | Execute one instruction, or `n` instructions. |
| `back [n]` | Rewind the requested number of instructions. |
| `break 42` / `break @3:5` | Stop at an instruction index or source location. |
| `watch ptr 10` | Stop when the pointer reaches cell 10. |
| `watch cell 10` | Stop when cell 10 changes. |
| `set cell[10] = 65` / `set ptr = 3` | Edit a cell or move the pointer. |
| `feed 65 66` | Queue input bytes for `,`. |
| `tape` / `list` / `info` | Inspect the tape, source, or debugger state. |

Program output goes to stdout. Debugger messages go to stderr, which
keeps output byte-clean in scripted sessions. The undo log holds up to
1,048,576 steps by default; `--undo-limit` changes the cap and `0`
disables history. When history is disabled, `back` is unavailable.

## Shared behavior and optimization

All execution backends use the same basic tape rules:

- The tape starts with 30,000 zeroed cells and grows to the right as needed.
- Cells are 8-bit values; addition and subtraction wrap modulo 256.
- Input at EOF leaves the current cell unchanged.
- Moving left of cell 0 is an error.
- Tape growth is capped at 1 GiB. Allocation failures also produce runtime errors.

`bfrun`, `bfinterp`, `bfjit`, `bfnative`, `bfwasm`, `bfjs`, and `bf2c`
share the parser and optimizer in `bfc`; their execution engines and code
generators are separate. `bfdbg` uses the parser but keeps raw
instructions so its source positions remain meaningful.

The optimizer runs to a fixed point. Its passes fold repeated
instructions, zeroing loops, simple scans, and eligible linear loops;
combine pointer movement into fixed-offset operations; merge constant
stores; and remove operations whose results are already known. Jump
targets are recalculated after the rewrites.

Linear-loop folding handles loops such as `[->+<]` and some loops with
other odd counter steps. The counter must return to the same pointer
position, and the loop body must be simple additive updates and moves.
I/O, nested loops, even counter steps, and pointer excursions that don't
write every visited cell prevent the fold. That last restriction keeps
underflow errors: visiting cell -1 can fail even when the program never
changes that cell.

Pointer-window fusion retains the minimum and maximum positions visited
by the original moves. It emits a bounds check before replacing those
moves with offset operations, so an optimization does not hide an
underflow or required tape growth. The optimizer avoids rewriting paths
when it cannot preserve those checks.

The `.bfry` file begins with the `BFRY` magic and format version 2.
`bfrun` checks the header and validates jump pairs before execution.
The bytecode contains the optimized instructions and resolved jump
targets; it is not a portable executable.

## Benchmarks

These `mandelbrot.b` measurements were taken on Windows 11 Pro x64, an
AMD Ryzen 7 4700U, Node.js 26.13.1, and MSVC `cl /O2`. Each backend
matched the 6,240-byte reference output. The table shows three runs
after one warm-up; it is a snapshot from this machine, not a general
ranking. AOT compile time is excluded. `bfinterp` includes parsing and
execution; `bfjit` includes its compile step; Node process startup is
included for `bfjs` and `bfwasm`.

| Tool | Minimum / median | Faster than `bfinterp` |
|---|---:|---:|
| `bfinterp` | 18.334 s / 18.510 s | 1.0x |
| `bfrun` | 8.415 s / 8.532 s | 2.2x |
| `bfjit` | 1.617 s / 1.659 s | 11.3x |
| `bfnative` | 1.655 s / 1.736 s | 11.1x |
| `bfwasm` (Node WASI Preview 1) | 4.971 s / 5.047 s | 3.7x |
| `bfjs` (Node) | 3.993 s / 4.011 s | 4.6x |
| `bf2c` (MSVC `/O2`) | 1.449 s / 1.451 s | 12.7x |

Run `scripts/benchmark_all.py` to repeat the comparison. The script
reports compiler times separately and includes optional C and Node
lanes when those tools are available.

## Tests and CI

The test workflow builds and tests the workspace on six native runners:

| Target | GitHub Actions runner |
|---|---|
| `linux-x86_64` | `ubuntu-latest` |
| `linux-aarch64` | `ubuntu-26.04-arm` |
| `macos-x86_64` | `macos-26-intel` |
| `macos-aarch64` | `macos-latest` |
| `windows-x86_64` | `windows-latest` |
| `windows-aarch64` | `windows-11-arm` |

Each job runs formatting and Clippy checks, `cargo +nightly llvm-cov`,
and the differential harness. Coverage is required to reach 100% for
functions, lines, regions, and branches on every target.

The harness compares backend stdout, stderr, and exit status with
`bfrun` over the Brainfuck corpus. It checks the native compiler, JIT,
interpreter, C output, WASI, and JavaScript. Node lanes run on versions
22, 24, and 26; the Node 22 and 24 jobs run only those lanes. The Node 26
job also tests browser output in Chromium, Firefox, and WebKit, including
the split Wasm loader. CI requires the runtimes and compilers, so missing
test dependencies fail the job instead of silently skipping a lane.

## Limits

- `bfnative` supports only the six targets listed above. `bfjit` can run
  only on the host where it is launched.
- The optimizer folds only loops it can prove safe. Other loops remain
  ordinary loops and keep their original runtime behavior.
- `bfwasm`'s split browser output fetches a neighboring `.wasm` file and
  needs an HTTP server. The default browser output embeds the module and
  works without that request.
- A WASI runtime controls how traps are reported. Wasm memory-growth
  failures and failed WASI calls can trap rather than print the usual
  Brainfuck runtime error.
- The debugger runs raw source instructions; it does not debug optimized
  bytecode or generated machine code.
