//! Byte-level primitives for the WebAssembly binary format: LEB128 varints
//! and the section/vector framing every part of a `.wasm` module is built
//! out of. Nothing in here knows about Brainfuck — that's `codegen.rs`,
//! which uses these to build the actual module. Kept separate the same way
//! `bfnative`'s `Emitter` is separate from its x86-64/ARM64 instruction
//! selection.

/// Unsigned LEB128, the encoding the format uses for byte counts, indices,
/// and unsigned immediates.
pub fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

/// Signed LEB128, for `i32.const`/`i64.const` immediates. Unlike the
/// unsigned form, this has to sign-extend the continuation check — a value
/// like -1 never naturally reaches 0 by right-shifting, so the loop instead
/// stops once the remaining sign bits already match what bit 6 of the last
/// byte will encode.
pub fn sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
        if done {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

/// A length-prefixed byte vector, the `vec(byte)` production used for
/// strings (import/export names) and raw blobs alike.
pub fn byte_vec(out: &mut Vec<u8>, bytes: &[u8]) {
    uleb(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Wraps `body` in a section: tag byte, then a ULEB128 byte length, then the
/// bytes themselves. Every top-level piece of a module (types, imports,
/// functions, memory, exports, code, ...) is one of these, so building the
/// body separately and measuring it here is simpler than threading a
/// backpatchable length field through the writer.
pub fn section(out: &mut Vec<u8>, id: u8, body: &[u8]) {
    out.push(id);
    uleb(out, body.len() as u64);
    out.extend_from_slice(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uleb_small_values_are_one_byte() {
        let mut out = Vec::new();
        uleb(&mut out, 0);
        uleb(&mut out, 127);
        assert_eq!(out, vec![0x00, 0x7f]);
    }

    #[test]
    fn uleb_multi_byte_matches_spec_example() {
        // 624485 is the worked example from the DWARF/wasm LEB128 spec text.
        let mut out = Vec::new();
        uleb(&mut out, 624485);
        assert_eq!(out, vec![0xe5, 0x8e, 0x26]);
    }

    #[test]
    fn sleb_negative_one_is_one_byte() {
        let mut out = Vec::new();
        sleb(&mut out, -1);
        assert_eq!(out, vec![0x7f]);
    }

    #[test]
    fn sleb_matches_spec_example() {
        // -123456, same worked example set as the ULEB128 test above.
        let mut out = Vec::new();
        sleb(&mut out, -123456);
        assert_eq!(out, vec![0xc0, 0xbb, 0x78]);
    }

    #[test]
    fn byte_vec_prefixes_length() {
        let mut out = Vec::new();
        byte_vec(&mut out, b"hi");
        assert_eq!(out, vec![0x02, b'h', b'i']);
    }
}
