#!/usr/bin/env python3
"""Benchmark: bfinterp vs bfrun vs bfjit vs bfnative on mandelbrot.b,
plus the two transpiler lanes (bfjs under node, bf2c compiled with cc)
when those host tools are present.

The same real workload the native-vs-bytecode benchmark used (Erik Bosman's
mandelbrot.b). Every lane produces byte-identical output (verified by the
differential harness); what differs is how fast.

What each lane measures, honestly:

  bfinterp      one command: parse + naive execution (slowest by design —
                it is the ground-truth reference, one op per character).
  bfrun         execution of the pre-compiled .bfry bytecode (the bfc
                compile step is measured separately and is single-digit
                milliseconds — it is not part of the run).
  bfnative      execution of the pre-built native ELF (the bfnative
                compile step is measured separately; same story).
  bfjit         one command: parse + optimize + codegen + mmap + patch +
                jump — the JIT compile is included in the run, which is
                the honest accounting for a JIT (it is milliseconds).

N runs each; the min and median are reported (min approximates the
noise-free number, median guards against one lucky run).

Optional emulated lanes (NOT comparable to native numbers — reported
separately at the bottom, only when the env vars are set):

    QEMU_AARCH64=... BFJIT_AARCH64=... BFNATIVE_AARCH64=... \\
        python3 scripts/benchmark_all.py [repo-root]
"""

import os
import statistics
import subprocess
import sys
import time
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(HERE)

BFC = os.path.join(ROOT, "target", "release", "bfc")
BFRUN = os.path.join(ROOT, "target", "release", "bfrun")
BFNATIVE = os.path.join(ROOT, "target", "release", "bfnative")
BFINTERP = os.path.join(ROOT, "target", "release", "bfinterp")
BFJIT = os.path.join(ROOT, "target", "release", "bfjit")
BFJS = os.path.join(ROOT, "target", "release", "bfjs")
BF2C = os.path.join(ROOT, "target", "release", "bf2c")
MANDELBROT = os.path.join(ROOT, "tests", "mandelbrot.b")
EXPECTED = os.path.join(ROOT, "tests", "mandelbrot.expected")


def _probe(cmd):
    try:
        return subprocess.run(cmd, capture_output=True,
                              timeout=60).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def find_host_node():
    return "node" if _probe(["node", "-e", "0"]) else None


def find_host_cc():
    for cc in ("cc", "gcc", "clang"):
        with tempfile.TemporaryDirectory() as td:
            src = os.path.join(td, "probe.c")
            with open(src, "w") as f:
                f.write("int main(void) { return 0; }\n")
            if _probe([cc, "-o", os.path.join(td, "probe"), src]):
                return cc
    return None


NODE = find_host_node()
CC = find_host_cc()

RUNS = int(os.environ.get("BENCH_RUNS", "3"))
WARMUP = 1

QEMU_AARCH64 = os.environ.get("QEMU_AARCH64", "")
BFJIT_AARCH64 = os.environ.get("BFJIT_AARCH64", "")
BFNATIVE_AARCH64 = os.environ.get("BFNATIVE_AARCH64", "")


def timed(cmd, stdin=b""):
    """Runs cmd once, returns (seconds, exit). Fails hard on nonzero exit."""
    t0 = time.perf_counter()
    r = subprocess.run(cmd, input=stdin, stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE)
    dt = time.perf_counter() - t0
    return dt, r.returncode, r.stdout


def bench(label, cmd, expect_stdout, stdin=b""):
    for _ in range(WARMUP):
        timed(cmd, stdin)
    times = []
    for _ in range(RUNS):
        dt, code, out = timed(cmd, stdin)
        assert code == 0, f"{label}: exit {code}"
        assert out == expect_stdout, f"{label}: output mismatch"
        times.append(dt)
    return min(times), statistics.median(times)


def main() -> int:
    for tool in [BFC, BFRUN, BFNATIVE, BFINTERP, BFJIT, BFJS, BF2C]:
        if not os.path.exists(tool):
            print(f"missing {tool} — run `cargo build --release` first")
            return 1
    if not os.path.exists(MANDELBROT):
        print(f"missing {MANDELBROT}")
        return 1
    with open(EXPECTED, "rb") as f:
        expected = f.read()

    if NODE is None:
        print("note: no `node` on PATH — skipping the bfjs lane\n")
    if CC is None:
        print("note: no C compiler on PATH — skipping the bf2c lane\n")

    # Prepare the AOT artifacts once (their compile steps are timed
    # separately, not inside the execution lanes).
    bfry = "/tmp/bench-mandel.bfry"
    native = "/tmp/bench-mandel-native"
    js = "/tmp/bench-mandel.js"
    csrc = "/tmp/bench-mandel.c"
    cbinary = "/tmp/bench-mandel-c"
    t_bfc, _, _ = timed([BFC, MANDELBROT, bfry])
    t_bfnative_compile, code, _ = timed([BFNATIVE, MANDELBROT, "-o", native])
    assert code == 0
    compile_ms = [("bfc -> .bfry", t_bfc * 1000),
                 ("bfnative -> native ELF", t_bfnative_compile * 1000)]
    if NODE is not None:
        t_bfjs, code, _ = timed([BFJS, MANDELBROT, "-o", js])
        assert code == 0
        compile_ms.append(("bfjs -> .js", t_bfjs * 1000))
    if CC is not None:
        t_bf2c, code, _ = timed([BF2C, MANDELBROT, "-o", csrc])
        assert code == 0
        t_cc, code, _ = timed([CC, "-O3", "-o", cbinary, csrc])
        assert code == 0
        compile_ms.append((f"bf2c -> .c ({CC} -O3 builds it)",
                           (t_bf2c + t_cc) * 1000))

    rows = []
    rows.append(("bfinterp (naive reference)",
                 bench("bfinterp", [BFINTERP, MANDELBROT], expected)))
    rows.append(("bfrun (bytecode VM)",
                 bench("bfrun", [BFRUN, bfry], expected)))
    rows.append(("bfjit (in-memory JIT)",
                 bench("bfjit", [BFJIT, MANDELBROT], expected)))
    rows.append(("bfnative (AOT ELF)",
                 bench("bfnative", [native], expected)))
    if NODE is not None:
        rows.append(("bfjs (node, host JIT)",
                     bench("bfjs", [NODE, js], expected)))
    if CC is not None:
        rows.append(("bf2c (cc -O3 binary)",
                     bench("bf2c", [cbinary], expected)))

    print(f"mandelbrot.b — {RUNS} runs each (min / median), output "
          f"{len(expected)} bytes, byte-identical across all lanes\n")
    base_min = rows[0][1][0]
    for label, (mn, med) in rows:
        print(f"  {label:28s} {mn:8.3f}s / {med:8.3f}s   "
              f"({base_min / mn:6.1f}x faster than bfinterp)")

    print(f"\n  compile steps (not in the execution numbers above):")
    for label, ms in compile_ms:
        print(f"    {label:34s} {ms:7.1f} ms")
    print(f"    (bfjit's compile is included in its run: parse + optimize +")
    print(f"     codegen + mmap + patch, single-digit milliseconds)")

    # ---- optional emulated aarch64 lanes ----
    emu_rows = []
    if QEMU_AARCH64 and os.path.exists(QEMU_AARCH64):
        if BFJIT_AARCH64 and os.path.exists(BFJIT_AARCH64):
            emu_rows.append(("bfjit aarch64 (qemu)",
                             bench("bfjit-a64",
                                   [QEMU_AARCH64, BFJIT_AARCH64, MANDELBROT],
                                   expected)))
        if BFNATIVE_AARCH64 and os.path.exists(BFNATIVE_AARCH64):
            native64 = "/tmp/bench-mandel-native-a64"
            t, code, _ = timed([QEMU_AARCH64, BFNATIVE_AARCH64, MANDELBROT,
                                "-o", native64])
            assert code == 0
            emu_rows.append(("bfnative aarch64 (qemu)",
                             bench("bfnative-a64",
                                   [QEMU_AARCH64, native64], expected)))
    if emu_rows:
        print("\n  under qemu-aarch64 emulation (NOT comparable to the "
              "native numbers):")
        for label, (mn, med) in emu_rows:
            print(f"  {label:28s} {mn:8.3f}s / {med:8.3f}s")

    return 0


if __name__ == "__main__":
    sys.exit(main())
