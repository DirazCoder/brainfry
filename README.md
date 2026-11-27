# brainfry

Brainfuck has eight instructions and no specification anyone agrees on. Does `,` at EOF leave the cell alone? Do cells wrap? What happens left of zero? The answer to all three is "depends who you ask," which is also the answer to tabs versus spaces.

Brainfry picks an answer for each, writes it down, and makes seven independent backends prove they agree with it. They don't, on the first try. That's what the test suite is for.

The workspace has a compiler, a bytecode format with its own VM, a JIT, a native compiler, a WebAssembly compiler, JavaScript and C transpilers, and an interactive debugger. That's a lot of infrastructure for a language whose name is an instruction. Pick whichever fits where the program has to run:

| Tool | Use it when |
|---|---|
| `bfinterp` | You want a direct, easy-to-audit reference run. Slow, but it has never lied to you. |
| `bfc` + `bfrun` | You want a reusable `.bfry` bytecode file and a VM to run it. |
| `bfjit` | You want native speed on this machine without saving an executable. |
| `bfnative` | You want a standalone executable for one of six OS/CPU targets. |
| `bfwasm` | You want a WASI module or a browser loader/bundle. |
| `bfjs` | You want standalone JavaScript with no WASI shim. |
| `bf2c` | You want C source you can compile with your own toolchain. |
| `bfdbg` | You want to inspect and change program state while it runs. `printf` debugging, with dignity. |
| `brainfry.wasm` | You want the command-line tools together in one WASI module. `bfjit` stays native. |

## Build

Install Rust, then:

```sh
cargo build --release
```

Binaries land in `target/release`. Cargo and your host linker are all the build needs. A C compiler, Node.js, or a WASI runtime only matters if you choose the generated C, JavaScript, or WASI output. If the build passes on the first try, that's fine. Just be a little suspicious.

## One WASI build of the toolset

The combined build puts the CLI tools and their shared code in one module:

```sh
rustup target add wasm32-wasip1
cargo build --release --target wasm32-wasip1 -p brainfry
```

You still need a WASI Preview 1 runtime such as Wasmtime to execute it. The
runtime also controls filesystem access, so preopen the directory containing
your source and output files:

```sh
wasmtime run --dir . target/wasm32-wasip1/release/brainfry.wasm bfc program.bf
wasmtime run --dir . target/wasm32-wasip1/release/brainfry.wasm bfrun program.bfry
```

Each command is a subcommand of the same module. `bfc` writes a `.bfry`
file, then `bfrun` reads it from the preopened directory; neither command
loads another `.wasm` file or starts a subprocess. The module also exposes
`bfnative`, `bfinterp`, `bfwasm`, `bfjs`, `bf2c`, and `bfdbg`. `bfjit` is
not included: it executes native machine code through OS memory APIs.

The WASI version of `bfnative` requires an explicit native target because
WASI itself isn't one of the targets it can emit:

```sh
wasmtime run --dir . target/wasm32-wasip1/release/brainfry.wasm bfnative --target linux-x86_64 program.bf
```

That flag selects the executable `bfnative` writes; it doesn't change the
architecture of `brainfry.wasm`. The generated executable still needs a
matching native OS and CPU to run.

## Run a program

### Bytecode

```sh
bfc program.bf                 # writes program.bfry
bfc program.bf output.bfry     # choose a different output path
bfrun program.bfry
```

`bfc` parses and optimizes the source, then writes bytecode. `bfrun` loads the `.bfry` file and executes it. It's the middle child: faster than the interpreter, slower than everything that got a compiler.

### Interpreter

```sh
bfinterp program.bf
bfinterp --optimize program.bf
```

By default `bfinterp` runs the raw parsed instructions, with no optimizer. That's on purpose. When a backend that consumes optimized IR misbehaves, the unoptimized run is your control. If the clever one and the simple one disagree, bet on the simple one. Add `--optimize` to use the shared optimizer instead.

### Native executable

```sh
bfnative program.bf
bfnative --target windows-x86_64 program.bf -o program.exe
bfnative --emit-asm program.bf
```

With no target, `bfnative` builds for the host. The supported targets:

```text
linux-x86_64       linux-aarch64
macos-x86_64       macos-aarch64
windows-x86_64     windows-aarch64
```

It generates machine code and the executable container itself. No assembler, no linker. Fewer tools means fewer places to blame.

Naming things is hard, and `--emit-asm` is the proof: it writes an annotated `.lst` listing of the machine code, not assembly. `--cc` and `--keep-asm` are deprecated and ignored. Your old scripts will keep running and keep wondering why nothing changed.

Output is for the target you picked. A cross-compiled executable won't run on the build host unless the host matches. It's "works on my machine" in reverse: it works on the machine you aren't using.

### JIT

```sh
bfjit program.bf
```

`bfjit` parses, optimizes, and compiles in memory, then runs the result on the current machine. No target option and no file written, so there's nothing to clean up and nothing to blame. If you need an artifact to copy to another machine, use `bfnative`.

### WebAssembly

By default `bfwasm` writes a standalone WASI Preview 1 module:

```sh
bfwasm program.bf
bfwasm program.bf -o output.wasm
wasmtime run output.wasm
```

For browser output, ask for it explicitly or give an output path ending in `.js`:

```sh
bfwasm program.bf --target browser
bfwasm program.bf -o program.js
```

The default browser output is a single JavaScript file. It embeds the compiled Wasm bytes and a small `wasi_snapshot_preview1` shim, makes no fetch request, and needs no sidecar file. Paste it into a console or drop it in a page, including one opened from `file://`. Zero dependencies, which in the JavaScript ecosystem counts as a personality.

If you'd rather keep the module separate, add `--split`:

```sh
bfwasm program.bf --target browser --split
bfwasm program.bf -o dist/app.js --split
```

That writes a JavaScript ES module plus an adjacent `.wasm` file with the same stem. The loader finds the module relative to itself and tries streaming instantiation before falling back to `arrayBuffer`. It fetches the Wasm file, so you have to serve both over HTTP. Opening the loader from `file://` won't work, and the error message won't be much help. `--split` only applies in browser mode, and an explicit `--target wasi` beats `.js` extension inference.

Both browser outputs read optional I/O hooks from `globalThis.__bfwasm`. Set them before the generated script runs:

```js
globalThis.__bfwasm = {
  stdin: "input bytes", // string or Uint8Array
  onStdout: chunk => console.log(chunk),
  onStderr: chunk => console.error(chunk),
};
```

No `stdin` means input is EOF. No output hooks means stdout and stderr are buffered and printed to the console when the module exits. Hooks get live `Uint8Array` chunks, and the final status lands in `globalThis.__bfwasmExitCode`. A Wasm trap isn't a normal Brainfuck runtime error. The browser shim reports it to the console and records status 134, a number with a long history of meaning "something went wrong."

### JavaScript

```sh
bfjs program.bf
node program.js
```

I love JavaScript, so of course there's a JavaScript converter. Check the profile description.

`bfjs` writes a standalone `.js` file with a `Uint8Array` tape and its own runtime code. It imports nothing from Brainfry, so there's no `node_modules`. Imagine that. It runs under Node, Bun, Deno, or a browser console. Node and Bun write bytes through their file APIs, Deno uses its stdout API, and browsers collect output for the console. Browser input is always EOF. Honestly, relatable.

### C

```sh
bf2c program.bf
cc -O2 -o program program.c
./program
```

`bf2c` writes C99 source, so you need a C compiler to get an executable. It's C, not C++. Nothing personal, C++, I'm just low-mid on you. The generated code handles tape growth, input, and output. On Windows it switches the standard streams to binary mode so I/O stays byte-oriented. Otherwise Windows would "helpfully" translate your newlines, and your bytes would stop being your bytes.

## Debug a program

```sh
bfdbg program.bf
bfdbg --stdin input.txt --break @3:5 program.bf
```

`bfdbg` reads raw source and steps one instruction at a time, which ties every step to a source line and column. The catch: it can't load `.bfry` files or optimized IR. It debugs the program you wrote, not the one the optimizer wished you'd written.

Type `help` inside a session for the full list. The ones you'll use most:

| Command | What it does |
|---|---|
| `run` / `continue` | Run until a breakpoint, watchpoint, error, or program end. |
| `step [n]` | Execute one instruction, or `n` of them. |
| `back [n]` | Rewind by that many instructions. Ctrl+Z for execution. |
| `break 42` / `break @3:5` | Stop at an instruction index or a source location. |
| `watch ptr 10` | Stop when the pointer reaches cell 10. |
| `watch cell 10` | Stop when cell 10 changes. Finally, an answer to "who touched this?" |
| `set cell[10] = 65` / `set ptr = 3` | Edit a cell or move the pointer. It's called debugging, not cheating. |
| `feed 65 66` | Queue input bytes for `,`. |
| `tape` / `list` / `info` | Inspect the tape, the source, or debugger state. |

Program output goes to stdout and debugger messages go to stderr, so scripted sessions get clean bytes. The undo log holds 1,048,576 steps by default, which is a long memory for something that mostly exists to undo mistakes. `--undo-limit` changes that, and `0` turns history off, which also makes `back` unavailable.

## Shared behavior and optimization

Every execution backend follows the same tape rules:

- The tape starts as 30,000 zeroed cells and grows to the right as needed.
- Cells are 8-bit. Addition and subtraction wrap modulo 256, so 255 + 1 is 0. That's documented behavior, which makes it a feature.
- Input at EOF leaves the current cell unchanged.
- Moving left of cell 0 is an error. Off-by-one errors are the one kind of bug that made it into the spec.
- Growth is capped at 1 GiB, and allocation failures are runtime errors too. That's effectively infinite for any program you'd want to run, and finite for the one you just accidentally wrote.

`bfrun`, `bfinterp`, `bfjit`, `bfnative`, `bfwasm`, `bfjs`, and `bf2c` all use the parser and optimizer in `bfc`. Their execution engines and code generators are separate, which is the point: seven independent implementations that have to agree. `bfdbg` uses the parser but keeps raw instructions so source positions stay meaningful.

The optimizer runs until nothing changes, which is more than you can say for most refactors. Its passes fold repeated instructions, zeroing loops, simple scans, and eligible linear loops. They also turn pointer movement into fixed-offset operations, merge constant stores, and drop operations whose results are already known. Jump targets get recalculated after the rewrites.

Linear-loop folding covers loops like `[->+<]` and some with other odd counter steps. The counter has to come back to the same pointer position, and the body has to be plain additive updates and moves. I/O, nested loops, even counter steps, and pointer excursions that don't write every visited cell all block the fold. That last rule exists to preserve underflow errors. A loop that visits cell -1 can fail even if it never changes that cell. An optimizer that hides that failure isn't helping, it's just scheduling the incident for 3 a.m.

Pointer-window fusion tracks the minimum and maximum positions the original moves would have visited. It emits a bounds check before swapping those moves for offset operations, so an optimization can't mask an underflow or skip a needed tape growth. If the optimizer can't keep those checks intact, it leaves the path alone. Knowing when not to be clever is rarer than it should be.

A `.bfry` file starts with the `BFRY` magic and format version 2. `bfrun` checks the header and validates jump pairs before running anything. The bytecode holds the optimized instructions with resolved jump targets. It isn't a portable executable, so please don't ship it to anyone and call it a binary.

## Benchmarks

These numbers come from `mandelbrot.b` on Windows 11 Pro x64, an AMD Ryzen 7 4700U, Node.js 26.13.1, and MSVC `cl /O2`. Every backend matched the 6,240-byte reference output. The table shows three runs after one warm-up. It's a snapshot of one laptop, not a ranking. Your results will vary, as will your thermal throttling.

Ahead-of-time compile time is excluded. `bfinterp` includes parsing and execution, `bfjit` includes its compile step, and Node startup is counted for `bfjs` and `bfwasm`.

| Tool | Minimum / median | Faster than `bfinterp` |
|---|---:|---:|
| `bfinterp` | 18.334 s / 18.510 s | 1.0x |
| `bfrun` | 8.415 s / 8.532 s | 2.2x |
| `bfjit` | 1.617 s / 1.659 s | 11.3x |
| `bfnative` | 1.655 s / 1.736 s | 11.1x |
| `bfwasm` (Node WASI Preview 1) | 4.971 s / 5.047 s | 3.7x |
| `bfjs` (Node) | 3.993 s / 4.011 s | 4.6x |
| `bf2c` (MSVC `/O2`) | 1.449 s / 1.451 s | 12.7x |

The reference interpreter takes eighteen seconds to draw a Mandelbrot set, so you'll have time to wonder whether it hung. It didn't. The winner is C, which will surprise nobody who has argued about this online.

To repeat the comparison, run `scripts/benchmark_all.py`. It reports compiler times separately and adds the C and Node lanes if those tools are installed.

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

Each job runs formatting and Clippy checks, `cargo +nightly llvm-cov`, and the differential harness. Coverage has to hit 100% for functions, lines, regions, and branches on every target. We know what coverage is worth as a metric. We did it anyway.

The harness runs the Brainfuck corpus through each backend and compares stdout, stderr, and exit status against `bfrun`. That covers the native compiler, JIT, interpreter, C output, WASI, and JavaScript. Node lanes run on versions 22, 24, and 26. The 22 and 24 jobs run only those lanes. The Node 26 job also tests browser output in Chromium, Firefox, and WebKit, split Wasm loader included. A separate Wasmtime job builds and exercises the combined `brainfry.wasm` commands, including file handoff between `bfc` and `bfrun`. The WASI artifact is behavior-tested; the 100% Rust coverage gate is measured by `cargo llvm-cov` on the six native targets. If a runtime or compiler is missing, the job fails. A silently skipped lane is just a green checkmark that lies by omission.

## Limits

- `bfnative` supports only the six targets above. `bfjit` runs only on the machine you launch it from, which is at least the machine where it works.
- The optimizer folds a loop only when it can prove the fold is safe. Everything else stays an ordinary loop with its original runtime behavior.
- Split browser output fetches a neighboring `.wasm` file, so it needs an HTTP server. The default browser output embeds the module and doesn't.
- Your WASI runtime decides how traps get reported. Wasm memory-growth failures and failed WASI calls can trap instead of printing the usual Brainfuck runtime error.
- The debugger works on raw source instructions. It can't debug optimized bytecode or generated machine code. If your bug only shows up after optimization, you get to enjoy it without tooling.

## FAQ that i asked, not you

### Why do I even exist?

Brainfuck's behavior is mostly rumor. Does `,` at EOF leave the cell alone? Do cells wrap? Does the pointer survive going left of zero? Brainfry picks an answer for each, writes it down, and checks that every backend agrees. Nobody asked for this. It's done anyway.

### Why is there a JavaScript converter?

Because I love JavaScript. Read the profile description. It also happens to be useful: `bfjs` produces a standalone file that runs under Node, Bun, Deno, or a browser console with no WASI shim, so there's a practical reason too. But mostly the profile description.

### Why am I like this? Why seven backends for a language with eight instructions?

One interpreter tells you what a program does. Several independent ones tell you whether you can trust that. `bfrun`, `bfinterp`, `bfjit`, `bfnative`, `bfwasm`, `bfjs`, and `bf2c` share a parser and optimizer, but each has its own execution engine or code generator. When they agree, it means something. When they don't, there's a bug to find.

### Why do I need lots of execution modes for Brainfuck?

I have no idea. Next.

### Why is `--emit-asm` not emitting assembly?

The name is a leftover. It writes an annotated `.lst` listing of the generated machine code, not assembly. `--cc` and `--keep-asm` are deprecated and ignored, so old scripts keep running without doing anything. I've made peace with this.

### Why does `bfnative` skip the assembler and the linker?

It writes the machine code and the executable container itself, so it needs no external toolchain. Yes, that's more work. No, nobody stopped me.

### Why won't `bfinterp` optimize unless I ask?

Somebody has to be the control group. The default run uses the raw parsed instructions, so when a backend that consumes optimized IR misbehaves, you have an unoptimized run to compare against. `--optimize` is there when you want it.

### Why does the optimizer leave some loops alone?

It folds a loop only when it can prove the fold is safe. I/O, nested loops, even counter steps, and pointer excursions that don't write every visited cell all block it. That last one preserves underflow errors, because a loop that visits cell -1 can fail even if it never changes that cell. A loop that stays a loop is slower. A bug that disappears after optimization is worse.

### Why can't the debugger read my `.bfry` file?

`bfdbg` works on raw source, one instruction at a time, so every step maps to a line and column. Optimized bytecode has lost that mapping. It would rather show you your program than a rewritten one.

### Why does input at EOF leave the cell alone?

Someone had to pick, and that was the pick. What matters is that every backend picks the same thing, and the differential harness checks that they do.

### Why does the split browser build need a server?

It fetches a neighboring `.wasm` file, and `file://` pages can't do that. The default browser output embeds the module and needs no server. If you only want to double-click something, use that one.

### Why does CI demand 100% coverage on six platforms?

Because the project requires it: functions, lines, regions, and branches, on every target. If a runtime or compiler is missing, the job fails instead of skipping the lane. A skipped test is a green checkmark that lies by omission.
