#!/usr/bin/env python3
"""Five-way differential test harness: bfinterp vs bfrun vs bfnative vs bfjit
vs bfwasm.

Every program in the corpus is run through five execution strategies fed by
one shared parser/optimizer front end:

  1. `bfinterp`         — naive raw-source interpreter (default, no optimizer)
  2. `bfinterp --optimize` — same interpreter, optimizer enabled (bonus lane)
  3. `bfrun`            — bytecode VM (via bfc; the project's ground truth)
  4. `bfnative`         — ahead-of-time native executable
  5. `bfjit`            — JIT: same codegen as bfnative, executed in memory
  6. `bfwasm`           — compiles to a standalone .wasm, run under wasmtime

bfwasm is a soft dependency: if the `wasmtime` Python package isn't
installed, that lane is skipped (with a note) rather than failing the run,
since it's an external runtime rather than something `cargo build` produces.

stdout, stderr, and exit status must be byte-identical across all of them.
The corpus is the 25-program set from differential_test.py (deep nesting,
multiplication loops, tape growth to 200k cells, wraparound, EOF semantics,
stdin echo, runtime-error paths) plus Erik Bosman's mandelbrot.b — the same
real workload used for the native-vs-bytecode benchmark.

If bfinterp's naive path disagrees with the others, that is a bug in
bfinterp specifically: it is the newest, least-optimized implementation.

Usage:
    python3 scripts/differential_test_all.py [repo-root]

Optional aarch64-under-QEMU mode (execution test of the arm64 JIT under
user-mode emulation, mirroring the bfnative harness's QEMU_AARCH64
convention):

    QEMU_AARCH64=/path/to/qemu-aarch64-static \\
    BFJIT_AARCH64=target/aarch64-unknown-linux-gnu/release/bfjit \\
    BFNATIVE_AARCH64=target/aarch64-unknown-linux-gnu/release/bfnative \\
    BFRUN_AARCH64=target/aarch64-unknown-linux-gnu/release/bfrun \\
        python3 scripts/differential_test_all.py [repo-root]

Each provided aarch64 binary is run under the emulator and compared against
the same x86_64 bfrun ground truth (Brainfuck I/O is byte-level, so outputs
are architecture-independent). Provide any subset.
"""

import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(HERE)

EXE = ".exe" if os.name == "nt" else ""

BFC = os.path.join(ROOT, "target", "release", "bfc" + EXE)
BFRUN = os.path.join(ROOT, "target", "release", "bfrun" + EXE)
BFNATIVE = os.path.join(ROOT, "target", "release", "bfnative" + EXE)
BFINTERP = os.path.join(ROOT, "target", "release", "bfinterp" + EXE)
BFJIT = os.path.join(ROOT, "target", "release", "bfjit" + EXE)
BFWASM = os.path.join(ROOT, "target", "release", "bfwasm" + EXE)
MANDELBROT = os.path.join(ROOT, "tests", "mandelbrot.b")

try:
    import wasmtime
    HAVE_WASMTIME = True
except ImportError:
    HAVE_WASMTIME = False

QEMU_AARCH64 = os.environ.get("QEMU_AARCH64", "")
BFJIT_AARCH64 = os.environ.get("BFJIT_AARCH64", "")
BFNATIVE_AARCH64 = os.environ.get("BFNATIVE_AARCH64", "")
BFRUN_AARCH64 = os.environ.get("BFRUN_AARCH64", "")

# (name, source, stdin) — the 25-program corpus from differential_test.py,
# plus mandelbrot.
CORPUS = [
    ("hello-world",
     "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---."
     "+++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.",
     ""),
    ("echo",
     ",[.[-],]",
     "echo, echo, echo\n"),
    ("eof-unchanged",
     ",+.",
     ""),
    ("eof-byte",
     ",.",
     "AB"),
    ("eof-inside-loop",
     ",[.[-],]",
     "hi!"),
    ("wrap-256",
     "+" * 256 + ".",
     ""),
    ("wrap-negative",
     "-.",
     ""),
    ("wrap-254",
     "--.",
     ""),
    ("deep-nesting-six",
     "++++++[>+++++[>++++[>+++[>++[>++[>+<-]<-]<-]<-]<-]<-]>>>>>>.",
     ""),
    ("multiply-copy-loop",
     "++++++[>++++++<-]>[<+>-]<+.",
     ""),
    ("multiply-loop-unfolded",
     "+++[>+++[>+<-]<-]>>.",  # 3*3=9 in cell 2
     ""),
    ("squares",
     "++++[>+++++<-]>.",  # 4*5 = 20
     ""),
    ("io-interleaved",
     ",.,.,.",
     "abcxyz"),
    ("io-sum",
     ",>,[<+>-]<.",  # add two bytes
     "\x0a\x0b"),
    ("tape-40k",
     ">" * 40_000 + "+.",
     ""),
    ("tape-200k",
     ">" * 200_000 + "+.",
     ""),
    ("tape-write-then-read-back",
     ">" * 65_600 + "+++" + "<" * 65_600 + ".",  # crosses a 64 KiB commit
     ""),
    ("zero-idiom",
     "++++++++++[-]+.",
     ""),
    ("zero-idiom-nested",
     "++[+++[-]+[-]]++.",
     ""),
    ("nested-zero-in-loop",
     ",[.[-],]",
     "roundtrip"),
    ("many-small-loops",
     "+[>+[>+<-]<-]>",  # idiom stress
     ""),
    # Loops folded into Scan/MulAdd lose their own zero check, so these
    # pin down that the folded forms still skip a zero cell entirely
    # (no move, no bounds check) rather than stepping once first.
    ("scan-left-on-zero-cell",
     "[<]+.",
     ""),
    ("scan-right-on-zero-cell",
     "[>]+.",
     ""),
    ("scan-stops-on-zero",
     "+>+>+<<[>]+.",
     ""),
    ("fold-negative-offset-zero-source",
     "[-<+>]+.",
     ""),
    ("fold-negative-offset-nonzero-source",
     ">+++[-<+>]<.",
     ""),
    # Runtime-error cases: every tool must reproduce bfrun's message and
    # exit status exactly.
    ("underflow-immediate",
     "<",
     ""),
    ("underflow-inside-loop",
     "+[<]",
     ""),
    ("underflow-after-walk",
     ">" * 10 + "<" * 11,
     ""),
    ("huge-left",
     ">" * 10 + "<" * 100_000,
     ""),
    # The real workload — same program the native-vs-bytecode benchmark
    # used. Only added when the asset is present.
]


def load_mandelbrot():
    if os.path.exists(MANDELBROT):
        with open(MANDELBROT) as f:
            return [("mandelbrot", f.read(), "")]
    print(f"note: {MANDELBROT} not found — running without it")
    return []


def run_cmd(cmd, stdin, cwd_files):
    """Runs cmd with stdin bytes; returns (exit, stdout, stderr)."""
    try:
        r = subprocess.run(cmd, input=stdin.encode(), capture_output=True,
                           timeout=600)
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return -99, b"<timeout>", b""


def run_ref(source, stdin):
    """bfc -> bfrun. The ground truth."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        bfry = os.path.join(td, "p.bfry")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BFC, src, bfry], capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        return run_cmd([BFRUN, bfry], stdin, td)


def run_source_tool(binary, source, stdin, extra_args=()):
    """Runs a takes-.bf-source tool (bfinterp, bfjit)."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        with open(src, "w") as f:
            f.write(source)
        return run_cmd([binary, *extra_args, src], stdin, td)


def run_native(source, stdin):
    """bfnative -> execute the produced ELF."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        exe = os.path.join(td, "p")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BFNATIVE, src, "-o", exe], capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        return run_cmd([exe], stdin, td)


def run_wasm(source, stdin):
    """bfwasm -> execute the produced module under wasmtime's WASI preview1.

    stdin/stdout/stderr are backed by real files rather than pipes: the
    `wasmtime` embedding API takes WASI file paths, not in-memory streams.
    """
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        wasm_path = os.path.join(td, "p.wasm")
        stdin_path = os.path.join(td, "stdin")
        stdout_path = os.path.join(td, "stdout")
        stderr_path = os.path.join(td, "stderr")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BFWASM, src, "-o", wasm_path], capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr

        with open(stdin_path, "wb") as f:
            f.write(stdin.encode())

        engine = wasmtime.Engine()
        module = wasmtime.Module.from_file(engine, wasm_path)
        linker = wasmtime.Linker(engine)
        linker.define_wasi()

        wasi = wasmtime.WasiConfig()
        wasi.stdin_file = stdin_path
        wasi.stdout_file = stdout_path
        wasi.stderr_file = stderr_path

        store = wasmtime.Store(engine)
        store.set_wasi(wasi)
        instance = linker.instantiate(store, module)
        start = instance.exports(store)["_start"]
        try:
            start(store)
            exit_code = 0
        except wasmtime.ExitTrap as trap:
            exit_code = trap.code
        except wasmtime.Trap:
            # Any other trap (unreachable, out-of-bounds memory access, ...)
            # has no real exit code — 134 mirrors the SIGABRT a native
            # equivalent would raise, keeping it in the same ballpark as
            # bfnative's trap handling for comparison purposes.
            exit_code = 134

        with open(stdout_path, "rb") as f:
            out = f.read()
        with open(stderr_path, "rb") as f:
            err = f.read()
        return exit_code, out, err


def run_emulated(binary, emulator, source, stdin):
    """An aarch64 tool binary under qemu-aarch64-static."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        with open(src, "w") as f:
            f.write(source)
        return run_cmd([emulator, binary, src], stdin, td)


def run_emulated_bfnative(binary, emulator, source, stdin):
    """The aarch64 bfnative under qemu: compile, then execute the produced
    aarch64 ELF under the same emulator."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        exe = os.path.join(td, "p")
        with open(src, "w") as f:
            f.write(source)
        c = run_cmd([emulator, binary, src, "-o", exe], "", td)
        if c[0] != 0:
            return c
        return run_cmd([emulator, exe], stdin, td)


def run_emulated_bfrun(binary, emulator, source, stdin):
    """An aarch64 bfrun under qemu. .bfry bytecode is platform-independent,
    so the host bfc produces it."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        bfry = os.path.join(td, "p.bfry")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BFC, src, bfry], capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        return run_cmd([emulator, binary, bfry], stdin, td)


def main() -> int:
    tools = [BFC, BFRUN, BFNATIVE, BFINTERP, BFJIT, BFWASM]
    missing = [t for t in tools if not os.path.exists(t)]
    if missing:
        print(f"missing {missing[0]} — run `cargo build --release` first")
        return 1

    if not HAVE_WASMTIME:
        print("note: `wasmtime` Python package not installed — "
              "skipping the bfwasm lane (pip install wasmtime)\n")

    corpus = CORPUS + load_mandelbrot()

    aarch64_lanes = []
    if QEMU_AARCH64 and os.path.exists(QEMU_AARCH64):
        for env, path in [
            ("bfjit", BFJIT_AARCH64),
            ("bfnative", BFNATIVE_AARCH64),
            ("bfrun", BFRUN_AARCH64),
        ]:
            if path and os.path.exists(path):
                aarch64_lanes.append((env, path))
        if aarch64_lanes:
            print(f"also testing under {QEMU_AARCH64}: "
                  f"{[name for name, _ in aarch64_lanes]}\n")

    failures = 0
    lane_failures = 0
    comparisons = 0
    for name, source, stdin in corpus:
        ref = run_ref(source, stdin)
        results = [
            ("bfinterp", run_source_tool(BFINTERP, source, stdin)),
            ("bfinterp --optimize",
             run_source_tool(BFINTERP, source, stdin, ["--optimize"])),
            ("bfnative", run_native(source, stdin)),
            ("bfjit", run_source_tool(BFJIT, source, stdin)),
        ]
        if HAVE_WASMTIME:
            results.append(("bfwasm", run_wasm(source, stdin)))
        for lane, path in aarch64_lanes:
            if lane == "bfrun":
                results.append((f"{lane}(qemu)",
                                run_emulated_bfrun(path, QEMU_AARCH64,
                                                   source, stdin)))
            elif lane == "bfnative":
                results.append((f"{lane}(qemu)",
                                run_emulated_bfnative(path, QEMU_AARCH64,
                                                      source, stdin)))
            else:
                results.append((f"{lane}(qemu)",
                                run_emulated(path, QEMU_AARCH64,
                                             source, stdin)))

        ok = all(ref == r for _, r in results)
        comparisons += len(results)
        status = "ok " if ok else "FAIL"
        print(f"[{status}] {name}: exit {ref[0]}, stdout {len(ref[1])}B, "
              f"{len(results)} lanes")
        if not ok:
            failures += 1
            lane_failures += sum(1 for _, r in results if r != ref)
            print(f"       ref   : exit={ref[0]} stdout={ref[1]!r} "
                  f"stderr={ref[2]!r}")
            for label, r in results:
                if r != ref:
                    print(f"       {label:20s}: exit={r[0]} stdout={r[1]!r} "
                          f"stderr={r[2]!r}")

    print(f"\n{len(corpus) - failures}/{len(corpus)} programs: "
          f"{comparisons - lane_failures}/{comparisons} executions match bfrun exactly "
          f"(stdout, stderr, exit status)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
