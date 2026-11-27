const TEMPLATE: &str = r#"(function () {
"use strict";
const wasmBytes = Uint8Array.from(atob("__BF_WASM_BASE64__"), c => c.charCodeAt(0));
const stdout = [];
const stderr = [];
let memory;
let exitCode = 0;
const exitSignal = {};

const wasi = {
  fd_read(fd, _iovs, _iovsLen, nread) {
    if (fd !== 0) return 8;
    new DataView(memory.buffer).setUint32(nread, 0, true);
    return 0;
  },
  fd_write(fd, iovs, iovsLen, nwritten) {
    if (fd !== 1 && fd !== 2) return 8;
    const view = new DataView(memory.buffer);
    const output = fd === 1 ? stdout : stderr;
    let written = 0;
    for (let i = 0; i < iovsLen; i++) {
      const base = iovs + i * 8;
      const pointer = view.getUint32(base, true);
      const length = view.getUint32(base + 4, true);
      for (let j = 0; j < length; j++) output.push(view.getUint8(pointer + j));
      written += length;
    }
    view.setUint32(nwritten, written, true);
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

function flushOutput() {
  if (stdout.length > 0) console.log(toConsoleText(stdout));
  if (stderr.length > 0) console.error(toConsoleText(stderr).replace(/\n$/, ""));
  globalThis.__bfwasmExitCode = exitCode;
}

WebAssembly.instantiate(wasmBytes, { wasi_snapshot_preview1: wasi })
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
    flushOutput();
  })
  .catch(error => {
    console.error(String(error));
    exitCode = 134;
    flushOutput();
  });
})();
"#;

pub fn compile(module: &[u8]) -> String {
    TEMPLATE.replace("__BF_WASM_BASE64__", &base64_encode(module))
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
    fn browser_source_embeds_the_module_and_inline_wasi_shim() {
        let source = compile(b"\0asm");
        assert!(source.contains("atob(\"AGFzbQ==\")"));
        assert!(source.contains("wasi_snapshot_preview1"));
        assert!(source.contains("fd_read"));
        assert!(source.contains("fd_write"));
        assert!(source.contains("proc_exit"));
        assert!(source.contains("globalThis.__bfwasmExitCode = exitCode"));
        assert!(!source.contains("fetch("));
        assert!(!source.contains(".wasm"));
    }
}
