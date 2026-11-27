#!/usr/bin/env python3
"""Differential test harness for the Brainfry execution backends.

Every program in the corpus is run through the execution strategies fed
by one shared parser/optimizer front end:

  1. `bfinterp`            — naive raw-source interpreter (default, no optimizer)
  2. `bfinterp --optimize` — same interpreter, optimizer enabled (bonus lane)
  3. `bfrun`               — bytecode VM (via bfc; the project's ground truth)
  4. `bfnative`            — ahead-of-time native executable
  5. `bfjit`               — JIT: same codegen as bfnative, executed in memory
  6. `bfwasm`              — standalone .wasm run under wasmtime and Node's WASI API
  7. `bfjs`                — standalone .js run under Node.js and browser engines
  8. `bf2c`                — portable C99 compiled with a host C compiler and run

Optional runtimes can be skipped during local development; `--require-all`
makes missing runtimes and compilers fatal for CI. `--only-node` runs only
the bfjs and bfwasm Node.js lanes, without executing the other backends.

stdout, stderr, and exit status must match bfrun byte-for-byte (with CRLF
normalized to LF on Windows). Browser lanes without configured browser hooks
use empty stdin because browser execution has EOF semantics; the split-loader
lane supplies corpus stdin through __bfwasm.
The corpus is the 27-program set from differential_test.py (deep nesting,
multiplication loops, tape growth to 200k cells, wraparound, EOF semantics,
stdin echo, runtime-error paths) plus Erik Bosman's mandelbrot.b — the same
real workload used for the native-vs-bytecode benchmark.

If bfinterp's naive path disagrees with the others, that is a bug in
bfinterp specifically: it is the newest, least-optimized implementation.

Usage:
    python3 scripts/differential_test_all.py [repo-root] [--only-node]
    python3 scripts/differential_test_all.py [repo-root] [--require-all]

Optional aarch64-under-QEMU mode (execution test of the arm64 JIT under
user-mode emulation, mirroring the bfnative harness's QEMU_AARCH64
convention):

    QEMU_AARCH64=/path/to/qemu-aarch64-static \
    BFJIT_AARCH64=target/aarch64-unknown-linux-gnu/release/bfjit \
    BFNATIVE_AARCH64=target/aarch64-unknown-linux-gnu/release/bfnative \
    BFRUN_AARCH64=target/aarch64-unknown-linux-gnu/release/bfrun \
        python3 scripts/differential_test_all.py [repo-root]

Each provided aarch64 binary is run under the emulator and compared against
the same x86_64 bfrun ground truth (Brainfuck I/O is byte-level, so outputs
are architecture-independent). Provide any subset.
"""

from __future__ import annotations

import argparse
import base64
from contextlib import contextmanager
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import sys
import tempfile
from threading import Thread
import time

IS_WINDOWS = os.name == "nt"
EXE = ".exe" if IS_WINDOWS else ""

HERE = Path(__file__).resolve().parent
DEFAULT_ROOT = HERE.parent

try:
    import wasmtime
    HAVE_WASMTIME = True
except ImportError:
    HAVE_WASMTIME = False

try:
    from playwright.sync_api import sync_playwright
    HAVE_PLAYWRIGHT = True
except ImportError:
    HAVE_PLAYWRIGHT = False


WASI_NODE_RUNNER_JS = """\
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
    process.exitCode = wasi.start(instance) ?? 0;
  })
  .catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
"""

BROWSER_CAPTURE_JS = """\
window.__bf = { out: [], err: [], thrown: false };
console.log = (...args) => {
  window.__bf.out.push(args.map(String).join(" "));
};
console.error = (...args) => {
  window.__bf.err.push(args.map(String).join(" "));
};
window.addEventListener("error", () => {
  window.__bf.thrown = true;
});
"""

BROWSER_WASM_HARNESS_JS = """\
async (encoded) => {
  const raw = atob(encoded);
  const bytes = Uint8Array.from(raw, (c) => c.charCodeAt(0));
  let memory;
  let exitCode = 0;
  const stdout = [];
  const stderr = [];
  const wasi = {
    fd_read(_fd, _iovs, _count, nread) {
      new DataView(memory.buffer).setUint32(nread, 0, true);
      return 0;
    },
    fd_write(fd, iovs, count, nwritten) {
      const view = new DataView(memory.buffer);
      let written = 0;
      for (let i = 0; i < count; i++) {
        const base = iovs + i * 8;
        const ptr = view.getUint32(base, true);
        const len = view.getUint32(base + 4, true);
        const sink = fd === 1 ? stdout : stderr;
        for (let j = 0; j < len; j++) {
          sink.push(view.getUint8(ptr + j));
        }
        written += len;
      }
      view.setUint32(nwritten, written, true);
      return 0;
    },
    proc_exit(code) {
      exitCode = code;
      throw { wasiExit: true };
    },
  };
  const { instance } = await WebAssembly.instantiate(bytes, {
    wasi_snapshot_preview1: wasi,
  });
  memory = instance.exports.memory;
  try {
    instance.exports._start();
  } catch (error) {
    if (!error || error.wasiExit !== true) {
      exitCode = 134;
    }
  }
  return [exitCode, stdout, stderr];
}
"""


@contextmanager
def temporary_directory(directory: str | Path | None = None):
    """Create a temp directory, retrying transient Windows executable/AV locks."""
    td = tempfile.mkdtemp(prefix="brainfry-diff-", dir=str(directory) if directory else None)
    try:
        yield Path(td)
    finally:
        for attempt in range(8):
            try:
                shutil.rmtree(td)
                break
            except PermissionError:
                if not IS_WINDOWS or attempt == 7:
                    raise
                time.sleep(min(0.1 * (2 ** attempt), 1.0))


def ensure_executable(path: Path) -> None:
    """Ensure POSIX executable permissions are set on compiled binaries."""
    if not IS_WINDOWS and path.exists():
        current_mode = path.stat().st_mode
        path.chmod(current_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def normalize_bytes(data: bytes) -> bytes:
    """Normalize CRLF to LF on Windows so cross-runtime comparisons match."""
    return data.replace(b"\r\n", b"\n") if IS_WINDOWS else data


def normalize_result(result: tuple[int, bytes, bytes]) -> tuple[int, bytes, bytes]:
    code, out, err = result
    return code, normalize_bytes(out), normalize_bytes(err)


def _probe(cmd: list[str], timeout: float = 60.0) -> bool:
    """Runs a command; True iff it exits 0 (crash/missing binary = False)."""
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


def find_msvc_vcvars() -> Path | None:
    """Find Visual Studio's vcvarsall.bat when cl.exe isn't already on PATH."""
    install = os.environ.get("VSINSTALLDIR", "").strip()
    if not install:
        program_files = os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")
        vswhere = Path(program_files) / "Microsoft Visual Studio" / "Installer" / "vswhere.exe"
        if vswhere.is_file():
            try:
                result = subprocess.run(
                    [
                        str(vswhere),
                        "-latest",
                        "-products",
                        "*",
                        "-requires",
                        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                        "-property",
                        "installationPath",
                    ],
                    capture_output=True,
                    text=True,
                    timeout=30,
                    check=False,
                )
                if result.returncode == 0:
                    install = result.stdout.strip()
            except (OSError, subprocess.SubprocessError):
                install = ""
    if not install:
        return None
    vcvars = Path(install) / "VC" / "Auxiliary" / "Build" / "vcvarsall.bat"
    return vcvars if vcvars.is_file() else None


def _msvc_batch_command(vcvars: Path, arch: str, command: str, batch_path: Path) -> list[str]:
    batch_content = (
        "@echo off\r\n"
        f'call "{vcvars}" {arch} >nul\r\n'
        "if errorlevel 1 exit /b %errorlevel%\r\n"
        f"{command}\r\n"
    )
    batch_path.write_bytes(batch_content.encode("utf-8"))
    return [os.environ.get("ComSpec", "cmd.exe"), "/d", "/c", str(batch_path)]


def _probe_msvc(compiler: tuple[str, ...]) -> bool:
    with temporary_directory() as td:
        src = td / "probe.c"
        exe = td / "probe.exe"
        obj = td / "probe.obj"
        src.write_text("int main(void) { return 0; }\n", encoding="utf-8")
        if compiler[0] == "direct":
            cmd = [compiler[1], "/nologo", f"/Fo{obj}", f"/Fe{exe}", str(src)]
        else:
            vcvars, arch = Path(compiler[1]), compiler[2]
            command = f'cl /nologo /Fo"{obj}" /Fe"{exe}" "{src}"'
            cmd = _msvc_batch_command(vcvars, arch, command, td / "probe-msvc.cmd")
        return _probe(cmd) and exe.is_file()


def find_host_cc() -> tuple[str, tuple[str, ...]] | None:
    """Find a working C compiler, including MSVC on Windows."""
    posix_candidates = ("clang", "gcc", "cc") if IS_WINDOWS else ("cc", "gcc", "clang")
    for cc_name in posix_candidates:
        cc_path = shutil.which(cc_name)
        if not cc_path:
            continue
        with temporary_directory() as td:
            src = td / "probe.c"
            exe = td / f"probe{EXE}"
            src.write_text("int main(void) { return 0; }\n", encoding="utf-8")
            if _probe([cc_path, "-o", str(exe), str(src)]) and exe.is_file():
                return ("native", (cc_path,))

    if IS_WINDOWS:
        cl = shutil.which("cl")
        if cl:
            compiler = ("direct", cl)
            if _probe_msvc(compiler):
                return ("msvc", compiler)
        vcvars = find_msvc_vcvars()
        if vcvars:
            arch = "arm64" if platform.machine().lower() in ("arm64", "aarch64") else "amd64"
            compiler = ("vcvars", str(vcvars), arch)
            if _probe_msvc(compiler):
                return ("msvc", compiler)
    return None


# (name, source, stdin) — the 27-program corpus from differential_test.py,
# plus mandelbrot.
CORPUS: list[tuple[str, str, str]] = [
    (
        "hello-world",
        "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---."
        "+++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.",
        "",
    ),
    ("echo", ",[.[-],]", "echo, echo, echo\n"),
    ("eof-unchanged", ",+.", ""),
    ("eof-byte", ",.", "AB"),
    ("eof-inside-loop", ",[.[-],]", "hi!"),
    ("wrap-256", "+" * 256 + ".", ""),
    ("wrap-negative", "-.", ""),
    ("wrap-254", "--.", ""),
    (
        "deep-nesting-six",
        "++++++[>+++++[>++++[>+++[>++[>++[>+<-]<-]<-]<-]<-]<-]>>>>>>.",
        "",
    ),
    ("multiply-copy-loop", "++++++[>++++++<-]>[<+>-]<+.", ""),
    ("multiply-loop-unfolded", "+++[>+++[>+<-]<-]>>.", ""),  # 3*3=9 in cell 2
    ("squares", "++++[>+++++<-]>.", ""),  # 4*5 = 20
    ("io-interleaved", ",.,.,.", "abcxyz"),
    ("io-sum", ",>,[<+>-]<.", "\x0a\x0b"),  # add two bytes
    ("tape-40k", ">" * 40_000 + "+.", ""),
    ("tape-200k", ">" * 200_000 + "+.", ""),
    (
        "tape-write-then-read-back",
        ">" * 65_600 + "+++" + "<" * 65_600 + ".",  # crosses a 64 KiB commit
        "",
    ),
    ("zero-idiom", "++++++++++[-]+.", ""),
    ("zero-idiom-nested", "++[+++[-]+[-]]++.", ""),
    # Even-step zero loop: must NOT fold to Zero.
    ("zero-loop-even-step", "++[--].", ""),
    ("nested-zero-in-loop", ",[.[-],]", "roundtrip"),
    ("many-small-loops", "+[>+[>+<-]<-]>", ""),  # idiom stress
    # Loops folded into Scan/MulAdd must still skip a zero cell entirely.
    ("scan-left-on-zero-cell", "[<]+.", ""),
    ("scan-right-on-zero-cell", "[>]+.", ""),
    ("scan-stops-on-zero", "+>+>+<<[>]+.", ""),
    ("fold-negative-offset-zero-source", "[-<+>]+.", ""),
    ("fold-negative-offset-nonzero-source", ">+++[-<+>]<.", ""),
    # Runtime-error cases: every tool must reproduce bfrun's message and
    # exit status consistently.
    ("underflow-immediate", "<", ""),
    ("underflow-inside-loop", "+[<]", ""),
    ("underflow-after-walk", ">" * 10 + "<" * 11, ""),
    ("huge-left", ">" * 10 + "<" * 100_000, ""),
    # Offset-add folding candidates whose body visits a cell without writing it.
    ("underflow-untouched-excursion", "+[<>-]", ""),
    ("underflow-trailing-excursion", "+[-<>]", ""),
]


def load_mandelbrot(mandelbrot_path: Path) -> list[tuple[str, str, str]]:
    if mandelbrot_path.is_file():
        return [("mandelbrot", mandelbrot_path.read_text(encoding="utf-8"), "")]
    print(f"note: {mandelbrot_path} not found — running without it")
    return []


def run_cmd(cmd: list[str], stdin: str) -> tuple[int, bytes, bytes]:
    """Runs cmd with stdin bytes; returns (exit, stdout, stderr)."""
    try:
        r = subprocess.run(
            cmd,
            input=stdin.encode("utf-8"),
            capture_output=True,
            timeout=600,
            check=False,
        )
        return r.returncode, r.stdout, r.stderr
    except subprocess.TimeoutExpired:
        return -99, b"<timeout>", b""


def run_ref(tools: dict[str, Path], source: str, stdin: str) -> tuple[int, bytes, bytes]:
    """bfc -> bfrun. The ground truth."""
    with temporary_directory() as td:
        src = td / "p.bf"
        bfry = td / "p.bfry"
        src.write_text(source, encoding="utf-8")
        c = subprocess.run([str(tools["bfc"]), str(src), str(bfry)], capture_output=True, check=False)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        return run_cmd([str(tools["bfrun"]), str(bfry)], stdin)


def run_source_tool(
    binary: Path, source: str, stdin: str, extra_args: tuple[str, ...] = ()
) -> tuple[int, bytes, bytes]:
    """Runs a tool that takes a .bf source file (bfinterp, bfjit)."""
    with temporary_directory() as td:
        src = td / "p.bf"
        src.write_text(source, encoding="utf-8")
        return run_cmd([str(binary), *extra_args, str(src)], stdin)


def run_native(bfnative: Path, source: str, stdin: str) -> tuple[int, bytes, bytes]:
    """bfnative -> execute the produced native binary."""
    with temporary_directory() as td:
        src = td / "p.bf"
        exe = td / f"p{EXE}"
        src.write_text(source, encoding="utf-8")
        c = subprocess.run([str(bfnative), str(src), "-o", str(exe)], capture_output=True, check=False)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        ensure_executable(exe)
        return run_cmd([str(exe)], stdin)


def run_wasm(bfwasm: Path, source: str, stdin: str) -> tuple[int, bytes, bytes]:
    """bfwasm -> execute the produced module under wasmtime's WASI preview1."""
    with temporary_directory() as td:
        src = td / "p.bf"
        wasm_path = td / "p.wasm"
        stdin_path = td / "stdin"
        stdout_path = td / "stdout"
        stderr_path = td / "stderr"
        src.write_text(source, encoding="utf-8")
        c = subprocess.run([str(bfwasm), str(src), "-o", str(wasm_path)], capture_output=True, check=False)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr

        stdin_path.write_bytes(stdin.encode("utf-8"))

        engine = wasmtime.Engine()
        module = wasmtime.Module.from_file(engine, str(wasm_path))
        linker = wasmtime.Linker(engine)
        linker.define_wasi()

        wasi = wasmtime.WasiConfig()
        wasi.stdin_file = str(stdin_path)
        wasi.stdout_file = str(stdout_path)
        wasi.stderr_file = str(stderr_path)

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
            exit_code = 134

        out = stdout_path.read_bytes() if stdout_path.exists() else b""
        err = stderr_path.read_bytes() if stderr_path.exists() else b""
        return exit_code, out, err


def run_wasm_node(bfwasm: Path, node_bin: str, source: str, stdin: str) -> tuple[int, bytes, bytes]:
    """Compile bfwasm and execute it through Node's built-in WASI preview1."""
    with temporary_directory() as td:
        src = td / "p.bf"
        wasm_path = td / "p.wasm"
        runner = td / "run-wasi.cjs"
        src.write_text(source, encoding="utf-8")
        c = subprocess.run([str(bfwasm), str(src), "-o", str(wasm_path)], capture_output=True, check=False)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        runner.write_text(WASI_NODE_RUNNER_JS, encoding="utf-8")
        return run_cmd([node_bin, "--no-warnings", str(runner), str(wasm_path)], stdin)


def run_js(bfjs: Path, node_bin: str, source: str, stdin: str) -> tuple[int, bytes, bytes]:
    """bfjs -> execute the produced .js under node."""
    with temporary_directory() as td:
        src = td / "p.bf"
        js_path = td / "p.js"
        src.write_text(source, encoding="utf-8")
        c = subprocess.run([str(bfjs), str(src), "-o", str(js_path)], capture_output=True, check=False)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        return run_cmd([node_bin, str(js_path)], stdin)


def compile_artifact(binary: Path, source: str, extension: str, td: Path) -> tuple[subprocess.CompletedProcess, Path]:
    source_path = td / "p.bf"
    output_path = td / f"p{extension}"
    source_path.write_text(source, encoding="utf-8")
    result = subprocess.run([str(binary), str(source_path), "-o", str(output_path)], capture_output=True, check=False)
    return result, output_path


def _decode_browser_byte_string(text: str) -> bytes:
    """Safely convert JS 0x00-0xFF character strings or UTF-8 text into raw bytes."""
    try:
        return text.encode("latin-1")
    except UnicodeEncodeError:
        return text.encode("utf-8")


class QuietHTTPRequestHandler(SimpleHTTPRequestHandler):
    def log_message(self, _format, *_args):
        pass


class BrowserHarness:
    """One Playwright browser instance per engine for the entire corpus."""

    def __init__(self, tools: dict[str, Path]):
        self.tools = tools
        self.playwright = sync_playwright().start()
        self.browsers = {}
        self.pages = {}
        self.http_root = tempfile.TemporaryDirectory(prefix="bfwasm-browser-")
        self.http_server = None
        self.http_thread = None
        try:
            index_path = Path(self.http_root.name) / "index.html"
            index_path.write_text("<!doctype html><meta charset=utf-8><title>bf</title>", encoding="utf-8")
            handler = partial(QuietHTTPRequestHandler, directory=self.http_root.name)
            self.http_server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
            self.http_thread = Thread(target=self.http_server.serve_forever, daemon=True)
            self.http_thread.start()
            self.http_url = f"http://127.0.0.1:{self.http_server.server_port}/"
            for name in ("chromium", "firefox", "webkit"):
                browser = getattr(self.playwright, name).launch()
                page = browser.new_page()
                page.add_init_script(BROWSER_CAPTURE_JS)
                page.set_content(
                    "<!doctype html><meta charset=utf-8><title>bf</title>"
                    f"<script>{BROWSER_CAPTURE_JS}</script>"
                )
                self.browsers[name] = browser
                self.pages[name] = page
        except Exception:
            self.close()
            raise

    def run_js(self, engine: str, source: str) -> tuple[int, bytes, bytes]:
        with temporary_directory() as td:
            result, js_path = compile_artifact(self.tools["bfjs"], source, ".js", td)
            if result.returncode != 0:
                return result.returncode, result.stdout, result.stderr
            page = self.pages[engine]
            page.evaluate("window.__bf = { out: [], err: [], thrown: false }")
            js_code = js_path.read_text(encoding="utf-8").removeprefix("#!/usr/bin/env node\n")
            page.add_script_tag(content=js_code)
            captured = page.evaluate("window.__bf")
            stdout = _decode_browser_byte_string("".join(captured["out"]))
            stderr = "".join(captured["err"]).encode("utf-8")
            if stderr:
                stderr += b"\n"
            return (1 if captured["thrown"] else 0), stdout, stderr

    def run_wasm(self, engine: str, source: str) -> tuple[int, bytes, bytes]:
        with temporary_directory() as td:
            result, wasm_path = compile_artifact(self.tools["bfwasm"], source, ".wasm", td)
            if result.returncode != 0:
                return result.returncode, result.stdout, result.stderr
            wasm_b64 = base64.b64encode(wasm_path.read_bytes()).decode("ascii")
            page = self.pages[engine]
            exit_code, stdout, stderr = page.evaluate(BROWSER_WASM_HARNESS_JS, wasm_b64)
            return exit_code, bytes(stdout), bytes(stderr)

    def run_wasm_browser_output(self, engine: str, source: str) -> tuple[int, bytes, bytes]:
        with temporary_directory() as td:
            source_path = td / "p.bf"
            js_path = td / "p.js"
            source_path.write_text(source, encoding="utf-8")
            result = subprocess.run(
                [str(self.tools["bfwasm"]), str(source_path), "--target", "browser", "-o", str(js_path)],
                capture_output=True,
                check=False,
            )
            if result.returncode != 0:
                return result.returncode, result.stdout, result.stderr

            page = self.pages[engine]
            page.evaluate(
                "window.__bf = { out: [], err: [], thrown: false };"
                "delete window.__bfwasm;"
                "delete window.__bfwasmExitCode;"
            )
            page.add_script_tag(content=js_path.read_text(encoding="utf-8"))
            page.wait_for_function("typeof window.__bfwasmExitCode === 'number'", timeout=30000)
            captured = page.evaluate("window.__bf")
            stdout = _decode_browser_byte_string("".join(captured["out"]))
            stderr = "".join(captured["err"]).encode("utf-8")
            if stderr:
                stderr += b"\n"
            return page.evaluate("window.__bfwasmExitCode"), stdout, stderr

    def run_wasm_split(self, engine: str, source: str, stdin: str) -> tuple[int, bytes, bytes]:
        with temporary_directory(self.http_root.name) as td:
            source_path = td / "p.bf"
            js_path = td / "p.js"
            source_path.write_text(source, encoding="utf-8")
            result = subprocess.run(
                [
                    str(self.tools["bfwasm"]),
                    str(source_path),
                    "--target",
                    "browser",
                    "--split",
                    "-o",
                    str(js_path),
                ],
                capture_output=True,
                check=False,
            )
            if result.returncode != 0:
                return result.returncode, result.stdout, result.stderr

            relative_dir = td.name
            page = self.pages[engine]
            page.goto(self.http_url + "index.html")
            page.evaluate(
                """({ input, useBytes }) => {
                  const capture = { stdout: [], stderr: [] };
                  globalThis.__bfwasmCapture = capture;
                  globalThis.__bfwasm = {
                    stdin: useBytes ? new Uint8Array(input) : "",
                    onStdout(chunk) { capture.stdout.push(...chunk); },
                    onStderr(chunk) { capture.stderr.push(...chunk); }
                  };
                  delete globalThis.__bfwasmExitCode;
                }""",
                {"input": list(stdin.encode("utf-8")), "useBytes": bool(stdin)},
            )
            page.evaluate("url => import(url)", f"{self.http_url}{relative_dir}/p.js")
            page.wait_for_function("typeof globalThis.__bfwasmExitCode === 'number'", timeout=30000)
            exit_code, stdout, stderr = page.evaluate(
                """() => [
                  globalThis.__bfwasmExitCode,
                  globalThis.__bfwasmCapture.stdout,
                  globalThis.__bfwasmCapture.stderr
                ]"""
            )
            return exit_code, bytes(stdout), bytes(stderr)

    def close(self):
        for browser in self.browsers.values():
            try:
                browser.close()
            except Exception:
                pass
        self.browsers.clear()
        if self.playwright:
            try:
                self.playwright.stop()
            except Exception:
                pass
            self.playwright = None
        if self.http_server:
            self.http_server.shutdown()
            self.http_server.server_close()
            self.http_server = None
        if self.http_thread:
            self.http_thread.join(timeout=5)
            self.http_thread = None
        self.http_root.cleanup()


def run_c(
    bf2c: Path, cc_info: tuple[str, tuple[str, ...]], source: str, stdin: str
) -> tuple[int, bytes, bytes]:
    """bf2c -> compile the produced C99 with the host cc, then execute."""
    with temporary_directory() as td:
        src = td / "p.bf"
        c_path = td / "p.c"
        exe = td / f"p{EXE}"
        obj = td / "p.obj"
        src.write_text(source, encoding="utf-8")
        c = subprocess.run([str(bf2c), str(src), "-o", str(c_path)], capture_output=True, check=False)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        if cc_info[0] == "msvc":
            compiler = cc_info[1]
            if compiler[0] == "direct":
                cmd = [compiler[1], "/nologo", "/O2", f"/Fo{obj}", f"/Fe{exe}", str(c_path)]
            else:
                command = f'cl /nologo /O2 /Fo"{obj}" /Fe"{exe}" "{c_path}"'
                cmd = _msvc_batch_command(Path(compiler[1]), compiler[2], command, td / "compile-msvc.cmd")
        else:
            cmd = [cc_info[1][0], "-O2", "-o", str(exe), str(c_path)]
        k = subprocess.run(cmd, capture_output=True, timeout=600, check=False)
        if k.returncode != 0:
            return k.returncode, k.stdout, k.stderr
        ensure_executable(exe)
        return run_cmd([str(exe)], stdin)


def run_emulated(binary: str, emulator: str, source: str, stdin: str) -> tuple[int, bytes, bytes]:
    """An aarch64 tool binary under qemu-aarch64-static."""
    with temporary_directory() as td:
        src = td / "p.bf"
        src.write_text(source, encoding="utf-8")
        return run_cmd([emulator, binary, str(src)], stdin)


def run_emulated_bfnative(binary: str, emulator: str, source: str, stdin: str) -> tuple[int, bytes, bytes]:
    """The aarch64 bfnative under qemu: compile, then execute the produced aarch64 ELF."""
    with temporary_directory() as td:
        src = td / "p.bf"
        exe = td / "p"
        src.write_text(source, encoding="utf-8")
        c = run_cmd([emulator, binary, str(src), "-o", str(exe)], "")
        if c[0] != 0:
            return c
        ensure_executable(exe)
        return run_cmd([emulator, str(exe)], stdin)


def run_emulated_bfrun(
    bfc: Path, binary: str, emulator: str, source: str, stdin: str
) -> tuple[int, bytes, bytes]:
    """An aarch64 bfrun under qemu (.bfry bytecode is platform-independent)."""
    with temporary_directory() as td:
        src = td / "p.bf"
        bfry = td / "p.bfry"
        src.write_text(source, encoding="utf-8")
        c = subprocess.run([str(bfc), str(src), str(bfry)], capture_output=True, check=False)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        return run_cmd([emulator, binary, str(bfry)], stdin)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo_root", nargs="?", default=str(DEFAULT_ROOT))
    parser.add_argument(
        "--only-node",
        action="store_true",
        help="run only bfjs and bfwasm via Node.js",
    )
    parser.add_argument(
        "--require-all",
        action="store_true",
        help="fail if any runtime/compiler required by selected lanes is missing",
    )
    args = parser.parse_args(argv)

    root = Path(args.repo_root).resolve()
    release = root / "target" / "release"
    tools = {
        name: release / f"{name}{EXE}"
        for name in ("bfc", "bfrun", "bfnative", "bfinterp", "bfjit", "bfwasm", "bfjs", "bf2c")
    }
    mandelbrot_path = root / "tests" / "mandelbrot.b"

    required_tools = ["bfc", "bfrun", "bfwasm", "bfjs"]
    if not args.only_node:
        required_tools.extend(["bfnative", "bfinterp", "bfjit", "bf2c"])
    missing = [tools[name] for name in required_tools if not tools[name].is_file()]
    if missing:
        print(f"missing {missing[0]} — run `cargo build --release` first", file=sys.stderr)
        return 1

    node_bin, node_has_wasi = find_host_node()
    cc_info = None if args.only_node else find_host_cc()

    unavailable = []
    if node_bin is None:
        unavailable.append("node")
    elif not node_has_wasi:
        unavailable.append("node:wasi module")

    if not args.only_node:
        if not HAVE_WASMTIME:
            unavailable.append("wasmtime Python package")
        if cc_info is None:
            unavailable.append("C compiler (cc, gcc, clang, or cl.exe)")
        if not HAVE_PLAYWRIGHT:
            unavailable.append("playwright Python package")

    if unavailable and args.require_all:
        print("required test dependencies missing: " + ", ".join(unavailable), file=sys.stderr)
        return 1
    if node_bin is None and args.only_node:
        print("the --only-node lane requires a working node on PATH", file=sys.stderr)
        return 1

    if not args.only_node:
        if not HAVE_WASMTIME:
            print("note: `wasmtime` Python package not installed — skipping the bfwasm wasmtime lane\n")
        if node_bin is not None and not node_has_wasi:
            print("note: `node` lacks `node:wasi` — skipping the bfwasm node:wasi lane\n")
        if cc_info is None:
            print("note: no working C compiler — skipping the bf2c lane\n")
        if not HAVE_PLAYWRIGHT:
            print("note: `playwright` Python package not installed — skipping browser lanes\n")

    browser_harness: BrowserHarness | None = None
    if HAVE_PLAYWRIGHT and not args.only_node:
        try:
            browser_harness = BrowserHarness(tools)
        except Exception as error:
            if args.require_all:
                print(f"could not launch required Playwright browsers: {error}", file=sys.stderr)
                return 1
            print(f"note: could not launch Playwright browsers — skipping browser lanes: {error}")

    try:
        corpus = CORPUS + load_mandelbrot(mandelbrot_path)

        qemu_env = os.environ.get("QEMU_AARCH64", "").strip()
        qemu_path = shutil.which(qemu_env) if qemu_env else None
        aarch64_lanes: list[tuple[str, str]] = []
        if not args.only_node and qemu_path:
            for lane_name, env_var in [
                ("bfjit", "BFJIT_AARCH64"),
                ("bfnative", "BFNATIVE_AARCH64"),
                ("bfrun", "BFRUN_AARCH64"),
            ]:
                bin_path = os.environ.get(env_var, "").strip()
                if bin_path and Path(bin_path).is_file():
                    aarch64_lanes.append((lane_name, bin_path))
            if aarch64_lanes:
                print(f"also testing under {qemu_path}: {[name for name, _ in aarch64_lanes]}\n")

        failures = 0
        lane_failures = 0
        comparisons = 0

        for name, source, stdin in corpus:
            ref = run_ref(tools, source, stdin)
            results: list[tuple[str, tuple[int, bytes, bytes] | tuple[tuple[int, bytes, bytes], tuple[int, bytes, bytes]]]] = []

            if args.only_node:
                assert node_bin is not None
                if node_has_wasi:
                    results.append(("bfwasm(node:wasi)", run_wasm_node(tools["bfwasm"], node_bin, source, stdin)))
                results.append(("bfjs(node)", run_js(tools["bfjs"], node_bin, source, stdin)))
            else:
                results.extend([
                    ("bfinterp", run_source_tool(tools["bfinterp"], source, stdin)),
                    ("bfinterp --optimize", run_source_tool(tools["bfinterp"], source, stdin, ("--optimize",))),
                    ("bfnative", run_native(tools["bfnative"], source, stdin)),
                    ("bfjit", run_source_tool(tools["bfjit"], source, stdin)),
                ])
                if HAVE_WASMTIME:
                    results.append(("bfwasm(wasmtime)", run_wasm(tools["bfwasm"], source, stdin)))
                if node_bin is not None:
                    if node_has_wasi:
                        results.append(("bfwasm(node:wasi)", run_wasm_node(tools["bfwasm"], node_bin, source, stdin)))
                    results.append(("bfjs(node)", run_js(tools["bfjs"], node_bin, source, stdin)))
                if cc_info is not None:
                    results.append(("bf2c", run_c(tools["bf2c"], cc_info, source, stdin)))
                if browser_harness is not None:
                    browser_ref = run_ref(tools, source, "")
                    for engine in ("chromium", "firefox", "webkit"):
                        results.extend([
                            (f"bfjs({engine})", (browser_ref, browser_harness.run_js(engine, source))),
                            (f"bfwasm({engine})", (browser_ref, browser_harness.run_wasm(engine, source))),
                            (f"bfwasm-js({engine})", (browser_ref, browser_harness.run_wasm_browser_output(engine, source))),
                            (f"bfwasm-split({engine})", (ref, browser_harness.run_wasm_split(engine, source, stdin))),
                        ])

            for lane, path in aarch64_lanes:
                assert qemu_path is not None
                if lane == "bfrun":
                    results.append((f"{lane}(qemu)", run_emulated_bfrun(tools["bfc"], path, qemu_path, source, stdin)))
                elif lane == "bfnative":
                    results.append((f"{lane}(qemu)", run_emulated_bfnative(path, qemu_path, source, stdin)))
                else:
                    results.append((f"{lane}(qemu)", run_emulated(path, qemu_path, source, stdin)))

            ok = True
            comparisons += len(results)
            for label, result in results:
                if (
                    isinstance(result, tuple)
                    and len(result) == 2
                    and isinstance(result[0], tuple)
                ):
                    expected_raw, actual_raw = result
                else:
                    expected_raw, actual_raw = ref, result  # type: ignore[assignment]

                expected = normalize_result(expected_raw)
                actual = normalize_result(actual_raw)

                if expected != actual:
                    ok = False
                    lane_failures += 1
                    print(f"       ref   : exit={expected[0]} stdout={expected[1]!r} stderr={expected[2]!r}")
                    print(f"       {label:20s}: exit={actual[0]} stdout={actual[1]!r} stderr={actual[2]!r}")

            status = "ok " if ok else "FAIL"
            print(f"[{status}] {name}: exit {ref[0]}, stdout {len(ref[1])}B, {len(results)} lanes")
            if not ok:
                failures += 1

        print(
            f"\n{len(corpus) - failures}/{len(corpus)} programs: "
            f"{comparisons - lane_failures}/{comparisons} executions match bfrun exactly "
            f"(stdout, stderr, exit status)"
        )
        return 1 if failures else 0
    finally:
        if browser_harness is not None:
            browser_harness.close()


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        print("\ndifferential test interrupted by user.", file=sys.stderr)
        sys.exit(130)
