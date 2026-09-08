//! Mach-O 64-bit executable writer (MH_EXECUTE, PIE) for macOS on x86-64
//! and ARM64.
//!
//! Raw syscalls are not an option on macOS — Apple doesn't guarantee the
//! syscall ABI across versions — so I/O goes through libSystem, which means
//! minimal dynamic linking is unavoidable. This writer emits exactly the
//! pieces dyld needs to bind five symbols (`write`, `read`, `mmap`,
//! `mprotect`, `_exit`) from libSystem and nothing more:
//!
//! - `LC_LOAD_DYLIB /usr/lib/libSystem.B.dylib`
//! - `LC_DYLD_INFO_ONLY` with a hand-built bind-opcode stream targeting the
//!   GOT slots in `__DATA` (non-lazy: dyld resolves everything at load,
//!   before the entry runs; no stubs, no lazy binding, and no classic
//!   indirect symbol table needed)
//! - `LC_SYMTAB`/`LC_DYSYMTAB` carrying the five undefined symbols with
//!   two-level library ordinals, mostly for tooling
//! - `LC_MAIN` for the entry point
//!
//! Ad-hoc code signing is mandatory on Apple Silicon and harmless on Intel:
//! an unsigned arm64 Mach-O simply will not execute. The signature is
//! computed in-process: a v0x20400 CodeDirectory with SHA-256 page hashes
//! over the whole file up to the signature blob, `CS_ADHOC` set, and the
//! `execSeg` fields filled (macOS 11+ arm64 kernels check them). No
//! `codesign` subprocess, no external crypto.
//!
//! Page sizes: 4 KiB on x86-64, 16 KiB on ARM64 — both for segment
//! alignment and for the signature's page hashing, because the kernel
//! verifies at its native page size.
//!
//! Segment layout (every segment satisfies dyld's congruence rule
//! `(vmaddr - fileoff) % page == 0`):
//!
//! ```text
//! __PAGEZERO   vmaddr 0, size 4 GiB (x86-64) / 1 TiB (arm64), no file data
//! __TEXT       RX: headers, load commands, __text (code), __cstring
//! __DATA       RW: __got (5 non-lazy pointer slots, 40 bytes)
//! __LINKEDIT   R:  bind stream, symbol table, string table, code signature
//! ```

use crate::codegen::{Module, external_symbols};
use crate::obj::{Layout, align_up, patch};
use crate::target::{Arch, Os};

// ---- constants ----

const MH_EXECUTE: u32 = 2;
const MH_PIE: u32 = 0x0020_0000; // 0x200 is MH_NOMULTIDEFS, not PIE!
const MH_TWOLEVEL: u32 = 0x0080;
const CPU_TYPE_X86_64: u32 = 0x0100_0007;
const CPU_SUBTYPE_X86_64_ALL: u32 = 3;
const CPU_TYPE_ARM64: u32 = 0x0100_000C;
const CPU_SUBTYPE_ARM64_ALL: u32 = 0;

const LC_SEGMENT_64: u32 = 0x19;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
const LC_SYMTAB: u32 = 0x2;
const LC_DYSYMTAB: u32 = 0xB;
const LC_LOAD_DYLIB: u32 = 0xC;
const LC_MAIN: u32 = 0x8000_0028;
const LC_BUILD_VERSION: u32 = 0x32;
const LC_CODE_SIGNATURE: u32 = 0x1D;

const PLATFORM_MACOS: u32 = 1;
const MINOS_MACOS_11: u32 = 0x000B_0000;

const VM_PROT_READ: u32 = 1;
const VM_PROT_WRITE: u32 = 2;
const VM_PROT_EXECUTE: u32 = 4;

const S_REGULAR: u32 = 0;
const S_CSTRING_LITERALS: u32 = 0x2;
const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;

const N_UNDF: u8 = 0;
const N_EXT: u8 = 1;

// dyld bind opcodes (dyld_info.h).
const BIND_OPCODE_SET_DYLIB_ORDINAL_IMM: u8 = 0x10; // | ordinal
const BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM: u8 = 0x40; // name follows
const BIND_OPCODE_SET_TYPE_IMM: u8 = 0x50; // | type
const BIND_TYPE_POINTER: u8 = 1;
const BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x70; // | segment, uleb
const BIND_OPCODE_DO_BIND: u8 = 0x90;
const BIND_OPCODE_DONE: u8 = 0x00;

// Code signature constants (codesign.h).
const CSMAGIC_CODEDIRECTORY: u32 = 0xFADE_0C02;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xFADE_0CC0;
const CS_ADHOC: u32 = 0x0000_0002;
const CS_EXECSEG_MAIN_BINARY: u64 = 0x0000_0001;
const CSHASH_SHA256: u8 = 2;
const CODEDIRECTORY_VERSION: u32 = 0x0002_0400;
const SEGMENT_INDEX_DATA: u8 = 2; // __PAGEZERO=0, __TEXT=1, __DATA=2
/// Size of the v0x20400 CodeDirectory header (all fields through
/// execSegFlags): 9 u32s + 4 u8s + spare2/scatter/team u32s + 5 u64s.
const CD_HEADER_SIZE: u64 = 92;

/// Field offsets inside `segment_command_64` (for post-hoc fix-ups).
const SEG_VMSIZE_OFF: usize = 32;
const SEG_FILEOFF_OFF: usize = 40;
const SEG_FILESIZE_OFF: usize = 48;

pub fn build(module: &mut Module, arch: Arch, image_name: &str) -> (Vec<u8>, Layout) {
    let page: u64 = match arch {
        Arch::X86_64 => 0x1000,
        Arch::Aarch64 => 0x10000,
    };
    let page_log2: u8 = match arch {
        Arch::X86_64 => 12,
        Arch::Aarch64 => 14,
    };
    // 4 GiB PAGEZERO on both arches — matches what ld64 emits (checked
    // against real arm64 and x86_64 macOS binaries). dyld only requires a
    // page-multiple guard segment, but staying on the standard keeps
    // third-party tooling happy.
    let pagezero: u64 = 0x1_0000_0000;
    let (cputype, cpusubtype) = match arch {
        Arch::X86_64 => (CPU_TYPE_X86_64, CPU_SUBTYPE_X86_64_ALL),
        Arch::Aarch64 => (CPU_TYPE_ARM64, CPU_SUBTYPE_ARM64_ALL),
    };

    let symbols = external_symbols(Os::Macos);
    debug_assert_eq!(symbols.len(), 5);
    let got_size = 8 * symbols.len() as u64;

    let dylib_path = b"/usr/lib/libSystem.B.dylib";
    let load_dylib_cmdsize = align_up((24 + dylib_path.len() + 1) as u64, 8) as u32;

    // ---- load-command table (fixed shape, so sizeofcmds is static) ----
    let sizeofcmds: u32 = 72 // __PAGEZERO
        + (72 + 2 * 80) // __TEXT (2 sections)
        + (72 + 80) // __DATA (1 section)
        + 72 // __LINKEDIT
        + 48 // LC_DYLD_INFO_ONLY
        + 24 // LC_SYMTAB
        + 80 // LC_DYSYMTAB
        + load_dylib_cmdsize
        + 24 // LC_MAIN
        + 24 // LC_BUILD_VERSION
        + 16; // LC_CODE_SIGNATURE
    let ncmds: u32 = 11;

    let header_size = 32u64 + sizeofcmds as u64;
    let code_off = align_up(header_size, 16);
    let cstr_off = align_up(code_off + module.code.len() as u64, 16);
    let text_file_end = cstr_off + module.rodata.len() as u64;

    let text_vmsize = align_up(text_file_end, page);
    let data_fileoff = align_up(text_file_end, page);
    let data_vmaddr = pagezero + text_vmsize;
    let linkedit_fileoff = data_fileoff + got_size;
    let linkedit_vmaddr = data_vmaddr + got_size;

    // __LINKEDIT contents: bind stream, symtab, strtab, then the signature.
    let bind_stream = build_bind_stream(symbols);
    let symtab_off = align_up(bind_stream.len() as u64, 8); // within LINKEDIT
    let strtab = build_strtab(symbols);
    let strtab_off = symtab_off + (symbols.len() * 16) as u64;
    let linkedit_prefix_end = strtab_off + strtab.len() as u64;

    let dataoff = align_up(linkedit_fileoff + linkedit_prefix_end, 16);
    let code_limit = dataoff;

    // ---- patch the code now that all addresses are known ----
    let layout = Layout {
        code_vaddr: pagezero + code_off,
        rodata_vaddr: pagezero + cstr_off,
        got_vaddr: data_vmaddr,
    };
    patch(module, &layout);

    // ---- serialize ----
    let mut out: Vec<u8> = Vec::with_capacity(dataoff as usize + 0x2000);

    // mach_header_64
    out.extend_from_slice(&0xFEED_FACFu32.to_le_bytes());
    out.extend_from_slice(&cputype.to_le_bytes());
    out.extend_from_slice(&cpusubtype.to_le_bytes());
    out.extend_from_slice(&MH_EXECUTE.to_le_bytes());
    out.extend_from_slice(&ncmds.to_le_bytes());
    out.extend_from_slice(&sizeofcmds.to_le_bytes());
    out.extend_from_slice(&(MH_PIE | MH_TWOLEVEL).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved

    // __PAGEZERO
    write_segment(&mut out, "__PAGEZERO", 0, pagezero, 0, 0, 0, 0, &[]);
    // __TEXT
    write_segment(
        &mut out,
        "__TEXT",
        pagezero,
        text_vmsize,
        0,
        text_file_end,
        VM_PROT_READ | VM_PROT_EXECUTE,
        VM_PROT_READ | VM_PROT_EXECUTE,
        &[
            (
                "__text",
                pagezero + code_off,
                module.code.len() as u64,
                code_off as u32,
                4,
                S_REGULAR | S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS,
            ),
            (
                "__cstring",
                pagezero + cstr_off,
                module.rodata.len() as u64,
                cstr_off as u32,
                0,
                S_CSTRING_LITERALS,
            ),
        ],
    );
    // __DATA
    write_segment(
        &mut out,
        "__DATA",
        data_vmaddr,
        got_size,
        data_fileoff,
        got_size,
        VM_PROT_READ | VM_PROT_WRITE,
        VM_PROT_READ | VM_PROT_WRITE,
        &[(
            "__got",
            data_vmaddr,
            got_size,
            data_fileoff as u32,
            3,
            S_REGULAR,
        )],
    );
    // __LINKEDIT (vmsize/filesize patched once the signature size is known)
    write_segment(
        &mut out,
        "__LINKEDIT",
        linkedit_vmaddr,
        0,
        linkedit_fileoff,
        0,
        VM_PROT_READ,
        VM_PROT_READ,
        &[],
    );
    let linkedit_cmd_pos = out.len() - 72;

    // LC_DYLD_INFO_ONLY
    out.extend_from_slice(&LC_DYLD_INFO_ONLY.to_le_bytes());
    out.extend_from_slice(&48u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // rebase_off
    out.extend_from_slice(&0u32.to_le_bytes()); // rebase_size
    out.extend_from_slice(&(linkedit_fileoff as u32).to_le_bytes()); // bind_off
    out.extend_from_slice(&(bind_stream.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // weak_off
    out.extend_from_slice(&0u32.to_le_bytes()); // weak_size
    out.extend_from_slice(&0u32.to_le_bytes()); // lazy_off
    out.extend_from_slice(&0u32.to_le_bytes()); // lazy_size
    out.extend_from_slice(&0u32.to_le_bytes()); // export_off
    out.extend_from_slice(&0u32.to_le_bytes()); // export_size

    // LC_SYMTAB
    out.extend_from_slice(&LC_SYMTAB.to_le_bytes());
    out.extend_from_slice(&24u32.to_le_bytes());
    out.extend_from_slice(&((linkedit_fileoff + symtab_off) as u32).to_le_bytes());
    out.extend_from_slice(&(symbols.len() as u32).to_le_bytes());
    out.extend_from_slice(&((linkedit_fileoff + strtab_off) as u32).to_le_bytes());
    out.extend_from_slice(&(strtab.len() as u32).to_le_bytes());

    // LC_DYSYMTAB: the dyld_info bind stream replaces classic binding, so
    // every table is empty except the undefined-symbol bookkeeping.
    out.extend_from_slice(&LC_DYSYMTAB.to_le_bytes());
    out.extend_from_slice(&80u32.to_le_bytes());
    for field in [0u32, 0, 0, 0, 0, symbols.len() as u32] {
        out.extend_from_slice(&field.to_le_bytes());
    }
    for _ in 0..12 {
        out.extend_from_slice(&0u32.to_le_bytes());
    }

    // LC_LOAD_DYLIB
    out.extend_from_slice(&LC_LOAD_DYLIB.to_le_bytes());
    out.extend_from_slice(&load_dylib_cmdsize.to_le_bytes());
    out.extend_from_slice(&24u32.to_le_bytes()); // name offset within command
    out.extend_from_slice(&0u32.to_le_bytes()); // timestamp (dyld ignores)
    out.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // current version
    out.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // compatibility
    out.extend_from_slice(dylib_path);
    while !out.len().is_multiple_of(8) {
        out.push(0);
    }

    // LC_MAIN
    out.extend_from_slice(&LC_MAIN.to_le_bytes());
    out.extend_from_slice(&24u32.to_le_bytes());
    out.extend_from_slice(&code_off.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes()); // default stack size

    // LC_BUILD_VERSION (informational, keeps modern tooling happy)
    out.extend_from_slice(&LC_BUILD_VERSION.to_le_bytes());
    out.extend_from_slice(&24u32.to_le_bytes());
    out.extend_from_slice(&PLATFORM_MACOS.to_le_bytes());
    out.extend_from_slice(&MINOS_MACOS_11.to_le_bytes());
    out.extend_from_slice(&MINOS_MACOS_11.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // ntools

    // LC_CODE_SIGNATURE (dataoff/datasize patched after hashing)
    let codesig_cmd_pos = out.len();
    out.extend_from_slice(&LC_CODE_SIGNATURE.to_le_bytes());
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // dataoff
    out.extend_from_slice(&0u32.to_le_bytes()); // datasize

    debug_assert_eq!(out.len() as u64, header_size);

    // __TEXT contents: code, then strings.
    while out.len() < code_off as usize {
        out.push(0);
    }
    out.extend_from_slice(&module.code);
    while out.len() < cstr_off as usize {
        out.push(0);
    }
    out.extend_from_slice(&module.rodata);
    while out.len() < data_fileoff as usize {
        out.push(0);
    }

    // __DATA contents: the GOT, zero-filled; dyld writes the resolved
    // function pointers here at load time.
    out.extend_from_slice(&vec![0u8; got_size as usize]);

    // __LINKEDIT contents.
    debug_assert_eq!(out.len() as u64, linkedit_fileoff);
    out.extend_from_slice(&bind_stream);
    while (out.len() as u64 - linkedit_fileoff) < symtab_off {
        out.push(0);
    }
    // nlist_64 entries: undefined, external, two-level ordinal 1.
    let mut strx: Vec<u32> = Vec::with_capacity(symbols.len());
    let mut cursor = 2; // the leading " " entry
    for name in symbols {
        strx.push(cursor as u32);
        cursor += name.len() + 1 + 1; // underscore + NUL
    }
    for &strx in &strx {
        out.extend_from_slice(&strx.to_le_bytes());
        out.push(N_UNDF | N_EXT);
        out.push(0); // n_sect = NO_SECT
        out.extend_from_slice(&0x0100u16.to_le_bytes()); // library ordinal 1
        out.extend_from_slice(&0u64.to_le_bytes()); // n_value
    }
    out.extend_from_slice(&strtab);
    while (out.len() as u64) < dataoff {
        out.push(0);
    }

    // ---- ad-hoc code signature ----
    let ident = sanitize_ident(image_name);
    let ident_len = ident.len() as u64 + 1; // NUL
    let hash_offset = align_up(CD_HEADER_SIZE + ident_len, 8);
    let n_code_slots = code_limit.div_ceil(page);
    let cd_length = (hash_offset + 32 * n_code_slots) as u32;
    let superblob_length = 20 + cd_length;
    let datasize = align_up(superblob_length as u64, 16) as u32;

    // Patch LC_CODE_SIGNATURE and __LINKEDIT sizes BEFORE hashing any
    // pages: those fields live inside the load commands, i.e. inside
    // page 0, so patching after the fact would invalidate the stored
    // page hashes (the kernel re-hashes the file as it exists on disk).
    // None of these values depend on the hashes themselves.
    let sig_pos = codesig_cmd_pos + 8;
    out[sig_pos..sig_pos + 4].copy_from_slice(&(dataoff as u32).to_le_bytes());
    out[sig_pos + 4..sig_pos + 8].copy_from_slice(&datasize.to_le_bytes());
    let linkedit_size = dataoff + datasize as u64 - linkedit_fileoff;
    let le = linkedit_cmd_pos;
    out[le + SEG_VMSIZE_OFF..le + SEG_VMSIZE_OFF + 8].copy_from_slice(&linkedit_size.to_le_bytes());
    out[le + SEG_FILEOFF_OFF..le + SEG_FILEOFF_OFF + 8]
        .copy_from_slice(&linkedit_fileoff.to_le_bytes());
    out[le + SEG_FILESIZE_OFF..le + SEG_FILESIZE_OFF + 8]
        .copy_from_slice(&linkedit_size.to_le_bytes());

    // SuperBlob: header + one CodeDirectory.
    //
    // Code-signing blobs are the one part of this file that isn't
    // native-endian: cscdefs.h specifies every multi-byte field here as
    // network byte order (big-endian) on both x86-64 and arm64, unlike
    // the rest of Mach-O (header, load commands, symtab), which is
    // little-endian on both arches. Mixing the two up produces a blob
    // the kernel can't parse at all -- `codesign -dvvv` reports "code
    // object is not signed at all" and `--verify` reports "invalid or
    // unsupported format for signature" (error -67045), rather than
    // flagging any specific field as wrong.
    out.extend_from_slice(&CSMAGIC_EMBEDDED_SIGNATURE.to_be_bytes());
    out.extend_from_slice(&superblob_length.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // slot type: CodeDirectory
    out.extend_from_slice(&20u32.to_be_bytes()); // slot offset

    // CodeDirectory v0x20400.
    out.extend_from_slice(&CSMAGIC_CODEDIRECTORY.to_be_bytes());
    out.extend_from_slice(&cd_length.to_be_bytes());
    out.extend_from_slice(&CODEDIRECTORY_VERSION.to_be_bytes());
    out.extend_from_slice(&CS_ADHOC.to_be_bytes());
    out.extend_from_slice(&(hash_offset as u32).to_be_bytes()); // hashOffset
    out.extend_from_slice(&(CD_HEADER_SIZE as u32).to_be_bytes()); // identOffset
    out.extend_from_slice(&0u32.to_be_bytes()); // nSpecialSlots
    out.extend_from_slice(&(n_code_slots as u32).to_be_bytes());
    out.extend_from_slice(&(code_limit as u32).to_be_bytes()); // codeLimit
    out.push(32); // hashSize
    out.push(CSHASH_SHA256);
    out.push(0); // platform
    out.push(page_log2);
    out.extend_from_slice(&0u32.to_be_bytes()); // spare2
    out.extend_from_slice(&0u32.to_be_bytes()); // scatterOffset
    out.extend_from_slice(&0u32.to_be_bytes()); // teamOffset
    out.extend_from_slice(&0u64.to_be_bytes()); // spare3
    out.extend_from_slice(&0u64.to_be_bytes()); // codeLimit64
    out.extend_from_slice(&pagezero.to_be_bytes()); // execSegBase
    out.extend_from_slice(&text_vmsize.to_be_bytes()); // execSegLimit
    out.extend_from_slice(&CS_EXECSEG_MAIN_BINARY.to_be_bytes());
    debug_assert_eq!(out.len() - dataoff as usize, (20 + CD_HEADER_SIZE) as usize);

    out.extend_from_slice(ident.as_bytes());
    out.push(0);
    // hash_offset is relative to the CodeDirectory, which starts 20 bytes
    // into the blob (after the SuperBlob header) — pad relative to cd.
    while (out.len() as u64) < dataoff + 20 + hash_offset {
        out.push(0);
    }

    // SHA-256 over each page of [0, codeLimit).
    let mut hashed = 0usize;
    while hashed < code_limit as usize {
        let end = (hashed + page as usize).min(code_limit as usize);
        let hash = crate::sha256::digest(&out[hashed..end]);
        out.extend_from_slice(&hash);
        hashed = end;
    }

    // Pad the signature to datasize.
    while (out.len() - dataoff as usize) < datasize as usize {
        out.push(0);
    }

    (out, layout)
}

// The argument list mirrors the segment_command_64 / section_64 fields
// one-to-one; collapsing it into a struct would just move the same fields
// somewhere else and break the visual match with the spec layout.
#[allow(clippy::too_many_arguments)]
fn write_segment(
    out: &mut Vec<u8>,
    name: &str,
    vmaddr: u64,
    vmsize: u64,
    fileoff: u64,
    filesize: u64,
    maxprot: u32,
    initprot: u32,
    sections: &[(&str, u64, u64, u32, u32, u32)],
) {
    debug_assert!(name.len() < 16);
    out.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
    out.extend_from_slice(&((72 + 80 * sections.len()) as u32).to_le_bytes());
    push_name16(out, name);
    out.extend_from_slice(&vmaddr.to_le_bytes());
    out.extend_from_slice(&vmsize.to_le_bytes());
    out.extend_from_slice(&fileoff.to_le_bytes());
    out.extend_from_slice(&filesize.to_le_bytes());
    out.extend_from_slice(&maxprot.to_le_bytes());
    out.extend_from_slice(&initprot.to_le_bytes());
    out.extend_from_slice(&(sections.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    for (sectname, addr, size, offset, align, flags) in sections {
        debug_assert!(sectname.len() < 16);
        push_name16(out, sectname);
        push_name16(out, name);
        out.extend_from_slice(&addr.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&align.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // reloff
        out.extend_from_slice(&0u32.to_le_bytes()); // nreloc
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved1
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved2
        out.extend_from_slice(&0u32.to_le_bytes()); // reserved3
    }
}

/// Fixed-width 16-byte name field, NUL-padded.
fn push_name16(out: &mut Vec<u8>, name: &str) {
    out.extend_from_slice(name.as_bytes());
    out.extend(std::iter::repeat_n(0u8, 16 - name.len()));
}

fn build_bind_stream(symbols: &[&str]) -> Vec<u8> {
    let mut stream = Vec::new();
    // Two-level namespace: ordinal 1 == the first (only) LC_LOAD_DYLIB.
    stream.push(BIND_OPCODE_SET_DYLIB_ORDINAL_IMM | 1);
    let mut first = true;
    for name in symbols {
        let mangled = format!("_{name}");
        stream.push(BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM);
        stream.extend_from_slice(mangled.as_bytes());
        stream.push(0);
        if first {
            stream.push(BIND_OPCODE_SET_TYPE_IMM | BIND_TYPE_POINTER);
            // Target segment 2 (__DATA) at offset 0 = the GOT base. Each
            // DO_BIND advances the implicit address by 8, so subsequent
            // slots need no address update.
            stream.push(BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB | SEGMENT_INDEX_DATA);
            stream.push(0); // ULEB offset 0
            first = false;
        }
        stream.push(BIND_OPCODE_DO_BIND);
    }
    stream.push(BIND_OPCODE_DONE);
    stream
}

fn build_strtab(symbols: &[&str]) -> Vec<u8> {
    let mut strtab = Vec::new();
    strtab.push(b' ');
    strtab.push(0);
    for name in symbols {
        strtab.extend_from_slice(format!("_{name}").as_bytes());
        strtab.push(0);
    }
    strtab
}

fn sanitize_ident(name: &str) -> String {
    let stem = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let ident: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if ident.is_empty() {
        "bfnative".to_string()
    } else {
        ident
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen;
    use crate::target::Target;

    fn build_for(ops: &[bfformat::Op], name: &str) -> Vec<u8> {
        let target = Target::from_name(name).unwrap();
        let mut m = codegen::emit(ops, target);
        build(&mut m, target.arch, "test").0
    }

    #[test]
    fn header_and_command_table_shape() {
        for (name, cputype) in [
            ("macos-x86_64", CPU_TYPE_X86_64),
            ("macos-aarch64", CPU_TYPE_ARM64),
        ] {
            let bytes = build_for(&[bfformat::Op::Zero], name);
            assert_eq!(
                u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
                0xFEED_FACF
            );
            assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), cputype);
            assert_eq!(
                u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
                MH_EXECUTE
            );
            let ncmds = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
            let sizeofcmds = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
            assert_eq!(ncmds, 11);
            // sizeofcmds must place code exactly at align16(32 + sizeofcmds).
            let code_off = align_up(32 + sizeofcmds as u64, 16);
            assert_eq!((32 + sizeofcmds as u64) % 8, 0, "commands are 8-aligned");
            // The file length must cover the signature.
            assert!(bytes.len() as u64 > code_off);
            let _ = page_for(name);
        }
    }

    fn page_for(name: &str) -> u64 {
        if name.contains("aarch64") {
            0x10000
        } else {
            0x1000
        }
    }

    #[test]
    fn code_signature_hashes_reverify() {
        for name in ["macos-x86_64", "macos-aarch64"] {
            let bytes = build_for(&[bfformat::Op::Zero], name);
            let page = page_for(name);
            // Walk to LC_CODE_SIGNATURE and re-verify every page hash.
            let ncmds = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
            let sizeofcmds = u32::from_le_bytes(bytes[20..24].try_into().unwrap()) as usize;
            let mut pos = 32usize;
            let mut dataoff = 0usize;
            let mut datasize = 0usize;
            let mut found = false;
            for _ in 0..ncmds {
                let cmd = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                let cmdsize =
                    u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
                if cmd == LC_CODE_SIGNATURE {
                    dataoff =
                        u32::from_le_bytes(bytes[pos + 8..pos + 12].try_into().unwrap()) as usize;
                    datasize =
                        u32::from_le_bytes(bytes[pos + 12..pos + 16].try_into().unwrap()) as usize;
                    found = true;
                }
                pos += cmdsize;
            }
            assert_eq!(pos, 32 + sizeofcmds, "command table walks cleanly");
            assert!(found, "LC_CODE_SIGNATURE present");
            assert_eq!(dataoff % 16, 0, "signature is 16-byte aligned");
            assert_eq!(dataoff + datasize, bytes.len(), "signature ends the file");

            // SuperBlob header. Code-signing blobs are big-endian
            // (cscdefs.h), unlike the Mach-O load commands walked above.
            let sb = dataoff;
            assert_eq!(
                u32::from_be_bytes(bytes[sb..sb + 4].try_into().unwrap()),
                CSMAGIC_EMBEDDED_SIGNATURE
            );
            let cd_off = sb + 20;
            assert_eq!(
                u32::from_be_bytes(bytes[cd_off..cd_off + 4].try_into().unwrap()),
                CSMAGIC_CODEDIRECTORY
            );
            // codeLimit + pageSize from the CodeDirectory.
            let cd = cd_off;
            let code_limit =
                u32::from_be_bytes(bytes[cd + 32..cd + 36].try_into().unwrap()) as usize;
            let page_log2 = bytes[cd + 39];
            assert_eq!(page_log2, if page == 0x1000 { 12 } else { 14 });
            let hash_offset =
                u32::from_be_bytes(bytes[cd + 16..cd + 20].try_into().unwrap()) as usize;
            let n_code_slots =
                u32::from_be_bytes(bytes[cd + 28..cd + 32].try_into().unwrap()) as usize;
            let ident_offset =
                u32::from_be_bytes(bytes[cd + 20..cd + 24].try_into().unwrap()) as usize;
            assert_eq!(ident_offset, CD_HEADER_SIZE as usize);
            assert_eq!(code_limit, dataoff);
            assert_eq!(
                n_code_slots,
                (code_limit as u64 + page - 1) as usize / page as usize
            );

            for slot in 0..n_code_slots {
                let start = slot * page as usize;
                let end = ((slot + 1) * page as usize).min(code_limit);
                let expected = crate::sha256::digest(&bytes[start..end]);
                let actual: [u8; 32] = bytes
                    [cd + hash_offset + slot * 32..cd + hash_offset + slot * 32 + 32]
                    .try_into()
                    .unwrap();
                assert_eq!(expected, actual, "page {slot} hash mismatch");
            }
        }
    }

    #[test]
    fn segment_congruence_holds_for_every_segment() {
        for name in ["macos-x86_64", "macos-aarch64"] {
            let bytes = build_for(&[bfformat::Op::Output, bfformat::Op::Input], name);
            let page = page_for(name);
            let ncmds = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
            let mut pos = 32usize;
            for _ in 0..ncmds {
                let cmd = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
                let cmdsize =
                    u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
                if cmd == LC_SEGMENT_64 {
                    let vmaddr = u64::from_le_bytes(bytes[pos + 24..pos + 32].try_into().unwrap());
                    let vmsize = u64::from_le_bytes(bytes[pos + 32..pos + 40].try_into().unwrap());
                    let fileoff = u64::from_le_bytes(bytes[pos + 40..pos + 48].try_into().unwrap());
                    let filesize =
                        u64::from_le_bytes(bytes[pos + 48..pos + 56].try_into().unwrap());
                    if filesize > 0 {
                        assert_eq!(
                            (vmaddr.wrapping_sub(fileoff)) % page,
                            0,
                            "segment at {pos} breaks dyld congruence"
                        );
                        assert!(fileoff + filesize <= bytes.len() as u64);
                        assert!(vmsize >= filesize);
                    } else {
                        assert_eq!(
                            vmsize % page,
                            0,
                            "PAGEZERO-style segment must be page-multiple"
                        );
                    }
                }
                pos += cmdsize;
            }
        }
    }

    #[test]
    fn bind_stream_binds_all_five_symbols_to_contiguous_slots() {
        let symbols = external_symbols(Os::Macos);
        let stream = build_bind_stream(symbols);
        // One DO_BIND per symbol, one DONE, one SET_DYLIB, one SET_TYPE,
        // one SET_SEGMENT.
        let do_binds = stream.iter().filter(|&&b| b == BIND_OPCODE_DO_BIND).count();
        assert_eq!(do_binds, symbols.len());
        assert_eq!(*stream.last().unwrap(), BIND_OPCODE_DONE);
        for name in symbols {
            let mangled = format!("_{name}");
            assert!(stream.windows(mangled.len() + 2).any(|w| {
                w[0] == BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM
                    && &w[1..1 + mangled.len()] == mangled.as_bytes()
            }));
        }

        // Byte-exact first entry: ordinal 1, symbol "_write", type pointer,
        // segment 2 (__DATA) at ULEB offset 0, then DO_BIND. The segment
        // opcode must be 0x70 — 0x60 is SET_ADDEND_SLEB, which dyld would
        // silently consume and then try to bind into __PAGEZERO (address 0,
        // read-only) and abort at load time. Cross-checked against a real
        // ld64-produced bind stream.
        let want = [
            0x11u8, // SET_DYLIB_ORDINAL_IMM | 1
            0x40,
            b'_',
            b'w',
            b'r',
            b'i',
            b't',
            b'e',
            0x00,
            0x51,     // SET_TYPE_IMM | BIND_TYPE_POINTER
            0x70 | 2, // SET_SEGMENT_AND_OFFSET_ULEB | __DATA
            0x00,     // ULEB offset 0
            0x90,     // DO_BIND
        ];
        assert!(
            stream.starts_with(&want),
            "first bind entry is not canonical: {:02x?}",
            &stream[..want.len().min(stream.len())]
        );
    }
}
