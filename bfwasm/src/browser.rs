const RUNTIME: &str = r#"
const config = globalThis.__bfwasm || {};
const configuredStdin = config.stdin;
const stdinBytes = typeof configuredStdin === "string"
  ? new TextEncoder().encode(configuredStdin)
  : configuredStdin instanceof Uint8Array ? configuredStdin : new Uint8Array();
const onStdout = typeof config.onStdout === "function" ? config.onStdout : null;
const onStderr = typeof config.onStderr === "function" ? config.onStderr : null;
const stdout = [];
const stderr = [];
let inputOffset = 0;
let memory;
let exitCode = 0;
const exitSignal = {};

function emit(fd, pointer, length) {
  const chunk = new Uint8Array(memory.buffer, pointer, length).slice();
  const hook = fd === 1 ? onStdout : onStderr;
  if (hook) {
    hook(chunk);
  } else {
    const output = fd === 1 ? stdout : stderr;
    for (const byte of chunk) output.push(byte);
  }
}

const wasi = {
  fd_read(fd, iovs, iovsLen, nread) {
    if (fd !== 0) return 8;
    const view = new DataView(memory.buffer);
    let bytesRead = 0;
    for (let i = 0; i < iovsLen; i++) {
      const base = iovs + i * 8;
      const pointer = view.getUint32(base, true);
      const length = view.getUint32(base + 4, true);
      const count = Math.min(length, stdinBytes.length - inputOffset);
      if (count > 0) {
        new Uint8Array(memory.buffer, pointer, count)
          .set(stdinBytes.subarray(inputOffset, inputOffset + count));
      }
      inputOffset += count;
      bytesRead += count;
      if (count < length) break;
    }
    view.setUint32(nread, bytesRead, true);
    return 0;
  },
  fd_write(fd, iovs, iovsLen, nwritten) {
    if (fd !== 1 && fd !== 2) return 8;
    const view = new DataView(memory.buffer);
    let bytesWritten = 0;
    for (let i = 0; i < iovsLen; i++) {
      const base = iovs + i * 8;
      const pointer = view.getUint32(base, true);
      const length = view.getUint32(base + 4, true);
      emit(fd, pointer, length);
      bytesWritten += length;
    }
    view.setUint32(nwritten, bytesWritten, true);
    return 0;
  },
  proc_exit(code) {
    exitCode = code;
    throw exitSignal;
  }
};

function toConsoleText(bytes) {
  let text = "";
  for (const byte of bytes) text += String.fromCharCode(byte);
  return text;
}

function finish() {
  globalThis.__bfwasmExitCode = exitCode;
  if (!onStdout && stdout.length > 0) console.log(toConsoleText(stdout));
  if (!onStderr && stderr.length > 0) console.error(toConsoleText(stderr).replace(/\n$/, ""));
}

function runWasm(instantiate) {
  const imports = { wasi_snapshot_preview1: wasi };
  Promise.resolve()
    .then(() => instantiate(imports))
    .then(({ instance }) => {
      memory = instance.exports.memory;
      try {
        instance.exports._start();
      } catch (error) {
        if (error !== exitSignal) {
          console.error(String(error));
          exitCode = 134;
        }
      }
      finish();
    })
    .catch(error => {
      console.error(String(error));
      exitCode = 134;
      finish();
    });
}
"#;

const SINGLE_TEMPLATE: &str = r#"(function () {
"use strict";
const wasmBytes = Uint8Array.from(atob("__BF_WASM_BASE64__"), c => c.charCodeAt(0));
__BF_WASI_RUNTIME__
runWasm(imports => WebAssembly.instantiate(wasmBytes, imports));
})();
"#;

const SPLIT_TEMPLATE: &str = r#"const wasmUrl = new URL("./__BF_WASM_FILENAME__", import.meta.url);
__BF_WASI_RUNTIME__
runWasm(async imports => {
  try {
    return await WebAssembly.instantiateStreaming(fetch(wasmUrl), imports);
  } catch {
    const response = await fetch(wasmUrl);
    return WebAssembly.instantiate(await response.arrayBuffer(), imports);
  }
});
"#;

pub fn compile(module: &[u8]) -> String {
    SINGLE_TEMPLATE
        .replace("__BF_WASI_RUNTIME__", RUNTIME)
        .replace("__BF_WASM_BASE64__", &base64_encode(module))
}

pub fn compile_split(wasm_filename: &str) -> String {
    SPLIT_TEMPLATE
        .replace("__BF_WASI_RUNTIME__", RUNTIME)
        .replace("__BF_WASM_FILENAME__", &encode_url_component(wasm_filename))
}

fn encode_url_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(HEX[(byte >> 4) as usize] as char);
                encoded.push(HEX[(byte & 0x0F) as usize] as char);
            }
        }
    }
    encoded
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);

        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0b11) << 4) | (second >> 4)) as usize] as char);

        match chunk.len() {
            1 => encoded.push_str("=="),
            2 => {
                encoded.push(ALPHABET[((second & 0b1111) << 2) as usize] as char);
                encoded.push('=');
            }
            _ => {
                encoded.push(ALPHABET[(((second & 0b1111) << 2) | (third >> 6)) as usize] as char);
                encoded.push(ALPHABET[(third & 0b111111) as usize] as char);
            }
        }
    }

    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_encodes_each_chunk_length_and_empty_input() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
    }

    #[test]
    fn browser_source_embeds_the_module_and_shared_wasi_runtime() {
        let source = compile(b"\0asm");
        assert!(source.contains("atob(\"AGFzbQ==\")"));
        assert!(source.contains("wasi_snapshot_preview1"));
        assert!(source.contains("fd_read"));
        assert!(source.contains("fd_write"));
        assert!(source.contains("proc_exit"));
        assert!(source.contains("new TextEncoder().encode(configuredStdin)"));
        assert!(source.contains("config.onStdout"));
        assert!(source.contains("config.onStderr"));
        assert!(source.contains("globalThis.__bfwasmExitCode = exitCode"));
        assert!(!source.contains("fetch("));
        assert!(!source.contains(".wasm"));
    }

    #[test]
    fn split_loader_fetches_a_relative_module_with_streaming_fallback() {
        let source = compile_split("program.wasm");
        assert!(source.contains("new URL(\"./program.wasm\", import.meta.url)"));
        assert!(source.contains("WebAssembly.instantiateStreaming(fetch(wasmUrl), imports)"));
        assert!(source.contains("WebAssembly.instantiate(await response.arrayBuffer(), imports)"));
        assert!(source.contains("wasi_snapshot_preview1"));
        assert!(source.contains("fd_read"));
        assert!(source.contains("fd_write"));
        assert!(source.contains("proc_exit"));
        assert!(source.contains("config.onStdout"));
        assert!(source.contains("config.onStderr"));
        assert!(source.contains("globalThis.__bfwasmExitCode = exitCode"));
    }

    #[test]
    fn split_loader_url_encodes_non_path_filename_characters() {
        let source = compile_split("my app#?.wasm");
        assert!(source.contains("new URL(\"./my%20app%23%3F.wasm\", import.meta.url)"));
        assert_eq!(encode_url_component("a-_.~"), "a-_.~");
        assert_eq!(encode_url_component("é"), "%C3%A9");
    }
}
