#!/usr/bin/env python3
"""Differential test harness: bfnative (native binary) vs bfrun (reference VM).

For every program in the corpus this script:
  1. compiles the .bf source with `bfc` and runs it on `bfrun` (ground truth),
  2. compiles the same source with `bfnative` (pure-Rust machine-code backend,
     executed on the host — linux-x86_64 here) and runs the produced ELF,
  3. compares stdout, stderr, and exit status byte-for-byte.

The corpus deliberately covers: deep nesting, multiplication loops (which the
optimizer does NOT fold), tape growth far past 30,000 cells, 8-bit wraparound,
EOF-on-input semantics, IO interleaving, and the two hard runtime-error paths
(left-of-cell-0 underflow, both immediate and after a long right walk).

Usage:
    python3 scripts/differential_test.py [path-to-repo-root]
        — linux-x86_64 binaries, run natively on the host.

    QEMU_AARCH64=/path/to/qemu-aarch64-static \
        python3 scripts/differential_test.py [repo-root]
        — also builds linux-aarch64 binaries and runs them under QEMU
          user-mode emulation, comparing against the same bfrun ground
          truth. Set QEMU_AARCH64 to "skip" to disable.
"""

import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(HERE)

BFC = os.path.join(ROOT, "target", "release", "bfc")
BFRUN = os.path.join(ROOT, "target", "release", "bfrun")
BFNATIVE = os.path.join(ROOT, "target", "release", "bfnative")
QEMU_AARCH64 = os.environ.get("QEMU_AARCH64", "skip")

# (name, source, stdin)
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
    # Runtime-error cases: the native binary must reproduce bfrun's message
    # and exit status exactly.
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
]


def run_ref(source: str, stdin: str):
    """bfc -> bfrun. Returns (exit, stdout, stderr)."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        bfry = os.path.join(td, "p.bfry")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BFC, src, bfry], capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        r = subprocess.run([BFRUN, bfry], input=stdin.encode(),
                           capture_output=True)
        return r.returncode, r.stdout, r.stderr


def run_native(source: str, stdin: str, target="linux-x86_64"):
    """bfnative -> execute. Returns (exit, stdout, stderr).

    target linux-x86_64 executes natively; linux-aarch64 executes under the
    QEMU_AARCH64 user-mode emulator.
    """
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        exe = os.path.join(td, "p")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BFNATIVE, "--target", target, src, "-o", exe],
                           capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        if target == "linux-aarch64":
            cmd = [QEMU_AARCH64, exe]
        else:
            cmd = [exe]
        r = subprocess.run(cmd, input=stdin.encode(), capture_output=True)
        return r.returncode, r.stdout, r.stderr


def main() -> int:
    for tool in (BFC, BFRUN, BFNATIVE):
        if not os.path.exists(tool):
            print(f"missing {tool} — run `cargo build --release` first")
            return 1

    targets = ["linux-x86_64"]
    if QEMU_AARCH64 != "skip" and os.path.exists(QEMU_AARCH64):
        targets.append("linux-aarch64")
        print(f"also testing linux-aarch64 under {QEMU_AARCH64}\n")

    failures = 0
    for target in targets:
        if target != "linux-x86_64":
            print(f"--- {target} ---")
        for name, source, stdin in CORPUS:
            ref = run_ref(source, stdin)
            nat = run_native(source, stdin, target)
            ok = ref == nat
            status = "ok " if ok else "FAIL"
            print(f"[{status}] {name}: exit {ref[0]}/{nat[0]}, "
                  f"stdout {len(ref[1])}B/{len(nat[1])}B")
            if not ok:
                failures += 1
                print(f"       ref : exit={ref[0]} stdout={ref[1]!r} "
                      f"stderr={ref[2]!r}")
                print(f"       native: exit={nat[0]} stdout={nat[1]!r} "
                      f"stderr={nat[2]!r}")

    total = len(CORPUS) * len(targets)
    print(f"\n{total - failures}/{total} executions match bfrun exactly "
          f"(stdout, stderr, exit status)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
