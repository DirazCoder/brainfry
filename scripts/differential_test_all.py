#!/usr/bin/env python3
"""Differential test harness for the Brainfry execution backends.

Every program in the corpus is run through the execution strategies fed
by one shared parser/optimizer front end:

  1. `bfinterp`         — naive raw-source interpreter (default, no optimizer)
  2. `bfinterp --optimize` — same interpreter, optimizer enabled (bonus lane)
  3. `bfrun`            — bytecode VM (via bfc; the project's ground truth)
  4. `bfnative`         — ahead-of-time native executable
  5. `bfjit`            — JIT: same codegen as bfnative, executed in memory
  6. `bfwasm`           — standalone .wasm run under wasmtime and Node's WASI API
  7. `bfjs`             — standalone .js run under Node.js and browser engines
  8. `bf2c`             — portable C99 compiled with a host C compiler and run

Optional runtimes can be skipped during local development; `--require-all`
makes missing runtimes and compilers fatal for CI. `--only-node` runs only
the bfjs and bfwasm Node.js lanes, without executing the other backends.

stdout, stderr, and exit status must match bfrun byte-for-byte. Browser lanes
without configured browser hooks use empty stdin because browser execution has
EOF semantics; the split-loader lane supplies corpus stdin through __bfwasm.
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

    QEMU_AARCH64=/path/to/qemu-aarch64-static \\
    BFJIT_AARCH64=target/aarch64-unknown-linux-gnu/release/bfjit \\
    BFNATIVE_AARCH64=target/aarch64-unknown-linux-gnu/release/bfnative \\
    BFRUN_AARCH64=target/aarch64-unknown-linux-gnu/release/bfrun \\
        python3 scripts/differential_test_all.py [repo-root]

Each provided aarch64 binary is run under the emulator and compared against
the same x86_64 bfrun ground truth (Brainfuck I/O is byte-level, so outputs
are architecture-independent). Provide any subset.
"""

from contextlib import contextmanager
import argparse
import base64
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from threading import Thread
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)

EXE = ".exe" if os.name == "nt" else ""


@contextmanager
def temporary_directory(directory=None):
    """Create a temp directory, retrying transient Windows executable locks."""
    td = tempfile.mkdtemp(dir=directory)
    try:
        yield td
    finally:
        for attempt in range(8):
            try:
                shutil.rmtree(td)
                break
            except PermissionError:
                if os.name != "nt" or attempt == 7:
                    raise
                time.sleep(min(0.1 * (2 ** attempt), 1.0))


BFC = os.path.join(ROOT, "target", "release", "bfc" + EXE)
BFRUN = os.path.join(ROOT, "target", "release", "bfrun" + EXE)
BFNATIVE = os.path.join(ROOT, "target", "release", "bfnative" + EXE)
BFINTERP = os.path.join(ROOT, "target", "release", "bfinterp" + EXE)
BFJIT = os.path.join(ROOT, "target", "release", "bfjit" + EXE)
BFWASM = os.path.join(ROOT, "target", "release", "bfwasm" + EXE)
BFJS = os.path.join(ROOT, "target", "release", "bfjs" + EXE)
BF2C = os.path.join(ROOT, "target", "release", "bf2c" + EXE)
MANDELBROT = os.path.join(ROOT, "tests", "mandelbrot.b")

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


def _probe(cmd, timeout=60):
    """Runs a command; True iff it exits 0 (crash/missing binary = False)."""
    try:
        return subprocess.run(cmd, capture_output=True,
                               timeout=timeout).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        return False


def find_host_node():
    """A `node` that actually executes a trivial script, or None."""
    return "node" if _probe(["node", "-e", "0"]) else None


def find_msvc_vcvars():
    """Find Visual Studio's vcvarsall.bat when cl.exe isn't already on PATH."""
    install = os.environ.get("VSINSTALLDIR")
    if not install:
        program_files = os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")
        vswhere = os.path.join(program_files, "Microsoft Visual Studio",
                               "Installer", "vswhere.exe")
        if os.path.exists(vswhere):
            result = subprocess.run(
                [vswhere, "-latest", "-products", "*", "-requires",
                 "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                 "-property", "installationPath"],
                capture_output=True, text=True, timeout=30)
            if result.returncode == 0:
                install = result.stdout.strip()
    if not install:
        return None
    vcvars = os.path.join(install, "VC", "Auxiliary", "Build", "vcvarsall.bat")
    return vcvars if os.path.exists(vcvars) else None


def _msvc_batch_command(vcvars, arch, command, batch_path):
    with open(batch_path, "w", newline="") as batch:
        batch.write("@echo off\r\n")
        batch.write(f'call "{vcvars}" {arch} >nul\r\n')
        batch.write("if errorlevel 1 exit /b %errorlevel%\r\n")
        batch.write(command + "\r\n")
    return [os.environ.get("ComSpec", "cmd.exe"), "/d", "/c", batch_path]


def _probe_msvc(compiler):
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "probe.c")
        exe = os.path.join(td, "probe.exe")
        obj = os.path.join(td, "probe.obj")
        with open(src, "w") as f:
            f.write("int main(void) { return 0; }\n")
        if compiler[0] == "direct":
            cmd = [compiler[1], "/nologo", "/Fe" + exe, src]
        else:
            vcvars, arch = compiler[1], compiler[2]
            command = f'cl /nologo /Fo"{obj}" /Fe"{exe}" "{src}"'
            cmd = _msvc_batch_command(
                vcvars, arch, command, os.path.join(td, "probe-msvc.cmd"))
        return _probe(cmd)


def find_host_cc():
    """Find a working C compiler, including MSVC on Windows."""
    for cc in ("cc", "gcc", "clang"):
        with tempfile.TemporaryDirectory() as td:
            src = os.path.join(td, "probe.c")
            exe = os.path.join(td, "probe" + EXE)
            with open(src, "w") as f:
                f.write("int main(void) { return 0; }\n")
            if _probe([cc, "-o", exe, src]):
                return ("native", cc)
    if os.name == "nt":
        cl = shutil.which("cl")
        if cl:
            compiler = ("direct", cl)
            if _probe_msvc(compiler):
                return ("msvc", compiler)
        vcvars = find_msvc_vcvars()
        if vcvars:
            arch = "arm64" if platform.machine().lower() in ("arm64", "aarch64") else "amd64"
            compiler = ("vcvars", vcvars, arch)
            if _probe_msvc(compiler):
                return ("msvc", compiler)
    return None


NODE = find_host_node()
CC = find_host_cc()

QEMU_AARCH64 = os.environ.get("QEMU_AARCH64", "")
BFJIT_AARCH64 = os.environ.get("BFJIT_AARCH64", "")
BFNATIVE_AARCH64 = os.environ.get("BFNATIVE_AARCH64", "")
BFRUN_AARCH64 = os.environ.get("BFRUN_AARCH64", "")

# (name, source, stdin) — the 27-program corpus from differential_test.py,
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
    # Even-step zero loop: must NOT fold to Zero (an even step from an odd
    # cell never reaches 0, so folding would turn a diverging loop into a
    # terminating one). From an even cell it terminates on every lane by
    # executing the real loop, which is what makes this testable at all.
    ("zero-loop-even-step",
     "++[--].",
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
    # Offset-add folding candidates whose body visits a cell without
    # writing it: bfc must NOT fold them to Zero, or the underflow the real
    # loop raises disappears on every bfc-fed lane while bfinterp's naive
    # path keeps it.
    ("underflow-untouched-excursion",
     "+[<>-]",
     ""),
    ("underflow-trailing-excursion",
     "+[-<>]",
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


def run_wasm_node(source, stdin):
    """Compile bfwasm and execute it through Node's built-in WASI preview1."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        wasm_path = os.path.join(td, "p.wasm")
        runner = os.path.join(td, "run-wasi.mjs")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BFWASM, src, "-o", wasm_path], capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        with open(runner, "w") as f:
            f.write(
                'import { readFileSync } from "node:fs";\n'
                'import { WASI } from "node:wasi";\n'
                'const wasi = new WASI({ version: "preview1", args: [], env: {}, preopens: {} });\n'
                'const { instance } = await WebAssembly.instantiate('
                'readFileSync(process.argv[2]), wasi.getImportObject());\n'
                'process.exitCode = wasi.start(instance) ?? 0;\n'
            )
        return run_cmd([NODE, "--no-warnings", runner, wasm_path], stdin, td)


def run_js(source, stdin):
    """bfjs -> execute the produced .js under node."""
    with tempfile.TemporaryDirectory() as td:
        src = os.path.join(td, "p.bf")
        js_path = os.path.join(td, "p.js")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BFJS, src, "-o", js_path], capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        return run_cmd([NODE, js_path], stdin, td)


def compile_artifact(binary, source, extension, td):
    source_path = os.path.join(td, "p.bf")
    output_path = os.path.join(td, "p" + extension)
    with open(source_path, "w") as f:
        f.write(source)
    result = subprocess.run([binary, source_path, "-o", output_path],
                            capture_output=True)
    return result, output_path


class QuietHTTPRequestHandler(SimpleHTTPRequestHandler):
    def log_message(self, _format, *_args):
        pass


class BrowserHarness:
    """One Playwright browser instance per engine for the entire corpus."""

    def __init__(self):
        self.playwright = sync_playwright().start()
        self.browsers = {}
        self.pages = {}
        self.http_root = tempfile.TemporaryDirectory(prefix="bfwasm-browser-")
        self.http_server = None
        self.http_thread = None
        try:
            with open(os.path.join(self.http_root.name, "index.html"), "w") as f:
                f.write("<!doctype html><meta charset=utf-8><title>bf</title>")
            handler = partial(QuietHTTPRequestHandler,
                             directory=self.http_root.name)
            self.http_server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
            self.http_thread = Thread(target=self.http_server.serve_forever,
                                      daemon=True)
            self.http_thread.start()
            self.http_url = f"http://127.0.0.1:{self.http_server.server_port}/"
            for name in ("chromium", "firefox", "webkit"):
                browser = getattr(self.playwright, name).launch()
                page = browser.new_page()
                capture_script = (
                    "window.__bf={out:[],err:[],thrown:false};"
                    "console.log=(...a)=>window.__bf.out.push(String(a[0]));"
                    "console.error=(...a)=>window.__bf.err.push(String(a[0]));"
                    "window.addEventListener('error',()=>{window.__bf.thrown=true});"
                )
                page.add_init_script(capture_script)
                page.set_content(
                    "<!doctype html><meta charset=utf-8><title>bf</title>"
                    f"<script>{capture_script}</script>")
                self.browsers[name] = browser
                self.pages[name] = page
        except Exception:
            self.close()
            raise

    def run_js(self, engine, source, stdin):
        with tempfile.TemporaryDirectory() as td:
            result, js_path = compile_artifact(BFJS, source, ".js", td)
            if result.returncode != 0:
                return result.returncode, result.stdout, result.stderr
            page = self.pages[engine]
            page.evaluate("window.__bf={out:[],err:[],thrown:false}")
            with open(js_path, encoding="utf-8") as f:
                js = f.read()
            page.add_script_tag(content=js.removeprefix("#!/usr/bin/env node\n"))
            captured = page.evaluate("window.__bf")
            output = "".join(captured["out"])
            stdout = bytes(ord(char) for char in output)
            stderr = "".join(captured["err"]).encode("utf-8")
            if stderr:
                stderr += b"\n"
            return (1 if captured["thrown"] else 0), stdout, stderr

    def run_wasm(self, engine, source, stdin):
        with tempfile.TemporaryDirectory() as td:
            result, wasm_path = compile_artifact(BFWASM, source, ".wasm", td)
            if result.returncode != 0:
                return result.returncode, result.stdout, result.stderr
            with open(wasm_path, "rb") as f:
                wasm = base64.b64encode(f.read()).decode("ascii")
            page = self.pages[engine]
            exit_code, stdout, stderr = page.evaluate(
                """async (encoded) => {
                  const raw = atob(encoded);
                  const bytes = Uint8Array.from(raw, c => c.charCodeAt(0));
                  let memory;
                  let exitCode = 0;
                  let stdout = [];
                  let stderr = [];
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
                        for (let j = 0; j < len; j++) sink.push(view.getUint8(ptr + j));
                        written += len;
                      }
                      view.setUint32(nwritten, written, true);
                      return 0;
                    },
                    proc_exit(code) { exitCode = code; throw { wasiExit: true }; }
                  };
                  const { instance } = await WebAssembly.instantiate(bytes, {
                    wasi_snapshot_preview1: wasi
                  });
                  memory = instance.exports.memory;
                  try { instance.exports._start(); }
                  catch (error) {
                    if (!error || error.wasiExit !== true) exitCode = 134;
                  }
                  return [exitCode, stdout, stderr];
                }""", wasm)
            return exit_code, bytes(stdout), bytes(stderr)

    def run_wasm_browser_output(self, engine, source):
        with tempfile.TemporaryDirectory() as td:
            source_path = os.path.join(td, "p.bf")
            js_path = os.path.join(td, "p.js")
            with open(source_path, "w") as f:
                f.write(source)
            result = subprocess.run(
                [BFWASM, source_path, "--target", "browser", "-o", js_path],
                capture_output=True)
            if result.returncode != 0:
                return result.returncode, result.stdout, result.stderr

            page = self.pages[engine]
            # The split lane leaves __bfwasm (with its stdout/stderr hooks)
            # on the page. The runtime reads it once at load, so a stale one
            # sends this run's output to the previous program's capture and
            # the console hooks below see nothing.
            page.evaluate(
                "window.__bf={out:[],err:[],thrown:false};"
                "delete window.__bfwasm;"
                "delete window.__bfwasmExitCode")
            with open(js_path, encoding="utf-8") as f:
                page.add_script_tag(content=f.read())
            page.wait_for_function(
                "typeof window.__bfwasmExitCode === 'number'", timeout=30000)
            captured = page.evaluate("window.__bf")
            stdout = bytes(ord(char) for char in "".join(captured["out"]))
            stderr = "".join(captured["err"]).encode("utf-8")
            if stderr:
                stderr += b"\n"
            return page.evaluate("window.__bfwasmExitCode"), stdout, stderr

    def run_wasm_split(self, engine, source, stdin):
        with temporary_directory(self.http_root.name) as td:
            source_path = os.path.join(td, "p.bf")
            js_path = os.path.join(td, "p.js")
            with open(source_path, "w") as f:
                f.write(source)
            result = subprocess.run(
                [BFWASM, source_path, "--target", "browser", "--split",
                 "-o", js_path],
                capture_output=True)
            if result.returncode != 0:
                return result.returncode, result.stdout, result.stderr

            relative_dir = os.path.basename(td)
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
                {"input": list(stdin.encode()), "useBytes": bool(stdin)})
            page.evaluate(
                "url => import(url)",
                self.http_url + relative_dir + "/p.js")
            page.wait_for_function(
                "typeof globalThis.__bfwasmExitCode === 'number'",
                timeout=30000)
            exit_code, stdout, stderr = page.evaluate(
                """() => [
                  globalThis.__bfwasmExitCode,
                  globalThis.__bfwasmCapture.stdout,
                  globalThis.__bfwasmCapture.stderr
                ]""")
            return exit_code, bytes(stdout), bytes(stderr)

    def close(self):
        for browser in self.browsers.values():
            browser.close()
        self.browsers.clear()
        if self.playwright:
            self.playwright.stop()
            self.playwright = None
        if self.http_server:
            self.http_server.shutdown()
            self.http_server.server_close()
            self.http_server = None
        if self.http_thread:
            self.http_thread.join(timeout=5)
            self.http_thread = None
        self.http_root.cleanup()


def run_c(source, stdin):
    """bf2c -> compile the produced C99 with the host cc, then execute."""
    with temporary_directory() as td:
        src = os.path.join(td, "p.bf")
        c_path = os.path.join(td, "p.c")
        exe = os.path.join(td, "p" + EXE)
        obj = os.path.join(td, "p.obj")
        with open(src, "w") as f:
            f.write(source)
        c = subprocess.run([BF2C, src, "-o", c_path], capture_output=True)
        if c.returncode != 0:
            return c.returncode, c.stdout, c.stderr
        if CC[0] == "msvc":
            compiler = CC[1]
            if compiler[0] == "direct":
                cmd = [compiler[1], "/nologo", "/O2", "/Fe" + exe, c_path]
            else:
                command = f'cl /nologo /O2 /Fo"{obj}" /Fe"{exe}" "{c_path}"'
                cmd = _msvc_batch_command(
                    compiler[1], compiler[2], command,
                    os.path.join(td, "compile-msvc.cmd"))
        else:
            cmd = [CC[1], "-O2", "-o", exe, c_path]
        k = subprocess.run(cmd, capture_output=True, timeout=600)
        if k.returncode != 0:
            return k.returncode, k.stdout, k.stderr
        return run_cmd([exe], stdin, td)


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


def main(argv=None) -> int:
    global ROOT, BFC, BFRUN, BFNATIVE, BFINTERP, BFJIT, BFWASM, BFJS, BF2C
    global MANDELBROT

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repo_root", nargs="?", default=ROOT)
    parser.add_argument("--only-node", action="store_true",
                        help="run only bfjs and bfwasm via Node.js")
    parser.add_argument("--require-all", action="store_true",
                        help="fail if any runtime/compiler required by selected lanes is missing")
    args = parser.parse_args(argv)
    ROOT = os.path.abspath(args.repo_root)
    release = os.path.join(ROOT, "target", "release")
    BFC = os.path.join(release, "bfc" + EXE)
    BFRUN = os.path.join(release, "bfrun" + EXE)
    BFNATIVE = os.path.join(release, "bfnative" + EXE)
    BFINTERP = os.path.join(release, "bfinterp" + EXE)
    BFJIT = os.path.join(release, "bfjit" + EXE)
    BFWASM = os.path.join(release, "bfwasm" + EXE)
    BFJS = os.path.join(release, "bfjs" + EXE)
    BF2C = os.path.join(release, "bf2c" + EXE)
    MANDELBROT = os.path.join(ROOT, "tests", "mandelbrot.b")

    tools = [BFC, BFRUN, BFWASM, BFJS]
    if not args.only_node:
        tools.extend([BFNATIVE, BFINTERP, BFJIT, BF2C])
    missing = [t for t in tools if not os.path.exists(t)]
    if missing:
        print(f"missing {missing[0]} — run `cargo build --release` first")
        return 1

    unavailable = []
    if NODE is None:
        unavailable.append("node")
    if not args.only_node:
        if not HAVE_WASMTIME:
            unavailable.append("wasmtime Python package")
        if CC is None:
            unavailable.append("C compiler (cc, gcc, clang, or cl.exe)")
        if not HAVE_PLAYWRIGHT:
            unavailable.append("playwright Python package")
    if unavailable and args.require_all:
        print("required test dependencies missing: " + ", ".join(unavailable))
        return 1
    if NODE is None and args.only_node:
        print("the --only-node lane requires a working node on PATH")
        return 1
    if not args.only_node:
        if not HAVE_WASMTIME:
            print("note: `wasmtime` Python package not installed — "
                  "skipping the bfwasm wasmtime lane (pip install wasmtime)\n")
        if CC is None:
            print("note: no working C compiler — skipping the bf2c lane\n")
        if not HAVE_PLAYWRIGHT:
            print("note: `playwright` Python package not installed — "
                  "skipping browser lanes\n")
    browser_harness = None
    if HAVE_PLAYWRIGHT and not args.only_node:
        try:
            browser_harness = BrowserHarness()
        except Exception as error:
            if args.require_all:
                print(f"could not launch required Playwright browsers: {error}")
                return 1
            print(f"note: could not launch Playwright browsers — skipping browser lanes: {error}")

    corpus = CORPUS + load_mandelbrot()

    aarch64_lanes = []
    if not args.only_node and QEMU_AARCH64 and os.path.exists(QEMU_AARCH64):
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
        results = []
        if args.only_node:
            results.extend([
                ("bfwasm(node:wasi)", run_wasm_node(source, stdin)),
                ("bfjs(node)", run_js(source, stdin)),
            ])
        else:
            results.extend([
                ("bfinterp", run_source_tool(BFINTERP, source, stdin)),
                ("bfinterp --optimize",
                 run_source_tool(BFINTERP, source, stdin, ["--optimize"])),
                ("bfnative", run_native(source, stdin)),
                ("bfjit", run_source_tool(BFJIT, source, stdin)),
            ])
            if HAVE_WASMTIME:
                results.append(("bfwasm(wasmtime)", run_wasm(source, stdin)))
            if NODE is not None:
                results.extend([
                    ("bfwasm(node:wasi)", run_wasm_node(source, stdin)),
                    ("bfjs(node)", run_js(source, stdin)),
                ])
            if CC is not None:
                results.append(("bf2c", run_c(source, stdin)))
            if browser_harness is not None:
                browser_ref = run_ref(source, "")
                for engine in ("chromium", "firefox", "webkit"):
                    results.extend([
                        (f"bfjs({engine})",
                         (browser_ref, browser_harness.run_js(engine, source, ""))),
                        (f"bfwasm({engine})",
                         (browser_ref, browser_harness.run_wasm(engine, source, ""))),
                        (f"bfwasm-js({engine})",
                         (browser_ref,
                          browser_harness.run_wasm_browser_output(engine, source))),
                        (f"bfwasm-split({engine})",
                         (ref,
                          browser_harness.run_wasm_split(engine, source, stdin))),
                    ])

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

        ok = True
        comparisons += len(results)
        for label, result in results:
            expected, actual = result if isinstance(result, tuple) and len(result) == 2 and \
                isinstance(result[0], tuple) else (ref, result)
            if expected != actual:
                ok = False
                lane_failures += 1
                print(f"       ref   : exit={expected[0]} stdout={expected[1]!r} "
                      f"stderr={expected[2]!r}")
                print(f"       {label:20s}: exit={actual[0]} stdout={actual[1]!r} "
                      f"stderr={actual[2]!r}")
        status = "ok " if ok else "FAIL"
        print(f"[{status}] {name}: exit {ref[0]}, stdout {len(ref[1])}B, "
              f"{len(results)} lanes")
        if not ok:
            failures += 1

    print(f"\n{len(corpus) - failures}/{len(corpus)} programs: "
          f"{comparisons - lane_failures}/{comparisons} executions match bfrun exactly "
          f"(stdout, stderr, exit status)")
    if browser_harness is not None:
        browser_harness.close()
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
