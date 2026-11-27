#!/usr/bin/env python3
"""Benchmark Brainfry execution backends on mandelbrot.b.

The same real workload the native-vs-bytecode benchmark used (Erik Bosman's
mandelbrot.b). Every lane matches the reference output before timing;
Windows CRLF output is normalized for comparison.

What each lane measures, honestly:

  bfinterp      one command: parse + naive execution (slowest by design —
                it is the ground-truth reference, one op per character).
  bfrun         execution of the pre-compiled .bfry bytecode (the bfc
                compile step is measured separately and is single-digit
                milliseconds — it is not part of the run).
  bfnative      execution of the pre-built native executable (the bfnative
                compile step is measured separately; same story).
  bfjit         one command: parse + optimize + codegen + mmap + patch +
                jump — the JIT compile is included in the run, which is
                the honest accounting for a JIT (it is milliseconds).
  bfwasm        execution of the pre-compiled .wasm through Node's built-in
                WASI preview1 API.
  bfjs          execution of the generated JavaScript under Node.
  bf2c          execution of generated C compiled with cc -O3 or cl /O2.

N runs each; the min and median are reported (min approximates the
noise-free number, median guards against one lucky run).

Optional emulated lanes (NOT comparable to native numbers — reported
separately at the bottom, only when the env vars are set):

    QEMU_AARCH64=... BFJIT_AARCH64=... BFNATIVE_AARCH64=... \
        python3 scripts/benchmark_all.py [repo-root]
"""

from __future__ import annotations

import os
import shutil
import stat
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

IS_WINDOWS = os.name == "nt"
EXE_SUFFIX = ".exe" if IS_WINDOWS else ""

HERE = Path(__file__).resolve().parent
ROOT = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else HERE.parent

TARGET_DIR = ROOT / "target" / "release"
TOOLS = {
    name: TARGET_DIR / f"{name}{EXE_SUFFIX}"
    for name in ("bfc", "bfrun", "bfnative", "bfinterp", "bfjit", "bfjs", "bf2c", "bfwasm")
}

MANDELBROT = ROOT / "tests" / "mandelbrot.b"
EXPECTED = ROOT / "tests" / "mandelbrot.expected"

WASI_RUNNER_JS = """\
const fs = require("node:fs");
const { WASI } = require("node:wasi");
const wasi = new WASI({
  version: "preview1",
  args: [],
  env: {},
  preopens: {},
  returnOnExit: true,
});
WebAssembly.instantiate(fs.readFileSync(process.argv[2]), {
  wasi_snapshot_preview1: wasi.wasiImport,
})
  .then(({ instance }) => {
    process.exitCode = wasi.start(instance);
  })
  .catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
"""


def _probe(cmd: list[str], timeout: float = 30.0) -> bool:
    """Safely check if a command executes and returns exit code 0."""
    try:
        res = subprocess.run(
            cmd,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=timeout,
            check=False,
        )
        return res.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def find_host_node() -> tuple[str | None, bool]:
    """Return (node_executable_path, supports_wasi)."""
    node_bin = shutil.which("node")
    if not node_bin:
        return None, False
    if not _probe([node_bin, "-e", "process.exit(0)"]):
        return None, False
    has_wasi = _probe([node_bin, "-e", 'require("node:wasi")'])
    return node_bin, has_wasi


def find_host_cc() -> tuple[str, str] | None:
    """Return (compiler_executable_path, compiler_family) where family is 'cl' or 'posix'."""
    candidates: list[tuple[str, str]] = []
    if IS_WINDOWS:
        candidates.extend([("cl", "cl"), ("clang", "posix"), ("gcc", "posix"), ("cc", "posix")])
    else:
        candidates.extend([("cc", "posix"), ("gcc", "posix"), ("clang", "posix")])

    with tempfile.TemporaryDirectory(prefix="brainfry-cc-probe-") as td:
        tmp = Path(td)
        src = tmp / "probe.c"
        src.write_text("int main(void) { return 0; }\n", encoding="utf-8")

        for name, family in candidates:
            compiler_path = shutil.which(name)
            if not compiler_path:
                continue

            if family == "cl":
                obj = tmp / "probe.obj"
                exe = tmp / "probe_cl.exe"
                cmd = [compiler_path, "/nologo", f"/Fo{obj}", f"/Fe{exe}", str(src)]
            else:
                exe = tmp / f"probe_{name}{EXE_SUFFIX}"
                cmd = [compiler_path, "-o", str(exe), str(src)]

            if _probe(cmd) and exe.is_file():
                return compiler_path, family

    return None


def ensure_executable(path: Path) -> None:
    """Ensure POSIX executable permissions are set on compiled binaries."""
    if not IS_WINDOWS and path.exists():
        current_mode = path.stat().st_mode
        path.chmod(current_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def normalize_output(data: bytes) -> bytes:
    """Normalize CRLF to LF on Windows so byte comparisons remain cross-platform."""
    return data.replace(b"\r\n", b"\n") if IS_WINDOWS else data


def run_checked(label: str, cmd: list[str], stdin: bytes = b"") -> tuple[float, bytes]:
    """Run cmd once, measure wall time, and raise RuntimeError on non-zero exit."""
    t0 = time.perf_counter()
    try:
        proc = subprocess.run(
            cmd,
            input=stdin,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    except OSError as exc:
        raise RuntimeError(f"{label}: failed to execute {cmd!r}: {exc}") from exc

    dt = time.perf_counter() - t0
    if proc.returncode != 0:
        err_text = proc.stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(
            f"{label}: exited with code {proc.returncode}"
            + (f"\nstderr:\n{err_text}" if err_text else "")
        )
    return dt, proc.stdout


def bench(
    label: str,
    cmd: list[str],
    expect_stdout: bytes,
    runs: int,
    warmup: int = 1,
    stdin: bytes = b"",
) -> tuple[float, float]:
    """Warm up and benchmark a command across `runs` iterations."""
    for i in range(warmup):
        _, out = run_checked(f"{label} (warmup {i + 1})", cmd, stdin)
        if normalize_output(out) != expect_stdout:
            raise RuntimeError(
                f"{label}: output mismatch during warmup "
                f"(got {len(out)} bytes, expected {len(expect_stdout)} bytes)"
            )

    times: list[float] = []
    for i in range(runs):
        dt, out = run_checked(f"{label} (run {i + 1})", cmd, stdin)
        if normalize_output(out) != expect_stdout:
            raise RuntimeError(
                f"{label}: output mismatch on run {i + 1} "
                f"(got {len(out)} bytes, expected {len(expect_stdout)} bytes)"
            )
        times.append(dt)

    return min(times), statistics.median(times)


def parse_int_env(var_name: str, default: int, min_val: int = 1) -> int:
    raw = os.environ.get(var_name, "").strip()
    if not raw:
        return default
    try:
        val = int(raw)
        if val < min_val:
            raise ValueError
        return val
    except ValueError:
        raise RuntimeError(
            f"Environment variable {var_name} must be an integer >= {min_val}, got {raw!r}"
        )


def main() -> int:
    try:
        runs = parse_int_env("BENCH_RUNS", 3, min_val=1)
        warmup = parse_int_env("BENCH_WARMUP", 1, min_val=0)
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    missing_tools = [str(p) for p in TOOLS.values() if not p.is_file()]
    if missing_tools:
        print("error: missing release binaries — run `cargo build --release` first:", file=sys.stderr)
        for p in missing_tools:
            print(f"  - {p}", file=sys.stderr)
        return 1

    for required_file in (MANDELBROT, EXPECTED):
        if not required_file.is_file():
            print(f"error: missing required test file: {required_file}", file=sys.stderr)
            return 1

    expected = normalize_output(EXPECTED.read_bytes())
    node_bin, node_has_wasi = find_host_node()
    cc_info = find_host_cc()

    if node_bin is None:
        print("note: no `node` on PATH — skipping bfjs and bfwasm lanes\n")
    elif not node_has_wasi:
        print("note: `node` lacks `node:wasi` — skipping bfwasm lane\n")

    if cc_info is None:
        print("note: no C compiler (cc/gcc/clang/cl) on PATH — skipping bf2c lane\n")

    with tempfile.TemporaryDirectory(prefix="brainfry-benchmark-") as td:
        bench_dir = Path(td)

        bfry = bench_dir / "mandelbrot.bfry"
        native = bench_dir / f"mandelbrot-native{EXE_SUFFIX}"
        wasm = bench_dir / "mandelbrot.wasm"
        wasm_runner = bench_dir / "run-wasi.cjs"
        js = bench_dir / "mandelbrot.js"
        csrc = bench_dir / "mandelbrot.c"
        cobj = bench_dir / "mandelbrot.obj"
        cbinary = bench_dir / f"mandelbrot-c{EXE_SUFFIX}"

        compile_ms: list[tuple[str, float]] = []

        # 1. AOT Compilation Steps (timed separately)
        t_bfc, _ = run_checked("bfc compile", [str(TOOLS["bfc"]), str(MANDELBROT), str(bfry)])
        compile_ms.append(("bfc -> .bfry", t_bfc * 1000.0))

        t_bfnative, _ = run_checked(
            "bfnative compile", [str(TOOLS["bfnative"]), str(MANDELBROT), "-o", str(native)]
        )
        ensure_executable(native)
        compile_ms.append(("bfnative -> native executable", t_bfnative * 1000.0))

        t_bfwasm, _ = run_checked(
            "bfwasm compile", [str(TOOLS["bfwasm"]), str(MANDELBROT), "-o", str(wasm)]
        )
        compile_ms.append(("bfwasm -> .wasm", t_bfwasm * 1000.0))

        if node_bin is not None:
            t_bfjs, _ = run_checked(
                "bfjs compile", [str(TOOLS["bfjs"]), str(MANDELBROT), "-o", str(js)]
            )
            compile_ms.append(("bfjs -> .js", t_bfjs * 1000.0))

            if node_has_wasi:
                wasm_runner.write_text(WASI_RUNNER_JS, encoding="utf-8")

        cc_label_desc = ""
        if cc_info is not None:
            cc_path, cc_family = cc_info
            cc_name = Path(cc_path).stem
            t_bf2c, _ = run_checked(
                "bf2c codegen", [str(TOOLS["bf2c"]), str(MANDELBROT), "-o", str(csrc)]
            )
            if cc_family == "cl":
                cc_cmd = [cc_path, "/nologo", "/O2", f"/Fo{cobj}", f"/Fe{cbinary}", str(csrc)]
                cc_label_desc = "cl /O2"
            else:
                cc_cmd = [cc_path, "-O3", "-o", str(cbinary), str(csrc)]
                cc_label_desc = f"{cc_name} -O3"

            t_cc, _ = run_checked(f"C compile ({cc_label_desc})", cc_cmd)
            ensure_executable(cbinary)
            compile_ms.append((f"bf2c -> .c ({cc_label_desc} builds it)", (t_bf2c + t_cc) * 1000.0))

        # 2. Execution Benchmarks
        rows: list[tuple[str, tuple[float, float]]] = [
            (
                "bfinterp (naive reference)",
                bench("bfinterp", [str(TOOLS["bfinterp"]), str(MANDELBROT)], expected, runs, warmup),
            ),
            (
                "bfrun (bytecode VM)",
                bench("bfrun", [str(TOOLS["bfrun"]), str(bfry)], expected, runs, warmup),
            ),
            (
                "bfjit (in-memory JIT)",
                bench("bfjit", [str(TOOLS["bfjit"]), str(MANDELBROT)], expected, runs, warmup),
            ),
            (
                "bfnative (AOT native)",
                bench("bfnative", [str(native)], expected, runs, warmup),
            ),
        ]

        if node_bin is not None:
            if node_has_wasi:
                rows.append(
                    (
                        "bfwasm (Node WASI preview1)",
                        bench("bfwasm", [node_bin, str(wasm_runner), str(wasm)], expected, runs, warmup),
                    )
                )
            rows.append(
                (
                    "bfjs (Node host JIT)",
                    bench("bfjs", [node_bin, str(js)], expected, runs, warmup),
                )
            )

        if cc_info is not None:
            rows.append(
                (
                    f"bf2c ({cc_label_desc} binary)",
                    bench("bf2c", [str(cbinary)], expected, runs, warmup),
                )
            )

        print(
            f"mandelbrot.b — {runs} runs each (min / median), output "
            f"{len(expected)} reference bytes, output matched across all lanes\n"
        )
        base_min = rows[0][1][0]
        for label, (mn, med) in rows:
            speedup = (base_min / mn) if mn > 0 else float("inf")
            print(f"  {label:30s} {mn:8.3f}s / {med:8.3f}s   ({speedup:6.1f}x faster than bfinterp)")

        print("\n  compile steps (not in the execution numbers above):")
        for label, ms in compile_ms:
            print(f"    {label:38s} {ms:7.1f} ms")
        print("    (bfjit's compile is included in its run: parse + optimize +")
        print("     codegen + mmap + patch, single-digit milliseconds)")

        # 3. Optional Emulated AArch64 Lanes
        qemu_env = os.environ.get("QEMU_AARCH64", "").strip()
        bfjit_a64_env = os.environ.get("BFJIT_AARCH64", "").strip()
        bfnative_a64_env = os.environ.get("BFNATIVE_AARCH64", "").strip()

        qemu_path = shutil.which(qemu_env) if qemu_env else None
        emu_rows: list[tuple[str, tuple[float, float]]] = []

        if qemu_path:
            if bfjit_a64_env and Path(bfjit_a64_env).is_file():
                emu_rows.append(
                    (
                        "bfjit aarch64 (qemu)",
                        bench(
                            "bfjit-a64",
                            [qemu_path, bfjit_a64_env, str(MANDELBROT)],
                            expected,
                            runs,
                            warmup,
                        ),
                    )
                )
            if bfnative_a64_env and Path(bfnative_a64_env).is_file():
                native64 = bench_dir / "mandelbrot-native-aarch64"
                run_checked(
                    "bfnative-a64 compile",
                    [qemu_path, bfnative_a64_env, str(MANDELBROT), "-o", str(native64)],
                )
                ensure_executable(native64)
                emu_rows.append(
                    (
                        "bfnative aarch64 (qemu)",
                        bench("bfnative-a64", [qemu_path, str(native64)], expected, runs, warmup),
                    )
                )

        if emu_rows:
            print("\n  under qemu-aarch64 emulation (NOT comparable to the native numbers):")
            for label, (mn, med) in emu_rows:
                print(f"  {label:30s} {mn:8.3f}s / {med:8.3f}s")

    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except RuntimeError as err:
        print(f"\nbenchmark failed: {err}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print("\nbenchmark interrupted by user.", file=sys.stderr)
        sys.exit(130)
